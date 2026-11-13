//! The acceptance test of the order clause (ADR 0059, its amendment): a
//! listing of one folder of plone.folder, sorted on its
//! `GopipIndex`, answered by the mirror in the order of `documentToKeyMap`.
//!
//! `tests/data/order.json` holds the folders of plone.folder and the
//! catalog entries of their items after each commit, the records of the
//! order and of the positions as `DefaultOrdering` wrote them, the plans of
//! a set of listings, and the answers of ZCatalog to them.
//! `tests/data/order.py` writes it. The test keeps the catalog in a
//! `CatalogModel` with the rids of the data, and writes the orders with the
//! writers of `testing`, which give the bytes of the data.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value as Json;
use zoe_catalog::testing::{
    self, CatalogModel, IndexKind, Pickle, Py, annotations_record, order_record, positions_record,
};
use zoe_catalog::value::Val;
use zoe_catalog::{
    Config, Fallback, Mirrors, Opener, PlanError, Reads, Reason, Reply, Wire, decode_ordered,
    decode_typed,
};
use zoe_core::Store;
use zoe_core::vfs::MemVfs;

const WAIT: Duration = Duration::from_secs(20);

const GOPIP: &str = "getObjPositionInParent";

const INDEXES: [(&str, IndexKind); 3] = [
    ("path", IndexKind::Path),
    ("portal_type", IndexKind::Field),
    (
        GOPIP,
        IndexKind::Other("plone.folder.nogopip", "GopipIndex"),
    ),
];

const COLUMNS: [&str; 1] = ["getId"];

/// The folders of the data.
const FOLDERS: [&str; 4] = ["/site", "/site/f0", "/site/f1", "/site/f0/sub"];

fn data() -> Json {
    let path = format!("{}/tests/data/order.json", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(path).expect("the test data");
    serde_json::from_str(&text).expect("JSON")
}

fn hex(text: &str) -> Vec<u8> {
    (0..text.len() / 2)
        .map(|i| u8::from_str_radix(&text[2 * i..2 * i + 2], 16).expect("hex"))
        .collect()
}

fn wire(json: &Json) -> Wire {
    match json {
        Json::Null => Wire::Nil,
        Json::Bool(b) => Wire::Bool(*b),
        Json::Number(n) => Wire::Int(n.as_i64().expect("an int")),
        Json::String(s) => Wire::Str(s.clone()),
        Json::Array(items) => Wire::Array(items.iter().map(wire).collect()),
        Json::Object(map) => Wire::Map(
            map.iter()
                .map(|(k, v)| (Wire::Str(k.clone()), wire(v)))
                .collect(),
        ),
    }
}

fn store() -> Arc<Store> {
    Arc::new(Store::open_vfs(MemVfs::new()).expect("a store"))
}

fn mirrors(store: &Arc<Store>) -> Mirrors {
    let weak = Arc::downgrade(store);
    let open: Opener = Arc::new(move || weak.upgrade().map(|s| s as Arc<dyn Reads>));
    Mirrors::start(open, store.last_transaction().0, Config::default())
}

fn ready_at(mirrors: &Mirrors, catalog: u64, tid: u64) -> bool {
    mirrors.wait_for(catalog, WAIT, |s| {
        s.is_some_and(|s| s.state == "ready" && s.applied >= Some(tid))
    })
}

/// The oids of the annotations, the order and the positions of a folder.
fn oids(folder: &str) -> (u64, u64, u64) {
    let k = FOLDERS.iter().position(|f| *f == folder).expect("a folder") as u64;
    let base = 1_000_000 + 10 * k;
    (base, base + 1, base + 2)
}

fn strs(json: &Json) -> Vec<&str> {
    json.as_array()
        .expect("an array")
        .iter()
        .map(|s| s.as_str().expect("a str"))
        .collect()
}

fn positions(json: &Json) -> Vec<(&str, i64)> {
    json.as_array()
        .expect("positions")
        .iter()
        .map(|p| {
            (
                p[0].as_str().expect("an id"),
                p[1].as_i64().expect("a place"),
            )
        })
        .collect()
}

/// The records of the annotations, the order and the positions of
/// `folder` in `state`.
fn folder_records(state: &Json, folder: &str) -> Vec<(u64, Vec<u8>)> {
    let (a, l, p) = oids(folder);
    vec![
        (a, annotations_record(l, p)),
        (l, order_record(&strs(&state["orders"][folder]))),
        (p, positions_record(&positions(&state["positions"][folder]))),
    ]
}

/// Changes the catalog of the model to the entries of `state`, as
/// `catalogObject` and `uncatalogObject` did.
fn apply(m: &mut CatalogModel, state: &Json) {
    let wanted: BTreeMap<String, (i32, String)> = state["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .map(|e| {
            (
                e[1].as_str().expect("a path").to_owned(),
                (
                    e[0].as_i64().expect("a rid") as i32,
                    e[2].as_str().expect("a type").to_owned(),
                ),
            )
        })
        .collect();
    for (rid, path) in m.entries() {
        if wanted.get(&path).map(|w| w.0) != Some(rid) {
            m.uncatalog(&path);
        }
    }
    for (path, (rid, kind)) in &wanted {
        if m.rid(path) != Some(*rid) {
            let id = path.rsplit('/').next().expect("an id");
            m.catalog(
                *rid,
                path,
                &[
                    ("portal_type", Val::Str(kind.as_str().into())),
                    ("getId", Val::Str(id.into())),
                ],
            );
        }
    }
}

/// The records of the catalog and of the orders of `state`.
fn records(m: &mut CatalogModel, state: &Json) -> Vec<(u64, Vec<u8>)> {
    apply(m, state);
    let mut out = m.records();
    for folder in FOLDERS {
        out.extend(folder_records(state, folder));
    }
    out
}

/// The order clause of the order of `folder` at `oid`.
fn order_clause(index: &str, oid: &[u8], path: &str) -> Wire {
    Wire::Map(vec![
        (Wire::Str("i".into()), Wire::Str(index.into())),
        (Wire::Str("oid".into()), Wire::Bin(oid.to_vec())),
        (Wire::Str("p".into()), Wire::Str(path.into())),
    ])
}

/// The plan of `query` of the data, with the order clause of its
/// container when `order` is true.
fn plan(query: &Json, order: bool) -> Wire {
    let mut items = Vec::new();
    for key in ["c", "s", "l", "bs", "bz"] {
        items.push((Wire::Str(key.into()), wire(&query[key])));
    }
    items.push((Wire::Str("cols".into()), Wire::Nil));
    items.push((Wire::Str("rn".into()), Wire::Int(0)));
    if order {
        let container = query["container"].as_str().expect("a container");
        let (_, l, _) = oids(container);
        items.push((
            Wire::Str("o".into()),
            order_clause(GOPIP, &l.to_be_bytes(), container),
        ));
    }
    Wire::Map(items)
}

/// Replaces the key `key` of a plan.
fn with(plan: &Wire, key: &str, value: Wire) -> Wire {
    let Wire::Map(items) = plan else {
        panic!("a plan")
    };
    let mut items: Vec<(Wire, Wire)> = items
        .iter()
        .filter(|(k, _)| *k != Wire::Str(key.into()))
        .cloned()
        .collect();
    items.push((Wire::Str(key.into()), value));
    Wire::Map(items)
}

fn answer(
    mirrors: &Mirrors,
    catalog: u64,
    tid: u64,
    plan: &Wire,
    overlay: Option<&[(u64, Vec<u8>)]>,
) -> Result<Reply, Reason> {
    mirrors
        .query_wire_overlay(catalog, tid + 1, plan, overlay)
        .expect("a plan of the form of section 8")
        .map_err(|f: Fallback| f.reason)
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

/// Checks each query of the data at `tid` against the answers of `state`.
fn check_state(mirrors: &Mirrors, catalog: u64, tid: u64, data: &Json, state: &Json) {
    let name = state["name"].as_str().expect("a name");
    for (query, plan_json) in data["queries"].as_object().expect("queries") {
        let got = answer(mirrors, catalog, tid, &plan(plan_json, true), None);
        same_answer(&got, &state["answers"][query], &format!("{name} {query}"));
    }
}

/// The store with the catalog and the orders of the states of the data up
/// to `last`, one commit each, and its mirror ready. Returns the store, the
/// mirrors, the model and the tid of each state.
fn prepared(data: &Json, last: &str) -> (Arc<Store>, Mirrors, CatalogModel, Vec<u64>) {
    let store = store();
    let mut m = CatalogModel::new(10, &INDEXES, &COLUMNS);
    let mut tids = Vec::new();
    let mut started: Option<Mirrors> = None;
    for state in data["states"].as_array().expect("states") {
        let found = records(&mut m, state);
        let keys = if tids.is_empty() {
            vec![m.key()]
        } else {
            Vec::new()
        };
        let tid = testing::commit(&store, &found, &keys, &[]).expect("a commit");
        match &started {
            Some(mirrors) => mirrors.committed(tid),
            None => started = Some(mirrors(&store)),
        }
        let mirrors = started.as_ref().expect("the mirrors");
        assert!(ready_at(mirrors, m.oid(), tid));
        tids.push(tid);
        if state["name"] == last {
            break;
        }
    }
    (store, started.expect("the mirrors"), m, tids)
}

#[test]
fn the_writers_give_the_records_of_default_ordering() {
    let data = data();
    for state in data["states"].as_array().expect("states") {
        let name = state["name"].as_str().expect("a name");
        let records = &state["records"];
        let oid = |what: &str| records[what]["oid"].as_u64().expect("an oid");
        let bytes = |what: &str| hex(records[what]["record"].as_str().expect("hex"));
        assert_eq!(
            order_record(&strs(&state["orders"]["/site/f0"])),
            bytes("order"),
            "{name}: the order"
        );
        assert_eq!(
            positions_record(&positions(&state["positions"]["/site/f0"])),
            bytes("positions"),
            "{name}: the positions"
        );
        assert_eq!(
            annotations_record(oid("order"), oid("positions")),
            bytes("annotations"),
            "{name}: the annotations"
        );
    }
}

#[test]
fn each_listing_gives_the_order_of_document_to_key_map_after_each_commit() {
    let data = data();
    let states = data["states"].as_array().expect("states");
    let last = states.last().expect("a state")["name"]
        .as_str()
        .expect("a name");
    let (_store, mirrors, m, tids) = prepared(&data, last);
    assert_eq!(tids.len(), states.len());
    for (state, tid) in states.iter().zip(&tids) {
        check_state(&mirrors, m.oid(), *tid, &data, state);
    }
    // A query at an older tid reads the order of that tid.
    for (state, tid) in states.iter().zip(&tids).rev() {
        check_state(&mirrors, m.oid(), *tid, &data, state);
    }
    // The commits moved ids: the listing of f0 changed.
    let f0 = |i: usize| &states[i]["answers"]["f0"]["r"];
    assert_ne!(f0(0), f0(1));
    // An id without a place (the ghost) and the rid 0 leave the result and
    // the count.
    assert_eq!(states[0]["answers"]["f0-batch-past"]["a"], 32);
    assert_eq!(states[0]["answers"]["f0"]["a"], 31);
    assert_eq!(states[0]["answers"]["f1"]["a"], 12);
    mirrors.stop();
}

#[test]
fn the_order_wins_when_the_positions_disagree() {
    let data = data();
    let states = data["states"].as_array().expect("states");
    let k = states
        .iter()
        .position(|s| s["name"] == "disagree")
        .expect("the state disagree");
    let state = &states[k];
    // The positions give another order than the order of the folder.
    assert_ne!(state["by_positions"], state["answers"]["f0"]["r"]);
    let (_store, mirrors, m, tids) = prepared(&data, "disagree");
    check_state(&mirrors, m.oid(), tids[k], &data, state);
    let got = answer(
        &mirrors,
        m.oid(),
        tids[k],
        &plan(&data["queries"]["f0"], true),
        None,
    );
    let rids: Vec<i64> = got
        .expect("an answer")
        .rids
        .iter()
        .map(|r| i64::from(*r))
        .collect();
    assert_ne!(Json::from(rids), state["by_positions"]);
    mirrors.stop();
}

#[test]
fn an_overlay_that_moves_ids_gives_the_order_of_the_transaction() {
    let data = data();
    let states = data["states"].as_array().expect("states");
    let k = states
        .iter()
        .position(|s| s["name"] == "disagree")
        .expect("the state disagree");
    let (before, after) = (&states[k], &states[k + 1]);
    let (_store, mirrors, m, tids) = prepared(&data, "disagree");
    let catalog = m.oid();
    let tid = tids[k];
    let moved = folder_records(after, "/site/f0");
    let (_, l, _) = oids("/site/f0");
    for (query, plan_json) in data["queries"].as_object().expect("queries") {
        let p = plan(plan_json, true);
        // The records of the transaction: the annotations, the order and
        // the positions; or the order alone.
        let got = answer(&mirrors, catalog, tid, &p, Some(&moved));
        same_answer(&got, &after["answers"][query], &format!("overlay {query}"));
        let order: Vec<(u64, Vec<u8>)> = moved.iter().filter(|r| r.0 == l).cloned().collect();
        let got = answer(&mirrors, catalog, tid, &p, Some(&order));
        same_answer(&got, &after["answers"][query], &format!("order {query}"));
        // Without the overlay, the order of the tid.
        let got = answer(&mirrors, catalog, tid, &p, None);
        same_answer(
            &got,
            &before["answers"][query],
            &format!("no overlay {query}"),
        );
    }
    assert_ne!(before["answers"]["f0"], after["answers"]["f0"]);
    // An order of the overlay that does not decode gives `overlay`.
    let p = plan(&data["queries"]["f0"], true);
    let bad = vec![(l, b"not a pickle".to_vec())];
    assert_eq!(
        answer(&mirrors, catalog, tid, &p, Some(&bad)),
        Err(Reason::Overlay)
    );
    mirrors.stop();
}

#[test]
fn an_item_that_is_not_a_str_keeps_its_place_and_names_no_row() {
    let data = data();
    let (store, mirrors, m, tids) = prepared(&data, "base");
    let state = &data["states"][0];
    let ids = strs(&state["orders"]["/site/f0"]);
    let mut p = Pickle::default();
    let mut items: Vec<Py> = ids.iter().map(|id| p.str(id)).collect();
    items.insert(3, Py::Int(7));
    items.insert(0, Py::None);
    let list = p.list(items);
    let key = p.str("data");
    let state_py = p.dict(vec![(key, list)]);
    let record = p.record("persistent.list", "PersistentList", state_py);
    let (_, l, _) = oids("/site/f0");
    let tid = testing::commit(&store, &[(l, record)], &[], &[]).expect("a commit");
    mirrors.committed(tid);
    assert!(ready_at(&mirrors, m.oid(), tid));
    for query in ["f0", "f0-reverse", "f0-limit", "f0-batch-end"] {
        let p = plan(&data["queries"][query], true);
        let got = answer(&mirrors, m.oid(), tid, &p, None);
        same_answer(&got, &state["answers"][query], query);
    }
    let _ = tids;
    mirrors.stop();
}

#[test]
fn the_refusals_of_an_order_clause() {
    let data = data();
    let (_store, mirrors, m, tids) = prepared(&data, "base");
    let catalog = m.oid();
    let tid = tids[0];
    let f0 = plan(&data["queries"]["f0"], true);
    let (_, l, _) = oids("/site/f0");
    // A result with several containers gives `sort`.
    let several = plan(&data["queries"]["f0-depth-2"], true);
    assert_eq!(
        answer(&mirrors, catalog, tid, &several, None),
        Err(Reason::Sort)
    );
    // A container that is not the parent of the rows gives `sort`.
    let other = with(&f0, "o", order_clause(GOPIP, &l.to_be_bytes(), "/site/f1"));
    assert_eq!(
        answer(&mirrors, catalog, tid, &other, None),
        Err(Reason::Sort)
    );
    // An order clause on an index that is not a GopipIndex gives `sort`.
    let field = with(
        &with(
            &f0,
            "s",
            Wire::Array(vec![Wire::Array(vec![
                Wire::Str("portal_type".into()),
                Wire::Bool(false),
            ])]),
        ),
        "o",
        order_clause("portal_type", &l.to_be_bytes(), "/site/f0"),
    );
    assert_eq!(
        answer(&mirrors, catalog, tid, &field, None),
        Err(Reason::Sort)
    );
    // An oid without a record before the tid, and a record of another
    // class, give `index_unavailable`.
    let none = with(
        &f0,
        "o",
        order_clause(GOPIP, &999_999_999u64.to_be_bytes(), "/site/f0"),
    );
    assert_eq!(
        answer(&mirrors, catalog, tid, &none, None),
        Err(Reason::IndexUnavailable)
    );
    let (a, _, _) = oids("/site/f0");
    let class = with(&f0, "o", order_clause(GOPIP, &a.to_be_bytes(), "/site/f0"));
    assert_eq!(
        answer(&mirrors, catalog, tid, &class, None),
        Err(Reason::IndexUnavailable)
    );
    // An unknown key of the order clause gives `plan`.
    let Wire::Map(mut clause) = order_clause(GOPIP, &l.to_be_bytes(), "/site/f0") else {
        unreachable!()
    };
    clause.push((Wire::Str("x".into()), Wire::Int(1)));
    let unknown = with(&f0, "o", Wire::Map(clause));
    assert_eq!(
        answer(&mirrors, catalog, tid, &unknown, None),
        Err(Reason::Plan)
    );
    // A plan with an order clause is unknown to a reader without it, so a
    // server that does not know it replies `plan`.
    assert!(matches!(decode_typed(&f0), Err(PlanError::Unknown(_))));
    assert!(decode_ordered(&f0).is_ok_and(|(_, _, _, o)| o.is_some_and(|o| o.oid == l)));
    // A malformed order clause: a sort of two indexes, an oid that is not 8
    // bytes, a clause without its path.
    let two = with(
        &f0,
        "s",
        Wire::Array(vec![
            Wire::Array(vec![Wire::Str(GOPIP.into()), Wire::Bool(false)]),
            Wire::Array(vec![Wire::Str("portal_type".into()), Wire::Bool(false)]),
        ]),
    );
    let short = with(&f0, "o", order_clause(GOPIP, &[0; 7], "/site/f0"));
    let pathless = with(
        &f0,
        "o",
        Wire::Map(vec![
            (Wire::Str("i".into()), Wire::Str(GOPIP.into())),
            (Wire::Str("oid".into()), Wire::Bin(l.to_be_bytes().to_vec())),
        ]),
    );
    for bad in [&two, &short, &pathless] {
        assert!(matches!(decode_ordered(bad), Err(PlanError::Malformed(_))));
        assert!(mirrors.query_wire(catalog, tid + 1, bad).is_err());
    }
    // Without the order clause, a sort on the GopipIndex is not served.
    let bare = plan(&data["queries"]["f0"], false);
    assert!(answer(&mirrors, catalog, tid, &bare, None).is_err());
    mirrors.stop();
}
