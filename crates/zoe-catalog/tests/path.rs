//! The acceptance tests of the catalog mirror with an ExtendedPathIndex
//! (ADR 0059, sections 3, 7 and 8).
//!
//! `tests/data/path.json` holds the records of a catalog with an
//! ExtendedPathIndex that Products.ZCatalog wrote, the records of seven
//! commits, the plans of a set of path queries and the answers of ZCatalog
//! after the base and after each commit, or the reason of the refusal of
//! the mirror. `tests/data/path.py` writes it.

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value as Json;
use zoe_catalog::build::build;
use zoe_catalog::query::View;
use zoe_catalog::testing::{
    self, CatalogModel, II, II_SET, IO, IndexKind, Item, OI, OO, PathOids, TreeRecord,
    bucket_record, extended_path_index_record, index_record, length_record, tree_record,
};
use zoe_catalog::value::Val;
use zoe_catalog::{
    Config, Mirrors, Opener, Plan, PlanError, Reads, Reason, Reply, Wire, decode_plan,
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
fn testing_writes_the_records_of_an_extendedpathindex_that_zcatalog_writes() {
    let data = data("path.json");
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
            "ExtendedPathIndex" => extended_path_index_record(
                id(),
                &PathOids {
                    depth: match record.attr("_depth") {
                        Some(Value::Int(i)) => i64::try_from(i).unwrap(),
                        other => panic!("_depth {other:?}"),
                    },
                    index: oid_of("_index"),
                    unindex: oid_of("_unindex"),
                    length: oid_of("_length"),
                    parents: oid_of("_index_parents"),
                    items: oid_of("_index_items"),
                    multi_valued: match record.attr("multi_valued") {
                        Some(Value::Bool(b)) => b,
                        other => panic!("multi_valued {other:?}"),
                    },
                },
            ),
            "FieldIndex" => index_record(
                &record.module,
                &record.name,
                id(),
                oid_of("_length"),
                oid_of("_index"),
                oid_of("_unindex"),
                oid_of("_counter"),
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
        "Products.ExtendedPathIndex.ExtendedPathIndex.ExtendedPathIndex",
        "BTrees.OOBTree.OOBTree",
        "BTrees.OOBTree.OOBucket",
        "BTrees.IOBTree.IOBTree",
        "BTrees.IOBTree.IOBucket",
        "BTrees.OIBTree.OIBTree",
        "BTrees.OIBTree.OIBucket",
        "BTrees.IIBTree.IITreeSet",
        "BTrees.IIBTree.IISet",
    ] {
        assert!(
            checked.get(class).is_some_and(|n| *n > 0),
            "no record of {class}: {checked:?}"
        );
    }
}

/// A store with the base of `path.json`, its mirrors, the catalog and the
/// tid of the base.
fn path_store(config: Config) -> (Arc<Store>, Mirrors, u64, u64, Json) {
    let data = data("path.json");
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
    for (name, plan_json) in data["plans"].as_object().unwrap() {
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
    let (store, mirrors, catalog, tid, data) = path_store(Config::default());
    check_answers(&mirrors, catalog, tid, &data, 0, "the base");
    let mut tids = vec![tid];
    for (at, c) in data["commits"].as_array().unwrap().iter().enumerate() {
        let sets = mirrors.status(catalog).unwrap().sets;
        let tid = commit(&store, Some(&mirrors), &records(&c["records"]), &[]);
        assert!(ready_at(&mirrors, catalog, tid));
        // A move of a folder of three levels is one commit, so one change
        // set holds each row of its subtree.
        assert_eq!(
            mirrors.status(catalog).unwrap().sets,
            sets + 1,
            "{}",
            c["name"]
        );
        check_answers(&mirrors, catalog, tid, &data, at + 1, "the last tid");
        tids.push(tid);
    }
    let status = mirrors.status(catalog).unwrap();
    assert_eq!((status.rebases, status.stale_at), (0, None));
    // A query at an older tid reads the overrides of that tid.
    for (at, tid) in tids.iter().enumerate() {
        check_answers(&mirrors, catalog, *tid, &data, at, "an older tid");
    }
    mirrors.stop();
}

#[test]
fn a_rebase_of_the_path_index_gives_the_answers_of_each_tid() {
    let config = Config {
        rebase_rows: 1,
        ..Config::default()
    };
    let (store, mirrors, catalog, tid, data) = path_store(config);
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

/// The JSON of a path clause on `path` with `options` over the defaults.
fn path_json(paths: Json, options: Json) -> Json {
    let mut clause = serde_json::json!({
        "k": "p", "i": "path", "q": paths, "lv": 0, "and": false, "d": -1,
        "nt": false, "ns": 0, "ip": ["site", "portal_catalog", "path"],
    });
    for (k, v) in options.as_object().unwrap() {
        clause[k] = v.clone();
    }
    serde_json::json!({"c": [clause], "s": [], "l": null, "bs": 0, "bz": null})
}

#[test]
fn the_plan_reads_the_path_clause_and_refuses_another_form() {
    let read = |json: Json| decode_plan(&wire(&json));
    let plan = read(path_json(
        serde_json::json!([["/site/a", null], ["b", 2]]),
        serde_json::json!({"lv": 1, "and": true, "d": 2, "nt": true, "ns": 1}),
    ))
    .unwrap();
    let clause = &plan.clauses[0];
    assert!(clause.and);
    let s = |text: &str| Val::Str(text.into());
    let tuple = |items: Vec<Val>| Val::Tuple(items.into());
    assert_eq!(
        clause.keys,
        vec![
            tuple(vec![
                Val::Int(1),
                Val::Int(2),
                Val::Bool(true),
                Val::Int(1),
                tuple(vec![s("site"), s("portal_catalog"), s("path")]),
            ]),
            tuple(vec![s("/site/a"), Val::None]),
            tuple(vec![s("b"), Val::Int(2)]),
        ]
    );
    let malformed = |json: Json| matches!(read(json), Err(PlanError::Malformed(_)));
    assert!(malformed(
        serde_json::json!({"c": [{"k": "p", "i": "path"}]})
    ));
    assert!(malformed(path_json(
        serde_json::json!([["/a"]]),
        serde_json::json!({})
    )));
    assert!(malformed(path_json(
        serde_json::json!([[1, null]]),
        serde_json::json!({})
    )));
    assert!(malformed(path_json(
        serde_json::json!([]),
        serde_json::json!({"d": "1"})
    )));
    assert!(malformed(path_json(
        serde_json::json!([]),
        serde_json::json!({"nt": 1})
    )));
    assert!(malformed(path_json(
        serde_json::json!([]),
        serde_json::json!({"ip": [1]})
    )));
    assert!(matches!(
        read(path_json(
            serde_json::json!([]),
            serde_json::json!({"not": []})
        )),
        Err(PlanError::Unknown(_))
    ));
}

#[test]
fn a_depth_below_minus_one_a_negative_navtree_start_and_a_sort_on_path_are_refused() {
    let (_store, mirrors, catalog, tid, _data) = path_store(Config::default());
    let refusal = |json: Json| {
        let plan = decode_plan(&wire(&json)).unwrap();
        mirrors
            .query(catalog, tid + 1, &plan)
            .map_err(|f| f.reason)
            .err()
    };
    let one = || serde_json::json!([["/site/folder-0", null]]);
    assert_eq!(
        refusal(path_json(one(), serde_json::json!({"d": -2}))),
        Some(Reason::Option)
    );
    assert_eq!(
        refusal(path_json(one(), serde_json::json!({"nt": true, "ns": -1}))),
        Some(Reason::Option)
    );
    // Without navtree, navtree_start has no effect.
    assert_eq!(
        refusal(path_json(one(), serde_json::json!({"ns": -1}))),
        None
    );
    // `_index_items`: a search at the level 0 with a depth of 0.
    assert_eq!(
        refusal(path_json(one(), serde_json::json!({"d": 0}))),
        Some(Reason::IndexUnavailable)
    );
    let mut sorted = path_json(one(), serde_json::json!({}));
    sorted["s"] = serde_json::json!([["path", false]]);
    assert_eq!(refusal(sorted), Some(Reason::Sort));
    // A clause of keys on the path index.
    let keys = serde_json::json!({"c": [{"k": "v", "i": "path", "q": ["/site"], "and": false,
        "r": null, "not": []}], "s": [], "l": null, "bs": 0, "bz": null});
    assert_eq!(refusal(keys), Some(Reason::Option));
    mirrors.stop();
}

const KINDS: [(&str, IndexKind); 2] =
    [("path", IndexKind::Path), ("portal_type", IndexKind::Field)];

fn s(text: &str) -> Val {
    Val::Str(text.into())
}

/// Catalogs the entry at `path` of a model with a rid of its own.
fn add(m: &mut CatalogModel, path: &str) {
    let mut h: i64 = 17;
    for b in path.bytes() {
        h = (h * 131 + i64::from(b)) % 1_000_003;
    }
    let rid = (h as i32 % 100_000) * 7 - 350_000 + path.len() as i32;
    let kind = if path.ends_with(|c: char| c.is_ascii_digit()) {
        "Document"
    } else {
        "Folder"
    };
    m.catalog(rid, path, &[("portal_type", s(kind))]);
}

/// The paths of a folder `top`: `n` folders of `n` folders of `n` items.
fn subtree(top: &str, n: usize) -> Vec<String> {
    let mut out = vec![top.to_owned()];
    for a in 0..n {
        let a = format!("{top}/a{a}");
        out.push(a.clone());
        for b in 0..n {
            let b = format!("{a}/b{b}");
            out.push(b.clone());
            for c in 0..n {
                out.push(format!("{b}/item-{c}"));
            }
        }
    }
    out
}

/// Moves the entries of the subtree at `old` to `new`, as CMFCore does.
fn move_subtree(m: &mut CatalogModel, old: &str, new: &str) {
    let moved: Vec<String> = m
        .entries()
        .into_iter()
        .map(|(_, p)| p)
        .filter(|p| p == old || p.starts_with(&format!("{old}/")))
        .collect();
    for path in &moved {
        m.uncatalog(path);
    }
    for path in &moved {
        add(m, &format!("{new}{}", &path[old.len()..]));
    }
}

fn model_plans() -> Vec<Plan> {
    let p = |paths: Json, options: Json| decode_plan(&wire(&path_json(paths, options))).unwrap();
    let one = |path: &str| serde_json::json!([[path, null]]);
    vec![
        p(one("/site/f1"), serde_json::json!({})),
        p(one("/site/f1"), serde_json::json!({"d": 1})),
        p(one("/site/f2/a1"), serde_json::json!({"d": 2})),
        p(one("/site"), serde_json::json!({"d": 2})),
        p(one("/site"), serde_json::json!({})),
        p(one("b1"), serde_json::json!({"lv": -1})),
        p(one("a0"), serde_json::json!({"lv": 2, "d": 1})),
        p(
            one("/site/f0/a1/b2/item-1"),
            serde_json::json!({"nt": true}),
        ),
        p(
            one("/site/f3/a2/b0"),
            serde_json::json!({"nt": true, "ns": 2}),
        ),
        p(
            one("/site/f1/a1/b1"),
            serde_json::json!({"nt": true, "d": 2}),
        ),
        p(
            serde_json::json!([["/site/f0", null], ["/site/f3/a0", null]]),
            serde_json::json!({"d": 999}),
        ),
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
fn moves_of_subtrees_in_the_model_follow_fresh_builds() {
    let store = store();
    let mut m = CatalogModel::with_fanout(10, &KINDS, &["getId"], Some(4));
    for f in 0..4 {
        for path in subtree(&format!("/site/f{f}"), 3) {
            add(&mut m, &path);
        }
    }
    let first = commit(&store, None, &m.records(), &[m.key()]);
    let config = Config {
        rebase_rows: 60,
        ..Config::default()
    };
    let mirrors = mirrors(&store, config);
    assert!(ready_at(&mirrors, m.oid(), first));
    check_like_fresh(&store, &mirrors, m.oid(), first, "the base");
    let mut tids = vec![first];
    let steps: [&dyn Fn(&mut CatalogModel); 5] = [
        // A folder of three levels moves to another folder, at the same
        // depth: `_depth` stays.
        &|m| move_subtree(m, "/site/f0/a1", "/site/f2/moved"),
        &|m| move_subtree(m, "/site/f1/a2/b0", "/site/f1/a2/renamed"),
        &|m| {
            for (_, path) in m.entries() {
                if path.starts_with("/site/f3/") {
                    m.uncatalog(&path);
                }
            }
        },
        &|m| {
            for path in subtree("/site/f3", 2) {
                add(m, &path);
            }
        },
        &|m| move_subtree(m, "/site/f2/moved", "/site/f0/a1"),
    ];
    for step in steps {
        step(&mut m);
        let tid = commit(&store, Some(&mirrors), &m.records(), &[]);
        assert!(ready_at(&mirrors, m.oid(), tid));
        check_like_fresh(&store, &mirrors, m.oid(), tid, "a commit");
        tids.push(tid);
    }
    assert!(mirrors.wait_for(m.oid(), WAIT, |s| s.is_some_and(|s| s.rebases >= 1)));
    assert_eq!(mirrors.status(m.oid()).unwrap().stale_at, None);
    for tid in &tids {
        check_like_fresh(&store, &mirrors, m.oid(), *tid, "an older tid");
    }
    mirrors.stop();
}

#[test]
fn a_path_deeper_than_depth_changes_the_index_record_and_builds_again() {
    let store = store();
    let mut m = CatalogModel::new(10, &KINDS, &["getId"]);
    for path in subtree("/site/f0", 2) {
        add(&mut m, &path);
    }
    let first = commit(&store, None, &m.records(), &[m.key()]);
    let mirrors = mirrors(&store, Config::default());
    assert!(ready_at(&mirrors, m.oid(), first));
    let plans = model_plans();
    let before = mirrors.query(m.oid(), first + 1, &plans[3]).unwrap();
    // `_depth` grows: the commit writes the index record.
    move_subtree(&mut m, "/site/f0/a1", "/site/f0/a0/b0/deeper");
    let records = m.records();
    let depth = records
        .iter()
        .filter_map(|(_, data)| zoe_catalog::read::Record::decode(data).ok())
        .find(|r| r.name == "ExtendedPathIndex")
        .and_then(|r| r.attr("_depth"));
    assert!(matches!(depth, Some(Value::Int(6))), "{depth:?}");
    let tid = commit(&store, Some(&mirrors), &records, &[]);
    assert!(mirrors.wait_for(m.oid(), WAIT, |s| {
        s.is_some_and(|s| s.state == "ready" && s.base_tid >= Some(tid))
    }));
    check_like_fresh(&store, &mirrors, m.oid(), tid, "the new build");
    // The new build holds no version before its tid.
    assert_eq!(
        mirrors
            .query(m.oid(), first + 1, &plans[3])
            .map_err(|f| f.reason)
            .err(),
        Some(Reason::OutsideWindow)
    );
    assert!(!before.rids.is_empty());
    mirrors.stop();
}

#[test]
fn a_rebase_after_any_set_of_commits_gives_the_answers_of_each_tid() {
    use zoe_catalog::change::{ChangeSet, read_records};
    use zoe_catalog::mirror::{Versions, rebase};
    let data = data("path.json");
    let store = store();
    let catalog = u64::from_str_radix(data["catalog"].as_str().unwrap(), 16).unwrap();
    let first = commit(
        &store,
        None,
        &records(&data["base"]),
        &[catalog.to_be_bytes()],
    );
    let commits: Vec<Vec<(u64, Option<Vec<u8>>)>> = data["commits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            records(&c["records"])
                .into_iter()
                .map(|(oid, bytes)| (oid, Some(bytes)))
                .collect()
        })
        .collect();
    // Bit `k` of `mask`: a rebase after the commit `k`. A rebase gives a
    // rid of the change sets a row of its own, whatever the rows before.
    for mask in 0u32..1 << commits.len() {
        let built = build(&*store, catalog, first, 2, &|_| true).unwrap();
        let (mut shapes, objects) = (built.shapes, built.objects);
        let mut versions = Versions::new(built.base);
        for (k, records) in commits.iter().enumerate() {
            let read = read_records(&versions.base, &versions.head, &shapes, &objects, records);
            assert_eq!(read.stale, None);
            shapes.take(read.edits);
            versions.push(ChangeSet {
                tid: first + 1 + k as u64,
                at: std::time::Instant::now(),
                changes: read.changes,
                lengths: read.lengths,
            });
            if mask & (1 << k) != 0 {
                let tid = versions.applied;
                let base = rebase(&versions.base, &versions.head, tid, &|_| true).unwrap();
                versions.rebased(base);
            }
        }
        for at in 0..=commits.len() {
            let (base, over) = versions.snapshot(first + at as u64).unwrap().resolve();
            let view = View::new(&base, &over);
            for (name, plan_json) in data["plans"].as_object().unwrap() {
                let got = view.query(&plan(plan_json)).map_err(|f| f.reason);
                same_answer(
                    &got,
                    &data["answers"][name][at],
                    &format!("{name} at {at}, rebases {mask:b}"),
                );
            }
        }
    }
}
