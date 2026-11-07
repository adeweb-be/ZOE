//! Acceptance test of the checks of the text deltas in the vote (ADR 0060,
//! section 8).
//!
//! The vote that takes the locks reserves the key of each staged text
//! delta in a set of the store, before its checks. A key that another
//! transaction reserved fails the vote at once with `TextConflict`, also
//! after a lock hand-off of ADR 0044. Then each text delta is checked
//! against the deltas committed since the snapshot of its transaction: a
//! delta equal to the live text is dropped, and another text fails the
//! vote with `TextConflict` and the oid of the catalog. A transaction that
//! stages a clear delta of the index writes its later text deltas with no
//! check. A vote whose snapshot is at or before the pack watermark fails
//! with `TextConflict`. The reservations survive the two votes of
//! `vote_resolving`, a failed vote releases them, and the finish releases
//! them before its locks, so a vote that the end of a commit wakes finds
//! its key free. A finish never fails for a delta.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use support::{CountingClock, MeteredVfs};
use zoe_core::testing::{self, commit, new_oid, wait_until};
use zoe_core::vfs::MemVfs;
use zoe_core::{LiveText, Observer, Oid, Options, Store, Tid, Txn, ZoeError};

const CATALOG: Oid = Oid(42);
const INDEX: &str = "SearchableText";

/// A `PersistentMapping` record: no tier merges it.
const MAPPING: &[u8] =
    b"\x80\x03cpersistent.mapping\nPersistentMapping\nq\x00.\x80\x03}q\x01X\x04\x00\x00\x00datag\x01s.";

/// A record of `BTrees.Length.Length` with the value `value`: tier 1
/// merges it, so a finish hands its lock off.
fn length(value: u8) -> Vec<u8> {
    let mut record = b"\x80\x03cBTrees.Length\nLength\nq\x00.\x80\x03K".to_vec();
    record.push(value);
    record.push(b'.');
    record
}

fn snapshot(store: &Store) -> Tid {
    Tid(store.last_transaction().0 + 1)
}

fn live(text: &str) -> Option<LiveText> {
    Some(LiveText {
        language: "en".to_owned(),
        text: text.to_owned(),
    })
}

fn text(store: &Store, rid: i32) -> Option<LiveText> {
    store.text_before(CATALOG, INDEX, rid, Tid::MAX).unwrap()
}

/// Begin a transaction that stages the text `value` of `rid` with the
/// snapshot `before`.
fn setting<'s>(store: &'s Store, rid: i32, value: &str, before: Tid) -> Txn<&'s Store> {
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.text_set(CATALOG, INDEX, rid, "en", value, before)
        .unwrap();
    txn
}

/// Return the number of deltas that the commit `tid` wrote.
fn deltas_of(store: &Store, tid: Tid) -> usize {
    store
        .text_changed(Tid(tid.0 - 1), tid)
        .unwrap()
        .into_iter()
        .filter(|change| change.tid == tid)
        .count()
}

fn is_text_conflict(result: &zoe_core::Result<()>) -> bool {
    matches!(result, Err(ZoeError::TextConflict { catalog }) if *catalog == CATALOG)
}

#[test]
fn a_second_writer_of_a_key_with_another_text_conflicts_and_with_the_same_text_writes_nothing() {
    let store = Store::open_vfs(MemVfs::new()).unwrap();
    let before = snapshot(&store);
    let mut first = setting(&store, 1, "first", before);
    first.vote().unwrap();
    first.finish().unwrap();
    let mut second = setting(&store, 1, "second", before);
    let voted = second.vote();
    assert!(is_text_conflict(&voted), "{voted:?}");
    drop(second);
    assert_eq!(text(&store, 1), live("first"));

    // The same text from the same snapshot: both commit, and the second
    // writes no delta.
    let before = snapshot(&store);
    let mut first = setting(&store, 2, "same", before);
    first.vote().unwrap();
    let t1 = first.finish().unwrap();
    let mut second = setting(&store, 2, "same", before);
    second.vote().unwrap();
    let t2 = second.finish().unwrap();
    assert_eq!(deltas_of(&store, t1), 1);
    assert_eq!(deltas_of(&store, t2), 0);
    assert_eq!(text(&store, 2), live("same"));
}

#[test]
fn a_text_equal_to_the_live_text_at_the_snapshot_is_dropped() {
    let store = Store::open_vfs(MemVfs::new()).unwrap();
    let mut txn = setting(&store, 1, "kept", snapshot(&store));
    txn.vote().unwrap();
    txn.finish().unwrap();
    let before = snapshot(&store);
    let mut txn = setting(&store, 1, "kept", before);
    // An unindex of a document with no live text is dropped too.
    txn.text_unindex(CATALOG, INDEX, 7, before).unwrap();
    txn.vote().unwrap();
    let tid = txn.finish().unwrap();
    assert_eq!(deltas_of(&store, tid), 0);
    assert_eq!(text(&store, 1), live("kept"));
}

#[test]
fn a_clear_and_then_a_text_delta_of_the_index_are_written_with_no_check() {
    let store = Store::open_vfs(MemVfs::new()).unwrap();
    let before = snapshot(&store);
    let mut other = setting(&store, 1, "other", before);
    other.vote().unwrap();
    other.finish().unwrap();
    // This transaction read before the commit of `other`, clears the
    // index, then sets another text of the same key.
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.text_clear(CATALOG, INDEX).unwrap();
    txn.text_set(CATALOG, INDEX, 1, "en", "mine", before)
        .unwrap();
    txn.vote().unwrap();
    let tid = txn.finish().unwrap();
    assert_eq!(deltas_of(&store, tid), 2);
    assert_eq!(text(&store, 1), live("mine"));
}

#[test]
fn a_vote_that_stages_a_key_of_a_transaction_in_flight_fails_at_once() {
    let store = Store::open_vfs(MemVfs::new()).unwrap();
    let before = snapshot(&store);
    let mut first = setting(&store, 1, "first", before);
    first.vote().unwrap();
    // The first transaction holds the key until its finish.
    let mut second = setting(&store, 1, "first", before);
    let voted = second.vote();
    assert!(is_text_conflict(&voted), "{voted:?}");
    drop(second);
    // Another key is free.
    let mut third = setting(&store, 2, "third", before);
    third.vote().unwrap();
    first.finish().unwrap();
    third.finish().unwrap();
    assert_eq!(text(&store, 1), live("first"));
    assert_eq!(text(&store, 2), live("third"));
}

/// A point where the leader of a batch stops after its sync while the gate
/// is armed, until the test opens it, and a count of the hand-offs.
#[derive(Default)]
struct Gate {
    armed: AtomicBool,
    held: AtomicBool,
    open: AtomicBool,
    handed_off: AtomicUsize,
}

impl Observer for Gate {
    fn batch_synced(&self, _: usize) {
        if self.armed.load(Ordering::SeqCst) {
            self.held.store(true, Ordering::SeqCst);
            while !self.open.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(1));
            }
        }
    }

    fn locks_handed_off(&self, locks: usize) {
        self.handed_off.fetch_add(locks, Ordering::SeqCst);
    }
}

#[test]
fn a_vote_that_takes_a_hand_off_and_stages_a_key_of_the_holder_fails_at_once() {
    let clock = CountingClock::default();
    let store = Store::open_full(MemVfs::new(), clock.clone(), Options::default()).unwrap();
    let gate = Arc::new(Gate::default());
    assert!(store.set_observer(gate.clone()));
    let oid = new_oid(&store);
    let read = commit(&store, oid, Tid::ZERO, &length(1));
    let before = snapshot(&store);
    let mut first = setting(&store, 1, "same", before);
    first.store(oid, read, &length(2)).unwrap();
    first.vote().unwrap();
    // Let the fold thread wait, so the next wait is the lock wait.
    store.fold().unwrap();
    thread::sleep(Duration::from_millis(50));
    let waits = clock.waits.load(Ordering::SeqCst);
    thread::scope(|scope| {
        let store = &store;
        let second = scope.spawn(move || {
            // The same text: without the reservation of the holder, check
            // 3 would drop it and the vote would pass.
            let mut second = setting(store, 1, "same", before);
            second.store(oid, read, &length(3)).unwrap();
            second.vote()
        });
        wait_until(|| clock.waits.load(Ordering::SeqCst) > waits);
        gate.armed.store(true, Ordering::SeqCst);
        let finish = scope.spawn(move || first.finish());
        // The holder stops after its sync, and its commit is in flight.
        wait_until(|| gate.held.load(Ordering::SeqCst));
        // The second vote got the lock with the hand-off, and fails while
        // the holder still holds the key: it does not wait for the commit.
        wait_until(|| second.is_finished());
        let voted = second.join().unwrap();
        assert!(is_text_conflict(&voted), "{voted:?}");
        gate.open.store(true, Ordering::SeqCst);
        finish.join().unwrap().unwrap();
    });
    assert_eq!(gate.handed_off.load(Ordering::SeqCst), 1);
    assert_eq!(text(&store, 1), live("same"));
    assert!(store.lock_table_is_empty());
}

#[test]
fn a_vote_at_or_before_the_pack_watermark_conflicts() {
    let store = Store::open_vfs(MemVfs::new()).unwrap();
    let oid = new_oid(&store);
    let before = snapshot(&store);
    commit(&store, oid, Tid::ZERO, MAPPING);
    let last = commit(&store, oid, store.last_transaction(), MAPPING);
    store.pack(last).unwrap();
    assert_eq!(store.pack_watermark().unwrap(), last);
    let mut txn = setting(&store, 1, "late", before);
    let voted = txn.vote();
    assert!(is_text_conflict(&voted), "{voted:?}");
    drop(txn);
    let mut txn = setting(&store, 1, "late", last);
    assert!(is_text_conflict(&txn.vote()));
    drop(txn);
    // A snapshot after the watermark passes.
    let mut txn = setting(&store, 1, "fresh", snapshot(&store));
    txn.vote().unwrap();
    txn.finish().unwrap();
    assert_eq!(text(&store, 1), live("fresh"));
}

#[test]
fn the_reservations_survive_vote_resolving_and_a_failed_vote_releases_them() {
    let store = Store::open_vfs(MemVfs::new()).unwrap();
    let oid = new_oid(&store);
    let read = commit(&store, oid, Tid::ZERO, MAPPING);
    let before = snapshot(&store);
    let current = commit(&store, oid, read, MAPPING);

    // A stale oid that the caller resolves: the transaction keeps its locks
    // and its key between the two votes.
    let mut txn = setting(&store, 1, "resolving", before);
    txn.store(oid, read, MAPPING).unwrap();
    let conflicts = txn.vote_resolving().unwrap();
    assert_eq!(conflicts.len(), 1);
    let mut other = setting(&store, 1, "resolving", before);
    assert!(is_text_conflict(&other.vote()));
    drop(other);
    txn.store(oid, current, MAPPING).unwrap();
    txn.vote().unwrap();
    let mut other = setting(&store, 1, "resolving", before);
    assert!(is_text_conflict(&other.vote()));
    drop(other);
    txn.finish().unwrap();
    assert_eq!(text(&store, 1), live("resolving"));

    // A vote that fails on a stale serial releases the key.
    let before = snapshot(&store);
    let mut failed = setting(&store, 2, "failed", before);
    failed.store(oid, read, MAPPING).unwrap();
    assert!(matches!(failed.vote(), Err(ZoeError::Conflict { .. })));
    let mut next = setting(&store, 2, "next", before);
    next.vote().unwrap();
    next.finish().unwrap();
    drop(failed);
    assert_eq!(text(&store, 2), live("next"));

    // A failed vote of vote_resolving releases it too.
    let before = snapshot(&store);
    let mut failed = setting(&store, 3, "failed", before);
    failed.store(oid, Tid::ZERO, MAPPING).unwrap();
    assert!(failed.vote_resolving().is_err());
    let mut next = setting(&store, 3, "next", before);
    next.vote().unwrap();
    next.finish().unwrap();
    assert_eq!(text(&store, 3), live("next"));

    // An abort releases it.
    let before = snapshot(&store);
    let mut aborted = setting(&store, 4, "aborted", before);
    aborted.vote().unwrap();
    aborted.abort();
    let mut next = setting(&store, 4, "next", before);
    next.vote().unwrap();
    next.finish().unwrap();
    assert_eq!(text(&store, 4), live("next"));
}

#[test]
fn a_vote_that_the_end_of_a_commit_wakes_finds_its_key_free() {
    let store = Store::open_vfs(MemVfs::new()).unwrap();
    let oid = new_oid(&store);
    let read = commit(&store, oid, Tid::ZERO, MAPPING);
    let before = snapshot(&store);
    let mut first = setting(&store, 1, "same", before);
    first.store(oid, read, MAPPING).unwrap();
    first.vote().unwrap();
    thread::scope(|scope| {
        let store = &store;
        let second = scope.spawn(move || {
            // It waits for the lock of `oid`, which the end of the first
            // commit releases after the key. Then the key is free, and the
            // equal text is dropped.
            let mut second = setting(store, 1, "same", before);
            second.store(oid, read, MAPPING).unwrap();
            let conflicts = second.vote_resolving().unwrap();
            assert_eq!(conflicts.len(), 1);
            let current = store.load(oid).unwrap().1;
            second.store(oid, current, MAPPING).unwrap();
            second.vote().unwrap();
            second.finish().unwrap()
        });
        thread::sleep(Duration::from_millis(50));
        first.finish().unwrap();
        let tid = second.join().unwrap();
        assert_eq!(deltas_of(store, tid), 0);
    });
    assert_eq!(text(&store, 1), live("same"));
}

#[test]
fn a_finish_never_fails_for_a_delta() {
    let store = Store::open_vfs(MemVfs::new()).unwrap();
    let mut txn = setting(&store, 1, "first", snapshot(&store));
    txn.vote().unwrap();
    let first = txn.finish().unwrap();
    // A damaged frame of the previous delta of the key: the finish reads
    // only its key.
    let key = testing::text_delta_key(CATALOG, INDEX, Some(1), first);
    let mut frame = testing::read_text_frame(&store, &key).unwrap();
    let last = frame.len() - 1;
    frame[last] ^= 1;
    testing::write_text_frame(&store, &key, Some(&frame)).unwrap();
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.text_clear(CATALOG, INDEX).unwrap();
    txn.text_set(CATALOG, INDEX, 1, "en", "second", snapshot(&store))
        .unwrap();
    txn.vote().unwrap();
    txn.finish().unwrap();
    assert_eq!(text(&store, 1), live("second"));
}

#[test]
fn two_writers_of_different_keys_in_one_commit_batch_both_commit() {
    let clock = CountingClock::default();
    let metered = MeteredVfs::default();
    let store = Store::open_full(metered.clone(), clock.clone(), Options::default()).unwrap();
    let before = snapshot(&store);
    let oid = new_oid(&store);
    let mut first = setting(&store, 1, "one", before);
    first.vote().unwrap();
    let mut second = setting(&store, 2, "two", before);
    second.vote().unwrap();
    store.fold().unwrap();
    thread::sleep(Duration::from_millis(50));
    let tids = thread::scope(|scope| {
        let store = &store;
        // A commit that holds the sync of the log, so the two finishes
        // wait for the next batch together.
        let held = metered.meter.hold_log();
        let leader = scope.spawn(move || commit(store, oid, Tid::ZERO, MAPPING));
        wait_until(|| metered.meter.log_gate.waiting() > 0);
        let waits = clock.waits.load(Ordering::SeqCst);
        let a = scope.spawn(move || first.finish());
        let b = scope.spawn(move || second.finish());
        wait_until(|| clock.waits.load(Ordering::SeqCst) >= waits + 2);
        drop(held);
        leader.join().unwrap();
        (a.join().unwrap().unwrap(), b.join().unwrap().unwrap())
    });
    assert_ne!(tids.0, tids.1);
    assert_eq!(text(&store, 1), live("one"));
    assert_eq!(text(&store, 2), live("two"));
    assert_eq!(deltas_of(&store, tids.0), 1);
    assert_eq!(deltas_of(&store, tids.1), 1);
}
