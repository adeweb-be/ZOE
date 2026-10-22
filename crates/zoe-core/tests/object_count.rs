//! Acceptance test of the object count of a store.
//!
//! The store keeps an exact count of its objects in the `meta` key
//! `object_count`: the oids that have a record, a deletion included,
//! until a pack removes every record of the oid. This is the `len()` of
//! FileStorage, the length of its index. Each commit and each pack batch
//! updates the count in the backend write transaction that changes the
//! records, so every snapshot and every crash image holds the count of
//! its records.
//!
//! The format is `ZOE format 2`. An open refuses a store of `ZOE format
//! 1`, which has no count, with the error of any other format name. The
//! refused open writes nothing.
//!
//! The test makes a store of format 1 with the test hooks
//! `zoe_core::testing::{remove_meta, write_meta}`. It checks the count
//! against the oids of `Store::record_keys` after each step, and on every
//! crash image of commits and of a pack. It checks the refusal of a store
//! of format 1 on every crash image of the refused open.

mod support;

use support::fresh_dir;
use zoe_core::testing;
use zoe_core::testing::commit_txn;
use zoe_core::vfs::MemVfs;
use zoe_core::{Oid, Store, Tid, Txn, ZoeError};

/// A `PersistentMapping` record with no reference.
const PICKLE: &[u8] =
    b"\x80\x03cpersistent.mapping\nPersistentMapping\nq\x00.\x80\x03}q\x01X\x04\x00\x00\x00datah\x01s.";

const ROOT: Oid = Oid(0);

/// Return the number of oids that have a record in `store`, deletions
/// included, from the keys of `records`.
fn oids_with_records(store: &Store) -> u64 {
    let mut count = 0;
    let mut from = Oid(0);
    loop {
        // Each list holds all the keys of each oid in it.
        let keys = store.record_keys(from, 1_000).unwrap();
        let Some(&(last, _)) = keys.last() else {
            return count;
        };
        let mut previous = None;
        for &(oid, _) in &keys {
            if previous != Some(oid) {
                count += 1;
                previous = Some(oid);
            }
        }
        if last.0 == u64::MAX {
            return count;
        }
        from = Oid(last.0 + 1);
    }
}

fn assert_exact(store: &Store, context: &str) {
    assert_eq!(
        store.object_count(),
        Some(oids_with_records(store)),
        "{context}"
    );
}

/// Begin a transaction that writes `writes`, each a store of a pickle or
/// a deletion (`None`), with the serial that the store holds for the oid.
fn writing<'s>(store: &'s Store, writes: &[(Oid, Option<&[u8]>)]) -> Txn<&'s Store> {
    let mut txn = store.tpc_begin(b"someone", b"count test", b"").unwrap();
    for &(oid, data) in writes {
        let serial = match store.history(oid, 1) {
            Ok(entries) => entries.first().map_or(Tid::ZERO, |entry| entry.tid),
            Err(_) => Tid::ZERO,
        };
        match data {
            Some(data) => txn.store(oid, serial, data).unwrap(),
            None => txn.delete(oid, serial).unwrap(),
        }
    }
    txn
}

/// Commit the root and `n` objects that nothing refers to. Return their
/// oids.
fn root_and_orphans(store: &Store, n: usize) -> Vec<Oid> {
    let oids: Vec<Oid> = (0..n).map(|_| store.new_oid().unwrap()).collect();
    let mut writes = vec![(ROOT, Some(PICKLE))];
    writes.extend(oids.iter().map(|&oid| (oid, Some(PICKLE))));
    commit_txn(writing(store, &writes));
    oids
}

/// Make `store` a store of format 1: no `object_count`, and the format
/// name `ZOE format 1`. A close writes no count, so the change stays.
fn downgrade(store: &Store) {
    testing::remove_meta(store, "object_count").unwrap();
    testing::write_meta(store, "format", b"ZOE format 1").unwrap();
}

/// Check that `error` is the refusal of the format name `name`.
fn assert_refused(error: Option<ZoeError>, name: &str, context: &str) {
    assert!(
        matches!(&error, Some(ZoeError::Format(text)) if text == name),
        "{context}: {error:?}"
    );
}

#[test]
fn a_new_store_counts_zero_objects() {
    let store = Store::open_vfs(MemVfs::new()).unwrap();
    assert_eq!(store.object_count(), Some(0));
}

#[test]
fn a_commit_counts_its_new_oids_once() {
    let store = Store::open_vfs(MemVfs::new()).unwrap();
    let oids = root_and_orphans(&store, 3);
    assert_eq!(store.object_count(), Some(4));
    // A new revision is no new object.
    commit_txn(writing(
        &store,
        &[(oids[0], Some(PICKLE)), (ROOT, Some(PICKLE))],
    ));
    assert_eq!(store.object_count(), Some(4));
    // A transaction that stores an oid twice counts it once.
    let new = store.new_oid().unwrap();
    commit_txn(writing(&store, &[(new, Some(PICKLE)), (new, Some(PICKLE))]));
    assert_eq!(store.object_count(), Some(5));
    assert_exact(&store, "after the commits");
}

#[test]
fn a_deletion_counts_until_a_pack_removes_the_object() {
    let store = Store::open_vfs(MemVfs::new()).unwrap();
    let oids = root_and_orphans(&store, 2);
    commit_txn(writing(&store, &[(oids[0], None)]));
    // The deletion is a record: the object still counts.
    assert_eq!(store.object_count(), Some(3));
    assert_exact(&store, "after the deletion");
    // Nothing refers to the two objects: the pack removes both.
    store.pack(store.last_transaction()).unwrap();
    assert_eq!(store.object_count(), Some(1));
    assert_exact(&store, "after the pack");
}

#[test]
fn a_pack_that_removes_old_revisions_only_keeps_the_count() {
    let store = Store::open_vfs(MemVfs::new()).unwrap();
    commit_txn(writing(&store, &[(ROOT, Some(PICKLE))]));
    for _ in 0..5 {
        commit_txn(writing(&store, &[(ROOT, Some(PICKLE))]));
    }
    store.pack(store.last_transaction()).unwrap();
    assert_eq!(store.object_count(), Some(1));
    assert_exact(&store, "after the pack");
}

#[test]
fn the_count_survives_a_reopen() {
    let vfs = MemVfs::new();
    let store = Store::open_vfs(vfs.clone()).unwrap();
    root_and_orphans(&store, 7);
    drop(store);
    let store = Store::open_vfs(vfs).unwrap();
    assert_eq!(store.object_count(), Some(8));
}

#[test]
fn a_store_of_format_1_does_not_open() {
    let dir = fresh_dir("format-1");
    let store = Store::open(&dir).unwrap();
    let oids = root_and_orphans(&store, 4);
    commit_txn(writing(&store, &[(oids[1], None)]));
    downgrade(&store);
    drop(store);

    // A read-only open refuses the store and writes nothing.
    let database = dir.join("zoe.redb");
    let before = std::fs::read(&database).unwrap();
    let refused = Store::open_read_only(&dir).err();
    assert_refused(refused, "ZOE format 1", "the read-only open");
    assert_eq!(std::fs::read(&database).unwrap(), before);

    // A writer open refuses it too and counts nothing, so the next open
    // refuses it again.
    assert_refused(Store::open(&dir).err(), "ZOE format 1", "the writer open");
    let refused = Store::open_read_only(&dir).err();
    assert_refused(refused, "ZOE format 1", "the next open");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn every_crash_image_of_commits_holds_the_count_of_its_records() {
    let vfs = MemVfs::new();
    let store = Store::open_vfs(vfs.clone()).unwrap();
    root_and_orphans(&store, 2);
    drop(store);
    let vfs = vfs.durable_copy();
    let store = Store::open_vfs(vfs.clone()).unwrap();
    for n in 1..=4 {
        let mut writes: Vec<(Oid, Option<&[u8]>)> = (0..n)
            .map(|_| (store.new_oid().unwrap(), Some(PICKLE)))
            .collect();
        writes.push((ROOT, Some(PICKLE)));
        commit_txn(writing(&store, &writes));
    }
    drop(store);

    for image in vfs.crash_images() {
        let reopened =
            Store::open_vfs(image.vfs()).unwrap_or_else(|e| panic!("{image:?} does not open: {e}"));
        assert_exact(&reopened, &format!("{image:?}"));
    }
}

#[test]
fn every_crash_image_of_a_pack_holds_the_count_of_its_records() {
    let vfs = MemVfs::new();
    let store = Store::open_vfs(vfs.clone()).unwrap();
    let oids = root_and_orphans(&store, 30);
    commit_txn(writing(&store, &[(oids[0], None), (oids[1], None)]));
    drop(store);
    let vfs = vfs.durable_copy();
    let store = Store::open_vfs(vfs.clone()).unwrap();
    store
        .pack_in_batches(store.last_transaction(), 4, || {})
        .unwrap();
    assert_eq!(store.object_count(), Some(1));
    drop(store);

    for image in vfs.crash_images() {
        let reopened =
            Store::open_vfs(image.vfs()).unwrap_or_else(|e| panic!("{image:?} does not open: {e}"));
        assert_exact(&reopened, &format!("{image:?}"));
    }
}

#[test]
fn every_crash_image_of_a_refused_open_of_format_1_is_refused() {
    let vfs = MemVfs::new();
    let store = Store::open_vfs(vfs.clone()).unwrap();
    let oids = root_and_orphans(&store, 6);
    commit_txn(writing(&store, &[(oids[2], None)]));
    downgrade(&store);
    drop(store);
    let vfs = vfs.durable_copy();
    let refused = Store::open_vfs(vfs.clone()).err();
    assert_refused(refused, "ZOE format 1", "the writer open");

    for image in vfs.crash_images() {
        // The refused open changes no row, so each image stays format 1.
        let refused = Store::open_vfs(image.vfs()).err();
        assert_refused(refused, "ZOE format 1", &format!("{image:?}"));
    }
}
