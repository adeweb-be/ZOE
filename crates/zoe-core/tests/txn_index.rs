//! Acceptance test of the transaction index: a `(tid, oid)` key space that
//! gives the oids of each transaction.
//!
//! Every commit and every restore writes one index key per record, in the
//! backend write transaction of its records. The transaction iterator
//! reads the oids of a transaction from the index. So a read of one
//! transaction, or of a short tid range, costs the size of the result, and
//! not a scan of every record key of the store. A record without its index
//! key, or an index key without its record, is `CorruptTxn`. A crash at
//! any point leaves the records and the index consistent.
//!
//! The cost is the number of bytes that the database reads from its file
//! in the simulated filesystem, as `MemVfs::bytes_read` counts them. Each
//! measure opens the store with no page cache, so every page that a read
//! needs comes from the file.
//!
//! The test uses three hidden test hooks of `Store`: `open_vfs_uncached`
//! opens a store with no page cache, `remove_index_key_for_test` removes
//! one index key and keeps its record, and `remove_record_for_test`
//! removes one record and keeps its index key.

use zoe_core::vfs::MemVfs;
use zoe_core::{Oid, RecordInfo, Store, Tid, TxnInfo, ZoeError};

/// The number of records in one filler transaction.
const FILLER_RECORDS: usize = 250;

/// The number of filler transactions in the small store. The large store
/// has `GROWTH` times more.
const FILLER_TXNS: usize = 8;

/// How many times more filler records the large store has.
const GROWTH: usize = 8;

/// The number of records of a measured transaction.
const RESULT_RECORDS: usize = 8;

/// The pickle of a record of `oid`, version `version`. Its size is near
/// the size of a small content object.
fn pickle(oid: Oid, version: u64) -> Vec<u8> {
    let mut data = b"\x80\x03cpersistent.mapping\nPersistentMapping\nq\x00.\x80\x03X".to_vec();
    let text = format!("{:0>200}-{version}", oid.0);
    data.extend_from_slice(&(text.len() as u32).to_le_bytes());
    data.extend_from_slice(text.as_bytes());
    data.push(b'.');
    data
}

/// Return `n` new oids.
fn new_oids(store: &Store, n: usize) -> Vec<Oid> {
    (0..n).map(|_| store.new_oid().unwrap()).collect()
}

/// Commit a normal transaction that stores a first version of each oid of
/// `oids`. Return its tid.
fn commit_new(store: &Store, oids: &[Oid]) -> Tid {
    let mut txn = store.tpc_begin(b"someone", b"created", b"ext").unwrap();
    for &oid in oids {
        txn.store(oid, Tid::ZERO, &pickle(oid, 0)).unwrap();
    }
    txn.vote().unwrap();
    txn.finish().unwrap()
}

/// Commit a restore transaction at the tid after the last tid. It copies
/// `writes`: `None` copies a deletion. Return its tid.
fn restore(store: &Store, writes: &[(Oid, Option<Vec<u8>>)]) -> Tid {
    let tid = Tid(store.last_transaction().0 + 1);
    let mut txn = store
        .tpc_begin_restore(tid, b"copier", b"restored", b"")
        .unwrap();
    for (oid, data) in writes {
        txn.restore(*oid, data.as_deref()).unwrap();
    }
    txn.vote().unwrap();
    assert_eq!(txn.finish().unwrap(), tid);
    tid
}

/// Commit `txns` restore transactions, each with `FILLER_RECORDS` new
/// objects.
fn fill(store: &Store, txns: usize) {
    for _ in 0..txns {
        let writes: Vec<_> = new_oids(store, FILLER_RECORDS)
            .into_iter()
            .map(|oid| (oid, Some(pickle(oid, 0))))
            .collect();
        restore(store, &writes);
    }
}

/// The records that a transaction at `tid` wrote, as the iterator gives
/// them. `writes` gives each oid and its pickle; `None` is a deletion.
fn records(tid: Tid, writes: &[(Oid, Option<Vec<u8>>)]) -> Vec<RecordInfo> {
    let mut records: Vec<_> = writes
        .iter()
        .map(|(oid, data)| RecordInfo {
            oid: *oid,
            tid,
            data: data.clone(),
        })
        .collect();
    records.sort_by_key(|r| r.oid);
    records
}

/// The records of a new object for each oid of `oids`, written at `tid`.
fn created(tid: Tid, oids: &[Oid]) -> Vec<RecordInfo> {
    let writes: Vec<_> = oids.iter().map(|&o| (o, Some(pickle(o, 0)))).collect();
    records(tid, &writes)
}

/// Open the closed store in `vfs` with no page cache. Read the
/// transactions from `start` to `stop`. Return the bytes that the read
/// cost and the transactions.
fn measured_read(vfs: &MemVfs, start: Tid, stop: Tid) -> (u64, Vec<TxnInfo>) {
    // The open loads no page for the read. Each page that the read needs
    // comes from the file.
    let store = Store::open_vfs_uncached(vfs.clone()).unwrap();
    let before = vfs.bytes_read();
    let txns: Vec<TxnInfo> = store
        .iterator(Some(start), Some(stop))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let cost = vfs.bytes_read() - before;
    (cost, txns)
}

/// The transactions that the cost test reads.
struct Measured {
    /// A normal commit amid the filler transactions.
    commit: (Tid, Vec<Oid>),
    /// A restore transaction after the filler transactions.
    restored: (Tid, Vec<Oid>),
    /// Three small transactions in a row, amid the filler transactions.
    range: Vec<(Tid, Vec<Oid>)>,
}

/// Write the small store in `vfs`.
fn small_store(vfs: &MemVfs) -> Measured {
    let store = Store::open_vfs(vfs.clone()).unwrap();
    fill(&store, FILLER_TXNS / 2);
    let oids = new_oids(&store, RESULT_RECORDS);
    let commit = (commit_new(&store, &oids), oids);
    let range = (0..3)
        .map(|_| {
            let oids = new_oids(&store, 3);
            (commit_new(&store, &oids), oids)
        })
        .collect();
    fill(&store, FILLER_TXNS / 2);
    let oids = new_oids(&store, RESULT_RECORDS);
    let writes: Vec<_> = oids.iter().map(|&o| (o, Some(pickle(o, 0)))).collect();
    let restored = (restore(&store, &writes), oids);
    Measured {
        commit,
        restored,
        range,
    }
}

/// Check that the transactions are the measured ones, with their records.
fn assert_measured(found: &[TxnInfo], expected: &[(Tid, Vec<Oid>)]) {
    let found: Vec<_> = found.iter().map(|t| (t.tid, t.records.clone())).collect();
    let expected: Vec<_> = expected
        .iter()
        .map(|(tid, oids)| (*tid, created(*tid, oids)))
        .collect();
    assert_eq!(found, expected);
}

#[test]
fn a_read_of_one_transaction_or_a_range_does_not_grow_with_the_store() {
    let vfs = MemVfs::new();
    let m = small_store(&vfs);
    let (commit_tid, restore_tid) = (m.commit.0, m.restored.0);
    let (first, last) = (m.range[0].0, m.range[2].0);

    let (commit_small, txns) = measured_read(&vfs, commit_tid, commit_tid);
    assert_measured(&txns, std::slice::from_ref(&m.commit));
    let (restore_small, txns) = measured_read(&vfs, restore_tid, restore_tid);
    assert_measured(&txns, std::slice::from_ref(&m.restored));
    let (range_small, txns) = measured_read(&vfs, first, last);
    assert_measured(&txns, &m.range);

    // The store grows. The measured transactions do not change.
    {
        let store = Store::open_vfs(vfs.clone()).unwrap();
        fill(&store, FILLER_TXNS * (GROWTH - 1));
    }
    let filler_bytes = (FILLER_TXNS * GROWTH * FILLER_RECORDS * pickle(Oid(0), 0).len()) as u64;

    let (commit_large, txns) = measured_read(&vfs, commit_tid, commit_tid);
    assert_measured(&txns, std::slice::from_ref(&m.commit));
    let (restore_large, txns) = measured_read(&vfs, restore_tid, restore_tid);
    assert_measured(&txns, std::slice::from_ref(&m.restored));
    let (range_large, txns) = measured_read(&vfs, first, last);
    assert_measured(&txns, &m.range);

    for (what, small, large) in [
        ("one normal commit", commit_small, commit_large),
        ("one restore transaction", restore_small, restore_large),
        ("a range of three transactions", range_small, range_large),
    ] {
        assert!(small > 0, "{what}: the read of the store is not counted");
        // A scan of every record key grows with the store. A read of the
        // index and of the records of the result does not. A deeper tree
        // can add a page to each lookup, so allow twice the cost.
        assert!(
            large <= 2 * small,
            "{what}: the read costs {small} bytes in the small store and \
             {large} bytes in a store with {GROWTH} times more records"
        );
        // The read costs a small part of the pickles in the store.
        assert!(
            large * 4 < filler_bytes,
            "{what}: the read costs {large} bytes in a store with \
             {filler_bytes} bytes of pickles"
        );
    }
}

#[test]
fn every_commit_and_every_restore_indexes_each_of_its_records() {
    let vfs = MemVfs::new();
    let store = Store::open_vfs(vfs.clone()).unwrap();
    let oids = new_oids(&store, 4);

    // A normal commit of new objects.
    let created_tid = commit_new(&store, &oids[..3]);
    // A normal commit with no record.
    let empty = {
        let mut txn = store.tpc_begin(b"someone", b"nothing", b"").unwrap();
        txn.vote().unwrap();
        txn.finish().unwrap()
    };
    // A normal commit of a new version of an existing object.
    let changed = {
        let mut txn = store.tpc_begin(b"someone", b"changed", b"").unwrap();
        txn.store(oids[1], created_tid, &pickle(oids[1], 1))
            .unwrap();
        txn.vote().unwrap();
        txn.finish().unwrap()
    };
    // A restore of a new version, a new object and a deletion.
    let restored_writes = vec![
        (oids[0], Some(pickle(oids[0], 1))),
        (oids[2], None),
        (oids[3], Some(pickle(oids[3], 0))),
    ];
    let restored = restore(&store, &restored_writes);
    // A restore with no record.
    let empty_restore = restore(&store, &[]);

    let expected = [
        (created_tid, created(created_tid, &oids[..3])),
        (empty, Vec::new()),
        (
            changed,
            records(changed, &[(oids[1], Some(pickle(oids[1], 1)))]),
        ),
        (restored, records(restored, &restored_writes)),
        (empty_restore, Vec::new()),
    ];
    let check = |store: &Store| {
        let all: Vec<_> = store
            .iterator(None, None)
            .unwrap()
            .map(|txn| {
                let txn = txn.unwrap();
                (txn.tid, txn.records)
            })
            .collect();
        assert_eq!(all, expected);
        for (tid, records) in &expected {
            let one: Vec<_> = store
                .iterator(Some(*tid), Some(*tid))
                .unwrap()
                .map(|txn| txn.unwrap().records)
                .collect();
            assert_eq!(one, std::slice::from_ref(records), "{tid:?}");
        }
    };
    check(&store);
    drop(store);
    check(&Store::open_vfs(vfs).unwrap());
}

/// The tid of each transaction that the iterator gives, or the tid of its
/// `CorruptTxn` error.
fn outcomes(store: &Store, start: Option<Tid>, stop: Option<Tid>) -> Vec<Result<Tid, Tid>> {
    store
        .iterator(start, stop)
        .unwrap()
        .map(|txn| match txn {
            Ok(txn) => Ok(txn.tid),
            Err(ZoeError::CorruptTxn { tid }) => Err(tid),
            Err(error) => panic!("{error:?}"),
        })
        .collect()
}

/// A store with three transactions. The middle one writes three objects.
/// Return the tids and the oids of the middle transaction.
fn three_transactions(store: &Store) -> ([Tid; 3], Vec<Oid>) {
    let before = commit_new(store, &new_oids(store, 2));
    let oids = new_oids(store, 3);
    let middle = commit_new(store, &oids);
    let after = commit_new(store, &new_oids(store, 2));
    ([before, middle, after], oids)
}

/// Check that only the middle transaction of `tids` is corrupt, in the
/// open store and after a reopen.
fn assert_only_middle_is_corrupt(store: Store, vfs: MemVfs, tids: [Tid; 3]) {
    let [before, middle, after] = tids;
    let check = |store: &Store| {
        assert_eq!(
            outcomes(store, None, None),
            [Ok(before), Err(middle), Ok(after)]
        );
        assert_eq!(outcomes(store, Some(middle), Some(middle)), [Err(middle)]);
        assert_eq!(outcomes(store, Some(after), None), [Ok(after)]);
    };
    check(&store);
    drop(store);
    check(&Store::open_vfs(vfs).unwrap());
}

#[test]
fn a_record_without_its_index_key_is_corrupt() {
    let vfs = MemVfs::new();
    let store = Store::open_vfs(vfs.clone()).unwrap();
    let (tids, oids) = three_transactions(&store);

    // The test hook removes the index key and keeps the record.
    store.remove_index_key_for_test(tids[1], oids[1]).unwrap();
    assert_eq!(store.load(oids[1]).unwrap(), (pickle(oids[1], 0), tids[1]));
    assert_only_middle_is_corrupt(store, vfs, tids);
}

#[test]
fn an_index_key_without_its_record_is_corrupt() {
    let vfs = MemVfs::new();
    let store = Store::open_vfs(vfs.clone()).unwrap();
    let (tids, oids) = three_transactions(&store);

    // The test hook removes the record and keeps its index key.
    store.remove_record_for_test(oids[1], tids[1]).unwrap();
    assert!(matches!(
        store.load(oids[1]),
        Err(ZoeError::NotFound { .. })
    ));
    assert_only_middle_is_corrupt(store, vfs, tids);
}

/// Return a filesystem that holds a closed, empty store. Its record
/// starts after the creation of the store, so its crash images cover only
/// what comes next.
fn created_store() -> MemVfs {
    let created = MemVfs::new();
    drop(Store::open_vfs(created.clone()).unwrap());
    // The last image holds the store at the end of the record.
    created.crash_images().pop().unwrap().vfs()
}

#[test]
fn the_records_and_the_index_survive_every_crash_image() {
    let vfs = created_store();
    let store = Store::open_vfs(vfs.clone()).unwrap();
    let oids = new_oids(&store, 3);
    let tid = commit_new(&store, &oids);
    vfs.mark("acknowledged");
    let expected = [(tid, created(tid, &oids))];

    // A crash does not close the store, so the images end here.
    let images = vfs.crash_images();
    drop(store);
    assert!(images.len() > 1, "the simulation produced crash images");
    for image in &images {
        let store = Store::open_vfs(image.vfs())
            .unwrap_or_else(|e| panic!("{image:?} does not open without repair: {e}"));
        let read = |start, stop| -> Vec<_> {
            store
                .iterator(start, stop)
                .unwrap()
                .map(|txn| {
                    let txn = txn.unwrap_or_else(|e| panic!("{image:?}: {e}"));
                    (txn.tid, txn.records)
                })
                .collect()
        };
        // The commit is whole or absent. It is there after the
        // acknowledgement.
        let found = read(None, None);
        if found.is_empty() {
            assert!(
                !image.is_after("acknowledged"),
                "{image:?} lost an acknowledged commit"
            );
        } else {
            assert_eq!(found, expected, "{image:?}");
            assert_eq!(read(Some(tid), Some(tid)), expected, "{image:?}");
        }
        // The records are there exactly when the index gives them.
        for &oid in &oids {
            match store.load(oid) {
                Ok(_) => assert!(
                    !found.is_empty(),
                    "{image:?}: the iterator does not give the record of {oid:?}"
                ),
                Err(ZoeError::NotFound { .. }) => assert!(
                    found.is_empty(),
                    "{image:?}: the iterator gives {oid:?}, which has no record"
                ),
                Err(e) => panic!("{image:?}: {e}"),
            }
        }
    }
}
