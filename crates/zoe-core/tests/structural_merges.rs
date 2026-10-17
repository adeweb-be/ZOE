//! Tier 1 in the vote: the merges of BTrees and `Length` (`zoe_core::merge`).
//!
//! The records are the canonical records of ZODB, built with the pickler of
//! `zoe_pickle::python`. The differential tests against ZODB's resolver are
//! the acceptance tests `test_native_merges.py` and `test_posting_dicts.py`.

use zoe_core::vfs::MemVfs;
use zoe_core::{
    Mode, Oid, Options, Reason, Resolved, Store, StructuralMerges, SystemClock, Tid, ZoeError,
};
use zoe_pickle::btrees::new_bucket_state;
use zoe_pickle::python::{Heap, Pickler, Value};

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

/// Returns the record of a `BTrees.Length.Length` of `value`.
fn length(value: i128) -> Vec<u8> {
    let mut heap = Heap::new();
    let class = heap.new_class("BTrees.Length", "Length");
    record(&heap, class, Value::Int(value))
}

fn record(heap: &Heap, class: Value, state: Value) -> Vec<u8> {
    let mut pickler = Pickler::new(heap);
    pickler.dump(class).unwrap();
    pickler.dump(state).unwrap();
    pickler.into_bytes()
}

fn open(merges: StructuralMerges) -> Store {
    let options = Options {
        structural_merges: merges,
        ..Options::default()
    };
    Store::open_full(MemVfs::new(), SystemClock, options).unwrap()
}

fn commit(store: &Store, oid: Oid, serial: Tid, data: &[u8]) -> Tid {
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(oid, serial, data).unwrap();
    txn.vote().unwrap();
    txn.finish().unwrap()
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
fn the_posting_rule_in_shadow_mode_logs_its_merge_and_refuses() {
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
    assert!(matches!(
        txn.vote_resolving(),
        Err(ZoeError::Conflict { .. })
    ));
    let [resolution] = txn.resolutions() else {
        panic!("{:?}", txn.resolutions());
    };
    assert_eq!(resolution.mode, Mode::On);
    assert_eq!(resolution.outcome, Resolved::Refused(Reason::BTrees(1)));
    assert_eq!(resolution.shadow, Some(Resolved::Merged));
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
         \"class\":\"BTrees.IIBTree.IIBucket\",\"tier\":1,\"mode\":\"on\",\
         \"outcome\":\"merged\",\"code\":null,\"reason\":null,\"shadow\":null,\"applied\":true,\
         \"keys\":[\"3\"],\"changed\":1,\"committed\":\"0x{:016x}\",\"read\":\"0x{:016x}\",\
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

/// A small generator of the xorshift family, so that a failure repeats.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
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
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
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
