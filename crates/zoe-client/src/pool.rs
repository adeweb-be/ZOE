//! A pool of connections to one storage, with one ordered stream of
//! deliveries (ADR 0036).
//!
//! Each transaction takes a free connection of the pool from `begin` to the
//! end of its finish or its abort. The pool opens a connection only when a
//! transaction finds none free, up to a bound, and a transaction waits for
//! a free connection when all of them are busy.
//!
//! One more connection, the listener, never commits. The server sends it
//! the invalidation of each commit of the other connections, the commits
//! of the pool included, in tid order. One dispatcher task reads that
//! stream and delivers each tid in turn:
//!
//! - a tid of another client goes to the consumer of the pool as
//!   `Delivery::Invalidate`, through `next_delivery` and `delivered`;
//! - a tid of the pool gives the turn to the finish that committed it. The
//!   caller of the finish runs its callback, then drops its `Turn`.
//!
//! The dispatcher delivers the next tid only after the consumer or the
//! finish confirmed the last one. `last_transaction` is the last tid that
//! the pool delivered. A finish of tid T gets its turn only after the pool
//! delivered every tid below T.
//!
//! While a finish waits for its reply, the dispatcher cannot tell if a new
//! tid is the tid of that finish. It holds the tid until each finish that
//! counted as in flight when the tid came has its reply or failed. A finish
//! counts as in flight from before its frame goes out.
//!
//! A lost or lagged listener is a lost connection. The consumer gets
//! `Delivery::Stale`. Then each finish that had its reply gets its turn,
//! in tid order, with `Turn::stale` true. A finish that had no reply fails
//! with `Error::Disconnected`, and so does each later `begin` and finish.
//!
//! A pool can hold a client cache (ADR 0034). The dispatcher applies each
//! tid to the cache before the consumer or the finish gets it: a tid of
//! another client ends the current entry of each of its oids, and a tid of
//! the pool also adds the data of its commit. A load stores its reply with
//! the position of the connection that carried it. A lost or lagged
//! listener, and the close, drop every entry, and the cache stores nothing
//! more.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock};

use tokio::sync::watch;
use zoe_core::{Oid, Tid};

use crate::Error;
use crate::cache::Cache;
use crate::client::{Client, HistoryEntry, Invalidation, Invalidations, Record, Transaction};
use crate::protocol::MAX_TID;
use crate::runtime::{BoxFuture, Runtime};

/// A function that opens one more connection of the pool to the server of
/// the listener.
pub type Connect = Arc<dyn Fn() -> BoxFuture<Result<Client, Error>> + Send + Sync>;

/// A tid that the pool gives to its consumer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Delivery {
    /// The commit of another client: its tid and its oids.
    Invalidate(Invalidation),
    /// The pool lost its listener. Any object can be stale.
    Stale,
}

/// The counts of the hold of a pool.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    /// The tids that the dispatcher held while a finish that counted as in
    /// flight when the tid came waited for its reply.
    pub held: usize,
    /// The finishes whose reply came while the dispatcher held a lower tid.
    pub waited: usize,
}

/// A pool of connections to one storage, with one ordered stream of
/// deliveries.
pub struct Pool {
    shared: Arc<Shared>,
}

/// The state that the pool, its transactions, its turns and its dispatcher
/// share.
struct Shared {
    listener: Arc<Client>,
    connect: Connect,
    bound: usize,
    /// The client cache, if the pool has one.
    cache: Option<Arc<Cache>>,
    /// The connections that commit and are open. A load picks one of them
    /// or the listener. The lock is held only to pick or to change one.
    conns: RwLock<Vec<Arc<Client>>>,
    next_load: AtomicUsize,
    state: Mutex<State>,
    /// A new value tells each waiter that the state changed.
    changed: watch::Sender<u64>,
}

/// The state of the connections and of the order of the pool.
#[derive(Default)]
struct State {
    closed: bool,
    /// The listener is lost or lagged.
    lost: bool,
    /// The connections that commit and wait for a transaction.
    free: Vec<Arc<Client>>,
    /// The connections that commit: open, or that open now.
    opened: usize,
    /// The number of the next finish.
    next_seq: u64,
    /// The finishes that wait for their reply.
    in_flight: BTreeSet<u64>,
    /// The finishes that waited for their reply when the listener was lost.
    doomed: BTreeSet<u64>,
    /// The tids of the finishes that have their reply, and wait for or
    /// have their turn.
    replied: BTreeMap<u64, Slot>,
    /// The tid of the finish that runs its callback now.
    running: Option<u64>,
    /// The tids whose finish dropped after its turn came, before it ran.
    abandoned: BTreeSet<u64>,
    /// The tid that the dispatcher delivers now.
    head: Option<u64>,
    /// The delivery that waits for the consumer, and whether the consumer
    /// took it.
    offer: Option<(Delivery, bool)>,
    /// The deliveries that the dispatcher started, and those that ended.
    started: u64,
    ended: u64,
    /// The last tid that the pool delivered.
    last: u64,
    counters: Counters,
}

/// A finish that has its reply.
#[derive(Clone, Copy, Debug, Default)]
struct Slot {
    /// The dispatcher gave the finish its turn.
    turn: bool,
    /// The turn came after the listener was lost.
    stale: bool,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }

    /// Wake each waiter, so that it reads the state again.
    fn bump(&self) {
        self.changed
            .send_modify(|version| *version = version.wrapping_add(1));
    }

    /// Wait until `check` gives a value. A waiter subscribes before it
    /// reads the state, so it never misses a change.
    async fn until<T>(&self, mut check: impl FnMut(&mut State) -> Option<T>) -> T {
        loop {
            let mut changes = self.changed.subscribe();
            if let Some(value) = check(&mut self.lock()) {
                return value;
            }
            // The pool holds the sender, so the channel never closes.
            let _ = changes.changed().await;
        }
    }

    /// Return a connection of the pool for a load, in turn: the listener
    /// or a connection that commits. Skip a connection that is lost. When
    /// all are lost, return the listener, so the load fails with
    /// `Error::Disconnected`.
    fn any_connection(&self) -> Arc<Client> {
        let conns = self.conns.read().unwrap_or_else(PoisonError::into_inner);
        let count = conns.len() + 1;
        let start = self.next_load.fetch_add(1, Ordering::Relaxed);
        for step in 0..count {
            let conn = match ((start + step) % count).checked_sub(1) {
                Some(i) => &conns[i],
                None => &self.listener,
            };
            if conn.is_connected() {
                return conn.clone();
            }
        }
        self.listener.clone()
    }

    /// Forget the connection `client`, which closed or which the pool
    /// closes.
    fn forget(&self, client: &Arc<Client>) {
        let mut conns = self.conns.write().unwrap_or_else(PoisonError::into_inner);
        conns.retain(|conn| !Arc::ptr_eq(conn, client));
    }
}

impl Pool {
    /// Start a pool on the open connection `listener`. `connect` opens each
    /// other connection, at most `bound` of them, each for one transaction
    /// at a time. The dispatcher runs as a task of `runtime`.
    ///
    /// The pool starts at the last tid of the listener. It reads the
    /// invalidations of the listener from now on. The pool needs a consumer
    /// of its deliveries: the dispatcher waits for each one.
    pub fn new(
        runtime: Arc<dyn Runtime>,
        listener: Client,
        connect: Connect,
        bound: usize,
    ) -> Pool {
        Pool::with_cache(runtime, listener, connect, bound, None)
    }

    /// Start a pool as `new` does, with the client cache `cache`. The
    /// cache must be empty: its `applied` moves to the last tid of the
    /// listener.
    pub fn with_cache(
        runtime: Arc<dyn Runtime>,
        listener: Client,
        connect: Connect,
        bound: usize,
        cache: Option<Cache>,
    ) -> Pool {
        // Subscribe first, then read the last tid: the reader raises the
        // last tid before it sends an invalidation.
        let invalidations = listener.invalidations();
        let state = State {
            last: listener.last_transaction().0,
            ..State::default()
        };
        // An empty cache is true at any tid.
        if let Some(cache) = &cache {
            cache.apply(Tid(state.last), &[]);
        }
        let shared = Arc::new(Shared {
            listener: Arc::new(listener),
            connect,
            bound: bound.max(1),
            cache: cache.map(Arc::new),
            conns: RwLock::new(Vec::new()),
            next_load: AtomicUsize::new(0),
            state: Mutex::new(state),
            changed: watch::channel(0).0,
        });
        runtime.spawn(Box::pin(dispatch(shared.clone(), invalidations)));
        Pool { shared }
    }

    /// Return the listener: the reply of its `get_info`, its extensions,
    /// its address and whether it registered read-only.
    pub fn listener(&self) -> &Client {
        &self.shared.listener
    }

    /// Return the most connections that commit at once.
    pub fn bound(&self) -> usize {
        self.shared.bound
    }

    /// Return the client cache of the pool, if it has one. A hit there
    /// sends nothing to the server.
    pub fn cache(&self) -> Option<&Arc<Cache>> {
        self.shared.cache.as_ref()
    }

    /// Return the counts of the hold.
    pub fn counters(&self) -> Counters {
        self.shared.lock().counters
    }

    /// Tell if the pool is open: not closed, and its listener is not lost.
    pub fn is_connected(&self) -> bool {
        let state = self.shared.lock();
        !state.closed && !state.lost && self.shared.listener.is_connected()
    }

    /// Return the last tid that the pool delivered: the tid of `register`
    /// of the listener at the start, then each tid that the consumer or a
    /// finish confirmed.
    pub fn last_transaction(&self) -> Tid {
        Tid(self.shared.lock().last)
    }

    /// Return the last tid that the pool delivered, after the delivery that
    /// runs now, if one runs. A caller that runs that delivery must call
    /// `last_transaction`: this call would wait for itself.
    pub async fn settled_transaction(&self) -> Tid {
        let target = self.shared.lock().started;
        self.shared
            .until(|state| (state.ended >= target || state.closed).then_some(Tid(state.last)))
            .await
    }

    /// Return `n` new oids, through the listener.
    pub async fn new_oids(&self, n: usize) -> Result<Vec<Oid>, Error> {
        self.shared.listener.new_oids(n).await
    }

    /// Load the record of `oid` that was current just before `tid` from
    /// the server, through any connection of the pool. Return the position
    /// of that connection with the record (see `Client::load_before_at`).
    /// It does not read the cache. It stores the record in the cache, if
    /// the pool has one: a current record only when `applied` of the cache
    /// is not above that position.
    pub async fn load_before(&self, oid: Oid, tid: Tid) -> Result<(Option<Record>, Tid), Error> {
        let (record, position) = self
            .shared
            .any_connection()
            .load_before_at(oid, tid)
            .await?;
        if let (Some(cache), Some(record)) = (&self.shared.cache, &record) {
            cache.store(oid, record.tid, record.end, &record.data, position);
        }
        Ok((record, position))
    }

    /// Load the current record of `oid` from the server, through any
    /// connection, and store it in the cache as `load_before` does. An oid
    /// without a record fails with the `POSKeyError` of the server.
    pub async fn load(&self, oid: Oid) -> Result<(Vec<u8>, Tid), Error> {
        match self.load_before(oid, MAX_TID).await? {
            (Some(record), _) => Ok((record.data, record.tid)),
            (None, _) => Err(Error::Server {
                class: "ZODB.POSException.POSKeyError".into(),
                message: format!("{oid:?}"),
            }),
        }
    }

    /// Load the record of `oid` that the transaction `serial` wrote,
    /// through any connection.
    pub async fn load_serial(&self, oid: Oid, serial: Tid) -> Result<Vec<u8>, Error> {
        self.shared.any_connection().load_serial(oid, serial).await
    }

    /// Download the blob of `oid` that the transaction `serial` wrote into
    /// `sink`, through any connection, as `Client::send_blob` does. The
    /// pieces of the download come on the connection that carries its
    /// `sendBlob`. Return the number of bytes.
    pub async fn send_blob<W: Write>(
        &self,
        oid: Oid,
        serial: Tid,
        sink: &mut W,
    ) -> Result<u64, Error> {
        self.shared
            .any_connection()
            .send_blob(oid, serial, sink)
            .await
    }

    /// Return at most `size` entries of the history of `oid`.
    pub async fn history(&self, oid: Oid, size: usize) -> Result<Vec<HistoryEntry>, Error> {
        self.shared.any_connection().history(oid, size).await
    }

    /// Return the next value of the sequence `name`, through the listener.
    pub async fn sequence_next(&self, name: &str) -> Result<u64, Error> {
        self.shared.listener.sequence_next(name).await
    }

    /// Begin a transaction on a free connection, as `Client::begin_with`
    /// does. Open a connection when none is free and the pool has less than
    /// its bound. Else wait for a free one. After the close or a lost
    /// listener, fail with `Error::Disconnected`.
    pub async fn begin_with(
        &self,
        user: &[u8],
        description: &[u8],
        extension: &[u8],
        tid: Option<Tid>,
        status: &str,
    ) -> Result<PoolTransaction, Error> {
        let lease = self.lease().await?;
        let txn = lease
            .client()
            .begin_with(user, description, extension, tid, status)
            .await?;
        Ok(PoolTransaction {
            txn: Some(txn),
            lease,
            writes: Vec::new(),
        })
    }

    /// Take a free connection, open one, or wait for one. A free
    /// connection that is lost closes and leaves the pool.
    async fn lease(&self) -> Result<Lease, Error> {
        let shared = &self.shared;
        let mut dead = Vec::new();
        let taken = shared
            .until(|state| {
                if state.closed || state.lost {
                    return Some(Err(Error::Disconnected));
                }
                while let Some(client) = state.free.pop() {
                    if client.is_connected() {
                        return Some(Ok(Some(client)));
                    }
                    state.opened = state.opened.saturating_sub(1);
                    dead.push(client);
                }
                if state.opened < shared.bound {
                    state.opened += 1;
                    return Some(Ok(None));
                }
                None
            })
            .await;
        for client in &dead {
            shared.forget(client);
            client.disconnect();
        }
        let client = match taken? {
            Some(client) => client,
            None => {
                // The slot goes back to the pool when the connect fails or
                // its future drops.
                let mut slot = Opening {
                    shared: shared.clone(),
                    armed: true,
                };
                let client = Arc::new((shared.connect)().await?);
                slot.armed = false;
                let mut conns = shared.conns.write().unwrap_or_else(PoisonError::into_inner);
                conns.push(client.clone());
                client
            }
        };
        Ok(Lease {
            shared: shared.clone(),
            client: Some(client),
        })
    }

    /// Return the next delivery for the consumer. Wait until the dispatcher
    /// gives one. Return `None` after the close. The consumer confirms each
    /// delivery with `delivered`.
    pub async fn next_delivery(&self) -> Option<Delivery> {
        self.shared
            .until(|state| match &mut state.offer {
                Some((delivery, taken @ false)) => {
                    *taken = true;
                    Some(Some(delivery.clone()))
                }
                _ if state.closed => Some(None),
                _ => None,
            })
            .await
    }

    /// Confirm the delivery that `next_delivery` gave: the consumer applied
    /// it. The pool then delivers the next tid.
    pub fn delivered(&self) {
        let mut state = self.shared.lock();
        if !matches!(state.offer, Some((_, true))) {
            return;
        }
        if let Some((Delivery::Invalidate(invalidation), _)) = state.offer.take() {
            state.last = state.last.max(invalidation.tid.0);
        }
        state.ended += 1;
        drop(state);
        self.shared.bump();
    }

    /// Close the pool and each of its connections, and do not wait for
    /// their tasks. Each call that waits fails with `Error::Disconnected`,
    /// and `next_delivery` gives `None`.
    pub fn disconnect(&self) {
        let free = {
            let mut state = self.shared.lock();
            state.closed = true;
            std::mem::take(&mut state.free)
        };
        if let Some(cache) = &self.shared.cache {
            cache.disable();
        }
        self.shared.bump();
        self.shared.listener.disconnect();
        let conns = self
            .shared
            .conns
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        for conn in &conns {
            conn.disconnect();
        }
        drop(free);
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        self.disconnect();
    }
}

impl std::fmt::Debug for Pool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pool")
            .field("last_transaction", &self.last_transaction())
            .finish_non_exhaustive()
    }
}

/// A connection that a transaction holds. The drop gives it back to the
/// pool, or closes it when it is lost or the pool is closed.
struct Lease {
    shared: Arc<Shared>,
    client: Option<Arc<Client>>,
}

impl Lease {
    fn client(&self) -> &Client {
        self.client.as_ref().expect("a lease holds its connection")
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let Some(client) = self.client.take() else {
            return;
        };
        let keep = {
            let mut state = self.shared.lock();
            let keep = !state.closed && client.is_connected();
            if keep {
                state.free.push(client.clone());
            } else {
                state.opened = state.opened.saturating_sub(1);
            }
            keep
        };
        if !keep {
            self.shared.forget(&client);
            client.disconnect();
        }
        self.shared.bump();
    }
}

/// A slot of `opened` for a connection that opens now. The drop gives the
/// slot back, unless the connection opened.
struct Opening {
    shared: Arc<Shared>,
    armed: bool,
}

impl Drop for Opening {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        {
            let mut state = self.shared.lock();
            state.opened = state.opened.saturating_sub(1);
        }
        self.shared.bump();
    }
}

/// A transaction of the pool, on its own connection: `store`, then `vote`,
/// then `finish`. A transaction that is dropped before its finish or its
/// abort sends `tpc_abort`, as `Transaction` does, and gives its
/// connection back.
pub struct PoolTransaction {
    // The transaction drops before the lease, so its abort goes out before
    // another transaction takes the connection.
    txn: Option<Transaction>,
    lease: Lease,
    /// The stores of the transaction, for the cache of the pool, in order.
    /// The data is `None` when the cache cannot hold it, or when the server
    /// resolved the oid. Empty when the pool has no cache.
    writes: Vec<(Oid, Option<Vec<u8>>)>,
}

impl PoolTransaction {
    fn txn(&mut self) -> &mut Transaction {
        self.txn.as_mut().expect("an open transaction")
    }

    /// Keep the store of `data` for `oid`, for the cache of the pool.
    fn keep(&mut self, oid: Oid, data: &[u8]) {
        if let Some(cache) = &self.lease.shared.cache {
            let data = cache.fits(data.len()).then(|| data.to_vec());
            self.writes.push((oid, data));
        }
    }

    /// Store `data` for `oid`, as `Transaction::store` does.
    pub async fn store(&mut self, oid: Oid, serial: Tid, data: &[u8]) -> Result<(), Error> {
        self.txn().store(oid, serial, data).await?;
        self.keep(oid, data);
        Ok(())
    }

    /// Check `serial` of `oid` at the vote, as
    /// `Transaction::check_current_serial` does.
    pub async fn check_current_serial(&mut self, oid: Oid, serial: Tid) -> Result<(), Error> {
        self.txn().check_current_serial(oid, serial).await
    }

    /// Store `data` for `oid` with the blob `blob`, as
    /// `Transaction::store_blob` does.
    pub async fn store_blob(
        &mut self,
        oid: Oid,
        serial: Tid,
        data: &[u8],
        blob: &[u8],
    ) -> Result<(), Error> {
        self.txn().store_blob(oid, serial, data, blob).await?;
        self.keep(oid, data);
        Ok(())
    }

    /// Store `data` for `oid` with the blob that `blob` reads, as
    /// `Transaction::store_blob_from` does.
    pub async fn store_blob_from<R: Read>(
        &mut self,
        oid: Oid,
        serial: Tid,
        data: &[u8],
        blob: &mut R,
    ) -> Result<(), Error> {
        self.txn().store_blob_from(oid, serial, data, blob).await?;
        self.keep(oid, data);
        Ok(())
    }

    /// Store `data` for `oid` with the blob file `filename` of a shared
    /// blob directory, as `Transaction::store_blob_shared` does.
    pub async fn store_blob_shared(
        &mut self,
        oid: Oid,
        serial: Tid,
        data: &[u8],
        filename: &str,
    ) -> Result<(), Error> {
        self.txn()
            .store_blob_shared(oid, serial, data, filename)
            .await?;
        self.keep(oid, data);
        Ok(())
    }

    /// Vote, as `Transaction::vote_resolved` does. The cache of the pool
    /// will not hold the pickles of the oids that the server resolved. A
    /// conflict drops the current entry of its oid from the cache, as
    /// `tpc_vote` of `ZEO.ClientStorage` does.
    pub async fn vote_resolved(&mut self) -> Result<Vec<Oid>, Error> {
        let voted = self.txn().vote_resolved().await;
        if let Some(cache) = &self.lease.shared.cache {
            match &voted {
                Ok(resolved) => {
                    let resolved: HashSet<Oid> = resolved.iter().copied().collect();
                    for (oid, data) in &mut self.writes {
                        if resolved.contains(oid) {
                            *data = None;
                        }
                    }
                }
                Err(Error::Conflict { oid, .. } | Error::ReadConflict { oid, .. }) => {
                    cache.drop_current(*oid);
                }
                Err(_) => {}
            }
        }
        voted
    }

    /// Vote, as `Transaction::vote` does, with the cache rules of
    /// `vote_resolved`.
    pub async fn vote(&mut self) -> Result<(), Error> {
        self.vote_resolved().await.map(drop)
    }

    /// Finish the transaction after its vote, and wait for the reply of the
    /// server. Then `Finished::turn` waits for the turn of its tid.
    ///
    /// The finish counts as in flight from before its frame goes out, until
    /// its reply or its failure, also when its future is dropped. After the
    /// close or a lost listener, it fails with `Error::Disconnected` and
    /// sends nothing. A finish that waited for its reply when the listener
    /// was lost fails with `Error::Disconnected` too: its outcome counts as
    /// unknown.
    pub async fn finish(mut self) -> Result<Finished, Error> {
        let shared = self.lease.shared.clone();
        let seq = {
            let mut state = shared.lock();
            if state.closed || state.lost {
                return Err(Error::Disconnected);
            }
            let seq = state.next_seq;
            state.next_seq += 1;
            state.in_flight.insert(seq);
            seq
        };
        let mut flight = InFlight {
            shared: shared.clone(),
            seq,
            armed: true,
        };
        let txn = self.txn.take().expect("an open transaction");
        let writes = std::mem::take(&mut self.writes);
        let tid = txn.finish().await?;
        {
            let mut state = shared.lock();
            state.in_flight.remove(&seq);
            if state.doomed.remove(&seq) || state.closed {
                drop(state);
                drop(flight);
                return Err(Error::Disconnected);
            }
            if state.head.is_some_and(|head| head < tid.0) {
                state.counters.waited += 1;
            }
            state.replied.insert(tid.0, Slot::default());
        }
        flight.armed = false;
        drop(flight);
        shared.bump();
        Ok(Finished {
            shared,
            tid,
            armed: true,
            writes,
        })
    }

    /// Abort the transaction, and wait for the reply of the server.
    pub async fn abort(mut self) -> Result<(), Error> {
        let txn = self.txn.take().expect("an open transaction");
        txn.abort().await
    }
}

impl std::fmt::Debug for PoolTransaction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PoolTransaction")
            .field("txn", &self.txn)
            .finish_non_exhaustive()
    }
}

/// A finish that counts as in flight. The drop takes it out of the hold,
/// unless the finish got its reply.
struct InFlight {
    shared: Arc<Shared>,
    seq: u64,
    /// `true` until the finish got its reply.
    armed: bool,
}

impl Drop for InFlight {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        {
            let mut state = self.shared.lock();
            state.in_flight.remove(&self.seq);
            state.doomed.remove(&self.seq);
        }
        self.shared.bump();
    }
}

/// A finish that has the reply of the server: its commit is durable.
/// `turn` waits until the pool delivered every tid below it.
///
/// A `Finished` that is dropped before its turn leaves its tid to the
/// consumer of the pool, as the tid of another client.
pub struct Finished {
    shared: Arc<Shared>,
    tid: Tid,
    /// `true` until `turn` took the turn.
    armed: bool,
    /// The stores of the commit, for the cache of the pool.
    writes: Vec<(Oid, Option<Vec<u8>>)>,
}

impl Finished {
    /// Return the tid of the commit.
    pub fn tid(&self) -> Tid {
        self.tid
    }

    /// Wait for the turn of the tid: the pool delivered every tid below it.
    /// The caller runs the callback of its finish, then drops the `Turn`.
    /// After a lost listener, the turn comes after `Delivery::Stale`. After
    /// the close, it fails with `Error::Disconnected`.
    ///
    /// At the turn, before it returns, the commit goes into the cache of the
    /// pool: it ends the current entry of each oid that it wrote, and adds
    /// the data of each store that the server did not resolve.
    pub async fn turn(mut self) -> Result<Turn, Error> {
        let tid = self.tid;
        let stale = self
            .shared
            .until(|state| match state.replied.get(&tid.0) {
                Some(slot) if slot.turn => {
                    let stale = slot.stale;
                    state.replied.remove(&tid.0);
                    state.running = Some(tid.0);
                    Some(Ok(stale))
                }
                _ if state.closed => Some(Err(Error::Disconnected)),
                _ => None,
            })
            .await?;
        self.armed = false;
        if let Some(cache) = &self.shared.cache
            && !stale
        {
            cache.commit(tid, &last_writes(&self.writes));
        }
        Ok(Turn {
            shared: self.shared.clone(),
            tid,
            stale,
        })
    }
}

impl Drop for Finished {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        {
            let mut state = self.shared.lock();
            if let Some(slot) = state.replied.remove(&self.tid.0)
                && slot.turn
            {
                state.abandoned.insert(self.tid.0);
            }
        }
        self.shared.bump();
    }
}

impl std::fmt::Debug for Finished {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Finished").field("tid", &self.tid).finish()
    }
}

/// The turn of a commit of the pool: every tid below it was delivered. The
/// caller runs the callback of the finish while it holds the turn. The drop
/// of the turn delivers the tid, and the pool goes on.
pub struct Turn {
    shared: Arc<Shared>,
    tid: Tid,
    stale: bool,
}

impl Turn {
    /// Return the tid of the commit.
    pub fn tid(&self) -> Tid {
        self.tid
    }

    /// Tell if the turn came after a lost listener, after
    /// `Delivery::Stale`.
    pub fn stale(&self) -> bool {
        self.stale
    }
}

impl Drop for Turn {
    fn drop(&mut self) {
        {
            let mut state = self.shared.lock();
            state.running = None;
            state.last = state.last.max(self.tid.0);
            state.ended += 1;
        }
        self.shared.bump();
    }
}

impl std::fmt::Debug for Turn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Turn")
            .field("tid", &self.tid)
            .field("stale", &self.stale)
            .finish()
    }
}

/// Return the last write of each oid of `writes`, in the order of these
/// last writes.
fn last_writes(writes: &[(Oid, Option<Vec<u8>>)]) -> Vec<(Oid, Option<&[u8]>)> {
    let mut seen = HashSet::new();
    let mut last: Vec<_> = writes
        .iter()
        .rev()
        .filter(|(oid, _)| seen.insert(*oid))
        .map(|(oid, data)| (*oid, data.as_deref()))
        .collect();
    last.reverse();
    last
}

/// How the dispatcher delivers one tid.
enum Route {
    /// A commit of another client.
    Foreign,
    /// A commit of the pool: its finish has the turn.
    Own,
}

/// The dispatcher: deliver each tid of the listener in turn, until the
/// close or the loss of the listener.
async fn dispatch(shared: Arc<Shared>, mut invalidations: Invalidations) {
    loop {
        match invalidations.recv().await {
            Ok(invalidation) => match deliver(&shared, invalidation).await {
                Step::Next => {}
                Step::Closed => return,
                Step::Lost => {
                    if !shared.lock().closed {
                        lose(&shared).await;
                    }
                    return;
                }
            },
            Err(_) => {
                if !shared.lock().closed {
                    lose(&shared).await;
                }
                return;
            }
        }
    }
}

/// What the dispatcher does after a tid.
enum Step {
    /// Deliver the next tid.
    Next,
    /// The pool is closed.
    Closed,
    /// The listener was lost during the hold of the tid.
    Lost,
}

/// Deliver the tid of `invalidation`.
async fn deliver(shared: &Shared, invalidation: Invalidation) -> Step {
    let tid = invalidation.tid.0;
    let horizon = {
        let mut state = shared.lock();
        if state.closed {
            return Step::Closed;
        }
        // A tid at or before the start of the pool was before the open.
        if tid <= state.last {
            return Step::Next;
        }
        state.head = Some(tid);
        state.next_seq
    };
    shared.bump();
    let mut held = false;
    let hold = shared.until(|state| {
        if state.closed {
            return Some(None);
        }
        if let Some(slot) = state.replied.get_mut(&tid) {
            slot.turn = true;
            state.started += 1;
            return Some(Some(Route::Own));
        }
        // A finish counted after the tid came commits a later tid.
        if state.in_flight.range(..horizon).next().is_none() {
            state.started += 1;
            return Some(Some(Route::Foreign));
        }
        if !held {
            held = true;
            state.counters.held += 1;
        }
        None
    });
    // A lost listener ends the hold. Then each finish in flight fails.
    let route = tokio::select! {
        biased;
        route = hold => route,
        () = shared.listener.closed() => {
            shared.lock().head = None;
            return Step::Lost;
        }
    };
    let Some(route) = route else {
        return Step::Closed;
    };
    shared.bump();
    let delivered = match route {
        Route::Foreign => offer(shared, invalidation).await,
        Route::Own => match wait_for_turn(shared, tid).await {
            // The finish dropped before it ran: the consumer gets the tid.
            Some(true) => offer(shared, invalidation).await,
            Some(false) => true,
            None => false,
        },
    };
    shared.lock().head = None;
    if delivered { Step::Next } else { Step::Closed }
}

/// Apply the commit of another client to the cache, then offer it to the
/// consumer, and wait until the consumer confirmed it. Return false after
/// the close.
async fn offer(shared: &Shared, invalidation: Invalidation) -> bool {
    if let Some(cache) = &shared.cache {
        cache.apply(invalidation.tid, &invalidation.oids);
    }
    shared.lock().offer = Some((Delivery::Invalidate(invalidation), false));
    shared.bump();
    wait_for_consumer(shared).await
}

/// Wait until the consumer confirmed the offer. Return false after the
/// close.
async fn wait_for_consumer(shared: &Shared) -> bool {
    shared
        .until(|state| {
            if state.offer.is_none() {
                Some(true)
            } else if state.closed {
                Some(false)
            } else {
                None
            }
        })
        .await
}

/// Wait until the finish of `tid` ran with its turn, or dropped. Return
/// whether it dropped before it ran, or `None` after the close.
async fn wait_for_turn(shared: &Shared, tid: u64) -> Option<bool> {
    shared
        .until(|state| {
            if state.abandoned.remove(&tid) {
                return Some(Some(true));
            }
            if !state.replied.contains_key(&tid) && state.running != Some(tid) {
                return Some(Some(false));
            }
            if state.closed {
                return Some(None);
            }
            None
        })
        .await
}

/// The listener is lost or lagged. Fail the finishes that wait for their
/// reply. Give the consumer `Delivery::Stale`, then give each finish that
/// has its reply its turn, in tid order.
async fn lose(shared: &Shared) {
    // The cache no longer gets the commits in order: an empty cache is true
    // at any tid.
    if let Some(cache) = &shared.cache {
        cache.disable();
    }
    {
        let mut state = shared.lock();
        state.lost = true;
        let in_flight = std::mem::take(&mut state.in_flight);
        state.doomed.extend(in_flight);
        state.offer = Some((Delivery::Stale, false));
        state.started += 1;
    }
    shared.bump();
    shared.listener.disconnect();
    if !wait_for_consumer(shared).await {
        return;
    }
    loop {
        let next = {
            let mut state = shared.lock();
            let next = state.replied.iter_mut().next().map(|(tid, slot)| {
                slot.turn = true;
                slot.stale = true;
                *tid
            });
            if next.is_some() {
                state.started += 1;
            }
            next
        };
        let Some(tid) = next else { return };
        shared.bump();
        match wait_for_turn(shared, tid).await {
            // The consumer already treats every object as stale.
            Some(true) => {
                shared.lock().ended += 1;
                shared.bump();
            }
            Some(false) => {}
            None => return,
        }
    }
}
