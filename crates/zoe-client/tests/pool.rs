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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use zoe_client::{
    Cache, Client, Connect, Delivery, Error, Event, Invalidation, Listed, Open, Options, Pool,
    PoolTransaction, Reconnect, Tokio, Turn, Verification, Voted,
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
    pool.listener().unwrap().disconnect();
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

/// Commit `oid` twice through `pool`, then begin a transaction that stores
/// `data` over the first serial. Return the transaction and both tids.
async fn stale(pool: &Pool, oid: Oid, data: &[u8]) -> (PoolTransaction, Tid, Tid) {
    let first = commit(pool, oid, Tid(0), &record(b"a"))
        .await
        .unwrap()
        .tid();
    let second = commit(pool, oid, first, &record(b"b")).await.unwrap().tid();
    let mut txn = pool.begin_with(b"", b"", b"", None, " ").await.unwrap();
    txn.store(oid, first, data).await.unwrap();
    (txn, first, second)
}

#[tokio::test]
async fn a_conflict_that_the_client_resolves_enters_the_cache_with_its_resolution() {
    let server = start("cache-resolved");
    let pool = open_cached(&server, 2).await;
    let cache = pool.cache().unwrap().clone();
    let oid = pool.new_oids(1).await.unwrap()[0];
    let (mut txn, first, second) = stale(&pool, oid, &record(b"c")).await;
    assert_eq!(cache.load(oid), Some((record(b"b"), second)));

    // The vote lists the conflict, and the cache stays as it is.
    let voted = txn.vote_listed().await.unwrap();
    let listed = Listed {
        oid,
        committed: second,
        read: first,
        data: record(b"c"),
    };
    assert_eq!(voted.conflicts, vec![listed]);
    assert!(voted.resolved.is_empty());
    assert_eq!(cache.load(oid), Some((record(b"b"), second)));

    // The resolution commits, and its turn replaces the entry of the oid
    // with the resolved pickle.
    txn.store_resolved(oid, second, &record(b"d"))
        .await
        .unwrap();
    assert_eq!(txn.vote_listed().await.unwrap(), Voted::default());
    let turn = txn.finish().await.unwrap().turn().await.unwrap();
    let tid = turn.tid();
    assert_eq!(cache.load(oid), Some((record(b"d"), tid)));
    let ended = cache.load_before(oid, tid).unwrap();
    assert_eq!((ended.tid, ended.end), (second, Some(tid)));
    drop(turn);
    assert_eq!(pool.load(oid).await.unwrap(), (record(b"d"), tid));
}

#[tokio::test]
async fn an_unresolved_conflict_drops_the_current_entry_and_waits_for_the_abort() {
    let server = start("cache-unresolved");
    let pool = open_cached(&server, 2).await;
    let cache = pool.cache().unwrap().clone();
    let oid = pool.new_oids(1).await.unwrap()[0];
    let (mut txn, _, second) = stale(&pool, oid, &record(b"c")).await;
    assert_eq!(txn.vote_listed().await.unwrap().conflicts.len(), 1);
    txn.unresolved(oid);
    assert_eq!(cache.load(oid), None);
    // The abort releases the locks of the listed conflict.
    txn.abort().await.unwrap();
    commit(&pool, oid, second, &record(b"e")).await.unwrap();
}

#[tokio::test]
async fn the_cache_applies_the_tid_of_a_dropped_finish_before_its_delivery() {
    let server = start("cache-dropped");
    let pool = open_cached(&server, 1).await;
    let cache = pool.cache().unwrap().clone();
    let oid = pool.new_oids(1).await.unwrap()[0];
    let first = commit(&pool, oid, Tid(0), &record(b"a"))
        .await
        .unwrap()
        .tid();
    assert_eq!(cache.load(oid), Some((record(b"a"), first)));
    let mut txn = pool.begin_with(b"", b"", b"", None, " ").await.unwrap();
    txn.store(oid, first, &record(b"b")).await.unwrap();
    txn.vote().await.unwrap();
    let finished = txn.finish().await.unwrap();
    let tid = finished.tid();
    drop(finished);
    // The tid reaches the consumer as a tid of another client. The cache
    // applied it first: the entry of the oid ends at the tid, and the data
    // of the dropped finish is not in the cache.
    assert!(matches!(next(&pool).await, Delivery::Invalidate(_)));
    assert_eq!(cache.applied(), tid);
    assert_eq!(cache.load(oid), None);
    let ended = cache.load_before(oid, tid).unwrap();
    assert_eq!((ended.tid, ended.end), (first, Some(tid)));
    pool.delivered();
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
    pool.listener().unwrap().disconnect();
    assert_eq!(next(&pool).await, Delivery::Stale);
    assert_eq!(cache.load(oid), None);
    assert!(!cache.store(oid, tid, None, &record(b"a"), tid));
    pool.delivered();
}

/// The time between two attempts of a pool to connect again, in the tests.
const POLL: Duration = Duration::from_millis(20);

/// Return an `Open` that connects to `server` while `gate` holds `true`,
/// with `options`, and fails with `Error::Disconnected` else. With
/// `writable`, it opens no client.
fn gated(server: SocketAddr, options: Options, gate: watch::Receiver<bool>) -> Open {
    Arc::new(move |writable| {
        let (options, open) = (options.clone(), *gate.borrow());
        Box::pin(async move {
            if !open || writable {
                return Err(Error::Disconnected);
            }
            let client = Client::open(Arc::new(Tokio), server, "1", &options).await?;
            let connect: Connect = Arc::new(move || Box::pin(Client::connect(server, "1")));
            Ok((client, connect))
        })
    })
}

/// Open a pool with a cache of 1 MB on `server`, that connects again to it
/// through `open`.
async fn open_reconnecting(server: &Server, open: Open) -> Pool {
    let addr = server.addr();
    let listener = Client::connect(addr, "1").await.unwrap();
    let connect: Connect = Arc::new(move || Box::pin(Client::connect(addr, "1")));
    let cache = Cache::new(1 << 20).unwrap();
    let reconnect = Reconnect::new(open, POLL);
    Pool::with_reconnect(
        Arc::new(Tokio),
        Some((listener, connect)),
        2,
        Some(cache),
        reconnect,
    )
}

async fn next_event(pool: &Pool) -> Event {
    tokio::time::timeout(TIMEOUT, pool.next_event())
        .await
        .unwrap()
        .unwrap()
}

/// Commit and load two new oids through `pool`, so the cache holds both.
/// Return them with the tid of the commit.
async fn cached_pair(pool: &Pool) -> (Oid, Oid, Tid) {
    let oids = pool.new_oids(2).await.unwrap();
    let mut txn = pool.begin_with(b"", b"", b"", None, " ").await.unwrap();
    txn.store(oids[0], Tid(0), &record(b"a")).await.unwrap();
    txn.store(oids[1], Tid(0), &record(b"b")).await.unwrap();
    txn.vote().await.unwrap();
    let tid = txn.finish().await.unwrap().turn().await.unwrap().tid();
    let cache = pool.cache().unwrap();
    assert!(cache.load(oids[0]).is_some() && cache.load(oids[1]).is_some());
    (oids[0], oids[1], tid)
}

/// Lose the listener of `pool`, and confirm the stale mark.
async fn lose(pool: &Pool) {
    pool.listener().unwrap().disconnect();
    assert_eq!(next_event(pool).await, Event::Delivery(Delivery::Stale));
    pool.delivered();
    assert!(!pool.is_connected());
}

#[tokio::test]
async fn a_pool_connects_again_and_verifies_its_cache_through_get_invalidations() {
    let server = start("reconnect-quick");
    let (gate, opened) = watch::channel(false);
    let pool = open_reconnecting(&server, gated(server.addr(), Options::default(), opened)).await;
    let (changed, kept, first) = cached_pair(&pool).await;
    // A transaction that begins before the loss never commits after it.
    let mut lost = pool.begin_with(b"", b"", b"", None, " ").await.unwrap();
    lose(&pool).await;
    assert!(matches!(pool.load(kept).await, Err(Error::Disconnected)));
    assert!(matches!(
        pool.begin_with(b"", b"", b"", None, " ").await,
        Err(Error::Disconnected)
    ));

    // Another client commits while the pool has no listener.
    let other = Client::connect(server.addr(), "1").await.unwrap();
    let tid = commit_other(&other, changed, first, &record(b"c")).await;
    gate.send_replace(true);

    // The pool gets the commit through getInvalidations, once, before
    // its last tid moves.
    assert_eq!(
        next_event(&pool).await,
        Event::Delivery(Delivery::Invalidate(Invalidation {
            tid,
            oids: vec![changed],
        }))
    );
    assert_eq!(pool.last_transaction(), first);
    pool.delivered();
    assert_eq!(pool.last_transaction(), tid);
    assert_eq!(
        next_event(&pool).await,
        Event::Connected(Verification::Quick)
    );
    pool.delivered();
    assert!(pool.is_connected());

    // The entry of the changed oid ends. The other entry stays.
    let cache = pool.cache().unwrap();
    assert_eq!(cache.load(changed), None);
    assert_eq!(cache.load(kept), Some((record(b"b"), first)));
    assert_eq!(pool.load(changed).await.unwrap(), (record(b"c"), tid));

    // The transaction of the lost server fails, and the pool commits.
    lost.store(kept, first, &record(b"d")).await.ok();
    lost.vote().await.ok();
    assert!(matches!(lost.finish().await, Err(Error::Disconnected)));
    let again = commit(&pool, kept, first, &record(b"e")).await.unwrap();
    assert!(again.tid() > tid);
    drop(again);

    // The next commit of the other client comes on the new listener.
    let later = commit_other(&other, changed, tid, &record(b"f")).await;
    assert_eq!(
        next_event(&pool).await,
        Event::Delivery(Delivery::Invalidate(Invalidation {
            tid: later,
            oids: vec![changed],
        }))
    );
    pool.delivered();
}

#[tokio::test]
async fn a_pool_without_a_commit_on_the_server_keeps_its_cache() {
    let server = start("reconnect-kept");
    let (gate, opened) = watch::channel(true);
    let pool = open_reconnecting(&server, gated(server.addr(), Options::default(), opened)).await;
    let (changed, kept, first) = cached_pair(&pool).await;
    lose(&pool).await;
    assert_eq!(
        next_event(&pool).await,
        Event::Connected(Verification::Kept)
    );
    pool.delivered();
    assert_eq!(pool.last_transaction(), first);
    let cache = pool.cache().unwrap();
    assert!(cache.load(changed).is_some() && cache.load(kept).is_some());
    drop(gate);
}

#[tokio::test]
async fn a_pool_drops_its_cache_when_the_server_does_not_keep_the_commits() {
    let dir = fresh_dir("reconnect-dropped");
    let server =
        Server::start(ServeOptions::new(dir.join("store")).invalidation_queue_size(1)).unwrap();
    let (gate, opened) = watch::channel(false);
    let pool = open_reconnecting(&server, gated(server.addr(), Options::default(), opened)).await;
    let (changed, kept, first) = cached_pair(&pool).await;
    lose(&pool).await;
    let other = Client::connect(server.addr(), "1").await.unwrap();
    let tid = commit_other(&other, changed, first, &record(b"c")).await;
    let tid = commit_other(&other, changed, tid, &record(b"d")).await;
    gate.send_replace(true);

    // The consumer gets no invalidation, but the mark of a dropped cache.
    // Its confirmation moves the last tid.
    assert_eq!(
        next_event(&pool).await,
        Event::Connected(Verification::Dropped)
    );
    assert_eq!(pool.last_transaction(), first);
    pool.delivered();
    assert_eq!(pool.last_transaction(), tid);
    let cache = pool.cache().unwrap();
    assert_eq!(cache.load(kept), None);
    assert_eq!(cache.applied(), tid);
}

#[tokio::test]
async fn a_pool_refuses_a_server_behind_its_cache() {
    let server = start("reconnect-refused");
    let empty = start("reconnect-empty");
    let (gate, opened) = watch::channel(true);
    let pool = open_reconnecting(&server, gated(empty.addr(), Options::default(), opened)).await;
    cached_pair(&pool).await;
    lose(&pool).await;
    let Event::Refused(text) = next_event(&pool).await else {
        panic!("the pool did not refuse the server");
    };
    assert!(text.contains(&empty.addr().to_string()), "{text}");
    pool.delivered();
    assert!(!pool.is_connected());
    assert!(matches!(pool.new_oids(1).await, Err(Error::Disconnected)));
    drop(gate);
}

#[tokio::test]
async fn a_pool_without_a_listener_connects_in_the_background() {
    let server = start("reconnect-fresh");
    let other = Client::connect(server.addr(), "1").await.unwrap();
    let oid = other.new_oids(1).await.unwrap()[0];
    let tid = commit_other(&other, oid, Tid(0), &record(b"a")).await;
    let (gate, opened) = watch::channel(false);
    let reconnect = Reconnect::new(gated(server.addr(), Options::default(), opened), POLL);
    let cache = Cache::new(1 << 20).unwrap();
    let pool = Pool::with_reconnect(Arc::new(Tokio), None, 2, Some(cache), reconnect);
    assert!(!pool.is_connected());
    assert!(pool.listener().is_none());
    assert!(matches!(pool.load(oid).await, Err(Error::Disconnected)));

    gate.send_replace(true);
    assert_eq!(
        next_event(&pool).await,
        Event::Connected(Verification::Kept)
    );
    pool.delivered();
    assert!(pool.is_connected());
    assert_eq!(pool.last_transaction(), tid);
    assert_eq!(pool.load(oid).await.unwrap(), (record(b"a"), tid));
    assert_eq!(pool.cache().unwrap().applied(), tid);
}

#[tokio::test]
async fn a_pool_on_a_read_only_server_takes_a_writable_one() {
    let dir = fresh_dir("reconnect-upgrade");
    drop(Server::start(ServeOptions::new(dir.join("read-only"))).unwrap());
    let read_only =
        Server::start(ServeOptions::new(dir.join("read-only")).read_only(true)).unwrap();
    let writable = start("reconnect-writable");
    let (gate, opened) = watch::channel(false);
    let (ro, rw) = (read_only.addr(), writable.addr());
    let open: Open = Arc::new(move |only_writable| {
        let open = *opened.borrow();
        Box::pin(async move {
            let (addr, options) = match (only_writable, open) {
                (true, false) => return Err(Error::Disconnected),
                (true, true) => (rw, Options::default()),
                (false, _) => {
                    let fallback = Options {
                        read_only_fallback: true,
                        ..Options::default()
                    };
                    (ro, fallback)
                }
            };
            let client = Client::open(Arc::new(Tokio), addr, "1", &options).await?;
            let connect: Connect = Arc::new(move || Box::pin(Client::connect(addr, "1")));
            Ok((client, connect))
        })
    });
    let reconnect = Reconnect {
        upgrade: true,
        ..Reconnect::new(open, POLL)
    };
    let pool = Pool::with_reconnect(Arc::new(Tokio), None, 2, None, reconnect);
    assert_eq!(
        next_event(&pool).await,
        Event::Connected(Verification::Kept)
    );
    pool.delivered();
    assert!(pool.listener().unwrap().read_only());

    // When the writable server opens, the pool leaves the read-only one.
    gate.send_replace(true);
    assert_eq!(next_event(&pool).await, Event::Delivery(Delivery::Stale));
    pool.delivered();
    assert_eq!(
        next_event(&pool).await,
        Event::Connected(Verification::Kept)
    );
    pool.delivered();
    let listener = pool.listener().unwrap();
    assert!(!listener.read_only());
    assert_eq!(listener.address(), &zoe_client::Address::Tcp(rw));
    let oid = pool.new_oids(1).await.unwrap()[0];
    let tid = commit(&pool, oid, Tid(0), &record(b"w"))
        .await
        .unwrap()
        .tid();
    let check = Client::connect(rw, "1").await.unwrap();
    assert_eq!(check.load(oid).await.unwrap(), (record(b"w"), tid));
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
    pool.listener().unwrap().disconnect();
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

#[tokio::test]
async fn blobs_download_through_any_connection_of_the_pool() {
    let server = start("blobs");
    let pool = open(&server, 2).await;
    let oid = pool.new_oids(1).await.unwrap()[0];
    let blob: Vec<u8> = (0..3 * 65_536 + 7).map(|n| (n % 251) as u8).collect();
    let mut txn = pool.begin_with(b"", b"", b"", None, " ").await.unwrap();
    txn.store_blob_from(oid, Tid(0), &record(b"blob"), &mut blob.as_slice())
        .await
        .unwrap();
    txn.vote().await.unwrap();
    let tid = txn.finish().await.unwrap().turn().await.unwrap().tid();

    // The downloads take the connection of the commit, not the listener.
    for _ in 0..4 {
        let mut sink = Vec::new();
        let size = pool.send_blob(oid, tid, &mut sink).await.unwrap();
        assert_eq!(size, blob.len() as u64);
        assert!(sink == blob, "the download gives the committed bytes");
    }
    // Downloads of one blob at once on the pool all get the whole blob.
    let downloads = (0..6).map(|_| async {
        let mut sink = Vec::new();
        pool.send_blob(oid, tid, &mut sink).await.map(|_| sink)
    });
    for sink in futures::future::join_all(downloads).await {
        assert!(sink.unwrap() == blob);
    }
    // A serial without a blob fails with the error of the server.
    let missing = pool.send_blob(oid, Tid(tid.0 + 1), &mut Vec::new()).await;
    assert!(
        matches!(&missing, Err(Error::Server { class, .. }) if class.ends_with("POSKeyError")),
        "{missing:?}"
    );
}

#[tokio::test]
async fn a_stale_turn_and_a_quick_verification_at_the_last_tid_give_no_invalidation() {
    let server = start("reconnect-stale-turn");
    let proxy = proxy(server.addr()).await;
    // The listener reads through the proxy. The connections that commit
    // reach the server directly.
    let listener = Client::connect(proxy.addr, "1").await.unwrap();
    let addr = server.addr();
    let connect: Connect = Arc::new(move || Box::pin(Client::connect(addr, "1")));
    let (_gate, opened) = watch::channel(true);
    let reconnect = Reconnect::new(gated(addr, Options::default(), opened), POLL);
    let cache = Cache::new(1 << 20).unwrap();
    let pool = Pool::with_reconnect(
        Arc::new(Tokio),
        Some((listener, connect)),
        2,
        Some(cache),
        reconnect,
    );
    let (changed, kept, first) = cached_pair(&pool).await;

    // The finish has its reply, but the listener does not see its tid.
    proxy.pause.send_replace(true);
    let mut txn = pool.begin_with(b"", b"", b"", None, " ").await.unwrap();
    txn.store(changed, first, &record(b"c")).await.unwrap();
    txn.vote().await.unwrap();
    let finished = txn.finish().await.unwrap();
    let tid = finished.tid();
    let turn = tokio::spawn(finished.turn());

    // The loss gives the stale mark, then the turn, which moves the last
    // tid past `applied` of the cache.
    pool.listener().unwrap().disconnect();
    assert_eq!(next_event(&pool).await, Event::Delivery(Delivery::Stale));
    pool.delivered();
    let turn = turn.await.unwrap().unwrap();
    assert!(turn.stale());
    drop(turn);
    assert_eq!(pool.last_transaction(), tid);
    let cache = pool.cache().unwrap();
    assert_eq!(cache.applied(), first);

    // getInvalidations(applied) gives the tid of the pool: the consumer
    // gets no invalidation, and only the cache moves.
    assert_eq!(
        next_event(&pool).await,
        Event::Connected(Verification::Quick)
    );
    pool.delivered();
    assert_eq!(pool.last_transaction(), tid);
    assert_eq!(cache.applied(), tid);
    assert_eq!(cache.load(changed), None);
    assert_eq!(cache.load(kept), Some((record(b"b"), first)));
    assert_eq!(pool.load(changed).await.unwrap(), (record(b"c"), tid));
}

#[tokio::test]
async fn an_upgrade_whose_listener_changed_during_the_open_tries_again() {
    let dir = fresh_dir("reconnect-upgrade-race");
    drop(Server::start(ServeOptions::new(dir.join("read-only"))).unwrap());
    let read_only =
        Server::start(ServeOptions::new(dir.join("read-only")).read_only(true)).unwrap();
    let writable = start("reconnect-upgrade-race-writable");
    let (ro, rw) = (read_only.addr(), writable.addr());
    // The first writable open waits until the test releases it.
    let (release, released) = watch::channel(false);
    let upgrades = Arc::new(AtomicUsize::new(0));
    let count = upgrades.clone();
    let open: Open = Arc::new(move |only_writable| {
        let first = only_writable && count.fetch_add(1, Ordering::SeqCst) == 0;
        let mut released = released.clone();
        Box::pin(async move {
            if first {
                let _ = released.wait_for(|released| *released).await;
            }
            let (addr, options) = match only_writable {
                true => (rw, Options::default()),
                false => {
                    let fallback = Options {
                        read_only_fallback: true,
                        ..Options::default()
                    };
                    (ro, fallback)
                }
            };
            let client = Client::open(Arc::new(Tokio), addr, "1", &options).await?;
            let connect: Connect = Arc::new(move || Box::pin(Client::connect(addr, "1")));
            Ok((client, connect))
        })
    });
    let reconnect = Reconnect {
        upgrade: true,
        ..Reconnect::new(open, POLL)
    };
    let pool = Pool::with_reconnect(Arc::new(Tokio), None, 2, None, reconnect);
    assert_eq!(
        next_event(&pool).await,
        Event::Connected(Verification::Kept)
    );
    pool.delivered();
    wait_until(|| upgrades.load(Ordering::SeqCst) >= 1, "upgrade").await;

    // The read-only listener is lost while the upgrade opens, and the pool
    // connects again to the read-only server.
    pool.listener().unwrap().disconnect();
    assert_eq!(next_event(&pool).await, Event::Delivery(Delivery::Stale));
    pool.delivered();
    assert_eq!(
        next_event(&pool).await,
        Event::Connected(Verification::Kept)
    );
    pool.delivered();
    assert!(pool.listener().unwrap().read_only());

    // The late writable listener does not count. The next attempt takes
    // the writable server.
    release.send_replace(true);
    assert_eq!(next_event(&pool).await, Event::Delivery(Delivery::Stale));
    pool.delivered();
    assert_eq!(
        next_event(&pool).await,
        Event::Connected(Verification::Kept)
    );
    pool.delivered();
    let listener = pool.listener().unwrap();
    assert!(!listener.read_only());
    assert_eq!(listener.address(), &zoe_client::Address::Tcp(rw));
    assert!(upgrades.load(Ordering::SeqCst) >= 2);
}

#[tokio::test]
async fn a_closed_pool_opens_no_listener() {
    let opens = Arc::new(AtomicUsize::new(0));
    let count = opens.clone();
    let open: Open = Arc::new(move |_| {
        count.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Err(Error::Disconnected) })
    });
    let pool = Pool::with_reconnect(Arc::new(Tokio), None, 2, None, Reconnect::new(open, POLL));
    wait_until(|| opens.load(Ordering::SeqCst) >= 3, "attempt").await;
    pool.disconnect();
    let after = opens.load(Ordering::SeqCst);
    tokio::time::sleep(POLL * 10).await;
    assert_eq!(opens.load(Ordering::SeqCst), after);
    assert_eq!(pool.next_event().await, None);
}
