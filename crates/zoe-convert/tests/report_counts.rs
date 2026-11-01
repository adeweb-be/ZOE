//! Acceptance test of the counts of the report of a conversion (F14).
//!
//! The report counts the problems of each kind, with no cap, beside the
//! capped list of lines. A copy of a site often comes without its blob
//! files: the counts tell such a copy from a store whose pickles differ.
//!
//! The test writes a `Data.fs` from a store with `to_datafs`: one
//! transaction with one record, then one transaction with 150 blob
//! records. It removes every blob file of the `Data.fs`. The target store
//! holds the first transaction with another pickle, so the conversion
//! copies only the second one. The verification then finds 150 missing
//! blob files and one record that differs: 151 problems.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use zoe_convert::{ProblemKind, from_datafs, to_datafs};
use zoe_core::{Oid, Store, Tid};

mod support;
use support::fresh_dir;

/// The class pickle of `ZODB.blob.Blob`, then the state `None`.
const BLOB: &[u8] = b"\x80\x03cZODB.blob\nBlob\nq\x00.\x80\x03N.";

/// The number of blob records whose files are missing.
const MISSING: u64 = 150;

/// A `PersistentMapping` whose state holds `value` under the key `v`.
fn record(value: &[u8]) -> Vec<u8> {
    assert!(value.len() < 256);
    let mut pickle = b"\x80\x03cpersistent.mapping\nPersistentMapping\nq\x00.".to_vec();
    pickle.extend_from_slice(b"\x80\x03}q\x00X\x01\x00\x00\x00vq\x01C");
    pickle.push(value.len() as u8);
    pickle.extend_from_slice(value);
    pickle.extend_from_slice(b"q\x02s.");
    pickle
}

/// Remove each blob file under `dir`. Return how many it removed.
fn remove_blob_files(dir: &Path) -> u64 {
    let mut removed = 0;
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            removed += remove_blob_files(&path);
        } else if path.extension().is_some_and(|ext| ext == "blob") {
            fs::remove_file(&path).unwrap();
            removed += 1;
        }
    }
    removed
}

/// Write the `Data.fs` and its blob directory in `dir`, without the blob
/// files. Return the paths of both and the tid of the first transaction.
fn source(dir: &Path) -> (PathBuf, PathBuf, Tid) {
    let store = Store::open(dir.join("source")).unwrap();
    let mut txn = store.tpc_begin(b"admin", b"create", b"").unwrap();
    txn.store(Oid(1), Tid(0), &record(b"a")).unwrap();
    txn.vote().unwrap();
    let first = txn.finish().unwrap();
    let mut txn = store.tpc_begin(b"admin", b"blobs", b"").unwrap();
    for oid in 2..2 + MISSING {
        let mut file = store.new_blob_file().unwrap();
        file.write_all(format!("blob {oid}").as_bytes()).unwrap();
        txn.store_blob(Oid(oid), Tid(0), BLOB, file).unwrap();
    }
    txn.vote().unwrap();
    txn.finish().unwrap();

    let data_fs = dir.join("Data.fs");
    let blobs = dir.join("blobs");
    let report = to_datafs(&store, &data_fs, &blobs).unwrap();
    assert!(report.problems.is_empty(), "{report:?}");
    assert_eq!(remove_blob_files(&blobs), MISSING);
    (data_fs, blobs, first)
}

/// Open the target store. It holds the first tid of the file with
/// another pickle.
fn target(dir: &Path, first: Tid) -> Store {
    let store = Store::open(dir.join("target")).unwrap();
    let mut txn = store
        .tpc_begin_restore(first, b"admin", b"create", b"")
        .unwrap();
    txn.restore(Oid(1), Some(&record(b"x"))).unwrap();
    txn.vote().unwrap();
    txn.finish().unwrap();
    store
}

#[test]
fn the_report_counts_each_kind_of_problem_with_no_cap() {
    let dir = fresh_dir("kinds");
    let (data_fs, blobs, first) = source(&dir);
    let store = target(&dir, first);

    let report = from_datafs(&data_fs, Some(&blobs), &store).unwrap();
    assert_eq!((report.transactions, report.copied), (2, 1), "{report:?}");

    // One count for each kind, with no cap.
    assert_eq!(report.count(ProblemKind::MissingBlobFile), MISSING);
    assert_eq!(report.count(ProblemKind::RecordDiffers), 1);
    // The total counts problems, not the lines of the list.
    assert_eq!(report.problem_count(), MISSING + 1);

    // The capped list does not change: the record of the first
    // transaction, then 99 missing blob files, then a line that counts
    // the others.
    assert_eq!(report.problems.len(), 101, "{report:?}");
    assert!(
        report.problems[0].contains("the record of oid 0x0000000000000001 differs"),
        "{report:?}"
    );
    assert!(
        report.problems[1..100]
            .iter()
            .all(|line| line.contains("the blob directory has no blob file")),
        "{report:?}"
    );
    assert_eq!(report.problems[100], "51 more problems");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_conversion_that_verifies_counts_no_problem() {
    let dir = fresh_dir("none");
    let store = Store::open(dir.join("source")).unwrap();
    let mut txn = store.tpc_begin(b"admin", b"create", b"").unwrap();
    txn.store(Oid(1), Tid(0), &record(b"a")).unwrap();
    txn.vote().unwrap();
    txn.finish().unwrap();
    let data_fs = dir.join("Data.fs");
    let blobs = dir.join("blobs");
    to_datafs(&store, &data_fs, &blobs).unwrap();

    let target = Store::open(dir.join("target")).unwrap();
    let report = from_datafs(&data_fs, Some(&blobs), &target).unwrap();
    assert!(report.problems.is_empty(), "{report:?}");
    assert_eq!(report.problem_count(), 0);
    assert_eq!(report.count(ProblemKind::MissingBlobFile), 0);
    assert_eq!(report.count(ProblemKind::RecordDiffers), 0);
    let _ = fs::remove_dir_all(&dir);
}
