//! Acceptance test of the read-only opens of a store with a write-ahead
//! log (ADR 0056, ADR 0028; N7).
//!
//! A clean close folds the log and marks each segment spare, so a strict
//! read-only open reads the store and changes no byte. A writer that a
//! kill stopped leaves commits in the log only: the strict read-only open
//! refuses such a store with `NotClosedCleanly`, and the open of the tools
//! folds the log into `zoe.redb` and writes no other file.

mod support;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use support::fresh_dir;
use xxhash_rust::xxh3::xxh3_64;
use zoe_core::testing::{commit, new_oid};
use zoe_core::{Options, Store, Tid, WriteLog, ZoeError};

/// The database file of a store.
const DATABASE: &str = "zoe.redb";

/// Settings under which no fold starts by itself.
fn held() -> Options {
    Options {
        write_log: WriteLog {
            fold_bytes: 1 << 40,
            backlog_bytes: 1 << 41,
            ..WriteLog::default()
        },
        ..Options::default()
    }
}

/// Return every entry under `dir`, by path: the bytes of a file, `None`
/// for a directory.
fn tree(dir: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    let mut entries = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path.clone());
                entries.insert(path, None);
            } else {
                entries.insert(path.clone(), Some(fs::read(&path).unwrap()));
            }
        }
    }
    entries
}

/// Copy every file and directory under `from` to `to`.
fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let path = entry.unwrap().path();
        let target = to.join(path.file_name().unwrap());
        if path.is_dir() {
            copy_tree(&path, &target);
        } else {
            fs::copy(&path, &target).unwrap();
        }
    }
}

/// Return the files of `wal/` in the store `dir`.
fn segments(dir: &Path) -> Vec<PathBuf> {
    match fs::read_dir(dir.join("wal")) {
        Ok(entries) => entries.map(|entry| entry.unwrap().path()).collect(),
        Err(_) => Vec::new(),
    }
}

/// Return the state of the segment `path`: 1 in use, 2 spare, or `None`
/// for a header that does not pass its checksum.
fn segment_state(path: &Path) -> Option<u32> {
    let bytes = fs::read(path).unwrap();
    let header = bytes.get(..32)?;
    let checksum = u64::from_le_bytes(header[24..32].try_into().unwrap());
    if &header[..8] != b"ZOE-WAL\0" || xxh3_64(&header[..24]) != checksum {
        return None;
    }
    Some(u32::from_le_bytes(header[12..16].try_into().unwrap()))
}

/// Check that only `zoe.redb` differs between `before` and `after`.
fn only_the_database_changed(
    dir: &Path,
    before: &BTreeMap<PathBuf, Option<Vec<u8>>>,
    after: &BTreeMap<PathBuf, Option<Vec<u8>>>,
) {
    assert_eq!(
        before.keys().collect::<Vec<_>>(),
        after.keys().collect::<Vec<_>>(),
        "the open added or removed an entry"
    );
    let database = dir.join(DATABASE);
    for (path, bytes) in before {
        if *path != database {
            assert_eq!(bytes, &after[path], "{} changed", path.display());
        }
    }
}

#[test]
fn a_tool_folds_the_log_of_a_killed_writer_and_a_strict_open_refuses_it() {
    let root = fresh_dir("killed-writer");
    let (live, image) = (root.join("live"), root.join("image"));
    let store = Store::open_with(&live, held()).unwrap();
    let oid = new_oid(&store);
    let first = commit(&store, oid, Tid::ZERO, b"first");
    store.fold().unwrap();
    // The second commit is in the log only. A copy of the directory now is
    // what a kill of the writer leaves.
    let second = commit(&store, oid, first, b"second");
    copy_tree(&live, &image);
    drop(store);
    assert!(!segments(&image).is_empty(), "the image holds no log");

    assert!(matches!(
        Store::open_read_only(&image),
        Err(ZoeError::NotClosedCleanly)
    ));
    let before = tree(&image);
    let store = Store::open_read_only_repairing(&image, Options::default()).unwrap();
    assert!(store.repaired());
    assert_eq!(store.load(oid).unwrap(), (b"second".to_vec(), second));
    assert_eq!(store.load_serial(oid, first).unwrap(), b"first".to_vec());
    drop(store);
    let after = tree(&image);
    only_the_database_changed(&image, &before, &after);

    // The repair folded the log, so a strict open reads the store and
    // changes no byte.
    let store = Store::open_read_only(&image).unwrap();
    assert_eq!(store.load(oid).unwrap(), (b"second".to_vec(), second));
    drop(store);
    assert_eq!(tree(&image), after);
}

#[test]
fn after_a_clean_close_a_strict_read_only_open_reads_every_commit_and_changes_no_byte() {
    let dir = fresh_dir("clean-close");
    let store = Store::open_with(&dir, held()).unwrap();
    let oid = new_oid(&store);
    let first = commit(&store, oid, Tid::ZERO, b"first");
    let second = commit(&store, oid, first, b"second");
    assert!(store.backlog_bytes() > 0);
    drop(store);

    let files = segments(&dir);
    assert!(!files.is_empty(), "the store has no log");
    for file in &files {
        assert_eq!(
            segment_state(file),
            Some(2),
            "{} is not spare",
            file.display()
        );
    }
    let before = tree(&dir);
    let store = Store::open_read_only(&dir).unwrap();
    assert_eq!(store.load(oid).unwrap(), (b"second".to_vec(), second));
    drop(store);
    assert_eq!(tree(&dir), before);
}
