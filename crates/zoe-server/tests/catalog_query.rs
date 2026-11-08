//! Acceptance test of `catalog_query` over the protocol (ADR 0059,
//! sections 5, 6, 8, 10 and 11): the catalog mirrors in `zoe serve`.
//!
//! A store holds a catalog and its registration key, written by
//! `zoe_catalog::testing`. The server builds the mirror of each registered
//! catalog in the background after its open. A client that declared the
//! extension `catalog_query` in `register` calls
//! `catalog_query(catalog, tid, plan, overlay)`: the server answers from
//! the mirror as of the snapshot that ends before `tid`, or gives the
//! reason of the Python path. The tests run the server in the process, so
//! a test hook can hold the apply of the mirror.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use zoe_catalog::testing::{self, CatalogModel, IndexKind};
use zoe_catalog::value::Val;
use zoe_server::serve::{ServeOptions, Server, StorageOptions};

mod common;

use common::{Conn, Fault, Value, fresh_dir, open_store, p64, u64_of};

/// The longest wait of a test for the server.
const WAIT: Duration = Duration::from_secs(30);

const TYPES: [&str; 3] = ["Document", "Folder", "News Item"];

/// A catalog of `n` entries whose objects take oids from `first_oid`: a
/// FieldIndex `portal_type`, a BooleanIndex `is_folderish` and the column
/// `getId`. The entry `i` has the rid `1000 + 2 i` and the type
/// `TYPES[(i + shift) % 3]`.
struct Catalog {
    model: CatalogModel,
    /// The type of each rid.
    types: HashMap<i32, &'static str>,
    shift: usize,
}

impl Catalog {
    fn new(first_oid: u64, n: usize, shift: usize) -> Catalog {
        let model = CatalogModel::new(
            first_oid,
            &[
                ("portal_type", IndexKind::Field),
                ("is_folderish", IndexKind::Boolean),
            ],
            &["getId"],
        );
        let mut catalog = Catalog {
            model,
            types: HashMap::new(),
            shift,
        };
        for i in 0..n {
            catalog.add(i);
        }
        catalog
    }

    /// Catalog the entry `i`.
    fn add(&mut self, i: usize) {
        let rid = 1000 + 2 * i as i32;
        let kind = TYPES[(i + self.shift) % 3];
        self.types.insert(rid, kind);
        self.model.catalog(
            rid,
            &format!("/site/item-{i}"),
            &[
                ("portal_type", Val::Str(kind.into())),
                ("is_folderish", Val::Bool(i.is_multiple_of(2))),
                ("getId", Val::Str(format!("item-{i}").into())),
            ],
        );
    }

    fn oid(&self) -> u64 {
        self.model.oid()
    }

    /// The rids of the entries of the type `kind`, in ascending order.
    fn rids(&self, kind: &str) -> Vec<i32> {
        let mut rids: Vec<i32> = self
            .types
            .iter()
            .filter(|(_, t)| **t == kind)
            .map(|(r, _)| *r)
            .collect();
        rids.sort_unstable();
        rids
    }
}

/// Write `catalogs` and their registration keys to the store in `path`.
/// Return the tid.
fn prepare(path: &Path, catalogs: &mut [&mut Catalog]) -> u64 {
    let store = open_store(path);
    let mut records = Vec::new();
    let mut keys = Vec::new();
    for catalog in catalogs.iter_mut() {
        records.extend(catalog.model.records());
        keys.push(catalog.model.key());
    }
    testing::commit(&store, &records, &keys, &[]).unwrap()
}

fn s(text: &str) -> Value {
    Value::from(text)
}

fn map(items: Vec<(&str, Value)>) -> Value {
    Value::Map(items.into_iter().map(|(k, v)| (s(k), v)).collect())
}

/// The plan of one clause, without sort and without rows.
fn plan(clause: Value) -> Value {
    map(vec![
        ("c", Value::Array(vec![clause])),
        ("s", Value::Array(vec![])),
        ("l", Value::Nil),
        ("bs", Value::from(0)),
        ("bz", Value::Nil),
        ("cols", Value::Nil),
        ("rn", Value::from(0)),
    ])
}

/// The plan `portal_type == kind`.
fn of_type(kind: &str) -> Value {
    plan(map(vec![
        ("k", s("v")),
        ("i", s("portal_type")),
        ("q", Value::Array(vec![s(kind)])),
        ("and", Value::Boolean(false)),
        ("r", Value::Nil),
        ("not", Value::Array(vec![])),
    ]))
}

/// Connect to `addr` and register with the storage `name`, declaring
/// `extensions`.
fn client(addr: SocketAddr, name: &str, read_only: bool, extensions: &[&str]) -> Conn {
    let mut conn = Conn::open(addr);
    conn.call(
        "register",
        vec![
            s(name),
            Value::Boolean(read_only),
            Value::Array(extensions.iter().map(|e| s(e)).collect()),
        ],
    )
    .unwrap();
    conn
}

/// Call `catalog_query` with a nil overlay.
fn query(conn: &mut Conn, catalog: u64, tid: u64, plan: &Value) -> Result<Value, Fault> {
    conn.call(
        "catalog_query",
        vec![p64(catalog), p64(tid), plan.clone(), Value::Nil],
    )
}

fn get<'a>(reply: &'a Value, key: &str) -> Option<&'a Value> {
    reply
        .as_map()
        .expect("a reply is a map")
        .iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .map(|(_, v)| v)
}

/// The rids of a native reply, or the reason of a fallback.
fn answer(reply: &Value) -> Result<Vec<i32>, String> {
    if let Some(reason) = get(reply, "f") {
        return Err(reason.as_str().unwrap().to_owned());
    }
    let Some(Value::Binary(bytes)) = get(reply, "r") else {
        panic!("a reply without r: {reply:?}");
    };
    Ok(bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| i32::from_be_bytes(*c))
        .collect())
}

/// Query until the reply is not `not_registered` or `not_ready`, at most
/// `WAIT`. Return the last answer.
fn settled(conn: &mut Conn, catalog: u64, tid: u64, plan: &Value) -> Result<Vec<i32>, String> {
    let deadline = Instant::now() + WAIT;
    loop {
        let found = answer(&query(conn, catalog, tid, plan).unwrap());
        match &found {
            Err(reason)
                if (reason == "not_registered" || reason == "not_ready")
                    && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => return found,
        }
    }
}

/// Query until the reason of the reply is `reason`, at most `WAIT`.
fn until_reason(conn: &mut Conn, catalog: u64, tid: u64, plan: &Value, reason: &str) -> bool {
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline {
        if answer(&query(conn, catalog, tid, plan).unwrap())
            .err()
            .as_deref()
            == Some(reason)
        {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

/// Commit `records` through `conn`, with the registration keys `register`
/// and without the keys `unregister`. `serials` gives the serial of each
/// oid, and takes the tid of the commit. Return the tid.
fn commit(
    conn: &mut Conn,
    serials: &mut HashMap<u64, u64>,
    records: &[(u64, Vec<u8>)],
    register: &[[u8; 8]],
    unregister: &[[u8; 8]],
) -> u64 {
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
    let namespace =
        |key: &[u8; 8]| vec![s("durable"), s("zoe.catalog"), Value::Binary(key.to_vec())];
    for key in register {
        let mut args = namespace(key);
        args.extend([Value::Binary(Vec::new()), Value::Nil]);
        conn.send_async("kv_set", args);
    }
    for key in unregister {
        conn.send_async("kv_delete", namespace(key));
    }
    conn.call("vote", vec![id.clone()]).unwrap();
    let tid = u64_of(&conn.call("tpc_finish", vec![id]).unwrap());
    for (oid, _) in records {
        serials.insert(*oid, tid);
    }
    tid
}

/// The serials of the records of a store that `prepare` wrote at `tid`.
fn serials_of(records: &[(u64, Vec<u8>)], tid: u64) -> HashMap<u64, u64> {
    records.iter().map(|(oid, _)| (*oid, tid)).collect()
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

/// The value of the sample `name{labels}` of `/metrics`.
fn sample(admin: SocketAddr, name: &str, labels: &str) -> Option<f64> {
    let prefix = format!("{name}{{{labels}}} ");
    metrics(admin)
        .lines()
        .find_map(|line| line.strip_prefix(&prefix).map(|v| v.parse().unwrap()))
}

/// The labels of the gauges of the mirror of `catalog` in the storage
/// `name`.
fn mirror(name: &str, catalog: u64) -> String {
    format!("storage=\"{name}\",catalog=\"{catalog:016x}\"")
}

fn options(path: &Path) -> ServeOptions {
    ServeOptions::new(path).admin_listen(SocketAddr::from(([127, 0, 0, 1], 0)))
}

const DECLARED: &[&str] = &["kv", "catalog_query"];

#[test]
fn the_server_builds_a_registered_catalog_after_its_open() {
    let dir = fresh_dir("catalog-open");
    let path = dir.join("store");
    let mut catalog = Catalog::new(10, 30, 0);
    let tid = prepare(&path, &mut [&mut catalog]);
    let server = Server::start(options(&path)).unwrap();
    let admin = server.admin_addr().unwrap();
    let labels = mirror("1", catalog.oid());
    let deadline = Instant::now() + WAIT;
    while sample(admin, "zoe_catalog_mirror_ready", &labels) != Some(1.0) {
        assert!(Instant::now() < deadline, "the mirror is not ready");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(sample(admin, "zoe_catalog_mirror_build_seconds", &labels).is_some());
    let mut conn = client(server.addr(), "1", false, DECLARED);
    let found = answer(&query(&mut conn, catalog.oid(), tid + 1, &of_type("Document")).unwrap());
    assert_eq!(found, Ok(catalog.rids("Document")));
    server.stop();
}

#[test]
fn a_commit_that_adds_a_key_builds_the_catalog_and_one_that_removes_it_drops_the_mirror() {
    let dir = fresh_dir("catalog-keys");
    let path = dir.join("store");
    let mut first = Catalog::new(10, 12, 0);
    let tid = prepare(&path, &mut [&mut first]);
    let server = Server::start(options(&path)).unwrap();
    let mut writer = client(server.addr(), "1", false, DECLARED);
    let mut other = Catalog::new(10_000, 20, 1);
    let mut serials = HashMap::new();
    let records = other.model.records();
    assert_eq!(
        answer(&query(&mut writer, other.oid(), tid + 1, &of_type("Folder")).unwrap()),
        Err("not_registered".to_owned())
    );
    let tid = commit(
        &mut writer,
        &mut serials,
        &records,
        &[other.model.key()],
        &[],
    );
    let plan = of_type("Folder");
    assert_eq!(
        settled(&mut writer, other.oid(), tid + 1, &plan),
        Ok(other.rids("Folder"))
    );
    let tid = commit(&mut writer, &mut serials, &[], &[], &[other.model.key()]);
    assert!(until_reason(
        &mut writer,
        other.oid(),
        tid + 1,
        &plan,
        "not_registered"
    ));
    let admin = server.admin_addr().unwrap();
    assert_eq!(
        sample(admin, "zoe_catalog_mirror_ready", &mirror("1", other.oid())),
        None
    );
    server.stop();
}

#[test]
fn a_query_at_a_tid_gives_the_rids_of_that_tid_after_later_commits() {
    let dir = fresh_dir("catalog-snapshot");
    let path = dir.join("store");
    let mut catalog = Catalog::new(10, 20, 0);
    let records = catalog.model.records();
    let first = {
        let store = open_store(&path);
        testing::commit(&store, &records, &[catalog.model.key()], &[]).unwrap()
    };
    let server = Server::start(options(&path)).unwrap();
    let mut reader = client(server.addr(), "1", false, DECLARED);
    let mut writer = client(server.addr(), "1", false, DECLARED);
    let plan = of_type("Document");
    let before = catalog.rids("Document");
    assert_eq!(
        settled(&mut reader, catalog.oid(), first + 1, &plan),
        Ok(before.clone())
    );
    let mut serials = serials_of(&records, first);
    let mut tids = Vec::new();
    for i in 20..26 {
        catalog.add(i);
        let records = catalog.model.records();
        tids.push((
            commit(&mut writer, &mut serials, &records, &[], &[]),
            catalog.rids("Document"),
        ));
    }
    assert_ne!(tids.last().unwrap().1, before);
    // The snapshot of the first tid keeps its rids.
    let found = answer(&query(&mut reader, catalog.oid(), first + 1, &plan).unwrap());
    assert_eq!(found, Ok(before));
    for (tid, rids) in &tids {
        let found = answer(&query(&mut reader, catalog.oid(), tid + 1, &plan).unwrap());
        assert_eq!(found.as_ref(), Ok(rids), "at {tid:#x}");
    }
    server.stop();
}

#[test]
fn a_client_gets_an_answer_at_the_tid_of_each_invalidation_within_catalog_wait() {
    let dir = fresh_dir("catalog-invalidations");
    let path = dir.join("store");
    let mut catalog = Catalog::new(10, 10, 0);
    let records = catalog.model.records();
    let first = prepare_records(&path, &records, catalog.model.key());
    let server = Server::start(options(&path)).unwrap();
    let mut reader = client(server.addr(), "1", false, DECLARED);
    let mut writer = client(server.addr(), "1", false, DECLARED);
    let plan = of_type("News Item");
    settled(&mut reader, catalog.oid(), first + 1, &plan).unwrap();
    let mut serials = serials_of(&records, first);
    let mut expected = Vec::new();
    for i in 10..20 {
        catalog.add(i);
        let records = catalog.model.records();
        let tid = commit(&mut writer, &mut serials, &records, &[], &[]);
        expected.push((tid, catalog.rids("News Item")));
    }
    for (tid, rids) in expected {
        let args = reader
            .wait_for("invalidateTransaction", WAIT)
            .expect("an invalidation");
        let invalidated = u64_of(&common::items(&args)[0]);
        assert_eq!(invalidated, tid);
        let started = Instant::now();
        let found = answer(&query(&mut reader, catalog.oid(), tid + 1, &plan).unwrap());
        assert_eq!(found, Ok(rids), "at {tid:#x}");
        assert!(started.elapsed() < Duration::from_secs(1));
    }
    server.stop();
}

/// Commit `records` and the key `key` to the store in `path`. Return the
/// tid.
fn prepare_records(path: &Path, records: &[(u64, Vec<u8>)], key: [u8; 8]) -> u64 {
    let store = open_store(path);
    testing::commit(&store, records, &[key], &[]).unwrap()
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

#[test]
fn with_the_apply_held_a_newer_tid_gets_lag_and_the_commits_are_not_delayed() {
    let dir = fresh_dir("catalog-lag");
    let path = dir.join("store");
    let mut catalog = Catalog::new(10, 10, 0);
    let records = catalog.model.records();
    let first = prepare_records(&path, &records, catalog.model.key());
    let hold = Hold::default();
    let hook = hold.clone();
    let hooks = zoe_catalog::Hooks {
        before_apply: Some(Arc::new(move |_| hook.wait())),
        ..zoe_catalog::Hooks::default()
    };
    let wait = Duration::from_millis(300);
    let options = options(&path).catalog_wait(wait).catalog_hooks(hooks);
    let server = Server::start(options).unwrap();
    let mut reader = client(server.addr(), "1", false, DECLARED);
    let mut writer = client(server.addr(), "1", false, DECLARED);
    let plan = of_type("Document");
    settled(&mut reader, catalog.oid(), first + 1, &plan).unwrap();
    let mut serials = serials_of(&records, first);
    hold.set(true);
    let mut last = first;
    for i in 10..14 {
        catalog.add(i);
        let records = catalog.model.records();
        let started = Instant::now();
        last = commit(&mut writer, &mut serials, &records, &[], &[]);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "a commit waited for the mirror"
        );
    }
    let started = Instant::now();
    let found = answer(&query(&mut reader, catalog.oid(), last + 1, &plan).unwrap());
    assert_eq!(found, Err("lag".to_owned()));
    assert!(started.elapsed() >= wait);
    // The snapshot that the mirror holds still answers.
    let found = answer(&query(&mut reader, catalog.oid(), first + 1, &plan).unwrap());
    assert!(found.is_ok(), "{found:?}");
    let admin = server.admin_addr().unwrap();
    let lag = sample(
        admin,
        "zoe_catalog_mirror_lag_tids",
        &mirror("1", catalog.oid()),
    );
    assert!(lag.is_some_and(|lag| lag >= 1.0), "{lag:?}");
    hold.set(false);
    let deadline = Instant::now() + WAIT;
    let found = loop {
        let found = answer(&query(&mut reader, catalog.oid(), last + 1, &plan).unwrap());
        if found.as_ref().err().map(String::as_str) != Some("lag") || Instant::now() > deadline {
            break found;
        }
    };
    assert_eq!(found, Ok(catalog.rids("Document")));
    server.stop();
}

#[test]
fn a_connection_that_did_not_declare_the_extension_gets_value_error_and_stays_open() {
    let dir = fresh_dir("catalog-undeclared");
    let path = dir.join("store");
    let mut catalog = Catalog::new(10, 5, 0);
    let tid = prepare(&path, &mut [&mut catalog]);
    let server = Server::start(options(&path)).unwrap();
    let mut stock = Conn::open(server.addr());
    stock.register("1", false).unwrap();
    let info = stock.call("get_info", vec![]).unwrap();
    assert!(
        get(&info, "extensions").is_none(),
        "a stock client sees extensions"
    );
    let fault = query(&mut stock, catalog.oid(), tid + 1, &of_type("Document")).unwrap_err();
    assert_eq!(fault.class, "builtins.ValueError");
    assert_eq!(stock.call("ping", vec![]).unwrap(), Value::Nil);
    // A declaration of other extensions does not give it either.
    let mut other = client(server.addr(), "1", false, &["kv"]);
    let fault = query(&mut other, catalog.oid(), tid + 1, &of_type("Document")).unwrap_err();
    assert_eq!(fault.class, "builtins.ValueError");
    let info = other.call("get_info", vec![]).unwrap();
    assert_eq!(get(&info, "extensions"), Some(&Value::Array(vec![s("kv")])));
    let mut ours = client(server.addr(), "1", false, &["catalog_query"]);
    let info = ours.call("get_info", vec![]).unwrap();
    assert_eq!(
        get(&info, "extensions"),
        Some(&Value::Array(vec![s("catalog_query")]))
    );
    server.stop();
}

#[test]
fn a_tid_outside_the_mirror_window_gets_outside_window() {
    let dir = fresh_dir("catalog-window");
    let path = dir.join("store");
    let mut catalog = Catalog::new(10, 10, 0);
    let records = catalog.model.records();
    let first = prepare_records(&path, &records, catalog.model.key());
    let options = options(&path).catalog_window_tids(2).catalog_rebase_rows(0);
    let server = Server::start(options).unwrap();
    let mut conn = client(server.addr(), "1", false, DECLARED);
    let plan = of_type("Document");
    settled(&mut conn, catalog.oid(), first + 1, &plan).unwrap();
    // The snapshot before the commit of the catalog is before the base.
    let found = answer(&query(&mut conn, catalog.oid(), first, &plan).unwrap());
    assert_eq!(found, Err("outside_window".to_owned()));
    let mut serials = serials_of(&records, first);
    let mut tids = vec![first];
    for i in 10..17 {
        catalog.add(i);
        let records = catalog.model.records();
        tids.push(commit(&mut conn, &mut serials, &records, &[], &[]));
    }
    let last = *tids.last().unwrap();
    assert_eq!(
        answer(&query(&mut conn, catalog.oid(), last + 1, &plan).unwrap()),
        Ok(catalog.rids("Document"))
    );
    assert!(until_reason(
        &mut conn,
        catalog.oid(),
        tids[1] + 1,
        &plan,
        "outside_window"
    ));
    server.stop();
}

#[test]
fn a_plan_that_does_not_decode_gets_value_error_and_an_unknown_kind_gets_plan() {
    let dir = fresh_dir("catalog-plans");
    let path = dir.join("store");
    let mut catalog = Catalog::new(10, 8, 0);
    let tid = prepare(&path, &mut [&mut catalog]);
    let server = Server::start(options(&path)).unwrap();
    let mut conn = client(server.addr(), "1", false, DECLARED);
    let oid = catalog.oid();
    settled(&mut conn, oid, tid + 1, &of_type("Document")).unwrap();
    for bad in [
        s("not a map"),
        map(vec![("c", s("not a list"))]),
        map(vec![("s", Value::Array(vec![]))]),
        plan(map(vec![("i", s("portal_type"))])),
    ] {
        let fault = query(&mut conn, oid, tid + 1, &bad).unwrap_err();
        assert_eq!(fault.class, "builtins.ValueError", "{bad:?}");
    }
    // The connection stays open.
    assert_eq!(conn.call("ping", vec![]).unwrap(), Value::Nil);
    let text = plan(map(vec![
        ("k", s("t")),
        ("i", s("SearchableText")),
        ("q", s("word")),
    ]));
    let found = answer(&query(&mut conn, oid, tid + 1, &text).unwrap());
    assert_eq!(found, Err("plan".to_owned()));
    let mut unknown_key = of_type("Document");
    if let Value::Map(items) = &mut unknown_key {
        items.push((s("o"), Value::Array(vec![])));
    }
    let found = answer(&query(&mut conn, oid, tid + 1, &unknown_key).unwrap());
    assert_eq!(found, Err("plan".to_owned()));
    // The kind of a clause must read the class of its index.
    let boolean_on_a_field = plan(map(vec![
        ("k", s("b")),
        ("i", s("portal_type")),
        ("q", Value::Boolean(true)),
    ]));
    let found = answer(&query(&mut conn, oid, tid + 1, &boolean_on_a_field).unwrap());
    assert_eq!(found, Err("plan".to_owned()));
    let range_on_a_boolean = plan(map(vec![
        ("k", s("d")),
        ("i", s("is_folderish")),
        ("t", Value::from(1)),
    ]));
    let found = answer(&query(&mut conn, oid, tid + 1, &range_on_a_boolean).unwrap());
    assert_eq!(found, Err("plan".to_owned()));
    let boolean = plan(map(vec![
        ("k", s("b")),
        ("i", s("is_folderish")),
        ("q", Value::Boolean(true)),
    ]));
    let found = answer(&query(&mut conn, oid, tid + 1, &boolean).unwrap());
    assert_eq!(found.map(|r| r.len()), Ok(4));
    // The server reads the transaction overlay: a record of an object that
    // the mirror does not read leaves the answer native.
    let overlay = Value::Array(vec![Value::Array(vec![p64(1), Value::Binary(vec![])])]);
    let reply = conn
        .call(
            "catalog_query",
            vec![p64(oid), p64(tid + 1), of_type("Document"), overlay],
        )
        .unwrap();
    assert_eq!(answer(&reply), Ok(catalog.rids("Document")));
    assert_eq!(get(&reply, "tid"), Some(&p64(tid + 1)));
    server.stop();
}

#[test]
fn a_read_only_store_answers() {
    let dir = fresh_dir("catalog-read-only");
    let path = dir.join("store");
    let mut catalog = Catalog::new(10, 9, 0);
    let tid = prepare(&path, &mut [&mut catalog]);
    let storage = StorageOptions::new("1", &path).read_only(true);
    let server = Server::start(ServeOptions::with_storages([storage])).unwrap();
    let mut conn = client(server.addr(), "1", true, DECLARED);
    let plan = of_type("Folder");
    assert_eq!(
        settled(&mut conn, catalog.oid(), tid + 1, &plan),
        Ok(catalog.rids("Folder"))
    );
    server.stop();
}

/// The bytes of the mirror of `catalog` in the store in `path`, from a
/// server with the default bound.
fn bytes_of_mirror(path: &Path, catalog: &Catalog, tid: u64) -> f64 {
    let server = Server::start(options(path)).unwrap();
    let mut conn = client(server.addr(), "1", false, DECLARED);
    settled(&mut conn, catalog.oid(), tid + 1, &of_type("Document")).unwrap();
    let admin = server.admin_addr().unwrap();
    let bytes = sample(
        admin,
        "zoe_catalog_mirror_bytes",
        &mirror("1", catalog.oid()),
    )
    .unwrap();
    drop(conn);
    server.stop();
    bytes
}

#[test]
fn two_named_storages_keep_their_mirrors_apart_each_under_its_own_bound() {
    let dir = fresh_dir("catalog-named");
    let (one, two) = (dir.join("one"), dir.join("two"));
    // The same oids in both stores, with other types.
    let mut a = Catalog::new(10, 40, 0);
    let mut b = Catalog::new(10, 40, 1);
    let tid_a = prepare(&one, &mut [&mut a]);
    let tid_b = prepare(&two, &mut [&mut b]);
    let bytes = bytes_of_mirror(&one, &a, tid_a);
    let options = ServeOptions::with_storages([
        StorageOptions::new("one", &one),
        StorageOptions::new("two", &two),
    ])
    .admin_listen(SocketAddr::from(([127, 0, 0, 1], 0)))
    .catalog_memory((bytes * 1.5) as usize);
    let server = Server::start(options).unwrap();
    let mut ca = client(server.addr(), "one", false, DECLARED);
    let mut cb = client(server.addr(), "two", false, DECLARED);
    let plan = of_type("Document");
    assert_eq!(
        settled(&mut ca, a.oid(), tid_a + 1, &plan),
        Ok(a.rids("Document"))
    );
    assert_eq!(
        settled(&mut cb, b.oid(), tid_b + 1, &plan),
        Ok(b.rids("Document"))
    );
    assert_ne!(a.rids("Document"), b.rids("Document"));
    let admin = server.admin_addr().unwrap();
    for name in ["one", "two"] {
        let labels = format!("storage=\"{name}\"");
        assert_eq!(
            sample(admin, "zoe_catalog_mirror_drops_total", &labels),
            Some(0.0)
        );
        let labels = mirror(name, a.oid());
        assert_eq!(
            sample(admin, "zoe_catalog_mirror_ready", &labels),
            Some(1.0)
        );
    }
    server.stop();
}

#[test]
fn past_the_bound_the_mirror_that_answered_least_recently_drops() {
    let dir = fresh_dir("catalog-bound");
    let path = dir.join("store");
    let mut a = Catalog::new(10, 40, 0);
    let tid = prepare(&path, &mut [&mut a]);
    let bytes = bytes_of_mirror(&path, &a, tid);
    let options = options(&path).catalog_memory((bytes * 1.5) as usize);
    let server = Server::start(options).unwrap();
    let mut conn = client(server.addr(), "1", false, DECLARED);
    let plan = of_type("Document");
    assert_eq!(
        settled(&mut conn, a.oid(), tid + 1, &plan),
        Ok(a.rids("Document"))
    );
    // A second catalog of the same size passes the bound.
    let mut b = Catalog::new(100_000, 40, 1);
    let records = b.model.records();
    let mut serials = HashMap::new();
    let tid = commit(&mut conn, &mut serials, &records, &[b.model.key()], &[]);
    assert_eq!(
        settled(&mut conn, b.oid(), tid + 1, &plan),
        Ok(b.rids("Document"))
    );
    let found = answer(&query(&mut conn, a.oid(), tid + 1, &plan).unwrap());
    assert_eq!(found, Err("dropped".to_owned()));
    let admin = server.admin_addr().unwrap();
    assert_eq!(
        sample(admin, "zoe_catalog_mirror_drops_total", "storage=\"1\""),
        Some(1.0)
    );
    assert_eq!(
        sample(
            admin,
            "zoe_catalog_fallbacks_total",
            "storage=\"1\",reason=\"dropped\",side=\"server\""
        ),
        Some(1.0)
    );
    server.stop();
}

#[test]
fn the_metrics_of_the_catalog_have_the_label_storage() {
    let dir = fresh_dir("catalog-metrics");
    let path = dir.join("store");
    let mut catalog = Catalog::new(10, 10, 0);
    let tid = prepare(&path, &mut [&mut catalog]);
    let server = Server::start(options(&path)).unwrap();
    let mut conn = client(server.addr(), "1", false, DECLARED);
    settled(&mut conn, catalog.oid(), tid + 1, &of_type("Document")).unwrap();
    let admin = server.admin_addr().unwrap();
    let text = metrics(admin);
    for name in [
        "zoe_catalog_queries_total",
        "zoe_catalog_query_latency_seconds",
        "zoe_catalog_mirror_ready",
        "zoe_catalog_mirror_build_seconds",
        "zoe_catalog_mirror_lag_tids",
        "zoe_catalog_fallbacks_total",
        "zoe_catalog_mirror_bytes",
        "zoe_catalog_mirror_drops_total",
    ] {
        assert!(
            text.contains(&format!("# TYPE {name} ")),
            "no {name}:\n{text}"
        );
    }
    let one = "storage=\"1\"";
    assert!(
        sample(
            admin,
            "zoe_catalog_queries_total",
            &format!("{one},path=\"native\"")
        ) >= Some(1.0)
    );
    assert!(sample(admin, "zoe_catalog_query_latency_seconds_count", one) >= Some(1.0));
    assert_eq!(
        sample(
            admin,
            "zoe_catalog_mirror_lag_tids",
            &mirror("1", catalog.oid())
        ),
        Some(0.0)
    );
    assert!(
        sample(
            admin,
            "zoe_catalog_mirror_bytes",
            &mirror("1", catalog.oid())
        ) > Some(0.0)
    );
    server.stop();
}
