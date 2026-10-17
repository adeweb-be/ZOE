//! Dumps of a store: [`backup`], [`restore`] and [`verify_backup`].
//!
//! A backup directory holds a chain of dump files. The first one is the
//! full dump: every transaction of the store up to its last tid. Each later
//! one is an incremental dump: the transactions after the last tid of the
//! dump before it. A dump also holds the blob of each blob record, the end
//! of each sequence and, from version 2 on, every row of the durable KV
//! namespaces with their generation rows (ADR 0043). ADR 0030 gives the
//! layout. A restore reads versions 1 and 2, and writes the durable rows of
//! the last dump of the chain.
//!
//! A backup reads the store through its transaction iterator. It writes a
//! dump file once, under a temporary name, syncs it and renames it into
//! place. So it never changes a dump file that the directory holds.
//!
//! A restore checks every dump file before it writes anything. It builds
//! the store in a work directory beside the target, and renames the work
//! directory to the target at the end. So a restore that fails leaves
//! nothing at the target.

use std::borrow::Borrow;
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, BufReader, BufWriter, ErrorKind, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use zoe_core::{BlobRecord, DurableRow, Oid, Store, Tid, TxnInfo, ZoeError};

use crate::Error;
use crate::datafs::Problems;
use crate::filestorage::hex;

/// The first 8 bytes of a dump file that a backup writes: the name of the
/// format, then its version, 2.
const MAGIC: &[u8; 8] = b"ZOEDUMP\x02";

/// The first 8 bytes of a dump file of version 1, which has no durable
/// rows. A restore reads it.
const MAGIC_1: &[u8; 8] = b"ZOEDUMP\x01";

/// The durable rows that one read of a backup takes.
const ROW_CHUNK: usize = 1024;

/// The suffix of the name of a dump file.
const SUFFIX: &str = ".zoedump";

/// The suffix of a dump file that a backup still writes.
const TEMP_SUFFIX: &str = ".tmp";

/// The suffix of the work directory of a restore, beside the target.
const WORK_SUFFIX: &str = ".restore";

/// The start of the name of the copy that `verify_backup` restores, before
/// the id of its process.
const CHECK_PREFIX: &str = "zoe-backup-check-";

/// The number of record keys that one read of the comparison takes.
const KEY_CHUNK: usize = 1024;

/// The database file in the directory of a store.
const DATABASE: &str = "zoe.redb";

/// The length of the header: the magic, then two tids.
const HEADER: usize = 24;

/// The length of a BLAKE3 hash.
const HASH: usize = 32;

/// The size of the buffer of a blob copy.
const CHUNK: usize = 1 << 16;

/// The tag of the end of a sequence.
const SEQUENCE: u8 = b'S';
/// The tag of a durable row of a KV namespace, from version 2 on.
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

/// What a backup wrote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Backup {
    /// The dump file that the backup wrote. It is `None` when the store
    /// has no transaction after the dumps.
    pub dump: Option<PathBuf>,
    /// The last tid of the dump before this one. It is `Tid::ZERO` for the
    /// full dump.
    pub after: Tid,
    /// The last tid of the dump: the last tid of the store at the start of
    /// the backup.
    pub last: Tid,
    /// The transactions of the dump.
    pub transactions: u64,
    /// The records of the dump, deletions too.
    pub records: u64,
    /// The blobs of the dump.
    pub blobs: u64,
    /// The bytes of the blobs of the dump.
    pub blob_bytes: u64,
    /// The sequences of the dump.
    pub sequences: u64,
    /// The durable rows of the KV namespaces of the dump, the generation
    /// rows included.
    pub durable_rows: u64,
}

/// What a restore read and wrote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Restored {
    /// The dump files.
    pub dumps: u64,
    /// The transactions.
    pub transactions: u64,
    /// The records, deletions too.
    pub records: u64,
    /// The blobs.
    pub blobs: u64,
    /// The sequences.
    pub sequences: u64,
    /// The durable rows of the KV namespaces of the last dump, the
    /// generation rows included.
    pub durable_rows: u64,
    /// The last tid of the last dump.
    pub last_tid: Tid,
}

/// The result of [`verify_backup`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackupCheck {
    /// What the restore of the dumps read.
    pub restored: Restored,
    /// The differences between the dumps and the store, one per line. At
    /// most 100 lines, and a last line counts the others.
    pub differences: Vec<String>,
}

/// Write a dump of `store` into the backup directory `dir`, and return
/// what it wrote.
///
/// The first dump of `dir` is the full dump. A later dump holds the
/// transactions after the last tid of the dumps, up to the last tid of the
/// store at the start of the call. The call reads the end of each sequence
/// after it reads that last tid. So each value of a sequence that a
/// transaction of the dump holds is below its end in the dump. When the
/// dumps end at the last tid of the store, the call writes nothing.
///
/// The call creates `dir` when it is missing. It takes a lock on `dir`, so
/// two backups never write into one directory together. It removes the
/// temporary dump files that a stopped backup left. It never changes a dump
/// file. A dump that does not follow the dump before it fails the call with
/// `Dump`. When the last tid of the dumps is not a tid of the store, the
/// call fails with `BackupDiverged` and writes nothing. When the store was
/// packed after that tid, it fails with `BackupPacked` and writes nothing.
/// A pack at that tid removes only history that the dumps hold, so the
/// backup goes on. Both errors ask for a new full dump in a new directory.
///
/// For an exact dump, no writer changes the store during the call. A
/// read-only open makes sure of this.
pub fn backup(store: &Store, dir: &Path) -> Result<Backup, Error> {
    create_dirs(dir).map_err(|source| io_error(dir, source))?;
    let _lock = lock_dir(dir)?;
    remove_temp_files(dir)?;
    let dumps = chain(dir)?;
    let after = dumps.last().map_or(Tid::ZERO, |dump| dump.last);
    let last = store.last_transaction();
    let mut report = Backup {
        dump: None,
        after,
        last,
        transactions: 0,
        records: 0,
        blobs: 0,
        blob_bytes: 0,
        sequences: 0,
        durable_rows: 0,
    };
    if !dumps.is_empty() {
        if last == after {
            return Ok(report);
        }
        continues(store, dir, after, last)?;
    }

    let path = dir.join(dump_name(after, last));
    let temp = sibling(&path, TEMP_SUFFIX)?;
    let written = write_dump(store, &temp, after, last, &mut report)
        .and_then(|()| fs::rename(&temp, &path).map_err(|source| io_error(&path, source)))
        .and_then(|()| sync_dir(dir));
    if written.is_err() {
        let _ = fs::remove_file(&temp);
    }
    written?;
    report.dump = Some(path);
    Ok(report)
}

/// Restore the store in the backup directory `dir` into `target`, and
/// return what the restore read and wrote.
///
/// The restore reads the dumps in order and commits each transaction
/// under its own tid, with its records and its blobs. It checks the BLAKE3
/// hash of each blob. Each sequence restarts at or above its end in the
/// dumps.
///
/// `target` must not exist, or be an empty directory on the filesystem of
/// its parent: a directory that holds a store or other files, or a mount
/// point, fails the call with `Target`. The call checks the hash of every
/// dump file before it writes anything, in a pass that reads each file as
/// a stream. A damaged dump file, or a dump that does not follow the dump
/// before it, fails the call with `Dump`. A directory without a dump fails
/// with `NoDump`. A file whose hash matches but that breaks the layout
/// fails the call while it builds the store.
///
/// The restore builds the store in a work directory beside `target`: the
/// name of `target` with the suffix `.restore`. It renames the work
/// directory to `target` at the end. When the call fails, it removes the
/// work directory. A work directory that exists fails the call, because
/// another restore can be writing into it.
pub fn restore(dir: &Path, target: &Path) -> Result<Restored, Error> {
    check_target(target)?;
    let dumps = chain(dir)?;
    if dumps.is_empty() {
        return Err(Error::NoDump {
            path: dir.to_owned(),
        });
    }
    for dump in &dumps {
        check_hash(&dump.path)?;
    }

    let parent = parent(target);
    create_dirs(parent).map_err(|source| io_error(parent, source))?;
    let work = sibling(target, WORK_SUFFIX)?;
    match fs::create_dir(&work) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            return Err(Error::Target {
                path: work,
                reason: "exists: another restore writes into it, or a restore stopped; \
                         remove it when no restore runs",
            });
        }
        Err(error) => return Err(io_error(&work, error)),
    }
    let restored = build(&dumps, &work)
        .and_then(|report| {
            fs::rename(&work, target).map_err(|source| io_error(target, source))?;
            Ok(report)
        })
        .and_then(|report| sync_dir(parent).map(|()| report));
    if restored.is_err() {
        let _ = fs::remove_dir_all(&work);
    }
    restored
}

/// Restore the dumps of the backup directory `dir` into a new directory in
/// `scratch`, open the store with `open`, compare the two, and remove that
/// directory.
///
/// The call builds the copy before it calls `open`. So the store stays
/// free while the copy is built, and a dump that does not restore fails
/// the call before the open. The copy is a throwaway store: the call
/// builds it with [`restore`] and removes it at the end, also when the
/// call fails. The call writes nothing outside `scratch`. It drops the
/// store of `open` before it returns.
///
/// The comparison covers the transactions of the store up to the last tid
/// of the dumps, so a commit after the last dump is no difference. Each
/// transaction and each record must be the same, with the same blob record:
/// the size and the BLAKE3 hash of its blob. A pack removes records at or
/// before its tid, and the dumps keep them. So at or before the pack
/// watermark of the store, a record or a transaction that the store lacks
/// is no difference. A pack after the last tid of the dumps is a
/// difference: it changed history that the next dump must hold. A pack at
/// that tid is no difference. The current record of each oid at the last
/// tid of the dumps must be the same too, so a store whose history
/// diverged differs, unless a pack removed that record. In the store, the
/// end of each sequence of the dumps must be at or above its end in the
/// dumps.
///
/// A dump that does not restore fails the call with its error, and an
/// open that fails fails the call with the error of `open`. The call
/// first removes the directories in `scratch` that a check in a process
/// that is gone left.
pub fn verify_backup<S: Borrow<Store>, E: From<Error>>(
    dir: &Path,
    scratch: &Path,
    open: impl FnOnce() -> Result<S, E>,
) -> Result<BackupCheck, E> {
    static CHECKS: AtomicU64 = AtomicU64::new(0);
    remove_stale_checks(scratch);
    let n = CHECKS.fetch_add(1, Ordering::Relaxed);
    let copy = scratch.join(format!("{CHECK_PREFIX}{}-{n}", std::process::id()));
    let checked = restore(dir, &copy).map_err(E::from).and_then(|restored| {
        let opened = Store::open_read_only(&copy).map_err(|e| E::from(Error::from(e)))?;
        let store = open()?;
        let differences = compare(store.borrow(), &opened, restored.last_tid).map_err(E::from)?;
        Ok(BackupCheck {
            restored,
            differences,
        })
    });
    match fs::remove_dir_all(&copy) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) if checked.is_ok() => return Err(io_error(&copy, error).into()),
        Err(_) => {}
    }
    checked
}

/// Remove each directory of a check in `scratch` whose process is gone: a
/// copy, or the work directory of its restore. The name of such a
/// directory holds the id of its process. A process that `/proc` lists
/// keeps its directories.
fn remove_stale_checks(scratch: &Path) {
    let Ok(entries) = fs::read_dir(scratch) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .and_then(|name| name.strip_prefix(CHECK_PREFIX))
            .and_then(|rest| rest.split('-').next())
            .and_then(|pid| pid.parse::<u32>().ok())
        else {
            continue;
        };
        if pid != std::process::id() && !Path::new("/proc").join(pid.to_string()).exists() {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

/// Check the hash at the end of the dump file `path` against every byte
/// before it. The pass reads the file as a stream and reads no length of
/// the file, so a damaged length allocates nothing.
fn check_hash(path: &Path) -> Result<(), Error> {
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

/// One dump file of a backup directory.
struct DumpFile {
    path: PathBuf,
    /// The last tid of the dump before it, or `Tid::ZERO`.
    after: Tid,
    /// The last tid that the dump covers.
    last: Tid,
}

/// Return the name of the dump file of the tids after `after` up to
/// `last`.
fn dump_name(after: Tid, last: Tid) -> String {
    format!("{:016x}-{:016x}{SUFFIX}", after.0, last.0)
}

/// Return the two tids of the name of a dump file, or `None` for another
/// name.
fn parse_name(name: &str) -> Option<(Tid, Tid)> {
    let (after, last) = name.strip_suffix(SUFFIX)?.split_once('-')?;
    let tid = |hex: &str| {
        let digits = hex.len() == 16 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        digits.then(|| u64::from_str_radix(hex, 16).ok().map(Tid))?
    };
    Some((tid(after)?, tid(last)?))
}

/// Return the dump files of `dir` in order. Each one must start after the
/// last tid of the one before it, and the first one at the start.
///
/// The call reads the header of each dump file and checks that it names
/// the tids of the file name. Names of other forms do not count.
fn chain(dir: &Path) -> Result<Vec<DumpFile>, Error> {
    let entries = fs::read_dir(dir).map_err(|source| io_error(dir, source))?;
    let mut dumps = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| io_error(dir, source))?;
        let Some((after, last)) = entry.file_name().to_str().and_then(parse_name) else {
            continue;
        };
        let path = entry.path();
        let kind = entry
            .file_type()
            .map_err(|source| io_error(&path, source))?;
        if !kind.is_file() {
            return Err(damaged(&path, "not a regular file".to_owned()));
        }
        dumps.push(DumpFile { path, after, last });
    }
    dumps.sort_by_key(|dump| (dump.after, dump.last));

    let mut end: Option<Tid> = None;
    for dump in &dumps {
        let reason = match end {
            None if dump.after != Tid::ZERO => Some(format!(
                "the full dump is missing: the first dump starts after tid {}",
                hex(dump.after.0)
            )),
            Some(end) if dump.after < end => Some(format!(
                "the dumps overlap: this dump starts after tid {}, before the end {} of the dump before it",
                hex(dump.after.0),
                hex(end.0)
            )),
            Some(end) if dump.after > end => Some(format!(
                "a dump is missing: this dump starts after tid {}, and the dump before it ends at tid {}",
                hex(dump.after.0),
                hex(end.0)
            )),
            // Only a full dump of an empty store covers no tid.
            _ if dump.last < dump.after || (dump.last == dump.after && end.is_some()) => {
                Some("the dump covers no tid".to_owned())
            }
            _ => None,
        };
        if let Some(reason) = reason {
            return Err(damaged(&dump.path, reason));
        }
        let header = read_header(&dump.path)?;
        if header != (dump.after, dump.last) {
            let reason = format!(
                "the header names the tids {} and {}, not those of the file name",
                hex(header.0.0),
                hex(header.1.0)
            );
            return Err(damaged(&dump.path, reason));
        }
        end = Some(dump.last);
    }
    Ok(dumps)
}

/// Read the header of the dump file `path`: its two tids.
fn read_header(path: &Path) -> Result<(Tid, Tid), Error> {
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
    if header[..8] != MAGIC[..] && header[..8] != MAGIC_1[..] {
        return Err(damaged(path, "not a dump file of ZOE".to_owned()));
    }
    let tid = |at: usize| Tid(u64::from_be_bytes(header[at..at + 8].try_into().unwrap()));
    Ok((tid(8), tid(16)))
}

/// Check that the store continues the dumps that end at `after`: `after`
/// is a tid of the store, before its last tid `last`, and no pack of the
/// store went past `after`.
///
/// The pack watermark does not tell one store from another, so a missing
/// transaction always refuses: a store restored from a part of the dumps,
/// or another store, never continues them.
fn continues(store: &Store, dir: &Path, after: Tid, last: Tid) -> Result<(), Error> {
    let diverged = || Error::BackupDiverged {
        path: dir.to_owned(),
        last: after,
    };
    if after == Tid::ZERO {
        return Ok(());
    }
    if last < after {
        return Err(diverged());
    }
    match store.iterator(Some(after), Some(after))?.next() {
        Some(Ok(txn)) if txn.tid == after => {}
        Some(Err(error)) => return Err(error.into()),
        _ => return Err(diverged()),
    }
    // A pack after `after` changed history that the next dump must hold.
    // A pack at `after` removed only history that the dumps hold.
    let watermark = store.pack_watermark()?;
    if watermark > after {
        return Err(Error::BackupPacked {
            path: dir.to_owned(),
            last: after,
            watermark,
        });
    }
    Ok(())
}

/// Write the dump of the transactions of `store` after `after` up to
/// `last` into the new file `path`, and sync it.
fn write_dump(
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
    let mut after_row: Option<Vec<u8>> = None;
    loop {
        let (rows, next) = store.kv_durable_rows(after_row.as_deref(), ROW_CHUNK)?;
        for row in &rows {
            out.put(&[DURABLE])?;
            out.bytes(row.name.as_bytes())?;
            out.bytes(&row.key)?;
            out.bytes(&row.payload)?;
            report.durable_rows += 1;
        }
        if rows.len() < ROW_CHUNK {
            break;
        }
        after_row = next;
    }

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
enum Entry {
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
struct Head {
    tid: Tid,
    user: Vec<u8>,
    description: Vec<u8>,
    extension: Vec<u8>,
    records: u64,
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
struct DumpReader {
    path: PathBuf,
    input: Hashed<BufReader<File>>,
    last: Tid,
    state: State,
    /// The tid of the last transaction read, or the last tid of the dump
    /// before.
    tid: Tid,
    /// True for a dump of version 2, which can hold durable rows.
    durable: bool,
    /// The blob of the last record, which the caller did not read yet.
    blob: Option<(Oid, BlobRecord)>,
}

impl DumpReader {
    /// Open the dump file `dump` and read its header.
    fn open(dump: &DumpFile) -> Result<Self, Error> {
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
            durable: false,
            blob: None,
        };
        let mut header = [0; HEADER];
        reader.exact(&mut header, "the header")?;
        reader.durable = header[..8] == MAGIC[..];
        if parse_header(path, &header)? != (dump.after, dump.last) {
            let reason = "the header names other tids than the file name";
            return Err(reader.damaged(reason.to_owned()));
        }
        Ok(reader)
    }

    /// Return the next entry. A blob that the caller did not read is read
    /// and checked first.
    fn next(&mut self) -> Result<Entry, Error> {
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
            (DURABLE, State::Sequences) if self.durable => {
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
                hex(tid.0),
                hex(self.tid.0),
                hex(self.last.0)
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
    fn blob(&mut self, out: &mut dyn Write) -> Result<(), Error> {
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
                hex(oid.0),
                hex(self.tid.0)
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

    fn damaged(&self, reason: String) -> Error {
        damaged(&self.path, reason)
    }
}

/// Build the store in `work` from `dumps`, which the caller checked.
fn build(dumps: &[DumpFile], work: &Path) -> Result<Restored, Error> {
    let store = Store::open(work)?;
    let mut report = Restored {
        dumps: 0,
        transactions: 0,
        records: 0,
        blobs: 0,
        sequences: 0,
        durable_rows: 0,
        last_tid: Tid::ZERO,
    };
    let mut names = BTreeSet::new();
    // The durable rows of the last dump: each dump holds every row.
    let mut rows = Vec::new();
    for dump in dumps {
        let mut reader = DumpReader::open(dump)?;
        rows.clear();
        loop {
            match reader.next()? {
                Entry::Sequence { name, end } => {
                    store.sequence(&name).restore_end(end)?;
                    names.insert(name);
                }
                Entry::Durable(row) => rows.push(row),
                Entry::Transaction(head) => restore_txn(&store, &mut reader, &head, &mut report)?,
                Entry::Record { .. } => {
                    return Err(reader.damaged("a record outside a transaction".to_owned()));
                }
                Entry::End => break,
            }
        }
        report.dumps += 1;
        report.last_tid = dump.last;
    }
    report.sequences = names.len() as u64;
    // One backend write holds them all.
    store.kv_restore_durable(&rows)?;
    report.durable_rows = rows.len() as u64;
    Ok(report)
}

/// Restore the transaction `head` and its records from `reader`.
fn restore_txn(
    store: &Store,
    reader: &mut DumpReader,
    head: &Head,
    report: &mut Restored,
) -> Result<(), Error> {
    let mut txn =
        store.tpc_begin_restore(head.tid, &head.user, &head.description, &head.extension)?;
    for _ in 0..head.records {
        let Entry::Record { oid, data, blob } = reader.next()? else {
            return Err(reader.damaged("a transaction without all its records".to_owned()));
        };
        match (data, blob) {
            (None, _) => txn.restore(oid, None)?,
            (Some(data), None) => txn.restore(oid, Some(&data))?,
            (Some(data), Some(_)) => {
                let mut file = store.new_blob_file()?;
                reader.blob(&mut file)?;
                txn.restore_blob(oid, &data, file)?;
                report.blobs += 1;
            }
        }
        report.records += 1;
    }
    txn.vote()?;
    txn.finish()?;
    report.transactions += 1;
    Ok(())
}

/// Return the differences between `store` and the restored dumps `copy`,
/// up to the last tid `last` of the dumps.
fn compare(store: &Store, copy: &Store, last: Tid) -> Result<Vec<String>, Error> {
    let watermark = store.pack_watermark()?;
    let mut problems = Problems::default();
    let mut ours = store.iterator(None, Some(last))?.peekable();
    for dumped in copy.iterator(None, None)? {
        let dumped = dumped?;
        // A transaction of the store before this one is not in the dumps.
        while let Some(txn) = ours.next_if(|txn| !txn.as_ref().is_ok_and(|t| t.tid >= dumped.tid)) {
            problems.push(match txn {
                Ok(txn) => not_in_dumps(txn.tid),
                Err(error) => format!("the store does not read: {error}"),
            });
        }
        let packed = dumped.tid <= watermark;
        match ours.next_if(|txn| txn.as_ref().is_ok_and(|t| t.tid == dumped.tid)) {
            Some(Ok(txn)) => compare_txn(store, copy, &txn, &dumped, packed, &mut problems)?,
            // A pack removed the transaction.
            _ if packed => {}
            _ => problems.push(format!("the store lacks transaction {}", hex(dumped.tid.0))),
        }
    }
    for txn in ours {
        problems.push(match txn {
            Ok(txn) => not_in_dumps(txn.tid),
            Err(error) => format!("the store does not read: {error}"),
        });
    }

    if watermark > last {
        problems.summary(format!(
            "the store was packed at tid {}, after the last tid {} of the dumps; \
             write a new full dump into a new directory",
            hex(watermark.0),
            hex(last.0)
        ));
    } else {
        compare_current(store, copy, last, watermark, &mut problems)?;
    }

    for name in copy.sequence_names()? {
        let Some(end) = copy.sequence_end(&name)? else {
            continue;
        };
        match store.sequence_end(&name) {
            Ok(Some(have)) if have >= end => {}
            Ok(Some(have)) => problems.push(format!(
                "sequence {name:?}: the store ends at {have}, below its end {end} in the dumps"
            )),
            Ok(None) => problems.push(format!("sequence {name:?}: the store has no row")),
            Err(error) => problems.push(format!("sequence {name:?}: {error}")),
        }
    }
    // The durable rows of the store can change only with a commit, so the
    // comparison holds when the store has no commit after the dumps.
    if store.last_transaction() == last {
        compare_durable(store, copy, &mut problems)?;
    }
    Ok(problems.finish())
}

/// Compare the durable rows of `store` with those of `copy`. A difference
/// names the namespace and the xxh3-64 hash of the key, never the key.
fn compare_durable(store: &Store, copy: &Store, problems: &mut Problems) -> Result<(), Error> {
    let all = |store: &Store| -> Result<Vec<DurableRow>, Error> {
        let mut rows = Vec::new();
        let mut after: Option<Vec<u8>> = None;
        loop {
            let (chunk, next) = store.kv_durable_rows(after.as_deref(), ROW_CHUNK)?;
            let full = chunk.len() == ROW_CHUNK;
            rows.extend(chunk);
            if !full {
                return Ok(rows);
            }
            after = next;
        }
    };
    let name = |row: &DurableRow| {
        format!(
            "durable row of namespace {:?}, the key of xxh3-64 {:#018x}",
            row.name,
            xxhash_rust::xxh3::xxh3_64(&row.key)
        )
    };
    let ours = all(store)?;
    let theirs = all(copy)?;
    let mut ours = ours.into_iter().peekable();
    for dumped in theirs {
        while let Some(row) = ours.next_if(|row| (&row.name, &row.key) < (&dumped.name, &dumped.key)) {
            problems.push(format!("{}: not in the dumps", name(&row)));
        }
        match ours.next_if(|row| (&row.name, &row.key) == (&dumped.name, &dumped.key)) {
            Some(row) if row.payload == dumped.payload => {}
            Some(row) => problems.push(format!("{}: another value in the dumps", name(&row))),
            None => problems.push(format!("{}: the store lacks it", name(&dumped))),
        }
    }
    for row in ours {
        problems.push(format!("{}: not in the dumps", name(&row)));
    }
    Ok(())
}

/// Compare the current record of each oid at `last` in `store` and in
/// `copy`. A record that `copy` has and `store` lacks is no difference
/// when its serial is at or before `watermark`: a pack removed it.
fn compare_current(
    store: &Store,
    copy: &Store,
    last: Tid,
    watermark: Tid,
    problems: &mut Problems,
) -> Result<(), Error> {
    let before = Tid(last.0.saturating_add(1));
    let (mut ours, mut theirs) = (Oids::new(store), Oids::new(copy));
    let (mut mine, mut restored) = (ours.next()?, theirs.next()?);
    loop {
        let oid = match (mine, restored) {
            (None, None) => break,
            (Some(oid), None) | (None, Some(oid)) => oid,
            (Some(a), Some(b)) => a.min(b),
        };
        if mine == Some(oid) {
            mine = ours.next()?;
        }
        if restored == Some(oid) {
            restored = theirs.next()?;
        }
        let at = format!("oid {} at {}", hex(oid.0), hex(last.0));
        let current_mine = match current(store, oid, before) {
            Ok(record) => record,
            Err(error) => {
                problems.push(format!("{at}: {error}"));
                continue;
            }
        };
        match (current_mine, current(copy, oid, before)?) {
            (a, b) if a == b => {}
            (None, Some((serial, _))) if serial <= watermark => {}
            _ => problems.push(format!("{at}: the current record differs")),
        }
    }
    Ok(())
}

/// Return the serial and the pickle of the current record of `oid` before
/// `before`, or `None` for a deletion or no record.
fn current(store: &Store, oid: Oid, before: Tid) -> zoe_core::Result<Option<(Tid, Vec<u8>)>> {
    match store.load_before(oid, before) {
        Ok(Some(record)) => Ok(Some((record.serial, record.data))),
        Ok(None) | Err(ZoeError::NotFound { .. }) => Ok(None),
        Err(error) => Err(error),
    }
}

/// The oids that have a record in a store, in ascending order. It reads
/// the record keys a chunk at a time, and no frame.
struct Oids<'a> {
    store: &'a Store,
    chunk: std::vec::IntoIter<Oid>,
    /// The first oid of the next chunk, or `None` at the end.
    from: Option<Oid>,
}

impl<'a> Oids<'a> {
    fn new(store: &'a Store) -> Self {
        Self {
            store,
            chunk: Vec::new().into_iter(),
            from: Some(Oid(0)),
        }
    }

    fn next(&mut self) -> Result<Option<Oid>, Error> {
        loop {
            if let Some(oid) = self.chunk.next() {
                return Ok(Some(oid));
            }
            let Some(from) = self.from else {
                return Ok(None);
            };
            let mut oids: Vec<Oid> = self
                .store
                .record_keys(from, KEY_CHUNK)?
                .into_iter()
                .map(|(oid, _)| oid)
                .collect();
            oids.dedup();
            self.from = oids.last().and_then(|oid| oid.0.checked_add(1)).map(Oid);
            self.chunk = oids.into_iter();
        }
    }
}

fn not_in_dumps(tid: Tid) -> String {
    format!(
        "the store has transaction {}, which the dumps do not have",
        hex(tid.0)
    )
}

/// Compare the transaction `ours` of `store` with the transaction `dumped`
/// of `copy`. When `packed`, a record that `store` lacks is no difference.
fn compare_txn(
    store: &Store,
    copy: &Store,
    ours: &TxnInfo,
    dumped: &TxnInfo,
    packed: bool,
    problems: &mut Problems,
) -> Result<(), Error> {
    let at = hex(ours.tid.0);
    let meta = |txn: &TxnInfo| {
        (
            txn.user.clone(),
            txn.description.clone(),
            txn.extension.clone(),
        )
    };
    if meta(ours) != meta(dumped) {
        problems.push(format!(
            "transaction {at}: the user, the description or the extension differs"
        ));
    }
    let lacks = |oid: Oid| format!("oid {} at {at}: the store lacks the record", hex(oid.0));
    let mut theirs = dumped.records.iter().peekable();
    for record in &ours.records {
        while let Some(other) = theirs.next_if(|other| other.oid < record.oid) {
            if !packed {
                problems.push(lacks(other.oid));
            }
        }
        let oid = hex(record.oid.0);
        match theirs.next_if(|other| other.oid == record.oid) {
            None => problems.push(format!(
                "oid {oid} at {at}: the store has a record that the dumps do not have"
            )),
            Some(other) if other.data != record.data => {
                problems.push(format!("oid {oid} at {at}: the record differs"));
            }
            Some(_) if record.data.is_none() => {}
            Some(_) => match (
                store.load_blob(record.oid, ours.tid),
                copy.load_blob(record.oid, ours.tid)?,
            ) {
                (Ok(mine), restored) if mine == restored => {}
                (Ok(_), _) => problems.push(format!("oid {oid} at {at}: the blob differs")),
                (Err(error), _) => problems.push(format!("oid {oid} at {at}: {error}")),
            },
        }
    }
    if !packed {
        for other in theirs {
            problems.push(lacks(other.oid));
        }
    }
    Ok(())
}

/// Refuse a target that holds a store or other files. A target that does
/// not exist, or an empty directory, passes.
fn check_target(target: &Path) -> Result<(), Error> {
    let refuse = |reason| {
        Err(Error::Target {
            path: target.to_owned(),
            reason,
        })
    };
    match fs::symlink_metadata(target) {
        Ok(meta) if !meta.is_dir() => refuse("not a directory"),
        Ok(_) if exists(&target.join(DATABASE))? => {
            refuse("holds a store; a restore never writes into a store")
        }
        // The restore builds the store beside the target and renames it,
        // and a rename never crosses a filesystem.
        Ok(meta) if meta.dev() != parent_device(target)? => refuse(
            "a mount point; a restore builds the store beside its target, so give a directory inside it",
        ),
        Ok(_) => {
            let mut entries = fs::read_dir(target).map_err(|source| io_error(target, source))?;
            match entries.next() {
                None => Ok(()),
                Some(_) => {
                    refuse("not empty; a restore writes only into a new or an empty directory")
                }
            }
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_error(target, error)),
    }
}

/// Return the device of the parent directory of `path`.
fn parent_device(path: &Path) -> Result<u64, Error> {
    let parent = parent(path);
    fs::metadata(parent)
        .map(|meta| meta.dev())
        .map_err(|source| io_error(parent, source))
}

/// Take the lock of the backup directory `dir`: an exclusive `flock` on a
/// descriptor of the directory itself. The lock adds no file.
fn lock_dir(dir: &Path) -> Result<File, Error> {
    let file = File::open(dir).map_err(|source| io_error(dir, source))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => Err(Error::Locked {
            path: dir.to_owned(),
        }),
        Err(TryLockError::Error(source)) => Err(io_error(dir, source)),
    }
}

/// Remove the temporary dump files of `dir`, which a stopped backup left.
/// The caller holds the lock of `dir`.
fn remove_temp_files(dir: &Path) -> Result<(), Error> {
    let entries = fs::read_dir(dir).map_err(|source| io_error(dir, source))?;
    for entry in entries {
        let entry = entry.map_err(|source| io_error(dir, source))?;
        let temp = entry
            .file_name()
            .to_str()
            .and_then(|name| name.strip_suffix(TEMP_SUFFIX))
            .is_some_and(|name| parse_name(name).is_some());
        if temp {
            let path = entry.path();
            fs::remove_file(&path).map_err(|source| io_error(&path, source))?;
        }
    }
    Ok(())
}

/// Create the directory `path` and its missing parents. Sync the parent of
/// each directory that the call creates, so that its entry is durable.
fn create_dirs(path: &Path) -> io::Result<()> {
    match fs::create_dir(path) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::AlreadyExists => return Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => {
            create_dirs(parent(path))?;
            match fs::create_dir(path) {
                Ok(()) => {}
                Err(error) if error.kind() == ErrorKind::AlreadyExists => return Ok(()),
                Err(error) => return Err(error),
            }
        }
        Err(error) => return Err(error),
    }
    File::open(parent(path))?.sync_all()
}

/// Sync the directory `path`, so that its entries are durable.
fn sync_dir(path: &Path) -> Result<(), Error> {
    File::open(path)
        .and_then(|dir| dir.sync_all())
        .map_err(|source| io_error(path, source))
}

/// Return the directory of `path`. A bare name is in the current directory.
fn parent(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

/// Return the path beside `path` whose name is the name of `path` with
/// `suffix` after it.
fn sibling(path: &Path, suffix: &str) -> Result<PathBuf, Error> {
    let Some(name) = path.file_name() else {
        let error = io::Error::new(ErrorKind::InvalidInput, "the path has no file name");
        return Err(io_error(path, error));
    };
    let mut name = name.to_owned();
    name.push(suffix);
    Ok(parent(path).join(name))
}

/// Tell if something exists at `path`.
fn exists(path: &Path) -> Result<bool, Error> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(error) => Err(io_error(path, error)),
    }
}

fn damaged(path: &Path, reason: String) -> Error {
    Error::Dump {
        path: path.to_owned(),
        reason,
    }
}

fn io_error(path: &Path, source: io::Error) -> Error {
    Error::Io {
        path: path.to_owned(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use zoe_core::TxnInfo;

    use super::*;
    use crate::filestorage::tests::test_dir;

    /// Compare the dumps in `dumps` with `store`, through a copy in
    /// `scratch`.
    fn check_dumps(store: &Store, dumps: &Path, scratch: &Path) -> Result<BackupCheck, Error> {
        verify_backup(dumps, scratch, || Ok::<_, Error>(store))
    }

    /// The record of a blob.
    const BLOB_RECORD: &[u8] = b"\x80\x03cZODB.blob\nBlob\nq\x00.\x80\x03N.";

    /// A record of a `PersistentMapping` that holds `value`, with no
    /// reference.
    fn pickle(value: &[u8]) -> Vec<u8> {
        let mut pickle = b"\x80\x03cpersistent.mapping\nPersistentMapping\nq\x00.".to_vec();
        pickle.extend_from_slice(b"\x80\x03}q\x00X\x01\x00\x00\x00vq\x01C");
        pickle.push(u8::try_from(value.len()).unwrap());
        pickle.extend_from_slice(value);
        pickle.extend_from_slice(b"q\x02s.");
        pickle
    }

    /// A write of a restore transaction: a pickle, a deletion or a blob.
    enum W<'a> {
        Data(u64, &'a [u8]),
        Delete(u64),
        Blob(u64, &'a [u8]),
    }

    /// Commit a restore transaction at `tid` with `writes`.
    fn commit(store: &Store, tid: u64, writes: &[W<'_>]) {
        let description = format!("at {tid}");
        let mut txn = store
            .tpc_begin_restore(Tid(tid), b"user", description.as_bytes(), b"ext")
            .unwrap();
        for write in writes {
            match write {
                W::Data(oid, data) => txn.restore(Oid(*oid), Some(data)).unwrap(),
                W::Delete(oid) => txn.restore(Oid(*oid), None).unwrap(),
                W::Blob(oid, bytes) => {
                    let mut file = store.new_blob_file().unwrap();
                    file.write_all(bytes).unwrap();
                    txn.restore_blob(Oid(*oid), BLOB_RECORD, file).unwrap();
                }
            }
        }
        txn.vote().unwrap();
        assert_eq!(txn.finish().unwrap(), Tid(tid));
    }

    /// A store in `dir` with two transactions, a deletion, a blob and a
    /// sequence.
    fn sample(dir: &Path) -> Store {
        let store = Store::open(dir).unwrap();
        commit(&store, 100, &[W::Data(1, b"one"), W::Data(2, b"two")]);
        commit(&store, 200, &[W::Delete(2), W::Blob(3, &[7; 70_000])]);
        for _ in 0..3 {
            store.sequence("intids").next().unwrap();
        }
        store
    }

    fn transactions(store: &Store) -> Vec<TxnInfo> {
        store
            .iterator(None, None)
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    fn blob(store: &Store, oid: u64, tid: u64) -> Vec<u8> {
        let mut bytes = Vec::new();
        store
            .open_blob(Oid(oid), Tid(tid))
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        bytes
    }

    /// The dump files of `dir`, in name order.
    fn dump_files(dir: &Path) -> Vec<PathBuf> {
        let mut files: Vec<PathBuf> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        files.sort();
        files
    }

    /// Commit `writes` of the namespaces in a normal transaction, each a
    /// flavour, a name, a key and a value or `None` for a delete. Return
    /// the tid.
    fn commit_kv(store: &Store, writes: &[(zoe_core::Flavour, &str, &[u8], Option<&[u8]>)]) -> Tid {
        use zoe_core::Flavour;
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        for &(flavour, name, key, value) in writes {
            match value {
                Some(value) => {
                    let ttl = (flavour == Flavour::Ephemeral)
                        .then(|| std::time::Duration::from_secs(3600));
                    txn.kv_set(flavour, name, key, value, ttl).unwrap();
                }
                None => txn.kv_delete(flavour, name, key).unwrap(),
            }
        }
        txn.vote().unwrap();
        txn.finish().unwrap()
    }

    #[test]
    fn a_dump_holds_every_durable_row_and_the_restore_writes_those_of_the_last_dump() {
        use zoe_core::Flavour::{Durable, Ephemeral};
        let dir = test_dir("backup-durable");
        let (source, dumps, target) = (dir.join("store"), dir.join("dumps"), dir.join("target"));
        let store = sample(&source);
        let first = commit_kv(
            &store,
            &[
                (Durable, "registry", b"a", Some(b"1")),
                (Durable, "registry", b"b", Some(b"2")),
                (Ephemeral, "sessions", b"s", Some(b"secret")),
            ],
        );
        let full = backup(&store, &dumps).unwrap();
        // Two keys and the generation row; the ephemeral row stays out.
        assert_eq!(full.durable_rows, 3);
        let second = commit_kv(
            &store,
            &[
                (Durable, "registry", b"a", None),
                (Durable, "registry", b"c", Some(b"3")),
                (Durable, "other", b"x", Some(b"4")),
            ],
        );
        let incremental = backup(&store, &dumps).unwrap();
        assert_eq!(incremental.durable_rows, 5);
        // A backup with nothing new writes nothing.
        assert_eq!(backup(&store, &dumps).unwrap().dump, None);
        assert!(check_dumps(&store, &dumps, &dir).unwrap().differences.is_empty());
        drop(store);

        let restored = restore(&dumps, &target).unwrap();
        assert_eq!(restored.durable_rows, 5);
        let copy = Store::open(&target).unwrap();
        let get = |name, key: &[u8]| copy.kv_get(Durable, name, key).unwrap();
        assert_eq!(get("registry", b"a"), None);
        assert_eq!(get("registry", b"b").as_deref(), Some(&b"2"[..]));
        assert_eq!(get("registry", b"c").as_deref(), Some(&b"3"[..]));
        assert_eq!(get("other", b"x").as_deref(), Some(&b"4"[..]));
        assert_eq!(copy.kv_generation("registry").unwrap(), second);
        assert!(first < second);
        assert_eq!(copy.kv_get(Ephemeral, "sessions", b"s").unwrap(), None);
        // The rows of the restored store verify: no generation after its
        // last tid. The pickles of the sample are not real pickles.
        let report = crate::verify(&copy, None);
        assert!(report.checksums.is_empty(), "{report:?}");
        assert_eq!(report.durable_keys, 3);
        drop(copy);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_check_of_a_backup_finds_a_durable_row_that_differs() {
        use zoe_core::Flavour::Durable;
        let dir = test_dir("backup-durable-check");
        let (source, dumps) = (dir.join("store"), dir.join("dumps"));
        let store = sample(&source);
        commit_kv(&store, &[(Durable, "registry", b"title", Some(b"site"))]);
        backup(&store, &dumps).unwrap();
        // A change of the row that no commit made: the dumps differ.
        let mut write_row = |payload: &[u8]| {
            let row = DurableRow {
                name: "registry".to_owned(),
                key: b"title".to_vec(),
                payload: payload.to_vec(),
            };
            store.kv_restore_durable(&[row]).unwrap();
        };
        write_row(b"other");
        let check = check_dumps(&store, &dumps, &dir).unwrap();
        assert_eq!(check.differences.len(), 1, "{:?}", check.differences);
        let text = &check.differences[0];
        assert!(text.contains("\"registry\"") && text.contains("another value"), "{text}");
        assert!(!text.contains("title"), "a difference names no key: {text}");
        drop(store);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_restore_reads_a_dump_of_version_1() {
        let dir = test_dir("backup-version-1");
        let (source, dumps, target) = (dir.join("store"), dir.join("dumps"), dir.join("target"));
        let store = sample(&source);
        let full = backup(&store, &dumps).unwrap();
        drop(store);
        // A dump of version 1 has the same layout without durable rows.
        let path = full.dump.unwrap();
        let mut bytes = fs::read(&path).unwrap();
        bytes[7] = 1;
        let end = bytes.len() - HASH;
        let hash = blake3::hash(&bytes[..end]);
        bytes[end..].copy_from_slice(hash.as_bytes());
        fs::write(&path, bytes).unwrap();
        let restored = restore(&dumps, &target).unwrap();
        assert_eq!((restored.transactions, restored.durable_rows), (2, 0));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_name_gives_back_its_tids() {
        let name = dump_name(Tid(0), Tid(0x03f6_db4f_2222_2222));
        assert_eq!(name, "0000000000000000-03f6db4f22222222.zoedump");
        assert_eq!(
            parse_name(&name),
            Some((Tid(0), Tid(0x03f6_db4f_2222_2222)))
        );
        for other in [
            "0000000000000000-03F6DB4F22222222.zoedump",
            "0000000000000000-03f6db4f2222222.zoedump",
            "0000000000000000-03f6db4f22222222.zoedump.tmp",
            "0000000000000000_03f6db4f22222222.zoedump",
            "+000000000000000-03f6db4f22222222.zoedump",
        ] {
            assert_eq!(parse_name(other), None, "{other}");
        }
    }

    #[test]
    fn a_restore_gives_back_the_store() {
        let dir = test_dir("backup-round-trip");
        let (source, dumps, target) = (dir.join("source"), dir.join("dumps"), dir.join("target"));
        let store = sample(&source);
        let full = backup(&store, &dumps).unwrap();
        assert_eq!((full.after, full.last), (Tid::ZERO, Tid(200)));
        assert_eq!((full.transactions, full.records, full.blobs), (2, 4, 1));
        assert_eq!((full.blob_bytes, full.sequences), (70_000, 1));

        commit(
            &store,
            300,
            &[W::Data(1, b"one again"), W::Data(4, b"four")],
        );
        let mut empty = store.tpc_begin(b"", b"empty", b"").unwrap();
        empty.vote().unwrap();
        empty.finish().unwrap();
        // A second block of the sequence: its end is in the next dump.
        let sequence = store.sequence("intids");
        let last_value = (0..120).map(|_| sequence.next().unwrap()).max().unwrap();
        let incremental = backup(&store, &dumps).unwrap();
        assert_eq!(incremental.after, Tid(200));
        assert_eq!(incremental.transactions, 2);
        drop(store);

        let restored = restore(&dumps, &target).unwrap();
        assert_eq!((restored.dumps, restored.transactions), (2, 4));
        assert_eq!(
            (restored.records, restored.blobs, restored.sequences),
            (6, 1, 1)
        );
        assert!(!sibling(&target, WORK_SUFFIX).unwrap().exists());

        let source = Store::open(&source).unwrap();
        let target = Store::open(&target).unwrap();
        assert_eq!(transactions(&source), transactions(&target));
        assert_eq!(source.last_transaction(), target.last_transaction());
        assert_eq!(blob(&target, 3, 200), vec![7; 70_000]);
        assert_eq!(
            source.load_blob(Oid(3), Tid(200)).unwrap(),
            target.load_blob(Oid(3), Tid(200)).unwrap()
        );
        assert!(target.sequence("intids").next().unwrap() > last_value);
        assert!(target.new_oid().unwrap() > Oid(4));
        drop((source, target));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_backup_writes_a_new_file_and_nothing_when_nothing_is_new() {
        let dir = test_dir("backup-files");
        let (source, dumps) = (dir.join("source"), dir.join("dumps"));
        let store = sample(&source);
        backup(&store, &dumps).unwrap();
        let first = dump_files(&dumps);
        assert_eq!(first.len(), 1);
        let bytes = fs::read(&first[0]).unwrap();

        // Nothing new: nothing written.
        let nothing = backup(&store, &dumps).unwrap();
        assert_eq!(nothing.dump, None);
        assert_eq!(dump_files(&dumps), first);

        // A stopped backup leaves a temporary file. The next one removes it.
        let leftover = dumps.join(format!("{}{TEMP_SUFFIX}", dump_name(Tid(200), Tid(250))));
        fs::write(&leftover, b"partial").unwrap();
        let foreign = dumps.join("notes.txt");
        fs::write(&foreign, b"kept").unwrap();
        commit(&store, 300, &[W::Data(1, b"one again")]);
        let written = backup(&store, &dumps).unwrap();
        let name = dump_name(Tid(200), Tid(300));
        assert_eq!(written.dump, Some(dumps.join(&name)));
        assert!(!leftover.exists());
        assert!(foreign.exists());
        assert_eq!(fs::read(&first[0]).unwrap(), bytes);
        drop(store);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_store_gives_a_full_dump_that_a_later_dump_follows() {
        let dir = test_dir("backup-empty");
        let (source, dumps, target) = (dir.join("source"), dir.join("dumps"), dir.join("target"));
        let store = Store::open(&source).unwrap();
        let empty = backup(&store, &dumps).unwrap();
        assert_eq!(empty.dump, Some(dumps.join(dump_name(Tid(0), Tid(0)))));
        assert_eq!(backup(&store, &dumps).unwrap().dump, None);
        commit(&store, 100, &[W::Data(1, b"one")]);
        backup(&store, &dumps).unwrap();
        drop(store);
        let restored = restore(&dumps, &target).unwrap();
        assert_eq!((restored.dumps, restored.transactions), (2, 1));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_damaged_dump_fails_the_restore_before_it_writes_anything() {
        let dir = test_dir("backup-damage");
        let (source, dumps) = (dir.join("source"), dir.join("dumps"));
        let store = sample(&source);
        backup(&store, &dumps).unwrap();
        drop(store);
        let file = dump_files(&dumps).remove(0);
        let good = fs::read(&file).unwrap();
        let len = good.len();
        let target = dir.join("target");

        let flips = [
            0,
            9,
            20,
            HEADER,
            HEADER + 5,
            len / 2,
            len - HASH - 1,
            len - 1,
        ];
        for at in flips {
            let mut bytes = good.clone();
            bytes[at] ^= 0x01;
            fs::write(&file, &bytes).unwrap();
            let error = restore(&dumps, &target).unwrap_err();
            assert!(
                matches!(error, Error::Dump { .. }),
                "a flip at {at}: {error}"
            );
            // After the header, the hash pass finds the damage before a
            // parse reads a length.
            if at >= HEADER {
                assert!(error.to_string().contains("checksum"), "{at}: {error}");
            }
            assert!(!target.exists(), "a flip at {at} left a target");
        }
        for bytes in [
            &good[..len - 1],
            &good[..len / 2],
            &[good.clone(), vec![0]].concat(),
        ] {
            fs::write(&file, bytes).unwrap();
            let error = restore(&dumps, &target).unwrap_err();
            assert!(matches!(error, Error::Dump { .. }), "{error}");
            assert!(!target.exists());
        }
        fs::write(&file, &good).unwrap();
        restore(&dumps, &target).unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_dump_breaks_the_chain() {
        let dir = test_dir("backup-chain");
        let (source, dumps) = (dir.join("source"), dir.join("dumps"));
        let store = sample(&source);
        backup(&store, &dumps).unwrap();
        for tid in [300, 400] {
            commit(&store, tid, &[W::Data(1, format!("{tid}").as_bytes())]);
            backup(&store, &dumps).unwrap();
        }
        let files = dump_files(&dumps);
        assert_eq!(files.len(), 3);
        let target = dir.join("target");

        let middle = fs::read(&files[1]).unwrap();
        fs::remove_file(&files[1]).unwrap();
        let error = restore(&dumps, &target).unwrap_err();
        assert!(error.to_string().contains("a dump is missing"), "{error}");
        // A backup does not add a dump after a gap.
        commit(&store, 500, &[W::Data(1, b"500")]);
        assert!(matches!(backup(&store, &dumps), Err(Error::Dump { .. })));
        fs::write(&files[1], &middle).unwrap();

        fs::rename(&files[0], dir.join("away")).unwrap();
        let error = restore(&dumps, &target).unwrap_err();
        assert!(
            error.to_string().contains("full dump is missing"),
            "{error}"
        );
        fs::rename(dir.join("away"), &files[0]).unwrap();

        // Two dumps that start after the same tid.
        let overlap = dumps.join(dump_name(Tid(200), Tid(350)));
        fs::copy(&files[1], &overlap).unwrap();
        let error = restore(&dumps, &target).unwrap_err();
        assert!(error.to_string().contains("the dumps overlap"), "{error}");
        fs::remove_file(&overlap).unwrap();

        // A dump under the name of another range.
        let renamed = dumps.join(dump_name(Tid(400), Tid(450)));
        fs::copy(&files[2], &renamed).unwrap();
        let error = restore(&dumps, &target).unwrap_err();
        assert!(error.to_string().contains("header"), "{error}");
        fs::remove_file(&renamed).unwrap();

        assert!(!target.exists());
        let empty = dir.join("empty");
        fs::create_dir(&empty).unwrap();
        assert!(matches!(
            restore(&empty, &target),
            Err(Error::NoDump { .. })
        ));
        drop(store);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_backup_refuses_the_dumps_of_another_store() {
        let dir = test_dir("backup-diverged");
        let (one, two, dumps) = (dir.join("one"), dir.join("two"), dir.join("dumps"));
        let first = sample(&one);
        backup(&first, &dumps).unwrap();
        let other = Store::open(&two).unwrap();
        commit(&other, 150, &[W::Data(1, b"other")]);
        commit(&other, 250, &[W::Data(1, b"other again")]);
        let error = backup(&other, &dumps).unwrap_err();
        assert!(matches!(error, Error::BackupDiverged { .. }), "{error}");
        // A store behind the dumps does not continue them.
        let behind = Store::open(dir.join("three")).unwrap();
        commit(&behind, 100, &[W::Data(1, b"one")]);
        let error = backup(&behind, &dumps).unwrap_err();
        assert!(matches!(error, Error::BackupDiverged { .. }), "{error}");
        // A store made after the dumps and packed after their last tid
        // does not continue them either.
        let later = Store::open(dir.join("four")).unwrap();
        commit(&later, 300, &[W::Data(0, &pickle(b"root"))]);
        commit(&later, 400, &[W::Data(1, &pickle(b"later"))]);
        later.pack(Tid(400)).unwrap();
        let error = backup(&later, &dumps).unwrap_err();
        assert!(matches!(error, Error::BackupDiverged { .. }), "{error}");
        assert!(
            error
                .to_string()
                .contains("new full dump into a new directory")
        );
        assert_eq!(dump_files(&dumps).len(), 1);
        drop((first, other, behind, later));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_backup_takes_the_lock_of_its_directory() {
        let dir = test_dir("backup-lock");
        let (source, dumps) = (dir.join("source"), dir.join("dumps"));
        let store = sample(&source);
        fs::create_dir(&dumps).unwrap();
        let held = lock_dir(&dumps).unwrap();
        assert!(matches!(backup(&store, &dumps), Err(Error::Locked { .. })));
        assert!(dump_files(&dumps).is_empty());
        drop(held);
        backup(&store, &dumps).unwrap();
        drop(store);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_restore_writes_only_into_a_new_or_an_empty_directory() {
        let dir = test_dir("backup-target");
        let (source, dumps) = (dir.join("source"), dir.join("dumps"));
        let store = sample(&source);
        backup(&store, &dumps).unwrap();
        drop(store);

        let refused = |target: &Path| match restore(&dumps, target) {
            Err(Error::Target { reason, .. }) => reason,
            other => panic!("{other:?}"),
        };
        assert!(refused(&source).starts_with("holds a store"));
        let busy = dir.join("busy");
        fs::create_dir(&busy).unwrap();
        fs::write(busy.join("file"), b"kept").unwrap();
        assert!(refused(&busy).starts_with("not empty"));
        assert!(refused(&busy.join("file")).starts_with("not a directory"));

        // A work directory that exists can belong to another restore.
        let target = dir.join("target");
        let work = sibling(&target, WORK_SUFFIX).unwrap();
        fs::create_dir(&work).unwrap();
        assert!(refused(&target).starts_with("exists"));
        assert!(work.exists() && !target.exists());
        fs::remove_dir(&work).unwrap();

        // An empty directory takes the store, also given with a slash.
        fs::create_dir(&target).unwrap();
        let with_slash = PathBuf::from(format!("{}/", target.display()));
        restore(&dumps, &with_slash).unwrap();
        assert!(target.join(DATABASE).exists());
        assert!(!work.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_check_of_a_backup_finds_each_difference() {
        let dir = test_dir("backup-check");
        let (source, dumps, scratch) = (dir.join("source"), dir.join("dumps"), dir.join("scratch"));
        let store = sample(&source);
        backup(&store, &dumps).unwrap();

        // A commit after the dumps is no difference.
        commit(&store, 300, &[W::Data(1, b"after")]);
        let check = check_dumps(&store, &dumps, &scratch).unwrap();
        assert_eq!(check.differences, Vec::<String>::new());
        assert_eq!(
            (check.restored.transactions, check.restored.last_tid),
            (2, Tid(200))
        );
        assert_eq!(fs::read_dir(&scratch).unwrap().count(), 0);
        drop(store);

        // Another store with the same tids differs in a record, lacks the
        // blob and a transaction, and has an older sequence.
        let other = Store::open(dir.join("other")).unwrap();
        commit(&other, 100, &[W::Data(1, b"ONE"), W::Data(2, b"two")]);
        commit(&other, 150, &[W::Data(5, b"five")]);
        let check = check_dumps(&other, &dumps, &scratch).unwrap();
        let text = check.differences.join("\n");
        assert!(
            text.contains("oid 0x0000000000000001 at 0x0000000000000064: the record differs"),
            "{text}"
        );
        assert!(
            text.contains("transaction 0x0000000000000096, which the dumps do not have"),
            "{text}"
        );
        assert!(
            text.contains("lacks transaction 0x00000000000000c8"),
            "{text}"
        );
        assert!(
            text.contains("sequence \"intids\": the store has no row"),
            "{text}"
        );
        // At the last tid of the dumps, the current records of oids 1, 2,
        // 3 and 5 differ: another pickle, a deletion, a blob and an object
        // that the dumps do not have.
        for oid in [1, 2, 3, 5] {
            let line = format!(
                "oid {} at 0x00000000000000c8: the current record differs",
                hex(oid)
            );
            assert!(text.contains(&line), "{text}");
        }
        assert_eq!(check.differences.len(), 8, "{text}");
        drop(other);

        // A damaged dump fails the check.
        let file = dump_files(&dumps).remove(0);
        let mut bytes = fs::read(&file).unwrap();
        let middle = bytes.len() / 2;
        bytes[middle] ^= 0x01;
        fs::write(&file, &bytes).unwrap();
        // The check restores the dumps before it opens the store, so it
        // fails before the open.
        let error = verify_backup(&dumps, &scratch, || -> Result<Store, Error> {
            panic!("the check opened the store")
        })
        .unwrap_err();
        assert!(matches!(error, Error::Dump { .. }), "{error}");
        assert_eq!(fs::read_dir(&scratch).unwrap().count(), 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn records_that_a_pack_removed_are_no_difference() {
        let dir = test_dir("backup-pack");
        let (source, dumps, scratch) = (dir.join("source"), dir.join("dumps"), dir.join("scratch"));
        let store = Store::open(&source).unwrap();
        // Oids 1 and 2 are not reachable from the root, so a pack removes
        // every record of them at or before the pack tid.
        commit(
            &store,
            100,
            &[W::Data(0, &pickle(b"root")), W::Data(1, &pickle(b"a"))],
        );
        commit(&store, 200, &[W::Data(1, &pickle(b"b"))]);
        commit(&store, 300, &[W::Data(1, &pickle(b"c"))]);
        commit(&store, 400, &[W::Data(2, &pickle(b"d"))]);
        backup(&store, &dumps).unwrap();
        store.pack(Tid(250)).unwrap();
        assert!(store.load_serial(Oid(1), Tid(200)).is_err());
        let check = check_dumps(&store, &dumps, &scratch).unwrap();
        assert_eq!(check.differences, Vec::<String>::new());

        // A pack before the last tid of the dumps lets the backups go on.
        commit(&store, 500, &[W::Data(0, &pickle(b"root again"))]);
        let written = backup(&store, &dumps).unwrap();
        assert_eq!((written.after, written.transactions), (Tid(400), 1));
        let check = check_dumps(&store, &dumps, &scratch).unwrap();
        assert_eq!(check.differences, Vec::<String>::new());
        assert_eq!((check.restored.dumps, check.restored.transactions), (2, 5));

        // A pack at the last tid of the dumps removes only history that the
        // dumps hold, so the backups go on.
        store.pack(Tid(500)).unwrap();
        assert!(store.load_serial(Oid(0), Tid(100)).is_err());
        assert!(store.load_serial(Oid(2), Tid(400)).is_err());
        let check = check_dumps(&store, &dumps, &scratch).unwrap();
        assert_eq!(check.differences, Vec::<String>::new());
        commit(&store, 600, &[W::Data(0, &pickle(b"root once more"))]);
        let written = backup(&store, &dumps).unwrap();
        assert_eq!((written.after, written.transactions), (Tid(500), 1));
        let check = check_dumps(&store, &dumps, &scratch).unwrap();
        assert_eq!(check.differences, Vec::<String>::new());
        assert_eq!((check.restored.dumps, check.restored.transactions), (3, 6));

        // A pack after the last tid of the dumps stops the backups, and it
        // is a difference. The pack keeps the root at 600, so the last
        // transaction of the dumps stays in the store.
        commit(&store, 700, &[W::Data(3, &pickle(b"f"))]);
        store.pack(Tid(700)).unwrap();
        match backup(&store, &dumps) {
            Err(Error::BackupPacked {
                last, watermark, ..
            }) => assert_eq!((last, watermark), (Tid(600), Tid(700))),
            other => panic!("{other:?}"),
        }
        assert_eq!(dump_files(&dumps).len(), 3);
        let check = check_dumps(&store, &dumps, &scratch).unwrap();
        assert_eq!(check.differences.len(), 1, "{:?}", check.differences);
        assert!(check.differences[0].starts_with(
            "the store was packed at tid 0x00000000000002bc, after the last tid 0x0000000000000258"
        ));
        drop(store);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_store_restored_from_a_part_of_the_dumps_does_not_continue_them() {
        let dir = test_dir("backup-rollback");
        let (source, dumps, scratch) = (dir.join("source"), dir.join("dumps"), dir.join("scratch"));
        let store = Store::open(&source).unwrap();
        commit(
            &store,
            100,
            &[W::Data(0, &pickle(b"root")), W::Data(1, &pickle(b"4"))],
        );
        backup(&store, &dumps).unwrap();
        commit(&store, 200, &[W::Data(1, &pickle(b"5"))]);
        backup(&store, &dumps).unwrap();
        drop(store);

        // A restore of the full dump alone, then other commits.
        let part = dir.join("part");
        fs::create_dir(&part).unwrap();
        let full = dump_files(&dumps).remove(0);
        fs::copy(&full, part.join(full.file_name().unwrap())).unwrap();
        let rolled = dir.join("rolled");
        restore(&part, &rolled).unwrap();
        let rolled = Store::open(&rolled).unwrap();
        for tid in [300, 400, 500] {
            commit(&rolled, tid, &[W::Data(2, &pickle(b"other"))]);
        }
        let error = backup(&rolled, &dumps).unwrap_err();
        assert!(matches!(error, Error::BackupDiverged { .. }), "{error}");
        let check = check_dumps(&rolled, &dumps, &scratch).unwrap();
        let text = check.differences.join("\n");
        assert!(
            text.contains("the store lacks transaction 0x00000000000000c8"),
            "{text}"
        );
        let differs = "oid 0x0000000000000001 at 0x00000000000000c8: the current record differs";
        assert!(text.contains(differs), "{text}");

        // A pack does not hide it.
        rolled.pack(Tid(500)).unwrap();
        let error = backup(&rolled, &dumps).unwrap_err();
        assert!(matches!(error, Error::BackupDiverged { .. }), "{error}");
        let check = check_dumps(&rolled, &dumps, &scratch).unwrap();
        let text = check.differences.join("\n");
        assert!(text.starts_with("the store was packed at tid"), "{text}");
        assert_eq!(dump_files(&dumps).len(), 2);
        drop(rolled);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_check_removes_the_directories_of_a_killed_check() {
        let dir = test_dir("backup-stale");
        let (source, dumps, scratch) = (dir.join("source"), dir.join("dumps"), dir.join("scratch"));
        let store = sample(&source);
        backup(&store, &dumps).unwrap();
        // No process has the id u32::MAX, and this process keeps its own.
        let gone = scratch.join(format!("{CHECK_PREFIX}{}-0{WORK_SUFFIX}", u32::MAX));
        let mine = scratch.join(format!("{CHECK_PREFIX}{}-999", std::process::id()));
        let other = scratch.join("zoe-backup-check-notes");
        for path in [&gone, &mine, &other] {
            fs::create_dir_all(path.join("inside")).unwrap();
        }
        check_dumps(&store, &dumps, &scratch).unwrap();
        assert!(!gone.exists());
        assert!(mine.exists() && other.exists());
        drop(store);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_open_that_fails_fails_the_check_and_removes_the_copy() {
        let dir = test_dir("backup-open");
        let (source, dumps, scratch) = (dir.join("source"), dir.join("dumps"), dir.join("scratch"));
        let store = sample(&source);
        backup(&store, &dumps).unwrap();
        // The copy is already built when the open fails.
        let error = verify_backup(&dumps, &scratch, || {
            Err::<Store, _>(Error::Store(ZoeError::Locked))
        })
        .unwrap_err();
        assert!(matches!(error, Error::Store(_)), "{error}");
        assert_eq!(fs::read_dir(&scratch).unwrap().count(), 0);
        drop(store);
        let _ = fs::remove_dir_all(&dir);
    }
}
