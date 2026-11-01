//! A reader that runs during commits sees either none or all of each
//! transaction (N2). Each transaction updates the same objects and adds
//! new ones, so the finish counts both old and new oids.

use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use zoe_core::testing::commit_txn;
use zoe_core::vfs::MemVfs;
use zoe_core::{Oid, Store, Tid, Txn};

/// The objects that each transaction updates.
const UPDATED: usize = 50;

/// The objects that each transaction adds.
const ADDED: usize = 10;

/// The transactions after the first one.
const TXNS: usize = 100;

/// The pickle of `oid` in the transaction `version`.
fn pickle(oid: Oid, version: usize) -> Vec<u8> {
    format!("object {} version {version}", oid.0).into_bytes()
}

/// Begin a transaction that writes `updated` at their serial `serial` and
/// `ADDED` new objects.
fn writing<'s>(store: &'s Store, updated: &[Oid], serial: Tid, version: usize) -> Txn<&'s Store> {
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    for &oid in updated {
        txn.store(oid, serial, &pickle(oid, version)).unwrap();
    }
    for _ in 0..ADDED {
        let oid = store.new_oid().unwrap();
        txn.store(oid, Tid::ZERO, &pickle(oid, version)).unwrap();
    }
    txn
}

#[test]
fn a_reader_during_commits_sees_none_or_all_of_each_transaction() {
    let store = Store::open_vfs(MemVfs::new()).unwrap();
    let updated: Vec<Oid> = (0..UPDATED).map(|_| store.new_oid().unwrap()).collect();
    let first = commit_txn(writing(&store, &[], Tid::ZERO, 0));
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    for &oid in &updated {
        txn.store(oid, Tid::ZERO, &pickle(oid, 0)).unwrap();
    }
    txn.vote().unwrap();
    let created = txn.finish().unwrap();
    assert!(created > first);

    let done = AtomicBool::new(false);
    let reads = thread::scope(|scope| {
        scope.spawn(|| {
            let mut serial = created;
            for version in 1..=TXNS {
                serial = commit_txn(writing(&store, &updated, serial, version));
            }
            done.store(true, Ordering::SeqCst);
        });
        let mut reads = 0;
        while !done.load(Ordering::SeqCst) || reads == 0 {
            let last = store.last_transaction();
            // Each updated object has its record of the last transaction.
            for &oid in &updated {
                let record = store.load_before(oid, Tid(last.0 + 1)).unwrap().unwrap();
                assert_eq!(record.serial, last, "{oid:?} at {last:?}");
            }
            // The last transaction gives all its records.
            let txns: Vec<_> = store
                .iterator(Some(last), Some(last))
                .unwrap()
                .map(Result::unwrap)
                .collect();
            assert_eq!(txns.len(), 1, "at {last:?}");
            let written = if last == created {
                UPDATED
            } else {
                UPDATED + ADDED
            };
            assert_eq!(txns[0].records.len(), written, "at {last:?}");
            reads += 1;
        }
        reads
    });
    assert!(reads > 0);
    assert_eq!(
        store.object_count(),
        Some((UPDATED + ADDED * (TXNS + 1)) as u64)
    );
}
