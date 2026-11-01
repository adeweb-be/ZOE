//! The layout of a dump file (ADR 0030): its writer and its reader.

use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, BufWriter, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};

use zoe_core::{BlobRecord, DurableRow, Oid, Store, Tid, ZoeError};

use super::Backup;
use super::chain::DumpFile;
use super::damaged;
use crate::Error;
use crate::fsutil::{CHUNK, io_error};
use crate::walk::each_durable_row;

/// The first 8 bytes of a dump file: the name of the format, then its
/// version, 2. A restore refuses a file with other bytes there.
pub(super) const MAGIC: &[u8; 8] = b"ZOEDUMP\x02";

/// The length of the header: the magic, then two tids.
pub(super) const HEADER: usize = 24;

/// The length of a BLAKE3 hash.
pub(super) const HASH: usize = 32;

/// The tag of the end of a sequence.
const SEQUENCE: u8 = b'S';
/// The tag of a durable row of a KV namespace.
const DURABLE: u8 = b'K';
/// The tag of a transaction.
const TRANSACTION: u8 = b'T';
/// The tag of a record that holds a pickle.
const RECORD: u8 = b'R';
/// The tag of a deletion.
const DELETION: u8 = b'D';
/// The tag of a record with its blob.
const BLOB: u8 = b'B';
/// The tag of the end of the file. The hash follows it.
const END: u8 = b'E';

/// Check the hash at the end of the dump file `path` against every byte
/// before it. The pass reads the file as a stream and reads no length of
/// the file, so a damaged length allocates nothing.
pub(super) fn check_hash(path: &Path) -> Result<(), Error> {
    let io = |source| io_error(path, source);
    let file = File::open(path).map_err(io)?;
    let len = file.metadata().map_err(io)?.len();
    let Some(body) = len
        .checked_sub(HASH as u64)
        .filter(|&body| body > HEADER as u64)
    else {
        return Err(damaged(path, "the file is too short for a dump".to_owned()));
    };
    let mut reader = BufReader::with_capacity(CHUNK, file);
    let mut hasher = blake3::Hasher::new();
    let read = io::copy(&mut (&mut reader).take(body), &mut hasher).map_err(io)?;
    let mut hash = [0; HASH];
    let complete = read == body && reader.read_exact(&mut hash).is_ok();
    if !complete || hash != *hasher.finalize().as_bytes() {
        let reason = "the checksum does not match: the file is damaged";
        return Err(damaged(path, reason.to_owned()));
    }
    Ok(())
}

/// Read the header of the dump file `path`: its two tids.
pub(super) fn read_header(path: &Path) -> Result<(Tid, Tid), Error> {
    let mut file = File::open(path).map_err(|source| io_error(path, source))?;
    let mut header = [0; HEADER];
    match file.read_exact(&mut header) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::UnexpectedEof => {
            return Err(damaged(path, "the file ends inside the header".to_owned()));
        }
        Err(error) => return Err(io_error(path, error)),
    }
    parse_header(path, &header)
}

/// Check the magic of `header` and return its two tids.
fn parse_header(path: &Path, header: &[u8; HEADER]) -> Result<(Tid, Tid), Error> {
    if header[..8] != MAGIC[..] {
        return Err(damaged(path, "not a dump file of ZOE".to_owned()));
    }
    let tid = |at: usize| Tid(u64::from_be_bytes(header[at..at + 8].try_into().unwrap()));
    Ok((tid(8), tid(16)))
}

/// Write the dump of the transactions of `store` after `after` up to
/// `last` into the new file `path`, and sync it.
pub(super) fn write_dump(
    store: &Store,
    path: &Path,
    after: Tid,
    last: Tid,
    report: &mut Backup,
) -> Result<(), Error> {
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|source| io_error(path, source))?;
    let mut out = DumpWriter {
        path,
        out: BufWriter::new(file),
        hasher: blake3::Hasher::new(),
    };
    out.put(MAGIC)?;
    out.put(&after.0.to_be_bytes())?;
    out.put(&last.0.to_be_bytes())?;

    for name in store.sequence_names()? {
        let Some(end) = store.sequence_end(&name)? else {
            continue;
        };
        out.put(&[SEQUENCE])?;
        out.bytes(name.as_bytes())?;
        out.put(&end.to_be_bytes())?;
        report.sequences += 1;
    }

    // Every durable row, as the store holds it after its last tid. For an
    // exact dump, no writer changes the store during the call.
    each_durable_row(store, |row| {
        out.put(&[DURABLE])?;
        out.bytes(row.name.as_bytes())?;
        out.bytes(&row.key)?;
        out.bytes(&row.payload)?;
        report.durable_rows += 1;
        Ok(())
    })?;

    // `after` is below `last`, so the next tid exists.
    for txn in store.iterator(Some(Tid(after.0 + 1)), Some(last))? {
        let txn = txn?;
        out.put(&[TRANSACTION])?;
        out.put(&txn.tid.0.to_be_bytes())?;
        out.bytes(&txn.user)?;
        out.bytes(&txn.description)?;
        out.bytes(&txn.extension)?;
        out.put(&(txn.records.len() as u64).to_be_bytes())?;
        for record in &txn.records {
            let oid = record.oid.0.to_be_bytes();
            let Some(data) = &record.data else {
                out.put(&[DELETION])?;
                out.put(&oid)?;
                report.records += 1;
                continue;
            };
            match store.load_blob(record.oid, txn.tid)? {
                None => {
                    out.put(&[RECORD])?;
                    out.put(&oid)?;
                    out.bytes(data)?;
                }
                Some(blob) => {
                    out.put(&[BLOB])?;
                    out.put(&oid)?;
                    out.bytes(data)?;
                    out.put(&blob.size.to_be_bytes())?;
                    out.put(&blob.hash)?;
                    let reader = store.open_blob(record.oid, txn.tid)?;
                    out.blob(reader, blob.size, record.oid, txn.tid)?;
                    report.blobs += 1;
                    report.blob_bytes += blob.size;
                }
            }
            report.records += 1;
        }
        report.transactions += 1;
    }
    out.finish()
}

/// The writer of a dump file. It hashes each byte that it writes.
struct DumpWriter<'a> {
    path: &'a Path,
    out: BufWriter<File>,
    hasher: blake3::Hasher,
}

impl DumpWriter<'_> {
    /// Write `bytes`.
    fn put(&mut self, bytes: &[u8]) -> Result<(), Error> {
        self.hasher.update(bytes);
        self.out
            .write_all(bytes)
            .map_err(|source| io_error(self.path, source))
    }

    /// Write the length of `bytes` in 4 bytes, then `bytes`.
    fn bytes(&mut self, bytes: &[u8]) -> Result<(), Error> {
        let Ok(len) = u32::try_from(bytes.len()) else {
            let error = io::Error::new(ErrorKind::InvalidInput, "a field of 4 GiB or more");
            return Err(io_error(self.path, error));
        };
        self.put(&len.to_be_bytes())?;
        self.put(bytes)
    }

    /// Copy the `size` bytes of the blob of `oid` at `tid` from `blob`.
    /// The reader of the store checks the size and the hash at the end.
    fn blob(&mut self, mut blob: impl Read, size: u64, oid: Oid, tid: Tid) -> Result<(), Error> {
        let mut buf = vec![0; CHUNK];
        let mut copied = 0u64;
        loop {
            let n = match blob.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(error) => return Err(store_error(error)),
            };
            self.put(&buf[..n])?;
            copied += n as u64;
        }
        if copied != size {
            return Err(Error::Store(ZoeError::CorruptBlob { oid, tid }));
        }
        Ok(())
    }

    /// Write the end tag and the hash of every byte before it, and sync
    /// the file.
    fn finish(mut self) -> Result<(), Error> {
        self.put(&[END])?;
        let hash = self.hasher.finalize();
        let path = self.path;
        let io = |source| io_error(path, source);
        self.out.write_all(hash.as_bytes()).map_err(io)?;
        let file = self.out.into_inner().map_err(|e| io(e.into_error()))?;
        file.sync_all().map_err(io)
    }
}

/// Return the error of a read or a write of a file of the store: the error
/// of the store that `error` holds, or `error` itself.
fn store_error(error: io::Error) -> Error {
    if !error.get_ref().is_some_and(|inner| inner.is::<ZoeError>()) {
        return Error::Store(error.into());
    }
    let kind = error.kind();
    match error.into_inner().map(|inner| inner.downcast::<ZoeError>()) {
        Some(Ok(inner)) => Error::Store(*inner),
        Some(Err(inner)) => Error::Store(io::Error::new(kind, inner).into()),
        None => Error::Store(io::Error::from(kind).into()),
    }
}

/// One entry of a dump file.
pub(super) enum Entry {
    /// The end of a sequence.
    Sequence { name: String, end: u64 },
    /// A durable row of a KV namespace.
    Durable(DurableRow),
    /// The header of a transaction. Its records follow.
    Transaction(Head),
    /// A record. When it has a blob, `DumpReader::blob` reads the bytes.
    Record {
        oid: Oid,
        /// The pickle. It is `None` for a deletion.
        data: Option<Vec<u8>>,
        /// The size and the hash of the blob.
        blob: Option<BlobRecord>,
    },
    /// The end of the file. The hash matches.
    End,
}

/// The header of a transaction in a dump file.
pub(super) struct Head {
    pub(super) tid: Tid,
    pub(super) user: Vec<u8>,
    pub(super) description: Vec<u8>,
    pub(super) extension: Vec<u8>,
    pub(super) records: u64,
}

/// Where a reader is in a dump file.
#[derive(Clone, Copy)]
enum State {
    /// Before the first transaction: sequences come here.
    Sequences,
    /// Inside a transaction: `left` records follow, after the oid `oid`.
    Records { left: u64, oid: Option<Oid> },
    /// After the records of a transaction.
    Transactions,
    /// After the end.
    Ended,
}

/// A reader that feeds each byte that it gives to a BLAKE3 hasher.
struct Hashed<R> {
    inner: R,
    hasher: blake3::Hasher,
    /// The number of bytes read.
    pos: u64,
}

impl<R: Read> Read for Hashed<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.hasher.update(&buf[..n]);
        self.pos += n as u64;
        Ok(n)
    }
}

/// The reader of a dump file. It checks the grammar of the file, the hash
/// of each blob and the hash of the file.
pub(super) struct DumpReader {
    path: PathBuf,
    input: Hashed<BufReader<File>>,
    last: Tid,
    state: State,
    /// The tid of the last transaction read, or the last tid of the dump
    /// before.
    tid: Tid,
    /// The blob of the last record, which the caller did not read yet.
    blob: Option<(Oid, BlobRecord)>,
}

impl DumpReader {
    /// Open the dump file `dump` and read its header.
    pub(super) fn open(dump: &DumpFile) -> Result<Self, Error> {
        let path = &dump.path;
        let file = File::open(path).map_err(|source| io_error(path, source))?;
        let mut reader = Self {
            path: path.clone(),
            input: Hashed {
                inner: BufReader::with_capacity(CHUNK, file),
                hasher: blake3::Hasher::new(),
                pos: 0,
            },
            last: dump.last,
            state: State::Sequences,
            tid: dump.after,
            blob: None,
        };
        let mut header = [0; HEADER];
        reader.exact(&mut header, "the header")?;
        if parse_header(path, &header)? != (dump.after, dump.last) {
            let reason = "the header names other tids than the file name";
            return Err(reader.damaged(reason.to_owned()));
        }
        Ok(reader)
    }

    /// Return the next entry. A blob that the caller did not read is read
    /// and checked first.
    pub(super) fn next(&mut self) -> Result<Entry, Error> {
        if self.blob.is_some() {
            self.blob(&mut io::sink())?;
        }
        match self.state {
            State::Records { left, oid } => return self.record(left, oid),
            State::Ended => return Err(self.damaged("a read after the end".to_owned())),
            State::Sequences | State::Transactions => {}
        }
        let at = self.input.pos;
        let tag = self.u8("an entry")?;
        match (tag, self.state) {
            (SEQUENCE, State::Sequences) => {
                let name = self.field("the name of a sequence")?;
                let end = self.u64("the end of a sequence")?;
                let Ok(name) = String::from_utf8(name) else {
                    let reason = format!("the name of the sequence at offset {at} is not UTF-8");
                    return Err(self.damaged(reason));
                };
                Ok(Entry::Sequence { name, end })
            }
            (DURABLE, State::Sequences) => {
                let name = self.field("the name of a namespace")?;
                let key = self.field("a key of a namespace")?;
                let payload = self.field("a value of a namespace")?;
                let Ok(name) = String::from_utf8(name) else {
                    let reason = format!("the name of the namespace at offset {at} is not UTF-8");
                    return Err(self.damaged(reason));
                };
                Ok(Entry::Durable(DurableRow { name, key, payload }))
            }
            (TRANSACTION, State::Sequences | State::Transactions) => self.transaction(at),
            (END, State::Sequences | State::Transactions) => self.end(),
            _ => Err(self.damaged(format!("an unexpected tag {tag:#04x} at offset {at}"))),
        }
    }

    /// Read the header of a transaction, after its tag at offset `at`.
    fn transaction(&mut self, at: u64) -> Result<Entry, Error> {
        let tid = Tid(self.u64("a transaction")?);
        if tid <= self.tid || tid > self.last {
            let reason = format!(
                "transaction {} at offset {at} is not after tid {} and at or before tid {}",
                tid.hex(),
                self.tid.hex(),
                self.last.hex()
            );
            return Err(self.damaged(reason));
        }
        self.tid = tid;
        let user = self.field("a user")?;
        let description = self.field("a description")?;
        let extension = self.field("an extension")?;
        let records = self.u64("a transaction")?;
        self.state = if records == 0 {
            State::Transactions
        } else {
            State::Records {
                left: records,
                oid: None,
            }
        };
        Ok(Entry::Transaction(Head {
            tid,
            user,
            description,
            extension,
            records,
        }))
    }

    /// Read a record of a transaction that has `left` records to read, the
    /// last one of `before`.
    fn record(&mut self, left: u64, before: Option<Oid>) -> Result<Entry, Error> {
        let at = self.input.pos;
        let tag = self.u8("a record")?;
        if !matches!(tag, RECORD | DELETION | BLOB) {
            let reason = format!("an unexpected tag {tag:#04x} at offset {at}, not a record");
            return Err(self.damaged(reason));
        }
        let oid = Oid(self.u64("a record")?);
        if before.is_some_and(|before| oid <= before) {
            let reason = format!("the records are not in oid order at offset {at}");
            return Err(self.damaged(reason));
        }
        self.state = match left - 1 {
            0 => State::Transactions,
            left => State::Records {
                left,
                oid: Some(oid),
            },
        };
        if tag == DELETION {
            return Ok(Entry::Record {
                oid,
                data: None,
                blob: None,
            });
        }
        let data = self.field("a pickle")?;
        if data.is_empty() {
            let reason = format!("the record at offset {at} has an empty pickle");
            return Err(self.damaged(reason));
        }
        if tag == RECORD {
            return Ok(Entry::Record {
                oid,
                data: Some(data),
                blob: None,
            });
        }
        let size = self.u64("a blob")?;
        let mut hash = [0; HASH];
        self.exact(&mut hash, "a blob")?;
        let blob = BlobRecord { hash, size };
        self.blob = Some((oid, blob));
        Ok(Entry::Record {
            oid,
            data: Some(data),
            blob: Some(blob),
        })
    }

    /// Copy the blob of the last record into `out`, and check its hash.
    pub(super) fn blob(&mut self, out: &mut dyn Write) -> Result<(), Error> {
        let Some((oid, blob)) = self.blob.take() else {
            return Err(self.damaged("no blob to read".to_owned()));
        };
        let mut hasher = blake3::Hasher::new();
        let mut buf = vec![0; CHUNK];
        let mut left = blob.size;
        while left > 0 {
            let n = usize::try_from(left).map_or(CHUNK, |left| left.min(CHUNK));
            self.exact(&mut buf[..n], "a blob")?;
            hasher.update(&buf[..n]);
            out.write_all(&buf[..n]).map_err(store_error)?;
            left -= n as u64;
        }
        if *hasher.finalize().as_bytes() != blob.hash {
            let reason = format!(
                "the blob of oid {} at tid {} does not match its hash",
                oid.hex(),
                self.tid.hex()
            );
            return Err(self.damaged(reason));
        }
        Ok(())
    }

    /// Check the hash at the end of the file, after the end tag.
    fn end(&mut self) -> Result<Entry, Error> {
        let expected = self.input.hasher.finalize();
        // The hash is not part of the bytes that it covers.
        let mut hash = [0; HASH];
        match self.input.inner.read_exact(&mut hash) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::UnexpectedEof => {
                return Err(self.damaged("the file ends inside the checksum".to_owned()));
            }
            Err(error) => return Err(io_error(&self.path, error)),
        }
        if hash != *expected.as_bytes() {
            let reason = "the checksum does not match: the file is damaged";
            return Err(self.damaged(reason.to_owned()));
        }
        let mut byte = [0; 1];
        match self.input.inner.read(&mut byte) {
            Ok(0) => {}
            Ok(_) => return Err(self.damaged("bytes after the checksum".to_owned())),
            Err(error) => return Err(io_error(&self.path, error)),
        }
        self.state = State::Ended;
        Ok(Entry::End)
    }

    fn exact(&mut self, buf: &mut [u8], what: &str) -> Result<(), Error> {
        match self.input.read_exact(buf) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::UnexpectedEof => {
                Err(self.damaged(format!("the file ends inside {what}")))
            }
            Err(error) => Err(io_error(&self.path, error)),
        }
    }

    fn u8(&mut self, what: &str) -> Result<u8, Error> {
        let mut buf = [0; 1];
        self.exact(&mut buf, what)?;
        Ok(buf[0])
    }

    fn u64(&mut self, what: &str) -> Result<u64, Error> {
        let mut buf = [0; 8];
        self.exact(&mut buf, what)?;
        Ok(u64::from_be_bytes(buf))
    }

    /// Read a length in 4 bytes, then that many bytes. The buffer grows as
    /// the bytes come, so a damaged length allocates no more than the
    /// file holds.
    fn field(&mut self, what: &str) -> Result<Vec<u8>, Error> {
        let mut len = [0; 4];
        self.exact(&mut len, what)?;
        let len = u64::from(u32::from_be_bytes(len));
        let mut bytes = Vec::new();
        (&mut self.input)
            .take(len)
            .read_to_end(&mut bytes)
            .map_err(|source| io_error(&self.path, source))?;
        if bytes.len() as u64 != len {
            return Err(self.damaged(format!("the file ends inside {what}")));
        }
        Ok(bytes)
    }

    pub(super) fn damaged(&self, reason: String) -> Error {
        damaged(&self.path, reason)
    }
}
