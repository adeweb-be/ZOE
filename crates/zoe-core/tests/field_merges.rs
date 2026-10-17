//! Tier 2 in the vote: the merges of fields that the merge policy names
//! (ADR 0046), the policy under the root, and the check of a merge of
//! shadow mode against the resolution of the caller.
//!
//! The records are built with the pickler of `zoe_pickle::python`. The
//! comparison with a Python reference of each strategy is the acceptance
//! test `test_field_merges.py`.

use zoe_core::vfs::MemVfs;
use zoe_core::{Mode, Oid, Options, Reason, Resolved, Store, SystemClock, Tid, ZoeError};
use zoe_pickle::python::{Heap, Instance, Made, Pickler, Value};

/// A value of a field of a test record.
#[derive(Clone)]
enum V {
    Int(i128),
    Str(&'static str),
    List(Vec<V>),
    Tuple(Vec<V>),
    /// A `DateTime.DateTime` of this many microseconds since the epoch.
    Date(i128),
}

fn value(heap: &mut Heap, v: &V) -> Value {
    match v {
        V::Int(int) => Value::Int(*int),
        V::Str(text) => heap.new_str(text),
        V::List(items) => {
            let items = items.iter().map(|item| value(heap, item)).collect();
            heap.new_list(items)
        }
        V::Tuple(items) => {
            let items = items.iter().map(|item| value(heap, item)).collect();
            heap.new_tuple(items)
        }
        V::Date(micros) => {
            let class = heap.new_class("DateTime.DateTime", "DateTime");
            let args = heap.new_tuple(Vec::new());
            let tz = heap.new_str("UTC");
            let state = heap.new_tuple(vec![Value::Int(*micros), Value::Bool(false), tz]);
            heap.new_instance(Instance {
                made: Made::New,
                class,
                args,
                state: Some(state),
            })
        }
    }
}

/// Returns the record of an object of `class` whose state holds `fields`.
fn record(class: (&str, &str), fields: &[(&str, V)]) -> Vec<u8> {
    let mut heap = Heap::new();
    let class = heap.new_class(class.0, class.1);
    let items = fields
        .iter()
        .map(|(name, v)| (heap.new_str(name), value(&mut heap, v)))
        .collect();
    let state = heap.new_dict(items);
    let mut pickler = Pickler::new(&heap);
    pickler.dump(class).unwrap();
    pickler.dump(state).unwrap();
    pickler.into_bytes()
}

const DOCUMENT: (&str, &str) = ("m", "Document");

fn doc(fields: &[(&str, V)]) -> Vec<u8> {
    record(DOCUMENT, fields)
}

/// The root of ZODB, a `PersistentMapping` that names the policy at
/// `policy`, as ZODB writes it.
fn root(policy: Oid) -> Vec<u8> {
    [
        &b"\x80\x03cpersistent.mapping\nPersistentMapping\nq\x00.\x80\x03}q\x01X\x04\x00\x00\x00\
           dataq\x02}q\x03X\x10\x00\x00\x00zoe.merge_policyq\x04C\x08"[..],
        &policy.0.to_be_bytes(),
        b"q\x05czoe.policy\nMergePolicy\nq\x06\x86q\x07Qss.",
    ]
    .concat()
}

/// An entry of the policy: its keys and values, as text.
type Entry = &'static [(&'static str, Setting)];

#[derive(Clone, Copy)]
enum Setting {
    Text(&'static str),
    Fields(&'static [(&'static str, &'static str)]),
    Coupled(&'static [&'static [&'static str]]),
}

/// Returns the record of a policy of `version` with an entry of each class.
fn policy(version: i128, classes: &[(&str, Entry)]) -> Vec<u8> {
    let mut heap = Heap::new();
    let class = heap.new_class("zoe.policy", "MergePolicy");
    let mut entries = Vec::new();
    for (name, entry) in classes {
        let mut items = Vec::new();
        for (key, setting) in *entry {
            let setting = match setting {
                Setting::Text(text) => heap.new_str(text),
                Setting::Fields(fields) => {
                    let items = fields
                        .iter()
                        .map(|(field, strategy)| (heap.new_str(field), heap.new_str(strategy)))
                        .collect();
                    heap.new_dict(items)
                }
                Setting::Coupled(groups) => {
                    let groups = groups
                        .iter()
                        .map(|group| {
                            let fields = group.iter().map(|field| heap.new_str(field)).collect();
                            heap.new_list(fields)
                        })
                        .collect();
                    heap.new_list(groups)
                }
            };
            items.push((heap.new_str(key), setting));
        }
        let entry = heap.new_dict(items);
        entries.push((heap.new_str(name), entry));
    }
    let classes = heap.new_dict(entries);
    let items = vec![
        (heap.new_str("version"), Value::Int(version)),
        (heap.new_str("classes"), classes),
    ];
    let state = heap.new_dict(items);
    let mut pickler = Pickler::new(&heap);
    pickler.dump(class).unwrap();
    pickler.dump(state).unwrap();
    pickler.into_bytes()
}

fn open() -> Store {
    Store::open_full(MemVfs::new(), SystemClock, Options::default()).unwrap()
}

fn commit(store: &Store, oid: Oid, serial: Tid, data: &[u8]) -> Tid {
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(oid, serial, data).unwrap();
    txn.vote().unwrap();
    txn.finish().unwrap()
}

fn new_oid(store: &Store) -> Oid {
    loop {
        let oid = store.new_oid().unwrap();
        if oid != Oid(0) {
            return oid;
        }
    }
}

/// Opens a store whose policy has an entry of `m.Document`. Returns the
/// store, the oid of the policy and its serial.
fn with_policy(entry: Entry) -> (Store, Oid, Tid) {
    let store = open();
    let oid = new_oid(&store);
    let serial = commit(&store, oid, Tid::ZERO, &policy(1, &[("m.Document", entry)]));
    commit(&store, Oid(0), Tid::ZERO, &root(oid));
    (store, oid, serial)
}

/// Commits `base`, then `committed` over it. Returns the oid and the serial
/// of `base`, which the next transaction reads.
fn conflict(store: &Store, base: &[u8], committed: &[u8]) -> (Oid, Tid) {
    let oid = new_oid(store);
    let read = commit(store, oid, Tid::ZERO, base);
    commit(store, oid, read, committed);
    (oid, read)
}

fn base() -> Vec<(&'static str, V)> {
    vec![
        ("title", V::Str("A")),
        ("description", V::Str("one")),
        ("count", V::Int(1)),
        ("modification_date", V::Date(1_000_000)),
    ]
}

/// Returns `base` with the fields of `changes` set or added.
fn changed(changes: &[(&'static str, V)]) -> Vec<(&'static str, V)> {
    let mut fields = base();
    for (name, v) in changes {
        match fields.iter_mut().find(|(field, _)| field == name) {
            Some(field) => field.1 = v.clone(),
            None => fields.push((name, v.clone())),
        }
    }
    fields
}

const ON: Entry = &[("tier2", Setting::Text("on"))];

#[test]
fn two_changes_of_different_fields_merge_at_the_vote() {
    let (store, _, _) = with_policy(ON);
    let (oid, read) = conflict(
        &store,
        &doc(&base()),
        &doc(&changed(&[("title", V::Str("B"))])),
    );
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(oid, read, &doc(&changed(&[("description", V::Str("two"))])))
        .unwrap();
    assert_eq!(txn.vote_resolving().unwrap(), []);
    let [resolution] = txn.resolutions() else {
        panic!("{:?}", txn.resolutions());
    };
    assert_eq!(
        (resolution.tier, resolution.mode, resolution.outcome),
        (2, Mode::On, Resolved::Merged)
    );
    assert_eq!(resolution.policy, Some(1));
    assert_eq!(resolution.keys, ["description"]);
    assert!(resolution.applied);
    assert_eq!(txn.natively_resolved(), [oid]);
    let tid = txn.finish().unwrap();
    let merged = doc(&changed(&[
        ("title", V::Str("B")),
        ("description", V::Str("two")),
    ]));
    assert_eq!(store.load(oid).unwrap(), (merged, tid));
}

#[test]
fn a_field_changed_differently_on_both_sides_is_refused() {
    let (store, _, _) = with_policy(ON);
    let (oid, read) = conflict(
        &store,
        &doc(&base()),
        &doc(&changed(&[("title", V::Str("B"))])),
    );
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(oid, read, &doc(&changed(&[("title", V::Str("C"))])))
        .unwrap();
    assert!(matches!(txn.vote_resolving(), Err(ZoeError::Conflict { oid: o }) if o == oid));
    assert_eq!(
        txn.resolutions()[0].outcome,
        Resolved::Refused(Reason::FieldChanged)
    );
}

#[test]
fn the_same_change_on_both_sides_merges() {
    let (store, _, _) = with_policy(ON);
    let both = doc(&changed(&[("title", V::Str("B"))]));
    let (oid, read) = conflict(&store, &doc(&base()), &both);
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(oid, read, &both).unwrap();
    assert_eq!(txn.vote_resolving().unwrap(), []);
    let tid = txn.finish().unwrap();
    assert_eq!(store.load(oid).unwrap(), (both, tid));
}

#[test]
fn coupled_fields_stay_strict() {
    const COUPLED: Entry = &[
        ("tier2", Setting::Text("on")),
        ("coupled", Setting::Coupled(&[&["title", "description"]])),
    ];
    let (store, _, _) = with_policy(COUPLED);
    let (oid, read) = conflict(
        &store,
        &doc(&base()),
        &doc(&changed(&[("title", V::Str("B"))])),
    );
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(oid, read, &doc(&changed(&[("description", V::Str("two"))])))
        .unwrap();
    assert!(txn.vote_resolving().is_err());
    assert_eq!(
        txn.resolutions()[0].outcome,
        Resolved::Refused(Reason::Coupled)
    );
}

#[test]
fn a_class_without_an_entry_never_merges() {
    let (store, _, _) = with_policy(ON);
    let other = |fields: &[(&str, V)]| record(("m", "Other"), fields);
    let (oid, read) = conflict(
        &store,
        &other(&base()),
        &other(&changed(&[("title", V::Str("B"))])),
    );
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(
        oid,
        read,
        &other(&changed(&[("description", V::Str("two"))])),
    )
    .unwrap();
    // The conflict goes to the caller, as without a policy.
    assert_eq!(txn.vote_resolving().unwrap().len(), 1);
    assert!(txn.resolutions().is_empty());
}

#[test]
fn a_delete_against_a_change_is_refused() {
    let (store, _, _) = with_policy(ON);
    let mut deleted = base();
    deleted.retain(|(name, _)| *name != "title");
    let (oid, read) = conflict(&store, &doc(&base()), &doc(&deleted));
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(oid, read, &doc(&changed(&[("title", V::Str("C"))])))
        .unwrap();
    assert!(txn.vote_resolving().is_err());
    assert_eq!(
        txn.resolutions()[0].outcome,
        Resolved::Refused(Reason::FieldDeleted)
    );
}

/// Merges `committed` and `new` changes of the field `field` of the
/// strategy `strategy`. Returns the outcome and the merged record.
fn strategy(
    field: &'static str,
    entry: Entry,
    base_value: Option<V>,
    committed: V,
    new: V,
) -> (Resolved, Option<Vec<u8>>) {
    let (store, _, _) = with_policy(entry);
    let mut old = base();
    if let Some(v) = base_value {
        old.push((field, v));
    }
    let with = |v: V| {
        let mut fields = old.clone();
        match fields.iter_mut().find(|(name, _)| *name == field) {
            Some(slot) => slot.1 = v,
            None => fields.push((field, v)),
        }
        doc(&fields)
    };
    let (oid, read) = conflict(&store, &doc(&old), &with(committed));
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(oid, read, &with(new)).unwrap();
    let voted = txn.vote_resolving();
    let outcome = txn.resolutions()[0].outcome;
    if voted.is_ok_and(|conflicts| conflicts.is_empty()) {
        let tid = txn.finish().unwrap();
        let (data, serial) = store.load(oid).unwrap();
        assert_eq!(serial, tid);
        return (outcome, Some(data));
    }
    (outcome, None)
}

/// Returns the record of `base` with `field` set to `v`.
fn base_with(field: &'static str, v: V) -> Vec<u8> {
    let mut fields = base();
    match fields.iter_mut().find(|(name, _)| *name == field) {
        Some(slot) => slot.1 = v,
        None => fields.push((field, v)),
    }
    doc(&fields)
}

#[test]
fn the_strategies_merge_a_field_that_both_sides_changed() {
    const LWW: Entry = &[
        ("tier2", Setting::Text("on")),
        ("fields", Setting::Fields(&[("title", "lww")])),
    ];
    let (outcome, merged) = strategy("title", LWW, None, V::Str("B"), V::Str("C"));
    assert_eq!(outcome, Resolved::Merged);
    assert_eq!(merged, Some(base_with("title", V::Str("C"))));

    const MAX: Entry = &[
        ("tier2", Setting::Text("on")),
        ("default", Setting::Text("max")),
    ];
    let (_, merged) = strategy(
        "modification_date",
        MAX,
        None,
        V::Date(3_000_000),
        V::Date(2_000_000),
    );
    assert_eq!(
        merged,
        Some(base_with("modification_date", V::Date(3_000_000)))
    );
    let (_, merged) = strategy("count", MAX, None, V::Int(2), V::Int(5));
    assert_eq!(merged, Some(base_with("count", V::Int(5))));

    const SUM: Entry = &[
        ("tier2", Setting::Text("on")),
        ("fields", Setting::Fields(&[("count", "sum")])),
    ];
    let (_, merged) = strategy("count", SUM, None, V::Int(3), V::Int(4));
    assert_eq!(merged, Some(base_with("count", V::Int(6))));

    const UNION: Entry = &[
        ("tier2", Setting::Text("on")),
        ("fields", Setting::Fields(&[("subject", "union")])),
    ];
    let old = V::Tuple(vec![V::Str("a"), V::Str("b")]);
    let (_, merged) = strategy(
        "subject",
        UNION,
        Some(old),
        V::Tuple(vec![V::Str("a"), V::Str("c")]),
        V::Tuple(vec![V::Str("b"), V::Str("a"), V::Str("d")]),
    );
    // "b" is gone on the committed side, "c" and "d" are new.
    let expected = V::Tuple(vec![V::Str("a"), V::Str("c"), V::Str("d")]);
    assert_eq!(merged, Some(base_with("subject", expected)));

    const APPEND: Entry = &[
        ("tier2", Setting::Text("on")),
        ("fields", Setting::Fields(&[("history", "append")])),
    ];
    let old = V::List(vec![V::Int(1)]);
    let (_, merged) = strategy(
        "history",
        APPEND,
        Some(old.clone()),
        V::List(vec![V::Int(1), V::Int(2)]),
        V::List(vec![V::Int(1), V::Int(3)]),
    );
    let expected = V::List(vec![V::Int(1), V::Int(2), V::Int(3)]);
    assert_eq!(merged, Some(base_with("history", expected)));
    // A change of the old items is not an append.
    let (outcome, merged) = strategy(
        "history",
        APPEND,
        Some(old),
        V::List(vec![V::Int(9), V::Int(2)]),
        V::List(vec![V::Int(1), V::Int(3)]),
    );
    assert_eq!(outcome, Resolved::Refused(Reason::NotAppend));
    assert_eq!(merged, None);
}

#[test]
fn shadow_mode_merges_for_the_log_and_checks_the_resolution_of_the_caller() {
    const SHADOW: Entry = &[("tier2", Setting::Text("shadow"))];
    for (resolved, same) in [
        (
            doc(&changed(&[
                ("title", V::Str("B")),
                ("description", V::Str("two")),
            ])),
            true,
        ),
        (doc(&changed(&[("title", V::Str("Z"))])), false),
    ] {
        let (store, _, _) = with_policy(SHADOW);
        let (oid, read) = conflict(
            &store,
            &doc(&base()),
            &doc(&changed(&[("title", V::Str("B"))])),
        );
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.store(oid, read, &doc(&changed(&[("description", V::Str("two"))])))
            .unwrap();
        let conflicts = txn.vote_resolving().unwrap();
        let [(listed, committed)] = &conflicts[..] else {
            panic!("one conflict");
        };
        assert_eq!(*listed, oid);
        let resolution = &txn.resolutions()[0];
        assert_eq!(
            (resolution.mode, resolution.outcome, resolution.applied),
            (Mode::Shadow, Resolved::Merged, false)
        );
        // The caller stores its own resolution, then votes again.
        txn.store(oid, committed.tid, &resolved).unwrap();
        txn.vote().unwrap();
        let [verification] = txn.verifications() else {
            panic!("{:?}", txn.verifications());
        };
        assert_eq!((verification.tier, verification.same), (2, same));
        let lines = txn.verification_log_lines(std::time::UNIX_EPOCH, None);
        assert!(
            lines[0].contains(&format!("\"outcome\":\"verified\",\"same\":{same}")),
            "{}",
            lines[0]
        );
        txn.finish().unwrap();
    }
}

#[test]
fn a_new_version_of_the_policy_applies_at_the_next_conflict() {
    let (store, policy_oid, serial) = with_policy(ON);
    let off: Entry = &[("tier2", Setting::Text("off"))];
    commit(
        &store,
        policy_oid,
        serial,
        &policy(2, &[("m.Document", off)]),
    );
    let (oid, read) = conflict(
        &store,
        &doc(&base()),
        &doc(&changed(&[("title", V::Str("B"))])),
    );
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(oid, read, &doc(&changed(&[("description", V::Str("two"))])))
        .unwrap();
    assert_eq!(txn.vote_resolving().unwrap().len(), 1);
    assert!(txn.resolutions().is_empty());
}

#[test]
fn a_policy_that_does_not_read_counts_as_no_policy() {
    let store = open();
    let oid = new_oid(&store);
    // A record of another class under the name of the policy.
    commit(
        &store,
        oid,
        Tid::ZERO,
        &record(("m", "Junk"), &[("version", V::Int(1))]),
    );
    commit(&store, Oid(0), Tid::ZERO, &root(oid));
    let (doc_oid, read) = conflict(
        &store,
        &doc(&base()),
        &doc(&changed(&[("title", V::Str("B"))])),
    );
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(
        doc_oid,
        read,
        &doc(&changed(&[("description", V::Str("two"))])),
    )
    .unwrap();
    assert_eq!(txn.vote_resolving().unwrap().len(), 1);
}

#[test]
fn the_policy_sets_the_mode_of_a_class_of_tier_1() {
    // A `Length` with tier 1 off in the policy goes to the caller.
    let store = open();
    let oid = new_oid(&store);
    let off: Entry = &[("tier1", Setting::Text("off"))];
    commit(
        &store,
        oid,
        Tid::ZERO,
        &policy(1, &[("BTrees.Length.Length", off)]),
    );
    commit(&store, Oid(0), Tid::ZERO, &root(oid));
    let length = |n: i128| {
        let mut heap = Heap::new();
        let class = heap.new_class("BTrees.Length", "Length");
        let mut pickler = Pickler::new(&heap);
        pickler.dump(class).unwrap();
        pickler.dump(Value::Int(n)).unwrap();
        pickler.into_bytes()
    };
    let (length_oid, read) = conflict(&store, &length(1), &length(2));
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(length_oid, read, &length(3)).unwrap();
    assert_eq!(txn.vote_resolving().unwrap().len(), 1);
    assert!(txn.resolutions().is_empty());
}
