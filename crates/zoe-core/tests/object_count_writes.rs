//! The object count through each kind of write: restores, a pack between
//! the vote and the finish of a commit, a pack without garbage collection,
//! the writes that change no record, and the opens that refuse a store.

use zoe_core::testing;
use zoe_core::vfs::MemVfs;
use zoe_core::{Oid, Options, Store, SystemClock, Tid, ZoeError};

/// A `PersistentMapping` record with no reference.
const PICKLE: &[u8] = b"\x80\x03cpersistent.mapping\nPersistentMapping\nq\x00.\x80\x03}q\x01.";

const ROOT: Oid = Oid(0);

/// Return the number of oids that have a record in `store`.
fn oids_with_records(store: &Store) -> u64 {
    let mut keys = store.record_keys(Oid(0), usize::MAX).unwrap();
    keys.dedup_by_key(|(oid, _)| *oid);
    keys.len() as u64
}

fn assert_exact(store: &Store) {
    assert_eq!(store.object_count(), Some(oids_with_records(store)));
}

/// Return the serial of `oid` in `store`, or `Tid::ZERO` for a new oid.
fn serial(store: &Store, oid: Oid) -> Tid {
    store
        .history(oid, 1)
        .ok()
        .and_then(|entries| entries.first().map(|entry| entry.tid))
        .unwrap_or(Tid::ZERO)
}

/// Commit a store of `PICKLE` for each oid of `oids`.
fn commit(store: &Store, oids: &[Oid]) -> Tid {
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    for &oid in oids {
        txn.store(oid, serial(store, oid), PICKLE).unwrap();
    }
    txn.vote().unwrap();
    txn.finish().unwrap()
}

/// Commit the root and `n` objects that nothing refers to.
fn root_and_orphans(store: &Store, n: usize) -> Vec<Oid> {
    let oids: Vec<Oid> = (0..n).map(|_| store.new_oid().unwrap()).collect();
    let mut all = vec![ROOT];
    all.extend(&oids);
    commit(store, &all);
    oids
}

/// Return a tid after the last tid of `store`.
fn next_tid(store: &Store) -> Tid {
    Tid(store.last_transaction().0 + 1_000)
}

#[test]
fn a_restore_counts_its_new_oids_and_a_deletion_too() {
    let store = Store::open_vfs(MemVfs::new()).unwrap();
    root_and_orphans(&store, 1);
    let mut txn = store
        .tpc_begin_restore(next_tid(&store), b"", b"", b"")
        .unwrap();
    txn.restore(Oid(10), Some(PICKLE)).unwrap();
    txn.restore(Oid(11), None).unwrap();
    txn.restore(ROOT, Some(PICKLE)).unwrap();
    txn.vote().unwrap();
    txn.finish().unwrap();
    assert_eq!(store.object_count(), Some(4));
    assert_exact(&store);
}

#[test]
fn a_deferred_restore_counts_its_new_oids() {
    let vfs = MemVfs::new();
    let store = Store::open_vfs(vfs.clone()).unwrap();
    for oid in [ROOT, Oid(5), Oid(6)] {
        let mut txn = store
            .tpc_begin_restore_deferred(next_tid(&store), b"", b"", b"")
            .unwrap();
        txn.restore(oid, Some(PICKLE)).unwrap();
        txn.vote().unwrap();
        txn.finish().unwrap();
    }
    assert_eq!(store.object_count(), Some(3));
    store.sync().unwrap();
    drop(store);
    let store = Store::open_vfs(vfs).unwrap();
    assert_eq!(store.object_count(), Some(3));
}

#[test]
fn a_pack_between_the_vote_and_the_finish_keeps_the_count_exact() {
    let store = Store::open_vfs(MemVfs::new()).unwrap();
    let oids = root_and_orphans(&store, 2);
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(oids[0], serial(&store, oids[0]), PICKLE).unwrap();
    txn.vote().unwrap();
    // Nothing refers to the two objects: the pack removes both, also the
    // one that the voted transaction writes.
    store.pack(store.last_transaction()).unwrap();
    assert_eq!(store.object_count(), Some(1));
    // The finish writes the first object again, as a new object.
    txn.finish().unwrap();
    assert_eq!(store.object_count(), Some(2));
    assert_exact(&store);
}

#[test]
fn a_pack_without_garbage_collection_lowers_the_count_for_a_deleted_object() {
    let options = Options {
        pack_gc: false,
        ..Options::default()
    };
    let store = Store::open_full(MemVfs::new(), SystemClock, options).unwrap();
    let oids = root_and_orphans(&store, 2);
    store.pack(store.last_transaction()).unwrap();
    // Without garbage collection, the pack keeps the unreachable objects.
    assert_eq!(store.object_count(), Some(3));
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.delete(oids[0], serial(&store, oids[0])).unwrap();
    txn.vote().unwrap();
    txn.finish().unwrap();
    assert_eq!(store.object_count(), Some(3));
    store.pack(store.last_transaction()).unwrap();
    assert_eq!(store.object_count(), Some(2));
    assert_exact(&store);
}

#[test]
fn a_dry_run_a_sequence_and_a_sync_keep_the_count() {
    let vfs = MemVfs::new();
    let store = Store::open_vfs(vfs.clone()).unwrap();
    root_and_orphans(&store, 3);
    store.pack_dry_run(store.last_transaction()).unwrap();
    store.sequence("ids").next().unwrap();
    store.sync().unwrap();
    assert_eq!(store.object_count(), Some(4));
    drop(store);
    let store = Store::open_vfs(vfs).unwrap();
    assert_eq!(store.object_count(), Some(4));
    assert_exact(&store);
}

#[test]
fn a_store_of_format_2_without_a_count_does_not_open() {
    let vfs = MemVfs::new();
    let store = Store::open_vfs(vfs.clone()).unwrap();
    root_and_orphans(&store, 1);
    testing::remove_meta(&store, "object_count").unwrap();
    drop(store);
    let error = Store::open_vfs(vfs).err();
    assert!(
        matches!(&error, Some(ZoeError::Format(text)) if text.contains("object count")),
        "{error:?}"
    );
}

#[test]
fn a_count_that_is_not_8_bytes_does_not_open() {
    let vfs = MemVfs::new();
    let store = Store::open_vfs(vfs.clone()).unwrap();
    testing::write_meta(&store, "object_count", b"\x01").unwrap();
    drop(store);
    let error = Store::open_vfs(vfs).err();
    assert!(matches!(error, Some(ZoeError::Format(_))), "{error:?}");
}

#[test]
fn another_format_name_does_not_open() {
    let vfs = MemVfs::new();
    let store = Store::open_vfs(vfs.clone()).unwrap();
    testing::write_meta(&store, "format", b"ZOE format 9").unwrap();
    drop(store);
    let error = Store::open_vfs(vfs).err();
    assert!(
        matches!(&error, Some(ZoeError::Format(text)) if text.contains("ZOE format 9")),
        "{error:?}"
    );
}
