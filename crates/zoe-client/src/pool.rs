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
//! A pool of `Pool::with_reconnect` then connects again, as
//! `ZEO.ClientStorage` does. It closes every connection of the lost server,
//! so a transaction that began before the loss never commits. It opens a
//! new listener at once, then every `poll`, and verifies the cache and the
//! last tid of the pool against it, as `verify` of ZEO does. Only then do
//! the connections of the new server serve calls. While the pool has no
//! listener, each call fails with `Error::Disconnected`. With `upgrade`, a
//! pool on a read-only server tries a writable server every `poll`, and
//! takes it.
//!
//! A pool can hold a client cache (ADR 0034). The dispatcher applies each
//! tid to the cache before the consumer or the finish gets it: a tid of
//! another client ends the current entry of each of its oids, and a tid of
//! the pool also adds the data of its commit. A load stores its reply with
//! the position of the connection that carried it. The close drops every
//! entry, and the cache stores nothing more. So does a lost or lagged
//! listener, unless the pool connects again: then the cache stays, and the
//! verification keeps it, ends some of its entries, or drops them all.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::Duration;

use rmpv::Value;
use tokio::sync::watch;
use zoe_core::{Oid, Tid};

use crate::Error;
use crate::cache::Cache;
use crate::client::{
    Client, HistoryEntry, Invalidation, Invalidations, Record, Transaction, Voted, merge,
};
use crate::protocol::{self, MAX_TID};
use crate::runtime::{BoxFuture, Runtime};

/// A function that opens one more connection of the pool to the server of
/// the listener.
pub type Connect = Arc<dyn Fn() -> BoxFuture<Result<Client, Error>> + Send + Sync>;

/// A function that opens a new listener, and returns it with the `Connect`
/// of its server. With `true`, it opens only a writable listener.
pub type Open = Arc<dyn Fn(bool) -> BoxFuture<Result<(Client, Connect), Error>> + Send + Sync>;

/// A function that the verification of a pool calls before it drops every
/// entry of the cache, with the reason: empty when the server does not
/// keep the commits after the cache, else the error of `getInvalidations`.
/// The pool drops the entries when its future ends. So the caller sees the
/// entries before the drop, as the `StaleCache` event of ZEO does.
pub type Dropping = Arc<dyn Fn(String) -> BoxFuture<()> + Send + Sync>;

/// The time between two attempts after the pool refused a server, as the
/// `register_failed_poll` of `ZEO.ClientStorage`.
pub const REFUSED_POLL: Duration = Duration::from_secs(9);

/// How a pool connects again after it lost its listener.
#[derive(Clone)]
pub struct Reconnect {
    /// Opens a new listener.
    pub open: Open,
    /// The time between two attempts.
    pub poll: Duration,
    /// The time before the next attempt after the pool refused a server.
    pub refused_poll: Duration,
    /// On a read-only server, try a writable server every `poll`.
    pub upgrade: bool,
    /// Called before a verification drops every entry of the cache.
    pub dropping: Option<Dropping>,
}

impl Reconnect {
    /// Connect again through `open` every `poll`, without `upgrade`.
    pub fn new(open: Open, poll: Duration) -> Reconnect {
        Reconnect {
            open,
            poll,
            refused_poll: REFUSED_POLL.max(poll),
            upgrade: false,
            dropping: None,
        }
    }
}

impl std::fmt::Debug for Reconnect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reconnect")
            .field("poll", &self.poll)
            .field("refused_poll", &self.refused_poll)
            .field("upgrade", &self.upgrade)
            .finish_non_exhaustive()
    }
}

/// A tid that the pool gives to its consumer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Delivery {
    /// The commit of another client: its tid and its oids.
    Invalidate(Invalidation),
    /// The pool lost its listener. Any object can be stale.
    Stale,
}

/// The result of the verification of a new listener.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verification {
    /// The server had no commit after the last tid of the pool, or the pool
    /// never had a listener.
    Kept,
    /// `getInvalidations` gave the commits after the base: `applied` of the
    /// cache, or the last tid of the pool without a cache. The consumer got
    /// them first, as one `Delivery::Invalidate`, also when the pool
    /// already delivered their tid.
    Quick,
    /// The server does not keep the commits after the last tid of the
    /// pool. The cache dropped every entry, and any object can be stale.
    Dropped,
}

/// How the verification of a cache that a file gave ended, at the open of
/// the pool.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileCheck {
    /// The server had no commit after `applied` of the file.
    Kept,
    /// `getInvalidations` gave the commits after `applied`: the cache
    /// dropped the current entry of each oid.
    Quick,
    /// The server does not keep the commits after `applied`: the cache
    /// dropped every entry.
    Dropped,
    /// `getInvalidations` failed with this text: the cache dropped every
    /// entry.
    Failed(String),
    /// The server holds no commit at `applied` of the file, so its history
    /// is not the one that the file saw, for example a server restored from
    /// an older copy: the cache dropped every entry.
    Other {
        /// `applied` of the file.
        applied: Tid,
    },
    /// `applied` of the file is above the last tid of the server: the
    /// cache dropped every entry.
    Ahead {
        /// `applied` of the file.
        applied: Tid,
        /// The last tid of the server.
        server: Tid,
    },
}

/// What the pool gives to its consumer: a delivery, or a change of the
/// connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// A tid, or the stale mark.
    Delivery(Delivery),
    /// The pool connected again, after the verification.
    Connected(Verification),
    /// The pool refused a server whose last tid is below the last tid of
    /// the pool or of its cache, as ZEO refuses it. It stays disconnected,
    /// and tries again after `Reconnect::refused_poll`.
    Refused(String),
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
    runtime: Arc<dyn Runtime>,
    bound: usize,
    /// The client cache, if the pool has one.
    cache: Option<Arc<Cache>>,
    /// How the pool connects again, if it does.
    reconnect: Option<Reconnect>,
    /// The connections of the server. A load picks one of them. The lock is
    /// held only to pick or to change one. A writer takes `state` first.
    server: RwLock<Server>,
    next_load: AtomicUsize,
    state: Mutex<State>,
    /// A new value tells each waiter that the state changed.
    changed: watch::Sender<u64>,
    /// The newest map of the `info` calls of a connection that left the
    /// pool, with its stamp. So `Pool::info` never goes back to older
    /// sizes when a connection goes. A new listener empties it.
    pushed: Mutex<Option<(u64, Value)>>,
}

/// The connections to the server of the pool.
#[derive(Default)]
struct Server {
    /// The listener, or `None` while the pool is disconnected.
    listener: Option<Arc<Client>>,
    /// The connections that commit and are open.
    conns: Vec<Arc<Client>>,
}

/// The state of the connections and of the order of the pool.
#[derive(Default)]
struct State {
    closed: bool,
    /// The pool has no listener: it lost it, or it never had one.
    lost: bool,
    /// The pool never had a listener.
    fresh: bool,
    /// The number of the server. It changes when a pool that connects
    /// again loses its listener. A connection of another number never
    /// serves again.
    generation: u64,
    /// The function that opens a connection to the server of the listener.
    connect: Option<Connect>,
    /// A writable listener that the upgrade opened, for the next connection.
    pending: Option<(Client, Connect)>,
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
    /// The event that waits for the consumer.
    offer: Option<Offer>,
    /// The deliveries that the dispatcher started, and those that ended.
    started: u64,
    ended: u64,
    /// The last tid that the pool delivered.
    last: u64,
    counters: Counters,
    /// How the verification of a cache file ended, until
    /// `Pool::take_file_check` takes it.
    file_check: Option<FileCheck>,
}

/// An event for the consumer.
struct Offer {
    event: Event,
    /// The consumer took it.
    taken: bool,
    /// The tid that `last` moves to when the consumer confirms it.
    tid: Option<u64>,
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

    fn server(&self) -> RwLockReadGuard<'_, Server> {
        self.server.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn server_mut(&self) -> RwLockWriteGuard<'_, Server> {
        self.server.write().unwrap_or_else(PoisonError::into_inner)
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

    /// Wait until the pool closes.
    async fn closed(&self) {
        self.until(|state| state.closed.then_some(())).await;
    }

    /// Wait for `duration`. Return false when the pool closes first.
    async fn pause(&self, duration: Duration) -> bool {
        tokio::select! {
            biased;
            () = self.closed() => false,
            () = self.runtime.sleep(duration) => true,
        }
    }

    /// Run `future` until it ends, or return `None` when the pool closes
    /// first.
    async fn unless_closed<T>(&self, future: impl Future<Output = T>) -> Option<T> {
        tokio::select! {
            biased;
            () = self.closed() => None,
            value = future => Some(value),
        }
    }

    /// Return the listener, or fail with `Error::Disconnected` while the
    /// pool has none.
    fn listener(&self) -> Result<Arc<Client>, Error> {
        self.server().listener.clone().ok_or(Error::Disconnected)
    }

    /// Return the listener with its generation, or `None` when `generation`
    /// is not the generation of now. Fail with `Error::Disconnected` while
    /// the pool has no listener. The mutex of the state keeps the listener
    /// and the generation together: a loss changes both under it.
    fn listener_of(&self, generation: Option<u64>) -> Result<Option<(Arc<Client>, u64)>, Error> {
        let state = self.lock();
        if generation.is_some_and(|generation| generation != state.generation) {
            return Ok(None);
        }
        Ok(Some((self.listener()?, state.generation)))
    }

    /// Return a connection of the pool for a load, in turn: the listener
    /// or a connection that commits. Skip a connection that is lost. When
    /// all are lost, return the listener, so the load fails with
    /// `Error::Disconnected`. Without a listener, fail with it.
    fn any_connection(&self) -> Result<Arc<Client>, Error> {
        let server = self.server();
        let count = server.conns.len() + 1;
        let start = self.next_load.fetch_add(1, Ordering::Relaxed);
        for step in 0..count {
            let conn = match ((start + step) % count).checked_sub(1) {
                Some(i) => Some(&server.conns[i]),
                None => server.listener.as_ref(),
            };
            if let Some(conn) = conn
                && conn.is_connected()
            {
                return Ok(conn.clone());
            }
        }
        server.listener.clone().ok_or(Error::Disconnected)
    }

    /// Return a connection of the pool for a download, in turn: a
    /// connection that commits and is not lost. When there is none, return
    /// the listener. Without a listener, fail with `Error::Disconnected`.
    fn download_connection(&self) -> Result<Arc<Client>, Error> {
        let server = self.server();
        let conns = &server.conns;
        let start = self.next_load.fetch_add(1, Ordering::Relaxed);
        (0..conns.len())
            .map(|step| &conns[(start + step) % conns.len()])
            .find(|conn| conn.is_connected())
            .or(server.listener.as_ref())
            .cloned()
            .ok_or(Error::Disconnected)
    }

    /// Load as `Pool::load_before` does.
    async fn load_before(&self, oid: Oid, tid: Tid) -> Result<(Option<Record>, Tid), Error> {
        let (record, position) = self.any_connection()?.load_before_at(oid, tid).await?;
        if let (Some(cache), Some(record)) = (&self.cache, &record) {
            cache.store(oid, record.tid, record.end, &record.data, position);
        }
        Ok((record, position))
    }

    /// Forget the connection `client`, which closed or which the pool
    /// closes.
    fn forget(&self, client: &Arc<Client>) {
        self.keep_pushed(client);
        self.server_mut()
            .conns
            .retain(|conn| !Arc::ptr_eq(conn, client));
    }

    /// Keep the map of the `info` calls of `client`, which leaves the pool,
    /// when it is newer than the map that the pool keeps.
    fn keep_pushed(&self, client: &Client) {
        let Some((stamp, map)) = client.pushed_info() else {
            return;
        };
        let mut kept = lock(&self.pushed);
        if kept.as_ref().is_none_or(|(old, _)| *old < stamp) {
            *kept = Some((stamp, map));
        }
    }
}

impl Pool {
    /// Start a pool on the open connection `listener`. `connect` opens each
    /// other connection, at most `bound` of them, each for one transaction
    /// at a time. The dispatcher runs as a task of `runtime`.
    ///
    /// The pool starts at the last tid of the listener. It reads the
    /// invalidations of the listener from now on. The pool needs a consumer
    /// of its deliveries: the dispatcher waits for each one. A lost
    /// listener ends the pool: it does not connect again.
    pub fn new(
        runtime: Arc<dyn Runtime>,
        listener: Client,
        connect: Connect,
        bound: usize,
    ) -> Pool {
        Pool::with_cache(runtime, listener, connect, bound, None)
    }

    /// Start a pool as `new` does, with the client cache `cache`. An empty
    /// cache moves its `applied` to the last tid of the listener. A cache
    /// that a file gave with entries serves only after the dispatcher
    /// verified it against the listener (`Pool::ready`).
    pub fn with_cache(
        runtime: Arc<dyn Runtime>,
        listener: Client,
        connect: Connect,
        bound: usize,
        cache: Option<Cache>,
    ) -> Pool {
        Pool::start(runtime, Some((listener, connect)), bound, cache, None)
    }

    /// Start a pool as `with_cache` does, that connects again through
    /// `reconnect` when it loses its listener. Without `first`, the pool
    /// starts disconnected and connects in the background: each call fails
    /// with `Error::Disconnected` until then.
    pub fn with_reconnect(
        runtime: Arc<dyn Runtime>,
        first: Option<(Client, Connect)>,
        bound: usize,
        cache: Option<Cache>,
        reconnect: Reconnect,
    ) -> Pool {
        Pool::start(runtime, first, bound, cache, Some(reconnect))
    }

    fn start(
        runtime: Arc<dyn Runtime>,
        first: Option<(Client, Connect)>,
        bound: usize,
        cache: Option<Cache>,
        reconnect: Option<Reconnect>,
    ) -> Pool {
        let mut state = State::default();
        let mut server = Server::default();
        let mut invalidations = None;
        match first {
            Some((listener, connect)) => {
                // Subscribe first, then read the last tid: the reader raises
                // the last tid before it sends an invalidation.
                invalidations = Some(listener.invalidations());
                state.last = listener.last_transaction().0;
                state.connect = Some(connect);
                server.listener = Some(Arc::new(listener));
            }
            None => {
                state.lost = true;
                state.fresh = true;
            }
        }
        // An empty cache is true at any tid. A cache that waits for its
        // verification keeps its `applied`.
        if let Some(cache) = &cache {
            cache.apply(Tid(state.last), &[]);
        }
        let upgrade = reconnect
            .as_ref()
            .is_some_and(|reconnect| reconnect.upgrade);
        let shared = Arc::new(Shared {
            runtime: runtime.clone(),
            bound: bound.max(1),
            cache: cache.map(Arc::new),
            reconnect,
            server: RwLock::new(server),
            next_load: AtomicUsize::new(0),
            state: Mutex::new(state),
            changed: watch::channel(0).0,
            pushed: Mutex::new(None),
        });
        runtime.spawn(Box::pin(dispatch(shared.clone(), invalidations)));
        if upgrade {
            runtime.spawn(Box::pin(upgrade_forever(shared.clone())));
        }
        Pool { shared }
    }

    /// Return the listener: the reply of its `get_info`, its extensions,
    /// its address and whether it registered read-only. Return `None`
    /// while the pool has no listener.
    pub fn listener(&self) -> Option<Arc<Client>> {
        self.shared.server().listener.clone()
    }

    /// Return the most connections that commit at once.
    pub fn bound(&self) -> usize {
        self.shared.bound
    }

    /// Wait until the cache of the pool serves: a cache that a file gave
    /// serves after its verification. Return at once without a cache, at
    /// the close, and when the cache is disabled, such as by a failed mark
    /// of its file.
    pub async fn ready(&self) {
        let Some(cache) = &self.shared.cache else {
            return;
        };
        self.shared
            .until(|state| (state.closed || cache.verified() || cache.is_disabled()).then_some(()))
            .await;
    }

    /// Return how the verification of a cache that a file gave ended, once.
    /// Return `None` before its end, after the first call, and for a cache
    /// that served at once.
    pub fn take_file_check(&self) -> Option<FileCheck> {
        self.shared.lock().file_check.take()
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

    /// Tell if the pool is open: not closed, and it has a listener that is
    /// not lost.
    pub fn is_connected(&self) -> bool {
        let state = self.shared.lock();
        !state.closed
            && !state.lost
            && self
                .shared
                .server()
                .listener
                .as_ref()
                .is_some_and(|listener| listener.is_connected())
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
        self.shared.listener()?.new_oids(n).await
    }

    /// Load the record of `oid` that was current just before `tid` from
    /// the server, through any connection of the pool. Return the position
    /// of that connection with the record (see `Client::load_before_at`).
    /// It does not read the cache. It stores the record in the cache, if
    /// the pool has one: a current record only when `applied` of the cache
    /// is not above that position.
    pub async fn load_before(&self, oid: Oid, tid: Tid) -> Result<(Option<Record>, Tid), Error> {
        self.shared.load_before(oid, tid).await
    }

    /// Prefetch the records of `oids` before `tid` into the cache, as
    /// `prefetch` of `ZEO.ClientStorage` does. Each oid that the cache does
    /// not hold for `tid` gets one load, and all the loads run at once, as
    /// tasks of the pool. Return the number of loads at once: the tasks
    /// wait for the replies, and store each one as `load_before` does. So a
    /// current record that an invalidation ended while its reply was on its
    /// way does not go in. A load that fails changes nothing. The loads
    /// carry no traceparent.
    ///
    /// Without a cache, it loads nothing. While the pool has no listener,
    /// or after the close, it fails with `Error::Disconnected` and loads
    /// nothing.
    pub fn prefetch(&self, oids: &[Oid], tid: Tid) -> Result<usize, Error> {
        let shared = &self.shared;
        {
            let state = shared.lock();
            if state.closed || state.lost {
                return Err(Error::Disconnected);
            }
        }
        let Some(cache) = &shared.cache else {
            return Ok(0);
        };
        let mut seen = HashSet::new();
        let mut sent = 0;
        for &oid in oids {
            if !seen.insert(oid) || cache.load_before_with(oid, tid, |_, _| Some(())).is_some() {
                continue;
            }
            let shared = shared.clone();
            shared.runtime.clone().spawn(Box::pin(async move {
                let _ = shared.load_before(oid, tid).await;
            }));
            sent += 1;
        }
        Ok(sent)
    }

    /// Call `ping` on the listener, as `sync` of `ZEO.ClientStorage` does
    /// with `server_sync`, then wait until the pool delivered each tid that
    /// the listener read before the reply. So a read after `sync` sees each
    /// commit that the server published before its reply. A lost listener,
    /// a new server or the close ends the wait. A caller that runs a delivery must not
    /// wait: with `wait` false, `sync` returns after the reply.
    pub async fn sync(&self, wait: bool) -> Result<(), Error> {
        let generation = self.shared.lock().generation;
        let listener = self.shared.listener()?;
        listener.ping().await?;
        if !wait {
            return Ok(());
        }
        let position = listener.last_transaction().0;
        self.shared
            .until(|state| {
                let ended = state.closed || state.lost || state.generation != generation;
                (state.last >= position || ended).then_some(())
            })
            .await;
        Ok(())
    }

    /// Call `method` of the server with `args` on the listener, and return
    /// its reply, as `_call` of `ZEO.ClientStorage` does on its one
    /// connection. The state that a call keeps on the server, as an
    /// iterator, stays on the listener, so the calls that follow find it
    /// there. A call of a transaction fails with `Error::Invalid` and sends
    /// nothing: the listener never commits. While the pool has no listener,
    /// it fails with `Error::Disconnected`.
    pub async fn call(&self, method: &str, args: Vec<Value>) -> Result<Value, Error> {
        check_listener_call(method)?;
        self.shared.listener()?.call(method, args).await
    }

    /// Send `method` with `args` on the listener as a call without a reply,
    /// as `_async` of `ZEO.ClientStorage` does. The rules of `call` apply.
    pub async fn notify(&self, method: &str, args: Vec<Value>) -> Result<(), Error> {
        check_listener_call(method)?;
        self.shared.listener()?.notify(method, args).await
    }

    /// Call `method` as `call` does, on the listener of `generation`, and
    /// return the reply with the generation of the listener that gave it.
    /// `None` takes the listener of now. The generation changes at each
    /// loss of the listener of a pool that connects again. The state that
    /// a call keeps on the server, as an iterator id, belongs to the
    /// listener of its generation: the server of the next listener gives
    /// the same ids to other state.
    ///
    /// When the listener of `generation` is gone, the call sends nothing
    /// and fails as the server fails a call on an id that it does not know:
    /// `Error::Server` with `builtins.KeyError` and the first arg.
    pub async fn call_on(
        &self,
        generation: Option<u64>,
        method: &str,
        args: Vec<Value>,
    ) -> Result<(Value, u64), Error> {
        check_listener_call(method)?;
        let Some((listener, current)) = self.shared.listener_of(generation)? else {
            let id = args.into_iter().next().unwrap_or(Value::Nil);
            return Err(Error::Server {
                class: "builtins.KeyError".into(),
                message: id.to_string(),
                value: Value::Array(vec![id]),
            });
        };
        Ok((listener.call(method, args).await?, current))
    }

    /// Send `method` as `notify` does, on the listener of `generation`
    /// (see `call_on`). When the listener of `generation` is gone, send
    /// nothing: its server dropped the state of the connection.
    pub async fn notify_on(
        &self,
        generation: Option<u64>,
        method: &str,
        args: Vec<Value>,
    ) -> Result<(), Error> {
        check_listener_call(method)?;
        match self.shared.listener_of(generation)? {
            Some((listener, _)) => listener.notify(method, args).await,
            None => Ok(()),
        }
    }

    /// Return the map of the reply of `get_info` of the listener, with the
    /// map of the last `info` call that the server sent on a connection of
    /// the pool over it, also on a connection that left the pool since.
    /// Return `None` while the pool has no listener.
    pub fn info(&self) -> Option<Value> {
        let server = self.shared.server();
        let listener = server.listener.as_ref()?;
        // A listener of `Client::connect` has no reply of `get_info`.
        let mut info = match listener.info() {
            Value::Map(entries) => entries.clone(),
            _ => Vec::new(),
        };
        let newest = server
            .conns
            .iter()
            .chain([listener])
            .filter_map(|client| client.pushed_info())
            .chain(lock(&self.shared.pushed).clone())
            .max_by_key(|(stamp, _)| *stamp);
        if let Some((_, Value::Map(pushed))) = newest {
            merge(&mut info, &pushed);
        }
        Some(Value::Map(info))
    }

    /// Load the current record of `oid` from the server, through any
    /// connection, and store it in the cache as `load_before` does. An oid
    /// without a record fails with the `POSKeyError` of the server.
    pub async fn load(&self, oid: Oid) -> Result<(Vec<u8>, Tid), Error> {
        match self.load_before(oid, MAX_TID).await? {
            (Some(record), _) => Ok((record.data, record.tid)),
            (None, _) => Err(protocol::pos_key_error(oid)),
        }
    }

    /// Load the record of `oid` that the transaction `serial` wrote,
    /// through any connection.
    pub async fn load_serial(&self, oid: Oid, serial: Tid) -> Result<Vec<u8>, Error> {
        self.shared.any_connection()?.load_serial(oid, serial).await
    }

    /// Download the blob of `oid` that the transaction `serial` wrote into
    /// `sink`, as `Client::send_blob` does, through a connection other than
    /// the listener when one is open. So the delivery of the invalidations
    /// never waits for the writes of a download. The pieces of the download
    /// come on the connection that carries its `sendBlob`. Return the
    /// number of bytes.
    ///
    /// The caller polls the future until it ends, or drops it: while the
    /// download waits for the caller, its connection waits too.
    pub async fn send_blob<W: Write>(
        &self,
        oid: Oid,
        serial: Tid,
        sink: &mut W,
    ) -> Result<u64, Error> {
        self.shared
            .download_connection()?
            .send_blob(oid, serial, sink)
            .await
    }

    /// Return at most `size` entries of the history of `oid`.
    pub async fn history(&self, oid: Oid, size: usize) -> Result<Vec<HistoryEntry>, Error> {
        self.shared.any_connection()?.history(oid, size).await
    }

    /// Return the next value of the sequence `name`, through the listener.
    pub async fn sequence_next(&self, name: &str) -> Result<u64, Error> {
        self.shared.listener()?.sequence_next(name).await
    }

    /// Begin a transaction on a free connection, as `Client::begin_with`
    /// does. Open a connection when none is free and the pool has less than
    /// its bound. Else wait for a free one. After the close, or while the
    /// pool has no listener, fail with `Error::Disconnected`.
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
                        return Some(Ok((Ok(client), state.generation)));
                    }
                    state.opened = state.opened.saturating_sub(1);
                    dead.push(client);
                }
                if state.opened < shared.bound
                    && let Some(connect) = &state.connect
                {
                    state.opened += 1;
                    return Some(Ok((Err(connect.clone()), state.generation)));
                }
                None
            })
            .await;
        for client in &dead {
            shared.forget(client);
            client.disconnect();
        }
        let (taken, generation) = taken?;
        let client = match taken {
            Ok(client) => client,
            Err(connect) => {
                // The slot goes back to the pool when the connect fails or
                // its future drops.
                let mut slot = Opening {
                    shared: shared.clone(),
                    generation,
                    armed: true,
                };
                let client = Arc::new(connect().await?);
                let state = shared.lock();
                // The pool lost the server while the connection opened.
                if state.closed || state.lost || state.generation != generation {
                    drop(state);
                    client.disconnect();
                    return Err(Error::Disconnected);
                }
                slot.armed = false;
                shared.server_mut().conns.push(client.clone());
                drop(state);
                client
            }
        };
        Ok(Lease {
            shared: shared.clone(),
            client: Some(client),
            generation,
        })
    }

    /// Return the next event for the consumer: a delivery, or a change of
    /// the connection. Wait until the dispatcher gives one. Return `None`
    /// after the close. The consumer confirms each event with `delivered`.
    pub async fn next_event(&self) -> Option<Event> {
        self.shared
            .until(|state| match &mut state.offer {
                Some(offer) if !offer.taken => {
                    offer.taken = true;
                    Some(Some(offer.event.clone()))
                }
                _ if state.closed => Some(None),
                _ => None,
            })
            .await
    }

    /// Return the next delivery for the consumer, as `next_event` does. It
    /// confirms each other event itself. The consumer confirms each
    /// delivery with `delivered`. The consumer of a pool of
    /// `with_reconnect` must call `next_event`: only that call gives
    /// `Event::Connected(Verification::Dropped)`, after which any object
    /// can be stale.
    pub async fn next_delivery(&self) -> Option<Delivery> {
        loop {
            match self.next_event().await? {
                Event::Delivery(delivery) => return Some(delivery),
                _ => self.delivered(),
            }
        }
    }

    /// Confirm the event that `next_event` or `next_delivery` gave: the
    /// consumer applied it. The pool then goes on.
    pub fn delivered(&self) {
        let mut state = self.shared.lock();
        let Some(offer) = state.offer.take_if(|offer| offer.taken) else {
            return;
        };
        if let Some(tid) = offer.tid {
            state.last = state.last.max(tid);
        }
        state.ended += 1;
        drop(state);
        self.shared.bump();
    }

    /// Close the pool and each of its connections, and do not wait for
    /// their tasks. Each call that waits fails with `Error::Disconnected`,
    /// and `next_delivery` gives `None`. The pool stops to connect again.
    pub fn disconnect(&self) {
        let (free, pending) = {
            let mut state = self.shared.lock();
            state.closed = true;
            (std::mem::take(&mut state.free), state.pending.take())
        };
        if let Some(cache) = &self.shared.cache {
            cache.close();
        }
        self.shared.bump();
        {
            let server = self.shared.server();
            if let Some(listener) = &server.listener {
                listener.disconnect();
            }
            for conn in &server.conns {
                conn.disconnect();
            }
        }
        drop(free);
        drop(pending);
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
/// pool, or closes it when it is lost, when the pool is closed, or when the
/// pool lost its server.
struct Lease {
    shared: Arc<Shared>,
    client: Option<Arc<Client>>,
    /// The number of the server of the connection.
    generation: u64,
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
            // The pool already forgot the connections of another server.
            if state.generation != self.generation {
                false
            } else {
                let keep = !state.closed && !state.lost && client.is_connected();
                if keep {
                    state.free.push(client.clone());
                } else {
                    state.opened = state.opened.saturating_sub(1);
                }
                keep
            }
        };
        if !keep {
            self.shared.forget(&client);
            client.disconnect();
        }
        self.shared.bump();
    }
}

/// A slot of `opened` for a connection that opens now. The drop gives the
/// slot back, unless the connection opened or the pool lost its server.
struct Opening {
    shared: Arc<Shared>,
    generation: u64,
    armed: bool,
}

impl Drop for Opening {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        {
            let mut state = self.shared.lock();
            if state.generation == self.generation {
                state.opened = state.opened.saturating_sub(1);
            }
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

    /// Delete `oid`, as `Transaction::delete_object` does. The turn ends
    /// the current entry of `oid` in the cache of the pool.
    pub async fn delete_object(&mut self, oid: Oid, serial: Tid) -> Result<(), Error> {
        self.txn().delete_object(oid, serial).await?;
        self.forget(oid);
        Ok(())
    }

    /// Copy a record of another storage, as `Transaction::restore` does.
    /// The turn ends the current entry of `oid` in the cache of the pool,
    /// and adds nothing.
    pub async fn restore(
        &mut self,
        oid: Oid,
        serial: Tid,
        data: Option<&[u8]>,
        prev_txn: Option<Tid>,
    ) -> Result<(), Error> {
        self.txn().restore(oid, serial, data, prev_txn).await?;
        self.forget(oid);
        Ok(())
    }

    /// Undo the transaction `trans_id`, as `Transaction::undo` does. The
    /// vote lists the oids of the undo as resolved by the server, and the
    /// turn ends their current entries in the cache of the pool.
    pub async fn undo(&mut self, trans_id: &[u8]) -> Result<(), Error> {
        self.txn().undo(trans_id).await
    }

    /// Keep a write of `oid` without data, for the cache of the pool.
    fn forget(&mut self, oid: Oid) {
        if self.lease.shared.cache.is_some() {
            self.writes.push((oid, None));
        }
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
        match &voted {
            Ok(resolved) => self.server_resolved(resolved),
            Err(error) => self.failed(error),
        }
        voted
    }

    /// Vote, as `Transaction::vote_listed` does, with the cache rules of
    /// `vote_resolved`. A listed conflict changes nothing in the cache: the
    /// caller resolves it with `store_resolved`, or calls `unresolved`.
    pub async fn vote_listed(&mut self) -> Result<Voted, Error> {
        let voted = self.txn().vote_listed().await;
        match &voted {
            Ok(voted) => self.server_resolved(&voted.resolved),
            Err(error) => self.failed(error),
        }
        voted
    }

    /// Store `data`, the resolution of a conflict that a vote listed, over
    /// the serial `committed` that the vote gave. The server stores these
    /// bytes unchanged, so the turn adds them to the cache of the pool, as
    /// `ZEO.ClientStorage` does.
    pub async fn store_resolved(
        &mut self,
        oid: Oid,
        committed: Tid,
        data: &[u8],
    ) -> Result<(), Error> {
        self.txn().store(oid, committed, data).await?;
        self.keep(oid, data);
        Ok(())
    }

    /// Drop the current entry of `oid` from the cache: the client did not
    /// resolve the conflict that a vote listed for it, as `tpc_vote` of
    /// `ZEO.ClientStorage` does. The transaction stays open for its abort.
    pub fn unresolved(&self, oid: Oid) {
        if let Some(cache) = &self.lease.shared.cache {
            cache.drop_current(oid);
        }
    }

    /// Keep no pickle of the oids that the server resolved for the cache.
    /// An oid that the transaction did not store, as an oid of an undo,
    /// becomes a write without data, as `TransactionBuffer` of ZEO gives
    /// it: the turn ends its current entry.
    fn server_resolved(&mut self, resolved: &[Oid]) {
        if resolved.is_empty() || self.lease.shared.cache.is_none() {
            return;
        }
        let mut left: HashSet<Oid> = resolved.iter().copied().collect();
        for (oid, data) in &mut self.writes {
            if resolved.contains(oid) {
                *data = None;
                left.remove(oid);
            }
        }
        // The order of the reply, for a fixed order in the cache.
        for oid in resolved {
            if left.remove(oid) {
                self.writes.push((*oid, None));
            }
        }
    }

    /// Drop the current entry of the oid of a conflict of a failed vote.
    fn failed(&self, error: &Error) {
        if let Error::Conflict { oid, .. } | Error::ReadConflict { oid, .. } = error {
            self.unresolved(*oid);
        }
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
    /// close, after a lost listener, or on a connection of a server that
    /// the pool lost, it fails with `Error::Disconnected` and sends
    /// nothing. A finish that waited for its reply when the listener was
    /// lost fails with `Error::Disconnected` too: its outcome counts as
    /// unknown.
    pub async fn finish(mut self) -> Result<Finished, Error> {
        let shared = self.lease.shared.clone();
        let seq = {
            let mut state = shared.lock();
            if state.closed || state.lost || state.generation != self.lease.generation {
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
    /// the data of each store that the server did not resolve. A turn after
    /// a lost listener only drops the current entry of each oid that it
    /// wrote: the cache did not get the commits before it.
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
        if let Some(cache) = &self.shared.cache {
            if stale {
                for (oid, _) in &self.writes {
                    cache.drop_current(*oid);
                }
            } else {
                cache.commit(tid, &last_writes(&self.writes));
            }
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

/// The calls of a transaction, which the listener never sends.
const TRANSACTION_CALLS: [&str; 15] = [
    "register",
    "tpc_begin",
    "storea",
    "restorea",
    "deleteObject",
    "checkCurrentSerialInTransaction",
    "undoa",
    "vote",
    "tpc_finish",
    "tpc_abort",
    "storeBlobStart",
    "storeBlobChunk",
    "storeBlobEnd",
    "storeBlobShared",
    "sendBlob",
];

/// Fail with `Error::Invalid` for a call that the listener never sends: a
/// call of a transaction, `register`, or `sendBlob`, whose pieces only
/// `Pool::send_blob` reads.
fn check_listener_call(method: &str) -> Result<(), Error> {
    if TRANSACTION_CALLS.contains(&method) {
        return Err(Error::Invalid(format!(
            "{method} does not go through the listener"
        )));
    }
    Ok(())
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

/// The dispatcher: deliver each tid of the listener in turn. When the
/// listener is lost, give the stale mark, then connect again if the pool
/// does. Stop at the close.
async fn dispatch(shared: Arc<Shared>, mut stream: Option<Invalidations>) {
    // A cache that a file gave serves only after its verification against
    // the first listener, at the last tid that the start read.
    if let Some(cache) = &shared.cache
        && stream.is_some()
        && !cache.verified()
    {
        let Ok(listener) = shared.listener() else {
            return;
        };
        let server_tid = Tid(shared.lock().last);
        if !verify_file(&shared, cache, &listener, server_tid).await {
            return;
        }
        shared.bump();
    }
    loop {
        if let Some(mut invalidations) = stream.take() {
            let Ok(listener) = shared.listener() else {
                return;
            };
            if !serve(&shared, &listener, &mut invalidations).await {
                return;
            }
            if shared.lock().closed {
                return;
            }
            lose(&shared, &listener).await;
            if shared.lock().closed {
                return;
            }
        }
        if shared.reconnect.is_none() {
            return;
        }
        match reconnect(&shared).await {
            Some(invalidations) => stream = Some(invalidations),
            None => return,
        }
    }
}

/// Deliver each tid of `listener` until the loss of the listener: then
/// return true. Return false after the close.
async fn serve(shared: &Shared, listener: &Client, invalidations: &mut Invalidations) -> bool {
    loop {
        match invalidations.recv().await {
            Ok(invalidation) => match deliver(shared, listener, invalidation).await {
                Step::Next => {}
                Step::Closed => return false,
                Step::Lost => return true,
            },
            Err(_) => return true,
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

/// Deliver the tid of `invalidation`, which came on `listener`.
async fn deliver(shared: &Shared, listener: &Client, invalidation: Invalidation) -> Step {
    let tid = invalidation.tid.0;
    let horizon = {
        let mut state = shared.lock();
        if state.closed {
            return Step::Closed;
        }
        // A tid at or before the last tid of the pool came before the open,
        // or before the verification of a new listener.
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
        () = listener.closed() => {
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
    let tid = invalidation.tid.0;
    let event = Event::Delivery(Delivery::Invalidate(invalidation));
    put(shared, event, Some(tid));
    wait_for_consumer(shared).await
}

/// Offer `event` to the consumer. Its confirmation moves `last` to `tid`.
/// The caller counts the delivery in `started`.
fn put(shared: &Shared, event: Event, tid: Option<u64>) {
    shared.lock().offer = Some(Offer {
        event,
        taken: false,
        tid,
    });
    shared.bump();
}

/// Count a delivery, offer `event` to the consumer, and wait until the
/// consumer confirmed it. Return false after the close.
async fn announce(shared: &Shared, event: Event, tid: Option<u64>) -> bool {
    shared.lock().started += 1;
    put(shared, event, tid);
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
///
/// A pool that connects again also closes each connection of the server
/// and forgets them: a transaction on one of them fails at its next call.
async fn lose(shared: &Shared, listener: &Client) {
    let reconnects = shared.reconnect.is_some();
    // Without a new connection, the cache no longer gets the commits in
    // order: an empty cache is true at any tid. With one, the verification
    // keeps what stays true.
    if let Some(cache) = &shared.cache
        && !reconnects
    {
        cache.disable();
    }
    let (free, conns) = {
        let mut state = shared.lock();
        state.lost = true;
        let in_flight = std::mem::take(&mut state.in_flight);
        state.doomed.extend(in_flight);
        state.offer = Some(Offer {
            event: Event::Delivery(Delivery::Stale),
            taken: false,
            tid: None,
        });
        state.started += 1;
        if reconnects {
            state.generation += 1;
            state.opened = 0;
            state.connect = None;
            let free = std::mem::take(&mut state.free);
            let mut server = shared.server_mut();
            server.listener = None;
            (free, std::mem::take(&mut server.conns))
        } else {
            (Vec::new(), Vec::new())
        }
    };
    shared.bump();
    listener.disconnect();
    for conn in conns.iter().chain(&free) {
        conn.disconnect();
    }
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

/// The outcome of the verification of a new listener.
enum Verified {
    /// The pool takes the server. The confirmation of `Event::Connected`
    /// moves `last` to the tid.
    Taken(Verification, Option<u64>),
    /// The pool refuses the server: its last tid is below the pool's.
    Refused(String),
    /// The verification failed: the connection is lost, or the server
    /// failed `getInvalidations`.
    Failed,
    /// The pool closed.
    Closed,
}

/// Connect again: open a listener at once, then every `poll`, until one
/// passes the verification. Then take its server, and return its stream of
/// invalidations. Return `None` after the close.
async fn reconnect(shared: &Shared) -> Option<Invalidations> {
    let settings = shared.reconnect.clone()?;
    let mut delay = Duration::ZERO;
    loop {
        if !delay.is_zero() && !shared.pause(delay).await {
            return None;
        }
        delay = settings.poll;
        // A pending listener that the server closed since is of no use.
        let pending = shared.lock().pending.take();
        let pending = pending.filter(|(client, _)| client.is_connected());
        let opened = match pending {
            Some(opened) => Ok(opened),
            None => shared.unless_closed((settings.open)(false)).await?,
        };
        let Ok((client, connect)) = opened else {
            continue;
        };
        // Subscribe first, then read the last tid: the reader raises the
        // last tid before it sends an invalidation.
        let invalidations = client.invalidations();
        match verify(shared, &client).await {
            Verified::Taken(verification, tid) => {
                take(shared, client, connect);
                if !announce(shared, Event::Connected(verification), tid).await {
                    return None;
                }
                return Some(invalidations);
            }
            Verified::Refused(text) => {
                client.disconnect();
                delay = settings.refused_poll;
                if !announce(shared, Event::Refused(text), None).await {
                    return None;
                }
            }
            Verified::Failed => client.disconnect(),
            Verified::Closed => return None,
        }
    }
}

/// Verify the cache and the last tid of the pool against the new listener
/// `client`, as `verify` of `ZEO/asyncio/client.py` does.
///
/// The base is `applied` of the cache, or the last tid of the pool without
/// a cache. A server whose last tid is the base keeps the cache. A last tid
/// below the base, or below the last tid of the pool, refuses the server.
/// A last tid above the base asks `getInvalidations(base)`: the cache drops
/// the current entry of each oid that it gives, and the consumer gets them
/// as one invalidation at the tid of the reply. Its confirmation never
/// moves the last tid of the pool back. When the server answers `None`, the
/// cache drops every entry. A pool that never had a listener takes the
/// server as it is.
async fn verify(shared: &Shared, client: &Client) -> Verified {
    let server_tid = client.last_transaction();
    let (last, fresh) = {
        let mut state = shared.lock();
        let fresh = std::mem::take(&mut state.fresh);
        if fresh {
            state.last = state.last.max(server_tid.0);
        }
        (state.last, fresh)
    };
    if fresh {
        if let Some(cache) = &shared.cache {
            if cache.verified() {
                cache.verify(server_tid, Some(&[]));
            } else if !verify_file(shared, cache, client, server_tid).await {
                return Verified::Closed;
            }
        }
        return Verified::Taken(Verification::Kept, None);
    }
    let base = shared
        .cache
        .as_ref()
        .map_or(last, |cache| cache.applied().0);
    if base.max(last) > server_tid.0 {
        return Verified::Refused(format!(
            "the last transaction {:016x} of the server at {} is before the last \
             transaction {last:016x} of the client or {base:016x} of its cache",
            server_tid.0,
            client.address(),
        ));
    }
    if base == server_tid.0 {
        return Verified::Taken(Verification::Kept, None);
    }
    let reply = match shared
        .unless_closed(client.get_invalidations(Tid(base)))
        .await
    {
        None => return Verified::Closed,
        Some(Err(_)) => return Verified::Failed,
        Some(Ok(reply)) => reply,
    };
    match reply {
        Some(invalidation) => {
            if let Some(cache) = &shared.cache {
                cache.verify(invalidation.tid, Some(&invalidation.oids));
            }
            // The consumer always gets the oids of the reply, as ZEO calls
            // `invalidateTransaction` after `getInvalidations`. The tid can
            // be the last tid of the pool: a turn after the loss moved the
            // last tid past the base, and the commits of other clients
            // between the base and that tid never reached the consumer.
            // Then the confirmation keeps the last tid.
            let tid = invalidation.tid.0;
            let event = Event::Delivery(Delivery::Invalidate(invalidation));
            if !announce(shared, event, Some(tid)).await {
                return Verified::Closed;
            }
            Verified::Taken(Verification::Quick, None)
        }
        None => {
            if let Some(cache) = &shared.cache {
                if !before_drop(shared, String::new()).await {
                    return Verified::Closed;
                }
                cache.verify(server_tid, None);
            }
            Verified::Taken(Verification::Dropped, Some(server_tid.0))
        }
    }
}

/// Verify a cache that a file gave against `client`, whose last tid is
/// `server_tid`, before the cache serves. The pool delivers nothing for it:
/// the consumer starts at the last tid of the pool.
///
/// The base is `applied` of the cache. A server tid equal to the base keeps
/// the cache. With a higher one, the server must hold a commit at the base,
/// else its history is another one and the cache drops every entry. Then
/// `getInvalidations(base)`: a reply drops the current entry of each of its
/// oids, and `None` or an error drops every entry. A server tid below the base drops every entry too, and `applied`
/// becomes the server tid. Then the cache serves. Return false at the
/// close.
async fn verify_file(shared: &Shared, cache: &Cache, client: &Client, server_tid: Tid) -> bool {
    let base = cache.applied();
    let check = if base == server_tid {
        cache.verify(server_tid, Some(&[]));
        FileCheck::Kept
    } else if base > server_tid {
        cache.verify(server_tid, None);
        FileCheck::Ahead {
            applied: base,
            server: server_tid,
        }
    } else {
        let held = if base == Tid(0) {
            Ok(true)
        } else {
            match shared.unless_closed(client.holds_transaction(base)).await {
                None => return false,
                Some(held) => held,
            }
        };
        match held {
            Ok(true) => {}
            Ok(false) => {
                cache.verify(server_tid, None);
                shared.lock().file_check = Some(FileCheck::Other { applied: base });
                return true;
            }
            Err(error) => {
                cache.verify(server_tid, None);
                shared.lock().file_check = Some(FileCheck::Failed(error.to_string()));
                return true;
            }
        }
        match shared.unless_closed(client.get_invalidations(base)).await {
            None => return false,
            Some(Ok(Some(invalidation))) => {
                cache.verify(invalidation.tid, Some(&invalidation.oids));
                FileCheck::Quick
            }
            Some(Ok(None)) => {
                if !before_drop(shared, String::new()).await {
                    return false;
                }
                cache.verify(server_tid, None);
                FileCheck::Dropped
            }
            Some(Err(error)) => {
                if !before_drop(shared, error.to_string()).await {
                    return false;
                }
                cache.verify(server_tid, None);
                FileCheck::Failed(error.to_string())
            }
        }
    };
    shared.lock().file_check = Some(check);
    true
}

/// Call the `dropping` function of the reconnect, if any, with `reason`,
/// and wait for it. Return false at the close.
async fn before_drop(shared: &Shared, reason: String) -> bool {
    let Some(dropping) = shared
        .reconnect
        .as_ref()
        .and_then(|reconnect| reconnect.dropping.clone())
    else {
        return true;
    };
    shared.unless_closed(dropping(reason)).await.is_some()
}

/// Take the server of the new listener `client`: the pool is connected.
fn take(shared: &Shared, client: Client, connect: Connect) {
    {
        let mut state = shared.lock();
        if state.closed {
            drop(state);
            client.disconnect();
            return;
        }
        state.lost = false;
        state.connect = Some(connect);
        // The `get_info` of the new listener is newer than any size of the
        // server that the pool lost.
        *lock(&shared.pushed) = None;
        shared.server_mut().listener = Some(Arc::new(client));
    }
    shared.bump();
}

/// While the listener is read-only, try a writable server every `poll`.
/// When one opens, keep it for the next connection, and close the
/// listener: the dispatcher then connects again, on that server. Stop at
/// the close.
async fn upgrade_forever(shared: Arc<Shared>) {
    let Some(settings) = shared.reconnect.clone() else {
        return;
    };
    loop {
        if !shared.pause(settings.poll).await {
            return;
        }
        let ready = {
            let state = shared.lock();
            !state.lost && state.pending.is_none()
        };
        let listener = shared.server().listener.clone();
        let Some(listener) = listener.filter(|listener| ready && listener.read_only()) else {
            continue;
        };
        let Some(opened) = shared.unless_closed((settings.open)(true)).await else {
            return;
        };
        if let Ok((client, connect)) = opened
            && !client.read_only()
        {
            let kept = {
                let mut state = shared.lock();
                if state.closed {
                    return;
                }
                // The listener can change during the open: the pool lost it,
                // and connected again. Then the writable listener comes too
                // late, and the next attempt opens a new one.
                let same = !state.lost
                    && shared
                        .server()
                        .listener
                        .as_ref()
                        .is_some_and(|current| Arc::ptr_eq(current, &listener));
                if same {
                    state.pending = Some((client, connect));
                    None
                } else {
                    Some(client)
                }
            };
            match kept {
                None => listener.disconnect(),
                Some(client) => client.disconnect(),
            }
        }
    }
}
