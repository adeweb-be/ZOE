//! Tests of the cache of a `Pool` after a split merge of the server that
//! keeps the bucket of the other side (ADR 0049).
//!
//! The tests run `zoe serve` in the test process with
//! `zoe_server::serve::Server`. The pool declares `server_writes`, as the
//! native mode of `zoe.zeo.ClientStorage` does. A second client commits the
//! split S1 as another client of the storage. Then the pool commits the
//! split S2, which read the tree before S1. The vote of the server merges
//! the keys into the bucket A and the new bucket Y of S1, writes A and Y,
//! and resolves the node and the new bucket X of S2 without a record. The
//! reply lists the node, A, X and Y as resolved.
//!
//! The records are those of ZODB, built with the pickler of
//! `zoe_pickle::python`: a node is `((child, key, child, ...), first)`, a
//! bucket `(items,)` or `(items, next)`, and a reference `(oid, class)`.

use std::sync::Arc;
use std::time::Duration;

use zoe_client::{Cache, Client, Connect, Delivery, Options, Pool, Tokio};
use zoe_core::{Oid, Tid};
use zoe_pickle::btrees::new_bucket_state;
use zoe_pickle::python::{Heap, Pickler, Value};
use zoe_server::serve::{ServeOptions, Server};

const TIMEOUT: Duration = Duration::from_secs(30);

const MODULE: &str = "BTrees.IIBTree";

fn fresh_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("zoe-client-split-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn pickled(heap: &Heap, class: Value, state: Value) -> Vec<u8> {
    let mut pickler = Pickler::new(heap);
    pickler.dump(class).unwrap();
    pickler.dump(state).unwrap();
    pickler.into_bytes()
}

/// Returns a reference to the `IIBucket` `oid`.
fn reference(heap: &mut Heap, oid: Oid) -> Value {
    let bytes = heap.new_bytes(&oid.0.to_be_bytes());
    let class = heap.new_class(MODULE, "IIBucket");
    let data = heap.new_tuple(vec![bytes, class]);
    heap.new_reference(data).unwrap()
}

/// Returns the record of an `IIBucket` of `keys`, each with the value 1,
/// and its next bucket.
fn bucket(keys: &[i128], next: Option<Oid>) -> Vec<u8> {
    let mut heap = Heap::new();
    let class = heap.new_class(MODULE, "IIBucket");
    let items = keys.iter().flat_map(|&k| [Value::Int(k), Value::Int(1)]);
    let next = next.map(|oid| reference(&mut heap, oid));
    let state = new_bucket_state(&mut heap, items.collect(), next);
    pickled(&heap, class, state)
}

/// Returns the record of an `IIBTree` whose children are the buckets
/// `first`, then each separator and its bucket.
fn node(first: Oid, children: &[(i128, Oid)]) -> Vec<u8> {
    let mut heap = Heap::new();
    let class = heap.new_class(MODULE, "IIBTree");
    let mut items = vec![reference(&mut heap, first)];
    for &(separator, oid) in children {
        items.push(Value::Int(separator));
        items.push(reference(&mut heap, oid));
    }
    let children = heap.new_tuple(items);
    let first = reference(&mut heap, first);
    let state = heap.new_tuple(vec![children, first]);
    pickled(&heap, class, state)
}

/// Returns the record of a merge policy that sets `split_merges` to `on`.
fn policy() -> Vec<u8> {
    let mut heap = Heap::new();
    let class = heap.new_class("zoe.policy", "MergePolicy");
    let classes = heap.new_dict(Vec::new());
    let items = vec![
        (heap.new_str("version"), Value::Int(1)),
        (heap.new_str("classes"), classes),
        (heap.new_str("split_merges"), heap.new_str("on")),
    ];
    let state = heap.new_dict(items);
    pickled(&heap, class, state)
}

/// The root of ZODB, a `PersistentMapping` that names the policy at
/// `policy`, as ZODB writes it.
fn root(policy: Oid) -> Vec<u8> {
    [
        &b"\x80\x03cpersistent.mapping\nPersistentMapping\nq\x00.\x80\x03}q\x01X\x04\x00\x00\x00\
           dataq\x02}q\x03X\x10\x00\x00\x00zoe.merge_policyq\x04C\x08"[..],
        &policy.0.to_be_bytes(),
        b"q\x05czoe.policy\nMergePolicy\nq\x06\x86q\x07Qss.",
    ]
    .concat()
}

/// The options of a client that declares `server_writes`.
fn declaring() -> Options {
    Options {
        extensions: vec!["server_writes".to_owned()],
        ..Options::default()
    }
}

/// Open a pool of at most `bound` connections that declare
/// `server_writes`, with a client cache of 1 MB.
async fn open_cached(server: &Server, bound: usize) -> Pool {
    let addr = server.addr();
    let listener = Client::open(Arc::new(Tokio), addr, "1", &declaring())
        .await
        .unwrap();
    let connect: Connect = Arc::new(move || {
        Box::pin(async move { Client::open(Arc::new(Tokio), addr, "1", &declaring()).await })
    });
    let cache = Cache::new(1 << 20).unwrap();
    Pool::with_cache(Arc::new(Tokio), listener, connect, bound, Some(cache))
}

/// Commit `writes` through `client`, as another client.
async fn commit_other(client: &Client, writes: &[(Oid, Tid, Vec<u8>)]) -> Tid {
    let mut txn = client.begin(b"", b"", b"").await.unwrap();
    for (oid, serial, data) in writes {
        txn.store(*oid, *serial, data).await.unwrap();
    }
    txn.vote().await.unwrap();
    txn.finish().await.unwrap()
}

/// Deliver the next tid of another client to the consumer of `pool`.
async fn deliver(pool: &Pool) {
    let delivery = tokio::time::timeout(TIMEOUT, pool.next_delivery())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(delivery, Delivery::Invalidate(_)), "{delivery:?}");
    pool.delivered();
}

#[tokio::test]
async fn a_node_resolved_without_a_record_enters_the_cache_at_its_next_load() {
    let server = Server::start(ServeOptions::new(fresh_dir("node").join("store"))).unwrap();
    let pool = open_cached(&server, 2).await;
    let cache = pool.cache().unwrap().clone();
    let other = Client::connect(server.addr(), "1").await.unwrap();
    let oids = other.new_oids(6).await.unwrap();
    let (policy_oid, n, b0, b1, y) = (oids[1], oids[2], oids[3], oids[4], oids[5]);
    commit_other(&other, &[(policy_oid, Tid::ZERO, policy())]).await;
    deliver(&pool).await;
    commit_other(&other, &[(Oid(0), Tid::ZERO, root(policy_oid))]).await;
    deliver(&pool).await;
    // The node n holds the buckets b0 [10, 20, 30] and b1 [40, 50, 60].
    let read = commit_other(
        &other,
        &[
            (n, Tid::ZERO, node(b0, &[(40, b1)])),
            (b0, Tid::ZERO, bucket(&[10, 20, 30], Some(b1))),
            (b1, Tid::ZERO, bucket(&[40, 50, 60], None)),
        ],
    )
    .await;
    deliver(&pool).await;
    // S1 splits b1 into b1 and y.
    let s1 = commit_other(
        &other,
        &[
            (n, read, node(b0, &[(40, b1), (60, y)])),
            (b1, read, bucket(&[40, 50], Some(y))),
            (y, Tid::ZERO, bucket(&[60, 70], None)),
        ],
    )
    .await;
    deliver(&pool).await;
    // The cache holds the node of S1.
    let (committed, serial) = pool.load(n).await.unwrap();
    assert_eq!(serial, s1);
    assert_eq!(cache.load(n), Some((committed.clone(), s1)));

    // S2 read before S1 and splits b1 into b1 and x. The server keeps the
    // node of S1: the node gets no record.
    let x = pool.new_oids(1).await.unwrap()[0];
    let mut s2 = pool.begin_with(b"", b"", b"", None, " ").await.unwrap();
    for (oid, serial, data) in [
        (n, read, node(b0, &[(40, b1), (50, x)])),
        (b1, read, bucket(&[40, 45], Some(x))),
        (x, Tid::ZERO, bucket(&[50, 60], None)),
    ] {
        s2.store(oid, serial, &data).await.unwrap();
    }
    s2.vote().await.unwrap();
    let turn = s2.finish().await.unwrap().turn().await.unwrap();
    let commit = turn.unstored_commit().unwrap().to_vec();
    assert!(commit.contains(&y), "{commit:?}");
    assert!(!commit.contains(&n), "{commit:?}");
    drop(turn);

    // The current record of the node is still the record of S1. A load
    // of it from the server enters the cache, and the next lookup hits.
    let stored = cache.counts().stored;
    assert_eq!(pool.load(n).await.unwrap(), (committed.clone(), s1));
    assert_eq!(cache.counts().stored, stored + 1);
    assert_eq!(cache.load(n), Some((committed, s1)));
}
