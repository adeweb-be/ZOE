//! Acceptance test of sequences, the in-process primitive. A sequence is
//! a named counter of a store. `next` returns values that are unique and
//! strictly increasing per store and per name, also across a reopen and a
//! crash. The store reserves values in durable blocks of 100, so a crash
//! leaves a gap of at most 100 values. An allocation takes no per-OID lock
//! and is not part of a transaction.

mod support;

use std::collections::BTreeSet;
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use support::fresh_dir;
use zoe_core::vfs::MemVfs;
use zoe_core::{Oid, Store, Tid};

/// The largest number of values that a crash can skip.
const MAX_GAP: u64 = 100;

const PICKLE: &[u8] =
    b"\x80\x03cpersistent.mapping\nPersistentMapping\nq\x00.\x80\x03}q\x01X\x04\x00\x00\x00datag\x01s.";

fn next(store: &Store, name: &str) -> u64 {
    store.sequence(name).next().unwrap()
}

#[test]
fn values_start_at_one_and_increase_by_one() {
    let store = Store::open_vfs(MemVfs::new()).unwrap();
    // 250 values cross two block boundaries.
    let values: Vec<u64> = (0..250).map(|_| next(&store, "intids")).collect();
    assert_eq!(values, (1..=250).collect::<Vec<_>>());
}

#[test]
fn each_name_has_its_own_sequence() {
    let store = Store::open_vfs(MemVfs::new()).unwrap();
    assert_eq!(next(&store, "intids"), 1);
    assert_eq!(next(&store, "intids"), 2);
    assert_eq!(next(&store, "lexicon"), 1);
    assert_eq!(next(&store, "intids"), 3);
    assert_eq!(next(&store, "lexicon"), 2);
    assert_eq!(next(&store, "a name with spaces / and slashes"), 1);
}

#[test]
fn values_keep_increasing_after_a_reopen() {
    let dir = fresh_dir("reopen");
    let last = {
        let store = Store::open(&dir).unwrap();
        let values: Vec<u64> = (0..5).map(|_| next(&store, "intids")).collect();
        values[4]
    };
    assert_eq!(last, 5);

    let store = Store::open(&dir).unwrap();
    let after: Vec<u64> = (0..150).map(|_| next(&store, "intids")).collect();
    assert!(after[0] > last, "{} came again after a reopen", after[0]);
    assert!(
        after[0] - last - 1 <= MAX_GAP,
        "a gap of {}",
        after[0] - last - 1
    );
    assert!(after.windows(2).all(|w| w[1] == w[0] + 1), "{after:?}");
}

#[test]
fn values_are_unique_across_threads_and_commits() {
    const THREADS: u64 = 8;
    const PER_THREAD: u64 = 300;
    let store = Store::open_vfs(MemVfs::new()).unwrap();

    let per_thread: Vec<Vec<u64>> = thread::scope(|scope| {
        // Two writers commit while the other threads allocate.
        for w in 0..2u64 {
            let store = &store;
            scope.spawn(move || {
                for i in 0..50u64 {
                    let oid = Oid(1_000 + w * 100 + i);
                    let mut txn = store.tpc_begin(b"", b"writer", b"").unwrap();
                    txn.store(oid, Tid::ZERO, PICKLE).unwrap();
                    txn.vote().unwrap();
                    txn.finish().unwrap();
                }
            });
        }
        let allocators: Vec<_> = (0..THREADS)
            .map(|_| {
                let store = &store;
                scope.spawn(move || {
                    (0..PER_THREAD)
                        .map(|_| next(store, "intids"))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        allocators.into_iter().map(|h| h.join().unwrap()).collect()
    });

    for values in &per_thread {
        assert!(values.windows(2).all(|w| w[0] < w[1]), "{values:?}");
    }
    let all: BTreeSet<u64> = per_thread.iter().flatten().copied().collect();
    // No value came twice, and without a crash no value was skipped.
    assert_eq!(all, (1..=THREADS * PER_THREAD).collect::<BTreeSet<_>>());
}

#[test]
fn an_allocation_does_not_wait_for_the_locks_of_a_transaction() {
    let store = Arc::new(Store::open_vfs(MemVfs::new()).unwrap());
    let oid = store.new_oid().unwrap();

    // The transaction holds the lock of `oid` from its vote.
    let mut txn = store.tpc_begin(b"", b"holds its locks", b"").unwrap();
    txn.store(oid, Tid::ZERO, PICKLE).unwrap();
    txn.vote().unwrap();

    // Another thread allocates 150 values, past a block boundary. A
    // thread that waits forever does not stop the test from failing.
    let (sent, received) = mpsc::channel();
    let allocator = Arc::clone(&store);
    thread::spawn(move || {
        let values: Vec<u64> = (0..150).map(|_| next(&allocator, "intids")).collect();
        let _ = sent.send(values);
    });
    let values = received
        .recv_timeout(Duration::from_secs(10))
        .expect("the allocation waited for the transaction");
    assert_eq!(values, (1..=150).collect::<Vec<_>>());

    // The same thread allocates between the vote and the finish.
    assert_eq!(next(&store, "intids"), 151);
    txn.finish().unwrap();
}

#[test]
fn an_aborted_transaction_does_not_give_its_values_back() {
    let store = Store::open_vfs(MemVfs::new()).unwrap();
    let oid = store.new_oid().unwrap();

    let mut txn = store.tpc_begin(b"", b"aborted", b"").unwrap();
    let value = next(&store, "intids");
    txn.store(oid, Tid::ZERO, PICKLE).unwrap();
    txn.vote().unwrap();
    txn.abort();

    assert_eq!(value, 1);
    assert_eq!(next(&store, "intids"), 2);
}

#[test]
fn a_crash_never_gives_a_value_twice() {
    // 120 values: the first block, then one value past it.
    const VALUES: u64 = 120;
    let vfs = MemVfs::new();
    {
        let store = Store::open_vfs(vfs.clone()).unwrap();
        for expected in 1..=VALUES {
            assert_eq!(next(&store, "intids"), expected);
            vfs.mark(&format!("returned {expected}"));
        }
    }

    let images = vfs.crash_images();
    assert!(images.len() > 1, "the simulation produced crash images");
    for image in &images {
        let returned = (1..=VALUES)
            .rev()
            .find(|v| image.is_after(&format!("returned {v}")))
            .unwrap_or(0);
        let store = Store::open_vfs(image.vfs())
            .unwrap_or_else(|e| panic!("{image:?} does not open without repair: {e}"));
        let first = next(&store, "intids");
        assert!(
            first > returned,
            "{image:?}: {first} came again after {returned}"
        );
        assert!(
            first - returned - 1 <= MAX_GAP,
            "{image:?}: a gap of {} after {returned}",
            first - returned - 1
        );
    }
}
