//! Every read at any tid gives the answer of a model of the commits:
//! `load`, `load_before` with its next serial, `load_serial`, `history`,
//! the transaction iterator and `record_iternext`. The commits create,
//! change and delete objects, in transactions that write one oid or many,
//! and some oids are written by no transaction. The reads run before and
//! after a reopen.

use std::collections::BTreeMap;

use zoe_core::vfs::MemVfs;
use zoe_core::{Oid, Store, Tid, ZoeError};

/// The number of transactions.
const TXNS: u64 = 60;

/// The oids that the commits use, from 1.
const OIDS: u64 = 12;

/// The pickle of `oid` in the transaction `n`. A size that changes with
/// `n` gives the history different sizes.
fn pickle(oid: Oid, n: u64) -> Vec<u8> {
    format!("oid {} txn {n} {}", oid.0, "x".repeat((n % 7) as usize)).into_bytes()
}

/// A deterministic choice of the oids that transaction `n` writes, and
/// whether it deletes them.
fn writes(n: u64) -> (Vec<Oid>, bool) {
    let oids = (1..=OIDS)
        .filter(|oid| (oid * 7 + n * 3) % 5 == 0 || (n % 10 == 0 && oid % 2 == 0))
        .map(Oid)
        .collect();
    (oids, n % 9 == 4)
}

/// The records of the model: for each oid, each tid with its data, `None`
/// for a deletion.
type Model = BTreeMap<Oid, BTreeMap<Tid, Option<Vec<u8>>>>;

fn build(store: &Store) -> (Model, Vec<Tid>) {
    let mut model = Model::new();
    let mut tids = Vec::new();
    for _ in 1..=OIDS {
        store.new_oid().unwrap();
    }
    for n in 1..=TXNS {
        let (oids, delete) = writes(n);
        let current = |oid: Oid| -> Tid {
            model
                .get(&oid)
                .and_then(|versions| versions.last_key_value())
                .map_or(Tid::ZERO, |(&tid, _)| tid)
        };
        let deleted = |oid: Oid| -> bool {
            model
                .get(&oid)
                .and_then(|v| v.last_key_value())
                .is_some_and(|(_, d)| d.is_none())
        };
        let mut txn = store
            .tpc_begin(b"user", format!("txn {n}").as_bytes(), b"")
            .unwrap();
        let mut written = Vec::new();
        for &oid in &oids {
            if delete && current(oid) != Tid::ZERO && !deleted(oid) {
                txn.delete(oid, current(oid)).unwrap();
                written.push((oid, None));
            } else if !delete {
                txn.store(oid, current(oid), &pickle(oid, n)).unwrap();
                written.push((oid, Some(pickle(oid, n))));
            }
        }
        txn.vote().unwrap();
        let tid = txn.finish().unwrap();
        tids.push(tid);
        for (oid, data) in written {
            model.entry(oid).or_default().insert(tid, data);
        }
    }
    (model, tids)
}

/// The tids to read at: each tid, the tid after it, and the ends.
fn probes(tids: &[Tid]) -> Vec<Tid> {
    let mut probes = vec![Tid(1), Tid::MAX];
    for tid in tids {
        probes.extend([*tid, Tid(tid.0 + 1)]);
    }
    probes
}

fn check(store: &Store, model: &Model, tids: &[Tid]) {
    for oid in (0..=OIDS + 2).map(Oid) {
        let versions = model.get(&oid);
        // `load`.
        let current = versions.and_then(|v| v.last_key_value());
        match (store.load(oid), current) {
            (Ok((data, serial)), Some((&tid, Some(expected)))) => {
                assert_eq!((data, serial), (expected.clone(), tid), "load {oid:?}")
            }
            (Err(ZoeError::NotFound { .. }), None | Some((_, None))) => {}
            (found, expected) => panic!("load {oid:?}: {found:?}, expected {expected:?}"),
        }
        for &at in &probes(tids) {
            // `load_before`: the newest record before `at`, and the next
            // serial.
            let before = versions.and_then(|v| v.range(..at).next_back());
            let next = versions.and_then(|v| v.range(at..).next().map(|(&tid, _)| tid));
            match (store.load_before(oid, at), before) {
                (Ok(Some(record)), Some((&tid, Some(data)))) => {
                    assert_eq!(
                        (record.data, record.serial, record.next),
                        (data.clone(), tid, next),
                        "load_before {oid:?} {at:?}"
                    );
                }
                (Ok(None), None) if versions.is_some() => {}
                (Err(ZoeError::NotFound { .. }), None) if versions.is_none() => {}
                (Err(ZoeError::NotFound { .. }), Some((_, None))) => {}
                (found, expected) => {
                    panic!("load_before {oid:?} {at:?}: {found:?}, expected {expected:?}")
                }
            }
            // `load_serial`.
            let exact = versions.and_then(|v| v.get(&at));
            match (store.load_serial(oid, at), exact) {
                (Ok(data), Some(Some(expected))) => {
                    assert_eq!(&data, expected, "load_serial {oid:?} {at:?}")
                }
                (Err(ZoeError::NotFound { .. }), None | Some(None)) => {}
                (found, expected) => {
                    panic!("load_serial {oid:?} {at:?}: {found:?}, expected {expected:?}")
                }
            }
        }
        // `history`, newest first, whole and cut.
        for size in [1, 3, 1000] {
            match (store.history(oid, size), versions) {
                (Ok(entries), Some(versions)) => {
                    let found: Vec<_> = entries.iter().map(|e| (e.tid, e.size)).collect();
                    let expected: Vec<_> = versions
                        .iter()
                        .rev()
                        .take(size)
                        .map(|(&tid, data)| (tid, data.as_ref().map_or(0, |d| d.len() as u64)))
                        .collect();
                    assert_eq!(found, expected, "history {oid:?} {size}");
                }
                (Err(ZoeError::NotFound { .. }), None) => {}
                (found, _) => panic!("history {oid:?}: {found:?}"),
            }
        }
    }
    // The iterator gives each transaction with the oids it wrote.
    let found: Vec<(Tid, Vec<Oid>)> = store
        .iterator(None, None)
        .unwrap()
        .map(|txn| {
            let txn = txn.unwrap();
            (txn.tid, txn.records.iter().map(|r| r.oid).collect())
        })
        .collect();
    let expected: Vec<(Tid, Vec<Oid>)> = tids
        .iter()
        .map(|&tid| {
            let oids = model
                .iter()
                .filter(|(_, v)| v.contains_key(&tid))
                .map(|(&oid, _)| oid)
                .collect();
            (tid, oids)
        })
        .collect();
    assert_eq!(found, expected, "the iterator");
    // `record_iternext` walks the oids with a current record.
    let mut walked = Vec::new();
    let mut next = None;
    while let Some(current) = store.record_iternext(next).unwrap() {
        walked.push(current.oid);
        match current.next {
            Some(oid) => next = Some(oid),
            None => break,
        }
    }
    let live: Vec<Oid> = model
        .iter()
        .filter(|(_, v)| v.last_key_value().is_some_and(|(_, d)| d.is_some()))
        .map(|(&oid, _)| oid)
        .collect();
    assert_eq!(walked, live, "record_iternext");
}

#[test]
fn every_read_at_any_tid_gives_the_answer_of_the_model() {
    let vfs = MemVfs::new();
    let store = Store::open_vfs(vfs.clone()).unwrap();
    let (model, tids) = build(&store);
    check(&store, &model, &tids);
    assert_eq!(
        store.object_count(),
        Some(model.len() as u64),
        "the object count"
    );
    drop(store);
    let store = Store::open_vfs(vfs).unwrap();
    check(&store, &model, &tids);
}
