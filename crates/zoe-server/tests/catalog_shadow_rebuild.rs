//! Acceptance test of the shadow rebuild of a catalog mirror through `zoe
//! serve` (ADR 0059, section 5): the way back from `stale`.
//!
//! Clients commit and query. A commit adds an index to the catalog, or
//! clears an index and fills it again. The mirror builds the changed index
//! in the background, beside its base, while a test hook holds that build.
//! Each `catalog_query` meanwhile is answered, or gets `stale` only when it
//! reads the changed index at the tid of that commit or later. After the
//! swap, each query is answered with the rids that the catalog holds at
//! its tid. A commit never waits for the shadow rebuild, and the shadow
//! rebuild runs in a thread of the mirrors, never in the hub or on the
//! read pool.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use zoe_catalog::testing::{self, CatalogModel, IndexKind};
use zoe_catalog::value::Val;
use zoe_server::serve::{ServeOptions, Server};

mod common;

use common::{Conn, Fault, Value, fresh_dir, open_store, p64, u64_of};

/// The longest wait of a test for the server.
const WAIT: Duration = Duration::from_secs(30);

const TYPES: [&str; 3] = ["Document", "Folder", "News Item"];

const STATES: [&str; 2] = ["published", "private"];

const DECLARED: &[&str] = &["kv", "catalog_query"];

/// A catalog with a FieldIndex `portal_type`, a BooleanIndex `is_folderish`
/// and the column `getId`. The entry `i` has the rid `1000 + 2 i`, the type
/// `TYPES[(i + shift) % 3]` and, once the catalog has the index, the state
/// `STATES[(i + shift) % 2]` in the FieldIndex `review_state`.
struct Catalog {
    model: CatalogModel,
    shift: usize,
    /// The type, the state and `is_folderish` of each rid.
    rows: HashMap<i32, (&'static str, &'static str, bool)>,
}

impl Catalog {
    fn new(n: usize) -> Catalog {
        let model = CatalogModel::new(
            10,
            &[
                ("portal_type", IndexKind::Field),
                ("is_folderish", IndexKind::Boolean),
            ],
            &["getId"],
        );
        let mut catalog = Catalog {
            model,
            shift: 0,
            rows: HashMap::new(),
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
        let state = STATES[(i + self.shift) % 2];
        let folderish = i.is_multiple_of(2);
        self.rows.insert(rid, (kind, state, folderish));
        self.model.catalog(
            rid,
            &format!("/site/item-{i}"),
            &[
                ("portal_type", Val::Str(kind.into())),
                ("review_state", Val::Str(state.into())),
                ("is_folderish", Val::Bool(folderish)),
                ("getId", Val::Str(format!("item-{i}").into())),
            ],
        );
    }

    fn oid(&self) -> u64 {
        self.model.oid()
    }

    /// The rids whose row passes `test`, in ascending order.
    fn rids(&self, test: impl Fn(&(&str, &str, bool)) -> bool) -> Vec<i32> {
        let mut rids: Vec<i32> = self
            .rows
            .iter()
            .filter(|(_, row)| test(row))
            .map(|(rid, _)| *rid)
            .collect();
        rids.sort_unstable();
        rids
    }
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

/// The plan `index == key` on a FieldIndex.
fn field(index: &str, key: &str) -> Value {
    plan(map(vec![
        ("k", s("v")),
        ("i", s(index)),
        ("q", Value::Array(vec![s(key)])),
        ("and", Value::Boolean(false)),
        ("r", Value::Nil),
        ("not", Value::Array(vec![])),
    ]))
}

/// The plan `is_folderish == true`.
fn folderish() -> Value {
    plan(map(vec![
        ("k", s("b")),
        ("i", s("is_folderish")),
        ("q", Value::Boolean(true)),
    ]))
}

/// Connect to `addr` and register with the storage `1`, declaring the
/// extensions.
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

/// Query until the reply passes `done`, at most `WAIT`. Return the last
/// answer.
fn until(
    conn: &mut Conn,
    catalog: u64,
    tid: u64,
    plan: &Value,
    done: impl Fn(&Result<Vec<i32>, String>) -> bool,
) -> Result<Vec<i32>, String> {
    let deadline = Instant::now() + WAIT;
    loop {
        let found = answer(&query(conn, catalog, tid, plan).unwrap());
        if done(&found) || Instant::now() > deadline {
            return found;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Commit `records` through `conn`. `serials` gives the serial of each oid,
/// and takes the tid of the commit. Return the tid, after a check that the
/// commit did not wait.
fn commit(conn: &mut Conn, serials: &mut HashMap<u64, u64>, records: &[(u64, Vec<u8>)]) -> u64 {
    let started = Instant::now();
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
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "a commit waited for the mirror"
    );
    tid
}

/// A hook that holds its caller while it is on, tells each call that it
/// holds, and keeps the name of the thread of each call.
#[derive(Clone)]
struct Gate {
    on: Arc<(Mutex<bool>, Condvar)>,
    reached: Arc<Mutex<mpsc::Sender<u64>>>,
    threads: Arc<Mutex<Vec<String>>>,
}

impl Gate {
    fn new() -> (Gate, mpsc::Receiver<u64>) {
        let (tx, rx) = mpsc::channel();
        let gate = Gate {
            on: Arc::new((Mutex::new(false), Condvar::new())),
            reached: Arc::new(Mutex::new(tx)),
            threads: Arc::default(),
        };
        (gate, rx)
    }

    fn set(&self, on: bool) {
        *self.on.0.lock().unwrap() = on;
        self.on.1.notify_all();
    }

    fn hook(&self) -> zoe_catalog::mirrors::Hook {
        let gate = self.clone();
        Arc::new(move |n| {
            let name = std::thread::current().name().unwrap_or("").to_owned();
            gate.threads.lock().unwrap().push(name);
            let mut on = gate.on.0.lock().unwrap();
            if !*on {
                return;
            }
            let _ = gate.reached.lock().unwrap().send(n);
            while *on {
                on = gate.on.1.wait(on).unwrap();
            }
        })
    }
}

/// The server over a store in `path` that holds `catalog` and its key, with
/// the hook `gate` before each build of a mirror.
fn start(path: &Path, catalog: &mut Catalog, gate: &Gate) -> (Server, u64, HashMap<u64, u64>) {
    let records = catalog.model.records();
    let first = {
        let store = open_store(path);
        testing::commit(&store, &records, &[catalog.model.key()], &[]).unwrap()
    };
    let hooks = zoe_catalog::Hooks {
        before_build: Some(gate.hook()),
        ..zoe_catalog::Hooks::default()
    };
    let options = ServeOptions::new(path)
        .admin_listen(SocketAddr::from(([127, 0, 0, 1], 0)))
        .catalog_hooks(hooks);
    let server = Server::start(options).unwrap();
    let serials = records.iter().map(|(oid, _)| (*oid, first)).collect();
    (server, first, serials)
}

/// The expected answers of the three plans at one tid.
struct Expected {
    tid: u64,
    documents: Vec<i32>,
    folderish: Vec<i32>,
    published: Vec<i32>,
}

fn expected(catalog: &Catalog, tid: u64) -> Expected {
    Expected {
        tid,
        documents: catalog.rids(|row| row.0 == "Document"),
        folderish: catalog.rids(|row| row.2),
        published: catalog.rids(|row| row.1 == "published"),
    }
}

/// The text of `GET /metrics` on the admin port `admin`.
fn metrics(admin: SocketAddr) -> String {
    use std::io::{Read, Write};
    let mut stream = std::net::TcpStream::connect(admin).unwrap();
    stream
        .write_all(b"GET /metrics HTTP/1.1\r\nHost: zoe\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut text = String::new();
    stream.read_to_string(&mut text).unwrap();
    text
}

/// One case: `change` writes the stale commit into the catalog; `changed`
/// tells which plans read the index that it changes, by their names
/// `documents`, `folderish` and `published`; `before` gives the answer of
/// the plan `published` before the stale commit.
fn run(
    name: &str,
    change: fn(&mut Catalog),
    changed: &[&str],
    before: fn(&Expected) -> Result<Vec<i32>, String>,
) {
    let dir = fresh_dir(name);
    let path = dir.join("store");
    let mut catalog = Catalog::new(30);
    if !changed.contains(&"published") {
        catalog.model.add_index("review_state", IndexKind::Field);
        for i in 0..30 {
            catalog.add(i);
        }
    }
    let (gate, reached) = Gate::new();
    let (server, first, mut serials) = start(&path, &mut catalog, &gate);
    let oid = catalog.oid();
    let mut reader = client(server.addr());
    let mut writer = client(server.addr());
    let documents = field("portal_type", "Document");
    let published = field("review_state", "published");
    let found = until(&mut reader, oid, first + 1, &documents, |f| {
        !matches!(
            f.as_ref().err().map(String::as_str),
            Some("not_ready" | "not_registered")
        )
    });
    assert!(found.is_ok(), "{name}: {found:?}");
    let mut seen = vec![expected(&catalog, first)];
    for i in 30..33 {
        catalog.add(i);
    }
    let tid = commit(&mut writer, &mut serials, &catalog.model.records());
    seen.push(expected(&catalog, tid));
    gate.set(true);
    change(&mut catalog);
    let stale = commit(&mut writer, &mut serials, &catalog.model.records());
    seen.push(expected(&catalog, stale));
    assert_eq!(reached.recv_timeout(WAIT), Ok(oid), "{name}");
    // Clients commit and query while the shadow rebuild waits.
    for i in 33..36 {
        catalog.add(i);
        catalog.add(i - 30);
        let tid = commit(&mut writer, &mut serials, &catalog.model.records());
        seen.push(expected(&catalog, tid));
    }
    let last = seen.last().unwrap().tid;
    for e in &seen {
        let reads = |plan: &str| changed.contains(&plan) && e.tid >= stale;
        let documents_found = answer(&query(&mut reader, oid, e.tid + 1, &documents).unwrap());
        let folderish_found = answer(&query(&mut reader, oid, e.tid + 1, &folderish()).unwrap());
        let published_found = answer(&query(&mut reader, oid, e.tid + 1, &published).unwrap());
        let what = format!("{name} at {:#x}, stale at {stale:#x}", e.tid);
        for (plan, found, wanted) in [
            ("documents", documents_found, Ok(e.documents.clone())),
            ("folderish", folderish_found, Ok(e.folderish.clone())),
            (
                "published",
                published_found,
                if e.tid < stale {
                    before(e)
                } else {
                    Ok(e.published.clone())
                },
            ),
        ] {
            if reads(plan) {
                assert_eq!(found, Err("stale".to_owned()), "{what}: {plan}");
            } else {
                assert_eq!(found, wanted, "{what}: {plan}");
            }
        }
    }
    gate.set(false);
    // The swap: the plan of the changed index is answered again.
    let watched = if changed.contains(&"documents") {
        &documents
    } else {
        &published
    };
    let found = until(&mut reader, oid, last + 1, watched, |f| f.is_ok());
    assert!(found.is_ok(), "{name}: {found:?}");
    // After the swap, each query at the stale commit and later is answered
    // with the rids of its tid.
    for e in seen.iter().filter(|e| e.tid >= stale) {
        let what = format!("{name} after the swap at {:#x}", e.tid);
        let found = answer(&query(&mut reader, oid, e.tid + 1, &documents).unwrap());
        assert_eq!(found, Ok(e.documents.clone()), "{what}");
        let found = answer(&query(&mut reader, oid, e.tid + 1, &folderish()).unwrap());
        assert_eq!(found, Ok(e.folderish.clone()), "{what}");
        let found = answer(&query(&mut reader, oid, e.tid + 1, &published).unwrap());
        assert_eq!(found, Ok(e.published.clone()), "{what}");
    }
    // A commit after the swap applies to the new base.
    catalog.add(40);
    let tid = commit(&mut writer, &mut serials, &catalog.model.records());
    let after = expected(&catalog, tid);
    let found = until(&mut reader, oid, tid + 1, &published, |f| {
        f.as_ref().err().map(String::as_str) != Some("lag")
    });
    assert_eq!(found, Ok(after.published), "{name} after a later commit");
    // The shadow rebuild ran in a thread of the mirrors.
    let threads = gate.threads.lock().unwrap().clone();
    assert!(
        threads.iter().any(|t| t == "zoe-catalog-shadow"),
        "{threads:?}"
    );
    assert!(
        threads.iter().all(|t| t.starts_with("zoe-catalog")),
        "{threads:?}"
    );
    let text = metrics(server.admin_addr().unwrap());
    assert!(
        text.lines().any(
            |l| l.starts_with("zoe_catalog_fallbacks_total{") && l.contains("reason=\"stale\"")
        ),
        "{text}"
    );
    assert!(
        text.lines()
            .any(|l| l.starts_with("zoe_catalog_mirror_build_seconds{")),
        "{text}"
    );
    server.stop();
}

#[test]
fn a_commit_that_adds_an_index_rebuilds_it_in_the_shadow_while_queries_are_answered() {
    run(
        "catalog-shadow-add",
        |catalog| {
            catalog.model.add_index("review_state", IndexKind::Field);
            for i in 0..33 {
                catalog.add(i);
            }
        },
        &["published"],
        // Before the commit, the catalog has no index `review_state`.
        |_| Err("index_unavailable".to_owned()),
    );
}

#[test]
fn a_commit_that_clears_and_fills_an_index_again_rebuilds_it_in_the_shadow() {
    run(
        "catalog-shadow-clear",
        |catalog| {
            catalog.model.clear_index("portal_type");
            catalog.shift = 1;
            for i in 0..33 {
                catalog.add(i);
            }
        },
        &["documents"],
        |e| Ok(e.published.clone()),
    );
}
