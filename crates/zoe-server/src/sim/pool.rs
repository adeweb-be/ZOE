//! The clients of a run with pools (`Simulation::pool`): `zoe_client::Pool`
//! on the runtime of the simulation.
//!
//! Each client opens a pool: a listener and at most `size` connections
//! that commit. It runs `size` workers at once, which share its
//! transactions, and one consumer of the deliveries of the pool. The
//! workers commit as the clients of `client.rs` do, through the pool. The
//! callback of each finish records the tid of the commit when its turn
//! comes. The consumer records each tid of another client and the stale
//! mark. So the log of a pool holds what it delivered, in order.
//!
//! With `Faults::listener_losses`, a worker whose finish has the reply of
//! the server breaks the listener of its pool, at most that many times in
//! a run. A client whose pool lost its listener opens a new pool for the
//! transactions that are left.
//!
//! The checks of the log that need the store come at the end of the run
//! (`check_logs`).

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::sync::watch;
use zoe_client::{Client, Connect, Delivery, Error, Finished, Pool, PoolTransaction};
use zoe_core::{Oid, Tid};

use super::client::record;
use super::common::{self, ADDR, TRIES};
use super::net;
use super::rng::Rng;
use super::server::SimRuntime;
use super::world::{Acked, Owner, Shared, phase, sleep};
use crate::protocol::STORAGE_NAME;

/// The chance that a finish with its reply breaks the listener, while the
/// run has listener losses left.
const LOSS_CHANCE: f64 = 0.25;

/// The longest time that a pool waits at its end for the tids that the
/// store committed.
const CATCH_UP: Duration = Duration::from_secs(10);

/// How a pool delivered a tid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// To the consumer, as the commit of another client.
    Foreign,
    /// Through the callback of the finish of the pool.
    Own,
}

/// What one pool delivered, in order.
#[derive(Debug, Default)]
pub(crate) struct PoolLog {
    pub(crate) client: usize,
    /// The last tid of the pool at its open.
    pub(crate) start: u64,
    /// Each tid that the pool delivered, in order.
    pub(crate) entries: Vec<(u64, Kind)>,
    /// The number of entries when the consumer got the stale mark.
    pub(crate) stale: Option<usize>,
    /// The tids that a finish of the pool returned.
    pub(crate) returned: Vec<u64>,
    /// The pool lost its listener.
    pub(crate) lost: bool,
    /// The last tid of the store when the workers of the pool ended, and
    /// whether the pool delivered it.
    pub(crate) target: Option<(u64, bool)>,
    /// The breaks of the rule that the run saw.
    pub(crate) faults: Vec<String>,
}

impl PoolLog {
    /// Return the last tid that the pool delivered.
    fn last(&self) -> u64 {
        self.entries.last().map_or(self.start, |(tid, _)| *tid)
    }

    /// Record the delivery of `tid`.
    fn deliver(&mut self, tid: u64, kind: Kind) {
        let last = self.last();
        if tid <= last {
            self.faults.push(format!(
                "c{} delivers tid {tid:016x} after {last:016x}",
                self.client
            ));
        }
        self.entries.push((tid, kind));
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One client of a run with pools.
pub(crate) struct SimPoolClient {
    pub(crate) shared: Arc<Shared>,
    pub(crate) id: usize,
    /// The transactions that the client runs.
    pub(crate) transactions: usize,
    pub(crate) rng: Rng,
    /// The most connections of the pool that commit, and its workers.
    pub(crate) size: usize,
}

/// The state that the workers of one pool share.
struct Run {
    shared: Arc<Shared>,
    id: usize,
    pool: Pool,
    /// The conn of the listener in the network of the simulation.
    listener: u64,
    log: Mutex<PoolLog>,
    /// The transactions that are left to the client.
    left: Arc<Mutex<usize>>,
}

/// The end of one try of a transaction of a pool.
type Outcome = common::Outcome<()>;

impl SimPoolClient {
    /// Run the transactions of the client on pools, then close the last
    /// pool.
    pub(crate) async fn run(mut self) {
        let runtime = Arc::new(SimRuntime::client(&self.shared, self.id));
        let left = Arc::new(Mutex::new(self.transactions));
        while *lock(&left) > 0 {
            let Some((pool, listener)) = self.open(&runtime).await else {
                break;
            };
            let start = pool.last_transaction().0;
            let run = Arc::new(Run {
                shared: Arc::clone(&self.shared),
                id: self.id,
                pool,
                listener,
                log: Mutex::new(PoolLog {
                    client: self.id,
                    start,
                    ..PoolLog::default()
                }),
                left: Arc::clone(&left),
            });
            self.run_pool(&run).await;
        }
        let mut state = self.shared.lock();
        state.trace(format!("c{} done", self.id));
        state.clients_done += 1;
    }

    fn trace(&self, text: &str) {
        self.shared.lock().trace(format!("c{} {text}", self.id));
    }

    /// Open a pool: connect its listener, and find its conn. Try again
    /// while the server is down. Return `None` when the store did not open
    /// again.
    async fn open(&mut self, runtime: &Arc<SimRuntime>) -> Option<(Pool, u64)> {
        loop {
            if self.shared.lock().server.halted {
                return None;
            }
            let dyn_runtime: Arc<dyn zoe_client::Runtime> = runtime.clone();
            match Client::connect_with(dyn_runtime.clone(), ADDR, STORAGE_NAME).await {
                Ok(listener) => {
                    // The connect made the newest conn of this client.
                    let conn = {
                        let state = self.shared.lock();
                        let mine = state.conns.iter().filter(|(_, c)| c.client == self.id);
                        mine.map(|(id, _)| *id).max().unwrap_or(0)
                    };
                    let connect_runtime = dyn_runtime.clone();
                    let connect: Connect = Arc::new(move || {
                        let runtime = connect_runtime.clone();
                        Box::pin(
                            async move { Client::connect_with(runtime, ADDR, STORAGE_NAME).await },
                        )
                    });
                    let pool = Pool::new(dyn_runtime, listener, connect, self.size);
                    self.trace(&format!(
                        "opens a pool of {} at {:016x}, listener connection {conn}",
                        self.size,
                        pool.last_transaction().0
                    ));
                    return Some((pool, conn));
                }
                Err(error) => self.trace(&format!("does not connect: {error}")),
            }
            let backoff = self
                .rng
                .between(Duration::from_millis(1), Duration::from_millis(50));
            sleep(&self.shared, backoff).await;
        }
    }

    /// Run the workers and the consumer of one pool until the transactions
    /// end or the pool loses its listener. Then wait for the tids of the
    /// store, close the pool and keep its log.
    async fn run_pool(&mut self, run: &Arc<Run>) {
        let (ended, mut ends) = watch::channel(0usize);
        let ended = Arc::new(ended);
        {
            let mut state = self.shared.lock();
            let consumer = consume(Arc::clone(run), Arc::clone(&ended));
            state.spawn(
                &self.shared.ready,
                Owner::Client(self.id),
                Box::pin(consumer),
            );
            for _ in 0..self.size {
                let worker = work(Arc::clone(run), state.rng.fork(), Arc::clone(&ended));
                state.spawn(&self.shared.ready, Owner::Client(self.id), Box::pin(worker));
            }
        }
        let workers = self.size;
        let _ = ends.wait_for(|n| *n >= workers).await;
        let lost = !run.pool.is_connected();
        if !lost {
            run.catch_up().await;
        }
        let counters = run.pool.counters();
        run.pool.disconnect();
        let _ = ends.wait_for(|n| *n > workers).await;
        let log = std::mem::take(&mut *lock(&run.log));
        let mut state = self.shared.lock();
        state.pools.held += counters.held;
        state.pools.waited += counters.waited;
        state.pools.logs.push(log);
    }
}

impl Run {
    fn trace(&self, text: &str) {
        self.shared.lock().trace(format!("c{} {text}", self.id));
    }

    /// Check that the last transaction of the pool is not above the last
    /// tid that it delivered.
    fn check_last(&self) {
        let last = self.pool.last_transaction().0;
        let mut log = lock(&self.log);
        if last > log.last() {
            let delivered = log.last();
            let client = log.client;
            log.faults.push(format!(
                "c{client} gives the last transaction {last:016x} above the delivered {delivered:016x}"
            ));
        }
    }

    /// Wait until the pool delivered the last tid of the store, at most
    /// `CATCH_UP`. Record the target.
    async fn catch_up(&self) {
        let target = {
            let state = self.shared.lock();
            state
                .server
                .store
                .as_ref()
                .map(|store| store.last_transaction().0)
        };
        let Some(target) = target else { return };
        let mut waited = Duration::ZERO;
        let step = Duration::from_millis(5);
        loop {
            let reached = lock(&self.log).last() >= target;
            if reached || waited >= CATCH_UP || !self.pool.is_connected() {
                lock(&self.log).target = Some((target, reached || !self.pool.is_connected()));
                return;
            }
            sleep(&self.shared, step).await;
            waited += step;
        }
    }

    /// Break the listener of the pool in the network, once per run budget.
    fn maybe_lose_listener(&self, rng: &mut Rng) -> bool {
        let wakers = {
            let mut state = self.shared.lock();
            if state.listener_losses == 0 || !rng.chance(LOSS_CHANCE) {
                return false;
            }
            if !state.conns.get(&self.listener).is_some_and(net::Conn::open) {
                return false;
            }
            state.listener_losses -= 1;
            state.trace(format!(
                "net breaks the listener connection {} of c{}",
                self.listener, self.id
            ));
            net::reset(&mut state, self.listener)
        };
        for waker in wakers {
            waker.wake();
        }
        true
    }

    /// Take a transaction of the client, or `None` when none is left.
    fn take(&self) -> Option<()> {
        let mut left = lock(&self.left);
        let some = *left > 0;
        if some {
            *left -= 1;
        }
        some.then_some(())
    }

    /// Give a transaction back to the client.
    fn give_back(&self) {
        *lock(&self.left) += 1;
    }

    /// Load the current serial of `oid`, and count a stale read, as the
    /// clients of `client.rs` do.
    async fn serial(&self, oid: u64) -> Result<Tid, Outcome> {
        let loaded = common::load(&self.shared, self.id, oid, self.pool.load(Oid(oid))).await;
        loaded.map(|(serial, _)| serial)
    }

    /// Try one transaction on the pool.
    async fn commit(&self, rng: &mut Rng, t: usize, attempt: usize) -> Outcome {
        let oids = common::oids(rng);
        let mut records = Vec::new();
        for &oid in &oids {
            let serial = match self.serial(oid).await {
                Ok(serial) => serial,
                Err(outcome) => return outcome,
            };
            let value = format!("c{} p{t} a{attempt} oid {oid}", self.id);
            records.push((oid, serial, record(value.as_bytes())));
        }
        let lost = |error: Error| match error {
            Error::Disconnected => Outcome::Lost { in_finish: false },
            _ => Outcome::Retry,
        };
        let user = format!("c{}", self.id);
        let mut txn: PoolTransaction = match self
            .pool
            .begin_with(user.as_bytes(), b"", b"", None, " ")
            .await
        {
            Ok(txn) => txn,
            Err(error) => return lost(error),
        };
        for (oid, serial, data) in &records {
            if let Err(error) = txn.store(Oid(*oid), *serial, data).await {
                return lost(error);
            }
        }
        if let Err(error) = txn.vote().await {
            if let Error::Conflict { oid, .. } = &error {
                self.trace(&format!("conflicts on oid {}", oid.0));
            }
            return lost(error);
        }
        let finished = match txn.finish().await {
            Ok(finished) => finished,
            Err(Error::Disconnected) => return Outcome::Lost { in_finish: true },
            Err(_) => return Outcome::Retry,
        };
        let broke = self.maybe_lose_listener(rng);
        self.turn(finished, broke, records).await
    }

    /// Wait for the turn of a finish that has its reply, run its callback
    /// and record the commit.
    async fn turn(
        &self,
        finished: Finished,
        broke: bool,
        records: Vec<(u64, Tid, Vec<u8>)>,
    ) -> Outcome {
        let tid = finished.tid().0;
        let turn = match finished.turn().await {
            Ok(turn) => turn,
            Err(error) => {
                let mut log = lock(&self.log);
                let client = log.client;
                log.faults.push(format!(
                    "c{client}: the finish of {tid:016x} had its reply and fails: {error}"
                ));
                return Outcome::Lost { in_finish: true };
            }
        };
        // The callback of the finish. It runs only in the turn of the tid:
        // the pool did not deliver the tid yet.
        let delivered = self.pool.last_transaction().0;
        {
            let mut log = lock(&self.log);
            let client = log.client;
            if turn.stale() && log.stale.is_none() {
                log.faults.push(format!(
                    "c{client}: the callback of {tid:016x} runs before the stale mark"
                ));
            }
            if delivered < tid {
                log.deliver(tid, Kind::Own);
            } else {
                log.faults.push(format!(
                    "c{client}: the turn of {tid:016x} comes after the pool delivered {delivered:016x}"
                ));
            }
        }
        let stale = turn.stale();
        drop(turn);
        // The finish returns its tid.
        let delivered = self.pool.last_transaction().0;
        {
            let mut log = lock(&self.log);
            log.returned.push(tid);
            if delivered < tid {
                let client = log.client;
                log.faults.push(format!(
                    "c{client}: the finish of {tid:016x} returns before the pool delivered it"
                ));
            }
        }
        self.check_last();
        let mut state = self.shared.lock();
        if stale {
            state.pools.lost_listener_finishes += 1;
            state.trace(format!(
                "c{} finishes {tid:016x} after the stale mark",
                self.id
            ));
        }
        if broke {
            state.trace(format!("c{} had the reply of {tid:016x}", self.id));
        }
        state.trace(format!("c{} commits tid {tid:016x}", self.id));
        for (oid, _, _) in &records {
            let known = state.known.entry(*oid).or_default();
            *known = (*known).max(tid);
        }
        state.acked.push(Acked {
            tid,
            records: records
                .into_iter()
                .map(|(oid, _, data)| (oid, data))
                .collect(),
            blobs: Vec::new(),
            tree: None,
        });
        drop(state);
        self.reach(phase::ACKNOWLEDGED);
        Outcome::Acked(())
    }

    /// Tell the driver that a commit reached `phase`, as `client.rs` does.
    fn reach(&self, phase: &'static str) {
        common::reach(&self.shared, self.id, phase);
    }
}

/// A worker of a pool: run the transactions of the client until none is
/// left or the pool loses its listener.
async fn work(run: Arc<Run>, mut rng: Rng, ended: Arc<watch::Sender<usize>>) {
    let mut t = 0;
    'transactions: while run.take().is_some() {
        t += 1;
        for attempt in 0..TRIES {
            match run.commit(&mut rng, t, attempt).await {
                Outcome::Acked(()) => continue 'transactions,
                Outcome::Retry => {
                    let backoff = rng.between(Duration::from_micros(100), Duration::from_millis(5));
                    sleep(&run.shared, backoff).await;
                }
                Outcome::Lost { in_finish } => {
                    run.trace("lost a connection");
                    if in_finish {
                        continue 'transactions;
                    }
                    if !run.pool.is_connected() {
                        // A new pool runs the transaction.
                        run.give_back();
                        break 'transactions;
                    }
                }
            }
        }
    }
    run.check_last();
    ended.send_modify(|n| *n += 1);
}

/// The consumer of a pool: record each delivery and confirm it, until the
/// close.
async fn consume(run: Arc<Run>, ended: Arc<watch::Sender<usize>>) {
    let mut foreign = BTreeSet::new();
    while let Some(delivery) = run.pool.next_delivery().await {
        {
            let mut log = lock(&run.log);
            match delivery {
                Delivery::Invalidate(invalidation) => {
                    log.deliver(invalidation.tid.0, Kind::Foreign);
                    foreign.insert(invalidation.tid.0);
                }
                Delivery::Stale => {
                    log.lost = true;
                    log.stale = Some(log.entries.len());
                }
            }
        }
        run.pool.delivered();
        run.check_last();
    }
    run.shared.lock().pools.foreign += foreign.len();
    ended.send_modify(|n| *n += 1);
}

/// Check the logs of the pools against the tids of the store at the end of
/// the run. Return the breaks of the rule, and the number of own
/// deliveries.
pub(crate) fn check_logs(logs: &[PoolLog], tids: &BTreeSet<u64>) -> (Vec<String>, usize) {
    let mut faults = Vec::new();
    let mut own = 0;
    for log in logs {
        faults.extend(log.faults.iter().cloned());
        let returned: BTreeSet<u64> = log.returned.iter().copied().collect();
        let delivered: BTreeSet<u64> = log.entries.iter().map(|(tid, _)| *tid).collect();
        for (tid, kind) in &log.entries {
            match kind {
                Kind::Foreign if returned.contains(tid) => faults.push(format!(
                    "c{}: the commit {tid:016x} of the pool reaches its consumer as an invalidation",
                    log.client
                )),
                Kind::Own => own += 1,
                Kind::Foreign => {}
            }
        }
        for tid in &returned {
            if !log.entries.contains(&(*tid, Kind::Own)) {
                faults.push(format!(
                    "c{}: the finish of {tid:016x} returns without its callback",
                    log.client
                ));
            }
        }
        // After the stale mark, the consumer treats every object as stale:
        // a tid that the listener did not deliver is no skip.
        let before = &log.entries[..log.stale.unwrap_or(log.entries.len())];
        let last = before.last().map_or(log.start, |(tid, _)| *tid);
        for tid in tids.range(log.start + 1..last.max(log.start) + 1) {
            if !delivered.contains(tid) {
                faults.push(format!(
                    "c{}: the pool skips tid {tid:016x}, up to {last:016x}",
                    log.client
                ));
            }
        }
        if let Some((target, false)) = log.target {
            let last = log.last();
            faults.push(format!(
                "c{}: the pool stops delivering at {last:016x}, before {target:016x}",
                log.client
            ));
        }
    }
    (faults, own)
}
