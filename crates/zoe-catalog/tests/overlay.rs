//! The acceptance test of the transaction overlay (ADR 0059, section 8):
//! the records that a transaction changed and did not commit, read by the
//! mirror over the version of one query.
//!
//! A model of a catalog changes, adds and removes entries without a
//! commit. Its new objects take provisional oids, as the client names
//! them. A query with these records as its overlay gives the answer of the
//! mirror after a commit of the same records. The overlay changes neither
//! the change sets nor the base, and a query without it gives the answer
//! without it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{Value as Json, json};
use zoe_catalog::build::build;
use zoe_catalog::mirrors::Hook;
use zoe_catalog::overlay::{PROVISIONAL, is_provisional};
use zoe_catalog::query::View;
use zoe_catalog::testing::{self, CatalogModel, IO, IndexKind, TreeRecord, tree_record};
use zoe_catalog::value::Val;
use zoe_catalog::{
    Columns, Config, Mirrors, Opener, Plan, Reads, Reason, Reply, Status, Wire, decode_request,
};
use zoe_core::Store;
use zoe_core::vfs::MemVfs;

const WAIT: Duration = Duration::from_secs(20);

const KINDS: [(&str, IndexKind); 8] = [
    ("portal_type", IndexKind::Field),
    ("Subject", IndexKind::Keyword),
    ("modified", IndexKind::Date),
    (
        "effectiveRange",
        IndexKind::DateRange("effective", "expires"),
    ),
    ("is_folderish", IndexKind::Boolean),
    ("UID", IndexKind::Uuid),
    ("path", IndexKind::Path),
    ("getId", IndexKind::Field),
];

const COLUMNS: [&str; 2] = ["getId", "Title"];
const TYPES: [&str; 3] = ["Document", "Folder", "News Item"];
const WORDS: [&str; 4] = ["alpha", "beta", "gamma", "delta"];

fn store() -> Arc<Store> {
    Arc::new(Store::open_vfs(MemVfs::new()).expect("a store"))
}

fn mirrors(store: &Arc<Store>) -> Mirrors {
    let weak = Arc::downgrade(store);
    let open: Opener = Arc::new(move || weak.upgrade().map(|s| s as Arc<dyn Reads>));
    Mirrors::start(open, store.last_transaction().0, Config::default())
}

fn commit(store: &Store, mirrors: Option<&Mirrors>, records: &[(u64, Vec<u8>)]) -> u64 {
    let tid = testing::commit(store, records, &[], &[]).expect("a commit");
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

fn s(text: &str) -> Val {
    Val::Str(text.into())
}

/// The path of the entry `i`, in one of four folders.
fn path_of(i: i32) -> String {
    format!("/site/f{}/item-{i}", i % 4)
}

/// Catalogs the entry `i` at `path`, with values that change with `turn`.
fn add_at(m: &mut CatalogModel, i: i32, turn: i32, path: &str) {
    let rid = (i * 7919 + 13) % 100_000 - 50_000;
    let k = i + turn;
    let words: Vec<Val> = (0..(k % 3))
        .map(|j| s(WORDS[((k + j) % 4) as usize]))
        .collect();
    let id = path.rsplit('/').next().unwrap_or_default().to_owned();
    let mut values = vec![
        ("portal_type", s(TYPES[(k % 3) as usize])),
        ("Subject", Val::List(words.into())),
        (
            "effectiveRange",
            Val::Tuple(vec![Val::Int(i64::from(1000 + k % 50)), Val::None].into()),
        ),
        ("is_folderish", Val::Bool(k % 4 == 1)),
        ("UID", s(&format!("uid-{i:05}"))),
        ("getId", s(&id)),
        ("Title", s(&format!("Title {k}"))),
    ];
    if k % 5 != 0 {
        values.push(("modified", Val::Int(i64::from(2000 + (k * 37) % 500))));
    }
    m.catalog(rid, path, &values);
}

fn add(m: &mut CatalogModel, i: i32, turn: i32) {
    add_at(m, i, turn, &path_of(i));
}

/// A plan in the form of section 8, from JSON.
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

fn keys(index: &str, values: Json) -> Json {
    json!({"k": "v", "i": index, "q": values, "and": false, "r": null, "not": []})
}

fn path(query: &str, depth: i64) -> Json {
    json!({"k": "p", "i": "path", "q": [[query, null]], "lv": 0, "and": false, "d": depth,
           "nt": false, "ns": 0, "ip": ["site", "portal_catalog", "path"]})
}

fn plan(clauses: Json, sort: Json, limit: Json, rows: bool) -> (Plan, Columns) {
    let cols = if rows { json!(COLUMNS) } else { Json::Null };
    let json = json!({"c": clauses, "s": sort, "l": limit, "bs": 0, "bz": null,
                      "cols": cols, "rn": 40});
    decode_request(&wire(&json)).expect("a plan")
}

fn plans() -> Vec<(Plan, Columns)> {
    vec![
        plan(
            json!([keys("portal_type", json!(["Document"]))]),
            json!([]),
            Json::Null,
            true,
        ),
        plan(
            json!([keys("Subject", json!(["alpha", "gamma"]))]),
            json!([["modified", true]]),
            json!(20),
            true,
        ),
        plan(
            json!([{"k": "b", "i": "is_folderish", "q": true}]),
            json!([["getId", false]]),
            Json::Null,
            false,
        ),
        plan(
            json!([{"k": "d", "i": "effectiveRange", "t": 1020},
                   keys("portal_type", json!(["Folder", "News Item"]))]),
            json!([]),
            Json::Null,
            false,
        ),
        plan(
            json!([keys("UID", json!(["uid-00007", "uid-00500", "uid-00031"]))]),
            json!([]),
            Json::Null,
            true,
        ),
        plan(json!([path("/site/f1", -1)]), json!([]), Json::Null, true),
        plan(
            json!([path("/site/f2", 1)]),
            json!([["getId", true]]),
            json!(15),
            false,
        ),
        plan(
            json!([path("/site", -1), keys("portal_type", json!(["Document"]))]),
            json!([["modified", false]]),
            Json::Null,
            false,
        ),
    ]
}

fn strip(answer: Result<Reply, zoe_catalog::Fallback>) -> Result<Reply, Reason> {
    answer
        .map(|mut reply| {
            reply.trace = Default::default();
            reply
        })
        .map_err(|f| f.reason)
}

fn answers(mirrors: &Mirrors, catalog: u64, tid: u64) -> Vec<Result<Reply, Reason>> {
    plans()
        .iter()
        .map(|(plan, columns)| strip(mirrors.query_rows(catalog, tid, plan, columns)))
        .collect()
}

fn with_overlay(
    mirrors: &Mirrors,
    catalog: u64,
    tid: u64,
    records: &[(u64, Vec<u8>)],
) -> Vec<Result<Reply, Reason>> {
    plans()
        .iter()
        .map(|(plan, columns)| strip(mirrors.query_overlay(catalog, tid, plan, columns, records)))
        .collect()
}

/// The answers of a mirror built from scratch at `tid`.
fn fresh(store: &Store, catalog: u64, tid: u64) -> Vec<Result<Reply, Reason>> {
    let built = build(store, catalog, tid, 2, &|_| true).expect("a build");
    let over = Default::default();
    let view = View::new(&built.base, &over);
    plans()
        .iter()
        .map(|(plan, columns)| strip(view.query_rows(plan, columns)))
        .collect()
}

/// A model of 300 entries, committed and registered, with its mirror
/// ready. Returns the store, the mirrors, the model and the tid.
fn prepared() -> (Arc<Store>, Mirrors, CatalogModel, u64) {
    let store = store();
    let mut m = CatalogModel::new(10, &KINDS, &COLUMNS);
    for i in 0..300 {
        add(&mut m, i, 0);
    }
    let tid = testing::commit(&store, &m.records(), &[m.key()], &[]).expect("a commit");
    let mirrors = mirrors(&store);
    assert!(ready_at(&mirrors, m.oid(), tid));
    (store, mirrors, m, tid)
}

/// The changes of a transaction: edits, new entries that split buckets, an
/// uncatalog and a move to another folder at the same depth.
fn change(m: &mut CatalogModel) {
    for i in (0..300).step_by(7) {
        add(m, i, 1);
    }
    for i in 300..450 {
        add(m, i, 0);
    }
    for i in [3, 50, 101] {
        m.uncatalog(&path_of(i));
    }
    m.uncatalog(&path_of(10));
    add_at(m, 10_010, 2, "/site/f1/moved-10");
}

fn same_status(a: &Status, b: &Status) -> bool {
    (a.base_tid, a.applied, a.floor, a.sets) == (b.base_tid, b.applied, b.floor, b.sets)
}

#[test]
fn an_overlay_gives_the_answer_of_a_mirror_after_a_commit_of_the_same_records() {
    let (store, mirrors, mut m, first) = prepared();
    let catalog = m.oid();
    let before = answers(&mirrors, catalog, first + 1);
    assert!(before.iter().all(Result::is_ok), "{before:?}");
    let status = mirrors.status(catalog).expect("a mirror");
    m.next_oids_from(PROVISIONAL | 1);
    change(&mut m);
    let records = m.records();
    // A split made new buckets: they have provisional oids.
    assert!(records.iter().any(|(oid, _)| is_provisional(*oid)));
    let overlaid = with_overlay(&mirrors, catalog, first + 1, &records);
    assert!(overlaid.iter().all(Result::is_ok), "{overlaid:?}");
    assert_ne!(overlaid, before);
    // The new buckets hold rows of the mirrored trees: without them, the
    // answers differ.
    let named: Vec<(u64, Vec<u8>)> = records
        .iter()
        .filter(|(oid, _)| !is_provisional(*oid))
        .cloned()
        .collect();
    assert_ne!(with_overlay(&mirrors, catalog, first + 1, &named), overlaid);
    // The overlay changes neither the change sets nor the base.
    let after = mirrors.status(catalog).expect("a mirror");
    assert!(same_status(&status, &after), "{status:?} {after:?}");
    // A later query without the overlay gives the answer without it.
    assert_eq!(answers(&mirrors, catalog, first + 1), before);
    let counts = mirrors.counts();
    let sent = (records.len() + named.len()) * plans().len();
    assert_eq!(counts.overlay_records, sent as u64);
    // The commit of the same records gives the same answers.
    let tid = commit(&store, Some(&mirrors), &records);
    assert!(ready_at(&mirrors, catalog, tid));
    assert_eq!(answers(&mirrors, catalog, tid + 1), overlaid);
    assert_eq!(fresh(&store, catalog, tid), overlaid);
    mirrors.stop();
}

#[test]
fn a_record_that_does_not_decode_gives_the_reply_overlay() {
    let (_store, mirrors, m, first) = prepared();
    let catalog = m.oid();
    let (plan, columns) = &plans()[0];
    let root = m.unindex_root("portal_type");
    let bad = vec![(root, b"not a pickle".to_vec())];
    let found = mirrors.query_overlay(catalog, first + 1, plan, columns, &bad);
    assert_eq!(found.map_err(|f| f.reason), Err(Reason::Overlay));
    // A record of an object that the mirror does not read costs a lookup.
    let other = vec![(1 << 40, b"not a pickle".to_vec())];
    let found = mirrors.query_overlay(catalog, first + 1, plan, columns, &other);
    assert_eq!(strip(found), answers(&mirrors, catalog, first + 1)[0]);
    // A new pickle of the catalog object makes the mirror stale: the
    // overlay cannot be read.
    let mut changed = CatalogModel::new(10, &KINDS, &COLUMNS);
    changed.records();
    changed.clear_index("modified");
    let records = changed.records();
    let found = mirrors.query_overlay(catalog, first + 1, plan, columns, &records);
    assert_eq!(found.map_err(|f| f.reason), Err(Reason::Overlay));
    mirrors.stop();
}

#[test]
fn a_node_whose_shape_a_later_commit_changed_gives_overlay() {
    let (store, mirrors, mut m, first) = prepared();
    let catalog = m.oid();
    // The same catalog, in the state of the store.
    let mut other = CatalogModel::new(10, &KINDS, &COLUMNS);
    for i in 0..300 {
        add(&mut other, i, 0);
    }
    other.records();
    // The transaction edits the entry with the lowest rid.
    let lowest = (0..300)
        .min_by_key(|i| (i * 7919 + 13) % 100_000 - 50_000)
        .unwrap();
    m.next_oids_from(PROVISIONAL | 1);
    add(&mut m, lowest, 1);
    let records = m.records();
    let overlaid = with_overlay(&mirrors, catalog, first + 1, &records);
    assert!(overlaid.iter().all(Result::is_ok), "{overlaid:?}");
    // A commit of another client edits another entry, whose rows stay in
    // each tree: no shape changes, and the answer over the overlay stays.
    let edited = (0..300)
        .find(|i| *i != lowest && [1, 3, 4].contains(&(i % 5)))
        .unwrap();
    add(&mut other, edited, 3);
    let tid = commit(&store, Some(&mirrors), &other.records());
    assert!(ready_at(&mirrors, catalog, tid));
    assert_eq!(
        with_overlay(&mirrors, catalog, first + 1, &records),
        overlaid
    );
    // A commit of another client adds an entry below the lowest rid: the
    // first bucket of each tree takes another key range. The shape at the
    // tid of the query is not known any more.
    other.catalog(-99_999, "/site/f0/first", &[("getId", s("first"))]);
    let tid = commit(&store, Some(&mirrors), &other.records());
    assert!(ready_at(&mirrors, catalog, tid));
    let (plan, columns) = &plans()[0];
    let found = mirrors.query_overlay(catalog, first + 1, plan, columns, &records);
    assert_eq!(found.map_err(|f| f.reason), Err(Reason::Overlay));
    mirrors.stop();
}

fn mirrors_with(store: &Arc<Store>, config: Config) -> Mirrors {
    let weak = Arc::downgrade(store);
    let open: Opener = Arc::new(move || weak.upgrade().map(|s| s as Arc<dyn Reads>));
    Mirrors::start(open, store.last_transaction().0, config)
}

#[test]
fn an_overlay_that_does_not_fit_takes_overlay_and_drops_no_mirror() {
    let store = store();
    let mut a = CatalogModel::new(10, &KINDS, &COLUMNS);
    let mut b = CatalogModel::new(100_000, &KINDS, &COLUMNS);
    for i in 0..300 {
        add(&mut a, i, 0);
        add(&mut b, i, 1);
    }
    let mut records = a.records();
    records.extend(b.records());
    let tid = testing::commit(&store, &records, &[a.key(), b.key()], &[]).expect("a commit");
    let config = Config {
        memory: 8 << 20,
        ..Config::default()
    };
    let mirrors = mirrors_with(&store, config);
    assert!(ready_at(&mirrors, a.oid(), tid));
    assert!(ready_at(&mirrors, b.oid(), tid));
    let (plan, columns) = &plans()[0];
    // A small overlay fits.
    let small = vec![(1 << 40, vec![0; 100])];
    let found = mirrors.query_overlay(a.oid(), tid + 1, plan, columns, &small);
    assert!(found.is_ok(), "{found:?}");
    // An overlay larger than the bound takes the Python path, and the
    // mirror of the other catalog stays.
    let large = vec![(1 << 40, vec![0; 9 << 20])];
    let found = mirrors.query_overlay(a.oid(), tid + 1, plan, columns, &large);
    assert_eq!(found.map_err(|f| f.reason), Err(Reason::Overlay));
    for catalog in [a.oid(), b.oid()] {
        assert_eq!(mirrors.status(catalog).expect("a mirror").state, "ready");
    }
    assert_eq!(mirrors.counts().drops, 0);
    let found = mirrors.query_overlay(b.oid(), tid + 1, plan, columns, &small);
    assert!(found.is_ok(), "{found:?}");
    mirrors.stop();
}

#[test]
fn a_query_whose_tid_the_thread_pruned_before_the_overlay_read_takes_overlay() {
    let store = store();
    let mut m = CatalogModel::new(10, &KINDS, &COLUMNS);
    for i in 0..300 {
        add(&mut m, i, 0);
    }
    let first = testing::commit(&store, &m.records(), &[m.key()], &[]).expect("a commit");
    let catalog = m.oid();
    // The same catalog, as another client changes it.
    let mut other = CatalogModel::new(10, &KINDS, &COLUMNS);
    for i in 0..300 {
        add(&mut other, i, 0);
    }
    other.records();
    let other = Arc::new(Mutex::new(other));
    let shared: Arc<OnceLock<Arc<Mirrors>>> = Arc::new(OnceLock::new());
    let armed = Arc::new(AtomicBool::new(false));
    // Between the version and the read of the overlay, other commits
    // change the shape of the first bucket of each tree and move the floor
    // of the versions past the tid of the query.
    let hook: Hook = {
        let (store, other, shared, armed) = (
            Arc::clone(&store),
            Arc::clone(&other),
            Arc::clone(&shared),
            Arc::clone(&armed),
        );
        Arc::new(move |_| {
            if !armed.swap(false, Ordering::SeqCst) {
                return;
            }
            let mirrors = shared.get().expect("the mirrors");
            let mut other = other.lock().unwrap();
            other.catalog(-99_999, "/site/f0/first", &[("getId", s("first"))]);
            for turn in [3, 8, 13] {
                if turn > 3 {
                    add(&mut other, 1, turn);
                }
                commit(&store, Some(mirrors), &other.records());
            }
            assert!(mirrors.wait_for(catalog, WAIT, |s| {
                s.is_some_and(|s| s.floor.is_some_and(|f| f > first))
            }));
            // The pass that moved the floor prunes the shape changes next.
            std::thread::sleep(Duration::from_millis(300));
        })
    };
    let config = Config {
        window_tids: 1,
        rebase_rows: 0,
        hooks: zoe_catalog::Hooks {
            before_overlay: Some(hook),
            ..zoe_catalog::Hooks::default()
        },
        ..Config::default()
    };
    let mirrors = Arc::new(mirrors_with(&store, config));
    shared.set(Arc::clone(&mirrors)).ok();
    assert!(ready_at(&mirrors, catalog, first));
    let lowest = (0..300)
        .min_by_key(|i| (i * 7919 + 13) % 100_000 - 50_000)
        .unwrap();
    m.next_oids_from(PROVISIONAL | 1);
    add(&mut m, lowest, 1);
    let records = m.records();
    let (plan, columns) = &plans()[0];
    armed.store(true, Ordering::SeqCst);
    let found = mirrors.query_overlay(catalog, first + 1, plan, columns, &records);
    assert!(!armed.load(Ordering::SeqCst), "the hook ran");
    assert_eq!(found.map_err(|f| f.reason), Err(Reason::Overlay));
    mirrors.stop();
}

#[test]
fn an_overlay_of_inner_nodes_named_in_reverse_order_reads_in_linear_time() {
    let (_store, mirrors, m, first) = prepared();
    let catalog = m.oid();
    // A chain of inner nodes from a root that the mirror owns, each one
    // naming the next, sent with the last node first.
    let n = 20_000u64;
    let root = m.unindex_root("portal_type");
    let node = |child: u64| {
        tree_record(
            IO,
            &TreeRecord::Split {
                children: vec![(child, true)],
                keys: Vec::new(),
                first: child,
            },
        )
    };
    let mut records: Vec<(u64, Vec<u8>)> = (1..=n)
        .rev()
        .map(|k| (PROVISIONAL | k, node(PROVISIONAL | (k + 1))))
        .collect();
    records.push((root, node(PROVISIONAL | 1)));
    let (plan, columns) = &plans()[0];
    let started = Instant::now();
    let _ = mirrors.query_overlay(catalog, first + 1, plan, columns, &records);
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "the read took {:?}",
        started.elapsed()
    );
    mirrors.stop();
}
