//! The conversion of a `Data.fs` to a ZOE store.
//!
//! The conversion makes two passes over the file. The copy pass skips the
//! transactions up to the last tid of the store and restores each later
//! transaction under its own tid, with its blobs. The verification pass
//! reads the file again, up to where the copy pass stopped, and compares
//! it with every transaction of the store.
//!
//! The copy pass holds one record at a time. The store holds the records of
//! a restore transaction in memory until its finish. The verification pass
//! holds one transaction of the file and one of the store. The transaction
//! iterator of the store also holds the transaction rows and the record
//! keys of the whole store.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, ErrorKind, Read};
use std::path::{Path, PathBuf};

use zoe_core::{Oid, Store, Tid, TxnInfo, ZoeError};

use crate::blobs::{BlobDir, is_blob_record};
use crate::filestorage::{Cut, DataRecord, End, Reader, TxnHeader, hex};
use crate::{Error, Report};

/// The verification compares the first blob, then one blob in this many,
/// byte for byte. It checks the size of every other blob. The copy hashes
/// each blob as it writes it, so the sample checks the read path of the
/// store against the source.
const BLOB_SAMPLE_STRIDE: u64 = 16;

/// The report lists at most this many problems about single transactions,
/// records and blobs, then a count of the others.
const MAX_PROBLEMS: usize = 100;

/// The size of the buffers of the blob comparison.
const CHUNK: usize = 1 << 16;

/// Convert the FileStorage file `data_fs` into the store `target`, then
/// verify the store against the file.
///
/// `blob_dir` is the blob directory of the file, in the bushy or the lawn
/// layout. Without it, the conversion copies no blob, and the report counts
/// the blob records of the file as a problem.
///
/// The conversion resumes after the last tid of `target`. That tid must be
/// a tid of the file, else the call fails with `Diverged` before it copies
/// anything. Each transaction is restored under its own tid, with its
/// user, its description and its extension bytes unchanged.
///
/// The verification compares the tids, the metadata and every pickle of
/// the file with those of the store, checks that every blob is in the
/// store with its size, and compares a sample of blobs byte for byte. The
/// report lists each difference in `problems`.
///
/// A file that is not a FileStorage file fails with `NotDataFs`, and a file
/// that breaks the format fails with `Corrupt`. The transactions copied
/// before such an error stay in the store, so a later call resumes after
/// them.
pub fn from_datafs(
    data_fs: &Path,
    blob_dir: Option<&Path>,
    target: &Store,
) -> Result<Report, Error> {
    let mut reader = Reader::open(data_fs)?;
    let blobs = blob_dir.map(BlobDir::open).transpose()?;
    let copied = copy(&mut reader, blobs.as_ref(), target)?;
    let stop = reader.end().unwrap_or(End::Clean);
    let reader = Reader::open_to(data_fs, reader.next_offset())?;
    let mut report = verify(reader, stop, blobs.as_ref(), target)?;
    report.copied = copied;
    Ok(report)
}

/// Copy the transactions of `reader` that are after the last tid of
/// `target`. Return the count of copied transactions.
fn copy(reader: &mut Reader, blobs: Option<&BlobDir>, target: &Store) -> Result<u64, Error> {
    let last = target.last_transaction();
    // An empty store continues every file. A store that is not empty
    // continues the file only when its last tid is a tid of the file.
    let mut continues = last == Tid::ZERO;
    let mut copied = 0;
    while let Some(txn) = reader.next_txn()? {
        if txn.tid <= last {
            continues = txn.tid == last;
            continue;
        }
        if !continues {
            return Err(Error::Diverged { last });
        }
        copy_txn(reader, &txn, blobs, target)?;
        copied += 1;
    }
    if !continues {
        return Err(Error::Diverged { last });
    }
    Ok(copied)
}

/// Restore the transaction `header` of `reader` in `target`.
///
/// A blob record with a file in `blobs` is staged with `store_blob`. Its
/// serial is the current serial of the oid in the store, so the serial
/// check of the vote passes. Each other record, a record without data
/// included, is staged with `restore`.
fn copy_txn(
    reader: &mut Reader,
    header: &TxnHeader,
    blobs: Option<&BlobDir>,
    target: &Store,
) -> Result<(), Error> {
    let mut txn = target.tpc_begin_restore(
        header.tid,
        &header.user,
        &header.description,
        &header.extension,
    )?;
    for record in reader.records(header) {
        let DataRecord { oid, data } = record?;
        let source = match (&data, blobs) {
            (Some(pickle), Some(blobs)) if is_blob_record(pickle) => {
                open_source(&blobs.path(oid, header.tid))?
            }
            _ => None,
        };
        match (data, source) {
            (Some(pickle), Some((path, mut source))) => {
                let mut file = target.new_blob_file()?;
                io::copy(&mut source, &mut file).map_err(|source| Error::Io { path, source })?;
                let serial = current_serial(target, oid)?;
                txn.store_blob(oid, serial, &pickle, file)?;
            }
            (data, _) => txn.restore(oid, data.as_deref())?,
        }
    }
    txn.vote()?;
    txn.finish()?;
    Ok(())
}

/// Open the blob file `path` of the source. A missing file gives `None`.
/// The verification reports it.
fn open_source(path: &Path) -> Result<Option<(PathBuf, File)>, Error> {
    match File::open(path) {
        Ok(file) => Ok(Some((path.to_owned(), file))),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(source) => Err(Error::Io {
            path: path.to_owned(),
            source,
        }),
    }
}

/// Return the serial of the newest record of `oid` in `store`, a deletion
/// included, or `Tid::ZERO` when `oid` has no record.
fn current_serial(store: &Store, oid: Oid) -> Result<Tid, Error> {
    match store.history(oid, 1) {
        Ok(history) => Ok(history.first().map_or(Tid::ZERO, |entry| entry.tid)),
        Err(ZoeError::NotFound { .. }) => Ok(Tid::ZERO),
        Err(error) => Err(error.into()),
    }
}

/// The problems that a verification found.
///
/// A summary problem is about the whole source. The summary problems come
/// first, and the list keeps all of them. The list keeps `MAX_PROBLEMS`
/// other problems, and a last line counts the others.
#[derive(Default)]
struct Problems {
    summary: Vec<String>,
    list: Vec<String>,
    more: u64,
}

impl Problems {
    fn summary(&mut self, problem: String) {
        self.summary.push(problem);
    }

    fn push(&mut self, problem: String) {
        if self.list.len() < MAX_PROBLEMS {
            self.list.push(problem);
        } else {
            self.more += 1;
        }
    }

    fn finish(mut self) -> Vec<String> {
        self.summary.append(&mut self.list);
        if self.more > 0 {
            self.summary.push(format!("{} more problems", self.more));
        }
        self.summary
    }
}

/// Compare the transactions of `reader` with every transaction of
/// `target`, and return the report. `stop` tells where the copy pass
/// stopped.
fn verify(
    mut reader: Reader,
    stop: End,
    blobs: Option<&BlobDir>,
    target: &Store,
) -> Result<Report, Error> {
    let mut report = Report {
        transactions: 0,
        copied: 0,
        records: 0,
        blobs: 0,
        last_tid: Tid::ZERO,
        problems: Vec::new(),
    };
    let mut problems = Problems::default();
    if let Some(problem) = stop_problem(stop) {
        problems.summary(problem);
    }
    // The blob records of the file when no blob directory is given.
    let mut without_dir = 0u64;
    let mut stored = target.iterator(None, None)?.peekable();

    while let Some(header) = reader.next_txn()? {
        let records: Vec<DataRecord> = reader.records(&header).collect::<Result<_, _>>()?;
        report.transactions += 1;
        report.records += records.len() as u64;
        report.last_tid = header.tid;

        // A transaction of the store before this one is not in the file.
        while let Some(extra) =
            stored.next_if(|txn| !txn.as_ref().is_ok_and(|txn| txn.tid >= header.tid))
        {
            problems.push(not_in_file(extra));
        }
        let txn = stored.next_if(|txn| txn.as_ref().is_ok_and(|txn| txn.tid == header.tid));
        let in_store = match txn {
            Some(Ok(txn)) => {
                compare_txn(&header, &records, &txn, &mut problems);
                true
            }
            _ => {
                problems.push(format!("the store lacks transaction {}", hex(header.tid.0)));
                false
            }
        };

        for record in &records {
            let Some(pickle) = &record.data else {
                continue;
            };
            if !is_blob_record(pickle) {
                continue;
            }
            let Some(blobs) = blobs else {
                without_dir += 1;
                continue;
            };
            let at = format!("oid {} at {}", hex(record.oid.0), hex(header.tid.0));
            let source = blobs.path(record.oid, header.tid);
            let size = match fs::metadata(&source) {
                Ok(meta) => meta.len(),
                Err(error) if error.kind() == ErrorKind::NotFound => {
                    problems.push(format!(
                        "{at}: the blob directory has no blob file {}",
                        source.display()
                    ));
                    continue;
                }
                Err(source_error) => {
                    return Err(Error::Io {
                        path: source,
                        source: source_error,
                    });
                }
            };
            let sample = report.blobs.is_multiple_of(BLOB_SAMPLE_STRIDE);
            report.blobs += 1;
            if in_store
                && let Some(problem) =
                    check_blob(target, record.oid, header.tid, &source, size, sample)?
            {
                problems.push(format!("{at}: {problem}"));
            }
        }
    }
    for extra in stored {
        problems.push(not_in_file(extra));
    }

    if without_dir > 0 {
        problems.summary(format!(
            "the Data.fs has {without_dir} blob records, and no blob directory was given"
        ));
    }
    // The verification reads only up to where the copy stopped. So it
    // stops early only when the file changed between the two passes.
    if let Some(problem) = reader.end().and_then(stop_problem) {
        problems.summary(problem);
    }
    report.problems = problems.finish();
    Ok(report)
}

/// Describe a stop of the reader before the end of the file. A clean end
/// and a commit in progress at the end are not problems.
fn stop_problem(end: End) -> Option<String> {
    let End::Cut { pos, cut } = end else {
        return None;
    };
    let what = match cut {
        Cut::Truncated => format!("the Data.fs ends inside the transaction at offset {pos}"),
        Cut::Junk => format!(
            "the Data.fs holds no valid transaction from offset {pos} to its end, \
             as after a crash"
        ),
        Cut::Commit => format!(
            "the transaction at offset {pos} has the status c, \
             but it is not a whole last transaction"
        ),
    };
    Some(format!("{what}: the conversion stops there"))
}

/// Describe a transaction of the store that the file does not have, or an
/// error of the transaction iterator.
fn not_in_file(txn: Result<TxnInfo, ZoeError>) -> String {
    match txn {
        Ok(txn) => format!(
            "the store has transaction {}, which the Data.fs does not have",
            hex(txn.tid.0)
        ),
        Err(error) => format!("the store does not read: {error}"),
    }
}

/// Compare the transaction `header` of the file and its `records` with
/// the transaction `txn` of the store.
fn compare_txn(header: &TxnHeader, records: &[DataRecord], txn: &TxnInfo, problems: &mut Problems) {
    let tid = hex(header.tid.0);
    for (name, file, store) in [
        ("user", &header.user, &txn.user),
        ("description", &header.description, &txn.description),
        ("extension", &header.extension, &txn.extension),
    ] {
        if file != store {
            problems.push(format!("transaction {tid}: the {name} differs"));
        }
    }

    // The last record of an oid wins, as in a restore transaction.
    let file: BTreeMap<Oid, Option<&[u8]>> = records
        .iter()
        .map(|record| (record.oid, record.data.as_deref()))
        .collect();
    let store: BTreeMap<Oid, Option<&[u8]>> = txn
        .records
        .iter()
        .map(|record| (record.oid, record.data.as_deref()))
        .collect();
    for (oid, data) in &file {
        let oid_hex = hex(oid.0);
        match store.get(oid) {
            None => problems.push(format!(
                "transaction {tid}: the store lacks the record of oid {oid_hex}"
            )),
            Some(stored) if stored != data => problems.push(format!(
                "transaction {tid}: the record of oid {oid_hex} differs"
            )),
            Some(_) => {}
        }
    }
    for oid in store.keys().filter(|oid| !file.contains_key(oid)) {
        problems.push(format!(
            "transaction {tid}: the store has a record of oid {}, which the Data.fs does not have",
            hex(oid.0)
        ));
    }
}

/// Check that `target` holds the blob of `oid` at `tid` with `size`
/// bytes. When `sample` is true, also compare it byte for byte with the
/// blob file `source`, and check its hash. Return the problem found.
fn check_blob(
    target: &Store,
    oid: Oid,
    tid: Tid,
    source: &Path,
    size: u64,
    sample: bool,
) -> Result<Option<String>, Error> {
    let path = match target.blob_path(oid, tid) {
        Ok(path) => path,
        Err(ZoeError::NotFound { .. }) => return Ok(Some("the store has no blob".into())),
        Err(error) => return Ok(Some(format!("the store does not read the blob: {error}"))),
    };
    match fs::metadata(&path) {
        Ok(meta) if meta.len() == size => {}
        Ok(meta) => {
            return Ok(Some(format!(
                "the sizes differ: {} bytes in the store, {size} in the blob file",
                meta.len()
            )));
        }
        Err(error) => return Ok(Some(format!("the blob file of the store: {error}"))),
    }
    if !sample {
        return Ok(None);
    }

    let io = |error| Error::Io {
        path: source.to_owned(),
        source: error,
    };
    let mut original = File::open(source).map_err(io)?;
    let mut copy = match target.open_blob(oid, tid) {
        Ok(copy) => copy,
        Err(error) => return Ok(Some(format!("the store does not open the blob: {error}"))),
    };
    let (mut want, mut got) = (vec![0; CHUNK], vec![0; CHUNK]);
    loop {
        let n = fill(&mut original, &mut want).map_err(io)?;
        // The reader of the store checks the size and the hash when it
        // reaches the end.
        let m = match fill(&mut copy, &mut got) {
            Ok(m) => m,
            Err(error) => return Ok(Some(format!("the blob of the store: {error}"))),
        };
        if want[..n] != got[..m] {
            return Ok(Some(
                "the blob of the store differs from the blob file".into(),
            ));
        }
        if n == 0 {
            return Ok(None);
        }
    }
}

/// Read from `reader` until `buf` is full or the reader ends. Return the
/// count of bytes read.
fn fill(reader: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match reader.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(read) => n += read,
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use zoe_core::vfs::MemVfs;

    use super::*;
    use crate::filestorage::tests::{Data, FileBuilder, refuses_huge_allocations, sparse_data_fs};

    /// The record of a blob.
    const BLOB: &[u8] = b"\x80\x03cZODB.blob\nBlob\nq\x00.\x80\x03N.";

    const NO_META: [&[u8]; 3] = [b"", b"", b""];

    /// Write a bushy blob directory beside the file `data_fs` with the blob
    /// files `files`, and return its path.
    fn blob_dir(data_fs: &Path, files: &[(u64, u64, &[u8])]) -> PathBuf {
        let root = data_fs.with_file_name("blobs");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join(".layout"), "bushy").unwrap();
        let blobs = BlobDir::open(&root).unwrap();
        for &(oid, tid, bytes) in files {
            write_blob(&blobs, oid, tid, bytes);
        }
        root
    }

    fn write_blob(blobs: &BlobDir, oid: u64, tid: u64, bytes: &[u8]) {
        let path = blobs.path(Oid(oid), Tid(tid));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    /// Open a store on disk beside the file `data_fs`.
    fn store_beside(data_fs: &Path) -> Store {
        Store::open(data_fs.with_file_name("store")).unwrap()
    }

    fn read_blob(store: &Store, oid: u64, tid: u64) -> Vec<u8> {
        let mut bytes = Vec::new();
        store
            .open_blob(Oid(oid), Tid(tid))
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        bytes
    }

    #[test]
    fn a_store_that_does_not_continue_the_file_is_refused() {
        let mut file = FileBuilder::new();
        file.txn(10, b' ', NO_META, &[(1, Data::Bytes(b"a"))]);
        file.txn(30, b' ', NO_META, &[(1, Data::Bytes(b"b"))]);
        let path = file.write("diverged");
        // A last tid between two tids of the file, and one after them.
        for last in [Tid(20), Tid(40)] {
            let store = Store::open_vfs(MemVfs::new()).unwrap();
            let mut txn = store.tpc_begin_restore(last, b"", b"", b"").unwrap();
            txn.restore(Oid(1), Some(b"x")).unwrap();
            txn.vote().unwrap();
            txn.finish().unwrap();

            let error = from_datafs(&path, None, &store).unwrap_err();
            assert!(
                matches!(error, Error::Diverged { last: found } if found == last),
                "{error:?}"
            );
            assert!(error.to_string().contains(&hex(last.0)), "{error}");
            assert_eq!(store.last_transaction(), last);
        }
    }

    #[test]
    fn each_difference_of_a_transaction_is_reported() {
        let mut file = FileBuilder::new();
        file.txn(
            10,
            b' ',
            [b"user", b"", b""],
            &[(1, Data::Bytes(b"a")), (2, Data::Bytes(b"b"))],
        );
        file.txn(20, b' ', NO_META, &[(1, Data::Bytes(b"c"))]);
        let path = file.write("differs");
        // The store holds the first tid of the file, with other content.
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let mut txn = store
            .tpc_begin_restore(Tid(10), b"other", b"", b"")
            .unwrap();
        txn.restore(Oid(1), Some(b"x")).unwrap();
        txn.restore(Oid(3), Some(b"z")).unwrap();
        txn.vote().unwrap();
        txn.finish().unwrap();

        let report = from_datafs(&path, None, &store).unwrap();
        assert_eq!((report.transactions, report.copied), (2, 1));
        let txn = "transaction 0x000000000000000a";
        assert_eq!(
            report.problems,
            [
                format!("{txn}: the user differs"),
                format!("{txn}: the record of oid 0x0000000000000001 differs"),
                format!("{txn}: the store lacks the record of oid 0x0000000000000002"),
                format!(
                    "{txn}: the store has a record of oid 0x0000000000000003, \
                     which the Data.fs does not have"
                ),
            ]
        );
    }

    #[test]
    fn a_blob_record_without_its_file_is_copied_and_reported() {
        let mut file = FileBuilder::new();
        file.txn(
            10,
            b' ',
            NO_META,
            &[(1, Data::Bytes(BLOB)), (2, Data::Bytes(BLOB))],
        );
        let path = file.write("blob-without-file");
        let blobs = blob_dir(&path, &[(1, 10, b"first blob")]);
        let store = store_beside(&path);

        let report = from_datafs(&path, Some(&blobs), &store).unwrap();
        assert_eq!((report.copied, report.records, report.blobs), (1, 2, 1));
        assert_eq!(report.problems.len(), 1, "{report:?}");
        let problem = "oid 0x0000000000000002 at 0x000000000000000a: \
                       the blob directory has no blob file";
        assert!(report.problems[0].starts_with(problem), "{report:?}");
        assert_eq!(read_blob(&store, 1, 10), b"first blob");
        assert_eq!(store.load_serial(Oid(2), Tid(10)).unwrap(), BLOB);
        assert!(store.open_blob(Oid(2), Tid(10)).is_err());
    }

    #[test]
    fn blob_records_without_a_blob_directory_are_reported() {
        let mut file = FileBuilder::new();
        file.txn(
            10,
            b' ',
            NO_META,
            &[(1, Data::Bytes(BLOB)), (2, Data::Bytes(BLOB))],
        );
        let path = file.write("blob-without-dir");
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let report = from_datafs(&path, None, &store).unwrap();
        assert_eq!((report.copied, report.blobs), (1, 0));
        assert_eq!(
            report.problems,
            ["the Data.fs has 2 blob records, and no blob directory was given"]
        );
    }

    #[test]
    fn a_blob_after_the_deletion_of_its_oid_is_restored() {
        let mut file = FileBuilder::new();
        file.txn(10, b' ', NO_META, &[(1, Data::Bytes(BLOB))]);
        // An undo of the creation of oid 1.
        file.txn(20, b' ', NO_META, &[(1, Data::Back(0))]);
        file.txn(30, b' ', NO_META, &[(1, Data::Bytes(BLOB))]);
        let path = file.write("blob-after-deletion");
        let blobs = blob_dir(&path, &[(1, 10, b"before"), (1, 30, b"after")]);
        let store = store_beside(&path);

        let report = from_datafs(&path, Some(&blobs), &store).unwrap();
        assert!(report.problems.is_empty(), "{report:?}");
        assert_eq!((report.copied, report.blobs), (3, 2));
        assert_eq!(read_blob(&store, 1, 10), b"before");
        assert_eq!(read_blob(&store, 1, 30), b"after");
    }

    #[test]
    fn the_sample_compares_the_first_blob_and_one_in_sixteen() {
        let oids: Vec<u64> = (1..=BLOB_SAMPLE_STRIDE + 1).collect();
        let records: Vec<_> = oids.iter().map(|&oid| (oid, Data::Bytes(BLOB))).collect();
        let mut file = FileBuilder::new();
        file.txn(10, b' ', NO_META, &records);
        let path = file.write("blob-sample");
        let files: Vec<_> = oids.iter().map(|&oid| (oid, 10, &b"a"[..])).collect();
        let root = blob_dir(&path, &files);
        let store = store_beside(&path);
        let report = from_datafs(&path, Some(&root), &store).unwrap();
        assert!(report.problems.is_empty(), "{report:?}");

        // The source changes after the copy. Only a sampled blob shows a
        // change that keeps the size: the 17th, not the 2nd.
        let blobs = BlobDir::open(&root).unwrap();
        for oid in [2, 17] {
            write_blob(&blobs, oid, 10, b"b");
        }
        // A change of the size shows for every blob.
        write_blob(&blobs, 3, 10, b"ab");
        let report = from_datafs(&path, Some(&root), &store).unwrap();
        assert_eq!(report.copied, 0);
        assert_eq!(
            report.problems,
            [
                "oid 0x0000000000000003 at 0x000000000000000a: \
                 the sizes differ: 1 bytes in the store, 2 in the blob file",
                "oid 0x0000000000000011 at 0x000000000000000a: \
                 the blob of the store differs from the blob file",
            ]
        );
    }

    #[test]
    fn a_truncated_tail_is_reported_and_a_commit_in_progress_is_not() {
        let mut file = FileBuilder::new();
        file.txn(10, b' ', NO_META, &[(1, Data::Bytes(b"a"))]);
        let pos = file.bytes.len();
        file.txn(20, b' ', NO_META, &[(1, Data::Bytes(b"b"))]);

        let mut truncated = FileBuilder {
            bytes: file.bytes.clone(),
        };
        truncated.bytes.pop();
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let report = from_datafs(&truncated.write("tail-truncated"), None, &store).unwrap();
        assert_eq!((report.transactions, report.copied), (1, 1));
        assert_eq!(report.last_tid, Tid(10));
        assert_eq!(
            report.problems,
            [format!(
                "the Data.fs ends inside the transaction at offset {pos}: \
                 the conversion stops there"
            )]
        );

        file.bytes[pos + 16] = b'c';
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let report = from_datafs(&file.write("tail-in-progress"), None, &store).unwrap();
        assert_eq!((report.transactions, report.copied), (1, 1));
        assert!(report.problems.is_empty(), "{report:?}");
    }

    #[test]
    fn a_commit_status_before_the_last_transaction_is_a_problem() {
        let mut file = FileBuilder::new();
        file.txn(10, b' ', NO_META, &[(1, Data::Bytes(b"a"))]);
        let pos = file.bytes.len();
        file.txn(20, b' ', NO_META, &[(1, Data::Bytes(b"b"))]);
        file.txn(30, b' ', NO_META, &[(1, Data::Bytes(b"c"))]);
        file.bytes[pos + 16] = b'c';
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let report = from_datafs(&file.write("commit-not-last"), None, &store).unwrap();
        assert_eq!((report.transactions, report.copied), (1, 1));
        assert_eq!(
            report.problems,
            [format!(
                "the transaction at offset {pos} has the status c, \
                 but it is not a whole last transaction: the conversion stops there"
            )]
        );
    }

    #[test]
    fn a_tail_of_zeros_is_a_problem_on_every_run() {
        let mut file = FileBuilder::new();
        file.txn(10, b' ', NO_META, &[(1, Data::Bytes(b"a"))]);
        file.txn(20, b' ', NO_META, &[(1, Data::Bytes(b"b"))]);
        let pos = file.bytes.len();
        file.bytes.resize(pos + 4096, 0);
        let path = file.write("tail-of-zeros");
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let problem = format!(
            "the Data.fs holds no valid transaction from offset {pos} to its end, \
             as after a crash: the conversion stops there"
        );
        for copied in [2, 0] {
            let report = from_datafs(&path, None, &store).unwrap();
            assert_eq!((report.transactions, report.copied), (2, copied));
            assert_eq!(report.last_tid, Tid(20));
            assert_eq!(report.problems, std::slice::from_ref(&problem));
        }
    }

    #[test]
    fn the_lines_about_the_whole_source_come_before_the_cap() {
        let records: Vec<_> = (1..=105).map(|oid| (oid, Data::Bytes(b"a"))).collect();
        let mut file = FileBuilder::new();
        file.txn(10, b' ', NO_META, &records);
        file.txn(20, b' ', NO_META, &[(200, Data::Bytes(BLOB))]);
        let pos = file.bytes.len();
        file.bytes.resize(pos + 64, 0);
        let path = file.write("summary-first");
        // The store holds the first transaction with other pickles, so
        // each of its records is a problem.
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let mut txn = store.tpc_begin_restore(Tid(10), b"", b"", b"").unwrap();
        for oid in 1..=105 {
            txn.restore(Oid(oid), Some(b"b")).unwrap();
        }
        txn.vote().unwrap();
        txn.finish().unwrap();

        let report = from_datafs(&path, None, &store).unwrap();
        assert_eq!(report.copied, 1);
        assert_eq!(report.problems.len(), 2 + MAX_PROBLEMS + 1, "{report:?}");
        assert!(
            report.problems[0].starts_with("the Data.fs holds no valid transaction"),
            "{report:?}"
        );
        assert_eq!(
            report.problems[1],
            "the Data.fs has 1 blob records, and no blob directory was given"
        );
        assert_eq!(report.problems.last().unwrap(), "5 more problems");
    }

    #[test]
    fn a_record_longer_than_the_memory_stops_the_conversion() {
        if !refuses_huge_allocations() {
            return;
        }
        let Some(path) = sparse_data_fs("sparse-conversion") else {
            return;
        };
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let error = from_datafs(&path, None, &store).unwrap_err();
        fs::remove_file(&path).unwrap();
        let Error::Io { source, .. } = &error else {
            panic!("not an I/O error: {error:?}");
        };
        assert_eq!(source.kind(), ErrorKind::OutOfMemory, "{error:?}");
        assert_eq!(store.last_transaction(), Tid::ZERO);
    }

    #[test]
    fn a_path_that_is_not_a_regular_file_stops_the_conversion() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let error = from_datafs(Path::new("/dev/null"), None, &store).unwrap_err();
        assert!(
            matches!(&error, Error::Io { source, .. } if source.kind() == ErrorKind::InvalidInput),
            "{error:?}"
        );
    }

    #[test]
    fn the_report_lists_at_most_a_hundred_problems() {
        let records: Vec<_> = (1..=105).map(|oid| (oid, Data::Bytes(BLOB))).collect();
        let mut file = FileBuilder::new();
        file.txn(10, b' ', NO_META, &records);
        let path = file.write("many-problems");
        let blobs = blob_dir(&path, &[]);
        let store = store_beside(&path);

        let report = from_datafs(&path, Some(&blobs), &store).unwrap();
        assert_eq!(report.problems.len(), MAX_PROBLEMS + 1);
        assert_eq!(report.problems.last().unwrap(), "5 more problems");
    }
}
