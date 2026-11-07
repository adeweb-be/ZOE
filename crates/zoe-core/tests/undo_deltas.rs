//! Acceptance test of the undo of the text deltas (ADR 0060, section 5;
//! F11, F18).
//!
//! For each text delta of the undone transaction, the undo reads the live
//! text of its key as its own transaction sees it. When that text is the
//! text of the delta, it stages the live text before the transaction, or
//! an unindex when there is none. Otherwise it fails with `Undo`: text has
//! no merge. A later clear delta of the index fails the undo, also for an
//! unindex, and so does a clear delta in the undone transaction. Two undos
//! in one transaction undo one over the other, the later first. The vote
//! checks each staged delta against the commits after the read of the
//! undo. The catalog records of the same undo go through the tiers.

use zoe_core::vfs::MemVfs;
use zoe_core::{LiveText, Oid, Store, Tid, ZoeError};
use zoe_pickle::python::{Heap, Pickler, Value};

const CATALOG: Oid = Oid(42);
const INDEX: &str = "SearchableText";

fn store() -> Store {
    Store::open_vfs(MemVfs::new()).unwrap()
}

/// Return the snapshot of a transaction that begins now.
fn snapshot(store: &Store) -> Tid {
    Tid(store.last_transaction().0 + 1)
}

fn live(text: &str) -> Option<LiveText> {
    Some(LiveText {
        language: "en".to_owned(),
        text: text.to_owned(),
    })
}

/// Return the live text of `rid` now.
fn text(store: &Store, rid: i32) -> Option<LiveText> {
    store.text_before(CATALOG, INDEX, rid, Tid::MAX).unwrap()
}

/// Commit the text `value` of `rid`. Return the tid.
fn set(store: &Store, rid: i32, value: &str) -> Tid {
    let before = snapshot(store);
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.text_set(CATALOG, INDEX, rid, "en", value, before)
        .unwrap();
    txn.vote().unwrap();
    txn.finish().unwrap()
}

/// Commit an unindex of `rid`. Return the tid.
fn unindex(store: &Store, rid: i32) -> Tid {
    let before = snapshot(store);
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.text_unindex(CATALOG, INDEX, rid, before).unwrap();
    txn.vote().unwrap();
    txn.finish().unwrap()
}

/// Commit a clear delta of `INDEX`. Return the tid.
fn clear(store: &Store) -> Tid {
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.text_clear(CATALOG, INDEX).unwrap();
    txn.vote().unwrap();
    txn.finish().unwrap()
}

/// Undo `tid` in a transaction of its own. Return the tid of the undo.
fn undo(store: &Store, tid: Tid) -> Result<Tid, ZoeError> {
    let mut txn = store.tpc_begin(b"", b"undo", b"").unwrap();
    let undone = txn.undo(tid)?;
    assert_eq!(undone.conflicts, []);
    txn.vote()?;
    txn.finish()
}

fn refused(result: Result<Tid, ZoeError>) -> bool {
    matches!(result, Err(ZoeError::Undo { .. }))
}

/// Return the record of a `Length` of `value`.
fn length(value: i128) -> Vec<u8> {
    let mut heap = Heap::new();
    let class = heap.new_class("BTrees.Length", "Length");
    let mut pickler = Pickler::new(&heap);
    pickler.dump(class).unwrap();
    pickler.dump(Value::Int(value)).unwrap();
    pickler.into_bytes()
}

#[test]
fn an_undo_stages_the_live_text_before_the_transaction() {
    let store = store();
    let first = set(&store, 1, "one");
    let second = set(&store, 1, "two");
    undo(&store, second).unwrap();
    assert_eq!(text(&store, 1), live("one"));
    // The undo of a first text stages an unindex.
    undo(&store, first).unwrap();
    assert_eq!(text(&store, 1), None);

    // The undo of an unindex gives the text back. No live text now equals
    // the unindex.
    set(&store, 2, "kept");
    let gone = unindex(&store, 2);
    undo(&store, gone).unwrap();
    assert_eq!(text(&store, 2), live("kept"));
}

#[test]
fn a_later_change_of_the_key_fails_the_undo() {
    let store = store();
    set(&store, 1, "one");
    let second = set(&store, 1, "two");
    set(&store, 1, "three");
    assert!(refused(undo(&store, second)));
    assert_eq!(text(&store, 1), live("three"));

    let gone = unindex(&store, 1);
    set(&store, 1, "again");
    assert!(refused(undo(&store, gone)));
}

#[test]
fn a_later_clear_of_the_index_fails_the_undo_also_for_an_unindex() {
    let store = store();
    set(&store, 1, "one");
    let second = set(&store, 1, "two");
    set(&store, 2, "other");
    let gone = unindex(&store, 2);
    clear(&store);
    assert!(refused(undo(&store, second)));
    assert!(refused(undo(&store, gone)));
    assert_eq!(text(&store, 1), None);
    assert_eq!(text(&store, 2), None);
}

#[test]
fn an_undo_of_a_transaction_with_a_clear_delta_fails() {
    let store = store();
    set(&store, 1, "one");
    let cleared = clear(&store);
    assert!(refused(undo(&store, cleared)));
    assert_eq!(text(&store, 1), None);
}

#[test]
fn two_undos_in_one_transaction_undo_one_over_the_other() {
    let store = store();
    set(&store, 1, "one");
    let second = set(&store, 1, "two");
    let third = set(&store, 1, "three");

    // The earlier first fails: the text now is not the text of `second`.
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    assert!(matches!(txn.undo(second), Err(ZoeError::Undo { .. })));
    txn.abort();

    // The later first: the second undo reads the text that the first one
    // staged.
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.undo(third).unwrap();
    txn.undo(second).unwrap();
    txn.vote().unwrap();
    txn.finish().unwrap();
    assert_eq!(text(&store, 1), live("one"));
}

#[test]
fn a_commit_of_the_key_after_the_read_of_the_undo_fails_the_vote() {
    let store = store();
    set(&store, 1, "one");
    let second = set(&store, 1, "two");
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.undo(second).unwrap();
    set(&store, 1, "three");
    match txn.vote() {
        Err(ZoeError::TextConflict { catalog }) => assert_eq!(catalog, CATALOG),
        other => panic!("{other:?}"),
    }
    txn.abort();
    assert_eq!(text(&store, 1), live("three"));
}

#[test]
fn the_catalog_records_of_the_same_undo_go_through_the_tiers() {
    let store = store();
    let counter = store.new_oid().unwrap();
    let before = snapshot(&store);
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(counter, Tid::ZERO, &length(10)).unwrap();
    txn.text_set(CATALOG, INDEX, 1, "en", "one", before).unwrap();
    txn.vote().unwrap();
    let first = txn.finish().unwrap();

    let before = snapshot(&store);
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(counter, first, &length(15)).unwrap();
    txn.text_set(CATALOG, INDEX, 1, "en", "two", before).unwrap();
    txn.vote().unwrap();
    let undone = txn.finish().unwrap();
    let later = zoe_core::testing::commit(&store, counter, undone, &length(17));

    // The undo merges the counter through tier 1, and gives the text back.
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    let staged = txn.undo(undone).unwrap();
    assert_eq!(staged.oids, [counter]);
    assert_eq!(staged.conflicts, []);
    // A commit of the counter after the undo: the vote merges it through
    // tier 1 too.
    zoe_core::testing::commit(&store, counter, later, &length(20));
    txn.vote().unwrap();
    txn.finish().unwrap();
    assert_eq!(store.load(counter).unwrap().0, length(15));
    assert_eq!(text(&store, 1), live("one"));
}
