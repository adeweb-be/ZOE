//! Tier 2 in the vote: the merges of fields that the merge policy names
//! (ADR 0046), the policy under the root, and the check of a merge of
//! shadow mode against the resolution of the caller.
//!
//! The records are built with the pickler of `zoe_pickle::python`. The
//! comparison with a Python reference of each strategy is the acceptance
//! test `test_field_merges.py`.

use zoe_core::vfs::MemVfs;
use zoe_core::{
    Mode, Oid, Options, Reason, Resolution, Resolved, Store, SystemClock, Tid, ZoeError,
};
use zoe_pickle::python::{Heap, Instance, Made, Pickler, Value};

/// A value of a field of a test record.
#[derive(Clone)]
enum V {
    Int(i128),
    Str(&'static str),
    Bytes(&'static [u8]),
    List(Vec<V>),
    Tuple(Vec<V>),
    /// A `DateTime.DateTime` of this many microseconds since the epoch.
    Date(i128),
    /// A dict of these keys and values, in this order.
    Dict(Vec<(V, V)>),
}

fn value(heap: &mut Heap, v: &V) -> Value {
    match v {
        V::Int(int) => Value::Int(*int),
        V::Str(text) => heap.new_str(text),
        V::Bytes(bytes) => heap.new_bytes(bytes),
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
        V::Dict(items) => {
            let items = items
                .iter()
                .map(|(key, item)| (value(heap, key), value(heap, item)))
                .collect();
            heap.new_dict(items)
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

/// Runs a conflict of a change of the title and a change of the
/// description of a Document. Returns the outcome of the vote: true when
/// the vote merged it, false when it listed it for the caller.
fn title_and_description(store: &Store) -> bool {
    let (oid, read) = conflict(
        store,
        &doc(&base()),
        &doc(&changed(&[("title", V::Str("B"))])),
    );
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(oid, read, &doc(&changed(&[("description", V::Str("two"))])))
        .unwrap();
    let conflicts = txn.vote_resolving().unwrap();
    conflicts.is_empty()
}

#[test]
fn a_new_version_of_the_policy_applies_at_the_next_conflict() {
    let (store, policy_oid, serial) = with_policy(ON);
    // A conflict reads the policy before its change.
    assert!(title_and_description(&store));
    let off: Entry = &[("tier2", Setting::Text("off"))];
    let serial = commit(
        &store,
        policy_oid,
        serial,
        &policy(2, &[("m.Document", off)]),
    );
    assert!(!title_and_description(&store));
    // A change of the root alone reads the same policy again.
    let root_serial = store.load(Oid(0)).unwrap().1;
    commit(&store, Oid(0), root_serial, &root(policy_oid));
    assert!(!title_and_description(&store));
    commit(
        &store,
        policy_oid,
        serial,
        &policy(3, &[("m.Document", ON)]),
    );
    assert!(title_and_description(&store));
}

#[test]
fn records_of_different_classes_give_no_answer() {
    let (store, _, _) = with_policy(ON);
    // The committed transaction changed the class of the object.
    let other = record(("m", "Other"), &changed(&[("title", V::Str("B"))]));
    let (oid, read) = conflict(&store, &doc(&base()), &other);
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(oid, read, &doc(&changed(&[("description", V::Str("two"))])))
        .unwrap();
    assert_eq!(txn.vote_resolving().unwrap().len(), 1);
    assert_eq!(
        txn.resolutions()[0].outcome,
        Resolved::Undecided("records of different classes")
    );
}

#[test]
fn a_refusal_names_its_field_or_its_group() {
    let (store, _, _) = with_policy(ON);
    let (oid, read) = conflict(
        &store,
        &doc(&base()),
        &doc(&changed(&[("title", V::Str("B"))])),
    );
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(oid, read, &doc(&changed(&[("title", V::Str("C"))])))
        .unwrap();
    assert!(txn.vote_resolving().is_err());
    assert_eq!(txn.resolutions()[0].fields, ["title"]);
    let line = &txn.resolution_log_lines(std::time::UNIX_EPOCH, None)[0];
    assert!(line.contains("\"fields\":[\"title\"]"), "{line}");
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
    assert_eq!(txn.resolutions()[0].fields, ["title", "description"]);
}

/// Returns the record of a Document whose state `make` builds.
fn built(make: impl FnOnce(&mut Heap) -> Vec<(Value, Value)>) -> Vec<u8> {
    let mut heap = Heap::new();
    let class = heap.new_class(DOCUMENT.0, DOCUMENT.1);
    let items = make(&mut heap);
    let state = heap.new_dict(items);
    let mut pickler = Pickler::new(&heap);
    pickler.dump(class).unwrap();
    pickler.dump(state).unwrap();
    pickler.into_bytes()
}

/// Runs a conflict of three records with the entry `entry`, on a thread with
/// a stack of 1 MiB. Returns the outcome of tier 2.
fn tier2_outcome(entry: Entry, old: Vec<u8>, committed: Vec<u8>, new: Vec<u8>) -> Resolved {
    std::thread::Builder::new()
        .stack_size(1 << 20)
        .spawn(move || {
            let (store, _, _) = with_policy(entry);
            let (oid, read) = conflict(&store, &old, &committed);
            let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
            txn.store(oid, read, &new).unwrap();
            let _ = txn.vote_resolving();
            txn.resolutions()[0].outcome
        })
        .unwrap()
        .join()
        .unwrap()
}

#[test]
fn a_deep_item_of_a_union_needs_no_deep_stack() {
    const UNION: Entry = &[
        ("tier2", Setting::Text("on")),
        ("fields", Setting::Fields(&[("tags", "union")])),
    ];
    let with = |heap: &mut Heap, tags: Vec<Value>| {
        let key = heap.new_str("tags");
        let list = heap.new_list(tags);
        vec![(key, list)]
    };
    let old = built(|heap| with(heap, Vec::new()));
    // A tuple nested 100,000 deep.
    let committed = built(|heap| {
        let mut deep = heap.new_tuple(Vec::new());
        for _ in 0..100_000 {
            deep = heap.new_tuple(vec![deep]);
        }
        with(heap, vec![deep])
    });
    let new = built(|heap| with(heap, vec![Value::Int(7)]));
    assert_eq!(tier2_outcome(UNION, old, committed, new), Resolved::Merged);
}

#[test]
fn a_value_that_shares_its_parts_merges_in_proportion_to_its_record() {
    // `x = (x, x)` 40 times in one field: 2^40 leaves, 41 objects.
    let make = |title: &'static str| {
        built(move |heap| {
            let mut shared = heap.new_tuple(vec![Value::Int(1)]);
            for _ in 0..40 {
                shared = heap.new_tuple(vec![shared, shared]);
            }
            let (key, value) = (heap.new_str("tree"), shared);
            let (title_key, title) = (heap.new_str("title"), heap.new_str(title));
            vec![(key, value), (title_key, title)]
        })
    };
    let started = std::time::Instant::now();
    let outcome = tier2_outcome(ON, make("A"), make("B"), make("A"));
    assert_eq!(outcome, Resolved::Merged);
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
}

#[test]
fn two_fields_can_share_an_immutable_value_and_not_a_list() {
    for (list, expected) in [(false, true), (true, false)] {
        let make = |title: &'static str| {
            built(move |heap| {
                let parts = vec![Value::Int(1), Value::Int(2)];
                let shared = if list {
                    heap.new_list(parts)
                } else {
                    heap.new_tuple(parts)
                };
                let (a, b) = (heap.new_str("a"), heap.new_str("b"));
                let (title_key, title) = (heap.new_str("title"), heap.new_str(title));
                vec![(a, shared), (b, shared), (title_key, title)]
            })
        };
        let outcome = tier2_outcome(ON, make("A"), make("B"), make("A"));
        assert_eq!(outcome == Resolved::Merged, expected, "list: {list}");
    }
}

#[test]
fn a_set_in_another_order_is_not_a_change() {
    let make = |order: [i128; 2], title: &'static str, description: &'static str| {
        built(move |heap| {
            let class = heap.new_class("builtins", "set");
            let items = order.iter().map(|&n| Value::Int(n)).collect();
            let set = heap.new_set(class, items);
            let fields = [("tags", set), ("title", heap.new_str(title))];
            let mut items: Vec<(Value, Value)> = fields
                .into_iter()
                .map(|(name, value)| (heap.new_str(name), value))
                .collect();
            let key = heap.new_str("description");
            items.push((key, heap.new_str(description)));
            items
        })
    };
    // The committed record holds the set in another order and the new title;
    // the new record changes the description: two fields, no conflict.
    let outcome = tier2_outcome(
        ON,
        make([1, 2], "A", "one"),
        make([2, 1], "B", "one"),
        make([1, 2], "A", "two"),
    );
    assert_eq!(outcome, Resolved::Merged);
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

#[test]
fn an_undo_after_a_change_of_another_field_merges_through_tier_2() {
    let (store, _, _) = with_policy(ON);
    let oid = new_oid(&store);
    let first = commit(&store, oid, Tid::ZERO, &doc(&base()));
    let undone = commit(
        &store,
        oid,
        first,
        &doc(&changed(&[("title", V::Str("B"))])),
    );
    commit(
        &store,
        oid,
        undone,
        &doc(&changed(&[
            ("title", V::Str("B")),
            ("description", V::Str("two")),
        ])),
    );
    let mut txn = store.tpc_begin(b"", b"undo", b"").unwrap();
    let staged = txn.undo(undone).unwrap();
    assert_eq!(staged.conflicts, []);
    let [resolution] = txn.resolutions() else {
        panic!("{:?}", txn.resolutions());
    };
    assert_eq!(
        (resolution.tier, resolution.mode, resolution.outcome),
        (2, Mode::On, Resolved::Merged)
    );
    assert!(resolution.undo && resolution.applied);
    assert_eq!(resolution.read, undone);
    txn.vote().unwrap();
    let tid = txn.finish().unwrap();
    // The undo gives the title back and keeps the later description.
    let merged = doc(&changed(&[("description", V::Str("two"))]));
    assert_eq!(store.load(oid).unwrap(), (merged, tid));
}

#[test]
fn an_undo_of_a_field_that_a_later_transaction_changed_again_fails() {
    let (store, _, _) = with_policy(ON);
    let oid = new_oid(&store);
    let first = commit(&store, oid, Tid::ZERO, &doc(&base()));
    let undone = commit(
        &store,
        oid,
        first,
        &doc(&changed(&[("title", V::Str("B"))])),
    );
    commit(
        &store,
        oid,
        undone,
        &doc(&changed(&[("title", V::Str("C"))])),
    );
    let mut txn = store.tpc_begin(b"", b"undo", b"").unwrap();
    assert!(matches!(
        txn.undo(undone),
        Err(ZoeError::Undo { oid: Some(found), .. }) if found == oid
    ));
}

/// Returns the record of a `Length` of `value`.
fn length(value: i128) -> Vec<u8> {
    let mut heap = Heap::new();
    let class = heap.new_class("BTrees.Length", "Length");
    let mut pickler = Pickler::new(&heap);
    pickler.dump(class).unwrap();
    pickler.dump(Value::Int(value)).unwrap();
    pickler.into_bytes()
}

#[test]
fn the_policy_sets_the_mode_of_tier_1_of_an_undo() {
    let store = open();
    let policy_oid = new_oid(&store);
    let entry: Entry = &[("tier1", Setting::Text("off"))];
    let record = policy(1, &[("BTrees.Length.Length", entry)]);
    commit(&store, policy_oid, Tid::ZERO, &record);
    commit(&store, Oid(0), Tid::ZERO, &root(policy_oid));
    let oid = new_oid(&store);
    let first = commit(&store, oid, Tid::ZERO, &length(10));
    let undone = commit(&store, oid, first, &length(15));
    commit(&store, oid, undone, &length(17));
    let mut txn = store.tpc_begin(b"", b"undo", b"").unwrap();
    let staged = txn.undo(undone).unwrap();
    // Tier 1 is off for the class: the caller gets the oid, and no entry.
    assert_eq!(staged.conflicts.len(), 1);
    assert_eq!(txn.resolutions(), []);
}

#[test]
fn a_field_of_strategy_lww_that_both_sides_changed_refuses_in_an_undo() {
    const LWW: Entry = &[
        ("tier2", Setting::Text("on")),
        ("fields", Setting::Fields(&[("title", "lww")])),
    ];
    let (store, _, _) = with_policy(LWW);
    let oid = new_oid(&store);
    let first = commit(&store, oid, Tid::ZERO, &doc(&base()));
    let undone = commit(
        &store,
        oid,
        first,
        &doc(&changed(&[("title", V::Str("B"))])),
    );
    commit(
        &store,
        oid,
        undone,
        &doc(&changed(&[("title", V::Str("C"))])),
    );
    let mut txn = store.tpc_begin(b"", b"undo", b"").unwrap();
    assert!(matches!(
        txn.undo(undone),
        Err(ZoeError::Undo { oid: Some(found), .. }) if found == oid
    ));
    let [entry] = txn.resolutions() else {
        panic!("{:?}", txn.resolutions());
    };
    assert_eq!(entry.outcome, Resolved::Refused(Reason::FieldChanged));
    // In the vote, lww takes the new value.
    let (outcome, _) = strategy("title", LWW, None, V::Str("B"), V::Str("C"));
    assert_eq!(outcome, Resolved::Merged);
}

/// The entry of a class whose field `data` merges key by key.
const KEYS: Entry = &[
    ("tier2", Setting::Text("on")),
    ("fields", Setting::Fields(&[("data", "keys")])),
];

/// Returns the record of a mapping whose dict holds `items`: an object
/// whose state holds the field `data` alone, as a `PersistentMapping`.
fn mapping(items: &[(V, V)]) -> Vec<u8> {
    doc(&[("data", V::Dict(items.to_vec()))])
}

/// Runs a conflict of the records `base`, `committed` and `new` with the
/// entry `entry`. Returns the resolution of tier 2, and the stored record
/// when the vote merged.
fn mapping_conflict(
    entry: Entry,
    base: &[u8],
    committed: &[u8],
    new: &[u8],
) -> (Resolution, Option<Vec<u8>>) {
    let (store, _, _) = with_policy(entry);
    let (oid, read) = conflict(&store, base, committed);
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(oid, read, new).unwrap();
    let voted = txn.vote_resolving();
    let resolution = txn.resolutions()[0].clone();
    if voted.is_ok_and(|conflicts| conflicts.is_empty()) {
        let tid = txn.finish().unwrap();
        let (data, serial) = store.load(oid).unwrap();
        assert_eq!(serial, tid);
        return (resolution, Some(data));
    }
    (resolution, None)
}

fn s(text: &'static str) -> V {
    V::Str(text)
}

#[test]
fn changes_of_different_keys_of_a_mapping_merge() {
    let base = mapping(&[(s("key-a"), s("one")), (s("key-b"), s("two"))]);
    let committed = mapping(&[
        (s("key-a"), s("one")),
        (s("key-b"), s("three")),
        (s("key-c"), s("four")),
    ]);
    let new = mapping(&[(s("key-b"), s("two")), (s("key-e"), s("five"))]);
    let (resolution, merged) = mapping_conflict(KEYS, &base, &committed, &new);
    assert_eq!(resolution.outcome, Resolved::Merged);
    // The committed keys in their order, then the key that only the new side
    // added. The new side deleted key-a.
    let expected = mapping(&[
        (s("key-b"), s("three")),
        (s("key-c"), s("four")),
        (s("key-e"), s("five")),
    ]);
    assert_eq!(merged, Some(expected));
    // The keys that the new record changed, in place of the field data.
    assert_eq!(resolution.keys, ["\"key-a\"", "\"key-e\""]);
    assert_eq!(resolution.changed, 2);
}

#[test]
fn equal_changes_of_one_key_merge_and_a_reorder_of_the_new_side_does_not_show() {
    let base = mapping(&[(s("key-a"), s("one")), (s("key-b"), s("two"))]);
    let committed = mapping(&[(s("key-a"), s("six")), (s("key-b"), s("two"))]);
    // The new side moves key-a to the end, with the value of the committed
    // side.
    let new = mapping(&[(s("key-b"), s("two")), (s("key-a"), s("six"))]);
    let (resolution, merged) = mapping_conflict(KEYS, &base, &committed, &new);
    assert_eq!(resolution.outcome, Resolved::Merged);
    assert_eq!(merged, Some(committed));
}

#[test]
fn two_changes_of_one_key_of_a_mapping_refuse() {
    let base = mapping(&[(s("key-a"), s("one")), (s("key-b"), s("two"))]);
    for (committed, new, reason) in [
        (
            mapping(&[(s("key-a"), s("six")), (s("key-b"), s("two"))]),
            mapping(&[(s("key-a"), s("seven")), (s("key-b"), s("two"))]),
            Reason::KeyChanged,
        ),
        (
            mapping(&[(s("key-b"), s("two"))]),
            mapping(&[(s("key-a"), s("seven")), (s("key-b"), s("two"))]),
            Reason::KeyDeleted,
        ),
        (
            mapping(&[(s("key-a"), s("six")), (s("key-b"), s("two"))]),
            mapping(&[(s("key-b"), s("two"))]),
            Reason::KeyDeleted,
        ),
        (
            mapping(&[
                (s("key-a"), s("one")),
                (s("key-b"), s("two")),
                (s("key-c"), s("x")),
            ]),
            mapping(&[
                (s("key-a"), s("one")),
                (s("key-b"), s("two")),
                (s("key-c"), s("y")),
            ]),
            Reason::KeyChanged,
        ),
    ] {
        let (resolution, merged) = mapping_conflict(KEYS, &base, &committed, &new);
        assert_eq!(resolution.outcome, Resolved::Refused(reason));
        assert_eq!(resolution.fields, ["data"]);
        assert_eq!(merged, None);
    }
}

#[test]
fn the_keys_of_a_mapping_are_ints_strs_bytes_and_tuples_of_those() {
    let tuple = |text, number| V::Tuple(vec![V::Str(text), V::Int(number)]);
    let base = mapping(&[
        (V::Int(7), s("one")),
        (s("key-a"), s("two")),
        (V::Bytes(b"key-b"), s("three")),
        (tuple("key-c", 1), s("four")),
    ]);
    let committed = mapping(&[
        (V::Int(7), s("five")),
        (s("key-a"), s("two")),
        (V::Bytes(b"key-b"), s("three")),
        (tuple("key-c", 1), s("four")),
    ]);
    let new = mapping(&[
        (V::Int(7), s("one")),
        (s("key-a"), s("six")),
        (V::Bytes(b"key-b"), s("seven")),
        (tuple("key-c", 1), s("eight")),
        (V::Int(-2), s("nine")),
    ]);
    let (resolution, merged) = mapping_conflict(KEYS, &base, &committed, &new);
    assert_eq!(resolution.outcome, Resolved::Merged);
    let expected = mapping(&[
        (V::Int(7), s("five")),
        (s("key-a"), s("six")),
        (V::Bytes(b"key-b"), s("seven")),
        (tuple("key-c", 1), s("eight")),
        (V::Int(-2), s("nine")),
    ]);
    assert_eq!(merged, Some(expected));
    // The order of the model: the ints, the strs, the bytes, the tuples.
    assert_eq!(
        resolution.keys,
        ["-2", "\"key-a\"", "b'key-b'", "(\"key-c\", 1)"]
    );
}

#[test]
fn the_strategy_keys_gives_no_answer_for_values_that_are_not_dicts() {
    let with = |data: V| doc(&[("data", data)]);
    let (resolution, merged) = mapping_conflict(
        KEYS,
        &with(V::List(vec![V::Int(1)])),
        &with(V::List(vec![V::Int(2)])),
        &with(V::List(vec![V::Int(3)])),
    );
    assert_eq!(
        resolution.outcome,
        Resolved::Undecided("values that the strategy keys does not take")
    );
    assert_eq!(merged, None);
    // A tuple of a tuple is not a key of the model.
    let nested = V::Tuple(vec![V::Tuple(vec![s("key-a")])]);
    let (resolution, _) = mapping_conflict(
        KEYS,
        &mapping(&[(nested.clone(), s("one"))]),
        &mapping(&[(nested.clone(), s("two"))]),
        &mapping(&[(nested, s("one")), (s("key-b"), s("three"))]),
    );
    assert_eq!(
        resolution.outcome,
        Resolved::Undecided("a dict key of another type")
    );
}

#[test]
fn two_keys_that_hold_one_list_give_no_answer() {
    // The new state adds key-d, and holds one list under two keys.
    let shared = built(|heap| {
        let list = heap.new_list(vec![Value::Int(1)]);
        let other = heap.new_list(vec![Value::Int(4)]);
        let items = vec![
            (heap.new_str("key-a"), list),
            (heap.new_str("key-b"), list),
            (heap.new_str("key-d"), other),
        ];
        vec![(heap.new_str("data"), heap.new_dict(items))]
    });
    let list = |n| V::List(vec![V::Int(n)]);
    let base = mapping(&[(s("key-a"), list(1)), (s("key-b"), list(1))]);
    let committed = mapping(&[
        (s("key-a"), list(1)),
        (s("key-b"), list(1)),
        (s("key-c"), list(3)),
    ]);
    let new = mapping(&[
        (s("key-a"), list(1)),
        (s("key-b"), list(1)),
        (s("key-d"), list(4)),
    ]);
    let (resolution, _) = mapping_conflict(KEYS, &base, &committed, &new);
    assert_eq!(resolution.outcome, Resolved::Merged);
    let (resolution, _) = mapping_conflict(KEYS, &base, &committed, &shared);
    assert_eq!(
        resolution.outcome,
        Resolved::Undecided("an object that two keys of a mapping share")
    );
}

#[test]
fn an_undo_merges_a_mapping_key_by_key_and_refuses_a_key_changed_later() {
    let (store, _, _) = with_policy(KEYS);
    let oid = new_oid(&store);
    let items = |a, b| mapping(&[(s("key-a"), s(a)), (s("key-b"), s(b))]);
    let first = commit(&store, oid, Tid::ZERO, &items("one", "two"));
    let undone = commit(&store, oid, first, &items("three", "two"));
    let later = commit(&store, oid, undone, &items("three", "four"));
    // No later transaction changed key-a: the undo gives it back.
    let mut txn = store.tpc_begin(b"", b"undo", b"").unwrap();
    assert_eq!(txn.undo(undone).unwrap().conflicts, []);
    assert_eq!(txn.resolutions()[0].outcome, Resolved::Merged);
    txn.vote().unwrap();
    let tid = txn.finish().unwrap();
    assert_eq!(store.load(oid).unwrap(), (items("one", "four"), tid));
    // A later transaction changed key-b: the undo of `later` would drop it.
    commit(&store, oid, tid, &items("one", "five"));
    let mut txn = store.tpc_begin(b"", b"undo", b"").unwrap();
    assert!(matches!(
        txn.undo(later),
        Err(ZoeError::Undo { oid: Some(found), .. }) if found == oid
    ));
}

/// The entry of a class whose fields all merge key by key.
const ALL_KEYS: Entry = &[
    ("tier2", Setting::Text("on")),
    ("default", Setting::Text("keys")),
];

/// Returns the record of a state with `fields` dict fields: field `n`
/// holds the dict that `dict` gives for `n`.
fn dict_fields(fields: usize, dict: impl Fn(&mut Heap, usize) -> Value) -> Vec<u8> {
    built(|heap| {
        (0..fields)
            .map(|n| (heap.new_str(&format!("field-{n:05}")), dict(heap, n)))
            .collect()
    })
}

#[test]
fn many_fields_of_the_strategy_keys_merge_in_proportion_to_their_record() {
    const FIELDS: usize = 20_000;
    let with = |key: Option<&'static str>| {
        dict_fields(FIELDS, move |heap, _| match key {
            Some(key) => {
                let key = heap.new_str(key);
                heap.new_dict(vec![(key, Value::Int(1))])
            }
            None => heap.new_dict(Vec::new()),
        })
    };
    let (base, committed, new) = (with(None), with(Some("xx")), with(Some("yy")));
    let started = std::time::Instant::now();
    let (resolution, merged) = mapping_conflict(ALL_KEYS, &base, &committed, &new);
    assert_eq!(resolution.outcome, Resolved::Merged);
    assert!(merged.is_some());
    // One key of each field, and the first fields in the line.
    assert_eq!(resolution.changed, FIELDS);
    assert_eq!(resolution.keys.len(), 20);
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
}
