//! Tests of the join of a load to a load in flight of the same oid at the
//! same tid, in a `Pool`.
//!
//! `Pool::load_before` of an oid and a tid that a load in flight already
//! asks for waits for the reply of that load, and sends nothing to the
//! server. Each waiter gets the record, or the error of the load. A load
//! at another tid sends its own request. The load goes on when its caller
//! drops its future, so the other waiters still get the reply. Once the
//! reply came, the next load sends a new request. Only a load at a tid of
//! at most the last tid of the listener plus 1 joins: a current load, at
//! `MAX_TID`, always sends its own request. The prefetch of the
//! references goes through the same loads, so a load of a child joins
//! its prefetch, and the reverse.
//!
//! The tests run `zoe serve` in the test process with
//! `zoe_server::serve::Server` and an admin port, and read the count of
//! the loads of the server, `zoe_loads_total`, from `GET /metrics`. The
//! server counts a load before its reply goes out. Each test runs on one
//! thread, the default of `tokio::test`: a task of the pool runs only when
//! the test waits, so the order of the loads is fixed.

mod common;

use std::future::{Future, poll_fn};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

use common::{Started, commit, open_cached, record, start_with};
use zoe_client::runtime::BoxFuture;
use zoe_client::{Client, Connect, Error, Pool, Record, Tokio};
use zoe_core::{Oid, Tid};
use zoe_pickle::python::{Heap, Pickler};
use zoe_server::serve::ServeOptions;

const TIMEOUT: Duration = Duration::from_secs(30);

/// The time that a test gives a load that must not happen. A load on the
/// loopback ends well within it.
const SETTLE: Duration = Duration::from_millis(300);

const MODULE: &str = "persistent.mapping";
const CLASS: &str = "PersistentMapping";

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

/// Return `n` new oids of `server`.
async fn new_oids(server: &Started, n: usize) -> Vec<Oid> {
    let other = Client::connect(server.addr(), "1").await.unwrap();
    other.new_oids(n).await.unwrap()
}

/// Commit `stores` (oid, serial and data) through a client of `server`, in
/// one transaction, and return its tid.
async fn commit_stores(server: &Started, stores: &[(Oid, Tid, Vec<u8>)]) -> Tid {
    let other = Client::connect(server.addr(), "1").await.unwrap();
    commit(&other, stores).await.unwrap()
}

/// Open a pool of at most 2 connections that commit on `server`, without a
/// cache.
async fn open_uncached(server: &Started) -> Pool {
    let addr = server.addr();
    let connect: Connect = Arc::new(move || -> BoxFuture<Result<Client, Error>> {
        Box::pin(Client::connect(addr, "1"))
    });
    let listener = Client::connect(addr, "1").await.unwrap();
    Pool::new(Arc::new(Tokio), listener, connect, 2)
}

/// Return the record of a load, or fail the test.
fn record_of(loaded: Result<(Option<Record>, Tid), Error>) -> Record {
    loaded.unwrap().0.expect("the record of the load")
}

/// Tell if `error` is the `POSKeyError` of the server.
fn is_pos_key_error(error: &Error) -> bool {
    matches!(error, Error::Server { class, .. } if class.ends_with("POSKeyError"))
}

#[tokio::test]
async fn two_loads_of_one_oid_at_one_tid_send_one_load() {
    let server = start("same");
    let oid = new_oids(&server, 1).await[0];
    let data = record(b"one");
    let tid = commit_stores(&server, &[(oid, Tid(0), data.clone())]).await;
    let before = Tid(tid.0 + 1);

    for pool in [
        open_uncached(&server).await,
        open_cached(&server, 2, None).await,
    ] {
        let stored = pool.cache().map(|cache| cache.counts().stored);
        let counted = loads(&server);
        let (first, second) = tokio::time::timeout(TIMEOUT, async {
            tokio::join!(pool.load_before(oid, before), pool.load_before(oid, before))
        })
        .await
        .unwrap();
        let expected = Record {
            data: data.clone(),
            tid,
            end: None,
        };
        assert_eq!(record_of(first), expected);
        assert_eq!(record_of(second), expected);
        assert_eq!(loads(&server) - counted, 1);
        // The reply goes into the cache once.
        if let (Some(cache), Some(stored)) = (pool.cache(), stored) {
            assert_eq!(cache.counts().stored - stored, 1);
            assert!(cache.load_before(oid, before).is_some());
        }
    }
}

#[tokio::test]
async fn a_load_at_another_tid_sends_its_own_load() {
    let server = start("other");
    let oid = new_oids(&server, 1).await[0];
    let (old, new) = (record(b"old"), record(b"new"));
    let first = commit_stores(&server, &[(oid, Tid(0), old.clone())]).await;
    let second = commit_stores(&server, &[(oid, first, new.clone())]).await;

    let pool = open_uncached(&server).await;
    let counted = loads(&server);
    let (at_second, after) = tokio::time::timeout(TIMEOUT, async {
        tokio::join!(
            pool.load_before(oid, second),
            pool.load_before(oid, Tid(second.0 + 1))
        )
    })
    .await
    .unwrap();
    // Each load gets the record of its own tid.
    let at_second = record_of(at_second);
    let after = record_of(after);
    assert_eq!(
        (at_second.data, at_second.tid, at_second.end),
        (old, first, Some(second))
    );
    assert_eq!((after.data, after.tid, after.end), (new, second, None));
    assert_eq!(loads(&server) - counted, 2);
}

#[tokio::test]
async fn a_failed_load_gives_its_error_to_every_waiter() {
    let server = start("failed");
    let oids = new_oids(&server, 2).await;
    // An oid that the server gave and that no commit wrote: its load fails.
    let (missing, other) = (oids[0], oids[1]);
    let tid = commit_stores(&server, &[(other, Tid(0), record(b"other"))]).await;
    let before = Tid(tid.0 + 1);

    let pool = open_uncached(&server).await;
    let counted = loads(&server);
    let (first, second, third) = tokio::time::timeout(TIMEOUT, async {
        tokio::join!(
            pool.load_before(missing, before),
            pool.load_before(missing, before),
            pool.load_before(missing, before)
        )
    })
    .await
    .unwrap();
    for loaded in [first, second, third] {
        let error = loaded.unwrap_err();
        assert!(is_pos_key_error(&error), "{error:?}");
    }
    assert_eq!(loads(&server) - counted, 1);

    // The next load of the key sends a new request.
    let next = tokio::time::timeout(TIMEOUT, pool.load_before(missing, before))
        .await
        .unwrap();
    let error = next.unwrap_err();
    assert!(is_pos_key_error(&error), "{error:?}");
    assert_eq!(loads(&server) - counted, 2);
}

#[tokio::test]
async fn a_dropped_caller_does_not_stop_the_load_for_the_others() {
    let server = start("dropped");
    let oids = new_oids(&server, 2).await;
    let (shared, alone) = (oids[0], oids[1]);
    let data = record(b"shared");
    let stores = [
        (shared, Tid(0), data.clone()),
        (alone, Tid(0), record(b"alone")),
    ];
    let tid = commit_stores(&server, &stores).await;
    let before = Tid(tid.0 + 1);

    let pool = open_cached(&server, 2, None).await;
    let counted = loads(&server);
    // The first load sends the request, the second waits for its reply.
    let mut first = Box::pin(pool.load_before(shared, before));
    let mut second = Box::pin(pool.load_before(shared, before));
    poll_fn(|cx| {
        assert!(first.as_mut().poll(cx).is_pending());
        assert!(second.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    drop(first);
    let loaded = tokio::time::timeout(TIMEOUT, second).await.unwrap();
    assert_eq!(record_of(loaded).data, data);
    assert_eq!(loads(&server) - counted, 1);

    // A load that no caller waits for still stores its reply.
    let mut lone = Box::pin(pool.load_before(alone, before));
    poll_fn(|cx| {
        assert!(lone.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    drop(lone);
    let cache = pool.cache().unwrap();
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while cache.load_before(alone, before).is_none() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "no reply in the cache"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(loads(&server) - counted, 2);
}

#[tokio::test]
async fn a_load_after_the_reply_sends_a_new_request() {
    let server = start("after");
    let oid = new_oids(&server, 1).await[0];
    let data = record(b"again");
    let tid = commit_stores(&server, &[(oid, Tid(0), data.clone())]).await;
    let before = Tid(tid.0 + 1);

    let pool = open_uncached(&server).await;
    let counted = loads(&server);
    for round in 1..=2 {
        let loaded = tokio::time::timeout(TIMEOUT, pool.load_before(oid, before))
            .await
            .unwrap();
        assert_eq!(record_of(loaded).data, data);
        assert_eq!(loads(&server) - counted, round);
    }

    // A cache that dropped the record: the caller loads again.
    let pool = open_cached(&server, 2, None).await;
    let counted = loads(&server);
    let loaded = tokio::time::timeout(TIMEOUT, pool.load_before(oid, before))
        .await
        .unwrap();
    assert_eq!(record_of(loaded).data, data);
    let cache = pool.cache().unwrap();
    cache.clear();
    assert!(cache.load_before(oid, before).is_none());
    let loaded = tokio::time::timeout(TIMEOUT, pool.load_before(oid, before))
        .await
        .unwrap();
    assert_eq!(record_of(loaded).data, data);
    assert_eq!(loads(&server) - counted, 2);
}

#[tokio::test]
async fn a_current_load_and_a_load_above_the_listener_do_not_join() {
    let server = start("current");
    let oid = new_oids(&server, 1).await[0];
    let data = record(b"current");
    let tid = commit_stores(&server, &[(oid, Tid(0), data.clone())]).await;

    // The last tid of the listener is `tid`. A load at `MAX_TID` or above
    // `tid + 1` gives the record of the moment of its read, so each one
    // sends its own request.
    let pool = open_uncached(&server).await;
    let counted = loads(&server);
    let (first, second) = tokio::time::timeout(TIMEOUT, async {
        tokio::join!(pool.load(oid), pool.load(oid))
    })
    .await
    .unwrap();
    assert_eq!(first.unwrap(), (data.clone(), tid));
    assert_eq!(second.unwrap(), (data.clone(), tid));
    assert_eq!(loads(&server) - counted, 2);

    let above = Tid(tid.0 + 2);
    let (first, second) = tokio::time::timeout(TIMEOUT, async {
        tokio::join!(pool.load_before(oid, above), pool.load_before(oid, above))
    })
    .await
    .unwrap();
    assert_eq!(record_of(first).data, data);
    assert_eq!(record_of(second).data, data);
    assert_eq!(loads(&server) - counted, 4);
}

#[tokio::test]
async fn a_load_of_a_child_and_its_prefetch_send_one_load() {
    let server = start("prefetch");
    let oids = new_oids(&server, 11).await;
    let (parent, children) = (oids[0], &oids[1..]);
    let data = parent_record(children);
    let mut stores = vec![(parent, Tid(0), data.clone())];
    stores.extend(children.iter().map(|&oid| (oid, Tid(0), record(b"child"))));
    let tid = commit_stores(&server, &stores).await;
    let before = Tid(tid.0 + 1);

    let pool = open_cached(&server, 2, None).await;
    pool.set_prefetch_references(true);
    let cache = pool.cache().unwrap().clone();
    let counted = loads(&server);
    let loaded = tokio::time::timeout(TIMEOUT, pool.load_before(parent, before))
        .await
        .unwrap();
    assert_eq!(record_of(loaded).data, data);
    // The task of the prefetch has not run yet: the load of the first
    // child sends the request, and the prefetch joins it. The loads of the
    // next children join their prefetch, or hit the cache, as a caller
    // that looks up the cache first.
    for &child in children {
        if cache.load_before(child, before).is_some() {
            continue;
        }
        let loaded = tokio::time::timeout(TIMEOUT, pool.load_before(child, before))
            .await
            .unwrap();
        assert_eq!(record_of(loaded).data, record(b"child"));
    }
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while !children
        .iter()
        .all(|&child| cache.load_before(child, before).is_some())
    {
        assert!(tokio::time::Instant::now() < deadline, "no prefetch");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    tokio::time::sleep(SETTLE).await;
    assert_eq!(loads(&server) - counted, 1 + children.len() as u64);
}

/// Return the record of a `PersistentMapping` whose `data` maps one key to
/// a persistent reference `(oid, class)` of each oid of `children`, as
/// ZODB writes it, built with the pickler of `zoe_pickle::python`.
fn parent_record(children: &[Oid]) -> Vec<u8> {
    let mut heap = Heap::new();
    let class = heap.new_class(MODULE, CLASS);
    let mut items = Vec::new();
    for (n, child) in children.iter().enumerate() {
        let key = heap.new_str(&format!("k{n:04}"));
        let oid = heap.new_bytes(&child.0.to_be_bytes());
        let child_class = heap.new_class(MODULE, CLASS);
        let id = heap.new_tuple(vec![oid, child_class]);
        items.push((key, heap.new_reference(id).unwrap()));
    }
    let data = heap.new_dict(items);
    let name = heap.new_str("data");
    let state = heap.new_dict(vec![(name, data)]);
    let mut pickler = Pickler::new(&heap);
    pickler.dump(class).unwrap();
    pickler.dump(state).unwrap();
    pickler.into_bytes()
}
