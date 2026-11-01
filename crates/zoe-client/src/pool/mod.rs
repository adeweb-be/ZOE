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
//!
//! A pool with a cache can also prefetch the references of each record
//! that a load fetches from the server (`set_prefetch_references`).
//!
//! A load of an oid at a tid that a load in flight already asks for waits
//! for the reply of that load, and sends nothing.
//!
//! `transaction` holds the transactions of the pool, their finishes and
//! their turns, and the early write-intent locks of an owner. `dispatch`
//! holds the dispatcher, `reconnect` the reconnect and its verification,
//! `references` the prefetch of the references of a record, and `join`
//! the loads in flight.

mod dispatch;
mod join;
mod reconnect;
mod references;
mod transaction;

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::Duration;

use rmpv::Value;
use tokio::sync::watch;
use zoe_core::{Flavour, Oid, Tid};

use crate::cache::Cache;
use crate::client::{Client, HistoryEntry, Invalidation, Record, merge};
use crate::protocol;
use crate::runtime::{BoxFuture, Runtime};
use crate::{Error, lock};
use dispatch::dispatch;
use join::Flights;
use reconnect::upgrade_forever;
pub use references::{PREFETCH_REFERENCES, PREFETCH_SCAN_LIMIT};
pub use transaction::{Finished, PoolIntents, PoolTransaction, Turn};

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
    /// A miss of `Pool::load_before` prefetches the references of its
    /// record.
    references: AtomicBool,
    /// The connections of the server. A load picks one of them. The lock is
    /// held only to pick or to change one. A writer takes `state` first.
    server: RwLock<Server>,
    next_load: AtomicUsize,
    /// The loads in flight, by oid and tid. The lock is held only to add
    /// or remove a load or a waiter.
    flights: Mutex<Flights>,
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
#[derive(Clone, Debug, Default)]
struct Slot {
    /// The dispatcher gave the finish its turn.
    turn: bool,
    /// The turn came after the listener was lost.
    stale: bool,
    /// The finish needs the oids of the commit: a vote listed an oid as
    /// resolved.
    wants_oids: bool,
    /// The oids of the commit, from its invalidation on the listener, when
    /// the dispatcher gave the turn and the finish needs them.
    oids: Option<Vec<Oid>>,
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

    /// Prefetch as `Pool::prefetch` does, with at most `limit` loads: the
    /// first oids of `oids` that the cache does not hold for `tid`.
    fn prefetch(self: &Arc<Self>, oids: &[Oid], tid: Tid, limit: usize) -> Result<usize, Error> {
        {
            let state = self.lock();
            if state.closed || state.lost {
                return Err(Error::Disconnected);
            }
        }
        let Some(cache) = &self.cache else {
            return Ok(0);
        };
        let mut seen = HashSet::new();
        let mut sent = 0;
        for &oid in oids {
            if sent == limit {
                break;
            }
            if !seen.insert(oid) || cache.load_before_with(oid, tid, |_, _| Some(())).is_some() {
                continue;
            }
            let shared = self.clone();
            self.runtime.spawn(Box::pin(async move {
                let _ = shared.load_before(oid, tid).await;
            }));
            sent += 1;
        }
        Ok(sent)
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
            references: AtomicBool::new(false),
            server: RwLock::new(server),
            next_load: AtomicUsize::new(0),
            flights: Mutex::new(Flights::default()),
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

    /// Turn on or off the prefetch of the references of a record that
    /// `load_before` fetches from the server. It is off when the pool
    /// starts, and does nothing without a cache. With it off, a load makes
    /// no other call.
    pub fn set_prefetch_references(&self, on: bool) {
        self.shared.references.store(on, Ordering::Relaxed);
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
    ///
    /// With `set_prefetch_references(true)` and a cache, a task then reads
    /// the persistent ids of the record, and prefetches as `prefetch` does
    /// the oids of the same database that the cache does not hold for
    /// `tid`: at most `PREFETCH_REFERENCES` of them, in the order of the
    /// record. The record returns without a wait for the task. A record of
    /// more than `PREFETCH_SCAN_LIMIT` bytes, or that does not decode,
    /// prefetches nothing, and neither do weak references and references
    /// into another database. The loads of the prefetch do not prefetch
    /// the references of their records. A prefetch that fails changes
    /// nothing.
    ///
    /// When a load of `oid` at `tid` is in flight, a load of the pool or of
    /// a prefetch, the call waits for its reply and sends nothing: it gets
    /// a copy of the record or of the error. A load at another tid sends
    /// its own request. A caller that drops its future does not stop the
    /// load for the other waiters. Once the reply is in the cache and the
    /// waiters have it, the next call sends a new request.
    pub async fn load_before(&self, oid: Oid, tid: Tid) -> Result<(Option<Record>, Tid), Error> {
        let loaded = self.shared.load_before(oid, tid).await?;
        if let Some(record) = &loaded.0
            && self.shared.references.load(Ordering::Relaxed)
        {
            references::prefetch_references(&self.shared, oid, tid, &record.data);
        }
        Ok(loaded)
    }

    /// Prefetch the records of `oids` before `tid` into the cache, as
    /// `prefetch` of `ZEO.ClientStorage` does. Each oid that the cache does
    /// not hold for `tid` gets one load, and all the loads run at once, as
    /// tasks of the pool. A load of an oid in flight at `tid` joins it, as
    /// in `load_before`. Return the number of loads at once: the tasks
    /// wait for the replies, and store each one as `load_before` does. So a
    /// current record that an invalidation ended while its reply was on its
    /// way does not go in. A load that fails changes nothing. The loads
    /// carry no traceparent.
    ///
    /// Without a cache, it loads nothing. While the pool has no listener,
    /// or after the close, it fails with `Error::Disconnected` and loads
    /// nothing.
    pub fn prefetch(&self, oids: &[Oid], tid: Tid) -> Result<usize, Error> {
        self.shared.prefetch(oids, tid, usize::MAX)
    }

    /// Call `ping` on the listener, as `sync` of `ZEO.ClientStorage` does
    /// with `server_sync`, then wait until the pool delivered each tid that
    /// the listener read before the reply. So a read after `sync` sees each
    /// commit that the server published before its reply. A lost listener,
    /// a new server or the close ends the wait. A caller that runs a
    /// delivery must not wait: with `wait` false, `sync` returns after the
    /// reply.
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
                class: zoe_wire::class::KEY_ERROR.into(),
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
        match self.load_before(oid, Tid(zoe_wire::MAX_TID)).await? {
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

    /// Return the value of a key of a namespace, as `Client::kv_get` does,
    /// on the listener.
    pub async fn kv_get(
        &self,
        flavour: Flavour,
        name: &str,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, Error> {
        self.shared.listener()?.kv_get(flavour, name, key).await
    }

    /// Return the pairs of a namespace, as `Client::kv_scan` does, on the
    /// listener.
    pub async fn kv_scan(
        &self,
        flavour: Flavour,
        name: &str,
        start: Option<&[u8]>,
        stop: Option<&[u8]>,
        limit: u64,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, Error> {
        let listener = self.shared.listener()?;
        listener.kv_scan(flavour, name, start, stop, limit).await
    }

    /// Return the generation of a durable namespace, as
    /// `Client::kv_generation` does, on the listener.
    pub async fn kv_generation(&self, name: &str) -> Result<Tid, Error> {
        self.shared.listener()?.kv_generation(name).await
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
        Ok(PoolTransaction::new(txn, lease))
    }

    /// Lease a connection for the early write-intent locks of one owner,
    /// as `begin_with` leases one for a transaction.
    pub async fn intents(&self) -> Result<PoolIntents, Error> {
        Ok(PoolIntents::new(self.lease().await?))
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
