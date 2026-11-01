//! The conversion of a `Data.fs` to a ZOE store.
//!
//! The conversion makes two passes over the file. The copy pass skips the
//! transactions up to the last tid of the store and restores each later
//! transaction under its own tid, with its blobs. The verification pass
//! reads the file again, up to where the copy pass stopped, and compares
//! it with every transaction of the store.
//!
//! The copy pass restores each transaction without a sync of the database
//! file, and makes the store durable once per batch of transactions and
//! once at its end, before the verification pass. A crash loses only the
//! restores of the open batch, and the next conversion resumes after them.
//!
//! The copy pass holds one record at a time. The store holds the records of
//! a restore transaction in memory until its finish. The verification pass
//! holds one transaction of the file and one of the store. The transaction
//! iterator of the store holds one transaction and a slice of tids at a
//! time, whatever the size of the store.

use std::fs::{self, File};
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};

use zoe_core::{Oid, Store, Tid, TxnInfo, ZoeError};

use crate::blobs::{BlobDir, is_blob_record};
use crate::compare::{BLOB_SAMPLE_STRIDE, TxnJoin, TxnPair, check_blob, compare_txn, stop_problem};
use crate::filestorage::{DataRecord, End, Reader, TxnHeader};
use crate::report::Problems;
use crate::{Error, ProblemKind, Report};

/// The copy pass makes the store durable after at most this many restore
/// transactions.
const BATCH_TRANSACTIONS: u64 = 1_000;

/// The copy pass makes the store durable after at most this many bytes of
/// pickles and blobs, when that comes before `BATCH_TRANSACTIONS`.
const BATCH_BYTES: u64 = 64 << 20;

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
/// A restore commits without a sync of the database file. The store
/// becomes durable after each batch of at most 1,000 transactions or
/// 64 MiB of pickles and blobs, whichever comes first, and once more
/// before the verification. A crash loses only the restores of the open
/// batch, and a later call resumes after them.
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
/// `target`, and make them durable. Return the count of copied
/// transactions.
///
/// After an error, the call makes the restores before it durable too, and
/// returns the first error.
fn copy(reader: &mut Reader, blobs: Option<&BlobDir>, target: &Store) -> Result<u64, Error> {
    let mut batch = Batch::default();
    let copied = copy_batches(reader, blobs, target, &mut batch);
    let synced = batch.sync(target);
    let copied = copied?;
    synced?;
    Ok(copied)
}

/// The restores of the copy pass since the last sync of the store.
#[derive(Default)]
struct Batch {
    transactions: u64,
    bytes: u64,
}

impl Batch {
    /// Count one restore of `bytes` bytes of pickles and blobs. Sync the
    /// store when the batch is full.
    fn add(&mut self, bytes: u64, target: &Store) -> Result<(), Error> {
        self.transactions += 1;
        self.bytes = self.bytes.saturating_add(bytes);
        if self.transactions >= BATCH_TRANSACTIONS || self.bytes >= BATCH_BYTES {
            self.sync(target)?;
        }
        Ok(())
    }

    /// Make the restores of the batch durable, and start a new batch. An
    /// empty batch does nothing.
    fn sync(&mut self, target: &Store) -> Result<(), Error> {
        if self.transactions > 0 {
            *self = Self::default();
            target.sync()?;
        }
        Ok(())
    }
}

/// Copy the transactions as `copy` does, and sync the store after each
/// full batch. The restores of the last batch stay in `batch`.
fn copy_batches(
    reader: &mut Reader,
    blobs: Option<&BlobDir>,
    target: &Store,
    batch: &mut Batch,
) -> Result<u64, Error> {
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
        let bytes = copy_txn(reader, &txn, blobs, target)?;
        copied += 1;
        batch.add(bytes, target)?;
    }
    if !continues {
        return Err(Error::Diverged { last });
    }
    Ok(copied)
}

/// Restore the transaction `header` of `reader` in `target`, without a
/// sync of the database file. Return the count of bytes of its pickles and
/// blobs.
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
) -> Result<u64, Error> {
    let mut txn = target.tpc_begin_restore_deferred(
        header.tid,
        &header.user,
        &header.description,
        &header.extension,
    )?;
    let mut bytes = 0u64;
    for record in reader.records(header) {
        let DataRecord { oid, data } = record?;
        bytes = bytes.saturating_add(data.as_ref().map_or(0, |pickle| pickle.len() as u64));
        let source = match (&data, blobs) {
            (Some(pickle), Some(blobs)) if is_blob_record(pickle) => {
                open_source(&blobs.path(oid, header.tid))?
            }
            _ => None,
        };
        match (data, source) {
            (Some(pickle), Some((path, mut source))) => {
                let mut file = target.new_blob_file()?;
                let size = io::copy(&mut source, &mut file)
                    .map_err(|source| Error::Io { path, source })?;
                bytes = bytes.saturating_add(size);
                let serial = current_serial(target, oid)?;
                txn.store_blob(oid, serial, &pickle, file)?;
            }
            (data, _) => txn.restore(oid, data.as_deref())?,
        }
    }
    txn.vote()?;
    txn.finish()?;
    Ok(bytes)
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

/// Compare the transactions of `reader` with every transaction of
/// `target`, and return the report. `stop` tells where the copy pass
/// stopped.
fn verify(
    mut reader: Reader,
    stop: End,
    blobs: Option<&BlobDir>,
    target: &Store,
) -> Result<Report, Error> {
    let mut report = Report::empty();
    let mut problems = Problems::default();
    if let Some(problem) = stop_problem(stop) {
        problems.add_summary(ProblemKind::Stopped, problem);
    }
    // The blob records of the file when no blob directory is given.
    let mut without_dir = 0u64;
    let mut join = TxnJoin::new(&mut reader, target.iterator(None, None)?);
    while let Some(pair) = join.next()? {
        let ((header, records), in_store) = match pair {
            TxnPair::Both(file, txn) => {
                compare_txn(&file.0, &file.1, &txn, &mut problems);
                (file, true)
            }
            TxnPair::FileOnly(file) => {
                problems.add(
                    ProblemKind::MissingTransaction,
                    format!("the store lacks transaction {}", file.0.tid.hex()),
                );
                (file, false)
            }
            // A transaction of the store that the file does not have, or
            // an error of the store.
            TxnPair::StoreOnly(txn) => {
                let (kind, problem) = not_in_file(txn);
                problems.add(kind, problem);
                continue;
            }
        };
        report.transactions += 1;
        report.records += records.len() as u64;
        report.last_tid = header.tid;

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
            let at = format!("oid {} at {}", record.oid.hex(), header.tid.hex());
            let source = blobs.path(record.oid, header.tid);
            let size = match fs::metadata(&source) {
                Ok(meta) => meta.len(),
                Err(error) if error.kind() == ErrorKind::NotFound => {
                    problems.add(
                        ProblemKind::MissingBlobFile,
                        format!(
                            "{at}: the blob directory has no blob file {}",
                            source.display()
                        ),
                    );
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
                && let Some((kind, problem)) =
                    check_blob(target, record.oid, header.tid, &source, size, sample)?
            {
                problems.add(kind, format!("{at}: {problem}"));
            }
        }
    }

    if without_dir > 0 {
        problems.add_summary(
            ProblemKind::NoBlobDirectory,
            format!("the Data.fs has {without_dir} blob records, and no blob directory was given"),
        );
    }
    // The verification reads only up to where the copy stopped. So it
    // stops early only when the file changed between the two passes.
    if let Some(problem) = reader.end().and_then(stop_problem) {
        problems.add_summary(ProblemKind::Stopped, problem);
    }
    problems.finish_into(&mut report);
    Ok(report)
}

/// Describe a transaction of the store that the file does not have, or an
/// error of the transaction iterator. Return the kind and the text.
fn not_in_file(txn: Result<TxnInfo, ZoeError>) -> (ProblemKind, String) {
    match txn {
        Ok(txn) => (
            ProblemKind::MissingTransaction,
            format!(
                "the store has transaction {}, which the Data.fs does not have",
                txn.tid.hex()
            ),
        ),
        Err(error) => (
            ProblemKind::UnreadableStore,
            format!("the store does not read: {error}"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use zoe_core::vfs::MemVfs;

    use super::*;
    use crate::filestorage::tests::{Data, FileBuilder, refuses_huge_allocations, sparse_data_fs};
    use crate::report::MAX_PROBLEMS;

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
            assert!(error.to_string().contains(&last.hex()), "{error}");
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
        let counts: Vec<_> = report.counts().collect();
        assert_eq!(
            counts,
            [
                (ProblemKind::MetadataDiffers, 1),
                (ProblemKind::MissingRecord, 2),
                (ProblemKind::RecordDiffers, 1),
            ]
        );
        assert_eq!(report.problem_count(), 4);
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
        // The line is one problem, whatever the number of blob records.
        assert_eq!(report.count(ProblemKind::NoBlobDirectory), 1);
        assert_eq!(report.problem_count(), 1);
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
    fn an_error_of_the_copy_pass_keeps_the_restores_before_it() {
        let mut file = FileBuilder::new();
        file.txn(10, b' ', NO_META, &[(1, Data::Bytes(b"a"))]);
        file.txn(20, b' ', NO_META, &[(1, Data::Bytes(b"b"))]);
        // A tid that does not increase stops the copy pass with `Corrupt`.
        file.txn(20, b' ', NO_META, &[(1, Data::Bytes(b"c"))]);
        let vfs = MemVfs::new();
        let store = Store::open_vfs(vfs.clone()).unwrap();
        let error = from_datafs(&file.write("copy-error"), None, &store).unwrap_err();
        assert!(matches!(error, Error::Corrupt { .. }), "{error:?}");

        // The copy pass synced the store before it returned the error, so
        // a crash keeps both restores.
        let image = vfs.crash_now(|_| 0);
        let reopened = Store::open_vfs(image).unwrap();
        assert_eq!(reopened.last_transaction(), Tid(20));
        assert_eq!(reopened.load(Oid(1)).unwrap(), (b"b".to_vec(), Tid(20)));
        drop(store);
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
        // The counts have no cap, and they add up to the number of
        // problems.
        let counts: Vec<_> = report.counts().collect();
        assert_eq!(
            counts,
            [
                (ProblemKind::Stopped, 1),
                (ProblemKind::NoBlobDirectory, 1),
                (ProblemKind::RecordDiffers, 105),
            ]
        );
        assert_eq!(report.problem_count(), 107);
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
        assert_eq!(report.count(ProblemKind::MissingBlobFile), 105);
        assert_eq!(report.problem_count(), 105);
    }
}
