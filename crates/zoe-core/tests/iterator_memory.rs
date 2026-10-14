//! Acceptance test of a transaction iterator whose memory does not grow
//! with its range (F10, N6, N15).
//!
//! `Store::iterator(start, stop)` reads the transaction rows and the index
//! keys of its range a slice at a time, as it goes. Its memory is bounded
//! by one transaction and a slice of rows, whatever the length of the
//! range: `getInvalidations` replay, the iterator of the wire, `zoe backup`
//! and the converters walk ranges of millions of transactions. It gives
//! each transaction of the range in tid order, with the errors that the
//! documentation of `Store::iterator` gives.
//!
//! The test counts the bytes of the heap with its own global allocator,
//! so it runs alone in its binary.

// A global allocator needs unsafe code.
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use zoe_core::vfs::MemVfs;
use zoe_core::{Oid, Store, Tid};

/// The allocator of the system, with a count of the bytes in use and of
/// the most bytes in use since the last reset.
struct Counting;

static IN_USE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            let now = IN_USE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(now, Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        IN_USE.fetch_sub(layout.size(), Ordering::Relaxed);
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Start a new peak. Return the bytes in use now.
fn reset() -> usize {
    let now = IN_USE.load(Ordering::Relaxed);
    PEAK.store(now, Ordering::Relaxed);
    now
}

fn peak() -> usize {
    PEAK.load(Ordering::Relaxed)
}

const TRANSACTIONS: usize = 1000;
const OIDS_PER_TRANSACTION: u64 = 20;
const DESCRIPTION: usize = 8 << 10;
/// The most heap that the start of an iteration may add. The range holds
/// more than 8 MiB of rows and 20,000 index keys.
const BOUND: usize = 1 << 20;

#[test]
fn the_memory_of_an_iteration_does_not_grow_with_its_range() {
    let store = Store::open_vfs(MemVfs::new()).unwrap();
    let description = vec![b'd'; DESCRIPTION];
    let mut tids = Vec::new();
    for t in 0..TRANSACTIONS {
        let mut txn = store.tpc_begin(b"user", &description, b"").unwrap();
        for n in 0..OIDS_PER_TRANSACTION {
            let oid = Oid(t as u64 * OIDS_PER_TRANSACTION + n + 1);
            txn.store(oid, Tid(0), b"\x80\x03N.\x80\x03N.").unwrap();
        }
        txn.vote().unwrap();
        tids.push(txn.finish().unwrap());
    }

    let before = reset();
    let mut iterator = store.iterator(None, None).unwrap();
    let first = iterator.next().unwrap().unwrap();
    let grown = peak() - before;
    assert_eq!(first.tid, tids[0]);
    assert_eq!(first.records.len(), OIDS_PER_TRANSACTION as usize);
    assert!(
        grown < BOUND,
        "the start of the iteration took {grown} bytes of heap for {TRANSACTIONS} transactions"
    );

    // The middle of the iteration holds no more than its start.
    for _ in 0..TRANSACTIONS / 2 {
        iterator.next().unwrap().unwrap();
    }
    let middle = reset();
    let item = iterator.next().unwrap().unwrap();
    assert!(peak() - middle < BOUND);
    assert_eq!(item.tid, tids[TRANSACTIONS / 2 + 1]);

    // The iterator still gives every transaction, in order, and stops at
    // the last one.
    let rest: Vec<Tid> = iterator.map(|item| item.unwrap().tid).collect();
    assert_eq!(rest, tids[TRANSACTIONS / 2 + 2..]);

    // A bounded range gives the transactions of the range.
    let some: Vec<Tid> = store
        .iterator(Some(tids[10]), Some(tids[19]))
        .unwrap()
        .map(|item| item.unwrap().tid)
        .collect();
    assert_eq!(some, tids[10..20]);
}
