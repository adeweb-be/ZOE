//! Acceptance test of the extension `text` over the protocol (ADR 0060,
//! sections 8, 10, 12 and 13): the text deltas in the transaction of a
//! connection, the text clause of `catalog_query`, and `text_query` and
//! `text_info` on the text index of each catalog in `zoe serve`.
//!
//! A store holds a catalog of `zoe_catalog::testing`, its registration key
//! and the settings of its text indexes in `zoe.text`. The server opens the
//! text index of the catalog in `text/<oid>/` of the store directory, or
//! builds it, in the background after its open. The tests run the server in
//! the process, so a test hook can hold a rebuild; the test of the threads
//! runs in a process of its own.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use zoe_catalog::testing::{self, CatalogModel, IndexKind, TextOp};
use zoe_catalog::text::{CatalogSettings, IndexSettings, TextConfig};
use zoe_catalog::value::Val;
use zoe_catalog::{Config, Hooks, Mirrors, Opener, Reads, Wire};
use zoe_core::Store;
use zoe_server::serve::{ServeOptions, Server};

mod common;

use common::{Conn, Value, fresh_dir, open_store, p64, u64_of};

/// The longest wait of a test for the server.
const WAIT: Duration = Duration::from_secs(30);

const DECLARED: &[&str] = &["kv", "catalog_query", "text"];

fn s(text: &str) -> Value {
    Value::from(text)
}

fn map(items: Vec<(&str, Value)>) -> Value {
    Value::Map(items.into_iter().map(|(k, v)| (s(k), v)).collect())
}

fn strs(words: &[&str]) -> Value {
    Value::Array(words.iter().map(|w| s(w)).collect())
}

fn atom(words: &[&str]) -> Value {
    map(vec![("n", s("atom")), ("w", strs(words))])
}

/// The text clause of `text_query` on `index` with the node `q`.
fn clause(index: &str, q: Value) -> Value {
    map(vec![
        ("i", s(index)),
        ("q", q),
        ("lg", Value::Nil),
        ("cl", Value::Array(vec![])),
        ("st", Value::Array(vec![])),
        ("set", Value::Nil),
    ])
}

/// The same clause in a plan: with `"k": "t"`.
fn plan_clause(index: &str, q: Value) -> Value {
    let Value::Map(mut items) = clause(index, q) else {
        unreachable!()
    };
    items.insert(0, (s("k"), s("t")));
    Value::Map(items)
}

/// A plan with `clauses`, no sort, and the columns `cols` for `rn` rows.
fn plan(clauses: Vec<Value>, cols: Option<&[&str]>, rn: i64) -> Value {
    map(vec![
        ("c", Value::Array(clauses)),
        ("s", Value::Array(vec![])),
        ("l", Value::Nil),
        ("bs", Value::from(0)),
        ("bz", Value::Nil),
        ("cols", cols.map_or(Value::Nil, strs)),
        ("rn", Value::from(rn)),
    ])
}

/// The clause `portal_type == kind`.
fn of_type(kind: &str) -> Value {
    map(vec![
        ("k", s("v")),
        ("i", s("portal_type")),
        ("q", Value::Array(vec![s(kind)])),
        ("and", Value::Boolean(false)),
        ("r", Value::Nil),
        ("not", Value::Array(vec![])),
    ])
}

/// The settings of the text index `Title`, in English.
fn settings() -> CatalogSettings {
    let mut out = CatalogSettings::default();
    out.indexes
        .insert("Title".into(), IndexSettings::enhanced(&["en"]));
    out
}

/// A catalog of `n` entries of the types Document and Folder, with the
/// column `getId` of `cell` bytes, and its texts in `Title`: `kalo` in
/// each, and `mite` in one entry in 3. Return the model and the tid.
fn prepare(path: &Path, n: i32, cell: usize, register: bool) -> (CatalogModel, u64) {
    let store = open_store(path);
    let mut model = CatalogModel::new(10, &[("portal_type", IndexKind::Field)], &["getId"]);
    let mut texts = Vec::new();
    for i in 0..n {
        let rid = 1000 + i;
        let kind = if i % 2 == 0 { "Document" } else { "Folder" };
        let id = format!("{i:0>width$}", width = cell.max(4));
        model.catalog(
            rid,
            &format!("/site/item-{i}"),
            &[
                ("portal_type", Val::Str(kind.into())),
                ("getId", Val::Str(id.into())),
            ],
        );
        let text = if i % 3 == 0 { "kalo mite" } else { "kalo" };
        texts.push((rid, text));
    }
    let keys: Vec<[u8; 8]> = if register { vec![model.key()] } else { vec![] };
    testing::commit(&store, &model.records(), &keys, &[]).unwrap();
    let ops: Vec<TextOp<'_>> = texts
        .iter()
        .map(|(rid, text)| TextOp::Set {
            index: "Title",
            rid: *rid,
            language: "en",
            text,
        })
        .collect();
    let settings = register.then(settings);
    let tid = testing::commit_text(&store, model.oid(), &ops, settings.as_ref(), false).unwrap();
    (model, tid)
}

fn options(path: &Path) -> ServeOptions {
    ServeOptions::new(path).admin_listen(SocketAddr::from(([127, 0, 0, 1], 0)))
}

/// Connect to `addr` and register with the storage `1`, declaring
/// `extensions`.
fn client(addr: SocketAddr, extensions: &[&str]) -> Conn {
    let mut conn = Conn::open(addr);
    conn.call(
        "register",
        vec![
            s("1"),
            Value::Boolean(false),
            Value::Array(extensions.iter().map(|e| s(e)).collect()),
        ],
    )
    .unwrap();
    conn
}

fn get<'a>(reply: &'a Value, key: &str) -> Option<&'a Value> {
    reply
        .as_map()
        .expect("a reply is a map")
        .iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .map(|(_, v)| v)
}

fn ints(value: Option<&Value>) -> Vec<i32> {
    match value {
        Some(Value::Binary(bytes)) => bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| i32::from_be_bytes(*c))
            .collect(),
        other => panic!("not ints: {other:?}"),
    }
}

/// `text_query` at `tid` with no filter.
fn text_query(conn: &mut Conn, catalog: u64, tid: u64, clause: &Value) -> Value {
    conn.call(
        "text_query",
        vec![p64(catalog), p64(tid), clause.clone(), Value::Nil],
    )
    .unwrap()
}

/// `text_query` until the index answers, at most `WAIT`.
fn answered(conn: &mut Conn, catalog: u64, tid: u64, clause: &Value) -> Value {
    let deadline = Instant::now() + WAIT;
    loop {
        let reply = text_query(conn, catalog, tid, clause);
        if get(&reply, "e").is_none() || Instant::now() > deadline {
            return reply;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn last_tid(conn: &mut Conn) -> u64 {
    u64_of(&conn.call("lastTransaction", vec![]).unwrap())
}

/// Commit the text deltas `calls`, each a method and its arguments, in one
/// transaction of `conn`. Return the tid.
fn commit_texts(conn: &mut Conn, calls: Vec<(&str, Vec<Value>)>) -> u64 {
    common::begin(conn, 1);
    for (method, args) in calls {
        conn.send_async(method, args);
    }
    conn.call("vote", vec![Value::from(1)]).unwrap();
    u64_of(&conn.call("tpc_finish", vec![Value::from(1)]).unwrap())
}

fn text_set(catalog: u64, rid: i32, text: &str, before: u64) -> (&'static str, Vec<Value>) {
    (
        "text_set",
        vec![
            p64(catalog),
            s("Title"),
            Value::from(rid),
            s("en"),
            s(text),
            p64(before),
        ],
    )
}

#[test]
fn deltas_commit_with_their_transaction_and_abort_with_it() {
    let dir = fresh_dir("text-commit");
    let path = dir.join("store");
    let (model, tid) = prepare(&path, 6, 4, true);
    let server = Server::start(options(&path)).unwrap();
    let mut conn = client(server.addr(), DECLARED);
    let catalog = model.oid();
    let q = clause("Title", atom(&["ruso"]));
    assert_eq!(
        ints(get(&answered(&mut conn, catalog, tid + 1, &q), "r")),
        Vec::<i32>::new()
    );
    let before = last_tid(&mut conn) + 1;
    let committed = commit_texts(&mut conn, vec![text_set(catalog, 1001, "ruso", before)]);
    let reply = answered(&mut conn, catalog, committed + 1, &q);
    assert_eq!(ints(get(&reply, "r")), vec![1001]);
    // An aborted transaction leaves nothing.
    common::begin(&mut conn, 2);
    conn.send_async("text_set", text_set(catalog, 1002, "ruso", committed + 1).1);
    conn.call("tpc_abort", vec![Value::from(2)]).unwrap();
    let now = last_tid(&mut conn) + 1;
    assert_eq!(
        ints(get(&answered(&mut conn, catalog, now, &q), "r")),
        vec![1001]
    );
    // An unindex and a clear in a transaction.
    let tid = commit_texts(
        &mut conn,
        vec![(
            "text_unindex",
            vec![p64(catalog), s("Title"), Value::from(1001), p64(now)],
        )],
    );
    assert_eq!(
        ints(get(&answered(&mut conn, catalog, tid + 1, &q), "r")),
        Vec::<i32>::new()
    );
    let tid = commit_texts(
        &mut conn,
        vec![("text_clear", vec![p64(catalog), s("Title")])],
    );
    let all = clause("Title", atom(&["kalo"]));
    assert_eq!(
        ints(get(&answered(&mut conn, catalog, tid + 1, &all), "r")),
        Vec::<i32>::new()
    );
    server.stop();
}

#[test]
fn a_text_conflict_fails_the_vote_of_the_second_client() {
    let dir = fresh_dir("text-conflict");
    let path = dir.join("store");
    let (model, _) = prepare(&path, 4, 4, true);
    let server = Server::start(options(&path)).unwrap();
    let catalog = model.oid();
    let mut first = client(server.addr(), DECLARED);
    let mut second = client(server.addr(), DECLARED);
    let before = last_tid(&mut first) + 1;
    commit_texts(&mut first, vec![text_set(catalog, 1000, "ruso", before)]);
    common::begin(&mut second, 1);
    second.send_async("text_set", text_set(catalog, 1000, "vite", before).1);
    let fault = second.call("vote", vec![Value::from(1)]).unwrap_err();
    assert_eq!(fault.class, "ZODB.POSException.ConflictError");
    second.call("tpc_abort", vec![Value::from(1)]).unwrap();
    // A bad rid fails the vote with `ValueError`.
    common::begin(&mut second, 2);
    second.send_async(
        "text_set",
        vec![
            p64(catalog),
            s("Title"),
            Value::from(1i64 << 40),
            s("en"),
            s("x"),
            p64(before),
        ],
    );
    let fault = second.call("vote", vec![Value::from(2)]).unwrap_err();
    assert_eq!(fault.class, "builtins.ValueError");
    server.stop();
}

#[test]
fn a_connection_that_did_not_declare_text_gets_value_error() {
    let dir = fresh_dir("text-undeclared");
    let path = dir.join("store");
    let (model, tid) = prepare(&path, 4, 4, true);
    let server = Server::start(options(&path)).unwrap();
    let catalog = model.oid();
    let mut conn = client(server.addr(), &["kv", "catalog_query"]);
    let calls: Vec<(&str, Vec<Value>)> = vec![
        (
            "text_query",
            vec![
                p64(catalog),
                p64(tid + 1),
                clause("Title", atom(&["kalo"])),
                Value::Nil,
            ],
        ),
        (
            "text_info",
            vec![p64(catalog), s("Title"), p64(tid + 1), Value::Nil],
        ),
        text_set(catalog, 1, "x", tid + 1),
        (
            "text_unindex",
            vec![p64(catalog), s("Title"), Value::from(1), p64(tid + 1)],
        ),
        ("text_clear", vec![p64(catalog), s("Title")]),
    ];
    for (method, args) in calls {
        let fault = conn.call(method, args).unwrap_err();
        assert_eq!(fault.class, "builtins.ValueError", "{method}");
    }
    // The connection stays open.
    assert!(conn.call("lastTransaction", vec![]).is_ok());
    // A stock register gets no `extensions` key.
    let mut stock = Conn::open(server.addr());
    stock.register("1", false).unwrap();
    let info = stock.call("get_info", vec![]).unwrap();
    assert!(get(&info, "extensions").is_none());
    server.stop();
}

/// Start the mirrors of `store` in the process, with its text indexes in
/// `dir`.
fn in_process(store: &Arc<Store>, dir: &Path) -> Mirrors {
    let weak = Arc::downgrade(store);
    let open: Opener = Arc::new(move || weak.upgrade().map(|s| s as Arc<dyn Reads>));
    let config = Config {
        text: TextConfig {
            dir: Some(dir.to_owned()),
            read_only: false,
        },
        ..Config::default()
    };
    Mirrors::start(open, store.last_transaction().0, config)
}

/// The msgpack value of the wire value `wire`.
fn value_of(wire: Wire) -> Value {
    match wire {
        Wire::Nil => Value::Nil,
        Wire::Bool(b) => Value::Boolean(b),
        Wire::Int(i) => Value::from(i),
        Wire::Float(f) => Value::F64(f),
        Wire::Str(s) => Value::from(s),
        Wire::Bin(b) => Value::Binary(b),
        Wire::Array(items) => Value::Array(items.into_iter().map(value_of).collect()),
        Wire::Map(items) => Value::Map(
            items
                .into_iter()
                .map(|(k, v)| (value_of(k), value_of(v)))
                .collect(),
        ),
        Wire::Ext(kind, data) => Value::Ext(kind, data),
    }
}

/// The wire value of the msgpack value `value`.
fn wire_of(value: &Value) -> Wire {
    match value {
        Value::Nil => Wire::Nil,
        Value::Boolean(b) => Wire::Bool(*b),
        Value::Integer(n) => Wire::Int(n.as_i64().unwrap()),
        Value::F64(f) => Wire::Float(*f),
        Value::F32(f) => Wire::Float(f64::from(*f)),
        Value::String(s) => Wire::Str(s.as_str().unwrap().to_owned()),
        Value::Binary(b) => Wire::Bin(b.clone()),
        Value::Array(items) => Wire::Array(items.iter().map(wire_of).collect()),
        Value::Map(items) => Wire::Map(
            items
                .iter()
                .map(|(k, v)| (wire_of(k), wire_of(v)))
                .collect(),
        ),
        Value::Ext(kind, data) => Wire::Ext(*kind, data.clone()),
    }
}

#[test]
fn zoe_serve_gives_the_replies_of_the_index_in_the_process() {
    let dir = fresh_dir("text-same");
    let path = dir.join("store");
    let (model, tid) = prepare(&path, 30, 4, true);
    let catalog = model.oid();
    let queries = [
        plan(
            vec![of_type("Document"), plan_clause("Title", atom(&["mite"]))],
            None,
            0,
        ),
        plan(
            vec![plan_clause("Title", atom(&["kalo"]))],
            Some(&["getId"]),
            5,
        ),
    ];
    let text = clause("Title", atom(&["mite"]));
    let server = Server::start(options(&path)).unwrap();
    let mut conn = client(server.addr(), DECLARED);
    answered(&mut conn, catalog, tid + 1, &text);
    let deadline = Instant::now() + WAIT;
    let mut served = Vec::new();
    for q in &queries {
        let reply = loop {
            let reply = conn
                .call(
                    "catalog_query",
                    vec![p64(catalog), p64(tid + 1), q.clone(), Value::Nil],
                )
                .unwrap();
            let pending = get(&reply, "f")
                .and_then(Value::as_str)
                .is_some_and(|f| f == "not_ready" || f == "not_registered");
            if !pending || Instant::now() > deadline {
                break reply;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(get(&reply, "r").is_some(), "{reply:?}");
        served.push(reply);
    }
    let served_text = text_query(&mut conn, catalog, tid + 1, &text);
    server.stop();
    let store = Arc::new(Store::open(&path).unwrap());
    let mirrors = in_process(&store, &path);
    assert!(mirrors.wait_for(catalog, WAIT, |s| s.is_some_and(|s| s.state == "ready")));
    let texts = mirrors.texts().unwrap();
    assert!(texts.wait_for(catalog, WAIT, |c| c.is_some_and(|c| c.ready)));
    for (q, reply) in queries.iter().zip(&served) {
        let answer = mirrors.query_wire(catalog, tid + 1, &wire_of(q)).unwrap();
        let local = value_of(zoe_catalog::encode_reply(tid + 1, &answer));
        assert_eq!(&local, reply);
    }
    let local = zoe_catalog::text::engine::text_query(
        Some(texts),
        &store,
        catalog,
        tid + 1,
        &wire_of(&text),
        &Wire::Nil,
    )
    .unwrap();
    let local = value_of(zoe_catalog::text::clause::encode_hits(tid + 1, &local));
    assert_eq!(local, served_text);
    mirrors.stop();
}

/// A hook that holds the thread until the test releases it.
#[derive(Clone, Default)]
struct Hold(Arc<(Mutex<(bool, bool)>, Condvar)>);

impl Hold {
    fn wait(&self) {
        let (lock, cv) = &*self.0;
        let mut state = lock.lock().unwrap();
        state.0 = true;
        cv.notify_all();
        while !state.1 {
            state = cv.wait(state).unwrap();
        }
    }

    fn reached(&self) {
        let (lock, cv) = &*self.0;
        let mut state = lock.lock().unwrap();
        while !state.0 {
            state = cv.wait_timeout(state, WAIT).unwrap().0;
        }
    }

    fn release(&self) {
        let (lock, cv) = &*self.0;
        lock.lock().unwrap().1 = true;
        cv.notify_all();
    }
}

#[test]
fn while_the_directory_builds_a_text_clause_is_unavailable_and_a_plan_without_one_is_answered() {
    let dir = fresh_dir("text-building");
    let path = dir.join("store");
    let (model, tid) = prepare(&path, 10, 4, true);
    let catalog = model.oid();
    let hold = Hold::default();
    let hook = hold.clone();
    let hooks = Hooks {
        before_text_build: Some(Arc::new(move |_| hook.wait())),
        ..Hooks::default()
    };
    let wait = Duration::from_millis(300);
    let server = Server::start(options(&path).catalog_wait(wait).catalog_hooks(hooks)).unwrap();
    hold.reached();
    let mut conn = client(server.addr(), DECLARED);
    let plain = plan(vec![of_type("Document")], None, 0);
    let deadline = Instant::now() + WAIT;
    loop {
        let reply = conn
            .call(
                "catalog_query",
                vec![p64(catalog), p64(tid + 1), plain.clone(), Value::Nil],
            )
            .unwrap();
        if get(&reply, "r").is_some() {
            assert_eq!(ints(get(&reply, "r")).len(), 5);
            break;
        }
        assert!(Instant::now() < deadline, "{reply:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
    let with_text = plan(
        vec![of_type("Document"), plan_clause("Title", atom(&["kalo"]))],
        None,
        0,
    );
    let started = Instant::now();
    let reply = conn
        .call(
            "catalog_query",
            vec![p64(catalog), p64(tid + 1), with_text, Value::Nil],
        )
        .unwrap();
    assert_eq!(
        get(&reply, "e").and_then(Value::as_str),
        Some("unavailable")
    );
    assert!(started.elapsed() >= wait);
    let reply = text_query(
        &mut conn,
        catalog,
        tid + 1,
        &clause("Title", atom(&["kalo"])),
    );
    assert_eq!(
        get(&reply, "e").and_then(Value::as_str),
        Some("unavailable")
    );
    hold.release();
    let reply = answered(
        &mut conn,
        catalog,
        tid + 1,
        &clause("Title", atom(&["kalo"])),
    );
    assert_eq!(ints(get(&reply, "r")).len(), 10);
    server.stop();
}

#[test]
fn a_reply_over_16_mib_gets_too_large() {
    let dir = fresh_dir("text-too-large");
    let path = dir.join("store");
    // 180 rows of 100 KB each: 18 MB of cells.
    let (model, tid) = prepare(&path, 180, 100_000, true);
    let catalog = model.oid();
    let server = Server::start(options(&path)).unwrap();
    let mut conn = client(server.addr(), DECLARED);
    answered(
        &mut conn,
        catalog,
        tid + 1,
        &clause("Title", atom(&["kalo"])),
    );
    let q = plan(
        vec![plan_clause("Title", atom(&["kalo"]))],
        Some(&["getId"]),
        180,
    );
    let small = plan(
        vec![plan_clause("Title", atom(&["kalo"]))],
        Some(&["getId"]),
        10,
    );
    let deadline = Instant::now() + WAIT;
    loop {
        let reply = conn
            .call(
                "catalog_query",
                vec![p64(catalog), p64(tid + 1), small.clone(), Value::Nil],
            )
            .unwrap();
        if get(&reply, "r").is_some() {
            break;
        }
        assert!(Instant::now() < deadline, "{reply:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
    let reply = conn
        .call(
            "catalog_query",
            vec![p64(catalog), p64(tid + 1), q, Value::Nil],
        )
        .unwrap();
    assert_eq!(get(&reply, "e").and_then(Value::as_str), Some("too_large"));
    server.stop();
}

/// The text of `GET /metrics` on the admin port `admin`.
fn metrics(admin: SocketAddr) -> String {
    let mut stream = TcpStream::connect(admin).unwrap();
    stream
        .write_all(b"GET /metrics HTTP/1.1\r\nHost: zoe\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut text = String::new();
    stream.read_to_string(&mut text).unwrap();
    text
}

#[test]
fn metrics_give_zoe_text_commit_lag_seconds() {
    let dir = fresh_dir("text-metrics");
    let path = dir.join("store");
    let (model, tid) = prepare(&path, 4, 4, true);
    let catalog = model.oid();
    let server = Server::start(options(&path)).unwrap();
    let mut conn = client(server.addr(), DECLARED);
    answered(
        &mut conn,
        catalog,
        tid + 1,
        &clause("Title", atom(&["kalo"])),
    );
    let line = format!("zoe_text_commit_lag_seconds{{storage=\"1\",catalog=\"{catalog:016x}\"}} 0");
    let deadline = Instant::now() + WAIT;
    while !metrics(server.admin_addr().unwrap())
        .lines()
        .any(|l| l == line)
    {
        assert!(
            Instant::now() < deadline,
            "{}",
            metrics(server.admin_addr().unwrap())
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        metrics(server.admin_addr().unwrap()).contains("# TYPE zoe_text_commit_lag_seconds gauge")
    );
    server.stop();
}

/// The names of the threads of this process.
#[cfg(target_os = "linux")]
fn threads() -> Vec<String> {
    std::fs::read_dir("/proc/self/task")
        .unwrap()
        .filter_map(|t| std::fs::read_to_string(t.ok()?.path().join("comm")).ok())
        .map(|n| n.trim().to_owned())
        .collect()
}

/// The child of `the_threads_of_tantivy_start_with_a_registered_catalog`:
/// a server in a process of its own, so no other test adds threads.
#[test]
#[ignore = "runs in a process of its own"]
fn tantivy_threads_child() {
    let Ok(mode) = std::env::var("ZOE_TEXT_THREADS") else {
        return;
    };
    let dir = fresh_dir(&format!("text-threads-{mode}"));
    let path = dir.join("store");
    let registered = mode == "registered";
    let (model, tid) = prepare(&path, 4, 4, registered);
    let server = Server::start(options(&path)).unwrap();
    let mut conn = client(server.addr(), DECLARED);
    if registered {
        let reply = answered(
            &mut conn,
            model.oid(),
            tid + 1,
            &clause("Title", atom(&["kalo"])),
        );
        assert_eq!(ints(get(&reply, "r")).len(), 4);
    } else {
        std::thread::sleep(Duration::from_millis(500));
    }
    #[cfg(target_os = "linux")]
    {
        let names = threads();
        let tantivy = names.iter().any(|n| n.starts_with("thrd-tantivy"));
        let text = names.iter().any(|n| n == "zoe-text");
        assert_eq!(tantivy, registered, "{names:?}");
        assert_eq!(text, registered, "{names:?}");
    }
    server.stop();
}

#[test]
fn the_threads_of_tantivy_start_with_a_registered_catalog() {
    for mode in ["registered", "none"] {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tantivy_threads_child",
                "--include-ignored",
                "--nocapture",
            ])
            .env("ZOE_TEXT_THREADS", mode)
            .status()
            .unwrap();
        assert!(status.success(), "{mode}");
    }
}
