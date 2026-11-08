//! Acceptance test of the transaction overlay of `catalog_query` over the
//! protocol (ADR 0059, section 8): `catalog_query(catalog, tid, plan,
//! overlay)` through `zoe serve`.
//!
//! A store holds a catalog and its registration key, written by
//! `zoe_catalog::testing`. A transaction of the model changes, adds and
//! removes entries without a commit; its new objects take provisional
//! oids. The records go with the query as its overlay. The server answers
//! as the mirror of the process answers for the same records. The tests
//! run the server in the process, so a test hook can hold the apply of the
//! mirror.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use zoe_catalog::overlay::{PROVISIONAL, is_provisional};
use zoe_catalog::testing::{self, CatalogModel, IndexKind};
use zoe_catalog::value::Val;
use zoe_catalog::{Config, Mirrors, Opener, Reads, Wire, decode_request, encode_reply};
use zoe_server::serve::{ServeOptions, Server};

mod common;

use common::{Conn, Fault, Value, fresh_dir, open_store, p64, u64_of};

/// The longest wait of a test for the server or the mirror.
const WAIT: Duration = Duration::from_secs(30);

const TYPES: [&str; 3] = ["Document", "Folder", "News Item"];

const DECLARED: &[&str] = &["kv", "catalog_query"];

/// A catalog of `n` entries: a FieldIndex `portal_type`, a BooleanIndex
/// `is_folderish` and the column `getId`.
fn model(n: i32) -> CatalogModel {
    let mut m = CatalogModel::new(
        10,
        &[
            ("portal_type", IndexKind::Field),
            ("is_folderish", IndexKind::Boolean),
        ],
        &["getId"],
    );
    for i in 0..n {
        add(&mut m, i, 0);
    }
    m
}

/// Catalog the entry `i`, of a type that changes with `turn`.
fn add(m: &mut CatalogModel, i: i32, turn: i32) {
    let rid = 1000 + 2 * i;
    let kind = TYPES[((i + turn) % 3) as usize];
    m.catalog(
        rid,
        &format!("/site/item-{i}"),
        &[
            ("portal_type", Val::Str(kind.into())),
            ("is_folderish", Val::Bool(i % 2 == 0)),
            ("getId", Val::Str(format!("item-{i}").into())),
        ],
    );
}

/// The changes of a transaction: edits, new entries that split buckets and
/// removed entries. The new objects take provisional oids.
fn change(m: &mut CatalogModel) -> Vec<(u64, Vec<u8>)> {
    m.next_oids_from(PROVISIONAL | 1);
    for i in (0..150).step_by(5) {
        add(m, i, 1);
    }
    for i in 150..260 {
        add(m, i, 0);
    }
    for i in [4, 77, 120] {
        m.uncatalog(&format!("/site/item-{i}"));
    }
    let records = m.records();
    assert!(records.iter().any(|(oid, _)| is_provisional(*oid)));
    records
}

fn s(text: &str) -> Value {
    Value::from(text)
}

/// The plan `portal_type == kind`, with the rows of `getId`.
fn of_type(kind: &str) -> Value {
    let clause = Value::Map(vec![
        (s("k"), s("v")),
        (s("i"), s("portal_type")),
        (s("q"), Value::Array(vec![s(kind)])),
        (s("and"), Value::Boolean(false)),
        (s("r"), Value::Nil),
        (s("not"), Value::Array(vec![])),
    ]);
    Value::Map(vec![
        (s("c"), Value::Array(vec![clause])),
        (s("s"), Value::Array(vec![])),
        (s("l"), Value::Nil),
        (s("bs"), Value::from(0)),
        (s("bz"), Value::Nil),
        (s("cols"), Value::Array(vec![s("getId")])),
        (s("rn"), Value::from(30)),
    ])
}

/// The overlay `records` as the wire gives it.
fn overlay(records: &[(u64, Vec<u8>)]) -> Value {
    Value::Array(
        records
            .iter()
            .map(|(oid, pickle)| Value::Array(vec![p64(*oid), Value::Binary(pickle.clone())]))
            .collect(),
    )
}

fn client(addr: SocketAddr) -> Conn {
    let mut conn = Conn::open(addr);
    conn.call(
        "register",
        vec![
            s("1"),
            Value::Boolean(false),
            Value::Array(DECLARED.iter().map(|e| s(e)).collect()),
        ],
    )
    .unwrap();
    conn
}

fn query(
    conn: &mut Conn,
    catalog: u64,
    tid: u64,
    plan: &Value,
    overlay: Value,
) -> Result<Value, Fault> {
    conn.call(
        "catalog_query",
        vec![p64(catalog), p64(tid), plan.clone(), overlay],
    )
}

fn reason(reply: &Value) -> Option<&str> {
    reply
        .as_map()
        .expect("a reply is a map")
        .iter()
        .find(|(k, _)| k.as_str() == Some("f"))
        .and_then(|(_, v)| v.as_str())
}

/// Query without an overlay until the mirror answers, at most `WAIT`.
fn settled(conn: &mut Conn, catalog: u64, tid: u64, plan: &Value) {
    let deadline = Instant::now() + WAIT;
    loop {
        let reply = query(conn, catalog, tid, plan, Value::Nil).unwrap();
        match reason(&reply) {
            None => return,
            Some("not_registered" | "not_ready") if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Some(other) => panic!("the mirror does not answer: {other}"),
        }
    }
}

/// The reply of the mirror of the process for the store in `path`, at the
/// exclusive tid `tid`, with the plan `plan` and the overlay `records`.
fn in_process(
    path: &Path,
    catalog: u64,
    tid: u64,
    plan: &Value,
    records: &[(u64, Vec<u8>)],
) -> Value {
    let store = Arc::new(open_store(path));
    let weak = Arc::downgrade(&store);
    let open: Opener = Arc::new(move || weak.upgrade().map(|s| s as Arc<dyn Reads>));
    let mirrors = Mirrors::start(open, store.last_transaction().0, Config::default());
    assert!(mirrors.wait_for(catalog, WAIT, |s| s.is_some_and(|s| s.state == "ready")));
    let (plan, columns) = decode_request(&Wire::from_value(plan).unwrap()).unwrap();
    let answer = mirrors.query_overlay(catalog, tid, &plan, &columns, records);
    assert!(answer.is_ok(), "{answer:?}");
    mirrors.stop();
    encode_reply(tid, &answer).into_value()
}

#[test]
fn a_query_with_an_overlay_gives_the_reply_of_the_mirror_of_the_process() {
    let dir = fresh_dir("catalog-overlay");
    let path = dir.join("store");
    let mut m = model(150);
    let first = testing::commit(&open_store(&path), &m.records(), &[m.key()], &[]).unwrap();
    let catalog = m.oid();
    let records = change(&mut m);
    let plan = of_type("Document");
    let expected = in_process(&path, catalog, first + 1, &plan, &records);
    let server = Server::start(ServeOptions::new(&path)).unwrap();
    let mut conn = client(server.addr());
    settled(&mut conn, catalog, first + 1, &plan);
    let without = query(&mut conn, catalog, first + 1, &plan, Value::Nil).unwrap();
    let found = query(&mut conn, catalog, first + 1, &plan, overlay(&records)).unwrap();
    assert_eq!(found, expected);
    assert_ne!(found, without);
    // A query without the overlay still gives the answer without it.
    let again = query(&mut conn, catalog, first + 1, &plan, Value::Nil).unwrap();
    assert_eq!(again, without);
    // A record that the server cannot read gives the reply `overlay`.
    let root = m.unindex_root("portal_type");
    let bad = overlay(&[(root, b"not a pickle".to_vec())]);
    let found = query(&mut conn, catalog, first + 1, &plan, bad).unwrap();
    assert_eq!(reason(&found), Some("overlay"));
    server.stop();
}

#[test]
fn a_malformed_overlay_gets_value_error_and_the_connection_stays_open() {
    let dir = fresh_dir("catalog-overlay-malformed");
    let path = dir.join("store");
    let m = &mut model(20);
    let first = testing::commit(&open_store(&path), &m.records(), &[m.key()], &[]).unwrap();
    let server = Server::start(ServeOptions::new(&path)).unwrap();
    let mut conn = client(server.addr());
    let plan = of_type("Folder");
    settled(&mut conn, m.oid(), first + 1, &plan);
    let pickle = Value::Binary(b"pickle".to_vec());
    for bad in [
        s("not an array"),
        Value::Array(vec![Value::Nil]),
        Value::Array(vec![Value::Array(vec![p64(1)])]),
        Value::Array(vec![Value::Array(vec![
            Value::Binary(vec![0; 7]),
            pickle.clone(),
        ])]),
        Value::Array(vec![Value::Array(vec![p64(1), s("a str")])]),
        Value::Array(vec![Value::Array(vec![p64(1), pickle.clone(), pickle])]),
    ] {
        let fault = query(&mut conn, m.oid(), first + 1, &plan, bad.clone()).unwrap_err();
        assert_eq!(fault.class, "builtins.ValueError", "{bad:?}");
    }
    // The connection stays open.
    assert_eq!(conn.call("ping", vec![]).unwrap(), Value::Nil);
    let found = query(&mut conn, m.oid(), first + 1, &plan, Value::Nil).unwrap();
    assert_eq!(reason(&found), None);
    server.stop();
}

/// A hook that holds the apply of the mirror while it is on.
#[derive(Clone, Default)]
struct Hold(Arc<(Mutex<bool>, Condvar)>);

impl Hold {
    fn set(&self, on: bool) {
        *self.0.0.lock().unwrap() = on;
        self.0.1.notify_all();
    }

    fn wait(&self) {
        let mut held = self.0.0.lock().unwrap();
        while *held {
            held = self.0.1.wait(held).unwrap();
        }
    }
}

/// Commit `records` through `conn`, with `serials` the serial of each oid.
/// Return the tid.
fn commit(conn: &mut Conn, serials: &mut HashMap<u64, u64>, records: &[(u64, Vec<u8>)]) -> u64 {
    let id = Value::from(1);
    common::begin(conn, 1);
    for (oid, data) in records {
        let serial = serials.get(oid).copied().unwrap_or(0);
        conn.send_async(
            "storea",
            vec![
                p64(*oid),
                p64(serial),
                Value::Binary(data.clone()),
                id.clone(),
            ],
        );
    }
    conn.call("vote", vec![id.clone()]).unwrap();
    let tid = u64_of(&conn.call("tpc_finish", vec![id]).unwrap());
    for (oid, _) in records {
        serials.insert(*oid, tid);
    }
    tid
}

#[test]
fn a_commit_of_another_client_during_the_query_changes_nothing_of_its_answer() {
    let dir = fresh_dir("catalog-overlay-commit");
    let path = dir.join("store");
    let mut m = model(150);
    let initial = m.records();
    let first = testing::commit(&open_store(&path), &initial, &[m.key()], &[]).unwrap();
    let catalog = m.oid();
    // The same catalog, as the other client changes it.
    let mut other = model(150);
    other.records();
    let records = change(&mut m);
    let plan = of_type("Document");
    let expected = in_process(&path, catalog, first + 1, &plan, &records);
    let hold = Hold::default();
    let hook = hold.clone();
    let hooks = zoe_catalog::Hooks {
        before_apply: Some(Arc::new(move |_| hook.wait())),
        ..zoe_catalog::Hooks::default()
    };
    let server = Server::start(ServeOptions::new(&path).catalog_hooks(hooks)).unwrap();
    let mut reader = client(server.addr());
    let mut writer = client(server.addr());
    settled(&mut reader, catalog, first + 1, &plan);
    // The other client changes the type of an entry: its rows stay in each
    // tree, so no node changes its shape.
    let mut serials: HashMap<u64, u64> = initial.iter().map(|(oid, _)| (*oid, first)).collect();
    hold.set(true);
    add(&mut other, 31, 1);
    let later = commit(&mut writer, &mut serials, &other.records());
    // The mirror has not applied the commit: the query reads the version
    // at its tid.
    let found = query(&mut reader, catalog, first + 1, &plan, overlay(&records)).unwrap();
    assert_eq!(found, expected);
    hold.set(false);
    settled(&mut reader, catalog, later + 1, &plan);
    let found = query(&mut reader, catalog, first + 1, &plan, overlay(&records)).unwrap();
    assert_eq!(found, expected);
    server.stop();
}
