//! The conversion of a ZOE store to a `Data.fs`, the exit path.
//!
//! The conversion writes the transactions of the store that come after the
//! last tid of the file. It writes them into a temporary file beside the
//! file: a copy of the file, or a new file when there is no file. It writes
//! the blob of each record into the blob directory. Then it syncs the
//! temporary file, verifies it against the store, renames it to the final
//! name and syncs the directory. So the final name holds the old file or
//! the whole new file, never a part of it.
//!
//! The conversion holds one transaction of the store at a time, and the
//! offset of the newest record of each oid. The transaction iterator of
//! the store holds the transaction rows and the record keys of the whole
//! store.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, ErrorKind, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use zoe_core::{Oid, Store, Tid, ZoeError};

use crate::blobs::{BlobDir, LAYOUT_FILE, Layout};
use crate::datafs::{BLOB_SAMPLE_STRIDE, Problems, check_blob, compare_txn, stop_problem};
use crate::filestorage::{DataRecord, Reader, TxnHeader, hex};
use crate::writer::{Writer, tail};
use crate::{Error, Report};

/// The suffix of the temporary file beside the `Data.fs`.
const TEMP_SUFFIX: &str = ".new";

/// The suffix of the lock file of FileStorage.
const LOCK_SUFFIX: &str = ".lock";

/// The temporary directory in a blob directory. ZODB uses the same name.
const BLOB_TEMP_DIR: &str = "tmp";

/// The layout that the converter writes.
const BUSHY: &[u8] = b"bushy";

/// The size of the buffer of a blob copy.
const CHUNK: usize = 1 << 16;

/// Convert the store `source` into the FileStorage file `data_fs` and the
/// blob directory `blob_dir`, then verify the file against the store.
///
/// The file has the magic `FS30`. Each transaction of the store becomes a
/// transaction record with the status ` ` and the same tid, user,
/// description and extension bytes. Each record becomes a data record that
/// holds its pickle, and a deletion becomes a data record with a back
/// pointer of 0. The blob of each record goes to `blob_dir` in the bushy
/// layout, read-only, as ZODB writes it. The converter writes no `.index`
/// file: FileStorage builds the index when it opens the file.
///
/// The conversion resumes after the last tid of an existing `data_fs`.
/// That tid must be a tid of the store, else the call fails with
/// `DataFsDiverged` and changes nothing. A file that does not end with a
/// whole transaction fails with `Corrupt`. The conversion never changes
/// the bytes of an existing file: it only adds transactions after them. A
/// resumed file is the file that one run writes.
///
/// The conversion takes the lock of FileStorage, `data_fs` with the suffix
/// `.lock`. A lock that another process holds fails with `Locked`. The
/// conversion writes to `data_fs` with the suffix `.new`, and renames that
/// file at the end. A failed conversion removes it.
///
/// The verification reads the file and compares it with every transaction
/// of the store: the tids, the metadata and every pickle, byte for byte. It
/// checks that the blob directory has every blob of the store with its
/// size, and compares the first blob and then one blob in 16 byte for
/// byte. The report lists each difference in `problems`. A blob that the
/// store does not read is not written, and the report names it.
pub fn to_datafs(source: &Store, data_fs: &Path, blob_dir: &Path) -> Result<Report, Error> {
    let _lock = lock(&sibling(data_fs, LOCK_SUFFIX)?)?;
    let temp = sibling(data_fs, TEMP_SUFFIX)?;
    // A crash can leave the temporary file.
    remove_if_exists(&temp)?;
    let result = convert(source, data_fs, &temp, blob_dir);
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// Do the conversion of `to_datafs` through the temporary file `temp`.
fn convert(source: &Store, data_fs: &Path, temp: &Path, blob_dir: &Path) -> Result<Report, Error> {
    let last = source.last_transaction();
    let existing = match fs::symlink_metadata(data_fs) {
        Ok(_) => Some(tail(data_fs)?),
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(error) => return Err(io_error(data_fs, error)),
    };

    let start = existing.as_ref().map(|tail| tail.last);
    let mut txns = source.iterator(start, Some(last))?.peekable();
    // A file with transactions continues the store only when its last tid
    // is a tid of the store.
    if let Some(file_last) = start.filter(|&tid| tid != Tid::ZERO) {
        match txns.next() {
            Some(Ok(txn)) if txn.tid == file_last => {}
            Some(Err(error)) => return Err(error.into()),
            _ => {
                return Err(Error::DataFsDiverged {
                    path: data_fs.to_owned(),
                    last: file_last,
                });
            }
        }
    }
    let blobs = OutBlobs::prepare(blob_dir)?;

    let mut problems = Problems::default();
    let mut copied = 0;
    let written = if existing.is_some() && txns.peek().is_none() {
        false
    } else {
        let mut writer = match existing {
            Some(tail) => {
                copy(data_fs, temp, tail.end)?;
                Writer::append(temp, tail)?
            }
            None => Writer::create(temp)?,
        };
        for txn in txns.by_ref() {
            let txn = txn?;
            for record in txn.records.iter().filter(|record| record.data.is_some()) {
                if let Some(problem) = blobs.write(source, record.oid, txn.tid)? {
                    problems.push(problem);
                }
            }
            writer.write_txn(&txn)?;
            copied += 1;
        }
        writer.finish()?;
        true
    };
    drop(txns);

    let file = if written { temp } else { data_fs };
    let mut report = verify(file, &blobs.dir, source, last, problems)?;
    report.copied = copied;
    if written {
        fs::rename(temp, data_fs).map_err(|source| io_error(data_fs, source))?;
        sync_dir(parent(data_fs))?;
    }
    Ok(report)
}

/// Take the lock of FileStorage at `path`. ZODB locks this file with
/// `flock` while a FileStorage has the file open for writing.
fn lock(path: &Path) -> Result<File, Error> {
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|source| io_error(path, source))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => Err(Error::Locked {
            path: path.to_owned(),
        }),
        Err(TryLockError::Error(source)) => Err(io_error(path, source)),
    }
}

/// Copy the file `from` to `to`. The copy must have `len` bytes.
fn copy(from: &Path, to: &Path, len: u64) -> Result<(), Error> {
    let copied = fs::copy(from, to).map_err(|source| io_error(to, source))?;
    if copied != len {
        let message = format!("the file changed: it has {copied} bytes, not {len}");
        return Err(io_error(from, io::Error::other(message)));
    }
    Ok(())
}

/// The blob directory of the output.
struct OutBlobs {
    dir: BlobDir,
    /// The directory of the temporary blob files.
    temp: PathBuf,
}

impl OutBlobs {
    /// Create the blob directory `root` with the bushy layout, or open it.
    ///
    /// The `.layout` file comes before the `tmp` directory. A directory
    /// that has `tmp` and no `.layout` gets the lawn layout from ZODB. An
    /// existing directory with another layout than bushy is refused with
    /// `NotBushy`.
    fn prepare(root: &Path) -> Result<Self, Error> {
        let io = |source| io_error(root, source);
        create_dirs(root).map_err(io)?;
        let dir = BlobDir::open(root)?;
        if dir.layout() != Layout::Bushy {
            return Err(Error::NotBushy {
                path: root.to_owned(),
            });
        }
        let layout = root.join(LAYOUT_FILE);
        if !exists(&layout)? {
            let temp = sibling(&layout, TEMP_SUFFIX)?;
            let mut file = File::create(&temp).map_err(|source| io_error(&temp, source))?;
            file.write_all(BUSHY)
                .and_then(|()| file.sync_all())
                .map_err(|source| io_error(&temp, source))?;
            fs::rename(&temp, &layout).map_err(|source| io_error(&layout, source))?;
            sync_dir(root)?;
        }
        let temp = root.join(BLOB_TEMP_DIR);
        create_dirs(&temp).map_err(|source| io_error(&temp, source))?;
        Ok(Self { dir, temp })
    }

    /// Copy the blob of `oid` at `tid` from `store` into the directory.
    ///
    /// The blob goes into a temporary file, which is made read-only and
    /// synced. Then the file is renamed into place, and its directory is
    /// synced. A record without a blob writes nothing. Return a problem
    /// when the store does not read the blob: a missing file, or a size or
    /// a hash that does not match. Then nothing is written.
    fn write(&self, store: &Store, oid: Oid, tid: Tid) -> Result<Option<String>, Error> {
        let problem = |error: &dyn std::fmt::Display| {
            let at = format!("oid {} at {}", hex(oid.0), hex(tid.0));
            Some(format!("{at}: the store does not read the blob: {error}"))
        };
        let mut blob = match store.open_blob(oid, tid) {
            Ok(blob) => blob,
            Err(ZoeError::NotFound { .. }) => return Ok(None),
            Err(error @ ZoeError::CorruptBlob { .. }) => return Ok(problem(&error)),
            Err(error) => return Err(error.into()),
        };
        let temp = self
            .temp
            .join(format!("{:016x}-{:016x}.blob", oid.0, tid.0));
        match write_blob_file(&mut blob, &temp) {
            Ok(None) => {}
            Ok(Some(error)) => {
                let _ = fs::remove_file(&temp);
                return Ok(problem(&error));
            }
            Err(error) => {
                let _ = fs::remove_file(&temp);
                return Err(error);
            }
        }
        let path = self.dir.path(oid, tid);
        let dir = parent(&path);
        create_dirs(dir).map_err(|source| io_error(dir, source))?;
        fs::rename(&temp, &path).map_err(|source| io_error(&path, source))?;
        sync_dir(dir)?;
        Ok(None)
    }
}

/// Copy `blob` into a new file at `path`. Make the file read-only, as ZODB
/// makes a committed blob file, and sync it.
///
/// Return the error of `blob` when it fails its check of the size and the
/// hash. Any other error of `blob` is a `Store` error.
fn write_blob_file(blob: &mut impl Read, path: &Path) -> Result<Option<io::Error>, Error> {
    let io = |source| io_error(path, source);
    let mut file = File::create(path).map_err(io)?;
    let mut buf = vec![0; CHUNK];
    loop {
        let n = match blob.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == ErrorKind::InvalidData => return Ok(Some(error)),
            Err(error) => return Err(Error::Store(error.into())),
        };
        file.write_all(&buf[..n]).map_err(io)?;
    }
    let mut permissions = file.metadata().map_err(io)?.permissions();
    permissions.set_mode(permissions.mode() & !0o222);
    file.set_permissions(permissions).map_err(io)?;
    file.sync_all().map_err(io)?;
    Ok(None)
}

/// Compare the FileStorage file at `path` with every transaction of
/// `store` up to `last`, and return the report. `problems` holds the
/// problems that the copy found.
///
/// The counts of the report describe the store. A transaction of the store
/// that the file does not have, and the reverse, is a problem.
fn verify(
    path: &Path,
    blobs: &BlobDir,
    store: &Store,
    last: Tid,
    mut problems: Problems,
) -> Result<Report, Error> {
    let mut report = Report {
        transactions: 0,
        copied: 0,
        records: 0,
        blobs: 0,
        last_tid: Tid::ZERO,
        problems: Vec::new(),
    };
    let mut reader = Reader::open(path)?;
    let mut next = read_txn(&mut reader)?;
    for txn in store.iterator(None, Some(last))? {
        let txn = match txn {
            Ok(txn) => txn,
            Err(error) => {
                problems.push(format!("the store does not read: {error}"));
                continue;
            }
        };
        report.transactions += 1;
        report.records += txn.records.len() as u64;
        report.last_tid = txn.tid;

        while let Some((header, _)) = &next
            && header.tid < txn.tid
        {
            problems.push(not_in_store(header.tid));
            next = read_txn(&mut reader)?;
        }
        match &next {
            Some((header, records)) if header.tid == txn.tid => {
                compare_txn(header, records, &txn, &mut problems);
                next = read_txn(&mut reader)?;
            }
            _ => problems.push(format!("the Data.fs lacks transaction {}", hex(txn.tid.0))),
        }

        for record in txn.records.iter().filter(|record| record.data.is_some()) {
            let (oid, tid) = (record.oid, txn.tid);
            let at = format!("oid {} at {}", hex(oid.0), hex(tid.0));
            match store.blob_path(oid, tid) {
                Ok(_) => {}
                Err(ZoeError::NotFound { .. }) => continue,
                Err(error) => {
                    problems.push(format!("{at}: the store does not read the blob: {error}"));
                    continue;
                }
            }
            let sample = report.blobs.is_multiple_of(BLOB_SAMPLE_STRIDE);
            report.blobs += 1;
            let copy = blobs.path(oid, tid);
            let size = match fs::metadata(&copy) {
                Ok(meta) => meta.len(),
                Err(error) if error.kind() == ErrorKind::NotFound => {
                    problems.push(format!(
                        "{at}: the blob directory has no blob file {}",
                        copy.display()
                    ));
                    continue;
                }
                Err(error) => return Err(io_error(&copy, error)),
            };
            if let Some(problem) = check_blob(store, oid, tid, &copy, size, sample)? {
                problems.push(format!("{at}: {problem}"));
            }
        }
    }
    while let Some((header, _)) = next {
        problems.push(not_in_store(header.tid));
        next = read_txn(&mut reader)?;
    }
    if let Some(problem) = reader.end().and_then(stop_problem) {
        problems.summary(problem);
    }
    report.problems = problems.finish();
    Ok(report)
}

/// Read the next transaction of `reader` with its records.
fn read_txn(reader: &mut Reader) -> Result<Option<(TxnHeader, Vec<DataRecord>)>, Error> {
    let Some(header) = reader.next_txn()? else {
        return Ok(None);
    };
    let records = reader.records(&header).collect::<Result<_, _>>()?;
    Ok(Some((header, records)))
}

fn not_in_store(tid: Tid) -> String {
    format!(
        "the Data.fs has transaction {}, which the store does not have",
        hex(tid.0)
    )
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

/// Return the path of `path` with `suffix` after its file name.
fn sibling(path: &Path, suffix: &str) -> Result<PathBuf, Error> {
    let Some(name) = path.file_name() else {
        let error = io::Error::new(ErrorKind::InvalidInput, "the path has no file name");
        return Err(io_error(path, error));
    };
    let mut name = name.to_owned();
    name.push(suffix);
    Ok(path.with_file_name(name))
}

/// Tell if something exists at `path`.
fn exists(path: &Path) -> Result<bool, Error> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(error) => Err(io_error(path, error)),
    }
}

/// Remove the file at `path`, if it exists.
fn remove_if_exists(path: &Path) -> Result<(), Error> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_error(path, error)),
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
    use std::collections::BTreeMap;

    use super::*;
    use crate::filestorage::tests::test_dir;
    use crate::from_datafs;

    /// The record of a blob.
    const BLOB: &[u8] = b"\x80\x03cZODB.blob\nBlob\nq\x00.\x80\x03N.";

    /// Commit `writes` and the blobs `blobs` in one transaction of `store`,
    /// with the description `description`.
    fn commit(
        store: &Store,
        description: &[u8],
        writes: &[(u64, &[u8])],
        blobs: &[(u64, &[u8])],
    ) -> Tid {
        let serial = |oid| match store.load(Oid(oid)) {
            Ok((_, serial)) => serial,
            Err(_) => Tid::ZERO,
        };
        let mut txn = store.tpc_begin(b"user", description, b"").unwrap();
        for &(oid, data) in writes {
            txn.store(Oid(oid), serial(oid), data).unwrap();
        }
        for &(oid, bytes) in blobs {
            let mut file = store.new_blob_file().unwrap();
            file.write_all(bytes).unwrap();
            txn.store_blob(Oid(oid), serial(oid), BLOB, file).unwrap();
        }
        txn.vote().unwrap();
        txn.finish().unwrap()
    }

    /// A directory for the test `name` with a store in it.
    fn setup(name: &str) -> (PathBuf, Store) {
        let dir = test_dir(name);
        let store = Store::open(dir.join("store")).unwrap();
        (dir, store)
    }

    fn convert(store: &Store, dir: &Path) -> Result<Report, Error> {
        to_datafs(store, &dir.join("Data.fs"), &dir.join("blobs"))
    }

    /// The transactions of a file: the tid, the metadata and the data of
    /// the last record of each oid.
    type Txns = Vec<(Tid, [Vec<u8>; 3], BTreeMap<Oid, Option<Vec<u8>>>)>;

    fn read_file(path: &Path) -> Txns {
        let mut reader = Reader::open(path).unwrap();
        let mut txns = Vec::new();
        while let Some((header, records)) = read_txn(&mut reader).unwrap() {
            let records = records.into_iter().map(|r| (r.oid, r.data)).collect();
            let meta = [header.user, header.description, header.extension];
            txns.push((header.tid, meta, records));
        }
        txns
    }

    #[test]
    fn a_data_fs_round_trips_through_zoe_with_the_same_transactions() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data-fs");
        let (dir, store) = setup("exit-round-trip");
        let read = from_datafs(
            &fixture.join("Data.fs"),
            Some(&fixture.join("blobs-bushy")),
            &store,
        )
        .unwrap();
        assert!(read.problems.is_empty(), "{read:?}");
        let report = convert(&store, &dir).unwrap();
        assert!(report.problems.is_empty(), "{report:?}");

        // Both files hold the same transactions and pickles. The source
        // holds some of them behind back pointers.
        let source = read_file(&fixture.join("Data.fs"));
        assert_eq!(read_file(&dir.join("Data.fs")), source);
        assert_eq!(report.transactions, source.len() as u64);
        // Both blob directories hold the same blob files.
        let (from, to) = (
            BlobDir::open(&fixture.join("blobs-bushy")).unwrap(),
            BlobDir::open(&dir.join("blobs")).unwrap(),
        );
        let mut blobs = 0;
        for (tid, _, records) in &source {
            for (oid, data) in records {
                if data.as_deref() == Some(BLOB) {
                    let bytes = fs::read(from.path(*oid, *tid)).unwrap();
                    assert_eq!(fs::read(to.path(*oid, *tid)).unwrap(), bytes);
                    blobs += 1;
                }
            }
        }
        assert_eq!((report.blobs, blobs), (2, 2));
    }

    #[test]
    fn the_blob_directory_is_bushy_and_its_files_are_read_only() {
        let (dir, store) = setup("exit-blob-dir");
        let tid = commit(&store, b"", &[(1, b"a")], &[(2, b"blob")]);
        let report = convert(&store, &dir).unwrap();
        assert!(report.problems.is_empty(), "{report:?}");
        assert_eq!(report.blobs, 1);

        let blobs = dir.join("blobs");
        assert_eq!(fs::read(blobs.join(".layout")).unwrap(), b"bushy");
        assert!(blobs.join("tmp").is_dir());
        assert_eq!(fs::read_dir(blobs.join("tmp")).unwrap().count(), 0);
        let path = BlobDir::open(&blobs).unwrap().path(Oid(2), tid);
        assert!(path.ends_with(format!(
            "0x00/0x00/0x00/0x00/0x00/0x00/0x00/0x02/0x{:016x}.blob",
            tid.0
        )));
        assert_eq!(fs::read(&path).unwrap(), b"blob");
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o222, 0, "{mode:o}");
    }

    #[test]
    fn an_empty_store_gives_a_file_with_only_the_magic() {
        let (dir, store) = setup("exit-empty");
        let report = convert(&store, &dir).unwrap();
        assert_eq!((report.transactions, report.copied), (0, 0));
        assert!(report.problems.is_empty(), "{report:?}");
        assert_eq!(fs::read(dir.join("Data.fs")).unwrap(), b"FS30");
    }

    #[test]
    fn a_file_that_the_store_does_not_continue_is_refused_and_kept() {
        let (dir, store) = setup("exit-diverged");
        commit(&store, b"", &[(1, b"a")], &[]);
        convert(&store, &dir).unwrap();
        let data_fs = dir.join("Data.fs");
        let before = fs::read(&data_fs).unwrap();

        // Another store with other tids.
        let other = Store::open(dir.join("other")).unwrap();
        let mut txn = other
            .tpc_begin_restore(Tid(store.last_transaction().0 - 1), b"", b"", b"")
            .unwrap();
        txn.restore(Oid(1), Some(b"x")).unwrap();
        txn.vote().unwrap();
        txn.finish().unwrap();
        let error = convert(&other, &dir).unwrap_err();
        let last = store.last_transaction();
        assert!(
            matches!(&error, Error::DataFsDiverged { last: found, .. } if *found == last),
            "{error:?}"
        );
        assert!(
            error.to_string().contains(&format!("{:#018x}", last.0)),
            "{error}"
        );
        // An empty store does not continue the file either.
        let empty = Store::open(dir.join("empty")).unwrap();
        let error = convert(&empty, &dir).unwrap_err();
        assert!(matches!(error, Error::DataFsDiverged { .. }), "{error:?}");

        assert_eq!(fs::read(&data_fs).unwrap(), before);
        assert!(!dir.join("Data.fs.new").exists());
    }

    #[test]
    fn a_file_that_is_not_a_whole_data_fs_is_refused_and_kept() {
        let (dir, store) = setup("exit-not-data-fs");
        commit(&store, b"", &[(1, b"a")], &[]);
        let data_fs = dir.join("Data.fs");
        fs::write(&data_fs, b"not a Data.fs").unwrap();
        let error = convert(&store, &dir).unwrap_err();
        assert!(matches!(error, Error::NotDataFs { .. }), "{error:?}");
        assert_eq!(fs::read(&data_fs).unwrap(), b"not a Data.fs");

        // A file that ends inside a transaction.
        fs::remove_file(&data_fs).unwrap();
        convert(&store, &dir).unwrap();
        let mut bytes = fs::read(&data_fs).unwrap();
        bytes.pop();
        fs::write(&data_fs, &bytes).unwrap();
        let error = convert(&store, &dir).unwrap_err();
        assert!(matches!(error, Error::Corrupt { pos: 4, .. }), "{error:?}");
        assert_eq!(fs::read(&data_fs).unwrap(), bytes);
        assert!(!dir.join("Data.fs.new").exists());
    }

    #[test]
    fn a_failed_conversion_leaves_no_file() {
        let (dir, store) = setup("exit-failed");
        commit(&store, b"", &[(1, b"a")], &[]);
        commit(&store, &vec![b'x'; 65536], &[(1, b"b")], &[]);
        let error = convert(&store, &dir).unwrap_err();
        assert!(
            matches!(
                error,
                Error::TooLong {
                    field: "description",
                    len: 65536,
                    ..
                }
            ),
            "{error:?}"
        );
        assert!(!dir.join("Data.fs").exists());
        assert!(!dir.join("Data.fs.new").exists());
    }

    #[test]
    fn a_failed_resume_keeps_the_file() {
        let (dir, store) = setup("exit-failed-resume");
        commit(&store, b"", &[(1, b"a")], &[]);
        convert(&store, &dir).unwrap();
        let before = fs::read(dir.join("Data.fs")).unwrap();
        commit(&store, b"", &[(1, b"b")], &[]);
        commit(&store, &vec![b'x'; 65536], &[(1, b"c")], &[]);
        let error = convert(&store, &dir).unwrap_err();
        assert!(matches!(error, Error::TooLong { .. }), "{error:?}");
        assert_eq!(fs::read(dir.join("Data.fs")).unwrap(), before);
        assert!(!dir.join("Data.fs.new").exists());
    }

    #[test]
    fn a_leftover_temporary_file_is_replaced() {
        let (dir, store) = setup("exit-leftover");
        commit(&store, b"", &[(1, b"a")], &[]);
        fs::write(dir.join("Data.fs.new"), b"the rest of a crash").unwrap();
        let report = convert(&store, &dir).unwrap();
        assert!(report.problems.is_empty(), "{report:?}");
        assert_eq!(read_file(&dir.join("Data.fs")).len(), 1);
        assert!(!dir.join("Data.fs.new").exists());
    }

    #[test]
    fn a_lock_that_is_held_is_refused() {
        let (dir, store) = setup("exit-locked");
        commit(&store, b"", &[(1, b"a")], &[]);
        let lock = File::create(dir.join("Data.fs.lock")).unwrap();
        lock.try_lock().unwrap();
        let error = convert(&store, &dir).unwrap_err();
        assert!(matches!(error, Error::Locked { .. }), "{error:?}");
        assert!(!dir.join("Data.fs").exists());
        drop(lock);
        convert(&store, &dir).unwrap();
    }

    #[test]
    fn a_lawn_blob_directory_is_refused() {
        let (dir, store) = setup("exit-lawn");
        commit(&store, b"", &[(1, b"a")], &[]);
        fs::create_dir(dir.join("blobs")).unwrap();
        fs::write(dir.join("blobs/.layout"), "lawn").unwrap();
        let error = convert(&store, &dir).unwrap_err();
        assert!(matches!(error, Error::NotBushy { .. }), "{error:?}");
        assert!(!dir.join("Data.fs").exists());
    }

    #[test]
    fn a_blob_that_the_store_does_not_read_is_reported_and_not_written() {
        let (dir, store) = setup("exit-bad-blob");
        let tid = commit(&store, b"", &[], &[(1, b"good"), (2, b"bad!")]);
        // A blob file of the store changes, and keeps its size.
        let path = store.blob_path(Oid(2), tid).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        fs::write(&path, b"evil").unwrap();

        let report = convert(&store, &dir).unwrap();
        let at = format!("oid 0x0000000000000002 at {}", hex(tid.0));
        assert_eq!(report.problems.len(), 2, "{report:?}");
        assert!(
            report.problems[0].starts_with(&format!("{at}: the store does not read the blob")),
            "{report:?}"
        );
        assert!(
            report.problems[1].starts_with(&format!("{at}: the blob directory has no blob file")),
            "{report:?}"
        );
        let blobs = BlobDir::open(&dir.join("blobs")).unwrap();
        assert_eq!(fs::read(blobs.path(Oid(1), tid)).unwrap(), b"good");
        assert!(!blobs.path(Oid(2), tid).exists());
        assert_eq!(report.blobs, 2);
    }

    #[test]
    fn the_verification_reports_a_file_that_differs_from_the_store() {
        let (dir, store) = setup("exit-differs");
        let tid = commit(&store, b"", &[(1, b"a"), (2, b"b")], &[]);
        // A file with the tid of the store and other records.
        let data_fs = dir.join("Data.fs");
        let mut writer = Writer::create(&data_fs).unwrap();
        writer
            .write_txn(&zoe_core::TxnInfo {
                tid,
                user: b"other".to_vec(),
                description: b"".to_vec(),
                extension: b"".to_vec(),
                records: vec![zoe_core::RecordInfo {
                    oid: Oid(1),
                    tid,
                    data: Some(b"x".to_vec()),
                }],
            })
            .unwrap();
        writer.finish().unwrap();

        let report = convert(&store, &dir).unwrap();
        assert_eq!((report.transactions, report.copied), (1, 0));
        let txn = format!("transaction {}", hex(tid.0));
        assert_eq!(
            report.problems,
            [
                format!("{txn}: the user differs"),
                format!("{txn}: the record of oid 0x0000000000000001 differs"),
                format!(
                    "{txn}: the store has a record of oid 0x0000000000000002, \
                     which the Data.fs does not have"
                ),
            ]
        );
    }
}
