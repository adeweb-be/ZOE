//! Acceptance test of the counts in the report of `zoe convert` (F14).
//!
//! Before the capped list of problems, the report gives one line for each
//! kind of problem, with its count and no cap. The line `problems` gives
//! the number of problems, not the number of lines of the list.
//!
//! The `Data.fs` comes from a store through `to_datafs`: one transaction
//! with one record, then one transaction with 150 blob records, whose blob
//! files the test removes. The target store holds the first transaction
//! with another pickle, so the conversion finds 150 missing blob files and
//! one record that differs.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use zoe_convert::to_datafs;
use zoe_core::{Oid, Tid};

mod common;

use common::{fresh_dir, record, s, zoe};

/// The class pickle of `ZODB.blob.Blob`, then the state `None`.
const BLOB: &[u8] = b"\x80\x03cZODB.blob\nBlob\nq\x00.\x80\x03N.";

/// The number of blob records whose files are missing.
const MISSING: u64 = 150;

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
/// files, and the target store. Return the paths of the three.
fn prepare(dir: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let source = common::open_store(&dir.join("source"));
    let mut txn = source.tpc_begin(b"admin", b"create", b"").unwrap();
    txn.store(Oid(1), Tid(0), &record(b"a")).unwrap();
    txn.vote().unwrap();
    let first = txn.finish().unwrap();
    let mut txn = source.tpc_begin(b"admin", b"blobs", b"").unwrap();
    for oid in 2..2 + MISSING {
        let mut file = source.new_blob_file().unwrap();
        file.write_all(format!("blob {oid}").as_bytes()).unwrap();
        txn.store_blob(Oid(oid), Tid(0), BLOB, file).unwrap();
    }
    txn.vote().unwrap();
    txn.finish().unwrap();
    let data_fs = dir.join("Data.fs");
    let blobs = dir.join("blobs");
    to_datafs(&source, &data_fs, &blobs).unwrap();
    drop(source);
    assert_eq!(remove_blob_files(&blobs), MISSING);

    let path = dir.join("target");
    let target = common::open_store(&path);
    let mut txn = target
        .tpc_begin_restore(first, b"admin", b"create", b"")
        .unwrap();
    txn.restore(Oid(1), Some(&record(b"x"))).unwrap();
    txn.vote().unwrap();
    txn.finish().unwrap();
    drop(target);
    (data_fs, blobs, path)
}

/// Tell if `line` holds the number `count` as a word of its own.
fn has_count(line: &str, count: u64) -> bool {
    line.split_whitespace()
        .any(|word| word == count.to_string())
}

#[test]
fn the_report_counts_problems_by_kind_before_the_list() {
    let dir = fresh_dir("kinds");
    let (data_fs, blobs, path) = prepare(&dir);
    let args = [
        "convert",
        "--from-data-fs",
        s(&data_fs),
        "--data-fs-blob-dir",
        s(&blobs),
        "--path",
        s(&path),
    ];
    let output = zoe(&args, &[]);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    // The exit status does not change: a conversion with problems fails.
    assert_eq!(output.status.code(), Some(1), "{stdout}\n{stderr}");

    let lines: Vec<&str> = stdout.lines().collect();
    // The count line gives the number of problems.
    assert!(
        lines.iter().any(|line| line.trim() == "problems      151"),
        "{stdout}"
    );
    // The capped list starts at the line of the record that differs.
    let list = lines
        .iter()
        .position(|line| line.contains("the record of oid 0x0000000000000001 differs"))
        .unwrap_or_else(|| panic!("no line of the list in\n{stdout}"));
    let before = &lines[..list];
    assert!(
        before
            .iter()
            .any(|line| has_count(line, MISSING) && line.contains("missing blob file")),
        "no count of the missing blob files before the list in\n{stdout}"
    );
    assert!(
        before
            .iter()
            .any(|line| has_count(line, 1) && line.contains("differ")),
        "no count of the records that differ before the list in\n{stdout}"
    );
    // The capped list and its last line do not change.
    assert_eq!(
        lines[list..]
            .iter()
            .filter(|line| line.contains("the blob directory has no blob file"))
            .count(),
        99,
        "{stdout}"
    );
    assert!(
        lines.iter().any(|line| line.trim() == "51 more problems"),
        "{stdout}"
    );
    let _ = fs::remove_dir_all(&dir);
}
