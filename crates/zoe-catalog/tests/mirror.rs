//! The acceptance tests of the catalog mirror with FieldIndexes (ADR 0059).
//!
//! `tests/data/zcatalog.json` holds the records of a catalog that
//! Products.ZCatalog wrote, the records of three commits, the plans of a
//! set of queries and the answers of ZCatalog after the base and after
//! each commit. `tests/data/zcatalog.py` writes it.

use std::collections::HashMap;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value as Json;
use zoe_catalog::build::build;
use zoe_catalog::query::View;
use zoe_catalog::testing::{
    self, CatalogModel, II_SET, IO, IndexKind, Item, OI, OO, Pickle, Py, TreeRecord, bucket_record,
    catalog_record, index_record, length_record, tree_record,
};
use zoe_catalog::value::Val;
use zoe_catalog::{
    Config, Hooks, Mirrors, Opener, Plan, PlanError, Reads, Reason, Reply, Wire, decode_plan,
};
use zoe_core::Store;
use zoe_core::vfs::MemVfs;
use zoe_pickle::btrees::{self, TreeState};
use zoe_pickle::python::{Heap, Object, ObjectId, Value};

const WAIT: Duration = Duration::from_secs(20);

fn data() -> Json {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/data/zcatalog.json"
    ))
    .expect("the test data");
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

/// Commits `records` and the keys, and hands the commit to the mirrors.
fn commit(
    store: &Store,
    mirrors: Option<&Mirrors>,
    records: &[(u64, Vec<u8>)],
    register: &[[u8; 8]],
    unregister: &[[u8; 8]],
) -> u64 {
    let tid = testing::commit(store, records, register, unregister).expect("a commit");
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

fn state_is(mirrors: &Mirrors, catalog: u64, state: &str) -> bool {
    mirrors.wait_for(catalog, WAIT, |s| s.is_some_and(|s| s.state == state))
}

fn same_answer(reply: &Reply, expected: &Json, what: &str) {
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

/// The answer of a mirror built from scratch at `tid`.
fn fresh(store: &Store, catalog: u64, tid: u64, plan: &Plan) -> Result<Reply, Reason> {
    let built = build(store, catalog, tid, 2, &|_| true).expect("a build");
    let over = Default::default();
    View::new(&built.base, &over)
        .query(plan)
        .map_err(|f| f.reason)
}

fn field_plan(clauses: &[(&str, Vec<Val>)], sort: &[(&str, bool)], limit: Option<i64>) -> Plan {
    Plan {
        clauses: clauses
            .iter()
            .map(|(index, keys)| zoe_catalog::Clause {
                index: (*index).to_owned(),
                keys: keys.clone(),
                and: false,
                range: None,
                not: Vec::new(),
            })
            .collect(),
        sort: sort
            .iter()
            .map(|(i, r)| ((*i).to_owned(), Some(*r)))
            .collect(),
        limit,
        b_start: 0,
        b_size: None,
    }
}

fn s(text: &str) -> Val {
    Val::Str(text.into())
}

/// A catalog of the model with `n` entries.
fn model(first_oid: u64, n: i32) -> CatalogModel {
    model_with(first_oid, n, None)
}

/// A catalog of the model with `n` entries, whose inner nodes split past
/// `fanout` children.
fn model_with(first_oid: u64, n: i32, fanout: Option<usize>) -> CatalogModel {
    let mut m = CatalogModel::with_fanout(
        first_oid,
        &[
            ("portal_type", IndexKind::Field),
            ("review_state", IndexKind::Field),
            ("num", IndexKind::Field),
            ("sortable_title", IndexKind::Field),
            (
                "Subject",
                IndexKind::Other(
                    "Products.PluginIndexes.KeywordIndex.KeywordIndex",
                    "KeywordIndex",
                ),
            ),
        ],
        &["getId"],
        fanout,
    );
    for i in 0..n {
        add(&mut m, i, 0);
    }
    m
}

const TYPES: [&str; 3] = ["Document", "Folder", "News Item"];
const STATES: [&str; 3] = ["private", "pending", "published"];

/// Catalogs the entry `i` of a model, with values that change with `turn`.
fn add(m: &mut CatalogModel, i: i32, turn: i32) {
    let rid = (i * 7919 + 13) % 100_000 - 50_000;
    let path = format!("/site/item-{i}");
    let mut values = vec![
        ("portal_type", s(TYPES[((i + turn) % 3) as usize])),
        ("review_state", s(STATES[((i * 2 + turn) % 3) as usize])),
        ("getId", s(&format!("item-{i}"))),
        (
            "sortable_title",
            s(&format!("title {:03}", (i * 31 + turn) % 97)),
        ),
    ];
    if (i + turn) % 5 != 0 {
        values.push(("num", Val::Int(i64::from((i * 13 + turn * 7) % 50))));
    }
    m.catalog(rid, &path, &values);
}

/// Some plans over the indexes of the model.
fn model_plans() -> Vec<Plan> {
    let mut range = field_plan(
        &[("portal_type", vec![s("Document"), s("Folder")])],
        &[],
        None,
    );
    range.clauses.push(zoe_catalog::Clause {
        index: "num".into(),
        keys: Vec::new(),
        and: false,
        range: Some((Some(Val::Int(5)), Some(Val::Int(30)))),
        not: Vec::new(),
    });
    let mut not_only = field_plan(&[], &[("sortable_title", false)], Some(15));
    not_only.clauses.push(zoe_catalog::Clause {
        index: "review_state".into(),
        keys: Vec::new(),
        and: false,
        range: None,
        not: vec![s("published")],
    });
    vec![
        field_plan(&[("portal_type", vec![s("Document")])], &[], None),
        field_plan(
            &[("review_state", vec![s("pending"), s("private")])],
            &[("num", true)],
            Some(7),
        ),
        field_plan(
            &[("portal_type", vec![s("Folder")])],
            &[("review_state", false)],
            None,
        ),
        field_plan(
            &[("portal_type", vec![s("News Item")])],
            &[("sortable_title", true)],
            None,
        ),
        field_plan(
            &[("portal_type", vec![s("Document"), s("News Item")])],
            &[("portal_type", false), ("num", true)],
            Some(40),
        ),
        range,
        not_only,
    ]
}

fn check_like_fresh(store: &Store, mirrors: &Mirrors, catalog: u64, tid: u64, what: &str) {
    let built = build(store, catalog, tid, 2, &|_| true).expect("a build");
    let over = Default::default();
    let view = View::new(&built.base, &over);
    for (i, plan) in model_plans().iter().enumerate() {
        let expected = view.query(plan).map_err(|f| f.reason);
        let got = mirrors.query(catalog, tid + 1, plan).map_err(|f| f.reason);
        let strip = |r: Result<Reply, Reason>| {
            r.map(|mut reply| {
                reply.trace = Default::default();
                reply
            })
        };
        assert_eq!(strip(got), strip(expected), "{what}: plan {i}");
    }
}

/// A gate that a hook waits on until the test opens it.
#[derive(Clone)]
struct Gate {
    open: Arc<(Mutex<bool>, std::sync::Condvar)>,
    reached: Arc<Mutex<Option<mpsc::Sender<u64>>>>,
}

impl Gate {
    fn new() -> (Gate, mpsc::Receiver<u64>) {
        let (tx, rx) = mpsc::channel();
        (
            Gate {
                open: Arc::new((Mutex::new(false), std::sync::Condvar::new())),
                reached: Arc::new(Mutex::new(Some(tx))),
            },
            rx,
        )
    }

    fn hook(&self) -> zoe_catalog::mirrors::Hook {
        let gate = self.clone();
        Arc::new(move |n| {
            if let Some(tx) = gate.reached.lock().unwrap().as_ref() {
                let _ = tx.send(n);
            }
            let (open, changed) = &*gate.open;
            let mut open = open.lock().unwrap();
            while !*open {
                open = changed.wait(open).unwrap();
            }
        })
    }

    fn release(&self) {
        let (open, changed) = &*self.open;
        *open.lock().unwrap() = true;
        changed.notify_all();
    }
}

#[test]
fn testing_writes_the_records_that_zcatalog_writes() {
    let data = data();
    let mut checked: HashMap<String, usize> = HashMap::new();
    let mut generic = 0;
    for (oid, bytes) in records(&data["base"]) {
        let record = zoe_catalog::read::Record::decode(&bytes).expect("a record");
        let heap = &record.heap;
        let class = format!("{}.{}", record.module, record.name);
        // The pickler writes each record as ZODB's ObjectWriter does.
        let mut p = Pickle::default();
        let state = to_py(heap, record.state, &mut p, &mut HashMap::new());
        assert_eq!(
            p.record(&record.module, &record.name, state),
            bytes,
            "{oid:#x} of {class}"
        );
        generic += 1;
        // The writers of the records of a catalog.
        let rebuilt = match class.as_str() {
            "Products.ZCatalog.Catalog.Catalog" => {
                let schema = heap.dict(record.attr("schema").unwrap()).unwrap();
                let columns: Vec<&str> =
                    schema.iter().map(|(k, _)| heap.str(*k).unwrap()).collect();
                let indexes: Vec<(&str, u64, &str, &str)> = heap
                    .dict(record.attr("indexes").unwrap())
                    .unwrap()
                    .iter()
                    .map(|(k, v)| {
                        let (oid, module, name) = reference(heap, *v);
                        (heap.str(*k).unwrap(), oid, module, name)
                    })
                    .collect();
                catalog_record(
                    &columns,
                    &indexes,
                    record.attr_oid("data").unwrap(),
                    record.attr_oid("uids").unwrap(),
                    record.attr_oid("paths").unwrap(),
                    record.attr_oid("_length").unwrap(),
                )
            }
            "Products.PluginIndexes.FieldIndex.FieldIndex.FieldIndex"
            | "Products.PluginIndexes.KeywordIndex.KeywordIndex.KeywordIndex" => index_record(
                &record.module,
                &record.name,
                heap.str(record.attr("id").unwrap()).unwrap(),
                record.attr_oid("_length").unwrap(),
                record.attr_oid("_index").unwrap(),
                record.attr_oid("_unindex").unwrap(),
                record.attr_oid("_counter").unwrap(),
            ),
            "BTrees.Length.Length" => length_record(record.int_state().unwrap()),
            _ => {
                let Some(family) = [IO, OI, OO, II_SET].into_iter().find(|f| {
                    f.module == record.module && (f.tree == record.name || f.bucket == record.name)
                }) else {
                    continue;
                };
                // A value that the record holds twice, such as one str of
                // a constant, is one object in CPython: the writers of the
                // model make each value anew.
                if shares(heap, record.state) {
                    continue;
                }
                if family.tree == record.name {
                    let state = match btrees::tree_state(heap, record.state).unwrap() {
                        TreeState::Empty => TreeRecord::Empty,
                        TreeState::Inline(bucket) => {
                            TreeRecord::Inline(items(heap, family.set, bucket))
                        }
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
                    tree_record(family, &state)
                } else {
                    let state = btrees::bucket_state(heap, record.state, !family.set).unwrap();
                    let next = state.next.map(|n| reference(heap, n).0);
                    bucket_record(family, &items(heap, family.set, record.state), next)
                }
            }
        };
        assert_eq!(rebuilt, bytes, "the record {oid:#x} of {class}");
        *checked.entry(class).or_default() += 1;
    }
    assert!(generic > 1000, "{generic} records");
    for class in [
        "Products.ZCatalog.Catalog.Catalog",
        "Products.PluginIndexes.FieldIndex.FieldIndex.FieldIndex",
        "BTrees.Length.Length",
        "BTrees.IOBTree.IOBTree",
        "BTrees.IOBTree.IOBucket",
        "BTrees.OIBTree.OIBTree",
        "BTrees.OIBTree.OIBucket",
        "BTrees.OOBTree.OOBTree",
        "BTrees.IIBTree.IITreeSet",
    ] {
        assert!(
            checked.get(class).is_some_and(|n| *n > 0),
            "no record of {class}: {checked:?}"
        );
    }
}

/// The value of a heap as a value of `Pickle`: one object for each object
/// of the heap, and a new tuple for each persistent reference.
fn to_py(heap: &Heap, value: Value, p: &mut Pickle, seen: &mut HashMap<ObjectId, Py>) -> Py {
    let id = match value {
        Value::None => return Py::None,
        Value::Bool(b) => return Py::Bool(b),
        Value::Int(i) => return Py::Int(i64::try_from(i).unwrap()),
        Value::Float(f) => return Py::Float(f),
        Value::Object(id) => id,
    };
    if let Some(py) = seen.get(&id) {
        return *py;
    }
    let py = match heap.get(id) {
        Object::Str(text) => p.str(text),
        Object::Bytes(bytes) => p.val(&Val::Bytes(bytes.as_slice().into())),
        Object::Tuple(items) => {
            let items = items.iter().map(|v| to_py(heap, *v, p, seen)).collect();
            p.tuple(items)
        }
        Object::List(items) => {
            let items = items.iter().map(|v| to_py(heap, *v, p, seen)).collect();
            p.list(items)
        }
        Object::Dict(items) => {
            let items = items
                .iter()
                .map(|(k, v)| (to_py(heap, *k, p, seen), to_py(heap, *v, p, seen)))
                .collect();
            p.dict(items)
        }
        Object::Class { module, name } => p.class(module, name),
        Object::Reference(_) => {
            let (oid, module, name) = reference(heap, value);
            return p.reference(oid, module, name);
        }
        other => panic!("an object the test data does not hold: {other:?}"),
    };
    seen.insert(id, py);
    py
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

/// The oid and the class of a persistent reference.
fn reference(heap: &Heap, value: Value) -> (u64, &'static str, &'static str) {
    let r = heap.reference(value).expect("a reference");
    let oid = u64::from_be_bytes(r.oid.as_slice().try_into().unwrap());
    let parts = heap.tuple(r.data).unwrap();
    let (module, name) = heap.class(parts[1]).unwrap();
    (oid, leak(module), leak(name))
}

fn leak(text: &str) -> &'static str {
    Box::leak(text.to_owned().into_boxed_str())
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
                        Item::Ref(oid, module, name)
                    }
                    _ => Item::Val(Val::read(heap, pair[1]).unwrap()),
                }
            };
            (key, value)
        })
        .collect()
}

#[test]
fn a_store_with_a_registration_key_builds_its_catalog_after_the_open() {
    let store = store();
    let mut m = model(10, 200);
    let tid = commit(&store, None, &m.records(), &[m.key()], &[]);
    let mirrors = mirrors(&store, Config::default());
    assert!(ready_at(&mirrors, m.oid(), tid));
    check_like_fresh(&store, &mirrors, m.oid(), tid, "after the open");
    mirrors.stop();
}

#[test]
fn a_store_without_a_registration_key_builds_nothing() {
    let store = store();
    let mut m = model(10, 50);
    let tid = commit(&store, None, &m.records(), &[], &[]);
    let mirrors = mirrors(&store, Config::default());
    std::thread::sleep(Duration::from_millis(200));
    assert!(mirrors.catalogs().is_empty());
    let plan = model_plans().remove(0);
    let refused = mirrors.query(m.oid(), tid + 1, &plan).unwrap_err();
    assert_eq!(refused.reason, Reason::NotRegistered);
    mirrors.stop();
}

#[test]
fn a_commit_that_adds_a_key_builds_and_one_that_removes_it_drops() {
    let store = store();
    let mut a = model(10, 60);
    let mut b = model(10_000, 60);
    let mut records = a.records();
    records.extend(b.records());
    let tid = commit(&store, None, &records, &[a.key()], &[]);
    let mirrors = mirrors(&store, Config::default());
    assert!(ready_at(&mirrors, a.oid(), tid));
    assert_eq!(mirrors.status(b.oid()), None);
    let tid = commit(&store, Some(&mirrors), &[], &[b.key()], &[]);
    assert!(ready_at(&mirrors, b.oid(), tid));
    check_like_fresh(&store, &mirrors, b.oid(), tid, "the added catalog");
    let tid = commit(&store, Some(&mirrors), &[], &[], &[a.key()]);
    assert!(mirrors.wait_for(a.oid(), WAIT, |s| s.is_none()));
    let plan = model_plans().remove(0);
    assert_eq!(
        mirrors.query(a.oid(), tid + 1, &plan).unwrap_err().reason,
        Reason::NotRegistered
    );
    mirrors.stop();
}

#[test]
fn a_key_whose_oid_holds_no_catalog_is_refused() {
    let store = store();
    let mut m = model(10, 20);
    let not_a_catalog = m.unindex_root("num");
    let tid = commit(
        &store,
        None,
        &m.records(),
        &[not_a_catalog.to_be_bytes()],
        &[],
    );
    let mirrors = mirrors(&store, Config::default());
    assert!(state_is(&mirrors, not_a_catalog, "refused"));
    let plan = model_plans().remove(0);
    assert_eq!(
        mirrors
            .query(not_a_catalog, tid + 1, &plan)
            .unwrap_err()
            .reason,
        Reason::Refused
    );
    assert!(
        mirrors
            .take_events()
            .iter()
            .any(|e| e.contains("not a Catalog"))
    );
    mirrors.stop();
}

#[test]
fn uids_that_is_not_the_inverse_of_paths_is_refused() {
    let store = store();
    let mut m = model(10, 20);
    let mut records = m.records();
    // uids of the catalog, with the rid of another path for one path.
    let entries = m.entries();
    let mut items: Vec<(Val, Item)> = entries
        .iter()
        .map(|(rid, path)| (s(path), Item::Val(Val::Int(i64::from(*rid)))))
        .collect();
    items.sort_by(|a, b| a.0.text().cmp(&b.0.text()));
    items[3].1 = items[4].1.clone();
    let uids = m.oid() + 2;
    for (oid, data) in &mut records {
        if *oid == uids {
            *data = tree_record(OI, &TreeRecord::Inline(items.clone()));
        }
    }
    commit(&store, None, &records, &[m.key()], &[]);
    let mirrors = mirrors(&store, Config::default());
    assert!(state_is(&mirrors, m.oid(), "refused"));
    let reason = mirrors.status(m.oid()).unwrap().reason.unwrap();
    assert!(
        reason.contains("uids is not the inverse of paths"),
        "{reason}"
    );
    mirrors.stop();
}

#[test]
fn a_commit_during_the_build_is_in_the_mirror_when_it_is_ready() {
    let store = store();
    let mut m = model(10, 200);
    commit(&store, None, &m.records(), &[m.key()], &[]);
    let (gate, reached) = Gate::new();
    let config = Config {
        hooks: Hooks {
            before_build: Some(gate.hook()),
            ..Hooks::default()
        },
        ..Config::default()
    };
    let mirrors = mirrors(&store, config);
    reached.recv_timeout(WAIT).expect("the build starts");
    for i in 0..30 {
        add(&mut m, i, 1);
    }
    m.uncatalog("/site/item-40");
    let during = commit(&store, Some(&mirrors), &m.records(), &[], &[]);
    let plan = model_plans().remove(0);
    assert_eq!(
        mirrors
            .query(m.oid(), during + 1, &plan)
            .unwrap_err()
            .reason,
        Reason::NotReady
    );
    gate.release();
    assert!(ready_at(&mirrors, m.oid(), during));
    check_like_fresh(&store, &mirrors, m.oid(), during, "after the build");
    mirrors.stop();
}

#[test]
fn a_query_at_a_tid_after_later_commits_gives_the_result_of_that_tid() {
    let store = store();
    let mut m = model(10, 250);
    let first = commit(&store, None, &m.records(), &[m.key()], &[]);
    let mirrors = mirrors(&store, Config::default());
    assert!(ready_at(&mirrors, m.oid(), first));
    let mut tids = vec![first];
    for turn in 1..6 {
        for i in (turn..250).step_by(9) {
            add(&mut m, i, turn);
        }
        m.uncatalog(&format!("/site/item-{}", turn * 11));
        add(&mut m, 1000 + turn, turn);
        tids.push(commit(&store, Some(&mirrors), &m.records(), &[], &[]));
    }
    assert!(ready_at(&mirrors, m.oid(), *tids.last().unwrap()));
    for tid in &tids {
        check_like_fresh(&store, &mirrors, m.oid(), *tid, &format!("at {tid:#x}"));
    }
    mirrors.stop();
}

/// Loads the base of the test data, and starts the mirrors.
fn zcatalog_store(config: Config) -> (Arc<Store>, Mirrors, u64, u64, Json) {
    let data = data();
    let store = store();
    let catalog = u64::from_str_radix(data["catalog"].as_str().unwrap(), 16).unwrap();
    let tid = commit(
        &store,
        None,
        &records(&data["base"]),
        &[catalog.to_be_bytes()],
        &[],
    );
    let mirrors = mirrors(&store, config);
    assert!(ready_at(&mirrors, catalog, tid));
    (store, mirrors, catalog, tid, data)
}

fn check_answers(mirrors: &Mirrors, catalog: u64, tid: u64, data: &Json, at: usize) {
    let plans = data["plans"].as_object().unwrap();
    for (name, plan_json) in plans {
        let reply = mirrors
            .query(catalog, tid + 1, &plan(plan_json))
            .unwrap_or_else(|f| panic!("{name} at {at}: {f:?}"));
        same_answer(
            &reply,
            &data["answers"][name][at],
            &format!("{name} at {at}"),
        );
    }
}

#[test]
fn the_answers_equal_zcatalogs_after_the_base_a_split_and_a_removal() {
    let (store, mirrors, catalog, tid, data) = zcatalog_store(Config::default());
    check_answers(&mirrors, catalog, tid, &data, 0);
    let commits = data["commits"].as_array().unwrap();
    let mut tids = vec![tid];
    for (at, c) in commits.iter().take(2).enumerate() {
        let tid = commit(&store, Some(&mirrors), &records(&c["records"]), &[], &[]);
        assert!(ready_at(&mirrors, catalog, tid));
        check_answers(&mirrors, catalog, tid, &data, at + 1);
        tids.push(tid);
    }
    // A query at an older tid gives the answers of that tid.
    check_answers(&mirrors, catalog, tids[0], &data, 0);
    check_answers(&mirrors, catalog, tids[1], &data, 1);
    mirrors.stop();
}

#[test]
fn ties_follow_zcatalog_on_each_of_its_four_sort_paths() {
    let (_store, mirrors, catalog, tid, data) = zcatalog_store(Config::default());
    for name in [
        "iterate-index",
        "iterate-index-desc",
        "resultset",
        "resultset-desc",
        "resultset-limit",
        "nbest",
        "nbest-missing",
        "nbest-reverse",
        "nbest-num",
        "multisort-mixed",
        "multisort-same",
        "batch-past-middle",
    ] {
        let reply = mirrors
            .query(catalog, tid + 1, &plan(&data["plans"][name]))
            .expect(name);
        same_answer(&reply, &data["answers"][name][0], name);
    }
    mirrors.stop();
}

#[test]
fn generic_numbers_tests_the_key_of_each_row_of_the_other_clauses() {
    let (_store, mirrors, catalog, tid, data) = zcatalog_store(Config::default());
    let plan = plan(&data["plans"]["generic-numbers"]);
    let range = &plan
        .clauses
        .iter()
        .find(|c| c.index == "num")
        .unwrap()
        .range;
    assert_eq!(range, &Some((Some(Val::Int(50)), Some(Val::Int(2950)))));
    let reply = mirrors.query(catalog, tid + 1, &plan).unwrap();
    same_answer(
        &reply,
        &data["answers"]["generic-numbers"][0],
        "generic-numbers",
    );
    assert_eq!(reply.trace.range_by_rows, 1);
    assert_eq!(reply.trace.range_by_keys, 0);
    // Without the other clauses, the range takes the rows of its keys.
    let alone = Plan {
        clauses: plan
            .clauses
            .into_iter()
            .filter(|c| c.index == "num")
            .collect(),
        ..Plan::default()
    };
    let reply = mirrors.query(catalog, tid + 1, &alone).unwrap();
    assert_eq!(reply.trace.range_by_keys, 1);
    mirrors.stop();
}

#[test]
fn a_new_root_of_a_mirrored_tree_makes_the_mirror_stale_and_it_builds_again() {
    let (gate, reached) = Gate::new();
    let blocked = Arc::new(Mutex::new(false));
    let hook = gate.hook();
    let block = Arc::clone(&blocked);
    let config = Config {
        hooks: Hooks {
            before_build: Some(Arc::new(move |n| {
                if *block.lock().unwrap() {
                    hook(n);
                }
            })),
            ..Hooks::default()
        },
        ..Config::default()
    };
    let (store, mirrors, catalog, _, data) = zcatalog_store(config);
    let commits = data["commits"].as_array().unwrap();
    let mut tid = 0;
    for c in &commits[..2] {
        tid = commit(&store, Some(&mirrors), &records(&c["records"]), &[], &[]);
    }
    assert!(ready_at(&mirrors, catalog, tid));
    *blocked.lock().unwrap() = true;
    let before = tid;
    let clear = commit(
        &store,
        Some(&mirrors),
        &records(&commits[2]["records"]),
        &[],
        &[],
    );
    reached.recv_timeout(WAIT).expect("the build starts again");
    let name = "iterate-index";
    let p = plan(&data["plans"][name]);
    assert_eq!(
        mirrors.query(catalog, clear + 1, &p).unwrap_err().reason,
        Reason::Stale
    );
    assert_eq!(
        mirrors.query(catalog, before + 1, &p).unwrap_err().reason,
        Reason::NotReady
    );
    gate.release();
    assert!(ready_at(&mirrors, catalog, clear));
    check_answers(&mirrors, catalog, clear, &data, 3);
    assert!(mirrors.take_events().iter().any(|e| e.contains("stale")));
    mirrors.stop();
}

#[test]
fn past_rebase_rows_the_mirror_rebases_and_each_query_equals_a_fresh_build() {
    let store = store();
    let mut m = model(10, 300);
    let first = commit(&store, None, &m.records(), &[m.key()], &[]);
    let (gate, reached) = Gate::new();
    let config = Config {
        rebase_rows: 40,
        hooks: Hooks {
            before_swap: Some(gate.hook()),
            ..Hooks::default()
        },
        ..Config::default()
    };
    let mirrors = mirrors(&store, config);
    assert!(ready_at(&mirrors, m.oid(), first));
    let mut tids = vec![first];
    for turn in 1..4 {
        for i in (turn..300).step_by(7) {
            add(&mut m, i, turn);
        }
        add(&mut m, 2000 + turn, turn);
        m.uncatalog(&format!("/site/item-{}", 100 + turn));
        tids.push(commit(&store, Some(&mirrors), &m.records(), &[], &[]));
    }
    reached.recv_timeout(WAIT).expect("a rebase");
    // During the rebase.
    assert!(ready_at(&mirrors, m.oid(), *tids.last().unwrap()));
    assert_eq!(mirrors.status(m.oid()).unwrap().rebases, 0);
    for tid in &tids {
        check_like_fresh(&store, &mirrors, m.oid(), *tid, "during the rebase");
    }
    gate.release();
    assert!(mirrors.wait_for(m.oid(), WAIT, |s| s.is_some_and(|s| s.rebases >= 1)));
    // After the rebase, also at the tids before its base, and after more
    // commits.
    for turn in 4..6 {
        for i in (turn..300).step_by(11) {
            add(&mut m, i, turn);
        }
        tids.push(commit(&store, Some(&mirrors), &m.records(), &[], &[]));
    }
    assert!(ready_at(&mirrors, m.oid(), *tids.last().unwrap()));
    for tid in &tids {
        check_like_fresh(&store, &mirrors, m.oid(), *tid, "after the rebase");
    }
    mirrors.stop();
}

#[test]
fn a_tid_outside_the_window_is_refused_and_the_last_tid_of_an_idle_store_is_served() {
    let store = store();
    let mut m = model(10, 100);
    let first = commit(&store, None, &m.records(), &[m.key()], &[]);
    let config = Config {
        window_tids: 2,
        rebase_rows: 0,
        ..Config::default()
    };
    let mirrors = mirrors(&store, config);
    assert!(ready_at(&mirrors, m.oid(), first));
    let plan = model_plans().remove(0);
    // A tid before the build.
    assert_eq!(
        mirrors.query(m.oid(), first, &plan).unwrap_err().reason,
        Reason::OutsideWindow
    );
    let mut tids = vec![first];
    for turn in 1..7 {
        add(&mut m, turn, turn);
        tids.push(commit(&store, Some(&mirrors), &m.records(), &[], &[]));
        assert!(ready_at(&mirrors, m.oid(), *tids.last().unwrap()));
    }
    let last = *tids.last().unwrap();
    assert!(mirrors.wait_for(m.oid(), WAIT, |s| {
        s.is_some_and(|s| s.floor >= Some(tids[3]) && s.sets <= 3)
    }));
    assert_eq!(
        mirrors
            .query(m.oid(), tids[1] + 1, &plan)
            .unwrap_err()
            .reason,
        Reason::OutsideWindow
    );
    check_like_fresh(&store, &mirrors, m.oid(), tids[5], "in the window");
    // The idle store: its last tid, and a snapshot after it.
    std::thread::sleep(Duration::from_millis(300));
    check_like_fresh(&store, &mirrors, m.oid(), last, "the last tid");
    assert!(mirrors.query(m.oid(), u64::MAX, &plan).is_ok());
    mirrors.stop();
}

#[test]
fn a_query_at_a_tid_that_the_mirror_did_not_apply_waits_then_gets_lag() {
    let store = store();
    let mut m = model(10, 100);
    let first = commit(&store, None, &m.records(), &[m.key()], &[]);
    let (gate, reached) = Gate::new();
    let config = Config {
        wait: Duration::from_secs(1),
        hooks: Hooks {
            before_apply: Some(gate.hook()),
            ..Hooks::default()
        },
        ..Config::default()
    };
    let mirrors = Arc::new(mirrors(&store, config));
    assert!(ready_at(&mirrors, m.oid(), first));
    add(&mut m, 1, 1);
    let tid = commit(&store, Some(&mirrors), &m.records(), &[], &[]);
    reached.recv_timeout(WAIT).expect("the apply starts");
    let plan = model_plans().remove(0);
    assert_eq!(
        mirrors.query(m.oid(), tid + 1, &plan).unwrap_err().reason,
        Reason::Lag
    );
    // The query before the commit does not wait.
    assert!(mirrors.query(m.oid(), first + 1, &plan).is_ok());
    // A query that the mirror reaches within the wait is served.
    let waiting = {
        let mirrors = Arc::clone(&mirrors);
        let plan = plan.clone();
        let catalog = m.oid();
        std::thread::spawn(move || mirrors.query(catalog, tid + 1, &plan))
    };
    std::thread::sleep(Duration::from_millis(50));
    gate.release();
    assert!(waiting.join().unwrap().is_ok());
    check_like_fresh(&store, &mirrors, m.oid(), tid, "after the wait");
    mirrors.stop();
}

#[test]
fn an_option_that_the_mirror_does_not_serve_gives_a_refusal() {
    let store = store();
    let mut m = model(10, 100);
    let tid = commit(&store, None, &m.records(), &[m.key()], &[]);
    let mirrors = mirrors(&store, Config::default());
    assert!(ready_at(&mirrors, m.oid(), tid));
    let refusal = |plan: &Plan| mirrors.query(m.oid(), tid + 1, plan).unwrap_err().reason;
    let docs = || vec![("portal_type", vec![s("Document")])];
    assert_eq!(
        refusal(&field_plan(&docs(), &[("Subject", false)], None)),
        Reason::IndexUnavailable
    );
    assert_eq!(
        refusal(&field_plan(&[("Subject", vec![s("x")])], &[], None)),
        Reason::IndexUnavailable
    );
    assert_eq!(
        refusal(&field_plan(&[("nothere", vec![s("x")])], &[], None)),
        Reason::IndexUnavailable
    );
    assert_eq!(
        refusal(&field_plan(&docs(), &[("num", true)], Some(0))),
        Reason::Sort
    );
    let mut no_order = field_plan(&docs(), &[("num", true)], None);
    no_order.sort[0].1 = None;
    assert_eq!(refusal(&no_order), Reason::Sort);
    let mut and_range = field_plan(&[], &[], None);
    and_range.clauses.push(zoe_catalog::Clause {
        index: "num".into(),
        keys: Vec::new(),
        and: true,
        range: Some((Some(Val::Int(1)), None)),
        not: Vec::new(),
    });
    assert_eq!(refusal(&and_range), Reason::AndRange);
    let mut bad_bound = field_plan(&[], &[], None);
    bad_bound.clauses.push(zoe_catalog::Clause {
        index: "num".into(),
        keys: Vec::new(),
        and: false,
        range: Some((Some(s("a str")), None)),
        not: Vec::new(),
    });
    assert_eq!(refusal(&bad_bound), Reason::Option);
    // A plan with a kind or a key that this version does not know.
    let unknown = |text: &str| {
        let json: Json = serde_json::from_str(text).unwrap();
        decode_plan(&wire(&json)).unwrap_err()
    };
    assert!(matches!(
        unknown(r#"{"c": [{"k": "p", "i": "path"}]}"#),
        PlanError::Unknown(_)
    ));
    assert!(matches!(
        unknown(r#"{"c": [{"k": "v", "i": "num", "x": 1}]}"#),
        PlanError::Unknown(_)
    ));
    assert!(matches!(
        unknown(r#"{"c": [], "o": 1}"#),
        PlanError::Unknown(_)
    ));
    assert!(matches!(
        unknown(r#"{"c": [], "cols": ["Title"]}"#),
        PlanError::Unknown(_)
    ));
    assert!(matches!(unknown(r#"{"s": []}"#), PlanError::Malformed(_)));
    assert!(matches!(
        unknown(r#"{"c": [{"k": "v"}]}"#),
        PlanError::Malformed(_)
    ));
    assert!(matches!(unknown(r#"{"c": 3}"#), PlanError::Malformed(_)));
    mirrors.stop();
}

#[test]
fn a_record_that_the_decoder_refuses_makes_its_index_unavailable_only() {
    let store = store();
    let mut m = model(10, 100);
    let broken = m.unindex_root("num");
    let mut records = m.records();
    for (oid, data) in &mut records {
        if *oid == broken {
            data.truncate(data.len() / 2);
        }
    }
    let tid = commit(&store, None, &records, &[m.key()], &[]);
    let mirrors = mirrors(&store, Config::default());
    assert!(ready_at(&mirrors, m.oid(), tid));
    let on_num = field_plan(&[("num", vec![Val::Int(3)])], &[], None);
    assert_eq!(
        mirrors.query(m.oid(), tid + 1, &on_num).unwrap_err().reason,
        Reason::IndexUnavailable
    );
    let sorted_on_num = field_plan(
        &[("portal_type", vec![s("Folder")])],
        &[("num", false)],
        None,
    );
    assert_eq!(
        mirrors
            .query(m.oid(), tid + 1, &sorted_on_num)
            .unwrap_err()
            .reason,
        Reason::IndexUnavailable
    );
    let other = field_plan(
        &[("portal_type", vec![s("Folder")])],
        &[("sortable_title", false)],
        None,
    );
    assert_eq!(
        mirrors
            .query(m.oid(), tid + 1, &other)
            .map(|r| r.rids)
            .map_err(|f| f.reason),
        fresh(&store, m.oid(), tid, &other).map(|r| r.rids)
    );
    mirrors.stop();
}

#[test]
fn past_catalog_memory_the_mirror_that_answered_least_recently_drops() {
    let store = store();
    let mut a = model(10, 400);
    let mut b = model(100_000, 400);
    let mut records = a.records();
    records.extend(b.records());
    let tid = commit(&store, None, &records, &[a.key()], &[]);
    // Measure one mirror, then bound the memory to one and a half.
    let probe = mirrors(&store, Config::default());
    assert!(ready_at(&probe, a.oid(), tid));
    let one = probe.status(a.oid()).unwrap().bytes;
    probe.stop();
    let config = Config {
        memory: one * 3 / 2,
        ..Config::default()
    };
    let mirrors = mirrors(&store, config);
    assert!(ready_at(&mirrors, a.oid(), tid));
    let plan = model_plans().remove(0);
    assert!(mirrors.query(a.oid(), tid + 1, &plan).is_ok());
    let tid = commit(&store, Some(&mirrors), &[], &[b.key()], &[]);
    assert!(ready_at(&mirrors, b.oid(), tid));
    assert!(state_is(&mirrors, a.oid(), "dropped"));
    assert_eq!(
        mirrors.query(a.oid(), tid + 1, &plan).unwrap_err().reason,
        Reason::Dropped
    );
    assert!(mirrors.query(b.oid(), tid + 1, &plan).is_ok());
    let counts = mirrors.counts();
    assert_eq!(counts.drops, 1);
    assert!(counts.fallbacks.contains(&(Reason::Dropped, 1)));
    assert!(counts.native >= 2);
    // The removal of the other key frees the room: the dropped mirror
    // builds again.
    let tid = commit(&store, Some(&mirrors), &[], &[], &[b.key()]);
    assert!(ready_at(&mirrors, a.oid(), tid));
    mirrors.stop();
}

#[test]
fn a_split_of_an_inner_node_or_of_a_root_keeps_the_rows_of_the_moved_buckets() {
    let store = store();
    let mut m = model_with(10, 100, Some(4));
    let first = commit(&store, None, &m.records(), &[m.key()], &[]);
    let at_base = m.splits();
    let mirrors = mirrors(&store, Config::default());
    assert!(ready_at(&mirrors, m.oid(), first));
    let mut tids = vec![first];
    for turn in 1..11 {
        for i in 0..60 {
            add(&mut m, 100 + turn * 60 + i, turn);
        }
        for i in (turn..100).step_by(13) {
            add(&mut m, i, turn);
        }
        // A removal in a bucket that a split moved.
        m.uncatalog(&format!("/site/item-{}", turn * 7));
        m.uncatalog(&format!("/site/item-{}", 100 + turn * 30));
        tids.push(commit(&store, Some(&mirrors), &m.records(), &[], &[]));
    }
    let (below, roots) = m.splits();
    assert!(
        below > at_base.0,
        "no split of an inner node: {:?}",
        m.splits()
    );
    assert!(roots > at_base.1, "no split of a root: {:?}", m.splits());
    assert!(ready_at(&mirrors, m.oid(), *tids.last().unwrap()));
    assert_eq!(mirrors.status(m.oid()).unwrap().stale_at, None);
    for tid in &tids {
        check_like_fresh(&store, &mirrors, m.oid(), *tid, &format!("at {tid:#x}"));
    }
    mirrors.stop();
}
