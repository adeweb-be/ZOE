//! Undo in the core: `Txn::undo` and the undo log (`Store::undo_log`).
//!
//! The acceptance test `test_undo.py` compares the undo with FileStorage,
//! in-process and through `zoe serve`. These tests check the core alone.

use std::io::{Read, Write};
use std::os::unix::fs::MetadataExt;

use zoe_core::vfs::MemVfs;
use zoe_core::{Oid, Store, Tid, ZoeError, undo_id, undo_tid};
use zoe_pickle::python::{Heap, Pickler, Value};

const PICKLE: &[u8] = b"\x80\x03cZODB.blob\nBlob\nq\x00.\x80\x03N.";

fn store() -> Store {
    Store::open_vfs(MemVfs::new()).unwrap()
}

/// Returns the record of a `Length` of `value`.
fn length(value: i128) -> Vec<u8> {
    let mut heap = Heap::new();
    let class = heap.new_class("BTrees.Length", "Length");
    let mut pickler = Pickler::new(&heap);
    pickler.dump(class).unwrap();
    pickler.dump(Value::Int(value)).unwrap();
    pickler.into_bytes()
}

fn commit(store: &Store, oid: Oid, serial: Tid, data: &[u8]) -> Tid {
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(oid, serial, data).unwrap();
    txn.vote().unwrap();
    txn.finish().unwrap()
}

/// Undoes `tid` in a transaction of its own, and returns the tid of the
/// undo.
fn undo(store: &Store, tid: Tid) -> Result<Tid, ZoeError> {
    let mut txn = store.tpc_begin(b"", b"undo", b"").unwrap();
    let undone = txn.undo(tid)?;
    assert_eq!(undone.conflicts, []);
    txn.vote()?;
    txn.finish()
}

fn current(store: &Store, oid: Oid) -> Result<Vec<u8>, ZoeError> {
    store.load(oid).map(|(data, _)| data)
}

#[test]
fn an_undo_stores_the_state_before_the_transaction() {
    let store = store();
    let oid = store.new_oid().unwrap();
    let first = commit(&store, oid, Tid::ZERO, b"one");
    let second = commit(&store, oid, first, b"two");
    let tid = undo(&store, second).unwrap();
    assert_eq!(store.load(oid).unwrap(), (b"one".to_vec(), tid));
    // An undo is a transaction: its own undo gives the state back.
    undo(&store, tid).unwrap();
    assert_eq!(current(&store, oid).unwrap(), b"two");
    assert_eq!(store.history(oid, 10).unwrap().len(), 4);
}

#[test]
fn an_undo_of_a_creation_deletes_the_object() {
    let store = store();
    let oid = store.new_oid().unwrap();
    let created = commit(&store, oid, Tid::ZERO, b"one");
    let tid = undo(&store, created).unwrap();
    assert!(matches!(
        current(&store, oid),
        Err(ZoeError::NotFound { .. })
    ));
    // The undo of the undo creates the object again.
    undo(&store, tid).unwrap();
    assert_eq!(current(&store, oid).unwrap(), b"one");
}

#[test]
fn an_undo_after_a_later_change_merges_through_tier_1() {
    let store = store();
    let oid = store.new_oid().unwrap();
    let first = commit(&store, oid, Tid::ZERO, &length(10));
    let undone = commit(&store, oid, first, &length(15));
    commit(&store, oid, undone, &length(17));
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    let staged = txn.undo(undone).unwrap();
    assert_eq!(staged.oids, [oid]);
    assert_eq!(staged.conflicts, []);
    assert_eq!(txn.resolutions().len(), 1);
    assert!(txn.resolutions()[0].applied);
    txn.vote().unwrap();
    // The undo merge is not a merge of the vote.
    assert_eq!(txn.natively_resolved(), []);
    txn.finish().unwrap();
    assert_eq!(current(&store, oid).unwrap(), length(12));
}

#[test]
fn an_undo_that_no_tier_merges_goes_to_the_caller() {
    let store = store();
    let oid = store.new_oid().unwrap();
    let first = commit(&store, oid, Tid::ZERO, b"one");
    let undone = commit(&store, oid, first, b"two");
    let last = commit(&store, oid, undone, b"three");
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    let staged = txn.undo(undone).unwrap();
    assert_eq!(staged.oids, [oid]);
    let [conflict] = &staged.conflicts[..] else {
        panic!("{staged:?}");
    };
    assert_eq!(conflict.oid, oid);
    assert_eq!(conflict.serial, last);
    assert_eq!(conflict.current, b"three");
    assert_eq!(conflict.undone, undone);
    assert_eq!(conflict.before, b"one");
    // The call stages nothing for the oid: the caller stores the result.
    assert_eq!(txn.staged(oid).unwrap(), None);
    txn.store(oid, conflict.serial, b"resolved").unwrap();
    txn.vote().unwrap();
    txn.finish().unwrap();
    assert_eq!(current(&store, oid).unwrap(), b"resolved");
}

#[test]
fn a_refusal_of_tier_1_fails_the_undo() {
    let store = store();
    let oid = store.new_oid().unwrap();
    let mut heap = Heap::new();
    let bucket = |heap: &mut Heap, items: &[(i128, i128)]| {
        let class = heap.new_class("BTrees.IIBTree", "IIBucket");
        let flat = items
            .iter()
            .flat_map(|&(k, v)| [Value::Int(k), Value::Int(v)])
            .collect();
        let state = zoe_pickle::btrees::new_bucket_state(heap, flat, None);
        let mut pickler = Pickler::new(heap);
        pickler.dump(class).unwrap();
        pickler.dump(state).unwrap();
        pickler.into_bytes()
    };
    let first = commit(&store, oid, Tid::ZERO, &bucket(&mut heap, &[(1, 1)]));
    let undone = commit(&store, oid, first, &bucket(&mut heap, &[(1, 1), (2, 1)]));
    commit(&store, oid, undone, &bucket(&mut heap, &[(1, 1), (2, 2)]));
    // The undo deletes 2, which the later transaction changed.
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    assert!(matches!(
        txn.undo(undone),
        Err(ZoeError::Undo { oid: Some(found), .. }) if found == oid
    ));
}

#[test]
fn an_undo_of_a_creation_that_a_later_transaction_changed_fails() {
    let store = store();
    let oid = store.new_oid().unwrap();
    let created = commit(&store, oid, Tid::ZERO, b"one");
    commit(&store, oid, created, b"two");
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    assert!(matches!(txn.undo(created), Err(ZoeError::Undo { .. })));
}

#[test]
fn two_undos_of_one_object_in_one_transaction_undo_both() {
    let store = store();
    let oid = store.new_oid().unwrap();
    let first = commit(&store, oid, Tid::ZERO, b"one");
    let second = commit(&store, oid, first, b"two");
    let third = commit(&store, oid, second, b"three");
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.undo(third).unwrap();
    // The second undo sees the state that the first one staged.
    let staged = txn.undo(second).unwrap();
    assert_eq!(staged.conflicts, []);
    txn.vote().unwrap();
    txn.finish().unwrap();
    assert_eq!(current(&store, oid).unwrap(), b"one");
}

#[test]
fn a_change_after_the_undo_read_its_object_is_a_conflict_of_the_vote() {
    let store = store();
    let oid = store.new_oid().unwrap();
    let first = commit(&store, oid, Tid::ZERO, b"one");
    let second = commit(&store, oid, first, b"two");
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.undo(second).unwrap();
    commit(&store, oid, second, b"three");
    assert!(matches!(txn.vote(), Err(ZoeError::Conflict { .. })));
}

#[test]
fn nothing_is_undoable_at_or_before_the_last_pack() {
    let store = store();
    // The root stays after a pack. The pack reads the references of the
    // records, so they are pickles.
    let oid = Oid(0);
    let first = commit(&store, oid, Tid::ZERO, &length(1));
    let second = commit(&store, oid, first, &length(2));
    let third = commit(&store, oid, second, &length(3));
    store.pack(second).unwrap();
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    assert!(matches!(
        txn.undo(second),
        Err(ZoeError::Undo { oid: None, .. })
    ));
    drop(txn);
    undo(&store, third).unwrap();
    assert_eq!(current(&store, oid).unwrap(), length(2));
    // The undo log starts after the pack.
    let log: Vec<Tid> = store
        .undo_log(Tid::MAX, 10)
        .unwrap()
        .into_iter()
        .map(|row| row.tid)
        .collect();
    assert_eq!(log.len(), 2);
    assert_eq!(log[1], third);
}

#[test]
fn an_undo_whose_state_before_a_pack_removed_fails() {
    let store = store();
    let root = commit(&store, Oid(0), Tid::ZERO, &length(0));
    // No reader can reach this object, so the pack removes its old record.
    let oid = store.new_oid().unwrap();
    let first = commit(&store, oid, Tid::ZERO, &length(1));
    let second = commit(&store, oid, first, &length(2));
    store.pack(Tid(second.0 - 1)).unwrap();
    assert!(store.pack_watermark().unwrap() > root);
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    assert!(matches!(
        txn.undo(second),
        Err(ZoeError::Undo { oid: Some(found), .. }) if found == oid
    ));
}

#[test]
fn an_undo_of_a_transaction_that_the_store_does_not_have_fails() {
    let store = store();
    let oid = store.new_oid().unwrap();
    let tid = commit(&store, oid, Tid::ZERO, b"one");
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    assert!(matches!(
        txn.undo(Tid(tid.0 + 1)),
        Err(ZoeError::Undo { oid: None, .. })
    ));
}

#[test]
fn a_restore_transaction_refuses_an_undo() {
    let store = store();
    let oid = store.new_oid().unwrap();
    let tid = commit(&store, oid, Tid::ZERO, b"one");
    let mut txn = store
        .tpc_begin_restore(Tid(tid.0 + 1), b"", b"", b"")
        .unwrap();
    assert!(matches!(txn.undo(tid), Err(ZoeError::Protocol(_))));
}

#[test]
fn the_undo_log_is_newest_first_with_the_rows_of_the_transactions() {
    let store = store();
    let oid = store.new_oid().unwrap();
    let mut tids = Vec::new();
    let mut serial = Tid::ZERO;
    for n in 0..5u8 {
        let mut txn = store.tpc_begin(b"user", &[b'0' + n], b"").unwrap();
        txn.store(oid, serial, &[n + 1]).unwrap();
        txn.vote().unwrap();
        serial = txn.finish().unwrap();
        tids.push(serial);
    }
    let log = store.undo_log(Tid::MAX, 3).unwrap();
    let found: Vec<Tid> = log.iter().map(|row| row.tid).collect();
    assert_eq!(found, [tids[4], tids[3], tids[2]]);
    assert_eq!(log[0].description, b"4");
    assert_eq!(log[0].user, b"user");
    // The next part starts before the last row of the part before it.
    let rest = store.undo_log(tids[2], 10).unwrap();
    assert_eq!(rest.len(), 2);
    let range: Vec<Tid> = store
        .undo_log_range(1, -3)
        .unwrap()
        .into_iter()
        .map(|row| row.tid)
        .collect();
    assert_eq!(range, [tids[3], tids[2], tids[1]]);
    assert_eq!(store.undo_log_range(4, 100).unwrap().len(), 1);
    // An undo id is the base64 text of the tid.
    assert_eq!(undo_tid(&undo_id(tids[0])), Some(tids[0]));
}

fn commit_blob(store: &Store, oid: Oid, serial: Tid, bytes: &[u8]) -> Tid {
    let mut file = store.new_blob_file().unwrap();
    file.write_all(bytes).unwrap();
    let mut txn = store.tpc_begin(b"", b"blob", b"").unwrap();
    txn.store_blob(oid, serial, PICKLE, file).unwrap();
    txn.vote().unwrap();
    txn.finish().unwrap()
}

fn read_blob(store: &Store, oid: Oid, tid: Tid) -> Vec<u8> {
    let mut bytes = Vec::new();
    store
        .open_blob(oid, tid)
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    bytes
}

#[test]
fn a_blob_undo_links_the_earlier_blob_file() {
    let dir = std::env::temp_dir().join(format!("zoe-undo-blob-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store = Store::open(&dir).unwrap();
    let oid = store.new_oid().unwrap();
    let first = commit_blob(&store, oid, Tid::ZERO, b"first blob");
    let second = commit_blob(&store, oid, first, b"second blob");
    let before = std::fs::metadata(store.blob_path(oid, first).unwrap()).unwrap();
    let tid = undo(&store, second).unwrap();
    assert_eq!(read_blob(&store, oid, tid), b"first blob");
    assert_eq!(read_blob(&store, oid, second), b"second blob");
    // The blob of the undo is the earlier file under a second name.
    let linked = std::fs::metadata(store.blob_path(oid, tid).unwrap()).unwrap();
    assert_eq!(linked.ino(), before.ino());
    assert_eq!(linked.mtime_nsec(), before.mtime_nsec());
    assert_eq!(linked.nlink(), 2);
    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn every_crash_image_of_a_blob_undo_is_whole_or_absent() {
    let built = MemVfs::new();
    let (oid, first, second) = {
        let store = Store::open_vfs(built.clone()).unwrap();
        let oid = store.new_oid().unwrap();
        let first = commit_blob(&store, oid, Tid::ZERO, b"first blob");
        let second = commit_blob(&store, oid, first, b"second blob");
        (oid, first, second)
    };
    let vfs = built.durable_copy();
    let tid = {
        let store = Store::open_vfs(vfs.clone()).unwrap();
        let tid = undo(&store, second).unwrap();
        vfs.mark("acknowledged");
        tid
    };
    for image in vfs.crash_images() {
        let image_vfs = image.vfs();
        let store = Store::open_vfs(image_vfs.clone())
            .unwrap_or_else(|e| panic!("{image:?} does not open: {e}"));
        let (data, serial) = store.load(oid).unwrap();
        assert_eq!(data, PICKLE, "{image:?}");
        if serial == tid {
            assert_eq!(read_blob(&store, oid, tid), b"first blob", "{image:?}");
        } else {
            assert_eq!(serial, second, "{image:?}");
            assert!(!image.is_after("acknowledged"), "{image:?} lost the undo");
        }
        // The open removed a link that no blob record names.
        let files = image_vfs
            .names()
            .iter()
            .filter(|name| name.ends_with(".blob"))
            .count();
        assert_eq!(files, if serial == tid { 3 } else { 2 }, "{image:?}");
        // The earlier blob files stay.
        assert_eq!(read_blob(&store, oid, first), b"first blob", "{image:?}");
        assert_eq!(read_blob(&store, oid, second), b"second blob", "{image:?}");
    }
}
