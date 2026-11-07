//! The acceptance tests of the catalog mirror with KeywordIndex, DateIndex,
//! DateRangeIndex, BooleanIndex and UUIDIndex (ADR 0059, sections 3, 7 and
//! 8).
//!
//! `tests/data/indexes.json` holds the records of a catalog with these
//! indexes that Products.ZCatalog wrote, the records of three commits, the
//! plans of a set of queries and the answers of ZCatalog after the base and
//! after each commit, or the reason of the refusal of the mirror.
//! `tests/data/indexes.py` writes it.

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value as Json;
use zoe_catalog::build::build;
use zoe_catalog::query::View;
use zoe_catalog::testing::{
    self, BooleanOids, CatalogModel, DateRangeOids, II, II_SET, IO, IndexKind, Item, OI, OO,
    TreeRecord, boolean_index_record, bucket_record, date_index_record, date_range_index_record,
    index_record, length_record, tree_record, uuid_index_record,
};
use zoe_catalog::value::Val;
use zoe_catalog::{
    Clause, Config, Mirrors, Opener, Plan, PlanError, Reads, Reason, Reply, Wire, decode_plan,
};
use zoe_core::Store;
use zoe_core::vfs::MemVfs;
use zoe_pickle::btrees::{self, TreeState};
use zoe_pickle::python::{Heap, Object, Value};

const WAIT: Duration = Duration::from_secs(20);

fn data(name: &str) -> Json {
    let path = format!("{}/tests/data/{name}", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(path).expect("the test data");
    serde_json::from_str(&text).expect("JSON")
}

fn hex(text: &str) -> Vec<u8> {
    (0..text.len() / 2)
        .map(|i| u8::from_str_radix(&text[2 * i..2 * i + 2], 16).expect("hex"))
        .collect()
}

fn records(json: &Json) -> Vec<(u64, Vec<u8>)> {
    json.as_object()
        .expect("records")
        .iter()
        .map(|(oid, data)| {
            (
                u64::from_str_radix(oid, 16).expect("an oid"),
                hex(data.as_str().expect("hex")),
            )
        })
        .collect()
}

fn wire(json: &Json) -> Wire {
    match json {
        Json::Null => Wire::Nil,
        Json::Bool(b) => Wire::Bool(*b),
        Json::Number(n) => match n.as_i64() {
            Some(i) => Wire::Int(i),
            None => Wire::Float(n.as_f64().expect("a number")),
        },
        Json::String(s) => Wire::Str(s.clone()),
        Json::Array(items) => Wire::Array(items.iter().map(wire).collect()),
        Json::Object(map) => Wire::Map(
            map.iter()
                .map(|(k, v)| (Wire::Str(k.clone()), wire(v)))
                .collect(),
        ),
    }
}

fn plan(json: &Json) -> Plan {
    decode_plan(&wire(json)).expect("a plan")
}

fn store() -> Arc<Store> {
    Arc::new(Store::open_vfs(MemVfs::new()).expect("a store"))
}

fn mirrors(store: &Arc<Store>, config: Config) -> Mirrors {
    let weak = Arc::downgrade(store);
    let open: Opener = Arc::new(move || weak.upgrade().map(|s| s as Arc<dyn Reads>));
    Mirrors::start(open, store.last_transaction().0, config)
}

fn commit(
    store: &Store,
    mirrors: Option<&Mirrors>,
    records: &[(u64, Vec<u8>)],
    keys: &[[u8; 8]],
) -> u64 {
    let tid = testing::commit(store, records, keys, &[]).expect("a commit");
    if let Some(mirrors) = mirrors {
        mirrors.committed(tid);
    }
    tid
}

fn ready_at(mirrors: &Mirrors, catalog: u64, tid: u64) -> bool {
    mirrors.wait_for(catalog, WAIT, |s| {
        s.is_some_and(|s| s.state == "ready" && s.applied >= Some(tid))
    })
}

/// Checks a reply, or a refusal, against an answer of the test data.
fn same_answer(got: &Result<Reply, Reason>, expected: &Json, what: &str) {
    if let Some(reason) = expected["f"].as_str() {
        assert_eq!(
            got.as_ref().map_err(|r| r.name()).err(),
            Some(reason),
            "{what}: the refusal"
        );
        return;
    }
    let reply = got
        .as_ref()
        .unwrap_or_else(|r| panic!("{what}: refused with {}", r.name()));
    let rids: Vec<i32> = expected["r"]
        .as_array()
        .expect("rids")
        .iter()
        .map(|r| r.as_i64().expect("a rid") as i32)
        .collect();
    assert_eq!(reply.rids, rids, "{what}: the rids");
    assert_eq!(Some(reply.len), expected["n"].as_i64(), "{what}: len()");
    assert_eq!(
        Some(reply.actual),
        expected["a"].as_i64(),
        "{what}: actual_result_count"
    );
}

/// The oid and the class of a persistent reference.
fn reference(heap: &Heap, value: Value) -> (u64, String, String) {
    let r = heap.reference(value).expect("a reference");
    let oid = u64::from_be_bytes(r.oid.as_slice().try_into().unwrap());
    let parts = heap.tuple(r.data).unwrap();
    let (module, name) = heap.class(parts[1]).unwrap();
    (oid, module.to_owned(), name.to_owned())
}

fn leak(text: String) -> &'static str {
    Box::leak(text.into_boxed_str())
}

/// The items of a bucket state, or of the inline bucket of a tree.
fn items(heap: &Heap, set: bool, state: Value) -> Vec<(Val, Item)> {
    let state = btrees::bucket_state(heap, state, !set).unwrap();
    let step = if set { 1 } else { 2 };
    state
        .items
        .chunks(step)
        .map(|pair| {
            let key = Val::read(heap, pair[0]).unwrap();
            let value = if set {
                Item::Val(Val::None)
            } else {
                match heap.object(pair[1]) {
                    Some(Object::Reference(_)) => {
                        let (oid, module, name) = reference(heap, pair[1]);
                        Item::Ref(oid, leak(module), leak(name))
                    }
                    _ => Item::Val(Val::read(heap, pair[1]).unwrap()),
                }
            };
            (key, value)
        })
        .collect()
}

/// Tells if the value holds a str object twice.
fn shares(heap: &Heap, value: Value) -> bool {
    let mut seen = std::collections::HashSet::new();
    let mut todo = vec![value];
    while let Some(v) = todo.pop() {
        let Value::Object(id) = v else { continue };
        match heap.get(id) {
            Object::Str(_) if !seen.insert(id) => return true,
            Object::Tuple(items) | Object::List(items) => todo.extend(items.iter().copied()),
            _ => {}
        }
    }
    false
}

/// The record of a tree or a bucket as the writers of `testing` give it,
/// or `None` for another class or a record whose values share an object.
fn tree_or_bucket(record: &zoe_catalog::read::Record) -> Option<Vec<u8>> {
    let heap = &record.heap;
    let family = [IO, II, OI, OO, II_SET].into_iter().find(|f| {
        f.module == record.module && (f.tree == record.name || f.bucket == record.name)
    })?;
    if shares(heap, record.state) {
        return None;
    }
    if family.tree == record.name {
        let state = match btrees::tree_state(heap, record.state).unwrap() {
            TreeState::Empty => TreeRecord::Empty,
            TreeState::Inline(bucket) => TreeRecord::Inline(items(heap, family.set, bucket)),
            TreeState::Split => {
                let whole = heap.tuple(record.state).unwrap();
                let parts = heap.tuple(whole[0]).unwrap();
                TreeRecord::Split {
                    first: reference(heap, whole[1]).0,
                    children: parts
                        .iter()
                        .step_by(2)
                        .map(|c| {
                            let (oid, _, name) = reference(heap, *c);
                            (oid, name == family.tree)
                        })
                        .collect(),
                    keys: parts
                        .iter()
                        .skip(1)
                        .step_by(2)
                        .map(|k| Val::read(heap, *k).unwrap())
                        .collect(),
                }
            }
        };
        Some(tree_record(family, &state))
    } else {
        let state = btrees::bucket_state(heap, record.state, !family.set).unwrap();
        let next = state.next.map(|n| reference(heap, n).0);
        Some(bucket_record(
            family,
            &items(heap, family.set, record.state),
            next,
        ))
    }
}

#[test]
fn testing_writes_the_index_records_that_zcatalog_writes() {
    let data = data("indexes.json");
    let mut checked: std::collections::HashMap<String, usize> = Default::default();
    let mut all: Vec<(u64, Vec<u8>)> = records(&data["base"]);
    for c in data["commits"].as_array().unwrap() {
        all.extend(records(&c["records"]));
    }
    for (oid, bytes) in all {
        let record = zoe_catalog::read::Record::decode(&bytes).expect("a record");
        let heap = &record.heap;
        let class = format!("{}.{}", record.module, record.name);
        let oid_of = |name: &str| record.attr_oid(name).unwrap();
        let id = || heap.str(record.attr("id").unwrap()).unwrap();
        let rebuilt = match record.name.as_str() {
            "KeywordIndex" | "FieldIndex" => index_record(
                &record.module,
                &record.name,
                id(),
                oid_of("_length"),
                oid_of("_index"),
                oid_of("_unindex"),
                oid_of("_counter"),
            ),
            "DateIndex" => date_index_record(
                id(),
                oid_of("_index"),
                oid_of("_unindex"),
                oid_of("_length"),
                oid_of("_counter"),
            ),
            "UUIDIndex" => uuid_index_record(
                id(),
                oid_of("_length"),
                oid_of("_index"),
                oid_of("_unindex"),
                oid_of("_counter"),
            ),
            "BooleanIndex" => boolean_index_record(
                id(),
                &BooleanOids {
                    index: oid_of("_index"),
                    index_length: oid_of("_index_length"),
                    index_value: match record.attr("_index_value") {
                        Some(Value::Int(i)) => i64::try_from(i).unwrap(),
                        other => panic!("_index_value {other:?}"),
                    },
                    unindex: oid_of("_unindex"),
                    length: oid_of("_length"),
                    counter: oid_of("_counter"),
                },
            ),
            "DateRangeIndex" => date_range_index_record(
                id(),
                &DateRangeOids {
                    since_field: heap.str(record.attr("_since_field").unwrap()).unwrap(),
                    until_field: heap.str(record.attr("_until_field").unwrap()).unwrap(),
                    always: oid_of("_always"),
                    trees: ["_since_only", "_until_only", "_since", "_until"].map(oid_of),
                    unindex: oid_of("_unindex"),
                    length: oid_of("_length"),
                    counter: oid_of("_counter"),
                },
            ),
            "Length" => length_record(record.int_state().unwrap()),
            _ => match tree_or_bucket(&record) {
                Some(rebuilt) => rebuilt,
                None => continue,
            },
        };
        assert_eq!(rebuilt, bytes, "the record {oid:#x} of {class}");
        *checked.entry(class).or_default() += 1;
    }
    for class in [
        "Products.PluginIndexes.KeywordIndex.KeywordIndex.KeywordIndex",
        "Products.PluginIndexes.DateIndex.DateIndex.DateIndex",
        "Products.PluginIndexes.DateRangeIndex.DateRangeIndex.DateRangeIndex",
        "Products.PluginIndexes.BooleanIndex.BooleanIndex.BooleanIndex",
        "Products.PluginIndexes.UUIDIndex.UUIDIndex.UUIDIndex",
        "BTrees.IIBTree.IIBTree",
        "BTrees.IIBTree.IIBucket",
        "BTrees.IIBTree.IITreeSet",
        "BTrees.IOBTree.IOBTree",
        "BTrees.IOBTree.IOBucket",
        "BTrees.OIBTree.OIBTree",
        "BTrees.OIBTree.OIBucket",
    ] {
        assert!(
            checked.get(class).is_some_and(|n| *n > 0),
            "no record of {class}: {checked:?}"
        );
    }
}

#[test]
fn testing_writes_a_tree_with_an_inner_child_as_cpython_does() {
    let data = data("zcatalog.json");
    let mut inner = 0;
    for (oid, bytes) in records(&data["base"]) {
        let record = zoe_catalog::read::Record::decode(&bytes).expect("a record");
        let heap = &record.heap;
        if record.module != OO.module || record.name != OO.tree {
            continue;
        }
        if !matches!(
            btrees::tree_state(heap, record.state),
            Some(TreeState::Split)
        ) {
            continue;
        }
        let whole = heap.tuple(record.state).unwrap();
        let parts = heap.tuple(whole[0]).unwrap();
        if !parts
            .iter()
            .step_by(2)
            .any(|c| reference(heap, *c).2 == OO.tree)
        {
            continue;
        }
        assert_eq!(
            tree_or_bucket(&record),
            Some(bytes),
            "the record {oid:#x} of a tree with an inner child"
        );
        inner += 1;
    }
    assert_eq!(inner, 1, "the test data holds one tree with inner children");
}

/// A store with the base of `indexes.json`, its mirrors, the catalog and
/// the tid of the base.
fn indexes_store(config: Config) -> (Arc<Store>, Mirrors, u64, u64, Json) {
    let data = data("indexes.json");
    let store = store();
    let catalog = u64::from_str_radix(data["catalog"].as_str().unwrap(), 16).unwrap();
    let tid = commit(
        &store,
        None,
        &records(&data["base"]),
        &[catalog.to_be_bytes()],
    );
    let mirrors = mirrors(&store, config);
    assert!(ready_at(&mirrors, catalog, tid));
    (store, mirrors, catalog, tid, data)
}

fn check_answers(mirrors: &Mirrors, catalog: u64, tid: u64, data: &Json, at: usize, what: &str) {
    let plans = data["plans"].as_object().unwrap();
    for (name, plan_json) in plans {
        let got = mirrors
            .query(catalog, tid + 1, &plan(plan_json))
            .map_err(|f| f.reason);
        same_answer(
            &got,
            &data["answers"][name][at],
            &format!("{name} at {at}, {what}"),
        );
    }
}

#[test]
fn each_option_gives_the_answer_of_zcatalog_after_each_commit_and_at_older_tids() {
    let (store, mirrors, catalog, tid, data) = indexes_store(Config::default());
    check_answers(&mirrors, catalog, tid, &data, 0, "the base");
    let mut tids = vec![tid];
    for (at, c) in data["commits"].as_array().unwrap().iter().enumerate() {
        let tid = commit(&store, Some(&mirrors), &records(&c["records"]), &[]);
        assert!(ready_at(&mirrors, catalog, tid));
        check_answers(&mirrors, catalog, tid, &data, at + 1, "the last tid");
        tids.push(tid);
    }
    assert_eq!(mirrors.status(catalog).unwrap().rebases, 0);
    // A query at an older tid reads the overrides of that tid.
    for (at, tid) in tids.iter().enumerate() {
        check_answers(&mirrors, catalog, *tid, &data, at, "an older tid");
    }
    mirrors.stop();
}

#[test]
fn a_rebase_of_the_touched_indexes_gives_the_answers_of_each_tid() {
    let config = Config {
        rebase_rows: 1,
        ..Config::default()
    };
    let (store, mirrors, catalog, tid, data) = indexes_store(config);
    let mut tids = vec![tid];
    for c in data["commits"].as_array().unwrap() {
        tids.push(commit(&store, Some(&mirrors), &records(&c["records"]), &[]));
    }
    let last = *tids.last().unwrap();
    assert!(mirrors.wait_for(catalog, WAIT, |s| {
        s.is_some_and(|s| s.rebases >= 1 && s.base_tid == Some(last))
    }));
    // The tids before the new base read the old values of the change sets.
    for (at, tid) in tids.iter().enumerate() {
        check_answers(&mirrors, catalog, *tid, &data, at, "after a rebase");
    }
    // A fresh build at each tid gives the same answers.
    for (at, tid) in tids.iter().enumerate() {
        let built = build(&*store, catalog, *tid, 2, &|_| true).expect("a build");
        let over = Default::default();
        let view = View::new(&built.base, &over);
        for (name, plan_json) in data["plans"].as_object().unwrap() {
            let got = view.query(&plan(plan_json)).map_err(|f| f.reason);
            same_answer(
                &got,
                &data["answers"][name][at],
                &format!("{name} at {at}, a fresh build"),
            );
        }
    }
    mirrors.stop();
}

fn s(text: &str) -> Val {
    Val::Str(text.into())
}

fn clause(index: &str, keys: Vec<Val>) -> Clause {
    Clause {
        index: index.into(),
        keys,
        and: false,
        range: None,
        not: Vec::new(),
    }
}

fn plan_of(clauses: Vec<Clause>, sort: &[(&str, bool)], limit: Option<i64>) -> Plan {
    Plan {
        clauses,
        sort: sort
            .iter()
            .map(|(i, r)| ((*i).to_owned(), Some(*r)))
            .collect(),
        limit,
        b_start: 0,
        b_size: None,
    }
}

#[test]
fn a_range_with_and_and_an_option_that_an_index_does_not_take_are_refused() {
    let (_store, mirrors, catalog, tid, _data) = indexes_store(Config::default());
    let refusal = |plan: &Plan| mirrors.query(catalog, tid + 1, plan).unwrap_err().reason;
    let mut range = clause("Subject", Vec::new());
    range.and = true;
    range.range = Some((Some(s("beta")), Some(s("eps"))));
    assert_eq!(refusal(&plan_of(vec![range], &[], None)), Reason::AndRange);
    let mut range = clause("modified", Vec::new());
    range.and = true;
    range.range = Some((Some(Val::Int(0)), None));
    assert_eq!(refusal(&plan_of(vec![range], &[], None)), Reason::AndRange);
    // A sort on an index that the mirror does not sort.
    for name in ["Subject", "is_folderish", "effectiveRange"] {
        let plan = plan_of(
            vec![clause("portal_type", vec![s("Document")])],
            &[(name, false)],
            None,
        );
        assert_eq!(refusal(&plan), Reason::Sort, "a sort on {name}");
    }
    // Options that a BooleanIndex and a DateRangeIndex do not take.
    let mut not = clause("is_folderish", vec![Val::Bool(true)]);
    not.not = vec![Val::Bool(false)];
    assert_eq!(refusal(&plan_of(vec![not], &[], None)), Reason::Option);
    let two = clause("effectiveRange", vec![Val::Int(1), Val::Int(2)]);
    assert_eq!(refusal(&plan_of(vec![two], &[], None)), Reason::Option);
    mirrors.stop();
}

#[test]
fn the_plan_reads_the_clauses_of_a_booleanindex_and_of_a_daterangeindex() {
    let read = |text: &str| {
        let json: Json = serde_json::from_str(text).unwrap();
        decode_plan(&wire(&json))
    };
    let plan = read(
        r#"{"c": [{"k": "b", "i": "is_folderish", "q": true},
                  {"k": "b", "i": "is_folderish", "q": null},
                  {"k": "d", "i": "effectiveRange", "t": 29453760},
                  {"k": "d", "i": "effectiveRange", "t": null}],
            "s": [], "l": null, "bs": 0, "bz": null}"#,
    )
    .unwrap();
    let keys: Vec<Vec<Val>> = plan.clauses.iter().map(|c| c.keys.clone()).collect();
    assert_eq!(
        keys,
        vec![
            vec![Val::Bool(true)],
            vec![],
            vec![Val::Int(29453760)],
            vec![Val::None]
        ]
    );
    assert!(matches!(
        read(r#"{"c": [{"k": "b", "i": "x", "q": 1}]}"#),
        Err(PlanError::Malformed(_))
    ));
    assert!(matches!(
        read(r#"{"c": [{"k": "d", "i": "x"}]}"#),
        Err(PlanError::Malformed(_))
    ));
    assert!(matches!(
        read(r#"{"c": [{"k": "d", "i": "x", "t": 1, "not": []}]}"#),
        Err(PlanError::Unknown(_))
    ));
}

const KINDS: [(&str, IndexKind); 8] = [
    ("portal_type", IndexKind::Field),
    ("Subject", IndexKind::Keyword),
    ("modified", IndexKind::Date),
    ("effective", IndexKind::Date),
    (
        "effectiveRange",
        IndexKind::DateRange("effective", "expires"),
    ),
    ("is_folderish", IndexKind::Boolean),
    ("UID", IndexKind::Uuid),
    ("allowedRolesAndUsers", IndexKind::Keyword),
];

const WORDS: [&str; 6] = ["alpha", "beta", "gamma", "delta", "eps", "zeta"];
const TYPES: [&str; 3] = ["Document", "Folder", "News Item"];

/// Catalogs the entry `i` of a model, with values that change with `turn`.
fn add(m: &mut CatalogModel, i: i32, turn: i32) {
    let rid = (i * 7919 + 13) % 100_000 - 50_000;
    let path = format!("/site/item-{i}");
    let k = i + turn;
    let words: Vec<Val> = (0..(k % 4))
        .map(|j| s(WORDS[((k * 3 + j * 5) % 6) as usize]))
        .collect();
    let since = if k % 5 == 0 {
        Val::None
    } else {
        Val::Int(i64::from(1000 + (k * 37) % 400))
    };
    let until = if k % 3 == 0 {
        Val::Int(i64::from(1200 + (k * 53) % 400))
    } else {
        Val::None
    };
    let mut values = vec![
        ("portal_type", s(TYPES[(k % 3) as usize])),
        ("Subject", Val::List(words.into())),
        (
            "allowedRolesAndUsers",
            Val::List(vec![s(if k % 2 == 0 { "Anonymous" } else { "Manager" })].into()),
        ),
        (
            "effectiveRange",
            Val::Tuple(vec![since.clone(), until].into()),
        ),
        ("is_folderish", Val::Bool(k % 4 == 1)),
        ("UID", s(&format!("uid-{i:05}"))),
    ];
    if k % 7 != 0 {
        values.push(("modified", Val::Int(i64::from(2000 + (k * 101) % 900))));
    }
    if let Val::Int(since) = since {
        values.push(("effective", Val::Int(since)));
    }
    m.catalog(rid, &path, &values);
}

fn model_plans() -> Vec<Plan> {
    let mut not_only = clause("Subject", Vec::new());
    not_only.not = vec![s("beta")];
    let mut kw_and = clause("Subject", vec![s("alpha"), s("gamma")]);
    kw_and.and = true;
    let mut kw_range = clause("Subject", Vec::new());
    kw_range.range = Some((Some(s("beta")), Some(s("eps"))));
    let mut date_range = clause("modified", Vec::new());
    date_range.range = Some((Some(Val::Int(2200)), Some(Val::Int(2600))));
    let mut uid_not = clause("UID", Vec::new());
    uid_not.not = vec![s("uid-00003"), s("uid-00010")];
    let docs = || clause("portal_type", vec![s("Document"), s("News Item")]);
    vec![
        plan_of(
            vec![clause("Subject", vec![s("alpha"), s("beta")])],
            &[],
            None,
        ),
        plan_of(vec![kw_and, docs()], &[], None),
        plan_of(vec![not_only, docs()], &[("modified", true)], Some(10)),
        plan_of(vec![kw_range], &[("UID", false)], None),
        plan_of(vec![date_range], &[("effective", false)], Some(25)),
        plan_of(
            vec![clause("is_folderish", vec![Val::Bool(true)])],
            &[],
            None,
        ),
        plan_of(
            vec![clause("is_folderish", vec![Val::Bool(false)]), docs()],
            &[("modified", false)],
            None,
        ),
        plan_of(
            vec![clause("effectiveRange", vec![Val::Int(1250)])],
            &[],
            None,
        ),
        plan_of(
            vec![
                clause("effectiveRange", vec![Val::Int(1390)]),
                clause("allowedRolesAndUsers", vec![s("Anonymous")]),
                docs(),
            ],
            &[("modified", true)],
            Some(10),
        ),
        plan_of(
            vec![clause("UID", vec![s("uid-00004"), s("uid-00100")])],
            &[],
            None,
        ),
        plan_of(vec![uid_not, docs()], &[("UID", true)], Some(7)),
    ]
}

/// Checks each plan of the model at `tid` against a mirror built from
/// scratch at `tid`.
fn check_like_fresh(store: &Store, mirrors: &Mirrors, catalog: u64, tid: u64, what: &str) {
    let built = build(store, catalog, tid, 2, &|_| true).expect("a build");
    let over = Default::default();
    let view = View::new(&built.base, &over);
    for (i, plan) in model_plans().iter().enumerate() {
        let strip = |r: Result<Reply, Reason>| {
            r.map(|mut reply| {
                reply.trace = Default::default();
                reply
            })
        };
        let expected = strip(view.query(plan).map_err(|f| f.reason));
        let got = strip(mirrors.query(catalog, tid + 1, plan).map_err(|f| f.reason));
        assert_eq!(got, expected, "{what}: plan {i}");
        assert!(expected.is_ok(), "{what}: plan {i}: {expected:?}");
    }
}

#[test]
fn the_records_of_each_index_kind_of_the_model_follow_fresh_builds() {
    let store = store();
    let mut m = CatalogModel::new(10, &KINDS, &["getId"]);
    for i in 0..400 {
        add(&mut m, i, 0);
    }
    let first = commit(&store, None, &m.records(), &[m.key()]);
    let config = Config {
        rebase_rows: 150,
        ..Config::default()
    };
    let mirrors = mirrors(&store, config);
    assert!(ready_at(&mirrors, m.oid(), first));
    check_like_fresh(&store, &mirrors, m.oid(), first, "the base");
    let mut tids = vec![first];
    for turn in 1..6 {
        for i in (turn..400).step_by(9) {
            add(&mut m, i, turn);
        }
        for i in 0..30 {
            add(&mut m, 1000 + turn * 100 + i, turn);
        }
        m.uncatalog(&format!("/site/item-{}", 200 + turn));
        let tid = commit(&store, Some(&mirrors), &m.records(), &[]);
        assert!(ready_at(&mirrors, m.oid(), tid));
        check_like_fresh(&store, &mirrors, m.oid(), tid, "a commit");
        tids.push(tid);
    }
    assert!(mirrors.wait_for(m.oid(), WAIT, |s| s.is_some_and(|s| s.rebases >= 1)));
    for tid in &tids {
        check_like_fresh(&store, &mirrors, m.oid(), *tid, "an older tid");
    }
    mirrors.stop();
}

#[test]
fn a_row_without_an_entry_in_a_daterangeindex_gives_date_range_order() {
    let store = store();
    let mut m = CatalogModel::new(10, &KINDS, &["getId"]);
    for i in 0..100 {
        add(&mut m, i, 0);
    }
    let first = commit(&store, None, &m.records(), &[m.key()]);
    let mirrors = mirrors(&store, Config::default());
    assert!(ready_at(&mirrors, m.oid(), first));
    // The entry 3 is a Document; the entry 4 is a Folder.
    m.unindex_from("effectiveRange", "/site/item-3");
    let tid = commit(&store, Some(&mirrors), &m.records(), &[]);
    assert!(ready_at(&mirrors, m.oid(), tid));
    let at = |name: &str, t: i64| {
        plan_of(
            vec![
                clause("effectiveRange", vec![Val::Int(t)]),
                clause("portal_type", vec![s(name)]),
            ],
            &[],
            None,
        )
    };
    assert_eq!(
        mirrors
            .query(m.oid(), tid + 1, &at("Document", 1250))
            .unwrap_err()
            .reason,
        Reason::DateRangeOrder
    );
    // The rows of Folders all have an entry.
    let folders = mirrors
        .query(m.oid(), tid + 1, &at("Folder", 1250))
        .unwrap();
    assert!(!folders.rids.is_empty());
    // A date that the index does not hold, with another clause.
    let none = plan_of(
        vec![
            clause("effectiveRange", vec![Val::None]),
            clause("portal_type", vec![s("Folder")]),
        ],
        &[],
        None,
    );
    assert_eq!(
        mirrors.query(m.oid(), tid + 1, &none).unwrap_err().reason,
        Reason::DateRangeOrder
    );
    // Alone, the clause comes first in the plan of ZCatalog: the row
    // without an entry is not in its result.
    let alone = plan_of(vec![clause("effectiveRange", vec![Val::None])], &[], None);
    let all = mirrors.query(m.oid(), tid + 1, &alone).unwrap();
    assert_eq!(all.rids.len(), 99);
    // The version before the commit holds the entry.
    assert!(
        mirrors
            .query(m.oid(), first + 1, &at("Document", 1250))
            .is_ok()
    );
    mirrors.stop();
}

/// Refuses `UID=uid` at `tid` with `index_unavailable`.
fn refuses_uid(mirrors: &Mirrors, catalog: u64, tid: u64, uid: &str, what: &str) {
    let plan = plan_of(vec![clause("UID", vec![s(uid)])], &[], None);
    assert_eq!(
        mirrors
            .query(catalog, tid + 1, &plan)
            .map_err(|f| f.reason)
            .err(),
        Some(Reason::IndexUnavailable),
        "{what}: a clause"
    );
    let sorted = plan_of(
        vec![clause("portal_type", vec![s("Document")])],
        &[("UID", false)],
        Some(5),
    );
    assert_eq!(
        mirrors
            .query(catalog, tid + 1, &sorted)
            .map_err(|f| f.reason)
            .err(),
        Some(Reason::IndexUnavailable),
        "{what}: a sort"
    );
}

#[test]
fn a_uuidindex_whose_length_differs_from_its_rows_is_refused() {
    // Two ways to a UUID that `_unindex` holds and `_index` lost: the row
    // that `_index` kept for a duplicate leaves, or the duplicate takes a
    // new UUID.
    for holder_leaves in [true, false] {
        let what = if holder_leaves {
            "the holder leaves"
        } else {
            "the duplicate moves on"
        };
        let store = store();
        let mut m = CatalogModel::new(10, &KINDS, &["getId"]);
        for i in 0..50 {
            add(&mut m, i, 0);
        }
        let first = commit(&store, None, &m.records(), &[m.key()]);
        // Each change set past the base starts a rebase.
        let config = Config {
            rebase_rows: 0,
            ..Config::default()
        };
        let mirrors = mirrors(&store, config);
        assert!(ready_at(&mirrors, m.oid(), first));
        let one = plan_of(vec![clause("UID", vec![s("uid-00007")])], &[], None);
        assert_eq!(
            mirrors.query(m.oid(), first + 1, &one).unwrap().rids.len(),
            1
        );
        // The entry 8 takes the UUID of the entry 7, which `_index` keeps.
        let mut values = vec![("portal_type", s("Document")), ("UID", s("uid-00007"))];
        m.catalog(0, "/site/item-8", &values);
        let duplicate = commit(&store, Some(&mirrors), &m.records(), &[]);
        assert!(ready_at(&mirrors, m.oid(), duplicate));
        refuses_uid(&mirrors, m.oid(), duplicate, "uid-00007", "a duplicate");
        if holder_leaves {
            m.uncatalog("/site/item-7");
        } else {
            values[1] = ("UID", s("uid-new"));
            m.catalog(0, "/site/item-8", &values);
        }
        let tid = commit(&store, Some(&mirrors), &m.records(), &[]);
        assert!(ready_at(&mirrors, m.oid(), tid));
        refuses_uid(&mirrors, m.oid(), tid, "uid-00007", what);
        // After a rebase at the last tid, the counts follow each change set.
        let rebased = mirrors.wait_for(m.oid(), WAIT, |s| {
            s.is_some_and(|s| s.rebases >= 1 && s.base_tid == Some(tid))
        });
        assert!(
            rebased,
            "{what}: {:?} {:?}",
            mirrors.status(m.oid()),
            mirrors.take_events()
        );
        refuses_uid(&mirrors, m.oid(), tid, "uid-00007", "after the rebase");
        refuses_uid(
            &mirrors,
            m.oid(),
            duplicate,
            "uid-00007",
            "a duplicate, after the rebase",
        );
        assert_eq!(
            mirrors.query(m.oid(), first + 1, &one).unwrap().rids.len(),
            1
        );
        // A fresh build refuses too.
        let built = build(&*store, m.oid(), tid, 2, &|_| true).expect("a build");
        let over = Default::default();
        assert_eq!(
            View::new(&built.base, &over)
                .query(&one)
                .map_err(|f| f.reason)
                .err(),
            Some(Reason::IndexUnavailable),
            "{what}: a fresh build"
        );
        mirrors.stop();
    }
}

#[test]
fn a_key_of_not_that_the_keys_of_the_index_do_not_order_is_refused() {
    let (_store, mirrors, catalog, tid, _data) = indexes_store(Config::default());
    for (name, key) in [
        ("Subject", Val::Int(1)),
        ("modified", s("a str")),
        ("UID", Val::Int(3)),
    ] {
        let mut with_keys = clause(name, Vec::new());
        with_keys.not = vec![key.clone()];
        let plan = plan_of(
            vec![with_keys, clause("portal_type", vec![s("Document")])],
            &[],
            None,
        );
        assert_eq!(
            mirrors.query(catalog, tid + 1, &plan).unwrap_err().reason,
            Reason::Option,
            "not {key:?} on {name}"
        );
    }
    mirrors.stop();
}
