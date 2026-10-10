//! Acceptance test of the staging buffer: a transaction keeps its pickles
//! in memory up to `staging_memory` bytes and spills the rest to a file
//! under `staging/` in the store directory.

use std::path::{Path, PathBuf};

use zoe_core::{Oid, Options, Store, Tid};

const LIMIT: usize = 1 << 20;
const OBJECT: usize = 64 * 1024;
const OBJECTS: usize = 64;

fn fresh_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("zoe-staging-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn open(dir: &Path) -> Store {
    let options = Options {
        staging_memory: LIMIT,
        ..Options::default()
    };
    Store::open_with(dir, options).unwrap()
}

/// A pickle of `OBJECT` bytes that is different for each oid.
fn pickle(oid: Oid) -> Vec<u8> {
    let mut data = vec![0u8; OBJECT];
    for (i, byte) in data.iter_mut().enumerate() {
        *byte = (oid.0 as usize).wrapping_mul(31).wrapping_add(i) as u8;
    }
    data
}

fn spill_files(dir: &Path) -> usize {
    match std::fs::read_dir(dir.join("staging")) {
        Ok(entries) => entries.count(),
        Err(_) => 0,
    }
}

#[test]
fn a_transaction_larger_than_the_limit_spills_and_commits() {
    let dir = fresh_dir("commit");
    let store = open(&dir);
    let oids: Vec<Oid> = (0..OBJECTS).map(|_| store.new_oid().unwrap()).collect();

    let mut txn = store.tpc_begin(b"", b"large", b"").unwrap();
    for &oid in &oids {
        txn.store(oid, Tid::ZERO, &pickle(oid)).unwrap();
        assert!(txn.staged_in_memory() <= LIMIT, "{}", txn.staged_in_memory());
    }
    assert!(spill_files(&dir) > 0, "nothing was spilled");
    txn.vote().unwrap();
    let tid = txn.finish().unwrap();

    assert_eq!(spill_files(&dir), 0, "the spill file outlived the commit");
    for &oid in &oids {
        assert_eq!(store.load(oid).unwrap(), (pickle(oid), tid));
    }
}

#[test]
fn a_small_transaction_does_not_spill() {
    let dir = fresh_dir("small");
    let store = open(&dir);
    let oid = store.new_oid().unwrap();
    let mut txn = store.tpc_begin(b"", b"small", b"").unwrap();
    txn.store(oid, Tid::ZERO, &pickle(oid)).unwrap();
    assert_eq!(spill_files(&dir), 0);
    txn.vote().unwrap();
    txn.finish().unwrap();
}

#[test]
fn an_aborted_spill_leaves_no_file() {
    let dir = fresh_dir("abort");
    let store = open(&dir);
    let mut txn = store.tpc_begin(b"", b"aborted", b"").unwrap();
    for _ in 0..OBJECTS {
        let oid = store.new_oid().unwrap();
        txn.store(oid, Tid::ZERO, &pickle(oid)).unwrap();
    }
    assert!(spill_files(&dir) > 0);
    txn.abort();
    assert_eq!(spill_files(&dir), 0);
}

#[test]
fn a_spill_file_left_by_a_crash_is_removed_at_open() {
    let dir = fresh_dir("crash");
    let store = open(&dir);
    let mut txn = store.tpc_begin(b"", b"crashed", b"").unwrap();
    for _ in 0..OBJECTS {
        let oid = store.new_oid().unwrap();
        txn.store(oid, Tid::ZERO, &pickle(oid)).unwrap();
    }
    assert!(spill_files(&dir) > 0);
    // A crash runs no destructor.
    std::mem::forget(txn);
    std::mem::forget(store);

    // The lock of the first open lives in this process, so a second open
    // must come from a fresh process. A copy of the directory stands in
    // for the store after the crash.
    let copy = fresh_dir("crash-copy");
    copy_dir(&dir, &copy);
    std::fs::remove_file(copy.join("lock")).unwrap();
    let reopened = open(&copy);
    assert_eq!(spill_files(&copy), 0);
    drop(reopened);
}

fn copy_dir(from: &Path, to: &Path) {
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            std::fs::create_dir_all(&target).unwrap();
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}
