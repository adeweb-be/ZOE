//! Acceptance test of the per-OID lock manager: exclusive locks for
//! written oids, shared locks for read-current oids, a wait queue per oid
//! in arrival order, and a lock-wait timeout that becomes a conflict.

use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use zoe_core::{Oid, Options, Store, Tid, ZoeError};

const PICKLE: &[u8] =
    b"\x80\x03cpersistent.mapping\nPersistentMapping\nq\x00.\x80\x03}q\x01X\x04\x00\x00\x00datag\x01s.";

fn fresh_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("zoe-locks-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn open(name: &str, lock_timeout: Duration) -> Store {
    let options = Options {
        lock_timeout,
        ..Options::default()
    };
    Store::open_with(fresh_dir(name), options).unwrap()
}

/// Commit `oids` as new objects and return the tid of the commit.
fn create(store: &Store, oids: &[Oid]) -> Tid {
    let mut txn = store.tpc_begin(b"", b"create", b"").unwrap();
    for &oid in oids {
        txn.store(oid, Tid::ZERO, PICKLE).unwrap();
    }
    txn.vote().unwrap();
    txn.finish().unwrap()
}

fn new_oids(store: &Store, n: usize) -> Vec<Oid> {
    (0..n).map(|_| store.new_oid().unwrap()).collect()
}

#[test]
fn votes_on_disjoint_oids_do_not_wait() {
    let store = open("disjoint", Duration::from_secs(10));
    let oids = new_oids(&store, 2);
    let serial = create(&store, &oids);

    let mut first = store.tpc_begin(b"", b"first", b"").unwrap();
    first.store(oids[0], serial, PICKLE).unwrap();
    first.vote().unwrap();

    let start = Instant::now();
    let mut second = store.tpc_begin(b"", b"second", b"").unwrap();
    second.store(oids[1], serial, PICKLE).unwrap();
    second.vote().unwrap();
    assert!(start.elapsed() < Duration::from_secs(1));

    first.finish().unwrap();
    second.finish().unwrap();
}

#[test]
fn a_waiter_gets_the_lock_when_the_holder_aborts() {
    let store = open("abort", Duration::from_secs(10));
    let oids = new_oids(&store, 1);
    let serial = create(&store, &oids);

    let mut holder = store.tpc_begin(b"", b"holder", b"").unwrap();
    holder.store(oids[0], serial, PICKLE).unwrap();
    holder.vote().unwrap();

    let (voted, waiting) = mpsc::channel();
    thread::scope(|scope| {
        scope.spawn(|| {
            let mut waiter = store.tpc_begin(b"", b"waiter", b"").unwrap();
            waiter.store(oids[0], serial, PICKLE).unwrap();
            voted.send(waiter.vote()).unwrap();
            waiter.finish().unwrap();
        });
        thread::sleep(Duration::from_millis(200));
        assert!(waiting.try_recv().is_err(), "the waiter did not wait");
        holder.abort();
        let result = waiting.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(result.is_ok(), "{result:?}");
    });
}

#[test]
fn a_waiter_sees_the_conflict_as_soon_as_the_holder_finishes() {
    let store = open("finish", Duration::from_secs(10));
    let oids = new_oids(&store, 1);
    let serial = create(&store, &oids);

    let mut holder = store.tpc_begin(b"", b"holder", b"").unwrap();
    holder.store(oids[0], serial, PICKLE).unwrap();
    holder.vote().unwrap();

    let (voted, waiting) = mpsc::channel();
    thread::scope(|scope| {
        scope.spawn(|| {
            let mut waiter = store.tpc_begin(b"", b"waiter", b"").unwrap();
            waiter.store(oids[0], serial, PICKLE).unwrap();
            voted.send(waiter.vote()).unwrap();
        });
        thread::sleep(Duration::from_millis(200));
        holder.finish().unwrap();
        let result = waiting.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(
            matches!(result, Err(ZoeError::Conflict { oid }) if oid == oids[0]),
            "{result:?}"
        );
    });
}

#[test]
fn waiters_get_the_lock_in_arrival_order() {
    let store = open("order", Duration::from_secs(10));
    let oids = new_oids(&store, 1);
    let serial = create(&store, &oids);

    let mut holder = store.tpc_begin(b"", b"holder", b"").unwrap();
    holder.store(oids[0], serial, PICKLE).unwrap();
    holder.vote().unwrap();

    thread::scope(|scope| {
        let (granted, order) = mpsc::channel();
        for name in ["first", "second", "third"] {
            let granted = granted.clone();
            let store = &store;
            let oid = oids[0];
            scope.spawn(move || {
                let mut waiter = store.tpc_begin(b"", name.as_bytes(), b"").unwrap();
                waiter.store(oid, serial, PICKLE).unwrap();
                // Each waiter votes without a conflict, then drops its
                // transaction. The drop aborts it and the next waiter
                // gets the lock.
                let result = waiter.vote();
                granted.send((name, result.is_ok())).unwrap();
            });
            thread::sleep(Duration::from_millis(100));
        }
        holder.abort();
        let names: Vec<_> = (0..3)
            .map(|_| order.recv_timeout(Duration::from_secs(5)).unwrap().0)
            .collect();
        assert_eq!(names, ["first", "second", "third"]);
    });
}

#[test]
fn a_wait_past_the_timeout_is_a_conflict() {
    let timeout = Duration::from_millis(300);
    let store = open("timeout", timeout);
    let oids = new_oids(&store, 1);
    let serial = create(&store, &oids);

    let mut holder = store.tpc_begin(b"", b"holder", b"").unwrap();
    holder.store(oids[0], serial, PICKLE).unwrap();
    holder.vote().unwrap();

    thread::scope(|scope| {
        scope
            .spawn(|| {
                let mut waiter = store.tpc_begin(b"", b"waiter", b"").unwrap();
                waiter.store(oids[0], serial, PICKLE).unwrap();
                let start = Instant::now();
                let result = waiter.vote();
                let waited = start.elapsed();
                assert!(
                    matches!(result, Err(ZoeError::Conflict { oid }) if oid == oids[0]),
                    "{result:?}"
                );
                assert!(waited >= timeout, "{waited:?}");
                assert!(waited < timeout * 10, "{waited:?}");
            })
            .join()
            .unwrap();
    });
    holder.finish().unwrap();
}

#[test]
fn read_current_oids_share_their_lock() {
    let store = open("shared", Duration::from_secs(10));
    let oids = new_oids(&store, 3);
    let serial = create(&store, &oids);

    // Two transactions read oid 0 as current and write other oids.
    let mut first = store.tpc_begin(b"", b"first", b"").unwrap();
    first.check_current_serial(oids[0], serial).unwrap();
    first.store(oids[1], serial, PICKLE).unwrap();
    first.vote().unwrap();

    let start = Instant::now();
    let mut second = store.tpc_begin(b"", b"second", b"").unwrap();
    second.check_current_serial(oids[0], serial).unwrap();
    second.store(oids[2], serial, PICKLE).unwrap();
    second.vote().unwrap();
    assert!(start.elapsed() < Duration::from_secs(1));

    // A writer of oid 0 waits for both readers.
    let (voted, waiting) = mpsc::channel();
    thread::scope(|scope| {
        scope.spawn(|| {
            let mut writer = store.tpc_begin(b"", b"writer", b"").unwrap();
            writer.store(oids[0], serial, PICKLE).unwrap();
            voted.send(writer.vote()).unwrap();
            writer.finish().unwrap();
        });
        thread::sleep(Duration::from_millis(200));
        first.finish().unwrap();
        thread::sleep(Duration::from_millis(200));
        assert!(
            waiting.try_recv().is_err(),
            "the writer did not wait for the second reader"
        );
        second.finish().unwrap();
        let result = waiting.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(result.is_ok(), "{result:?}");
    });
}

#[test]
fn a_stale_read_current_serial_is_a_read_conflict() {
    let store = open("read-conflict", Duration::from_secs(10));
    let oids = new_oids(&store, 2);
    let old = create(&store, &oids);

    let mut change = store.tpc_begin(b"", b"change", b"").unwrap();
    change.store(oids[0], old, PICKLE).unwrap();
    change.vote().unwrap();
    change.finish().unwrap();

    let mut txn = store.tpc_begin(b"", b"stale read", b"").unwrap();
    txn.check_current_serial(oids[0], old).unwrap();
    txn.store(oids[1], old, PICKLE).unwrap();
    let result = txn.vote();
    assert!(
        matches!(result, Err(ZoeError::ReadConflict { oid }) if oid == oids[0]),
        "{result:?}"
    );

    // The failed vote released every lock.
    let mut next = store.tpc_begin(b"", b"next", b"").unwrap();
    next.store(oids[1], old, PICKLE).unwrap();
    next.vote().unwrap();
    next.finish().unwrap();
}
