//! Tests of `Pool`: several transactions at once on a pool of connections,
//! with one ordered stream of deliveries (ADR 0036).
//!
//! The tests run `zoe serve` in the test process with
//! `zoe_server::serve::Server`. A second client commits as another client
//! of the storage.

use std::sync::Arc;
use std::time::Duration;

use zoe_client::{Client, Connect, Delivery, Error, Pool, Tokio, Turn};
use zoe_core::{Oid, Tid};
use zoe_server::serve::{ServeOptions, Server};

const TIMEOUT: Duration = Duration::from_secs(30);

/// The time that a test waits to see that a call stays blocked.
const BLOCKED: Duration = Duration::from_millis(200);

fn fresh_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("zoe-client-pool-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn start(name: &str) -> Server {
    Server::start(ServeOptions::new(fresh_dir(name).join("store"))).unwrap()
}

/// A ZODB record of a `PersistentMapping` that holds `value`.
fn record(value: &[u8]) -> Vec<u8> {
    let mut pickle = b"\x80\x03cpersistent.mapping\nPersistentMapping\nq\x00.".to_vec();
    pickle.extend_from_slice(b"\x80\x03}q\x00X\x01\x00\x00\x00vq\x01C");
    pickle.push(value.len() as u8);
    pickle.extend_from_slice(value);
    pickle.extend_from_slice(b"q\x02s.");
    pickle
}

/// Open a pool of at most `bound` connections that commit on `server`.
async fn open(server: &Server, bound: usize) -> Pool {
    let addr = server.addr();
    let listener = Client::connect(addr, "1").await.unwrap();
    let connect: Connect = Arc::new(move || Box::pin(Client::connect(addr, "1")));
    Pool::new(Arc::new(Tokio), listener, connect, bound)
}

/// Commit `data` for `oid` through `client`, as another client.
async fn commit_other(client: &Client, oid: Oid, serial: Tid, data: &[u8]) -> Tid {
    let mut txn = client.begin(b"", b"", b"").await.unwrap();
    txn.store(oid, serial, data).await.unwrap();
    txn.vote().await.unwrap();
    txn.finish().await.unwrap()
}

/// Commit `data` for `oid` through `pool`, and return the turn.
async fn commit(pool: &Pool, oid: Oid, serial: Tid, data: &[u8]) -> Result<Turn, Error> {
    let mut txn = pool.begin_with(b"", b"", b"", None, " ").await?;
    txn.store(oid, serial, data).await?;
    txn.vote().await?;
    txn.finish().await?.turn().await
}

async fn next(pool: &Pool) -> Delivery {
    tokio::time::timeout(TIMEOUT, pool.next_delivery())
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn a_commit_of_the_pool_takes_its_turn_and_never_reaches_the_consumer() {
    let server = start("turn");
    let pool = open(&server, 2).await;
    let other = Client::connect(server.addr(), "1").await.unwrap();
    let oids = other.new_oids(3).await.unwrap();

    // A commit of another client goes to the consumer. The last transaction
    // moves when the consumer confirms it.
    let first = commit_other(&other, oids[0], Tid(0), &record(b"a")).await;
    let Delivery::Invalidate(invalidation) = next(&pool).await else {
        panic!("the commit of the other client is not an invalidation");
    };
    assert_eq!(
        (invalidation.tid, invalidation.oids),
        (first, vec![oids[0]])
    );
    assert!(pool.last_transaction() < first);
    pool.delivered();
    assert_eq!(pool.last_transaction(), first);

    // A commit of the pool comes at its turn. The last transaction moves
    // when the turn ends.
    let turn = commit(&pool, oids[1], Tid(0), &record(b"b")).await.unwrap();
    let own = turn.tid();
    assert!(own > first);
    assert!(!turn.stale());
    assert_eq!(pool.last_transaction(), first);
    drop(turn);
    assert_eq!(pool.last_transaction(), own);

    // The next delivery is the next commit of the other client, not the
    // commit of the pool.
    let third = commit_other(&other, oids[2], Tid(0), &record(b"c")).await;
    assert_eq!(
        next(&pool).await,
        Delivery::Invalidate(zoe_client::Invalidation {
            tid: third,
            oids: vec![oids[2]],
        })
    );
    pool.delivered();
    assert_eq!(pool.last_transaction(), third);
    assert_eq!(pool.load(oids[1]).await.unwrap(), (record(b"b"), own));
}

#[tokio::test]
async fn a_finish_waits_for_the_tids_below_its_own() {
    let server = start("order");
    let pool = open(&server, 2).await;
    let other = Client::connect(server.addr(), "1").await.unwrap();
    let oids = other.new_oids(2).await.unwrap();

    // The consumer does not confirm the commit of the other client yet.
    let foreign = commit_other(&other, oids[0], Tid(0), &record(b"a")).await;
    assert!(matches!(next(&pool).await, Delivery::Invalidate(_)));
    let mut txn = pool.begin_with(b"", b"", b"", None, " ").await.unwrap();
    txn.store(oids[1], Tid(0), &record(b"b")).await.unwrap();
    txn.vote().await.unwrap();
    let finished = txn.finish().await.unwrap();
    assert!(finished.tid() > foreign);
    let turn = tokio::spawn(finished.turn());
    tokio::time::sleep(BLOCKED).await;
    assert!(!turn.is_finished(), "the turn came before a lower tid");
    pool.delivered();
    let turn = tokio::time::timeout(TIMEOUT, turn).await.unwrap().unwrap();
    assert_eq!(pool.last_transaction(), foreign);
    drop(turn);
}

#[tokio::test]
async fn the_settled_last_transaction_waits_for_the_turn_that_runs() {
    let server = start("settled");
    let pool = Arc::new(open(&server, 1).await);
    let oid = pool.new_oids(1).await.unwrap()[0];
    let turn = commit(&pool, oid, Tid(0), &record(b"a")).await.unwrap();
    let tid = turn.tid();
    let settled = tokio::spawn({
        let pool = pool.clone();
        async move { pool.settled_transaction().await }
    });
    tokio::time::sleep(BLOCKED).await;
    assert!(
        !settled.is_finished(),
        "the settled tid came during the turn"
    );
    assert!(pool.last_transaction() < tid);
    drop(turn);
    let settled = tokio::time::timeout(TIMEOUT, settled)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(settled, tid);
}

#[tokio::test]
async fn a_transaction_waits_for_a_free_connection_at_the_bound() {
    let server = start("bound");
    let pool = Arc::new(open(&server, 1).await);
    assert_eq!(pool.bound(), 1);
    let first = pool.begin_with(b"", b"", b"", None, " ").await.unwrap();
    let second = tokio::spawn({
        let pool = pool.clone();
        async move { pool.begin_with(b"", b"", b"", None, " ").await }
    });
    tokio::time::sleep(BLOCKED).await;
    assert!(
        !second.is_finished(),
        "a second transaction got a connection"
    );
    // The abort of the first gives its connection back.
    first.abort().await.unwrap();
    let second = tokio::time::timeout(TIMEOUT, second)
        .await
        .unwrap()
        .unwrap();
    second.unwrap().abort().await.unwrap();
}

#[tokio::test]
async fn a_finish_that_drops_before_its_turn_leaves_its_tid_to_the_consumer() {
    let server = start("dropped");
    let pool = open(&server, 1).await;
    let oid = pool.new_oids(1).await.unwrap()[0];
    let mut txn = pool.begin_with(b"", b"", b"", None, " ").await.unwrap();
    txn.store(oid, Tid(0), &record(b"a")).await.unwrap();
    txn.vote().await.unwrap();
    let finished = txn.finish().await.unwrap();
    let tid = finished.tid();
    drop(finished);
    assert_eq!(
        next(&pool).await,
        Delivery::Invalidate(zoe_client::Invalidation {
            tid,
            oids: vec![oid],
        })
    );
    pool.delivered();
    assert_eq!(pool.last_transaction(), tid);
}

#[tokio::test]
async fn a_lost_listener_gives_the_stale_mark_and_fails_new_transactions() {
    let server = start("lost");
    let pool = open(&server, 2).await;
    assert!(pool.is_connected());
    pool.listener().disconnect();
    assert_eq!(next(&pool).await, Delivery::Stale);
    pool.delivered();
    assert!(!pool.is_connected());
    assert!(matches!(
        pool.begin_with(b"", b"", b"", None, " ").await,
        Err(Error::Disconnected)
    ));
    // The close ends the deliveries.
    pool.disconnect();
    assert_eq!(pool.next_delivery().await, None);
}

#[tokio::test]
async fn a_load_gives_the_position_of_its_connection() {
    let server = start("position");
    let client = Client::connect(server.addr(), "1").await.unwrap();
    let other = Client::connect(server.addr(), "1").await.unwrap();
    let mut invalidations = client.invalidations();
    let oid = other.new_oids(1).await.unwrap()[0];
    let tid = commit_other(&other, oid, Tid(0), &record(b"a")).await;
    let invalidation = tokio::time::timeout(TIMEOUT, invalidations.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(invalidation.tid, tid);
    let (loaded, position) = client.load_before_at(oid, Tid(u64::MAX)).await.unwrap();
    assert_eq!(loaded.unwrap().tid, tid);
    assert_eq!(position, tid);
    // The reply of a finish of the connection moves its position too.
    let own = commit_other(&client, oid, tid, &record(b"b")).await;
    let (_, position) = client.load_before_at(oid, Tid(u64::MAX)).await.unwrap();
    assert_eq!(position, own);
}
