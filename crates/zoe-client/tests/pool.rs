//! Tests of `Pool`: several transactions at once on a pool of connections,
//! with one ordered stream of deliveries (ADR 0036).
//!
//! The tests run `zoe serve` in the test process with
//! `zoe_server::serve::Server`. A second client commits as another client
//! of the storage. A TCP proxy between the pool and the server holds the
//! reply of a finish, so a finish waits for its reply as long as a test
//! needs.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use zoe_client::{
    Cache, Client, Connect, Delivery, Error, Invalidation, Pool, PoolTransaction, Tokio, Turn,
};
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

/// Open a pool as `open` does, with a client cache of 1 MB.
async fn open_cached(server: &Server, bound: usize) -> Pool {
    let addr = server.addr();
    let listener = Client::connect(addr, "1").await.unwrap();
    let connect: Connect = Arc::new(move || Box::pin(Client::connect(addr, "1")));
    let cache = Cache::new(1 << 20).unwrap();
    Pool::with_cache(Arc::new(Tokio), listener, connect, bound, Some(cache))
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

#[tokio::test]
async fn the_cache_applies_a_tid_of_another_client_before_its_delivery() {
    let server = start("cache-foreign");
    let pool = open_cached(&server, 2).await;
    let cache = pool.cache().unwrap().clone();
    let other = Client::connect(server.addr(), "1").await.unwrap();
    let oid = other.new_oids(1).await.unwrap()[0];
    let first = commit_other(&other, oid, Tid(0), &record(b"a")).await;
    assert!(matches!(next(&pool).await, Delivery::Invalidate(_)));
    assert_eq!(cache.applied(), first);
    pool.delivered();

    // A load stores its reply. The next lookup hits.
    let after = Tid(first.0 + 1);
    let (loaded, _) = pool.load_before(oid, after).await.unwrap();
    assert_eq!(loaded.as_ref().unwrap().end, None);
    assert_eq!(cache.load_before(oid, after), loaded);

    // The consumer gets the next tid after the cache ended the entry.
    let second = commit_other(&other, oid, first, &record(b"b")).await;
    assert!(matches!(next(&pool).await, Delivery::Invalidate(_)));
    assert_eq!(cache.applied(), second);
    assert_eq!(cache.load(oid), None);
    let ended = cache.load_before(oid, second).unwrap();
    assert_eq!((ended.tid, ended.end), (first, Some(second)));
    pool.delivered();
}

#[tokio::test]
async fn a_commit_of_the_pool_enters_the_cache_at_its_turn() {
    let server = start("cache-own");
    let pool = open_cached(&server, 2).await;
    let cache = pool.cache().unwrap().clone();
    let oid = pool.new_oids(1).await.unwrap()[0];
    let turn = commit(&pool, oid, Tid(0), &record(b"a")).await.unwrap();
    // The callback of the finish runs here: the cache already has the data.
    assert_eq!(cache.applied(), turn.tid());
    assert_eq!(cache.load(oid), Some((record(b"a"), turn.tid())));
    assert!(pool.last_transaction() < turn.tid());
    drop(turn);
}

#[tokio::test]
async fn a_conflict_of_a_vote_drops_the_current_entry() {
    let server = start("cache-conflict");
    let pool = open_cached(&server, 2).await;
    let cache = pool.cache().unwrap().clone();
    let oid = pool.new_oids(1).await.unwrap()[0];
    let tid = commit(&pool, oid, Tid(0), &record(b"a"))
        .await
        .unwrap()
        .tid();
    assert_eq!(cache.load(oid), Some((record(b"a"), tid)));
    let stale = commit(&pool, oid, Tid(0), &record(b"b")).await;
    assert!(matches!(stale, Err(Error::Conflict { .. })), "{stale:?}");
    assert_eq!(cache.load(oid), None);
}

#[tokio::test]
async fn a_lost_listener_empties_the_cache() {
    let server = start("cache-lost");
    let pool = open_cached(&server, 2).await;
    let cache = pool.cache().unwrap().clone();
    let oid = pool.new_oids(1).await.unwrap()[0];
    let tid = commit(&pool, oid, Tid(0), &record(b"a"))
        .await
        .unwrap()
        .tid();
    assert!(cache.load(oid).is_some());
    pool.listener().disconnect();
    assert_eq!(next(&pool).await, Delivery::Stale);
    assert_eq!(cache.load(oid), None);
    assert!(!cache.store(oid, tid, None, &record(b"a"), tid));
    pool.delivered();
}

/// A TCP proxy to the server. While `pause` holds `true`, the proxy keeps
/// the bytes from the server and does not send them on.
struct Proxy {
    addr: SocketAddr,
    pause: watch::Sender<bool>,
}

/// Start a proxy to `server`.
async fn proxy(server: SocketAddr) -> Proxy {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (pause, paused) = watch::channel(false);
    tokio::spawn(async move {
        while let Ok((client, _)) = listener.accept().await {
            let upstream = TcpStream::connect(server).await.unwrap();
            let (mut client_read, mut client_write) = client.into_split();
            let (mut server_read, mut server_write) = upstream.into_split();
            tokio::spawn(async move {
                let _ = tokio::io::copy(&mut client_read, &mut server_write).await;
                let _ = server_write.shutdown().await;
            });
            let mut paused = paused.clone();
            tokio::spawn(async move {
                let mut buffer = vec![0; 64 * 1024];
                loop {
                    let n = match server_read.read(&mut buffer).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => n,
                    };
                    if paused.wait_for(|paused| !*paused).await.is_err() {
                        return;
                    }
                    if client_write.write_all(&buffer[..n]).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    Proxy { addr, pause }
}

/// Open a pool of one connection that commits through `proxy`. The
/// listener connects to the server directly.
async fn open_proxied(server: &Server, proxy: &Proxy) -> Pool {
    let listener = Client::connect(server.addr(), "1").await.unwrap();
    let addr = proxy.addr;
    let connect: Connect = Arc::new(move || Box::pin(Client::connect(addr, "1")));
    Pool::new(Arc::new(Tokio), listener, connect, 1)
}

/// Begin a transaction on `pool` that stores `oid`, and vote.
async fn voted(pool: &Pool, oid: Oid) -> PoolTransaction {
    let mut txn = pool.begin_with(b"", b"", b"", None, " ").await.unwrap();
    txn.store(oid, Tid(0), &record(b"a")).await.unwrap();
    txn.vote().await.unwrap();
    txn
}

/// Wait until `check` is true.
async fn wait_until(mut check: impl FnMut() -> bool, what: &str) {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while !check() {
        assert!(tokio::time::Instant::now() < deadline, "no {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn the_pool_frees_its_state_after_a_commit_and_its_drop() {
    let server = start("free");
    let addr = server.addr();
    let listener = Client::connect(addr, "1").await.unwrap();
    // The pool holds `connect`, so the marker lives as long as the pool.
    let marker = Arc::new(());
    let freed = Arc::downgrade(&marker);
    let connect: Connect = Arc::new(move || {
        let _held = &marker;
        Box::pin(Client::connect(addr, "1"))
    });
    let pool = Pool::new(Arc::new(Tokio), listener, connect, 2);
    let oid = pool.new_oids(1).await.unwrap()[0];
    drop(commit(&pool, oid, Tid(0), &record(b"a")).await.unwrap());
    drop(pool);
    wait_until(|| freed.strong_count() == 0, "free of the pool").await;
}

#[tokio::test]
async fn a_tid_waits_while_a_finish_sent_before_it_waits_for_its_reply() {
    let server = start("hold");
    let proxy = proxy(server.addr()).await;
    let pool = open_proxied(&server, &proxy).await;
    let other = Client::connect(server.addr(), "1").await.unwrap();
    let oids = pool.new_oids(2).await.unwrap();
    let txn = voted(&pool, oids[0]).await;
    proxy.pause.send_replace(true);
    let finish = tokio::spawn(txn.finish());
    // The server commits and the listener gets the tid. The reply of the
    // finish waits in the proxy, so the pool cannot tell whose tid it is.
    wait_until(|| pool.counters().held == 1, "hold of the tid").await;
    let foreign = commit_other(&other, oids[1], Tid(0), &record(b"b")).await;
    assert!(
        tokio::time::timeout(BLOCKED, pool.next_delivery())
            .await
            .is_err(),
        "a tid reached the consumer during the hold"
    );
    proxy.pause.send_replace(false);
    let finished = tokio::time::timeout(TIMEOUT, finish)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let own = finished.tid();
    assert!(own < foreign);
    drop(finished.turn().await.unwrap());
    assert_eq!(pool.last_transaction(), own);
    // The commit of the other client comes after the commit of the pool.
    assert_eq!(
        next(&pool).await,
        Delivery::Invalidate(Invalidation {
            tid: foreign,
            oids: vec![oids[1]],
        })
    );
    pool.delivered();
    assert_eq!(pool.last_transaction(), foreign);
}

#[tokio::test]
async fn a_finish_dropped_after_its_frame_went_out_leaves_the_hold() {
    let server = start("dropped-finish");
    let proxy = proxy(server.addr()).await;
    let pool = open_proxied(&server, &proxy).await;
    let oid = pool.new_oids(1).await.unwrap()[0];
    let txn = voted(&pool, oid).await;
    proxy.pause.send_replace(true);
    let finish = tokio::spawn(txn.finish());
    wait_until(|| pool.counters().held == 1, "hold of the tid").await;
    // The drop of the future ends the hold. The tid reaches the consumer
    // as a tid of another client.
    finish.abort();
    assert!(finish.await.unwrap_err().is_cancelled());
    let Delivery::Invalidate(invalidation) = next(&pool).await else {
        panic!("the tid of the dropped finish is not an invalidation");
    };
    assert_eq!(invalidation.oids, vec![oid]);
    pool.delivered();
    assert_eq!(pool.last_transaction(), invalidation.tid);
    proxy.pause.send_replace(false);
}

#[tokio::test]
async fn a_finish_in_flight_when_the_listener_is_lost_fails() {
    let server = start("doomed");
    let proxy = proxy(server.addr()).await;
    let pool = open_proxied(&server, &proxy).await;
    let oid = pool.new_oids(1).await.unwrap()[0];
    let txn = voted(&pool, oid).await;
    proxy.pause.send_replace(true);
    let finish = tokio::spawn(txn.finish());
    wait_until(|| pool.counters().held == 1, "hold of the tid").await;
    pool.listener().disconnect();
    assert_eq!(next(&pool).await, Delivery::Stale);
    pool.delivered();
    // The reply comes after the loss: the outcome of the finish counts as
    // unknown.
    proxy.pause.send_replace(false);
    let finished = tokio::time::timeout(TIMEOUT, finish)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(finished, Err(Error::Disconnected)));
}
