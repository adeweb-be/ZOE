//! Tests of the prefetch of the references of a record that the server
//! served to a `Pool`.
//!
//! When `Pool::load_before` fetches a record from the server, a pool with
//! the flag `set_prefetch_references(true)` and a cache reads the
//! persistent ids of the record, and prefetches the oids of the same
//! database that the cache does not hold for the tid of the load, as
//! `Pool::prefetch` does. A record that a prefetch brings is not scanned.
//! Weak references and references into another database are not
//! prefetched. A record that does not decode prefetches nothing. One
//! record prefetches at most `PREFETCH_REFERENCES` oids, and a record of
//! more than `PREFETCH_SCAN_LIMIT` bytes is not scanned.
//!
//! The tests run `zoe serve` in the test process with
//! `zoe_server::serve::Server` and an admin port, and read the count of the
//! loads of the server, `zoe_loads_total`, from `GET /metrics`. The server
//! counts a load before its reply goes out. A record is a
//! `PersistentMapping` as ZODB writes it, built with the pickler of
//! `zoe_pickle::python`.

mod common;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use common::{Started, commit, open_cached, record, start_with};
use zoe_client::{Client, PREFETCH_REFERENCES, PREFETCH_SCAN_LIMIT, Pool};
use zoe_core::{Oid, Tid};
use zoe_pickle::python::{Heap, Pickler, Value};
use zoe_server::serve::ServeOptions;

const TIMEOUT: Duration = Duration::from_secs(30);

/// The time that a test gives a prefetch that must not happen. A prefetch
/// on the loopback ends well within it.
const SETTLE: Duration = Duration::from_millis(300);

const MODULE: &str = "persistent.mapping";
const CLASS: &str = "PersistentMapping";

/// A persistent id of a test record.
#[derive(Clone, Copy)]
enum Ref {
    /// `(oid, class)`: an object of the same database.
    Oid(Oid),
    /// `['w', (oid,)]`: a weak reference.
    Weak(Oid),
    /// `['m', (database, oid, class)]`: an object of another database.
    Multi(Oid),
    /// `['n', (database, oid)]`: an object of another database.
    MultiOid(Oid),
}

/// Start a server with an admin port for the test `name`.
fn start(name: &str) -> Started {
    start_with(name, |path| {
        ServeOptions::new(path).admin_listen(SocketAddr::from(([127, 0, 0, 1], 0)))
    })
}

/// Return the loads that the server counted for the storage 1.
fn loads(server: &Started) -> u64 {
    let admin = server.admin_addr().expect("the server has an admin port");
    let mut stream = TcpStream::connect(admin).unwrap();
    stream.set_read_timeout(Some(TIMEOUT)).unwrap();
    write!(
        stream,
        "GET /metrics HTTP/1.1\r\nHost: zoe\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let prefix = "zoe_loads_total{storage=\"1\"} ";
    let value = response
        .lines()
        .find_map(|line| line.strip_prefix(prefix))
        .unwrap_or_else(|| panic!("no sample of zoe_loads_total:\n{response}"));
    value.trim().parse::<f64>().unwrap() as u64
}

/// Return the persistent reference of `reference` in `heap`.
fn reference(heap: &mut Heap, reference: Ref) -> Value {
    let bytes = |heap: &mut Heap, oid: Oid| heap.new_bytes(&oid.0.to_be_bytes());
    let data = match reference {
        Ref::Oid(oid) => {
            let oid = bytes(heap, oid);
            let class = heap.new_class(MODULE, CLASS);
            heap.new_tuple(vec![oid, class])
        }
        Ref::Weak(oid) => {
            let kind = heap.new_str("w");
            let oid = bytes(heap, oid);
            let args = heap.new_tuple(vec![oid]);
            heap.new_list(vec![kind, args])
        }
        Ref::Multi(oid) => {
            let kind = heap.new_str("m");
            let database = heap.new_str("other");
            let oid = bytes(heap, oid);
            let class = heap.new_class(MODULE, CLASS);
            let args = heap.new_tuple(vec![database, oid, class]);
            heap.new_list(vec![kind, args])
        }
        Ref::MultiOid(oid) => {
            let kind = heap.new_str("n");
            let database = heap.new_str("other");
            let oid = bytes(heap, oid);
            let args = heap.new_tuple(vec![database, oid]);
            heap.new_list(vec![kind, args])
        }
    };
    heap.new_reference(data).unwrap()
}

/// Return the record of a `PersistentMapping` whose `data` maps one key to
/// each persistent id of `refs`, in their order, and one more key to
/// `padding` bytes when `padding` is above 0.
fn mapping(refs: &[Ref], padding: usize) -> Vec<u8> {
    let mut heap = Heap::new();
    let class = heap.new_class(MODULE, CLASS);
    let mut items = Vec::new();
    for (n, r) in refs.iter().enumerate() {
        let key = heap.new_str(&format!("k{n:04}"));
        items.push((key, reference(&mut heap, *r)));
    }
    if padding > 0 {
        let key = heap.new_str("padding");
        items.push((key, heap.new_bytes(&vec![b'x'; padding])));
    }
    let data = heap.new_dict(items);
    let name = heap.new_str("data");
    let state = heap.new_dict(vec![(name, data)]);
    let mut pickler = Pickler::new(&heap);
    pickler.dump(class).unwrap();
    pickler.dump(state).unwrap();
    pickler.into_bytes()
}

/// Commit `stores` (oid and data) as new objects through a client of
/// `server`, in one transaction. Return the tid after the commit, the
/// `before` of a load of a connection that starts then.
async fn commit_new(server: &Started, stores: &[(Oid, Vec<u8>)]) -> Tid {
    let other = Client::connect(server.addr(), "1").await.unwrap();
    let stores: Vec<_> = stores
        .iter()
        .map(|(oid, data)| (*oid, Tid(0), data.clone()))
        .collect();
    let tid = commit(&other, &stores).await.unwrap();
    Tid(tid.0 + 1)
}

/// Return `n` new oids of `server`.
async fn new_oids(server: &Started, n: usize) -> Vec<Oid> {
    let other = Client::connect(server.addr(), "1").await.unwrap();
    other.new_oids(n).await.unwrap()
}

/// Open a pool with a cache on `server`, with the flag `on`.
async fn open(server: &Started, on: bool) -> Pool {
    let pool = open_cached(server, 2, None).await;
    pool.set_prefetch_references(on);
    pool
}

/// Tell if the cache of `pool` holds `oid` for `before`.
fn holds(pool: &Pool, oid: Oid, before: Tid) -> bool {
    pool.cache().unwrap().load_before(oid, before).is_some()
}

/// Return the oids of the entries of the cache of `pool`, sorted.
fn cached(pool: &Pool) -> Vec<Oid> {
    let mut oids: Vec<_> = pool
        .cache()
        .unwrap()
        .entries()
        .into_iter()
        .map(|(oid, _, _)| oid)
        .collect();
    oids.sort();
    oids
}

async fn wait_until(mut check: impl FnMut() -> bool, what: &str) {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while !check() {
        assert!(tokio::time::Instant::now() < deadline, "no {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Load `oid` before `before` through `pool`, and check that the record is
/// `data`.
async fn load(pool: &Pool, oid: Oid, before: Tid, data: &[u8]) {
    let (record, _) = tokio::time::timeout(TIMEOUT, pool.load_before(oid, before))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(record.expect("the record of the load").data, data);
}

#[tokio::test]
async fn the_children_of_a_record_are_in_the_cache_after_a_miss() {
    let server = start("children");
    let oids = new_oids(&server, 21).await;
    let (parent, children) = (oids[0], &oids[1..]);
    let refs: Vec<_> = children.iter().map(|&oid| Ref::Oid(oid)).collect();
    let data = mapping(&refs, 0);
    let mut stores = vec![(parent, data.clone())];
    stores.extend(children.iter().map(|&oid| (oid, record(b"child"))));
    let before = commit_new(&server, &stores).await;

    let pool = open(&server, true).await;
    let counted = loads(&server);
    load(&pool, parent, before, &data).await;
    wait_until(
        || children.iter().all(|&oid| holds(&pool, oid, before)),
        "prefetch of the children",
    )
    .await;
    assert_eq!(loads(&server) - counted, 21);
    // Each child is the current record of the commit.
    let start = Tid(before.0 - 1);
    for &oid in children {
        let entry = pool.cache().unwrap().load_before(oid, before).unwrap();
        assert_eq!(
            (entry.data, entry.tid, entry.end),
            (record(b"child"), start, None)
        );
    }
    // A hit of the parent sends nothing.
    tokio::time::sleep(SETTLE).await;
    assert_eq!(loads(&server) - counted, 21);
}

#[tokio::test]
async fn the_references_of_a_prefetched_record_are_not_followed() {
    let server = start("depth");
    let oids = new_oids(&server, 3).await;
    let (a, b, c) = (oids[0], oids[1], oids[2]);
    let data = mapping(&[Ref::Oid(b)], 0);
    let stores = [
        (a, data.clone()),
        (b, mapping(&[Ref::Oid(c)], 0)),
        (c, record(b"leaf")),
    ];
    let before = commit_new(&server, &stores).await;

    let pool = open(&server, true).await;
    let counted = loads(&server);
    load(&pool, a, before, &data).await;
    wait_until(|| holds(&pool, b, before), "prefetch of the child").await;
    tokio::time::sleep(SETTLE).await;
    assert!(
        !holds(&pool, c, before),
        "the prefetch of b loaded its child"
    );
    assert_eq!(cached(&pool), [a, b]);
    assert_eq!(loads(&server) - counted, 2);
}

#[tokio::test]
async fn weak_foreign_and_missing_references_are_not_prefetched() {
    let server = start("forms");
    let oids = new_oids(&server, 7).await;
    let (parent, child) = (oids[0], oids[1]);
    let (weak, multi, multi_oid) = (oids[2], oids[3], oids[4]);
    // An oid that the server gave and that no commit wrote: its load fails.
    let missing = oids[5];
    let unrelated = oids[6];
    let refs = [
        Ref::Weak(weak),
        Ref::Multi(multi),
        Ref::MultiOid(multi_oid),
        Ref::Oid(missing),
        Ref::Oid(child),
    ];
    let data = mapping(&refs, 0);
    let mut stores = vec![(parent, data.clone()), (child, record(b"child"))];
    // The targets of the weak and foreign references exist in this
    // database, so a load of them would succeed.
    stores.extend([weak, multi, multi_oid, unrelated].map(|oid| (oid, record(b"other"))));
    let before = commit_new(&server, &stores).await;

    let pool = open(&server, true).await;
    let counted = loads(&server);
    // The failed load of `missing` raises nothing to the caller.
    load(&pool, parent, before, &data).await;
    wait_until(|| holds(&pool, child, before), "prefetch of the child").await;
    tokio::time::sleep(SETTLE).await;
    let mut expected = vec![parent, child];
    expected.sort();
    assert_eq!(cached(&pool), expected);
    // The parent, the child and the failed load of `missing`.
    assert_eq!(loads(&server) - counted, 3);
}

#[tokio::test]
async fn a_record_that_does_not_decode_prefetches_nothing() {
    let server = start("refused");
    let oids = new_oids(&server, 2).await;
    let (parent, other) = (oids[0], oids[1]);
    // Bytes that are not a pickle, with the 8 bytes of an oid inside.
    let mut data = b"not a pickle ".to_vec();
    data.extend_from_slice(&other.0.to_be_bytes());
    let stores = [(parent, data.clone()), (other, record(b"other"))];
    let before = commit_new(&server, &stores).await;

    let pool = open(&server, true).await;
    let counted = loads(&server);
    load(&pool, parent, before, &data).await;
    tokio::time::sleep(SETTLE).await;
    assert_eq!(cached(&pool), [parent]);
    assert_eq!(loads(&server) - counted, 1);
}

#[tokio::test]
async fn with_the_flag_off_a_miss_scans_nothing() {
    let server = start("off");
    let oids = new_oids(&server, 11).await;
    let (parent, children) = (oids[0], &oids[1..]);
    let refs: Vec<_> = children.iter().map(|&oid| Ref::Oid(oid)).collect();
    let data = mapping(&refs, 0);
    let mut stores = vec![(parent, data.clone())];
    stores.extend(children.iter().map(|&oid| (oid, record(b"child"))));
    let before = commit_new(&server, &stores).await;

    // A pool that never sets the flag, and a pool that turns it off.
    let default = open_cached(&server, 2, None).await;
    let off = open(&server, true).await;
    off.set_prefetch_references(false);
    for pool in [&default, &off] {
        let counted = loads(&server);
        load(pool, parent, before, &data).await;
        tokio::time::sleep(SETTLE).await;
        assert_eq!(cached(pool), [parent]);
        assert_eq!(loads(&server) - counted, 1);
    }
}

#[tokio::test]
async fn one_record_prefetches_at_most_its_cap_of_oids_in_pickle_order() {
    let server = start("count");
    let n = PREFETCH_REFERENCES + 8;
    let oids = new_oids(&server, n + 1).await;
    let (parent, children) = (oids[0], &oids[1..]);
    // The first child comes twice: it counts once.
    let mut refs = vec![Ref::Oid(children[0])];
    refs.extend(children.iter().map(|&oid| Ref::Oid(oid)));
    let data = mapping(&refs, 0);
    let mut stores = vec![(parent, data.clone())];
    stores.extend(children.iter().map(|&oid| (oid, record(b"child"))));
    let before = commit_new(&server, &stores).await;

    let pool = open(&server, true).await;
    let counted = loads(&server);
    load(&pool, parent, before, &data).await;
    let (first, rest) = children.split_at(PREFETCH_REFERENCES);
    wait_until(
        || first.iter().all(|&oid| holds(&pool, oid, before)),
        "prefetch of the first children",
    )
    .await;
    tokio::time::sleep(SETTLE).await;
    for &oid in rest {
        assert!(!holds(&pool, oid, before), "the prefetch passed its cap");
    }
    assert_eq!(loads(&server) - counted, 1 + PREFETCH_REFERENCES as u64);
}

#[tokio::test]
async fn a_record_above_the_scan_limit_is_not_scanned() {
    let server = start("size");
    let oids = new_oids(&server, 4).await;
    let (small, large, a, b) = (oids[0], oids[1], oids[2], oids[3]);
    let at_limit = mapping(&[Ref::Oid(a)], PREFETCH_SCAN_LIMIT - 1024);
    let above = mapping(&[Ref::Oid(b)], PREFETCH_SCAN_LIMIT + 1);
    assert!(at_limit.len() <= PREFETCH_SCAN_LIMIT, "{}", at_limit.len());
    assert!(above.len() > PREFETCH_SCAN_LIMIT, "{}", above.len());
    let stores = [
        (small, at_limit.clone()),
        (large, above.clone()),
        (a, record(b"a")),
        (b, record(b"b")),
    ];
    let before = commit_new(&server, &stores).await;

    let pool = open(&server, true).await;
    let counted = loads(&server);
    load(&pool, small, before, &at_limit).await;
    wait_until(|| holds(&pool, a, before), "prefetch of the child").await;
    load(&pool, large, before, &above).await;
    tokio::time::sleep(SETTLE).await;
    assert!(
        !holds(&pool, b, before),
        "a record above the limit was scanned"
    );
    assert_eq!(loads(&server) - counted, 3);
}
