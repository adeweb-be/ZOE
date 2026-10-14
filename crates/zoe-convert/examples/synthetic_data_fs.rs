//! Write a synthetic `Data.fs` for the measure of the memory of a
//! conversion. The file holds no production data: every pickle comes from
//! a seeded generator.
//!
//! Usage:
//!
//! ```text
//! synthetic_data_fs OUT TRANSACTIONS RECORDS_PER_TXN [LARGE_RECORDS] [BLOB_EVERY]
//! ```
//!
//! - `OUT` is the path of the new file. Blob files go to `OUT.blobs/` in
//!   the lawn layout.
//! - Each transaction writes `RECORDS_PER_TXN` records. About one record in
//!   four writes an oid again, so the store has history.
//! - `LARGE_RECORDS`, when not 0, adds one transaction in the middle of the
//!   file with that many records.
//! - `BLOB_EVERY`, when not 0, makes one record in that many a blob record
//!   with a blob file of 4 KiB.
//!
//! The sizes of the pickles follow a skewed spread: most are between 200
//! bytes and 2 KiB, one in 64 is between 8 and 64 KiB, and one in 4,096 is
//! between 128 and 512 KiB. Half of each pickle is text that compresses.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

/// The first tid. Each later transaction adds `TID_STEP`.
const FIRST_TID: u64 = 0x03e0_0000_0000_0000;
const TID_STEP: u64 = 0x1_0000;

/// The pickle of a blob record: its class pickle is `ZODB.blob.Blob`.
const BLOB_PICKLE: &[u8] = b"\x80\x03cZODB.blob\nBlob\nq\x00.\x80\x03N.";

/// The class pickle of the other records.
const CLASS: &[u8] = b"\x80\x03cplone.synthetic\nItem\nq\x00.";

/// A seeded xorshift generator.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn pickle_len(rng: &mut Rng) -> usize {
    let len = match rng.below(4096) {
        0 => 128 * 1024 + rng.below(384 * 1024),
        n if n % 64 == 0 => 8 * 1024 + rng.below(56 * 1024),
        _ => 200 + rng.below(1848),
    };
    len as usize
}

/// Write a pickle of `len` bytes into `out`: the class pickle, text that
/// compresses, then bytes that do not.
fn pickle(rng: &mut Rng, len: usize, out: &mut Vec<u8>) {
    out.clear();
    out.extend_from_slice(CLASS);
    const TEXT: &[u8] = b"title description text subject creator ";
    while out.len() < len / 2 {
        out.extend_from_slice(TEXT);
    }
    while out.len() < len {
        out.extend_from_slice(&rng.next().to_le_bytes());
    }
    out.truncate(len.max(CLASS.len()));
    out.push(b'.');
}

struct Writer {
    out: BufWriter<File>,
    pos: u64,
    blobs: PathBuf,
    next_oid: u64,
    rng: Rng,
    blob_every: u64,
    record: u64,
    buf: Vec<u8>,
    body: Vec<u8>,
}

impl Writer {
    /// Write one transaction of `records` records under `tid`.
    fn txn(&mut self, tid: u64, records: u64) -> std::io::Result<()> {
        let user = b" /plone/admin";
        let description = b"synthetic";
        let pos = self.pos;
        let header_len = 23 + (user.len() + description.len()) as u64;
        // The data records, written to memory first because the header
        // holds their length. One transaction at a time.
        self.body.clear();
        // An oid comes at most once in a transaction, as in a file that
        // ZODB writes. A rewrite takes an oid of an earlier transaction.
        let first_new = self.next_oid;
        let mut seen = HashSet::new();
        for _ in 0..records {
            let old = (first_new > 1 && self.rng.below(4) == 0)
                .then(|| 1 + self.rng.below(first_new - 1))
                .filter(|oid| seen.insert(*oid));
            let oid = old.unwrap_or_else(|| {
                let oid = self.next_oid;
                self.next_oid += 1;
                oid
            });
            self.record += 1;
            let blob = self.blob_every != 0 && self.record.is_multiple_of(self.blob_every);
            if blob {
                self.buf.clear();
                self.buf.extend_from_slice(BLOB_PICKLE);
                self.blob_file(oid, tid)?;
            } else {
                let len = pickle_len(&mut self.rng);
                pickle(&mut self.rng, len, &mut self.buf);
            }
            self.body.extend_from_slice(&oid.to_be_bytes());
            self.body.extend_from_slice(&tid.to_be_bytes());
            self.body.extend_from_slice(&0u64.to_be_bytes());
            self.body.extend_from_slice(&pos.to_be_bytes());
            self.body.extend_from_slice(&[0, 0]);
            self.body
                .extend_from_slice(&(self.buf.len() as u64).to_be_bytes());
            self.body.extend_from_slice(&self.buf);
        }
        let len = header_len + self.body.len() as u64;
        self.out.write_all(&tid.to_be_bytes())?;
        self.out.write_all(&len.to_be_bytes())?;
        self.out.write_all(b" ")?;
        self.out.write_all(&(user.len() as u16).to_be_bytes())?;
        self.out
            .write_all(&(description.len() as u16).to_be_bytes())?;
        self.out.write_all(&0u16.to_be_bytes())?;
        self.out.write_all(user)?;
        self.out.write_all(description)?;
        self.out.write_all(&self.body)?;
        self.out.write_all(&len.to_be_bytes())?;
        self.pos += len + 8;
        Ok(())
    }

    /// Write the blob file of `oid` at `tid` in the lawn layout.
    fn blob_file(&mut self, oid: u64, tid: u64) -> std::io::Result<()> {
        let dir = self.blobs.join(repr(oid));
        fs::create_dir_all(&dir)?;
        let mut bytes = vec![0u8; 4096];
        for chunk in bytes.chunks_mut(8) {
            chunk.copy_from_slice(&self.rng.next().to_le_bytes());
        }
        fs::write(dir.join(format!("{}.blob", repr(tid))), bytes)
    }
}

/// Write `n` as ZODB's `oid_repr` does.
fn repr(n: u64) -> String {
    let digits = format!("{n:x}");
    if digits.len() % 2 == 1 {
        format!("0x0{digits}")
    } else {
        format!("0x{digits}")
    }
}

fn arg(args: &[String], i: usize) -> u64 {
    args.get(i).map_or(0, |s| s.parse().expect("a number"))
}

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!(
            "usage: {} OUT TRANSACTIONS RECORDS_PER_TXN [LARGE_RECORDS] [BLOB_EVERY]",
            args[0]
        );
        std::process::exit(2);
    }
    let path = Path::new(&args[1]);
    let (transactions, per_txn) = (arg(&args, 2), arg(&args, 3));
    let (large, blob_every) = (arg(&args, 4), arg(&args, 5));

    let blobs = PathBuf::from(format!("{}.blobs", path.display()));
    if blob_every != 0 {
        fs::create_dir_all(&blobs)?;
        fs::write(blobs.join(".layout"), "lawn")?;
    }
    let mut writer = Writer {
        out: BufWriter::with_capacity(1 << 20, File::create(path)?),
        pos: 4,
        blobs,
        next_oid: 1,
        rng: Rng(0x9e37_79b9_7f4a_7c15),
        blob_every,
        record: 0,
        buf: Vec::new(),
        body: Vec::new(),
    };
    writer.out.write_all(b"FS30")?;
    let mut tid = FIRST_TID;
    for i in 0..transactions {
        if large != 0 && i == transactions / 2 {
            writer.txn(tid, large)?;
            tid += TID_STEP;
        }
        writer.txn(tid, per_txn)?;
        tid += TID_STEP;
    }
    writer.out.flush()?;
    let txns = transactions + u64::from(large != 0);
    println!(
        "{}: {txns} transactions, {} records, {} oids, {} bytes",
        path.display(),
        writer.record,
        writer.next_oid - 1,
        writer.pos
    );
    Ok(())
}
