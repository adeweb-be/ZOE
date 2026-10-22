//! Acceptance test of the read and copy paths of the core: `load_before`,
//! `load_serial`, `history`, the transaction iterator, the current-record
//! iteration and `restore`. They are what ZODB's MVCC adapter,
//! `zodbconvert` and the backup tools call.

mod support;

use support::fresh_dir;
use zoe_core::testing::commit_as;
use zoe_core::{Oid, Store, Tid, ZoeError};

fn pickle(text: &str) -> Vec<u8> {
    let mut data = b"\x80\x03cpersistent.mapping\nPersistentMapping\nq\x00.\x80\x03X".to_vec();
    data.extend_from_slice(&(text.len() as u32).to_le_bytes());
    data.extend_from_slice(text.as_bytes());
    data.push(b'.');
    data
}

#[test]
fn load_before_returns_the_revision_and_the_next_serial() {
    let dir = fresh_dir("load-before");
    let store = Store::open(&dir).unwrap();
    let oid = store.new_oid().unwrap();
    let t1 = commit_as(
        &store,
        (b"someone", b"one", b"ext"),
        &[(oid, Tid::ZERO, pickle("v1"))],
    );
    let t2 = commit_as(
        &store,
        (b"someone", b"two", b"ext"),
        &[(oid, t1, pickle("v2"))],
    );

    let old = store.load_before(oid, t2).unwrap().unwrap();
    assert_eq!(old.data, pickle("v1"));
    assert_eq!(old.serial, t1);
    assert_eq!(old.next, Some(t2));

    let current = store.load_before(oid, Tid::MAX).unwrap().unwrap();
    assert_eq!(current.data, pickle("v2"));
    assert_eq!(current.serial, t2);
    assert_eq!(current.next, None);

    // The object did not exist before its first transaction.
    assert!(store.load_before(oid, t1).unwrap().is_none());

    let unknown = store.load_before(Oid(0xdead), Tid::MAX);
    assert!(
        matches!(unknown, Err(ZoeError::NotFound { .. })),
        "{unknown:?}"
    );
}

#[test]
fn load_serial_reads_one_revision() {
    let dir = fresh_dir("load-serial");
    let store = Store::open(&dir).unwrap();
    let oid = store.new_oid().unwrap();
    let t1 = commit_as(
        &store,
        (b"someone", b"one", b"ext"),
        &[(oid, Tid::ZERO, pickle("v1"))],
    );
    let t2 = commit_as(
        &store,
        (b"someone", b"two", b"ext"),
        &[(oid, t1, pickle("v2"))],
    );

    assert_eq!(store.load_serial(oid, t1).unwrap(), pickle("v1"));
    assert_eq!(store.load_serial(oid, t2).unwrap(), pickle("v2"));
    let missing = store.load_serial(oid, Tid(t1.0 + 1));
    assert!(
        matches!(missing, Err(ZoeError::NotFound { .. })),
        "{missing:?}"
    );
}

#[test]
fn history_lists_revisions_newest_first() {
    let dir = fresh_dir("history");
    let store = Store::open(&dir).unwrap();
    let oid = store.new_oid().unwrap();
    let t1 = commit_as(
        &store,
        (b"someone", b"one", b"ext"),
        &[(oid, Tid::ZERO, pickle("v1"))],
    );
    let t2 = commit_as(
        &store,
        (b"someone", b"two", b"ext"),
        &[(oid, t1, pickle("v2"))],
    );
    let t3 = commit_as(
        &store,
        (b"someone", b"three", b"ext"),
        &[(oid, t2, pickle("v3"))],
    );

    let all = store.history(oid, 10).unwrap();
    let tids: Vec<Tid> = all.iter().map(|r| r.tid).collect();
    assert_eq!(tids, [t3, t2, t1]);
    assert_eq!(all[0].user, b"someone");
    assert_eq!(all[0].description, b"three");
    assert_eq!(all[0].size, pickle("v3").len() as u64);

    let last_two = store.history(oid, 2).unwrap();
    assert_eq!(last_two.len(), 2);
    assert_eq!(last_two[1].tid, t2);
}

#[test]
fn the_iterator_walks_transactions_in_order_with_their_records() {
    let dir = fresh_dir("iterator");
    let store = Store::open(&dir).unwrap();
    let a = store.new_oid().unwrap();
    let b = store.new_oid().unwrap();
    let t1 = commit_as(
        &store,
        (b"someone", b"one", b"ext"),
        &[(a, Tid::ZERO, pickle("a1"))],
    );
    let t2 = commit_as(
        &store,
        (b"someone", b"two", b"ext"),
        &[(a, t1, pickle("a2")), (b, Tid::ZERO, pickle("b1"))],
    );
    let t3 = commit_as(
        &store,
        (b"someone", b"three", b"ext"),
        &[(b, t2, pickle("b2"))],
    );

    let all: Vec<_> = store
        .iterator(None, None)
        .unwrap()
        .map(|txn| txn.unwrap())
        .collect();
    let tids: Vec<Tid> = all.iter().map(|txn| txn.tid).collect();
    assert_eq!(tids, [t1, t2, t3]);
    assert_eq!(all[1].description, b"two");
    assert_eq!(all[1].extension, b"ext");
    let mut records: Vec<_> = all[1]
        .records
        .iter()
        .map(|r| (r.oid, r.tid, r.data.clone()))
        .collect();
    records.sort();
    assert_eq!(
        records,
        [(a, t2, Some(pickle("a2"))), (b, t2, Some(pickle("b1")))]
    );

    // Both bounds are inclusive, as in ZODB.
    let middle: Vec<Tid> = store
        .iterator(Some(t2), Some(t2))
        .unwrap()
        .map(|txn| txn.unwrap().tid)
        .collect();
    assert_eq!(middle, [t2]);
}

#[test]
fn record_iternext_walks_the_current_records() {
    let dir = fresh_dir("iternext");
    let store = Store::open(&dir).unwrap();
    let oids: Vec<Oid> = (0..3).map(|_| store.new_oid().unwrap()).collect();
    let writes: Vec<_> = oids
        .iter()
        .map(|&oid| (oid, Tid::ZERO, pickle(&format!("{oid:?}"))))
        .collect();
    let t1 = commit_as(&store, (b"someone", b"create", b"ext"), &writes);
    let t2 = commit_as(
        &store,
        (b"someone", b"update", b"ext"),
        &[(oids[1], t1, pickle("changed"))],
    );

    let mut seen = Vec::new();
    let mut next = None;
    loop {
        let record = store.record_iternext(next).unwrap().unwrap();
        seen.push((record.oid, record.tid, record.data));
        match record.next {
            Some(oid) => next = Some(oid),
            None => break,
        }
    }
    assert_eq!(
        seen,
        [
            (oids[0], t1, pickle(&format!("{:?}", oids[0]))),
            (oids[1], t2, pickle("changed")),
            (oids[2], t1, pickle(&format!("{:?}", oids[2]))),
        ]
    );
}

#[test]
fn restore_copies_transactions_with_their_tids() {
    let source_dir = fresh_dir("restore-source");
    let source = Store::open(&source_dir).unwrap();
    let a = source.new_oid().unwrap();
    let b = source.new_oid().unwrap();
    let t1 = commit_as(
        &source,
        (b"someone", b"one", b"ext"),
        &[(a, Tid::ZERO, pickle("a1"))],
    );
    commit_as(
        &source,
        (b"someone", b"two", b"ext"),
        &[(a, t1, pickle("a2")), (b, Tid::ZERO, pickle("b1"))],
    );

    let target_dir = fresh_dir("restore-target");
    let target = Store::open(&target_dir).unwrap();
    for txn in source.iterator(None, None).unwrap() {
        let txn = txn.unwrap();
        let mut copy = target
            .tpc_begin_restore(txn.tid, &txn.user, &txn.description, &txn.extension)
            .unwrap();
        for record in &txn.records {
            copy.restore(record.oid, record.data.as_deref()).unwrap();
        }
        copy.vote().unwrap();
        assert_eq!(copy.finish().unwrap(), txn.tid);
    }

    let copied: Vec<_> = target
        .iterator(None, None)
        .unwrap()
        .map(|t| t.unwrap())
        .collect();
    let original: Vec<_> = source
        .iterator(None, None)
        .unwrap()
        .map(|t| t.unwrap())
        .collect();
    assert_eq!(copied, original);
    assert_eq!(target.last_transaction(), source.last_transaction());

    // New oids in the target never reuse a restored oid.
    assert!(target.new_oid().unwrap() > b);
}

#[test]
fn restore_refuses_a_tid_that_is_not_after_the_last_one() {
    let dir = fresh_dir("restore-order");
    let store = Store::open(&dir).unwrap();
    let oid = store.new_oid().unwrap();
    let t1 = commit_as(
        &store,
        (b"someone", b"one", b"ext"),
        &[(oid, Tid::ZERO, pickle("v1"))],
    );
    let result = store.tpc_begin_restore(t1, b"", b"", b"");
    assert!(
        matches!(result, Err(ZoeError::Protocol(_))),
        "{:?}",
        result.err()
    );
}

#[test]
fn a_restored_deletion_removes_the_object() {
    let dir = fresh_dir("restore-delete");
    let store = Store::open(&dir).unwrap();
    let oid = store.new_oid().unwrap();
    let t1 = commit_as(
        &store,
        (b"someone", b"one", b"ext"),
        &[(oid, Tid::ZERO, pickle("v1"))],
    );

    let mut delete = store
        .tpc_begin_restore(Tid(t1.0 + 1), b"", b"undo creation", b"")
        .unwrap();
    delete.restore(oid, None).unwrap();
    delete.vote().unwrap();
    let t2 = delete.finish().unwrap();

    let loaded = store.load(oid);
    assert!(
        matches!(loaded, Err(ZoeError::NotFound { .. })),
        "{loaded:?}"
    );
    // The revision before the deletion stays readable.
    let old = store.load_before(oid, t2).unwrap().unwrap();
    assert_eq!(old.data, pickle("v1"));
}
