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
//! The format becomes `ZOE format 2`. The first writer open of a store of
//! `ZOE format 1`, which has no count, counts the oids of `records` once
//! and writes the count and the new format name in one backend write. A
//! read-only open of such a store writes nothing and has no count.
//!
//! The test makes a store of format 1 with the test hook
//! `zoe_core::testing::{remove_meta, write_meta}`, which this card adds.
//! It checks the count against the oids of `Store::record_keys` after
//! each step, and on every crash image of commits, of a pack and of the
//! upgrade.

use std::path::PathBuf;

use zoe_core::testing;
use zoe_core::vfs::MemVfs;
use zoe_core::{Oid, Store, Tid};

/// A `PersistentMapping` record with no reference.
const PICKLE: &[u8] =
    b"\x80\x03cpersistent.mapping\nPersistentMapping\nq\x00.\x80\x03}q\x01X\x04\x00\x00\x00datag\x01s.";

const ROOT: Oid = Oid(0);

fn fresh_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("zoe-object-count-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

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

/// Commit `writes`, each a store of a pickle or a deletion (`None`), with
/// the serial that the store holds for the oid. Return the tid.
fn commit(store: &Store, writes: &[(Oid, Option<&[u8]>)]) -> Tid {
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
    txn.vote().unwrap();
    txn.finish().unwrap()
}

/// Commit the root and `n` objects that nothing refers to. Return their
/// oids.
fn root_and_orphans(store: &Store, n: usize) -> Vec<Oid> {
    let oids: Vec<Oid> = (0..n).map(|_| store.new_oid().unwrap()).collect();
    let mut writes = vec![(ROOT, Some(PICKLE))];
    writes.extend(oids.iter().map(|&oid| (oid, Some(PICKLE))));
    commit(store, &writes);
    oids
}

/// Make `store` a store of format 1: no `object_count`, and the format
/// name of before this card. A close writes no count, so the change stays.
fn downgrade(store: &Store) {
    testing::remove_meta(store, "object_count").unwrap();
    testing::write_meta(store, "format", b"ZOE format 1").unwrap();
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
    commit(&store, &[(oids[0], Some(PICKLE)), (ROOT, Some(PICKLE))]);
    assert_eq!(store.object_count(), Some(4));
    // A transaction that stores an oid twice counts it once.
    let new = store.new_oid().unwrap();
    commit(&store, &[(new, Some(PICKLE)), (new, Some(PICKLE))]);
    assert_eq!(store.object_count(), Some(5));
    assert_exact(&store, "after the commits");
}

#[test]
fn a_deletion_counts_until_a_pack_removes_the_object() {
    let store = Store::open_vfs(MemVfs::new()).unwrap();
    let oids = root_and_orphans(&store, 2);
    commit(&store, &[(oids[0], None)]);
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
    commit(&store, &[(ROOT, Some(PICKLE))]);
    for _ in 0..5 {
        commit(&store, &[(ROOT, Some(PICKLE))]);
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
fn the_first_writer_open_of_a_store_of_format_1_counts_once() {
    let dir = fresh_dir("upgrade");
    let store = Store::open(&dir).unwrap();
    let oids = root_and_orphans(&store, 4);
    commit(&store, &[(oids[1], None)]);
    downgrade(&store);
    drop(store);

    // A read-only open writes nothing and has no count.
    let database = dir.join("zoe.redb");
    let before = std::fs::read(&database).unwrap();
    let reader = Store::open_read_only(&dir).unwrap();
    assert_eq!(reader.object_count(), None);
    drop(reader);
    assert_eq!(std::fs::read(&database).unwrap(), before);

    // The first writer open counts and writes the count.
    let store = Store::open(&dir).unwrap();
    assert_eq!(store.object_count(), Some(5));
    assert_exact(&store, "after the upgrade");
    drop(store);

    // Then a read-only open reads it.
    let reader = Store::open_read_only(&dir).unwrap();
    assert_eq!(reader.object_count(), Some(5));
    drop(reader);
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
        commit(&store, &writes);
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
    commit(&store, &[(oids[0], None), (oids[1], None)]);
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
fn every_crash_image_of_the_upgrade_opens_with_an_exact_count() {
    let vfs = MemVfs::new();
    let store = Store::open_vfs(vfs.clone()).unwrap();
    let oids = root_and_orphans(&store, 6);
    commit(&store, &[(oids[2], None)]);
    downgrade(&store);
    drop(store);
    let vfs = vfs.durable_copy();
    let store = Store::open_vfs(vfs.clone()).unwrap();
    assert_eq!(store.object_count(), Some(7));
    drop(store);

    for image in vfs.crash_images() {
        // A crash before the write of the upgrade leaves format 1, and
        // this open counts again.
        let reopened =
            Store::open_vfs(image.vfs()).unwrap_or_else(|e| panic!("{image:?} does not open: {e}"));
        assert_eq!(reopened.object_count(), Some(7), "{image:?}");
    }
}
