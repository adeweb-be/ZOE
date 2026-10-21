//! Tier 1 in the vote: the merges of BTrees and `Length` (`zoe_core::merge`).
//!
//! The records are the canonical records of ZODB, built with the pickler of
//! `zoe_pickle::python`. The differential tests against ZODB's resolver are
//! the acceptance tests `test_native_merges.py` and `test_posting_dicts.py`.

mod support;

use support::Rng;
use zoe_core::testing::{commit, record};
use zoe_core::vfs::MemVfs;
use zoe_core::{
    Mode, Oid, Options, Reason, Resolved, Store, StructuralMerges, SystemClock, Tid, ZoeError,
};
use zoe_pickle::btrees::{new_bucket_state, new_tree_state};
use zoe_pickle::python::{Heap, Value};

/// Returns the record of an `IIBucket` of `items`.
fn ii_bucket(items: &[(i128, i128)]) -> Vec<u8> {
    let mut heap = Heap::new();
    let class = heap.new_class("BTrees.IIBTree", "IIBucket");
    let flat = items
        .iter()
        .flat_map(|&(k, v)| [Value::Int(k), Value::Int(v)])
        .collect();
    let state = new_bucket_state(&mut heap, flat, None);
    record(&heap, class, state)
}

/// Returns the record of an `IIBTree` that holds one bucket of `items`
/// inline.
fn ii_tree(items: &[(i128, i128)]) -> Vec<u8> {
    let mut heap = Heap::new();
    let class = heap.new_class("BTrees.IIBTree", "IIBTree");
    let flat = items
        .iter()
        .flat_map(|&(k, v)| [Value::Int(k), Value::Int(v)])
        .collect();
    let bucket = new_bucket_state(&mut heap, flat, None);
    let state = new_tree_state(&mut heap, bucket);
    record(&heap, class, state)
}

/// Returns the record of an `IITreeSet` that holds one bucket of `keys`
/// inline.
fn ii_tree_set(keys: &[i128]) -> Vec<u8> {
    let mut heap = Heap::new();
    let class = heap.new_class("BTrees.IIBTree", "IITreeSet");
    let keys = keys.iter().map(|&k| Value::Int(k)).collect();
    let bucket = new_bucket_state(&mut heap, keys, None);
    let state = new_tree_state(&mut heap, bucket);
    record(&heap, class, state)
}

/// Runs the vote of a conflict of `base`, `committed` and `new` with the
/// rules of ADR 0042 in `rule`. Returns the outcome, the outcome of the
/// rules in shadow mode, and the record of the oid after a vote that
/// passed.
fn first_key(
    rule: Mode,
    base: &[u8],
    committed: &[u8],
    new: &[u8],
) -> (Resolved, Option<Resolved>, Option<Vec<u8>>) {
    let store = open(StructuralMerges {
        posting_dicts: rule,
        ..StructuralMerges::default()
    });
    let (oid, read) = conflict(&store, base, committed);
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(oid, read, new).unwrap();
    let voted = txn.vote_resolving();
    let resolution = txn.resolutions()[0].clone();
    let stored = match voted {
        Ok(listed) => {
            assert_eq!(listed, [], "{rule:?}");
            txn.finish().unwrap();
            Some(store.load(oid).unwrap().0)
        }
        Err(ZoeError::Conflict { .. }) => None,
        Err(other) => panic!("{other}"),
    };
    (resolution.outcome, resolution.shadow, stored)
}

/// Returns the record of an `IOBucket` whose values are dicts of ints.
fn io_postings(items: &[(i128, &[(i128, i128)])]) -> Vec<u8> {
    let mut heap = Heap::new();
    let class = heap.new_class("BTrees.IOBTree", "IOBucket");
    let mut flat = Vec::new();
    for &(wid, posting) in items {
        let dict = posting
            .iter()
            .map(|&(doc, score)| (Value::Int(doc), Value::Int(score)))
            .collect();
        flat.push(Value::Int(wid));
        flat.push(heap.new_dict(dict));
    }
    let state = new_bucket_state(&mut heap, flat, None);
    record(&heap, class, state)
}

/// A value of an `IOBucket` of [`io_bucket`].
#[derive(Clone, Copy)]
enum Item {
    None,
    Int(i128),
    Dict(&'static [(i128, i128)]),
}

/// Returns the record of an `IOBucket` of `items`.
fn io_bucket(items: &[(i128, Item)]) -> Vec<u8> {
    let mut heap = Heap::new();
    let class = heap.new_class("BTrees.IOBTree", "IOBucket");
    let mut flat = Vec::new();
    for &(key, item) in items {
        flat.push(Value::Int(key));
        flat.push(match item {
            Item::None => Value::None,
            Item::Int(value) => Value::Int(value),
            Item::Dict(entries) => heap.new_dict(
                entries
                    .iter()
                    .map(|&(k, v)| (Value::Int(k), Value::Int(v)))
                    .collect(),
            ),
        });
    }
    let state = new_bucket_state(&mut heap, flat, None);
    record(&heap, class, state)
}

/// Returns the record of a `BTrees.Length.Length` of `value`.
fn length(value: i128) -> Vec<u8> {
    let mut heap = Heap::new();
    let class = heap.new_class("BTrees.Length", "Length");
    record(&heap, class, Value::Int(value))
}

fn open(merges: StructuralMerges) -> Store {
    let options = Options {
        structural_merges: merges,
        ..Options::default()
    };
    Store::open_full(MemVfs::new(), SystemClock, options).unwrap()
}

/// Commits `base`, then `committed` over it. Returns the oid and the serial
/// of `base`, which the next transaction reads.
fn conflict(store: &Store, base: &[u8], committed: &[u8]) -> (Oid, Tid) {
    let oid = store.new_oid().unwrap();
    let read = commit(store, oid, Tid::ZERO, base);
    commit(store, oid, read, committed);
    (oid, read)
}

#[test]
fn a_conflict_on_a_bucket_merges_at_the_vote() {
    let store = open(StructuralMerges::default());
    let (oid, read) = conflict(&store, &ii_bucket(&[(1, 1)]), &ii_bucket(&[(1, 1), (2, 2)]));
    let mut txn = store.tpc_begin(b"editor", b"/doc/edit", b"").unwrap();
    txn.store(oid, read, &ii_bucket(&[(1, 1), (3, 3)])).unwrap();
    assert_eq!(txn.vote_resolving().unwrap(), []);
    assert_eq!(txn.natively_resolved(), [oid]);
    let [resolution] = txn.resolutions() else {
        panic!("{:?}", txn.resolutions());
    };
    assert_eq!(resolution.class, "BTrees.IIBTree.IIBucket");
    assert_eq!(
        (resolution.mode, resolution.outcome),
        (Mode::On, Resolved::Merged)
    );
    assert!(resolution.applied);
    assert_eq!(
        (resolution.keys.clone(), resolution.changed),
        (vec!["3".to_owned()], 1)
    );
    let tid = txn.finish().unwrap();
    let merged = ii_bucket(&[(1, 1), (2, 2), (3, 3)]);
    assert_eq!(store.load(oid).unwrap(), (merged, tid));
}

#[test]
fn a_vote_that_lists_no_conflict_merges_too() {
    let store = open(StructuralMerges::default());
    let (oid, read) = conflict(&store, &length(10), &length(12));
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(oid, read, &length(7)).unwrap();
    txn.vote().unwrap();
    let tid = txn.finish().unwrap();
    // committed + new - old.
    assert_eq!(store.load(oid).unwrap(), (length(9), tid));
}

#[test]
fn a_refusal_fails_the_vote_with_a_conflict() {
    let store = open(StructuralMerges::default());
    let (oid, read) = conflict(&store, &ii_bucket(&[(1, 1)]), &ii_bucket(&[(1, 2)]));
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(oid, read, &ii_bucket(&[(1, 3)])).unwrap();
    assert!(matches!(txn.vote_resolving(), Err(ZoeError::Conflict { oid: o }) if o == oid));
    let committed = store.load(oid).unwrap().1;
    assert_eq!(txn.conflict_serials(), Some((committed, read)));
    let [resolution] = txn.resolutions() else {
        panic!("{:?}", txn.resolutions());
    };
    assert_eq!(resolution.outcome, Resolved::Refused(Reason::BTrees(1)));
    assert!(resolution.applied);
    assert!(txn.natively_resolved().is_empty());
}

#[test]
fn shadow_mode_leaves_the_conflict_to_the_caller() {
    let merges = StructuralMerges {
        mode: Mode::Shadow,
        ..StructuralMerges::default()
    };
    let store = open(merges);
    let (oid, read) = conflict(&store, &ii_bucket(&[(1, 1)]), &ii_bucket(&[(1, 1), (2, 2)]));
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(oid, read, &ii_bucket(&[(1, 1), (3, 3)])).unwrap();
    let conflicts = txn.vote_resolving().unwrap();
    assert_eq!(conflicts.len(), 1);
    assert!(txn.natively_resolved().is_empty());
    let [resolution] = txn.resolutions() else {
        panic!("{:?}", txn.resolutions());
    };
    assert_eq!(
        (resolution.mode, resolution.outcome),
        (Mode::Shadow, Resolved::Merged)
    );
    assert!(!resolution.applied);
    // The staged record is still the record of the transaction.
    assert_eq!(txn.staged(oid).unwrap().unwrap().0, read);
}

#[test]
fn the_mode_of_a_class_wins_over_the_mode_of_tier_1() {
    let merges = StructuralMerges {
        classes: [("BTrees.IIBTree.IIBucket".to_owned(), Mode::Off)].into(),
        ..StructuralMerges::default()
    };
    let store = open(merges);
    let (oid, read) = conflict(&store, &ii_bucket(&[(1, 1)]), &ii_bucket(&[(1, 1), (2, 2)]));
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(oid, read, &ii_bucket(&[(1, 1), (3, 3)])).unwrap();
    assert_eq!(txn.vote_resolving().unwrap().len(), 1);
    assert!(txn.resolutions().is_empty());
    // Length keeps the mode of tier 1.
    let (oid, read) = conflict(&store, &length(1), &length(2));
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(oid, read, &length(3)).unwrap();
    assert_eq!(txn.vote_resolving().unwrap(), []);
    assert_eq!(txn.natively_resolved(), [oid]);
}

#[test]
fn a_record_that_tier_1_does_not_merge_goes_on_as_before() {
    let store = open(StructuralMerges::default());
    let (oid, read) = conflict(
        &store,
        b"\x80\x03cm\nC\nq\x00.\x80\x03}q\x01.",
        b"\x80\x03cm\nC\nq\x00.\x80\x03}q\x01.",
    );
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(oid, read, b"\x80\x03cm\nC\nq\x00.\x80\x03}q\x01.")
        .unwrap();
    assert_eq!(txn.vote_resolving().unwrap().len(), 1);
    assert!(txn.resolutions().is_empty());
    // A record that does not decode gives no answer.
    let (oid, read) = conflict(
        &store,
        &ii_bucket(&[(1, 1)]),
        b"\x80\x03cBTrees.IIBTree\nIIBucket\nq\x00.junk",
    );
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(oid, read, &ii_bucket(&[(2, 2)])).unwrap();
    assert_eq!(txn.vote_resolving().unwrap().len(), 1);
    let [resolution] = txn.resolutions() else {
        panic!("{:?}", txn.resolutions());
    };
    assert!(matches!(resolution.outcome, Resolved::Undecided(_)));
}

#[test]
fn the_posting_rule_merges_two_changes_of_one_dict() {
    let base: &[(i128, &[(i128, i128)])] = &[(10, &[(1, 3), (2, 1)]), (20, &[(1, 2)])];
    let committed: &[(i128, &[(i128, i128)])] = &[(10, &[(1, 3), (2, 1), (3, 4)]), (20, &[(1, 2)])];
    let new: &[(i128, &[(i128, i128)])] = &[(10, &[(1, 3), (2, 1), (4, 2)]), (20, &[(1, 2)])];
    for rule in [Mode::Off, Mode::On] {
        let store = open(StructuralMerges {
            posting_dicts: rule,
            ..StructuralMerges::default()
        });
        let (oid, read) = conflict(&store, &io_postings(base), &io_postings(committed));
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.store(oid, read, &io_postings(new)).unwrap();
        let conflicts = txn.vote_resolving().unwrap();
        let [resolution] = txn.resolutions() else {
            panic!("{:?}", txn.resolutions());
        };
        if rule == Mode::Off {
            // BTrees cannot compare two dicts: the client resolves.
            assert_eq!(conflicts.len(), 1);
            assert!(matches!(resolution.outcome, Resolved::Undecided(_)));
            continue;
        }
        assert_eq!(conflicts, []);
        assert_eq!(resolution.keys, ["10"]);
        let tid = txn.finish().unwrap();
        let merged: &[(i128, &[(i128, i128)])] =
            &[(10, &[(1, 3), (2, 1), (3, 4), (4, 2)]), (20, &[(1, 2)])];
        assert_eq!(store.load(oid).unwrap(), (io_postings(merged), tid));
    }
}

#[test]
fn the_posting_rule_in_shadow_mode_logs_its_merge_and_goes_on_without_it() {
    let base: &[(i128, &[(i128, i128)])] = &[(10, &[(1, 3), (2, 1)]), (20, &[(1, 2)])];
    let committed: &[(i128, &[(i128, i128)])] = &[(10, &[(1, 3), (2, 1), (3, 4)]), (20, &[(1, 2)])];
    let new: &[(i128, &[(i128, i128)])] = &[(10, &[(1, 3), (2, 1), (4, 2)]), (20, &[(1, 2)])];
    let store = open(StructuralMerges {
        posting_dicts: Mode::Shadow,
        ..StructuralMerges::default()
    });
    let (oid, read) = conflict(&store, &io_postings(base), &io_postings(committed));
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(oid, read, &io_postings(new)).unwrap();
    // Without the rule, BTrees cannot compare two dicts: the client
    // resolves, as with the rule off.
    assert_eq!(txn.vote_resolving().unwrap().len(), 1);
    let [resolution] = txn.resolutions() else {
        panic!("{:?}", txn.resolutions());
    };
    assert_eq!(resolution.mode, Mode::On);
    assert!(matches!(resolution.outcome, Resolved::Undecided(_)));
    assert!(!resolution.applied);
    assert_eq!(resolution.shadow, Some(Resolved::Merged));
    // The line lists the keys of the merge of the rule.
    assert_eq!(resolution.keys, ["10"]);
    assert_eq!(resolution.changed, 1);
    let time = std::time::UNIX_EPOCH;
    let line = &txn.resolution_log_lines(time, None)[0];
    assert!(
        line.contains("\"shadow\":{\"outcome\":\"merged\",\"code\":null,\"reason\":null}"),
        "{line}"
    );
    // A bucket of ints compares no dict, so the rule in shadow mode
    // changes nothing there.
    let (oid, read) = conflict(&store, &ii_bucket(&[(1, 1)]), &ii_bucket(&[(1, 1), (2, 2)]));
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(oid, read, &ii_bucket(&[(1, 1), (3, 3)])).unwrap();
    assert_eq!(txn.vote_resolving().unwrap(), []);
    assert_eq!(txn.resolutions()[0].shadow, None);
}

#[test]
fn shadow_mode_merges_what_the_rule_off_merges() {
    // Buckets where a dict meets None or an int. BTrees and ZODB's
    // resolver merge them.
    let cases: [[&[(i128, Item)]; 4]; 2] = [
        [
            &[(1, Item::None), (2, Item::Int(5))],
            &[(1, Item::Dict(&[(7, 1)])), (2, Item::Int(5))],
            &[(1, Item::None), (2, Item::Int(6))],
            &[(1, Item::Dict(&[(7, 1)])), (2, Item::Int(6))],
        ],
        [
            &[(1, Item::Int(5)), (2, Item::Int(5))],
            &[(1, Item::Int(5)), (2, Item::Int(6))],
            &[(1, Item::Dict(&[(7, 1)])), (2, Item::Int(5))],
            &[(1, Item::Dict(&[(7, 1)])), (2, Item::Int(6))],
        ],
    ];
    for [base, committed, new, merged] in cases {
        for rule in [Mode::Off, Mode::Shadow, Mode::On] {
            let store = open(StructuralMerges {
                posting_dicts: rule,
                ..StructuralMerges::default()
            });
            let (oid, read) = conflict(&store, &io_bucket(base), &io_bucket(committed));
            let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
            txn.store(oid, read, &io_bucket(new)).unwrap();
            assert_eq!(txn.vote_resolving().unwrap(), [], "{rule:?}");
            assert_eq!(txn.resolutions()[0].outcome, Resolved::Merged, "{rule:?}");
            let tid = txn.finish().unwrap();
            assert_eq!(
                store.load(oid).unwrap(),
                (io_bucket(merged), tid),
                "{rule:?}"
            );
        }
    }
}

#[test]
fn the_first_key_rule_merges_a_delete_of_the_first_key_in_a_tree_of_one_bucket() {
    // The committed transaction deletes the first key, and the new one
    // adds a key.
    let cases: [[&[(i128, i128)]; 4]; 1] = [[
        &[(1, 1), (2, 2), (3, 3)],
        &[(2, 2), (3, 3)],
        &[(1, 1), (2, 2), (3, 3), (4, 4)],
        &[(2, 2), (3, 3), (4, 4)],
    ]];
    for [base, committed, new, merged] in cases {
        for rule in [Mode::Off, Mode::Shadow, Mode::On] {
            let store = open(StructuralMerges {
                posting_dicts: rule,
                ..StructuralMerges::default()
            });
            let (oid, read) = conflict(&store, &ii_tree(base), &ii_tree(committed));
            let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
            txn.store(oid, read, &ii_tree(new)).unwrap();
            let voted = txn.vote_resolving();
            let resolution = &txn.resolutions()[0];
            if rule == Mode::On {
                assert_eq!(voted.unwrap(), [], "{rule:?}");
                assert_eq!(resolution.outcome, Resolved::Merged);
                assert_eq!(resolution.shadow, None);
                let tid = txn.finish().unwrap();
                assert_eq!(store.load(oid).unwrap(), (ii_tree(merged), tid));
            } else {
                // BTrees refuses the delete of the first key: "this will
                // modify the parent node".
                assert!(matches!(voted, Err(ZoeError::Conflict { .. })), "{rule:?}");
                assert_eq!(resolution.outcome, Resolved::Refused(Reason::BTrees(13)));
                let shadow = (rule == Mode::Shadow).then_some(Resolved::Merged);
                assert_eq!(resolution.shadow, shadow, "{rule:?}");
            }
        }
    }
}

#[test]
fn the_first_key_rule_leaves_a_standalone_bucket_to_btrees() {
    // A bucket can be the child of a tree of many buckets, whose parent
    // node holds its first key.
    let store = open(StructuralMerges {
        posting_dicts: Mode::On,
        ..StructuralMerges::default()
    });
    let (oid, read) = conflict(&store, &ii_bucket(&[(1, 1), (2, 2)]), &ii_bucket(&[(2, 2)]));
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(oid, read, &ii_bucket(&[(1, 1), (2, 2), (3, 3)]))
        .unwrap();
    assert!(matches!(
        txn.vote_resolving(),
        Err(ZoeError::Conflict { .. })
    ));
    assert_eq!(
        txn.resolutions()[0].outcome,
        Resolved::Refused(Reason::BTrees(13))
    );
}

#[test]
fn the_first_key_rule_refuses_a_delete_of_the_first_key_by_the_new_transaction() {
    // In a node inside a tree, the new transaction can have moved the
    // separator of the parent node in its own write of that node, and no
    // check of the vote sees it.
    let (base, committed, new) = (
        ii_tree(&[(1, 1), (2, 2), (3, 3)]),
        ii_tree(&[(1, 1), (2, 2), (3, 3), (4, 4)]),
        ii_tree(&[(2, 2), (3, 3)]),
    );
    for rule in [Mode::Off, Mode::Shadow, Mode::On] {
        assert_eq!(
            first_key(rule, &base, &committed, &new),
            (Resolved::Refused(Reason::BTrees(13)), None, None),
            "{rule:?}"
        );
    }
}

#[test]
fn the_first_key_rule_merges_a_tree_set_too() {
    let (base, committed, new) = (
        ii_tree_set(&[1, 2, 3]),
        ii_tree_set(&[2, 3]),
        ii_tree_set(&[1, 2, 3, 4]),
    );
    assert_eq!(
        first_key(Mode::On, &base, &committed, &new),
        (Resolved::Merged, None, Some(ii_tree_set(&[2, 3, 4])))
    );
    assert_eq!(
        first_key(Mode::Off, &base, &committed, &new),
        (Resolved::Refused(Reason::BTrees(13)), None, None)
    );
}

#[test]
fn the_first_key_rule_keeps_the_other_refusals_of_btrees() {
    // Both deletes leave the bucket empty: reason 10. In shadow mode the
    // vote refuses as without the rule, and the line gives reason 10.
    let (base, committed, new) = (
        ii_tree(&[(1, 1), (2, 2)]),
        ii_tree(&[(2, 2)]),
        ii_tree(&[(1, 1)]),
    );
    assert_eq!(
        first_key(Mode::On, &base, &committed, &new),
        (Resolved::Refused(Reason::BTrees(10)), None, None)
    );
    assert_eq!(
        first_key(Mode::Shadow, &base, &committed, &new),
        (
            Resolved::Refused(Reason::BTrees(13)),
            Some(Resolved::Refused(Reason::BTrees(10))),
            None
        )
    );
    // A delete of the first key against a change of its value: reason 2.
    let (base, committed, new) = (
        ii_tree(&[(1, 1), (2, 2)]),
        ii_tree(&[(1, 5), (2, 2)]),
        ii_tree(&[(2, 2)]),
    );
    assert_eq!(
        first_key(Mode::On, &base, &committed, &new),
        (Resolved::Refused(Reason::BTrees(2)), None, None)
    );
}

#[test]
fn the_first_key_rule_leaves_an_inline_bucket_with_a_next_bucket_to_btrees() {
    // `IIBTree`s whose inline bucket names the next bucket, the oid 9. Only
    // a node inside a tree of many buckets has such a bucket, and the
    // parent node holds its first key.
    let old = b"\x80\x03cBTrees.IIBTree\nIIBTree\nq\x00.\x80\x03(K\x01K\x01K\x02K\x02tq\x01\
        C\x08\x00\x00\x00\x00\x00\x00\x00\x09q\x02cBTrees.IIBTree\nIIBucket\nq\x03\x86q\x04\
        Q\x86q\x05\x85q\x06\x85q\x07.";
    let committed = b"\x80\x03cBTrees.IIBTree\nIIBTree\nq\x00.\x80\x03K\x02K\x02\x86q\x01\
        C\x08\x00\x00\x00\x00\x00\x00\x00\x09q\x02cBTrees.IIBTree\nIIBucket\nq\x03\x86q\x04\
        Q\x86q\x05\x85q\x06\x85q\x07.";
    let new =
        b"\x80\x03cBTrees.IIBTree\nIIBTree\nq\x00.\x80\x03(K\x01K\x01K\x02K\x02K\x03K\x03tq\x01\
        C\x08\x00\x00\x00\x00\x00\x00\x00\x09q\x02cBTrees.IIBTree\nIIBucket\nq\x03\x86q\x04\
        Q\x86q\x05\x85q\x06\x85q\x07.";
    assert_eq!(
        first_key(Mode::On, old, committed, new),
        (Resolved::Refused(Reason::BTrees(13)), None, None)
    );
}

#[test]
fn a_stale_read_current_oid_fails_the_vote_after_a_merge_of_tier_1() {
    // The first-key rule rests on this: a merge counts only when every
    // read-current serial is still current.
    let store = open(StructuralMerges {
        posting_dicts: Mode::On,
        ..StructuralMerges::default()
    });
    let (bucket, read) = conflict(&store, &ii_bucket(&[(1, 1)]), &ii_bucket(&[(1, 1), (2, 2)]));
    let committed = store.load(bucket).unwrap();
    let parent = store.new_oid().unwrap();
    let seen = commit(&store, parent, Tid::ZERO, &length(1));
    commit(&store, parent, seen, &length(2));
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(bucket, read, &ii_bucket(&[(1, 1), (3, 3)]))
        .unwrap();
    txn.check_current_serial(parent, seen).unwrap();
    assert!(matches!(txn.vote_resolving(), Err(ZoeError::ReadConflict { oid }) if oid == parent));
    // Tier 1 merged the bucket, and the vote staged nothing of it.
    let [resolution] = txn.resolutions() else {
        panic!("{:?}", txn.resolutions());
    };
    assert_eq!(
        (resolution.outcome, resolution.applied),
        (Resolved::Merged, false)
    );
    assert!(txn.natively_resolved().is_empty());
    txn.abort();
    assert_eq!(store.load(bucket).unwrap(), committed);
}

#[test]
fn two_next_buckets_of_one_oid_with_two_persistent_ids_give_no_answer() {
    // `IIBucket`s whose next bucket is the oid 9, as ZODB's pickler writes
    // them. The new record names the class of the next bucket by another
    // module, so its persistent id differs.
    let old = b"\x80\x03cBTrees.IIBTree\nIIBucket\nq\x00.\x80\x03K\x01K\x01\x86q\x01C\x08\
        \x00\x00\x00\x00\x00\x00\x00\x09q\x02h\x00\x86q\x03Q\x86q\x04.";
    let committed = b"\x80\x03cBTrees.IIBTree\nIIBucket\nq\x00.\x80\x03(K\x01K\x01K\x02K\x02tq\x01\
        C\x08\x00\x00\x00\x00\x00\x00\x00\x09q\x02h\x00\x86q\x03Q\x86q\x04.";
    let alias = b"\x80\x03cBTrees.IIBTree\nIIBucket\nq\x00.\x80\x03(K\x01K\x01K\x03K\x03tq\x01\
        C\x08\x00\x00\x00\x00\x00\x00\x00\x09q\x02cBTrees._IIBTree\nIIBucket\nq\x03\x86q\x04\
        Q\x86q\x05.";
    let other = b"\x80\x03cBTrees.IIBTree\nIIBucket\nq\x00.\x80\x03(K\x01K\x01K\x03K\x03tq\x01\
        C\x08\x00\x00\x00\x00\x00\x00\x00\x0aq\x02h\x00\x86q\x03Q\x86q\x04.";
    let store = open(StructuralMerges::default());
    let (oid, read) = conflict(&store, old, committed);
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(oid, read, alias).unwrap();
    assert_eq!(txn.vote_resolving().unwrap().len(), 1);
    assert_eq!(
        txn.resolutions()[0].outcome,
        Resolved::Undecided("next buckets of one oid with two persistent ids")
    );
    // A next bucket of another oid is a bucket split, as in BTrees.
    let (oid, read) = conflict(&store, old, committed);
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(oid, read, other).unwrap();
    assert!(matches!(
        txn.vote_resolving(),
        Err(ZoeError::Conflict { .. })
    ));
    assert_eq!(
        txn.resolutions()[0].outcome,
        Resolved::Refused(Reason::BTrees(0))
    );
}

#[test]
fn the_posting_rule_refuses_a_dict_of_more_than_10_entries() {
    let nine: Vec<(i128, i128)> = (1..10).map(|doc| (doc, 1)).collect();
    let committed = [&nine[..], &[(20, 1)]].concat();
    let new = [&nine[..], &[(21, 1)]].concat();
    let store = open(StructuralMerges {
        posting_dicts: Mode::On,
        ..StructuralMerges::default()
    });
    let (oid, read) = conflict(
        &store,
        &io_postings(&[(50, &nine)]),
        &io_postings(&[(50, &committed)]),
    );
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(oid, read, &io_postings(&[(50, &new)])).unwrap();
    assert!(matches!(
        txn.vote_resolving(),
        Err(ZoeError::Conflict { .. })
    ));
    assert_eq!(
        txn.resolutions()[0].outcome,
        Resolved::Refused(Reason::PostingTooLarge)
    );
}

#[test]
fn a_line_of_the_resolution_log_names_both_transactions() {
    let store = open(StructuralMerges::default());
    let oid = store.new_oid().unwrap();
    let read = commit(&store, oid, Tid::ZERO, &ii_bucket(&[(1, 1)]));
    let mut txn = store
        .tpc_begin(b"first", b"/doc/edit \"one\"", b"")
        .unwrap();
    txn.store(oid, read, &ii_bucket(&[(1, 1), (2, 2)])).unwrap();
    txn.vote().unwrap();
    let committed = txn.finish().unwrap();
    let mut txn = store.tpc_begin(b"second", b"/doc/edit", b"").unwrap();
    txn.store(oid, read, &ii_bucket(&[(1, 1), (3, 3)])).unwrap();
    txn.vote_resolving().unwrap();
    let time = std::time::UNIX_EPOCH + std::time::Duration::from_micros(1_760_000_000_000_001);
    let [line] = &txn.resolution_log_lines(time, Some("main"))[..] else {
        panic!("one line");
    };
    let expected = format!(
        "{{\"time\":1760000000.000001,\"storage\":\"main\",\"oid\":\"0x{:016x}\",\
         \"class\":\"BTrees.IIBTree.IIBucket\",\"tier\":1,\"mode\":\"on\",\"policy\":null,\
         \"outcome\":\"merged\",\"code\":null,\"reason\":null,\"shadow\":null,\"applied\":true,\"undo\":false,\
         \"keys\":[\"3\"],\"changed\":1,\"fields\":[],\"committed\":\"0x{:016x}\",\"read\":\"0x{:016x}\",\
         \"user\":\"second\",\"description\":\"/doc/edit\",\"committed_user\":\"first\",\
         \"committed_description\":\"/doc/edit \\\"one\\\"\"}}",
        oid.0, committed.0, read.0
    );
    assert_eq!(line, &expected);
}

#[test]
fn modes_and_class_modes_parse() {
    assert_eq!("Shadow".parse::<Mode>(), Ok(Mode::Shadow));
    assert!("maybe".parse::<Mode>().is_err());
    assert_eq!(
        zoe_core::merge::class_mode("BTrees.IOBTree.IOBucket=off"),
        Ok(("BTrees.IOBTree.IOBucket".to_owned(), Mode::Off))
    );
    assert!(zoe_core::merge::class_mode("IOBucket=off").is_err());
    assert!(zoe_core::merge::class_mode("BTrees.IOBTree.IOBucket").is_err());
}

/// The records of an `IOBucket` of the metadata of a catalog: each rid maps
/// to `(title, created, modified, start, Type, created)`, with
/// `DateTime`s in three zones, `Missing.Value` and `Message`s, as ZODB 6
/// writes them with Python 3.13. The committed transaction moves
/// `modified` of rid 1 to the same instant in another zone, and gives rid
/// 5 a `start`. The new transaction gives `Type` of rid 3 another domain,
/// equal as a str, and inserts rid 4. `CATALOG_MERGED` is the record that
/// ZODB's resolver writes. `CATALOG_NEW2` changes the title of rid 1, so
/// both transactions change rid 1, and ZODB's resolver refuses.
const CATALOG_OLD: &[u8] =
    b"\x80\x03cBTrees.IOBTree\nIOBucket\nq\x00.\x80\x03(K\x01(X\x03\x00\x00\x00oneq\x01cDateTi\
    me.DateTime\nDateTime\nq\x02)\x81q\x03\x8a\x07\x00@ FHG\x06\x89X\x05\x00\x00\x00GMT+2q\
    \x04\x87q\x05bh\x03cMissing\nV\nq\x06czope.i18nmessageid.message\nMessage\nq\x07(X\x04\
    \x00\x00\x00Pageq\x08X\x05\x00\x00\x00ploneq\x09NNNNNtq\nRq\x0bh\x03tq\x0cK\x03(X\x05\
    \x00\x00\x00threeq\x0dh\x03h\x02)\x81q\x0e\x8a\x07\x00!\x16LHG\x06\x89X\x03\x00\x00\x00U\
    TCq\x0f\x87q\x10bh\x06h\x0bh\x03tq\x11K\x05(X\x04\x00\x00\x00fiveq\x12h\x03h\x03h\x06h\
    \x0bh\x03tq\x13tq\x14\x85q\x15.";
const CATALOG_COMMITTED: &[u8] =
    b"\x80\x03cBTrees.IOBTree\nIOBucket\nq\x00.\x80\x03(K\x01(X\x03\x00\x00\x00oneq\x01cDateTi\
    me.DateTime\nDateTime\nq\x02)\x81q\x03\x8a\x07\x00@ FHG\x06\x89X\x05\x00\x00\x00GMT+2q\
    \x04\x87q\x05bh\x02)\x81q\x06\x8a\x07\x00@ FHG\x06\x89X\x03\x00\x00\x00UTCq\x07\x87q\x08\
    bcMissing\nV\nq\x09czope.i18nmessageid.message\nMessage\nq\n(X\x04\x00\x00\x00Pageq\x0bX\
    \x05\x00\x00\x00ploneq\x0cNNNNNtq\x0dRq\x0eh\x03tq\x0fK\x03(X\x05\x00\x00\x00threeq\x10h\
    \x03h\x02)\x81q\x11\x8a\x07\x00!\x16LHG\x06\x89h\x07\x87q\x12bh\x09h\x0eh\x03tq\x13K\x05\
    (X\x04\x00\x00\x00fiveq\x14h\x03h\x03h\x02)\x81q\x15\x8a\x07\xc0\xa9SLIG\x06\x89X\x05\
    \x00\x00\x00GMT-5q\x16\x87q\x17bh\x0eh\x03tq\x18tq\x19\x85q\x1a.";
const CATALOG_NEW: &[u8] =
    b"\x80\x03cBTrees.IOBTree\nIOBucket\nq\x00.\x80\x03(K\x01(X\x03\x00\x00\x00oneq\x01cDateTi\
    me.DateTime\nDateTime\nq\x02)\x81q\x03\x8a\x07\x00@ FHG\x06\x89X\x05\x00\x00\x00GMT+2q\
    \x04\x87q\x05bh\x03cMissing\nV\nq\x06czope.i18nmessageid.message\nMessage\nq\x07(X\x04\
    \x00\x00\x00Pageq\x08X\x05\x00\x00\x00ploneq\x09NNNNNtq\nRq\x0bh\x03tq\x0cK\x03(X\x05\
    \x00\x00\x00threeq\x0dh\x03h\x02)\x81q\x0e\x8a\x07\x00!\x16LHG\x06\x89X\x03\x00\x00\x00U\
    TCq\x0f\x87q\x10bh\x06h\x07(X\x04\x00\x00\x00Pageq\x11X\x16\x00\x00\x00plone.app.content\
    typesq\x12NNNNNtq\x13Rq\x14h\x03tq\x15K\x04(X\x04\x00\x00\x00fourq\x16h\x03h\x03h\x06h\
    \x0bh\x03tq\x17K\x05(X\x04\x00\x00\x00fiveq\x18h\x03h\x03h\x06h\x0bh\x03tq\x19tq\x1a\x85\
    q\x1b.";
const CATALOG_MERGED: &[u8] =
    b"\x80\x03cBTrees.IOBTree\nIOBucket\nq\x00.\x80\x03(K\x01(X\x03\x00\x00\x00oneq\x01cDateTi\
    me.DateTime\nDateTime\nq\x02)\x81q\x03\x8a\x07\x00@ FHG\x06\x89X\x05\x00\x00\x00GMT+2q\
    \x04\x87q\x05bh\x02)\x81q\x06\x8a\x07\x00@ FHG\x06\x89X\x03\x00\x00\x00UTCq\x07\x87q\x08\
    bcMissing\nV\nq\x09czope.i18nmessageid.message\nMessage\nq\n(X\x04\x00\x00\x00Pageq\x0bX\
    \x05\x00\x00\x00ploneq\x0cNNNNNtq\x0dRq\x0eh\x03tq\x0fK\x03(X\x05\x00\x00\x00threeq\x10h\
    \x02)\x81q\x11\x8a\x07\x00@ FHG\x06\x89h\x04\x87q\x12bh\x02)\x81q\x13\x8a\x07\x00!\x16LH\
    G\x06\x89h\x07\x87q\x14bh\x09h\n(X\x04\x00\x00\x00Pageq\x15X\x16\x00\x00\x00plone.app.co\
    ntenttypesq\x16NNNNNtq\x17Rq\x18h\x11tq\x19K\x04(X\x04\x00\x00\x00fourq\x1ah\x11h\x11h\
    \x09h\n(X\x04\x00\x00\x00Pageq\x1bX\x05\x00\x00\x00ploneq\x1cNNNNNtq\x1dRq\x1eh\x11tq\
    \x1fK\x05(X\x04\x00\x00\x00fiveq h\x03h\x03h\x02)\x81q!\x8a\x07\xc0\xa9SLIG\x06\x89X\x05\
    \x00\x00\x00GMT-5q\"\x87q#bh\x0eh\x03tq$tq%\x85q&.";
const CATALOG_NEW2: &[u8] =
    b"\x80\x03cBTrees.IOBTree\nIOBucket\nq\x00.\x80\x03(K\x01(X\x04\x00\x00\x00one!q\x01cDateT\
    ime.DateTime\nDateTime\nq\x02)\x81q\x03\x8a\x07\x00@ FHG\x06\x89X\x05\x00\x00\x00GMT+2q\
    \x04\x87q\x05bh\x03cMissing\nV\nq\x06czope.i18nmessageid.message\nMessage\nq\x07(X\x04\
    \x00\x00\x00Pageq\x08X\x05\x00\x00\x00ploneq\x09NNNNNtq\nRq\x0bh\x03tq\x0cK\x03(X\x05\
    \x00\x00\x00threeq\x0dh\x03h\x02)\x81q\x0e\x8a\x07\x00!\x16LHG\x06\x89X\x03\x00\x00\x00U\
    TCq\x0f\x87q\x10bh\x06h\x0bh\x03tq\x11K\x05(X\x04\x00\x00\x00fiveq\x12h\x03h\x03h\x06h\
    \x0bh\x03tq\x13tq\x14\x85q\x15.";

#[test]
fn metadata_with_datetimes_messages_and_missing_values_merge_as_zodb_merges_them() {
    let store = open(StructuralMerges::default());
    let (oid, read) = conflict(&store, CATALOG_OLD, CATALOG_COMMITTED);
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(oid, read, CATALOG_NEW).unwrap();
    assert_eq!(txn.vote_resolving().unwrap(), []);
    assert_eq!(txn.natively_resolved(), [oid]);
    let tid = txn.finish().unwrap();
    assert_eq!(store.load(oid).unwrap(), (CATALOG_MERGED.to_vec(), tid));
}

#[test]
fn a_datetime_in_another_zone_is_a_change_of_its_value() {
    let store = open(StructuralMerges::default());
    let (oid, read) = conflict(&store, CATALOG_OLD, CATALOG_COMMITTED);
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(oid, read, CATALOG_NEW2).unwrap();
    assert!(matches!(
        txn.vote_resolving(),
        Err(ZoeError::Conflict { .. })
    ));
    let [resolution] = txn.resolutions() else {
        panic!("{:?}", txn.resolutions());
    };
    assert_eq!(resolution.outcome, Resolved::Refused(Reason::BTrees(1)));
}

#[test]
fn a_datetime_of_a_zone_out_of_the_table_gives_no_answer() {
    // `_zmap` of DateTime maps `utc` to `UTC`: the name in the state is
    // not the name that `__setstate__` gives.
    let lower = |record: &[u8]| -> Vec<u8> {
        let at = record
            .windows(7)
            .position(|window| window == b"\x03\x00\x00\x00UTC")
            .unwrap();
        let mut record = record.to_vec();
        record[at + 4..at + 7].copy_from_slice(b"utc");
        record
    };
    let store = open(StructuralMerges::default());
    let (oid, read) = conflict(&store, CATALOG_OLD, &lower(CATALOG_COMMITTED));
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(oid, read, CATALOG_NEW).unwrap();
    assert_eq!(txn.vote_resolving().unwrap().len(), 1);
    let [resolution] = txn.resolutions() else {
        panic!("{:?}", txn.resolutions());
    };
    assert!(matches!(resolution.outcome, Resolved::Undecided(_)));
}

/// Changes one to four bytes of `record`, or cuts it.
fn damaged(rng: &mut Rng, record: &[u8]) -> Vec<u8> {
    let mut out = record.to_vec();
    if rng.below(8) == 0 {
        out.truncate(rng.below(out.len().max(1)));
        return out;
    }
    for _ in 0..=rng.below(4) {
        if !out.is_empty() {
            let at = rng.below(out.len());
            out[at] = rng.next() as u8;
        }
    }
    out
}

#[test]
fn damaged_records_never_panic_and_a_merge_always_decodes() {
    let base: &[(i128, &[(i128, i128)])] = &[(10, &[(1, 3), (2, 1)]), (20, &[(1, 2)])];
    let committed: &[(i128, &[(i128, i128)])] = &[(10, &[(1, 3), (2, 1), (3, 4)]), (20, &[(1, 2)])];
    let new: &[(i128, &[(i128, i128)])] = &[(10, &[(1, 3), (2, 1), (4, 2)]), (20, &[(1, 2)])];
    let triples = [
        [
            ii_bucket(&[(1, 1)]),
            ii_bucket(&[(1, 1), (2, 2)]),
            ii_bucket(&[(1, 1), (3, 3)]),
        ],
        [length(10), length(12), length(7)],
        [io_postings(base), io_postings(committed), io_postings(new)],
    ];
    let mut rng = Rng::new(0x9e37_79b9_7f4a_7c15);
    let mut merged = 0;
    for round in 0..20_000 {
        let triple = &triples[round % triples.len()];
        let mut records = triple.clone();
        let which = rng.below(4);
        if which < 3 {
            records[which] = damaged(&mut rng, &triple[which]);
        }
        let posting = [Mode::Off, Mode::Shadow, Mode::On][rng.below(3)];
        let Some(merge) = zoe_core::merge::tier1(&records[0], &records[1], &records[2], posting)
        else {
            continue;
        };
        if let zoe_core::merge::Outcome::Merged(record) = merge.outcome {
            zoe_pickle::decode_record(&record).expect("a merged record decodes");
            merged += 1;
        }
    }
    assert!(merged > 1_000, "{merged}");
}
