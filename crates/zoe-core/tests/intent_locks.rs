//! Early write-intent locks (ADR 0045): an intent owner takes exclusive
//! locks before the vote, in oid order; a transaction adopts them; the vote
//! counts them as its own and never waits for itself.

use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use zoe_core::vfs::MemVfs;
use zoe_core::{IntentLocks, LockCancel, Oid, Options, Store, SystemClock, Tid, ZoeError};

const PICKLE: &[u8] =
    b"\x80\x03cpersistent.mapping\nPersistentMapping\nq\x00.\x80\x03}q\x01X\x04\x00\x00\x00datag\x01s.";

/// A time that no test waits when the code is correct.
const LONG: Duration = Duration::from_secs(10);

fn open(lock_timeout: Duration) -> Arc<Store> {
    let options = Options {
        lock_timeout,
        ..Options::default()
    };
    Arc::new(Store::open_full(MemVfs::new(), SystemClock, options).unwrap())
}

/// Commit `n` new objects. Return their oids, in order, and the tid.
fn objects(store: &Store, n: usize) -> (Vec<Oid>, Tid) {
    let oids: Vec<Oid> = (0..n).map(|_| store.new_oid().unwrap()).collect();
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    for &oid in &oids {
        txn.store(oid, Tid::ZERO, PICKLE).unwrap();
    }
    txn.vote().unwrap();
    (oids, txn.finish().unwrap())
}

/// Commit `oids`, read at `serial`, in a transaction that adopts `intents`.
fn commit_with(store: &Arc<Store>, intents: &mut IntentLocks<Arc<Store>>, oids: &[Oid]) -> Tid {
    let mut txn = zoe_core::Txn::begin(Arc::clone(store), b"", b"", b"");
    txn.adopt_intents(intents).unwrap();
    for &oid in oids {
        let serial = store.load(oid).unwrap().1;
        txn.store(oid, serial, PICKLE).unwrap();
    }
    txn.vote().unwrap();
    txn.finish().unwrap()
}

#[test]
fn intent_locks_come_in_oid_order() {
    let store = open(LONG);
    let (oids, tid) = objects(&store, 3);
    let (low, middle, high) = (oids[0], oids[1], oids[2]);
    let mut owner = IntentLocks::new(Arc::clone(&store));
    assert_eq!(owner.lock(&[low]).unwrap(), [tid]);
    assert_eq!(owner.lock(&[middle, high]).unwrap(), [tid, tid]);
    assert_eq!(owner.oids(), [low, middle, high]);
    // A lower oid, or an oid that the owner holds, fails and takes nothing.
    owner.release();
    owner.lock(&[high]).unwrap();
    for oids in [&[low][..], &[high], &[middle]] {
        assert!(matches!(owner.lock(oids), Err(ZoeError::LockOrder { .. })));
    }
    assert_eq!(owner.oids(), [high]);
    // A call out of order in itself takes nothing.
    owner.release();
    assert!(matches!(
        owner.lock(&[high, low]),
        Err(ZoeError::LockOrder { .. })
    ));
    assert!(owner.is_empty());
    // A new oid has the serial zero.
    let fresh = store.new_oid().unwrap();
    assert_eq!(owner.lock(&[fresh]).unwrap(), [Tid::ZERO]);
    drop(owner);
    assert!(store.lock_table_is_empty());
}

#[test]
fn a_second_owner_waits_and_reads_the_commit_of_the_holder() {
    let store = open(LONG);
    let (oids, _) = objects(&store, 1);
    let oid = oids[0];
    let mut holder = IntentLocks::new(Arc::clone(&store));
    holder.lock(&[oid]).unwrap();
    let (sent, got) = mpsc::channel();
    thread::scope(|scope| {
        let waiter = scope.spawn(|| {
            let mut writer = IntentLocks::new(Arc::clone(&store));
            let serials = writer.lock(&[oid]).unwrap();
            sent.send(()).unwrap();
            (writer, serials)
        });
        assert!(got.recv_timeout(Duration::from_millis(200)).is_err());
        let first = commit_with(&store, &mut holder, &[oid]);
        let (mut writer, serials) = waiter.join().unwrap();
        // The writer reads after the grant, so it reads the commit of the
        // holder, and its commit has no conflict.
        assert_eq!(serials, [first]);
        commit_with(&store, &mut writer, &[oid]);
    });
    assert!(store.lock_table_is_empty());
}

#[test]
fn the_vote_of_the_holder_does_not_wait_for_its_own_intent_locks() {
    let store = open(Duration::from_secs(1));
    let (oids, _) = objects(&store, 3);
    let mut holder = IntentLocks::new(Arc::clone(&store));
    holder.lock(&oids).unwrap();
    let started = Instant::now();
    commit_with(&store, &mut holder, &oids);
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(holder.is_empty());
    assert!(store.lock_table_is_empty());
}

#[test]
fn a_wait_ends_at_the_lock_timeout_and_keeps_nothing() {
    let store = open(Duration::from_millis(200));
    let (oids, _) = objects(&store, 2);
    let mut holder = IntentLocks::new(Arc::clone(&store));
    holder.lock(&oids[1..]).unwrap();
    let mut writer = IntentLocks::new(Arc::clone(&store));
    let started = Instant::now();
    let error = writer.lock(&oids).unwrap_err();
    assert!(started.elapsed() >= Duration::from_millis(200));
    assert!(matches!(error, ZoeError::Conflict { oid } if oid == oids[1]));
    assert!(writer.is_empty());
    // The first oid that the call took is free again.
    let mut other = IntentLocks::new(Arc::clone(&store));
    other.lock(&oids[..1]).unwrap();
}

#[test]
fn a_cancel_ends_a_wait() {
    let store = open(LONG);
    let (oids, _) = objects(&store, 1);
    let mut holder = IntentLocks::new(Arc::clone(&store));
    holder.lock(&oids).unwrap();
    let cancel = LockCancel::new();
    thread::scope(|scope| {
        let waiter = scope.spawn(|| {
            let mut writer = IntentLocks::new(Arc::clone(&store));
            writer.set_lock_cancel(cancel.clone());
            writer.lock(&oids)
        });
        thread::sleep(Duration::from_millis(50));
        cancel.cancel();
        assert!(matches!(
            waiter.join().unwrap(),
            Err(ZoeError::Conflict { .. })
        ));
    });
}

#[test]
fn a_vote_releases_the_intent_locks_above_an_oid_that_it_waits_for() {
    let store = open(LONG);
    let (oids, serial) = objects(&store, 2);
    let (low, high) = (oids[0], oids[1]);
    // The first transaction holds the intent lock of the high oid.
    let mut first = IntentLocks::new(Arc::clone(&store));
    first.lock(&[high]).unwrap();
    let started = Instant::now();
    thread::scope(|scope| {
        // The second one votes on the low oid and the high oid: it holds
        // the low one and waits for the high one.
        let second = scope.spawn(|| {
            let mut txn = zoe_core::Txn::begin(Arc::clone(&store), b"", b"", b"");
            txn.store(low, serial, PICKLE).unwrap();
            txn.store(high, serial, PICKLE).unwrap();
            txn.vote().unwrap();
            txn.finish().unwrap()
        });
        thread::sleep(Duration::from_millis(50));
        // The first one writes both, so its vote needs the low oid. It
        // releases its intent lock of the high oid instead of a wait for
        // ever, so the second one commits.
        let mut txn = zoe_core::Txn::begin(Arc::clone(&store), b"", b"", b"");
        txn.adopt_intents(&mut first).unwrap();
        txn.store(low, serial, PICKLE).unwrap();
        txn.store(high, serial, PICKLE).unwrap();
        let voted = txn.vote();
        second.join().unwrap();
        // The first one read before the second commit: its vote fails on
        // the serial check, as without early locks.
        assert!(matches!(voted, Err(ZoeError::Conflict { .. })), "{voted:?}");
    });
    assert!(started.elapsed() < LONG);
    assert!(store.lock_table_is_empty());
}

#[test]
fn an_intent_request_waits_for_a_commit_in_flight() {
    use zoe_pickle::python::{Heap, Pickler, Value};

    fn length(value: i128) -> Vec<u8> {
        let mut heap = Heap::new();
        let class = heap.new_class("BTrees.Length", "Length");
        let mut pickler = Pickler::new(&heap);
        pickler.dump(class).unwrap();
        pickler.dump(Value::Int(value)).unwrap();
        pickler.into_bytes()
    }

    let store = open(LONG);
    let oid = store.new_oid().unwrap();
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(oid, Tid::ZERO, &length(0)).unwrap();
    txn.vote().unwrap();
    let read = txn.finish().unwrap();
    let mut first = store.tpc_begin(b"", b"", b"").unwrap();
    first.store(oid, read, &length(1)).unwrap();
    first.vote().unwrap();
    thread::scope(|scope| {
        // A vote waits for the oid; it merges onto the first commit when
        // the finish of the first one hands the lock to it.
        let second = scope.spawn(|| {
            let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
            txn.store(oid, read, &length(1)).unwrap();
            txn.vote().unwrap();
            txn.finish().unwrap()
        });
        thread::sleep(Duration::from_millis(50));
        let intent = scope.spawn(|| {
            let mut owner = IntentLocks::new(Arc::clone(&store));
            owner.lock(&[oid]).unwrap()
        });
        thread::sleep(Duration::from_millis(50));
        first.finish().unwrap();
        let second = second.join().unwrap();
        // The intent request never took the hand-off: it got the lock after
        // the last commit, and reads its serial.
        assert_eq!(intent.join().unwrap(), [second]);
    });
    assert_eq!(store.load(oid).unwrap().0, length(2));
}

#[test]
fn a_transaction_adopts_only_intent_locks_of_its_store() {
    let store = open(LONG);
    let other = open(LONG);
    let (oids, _) = objects(&other, 1);
    let mut owner = IntentLocks::new(Arc::clone(&other));
    owner.lock(&oids).unwrap();
    let mut txn = zoe_core::Txn::begin(Arc::clone(&store), b"", b"", b"");
    assert!(matches!(
        txn.adopt_intents(&mut owner),
        Err(ZoeError::Protocol(_))
    ));
    assert_eq!(owner.oids(), oids);
}
