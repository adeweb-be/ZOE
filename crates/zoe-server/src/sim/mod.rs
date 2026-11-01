//! A deterministic simulator of the server and its clients (N18).
//!
//! The server and its clients run in one process on a simulated network
//! and a simulated clock. The store is on the `MemVfs` of the core. A seed
//! decides the whole schedule: the order of the tasks and of the threads,
//! the delays, losses, partitions and closes of the network, and the
//! points where the server crashes. A crash comes at a phase of a commit:
//! staged, locked, finishing or acknowledged. It drops the server process
//! and opens the store again from a crash image of the `MemVfs`. The same
//! seed gives the same run, event for event, so a failing seed reproduces.
//!
//! The simulator runs the production code. Each connection of the server
//! runs `protocol::serve_stream`, the session of the server and the core,
//! and each client runs `zoe-client`. Both reach their tasks, their
//! threads, the network and the time through a runtime trait, and the
//! simulator gives its own runtime.
//!
//! The driver is an executor of its own. It polls the tasks and runs the
//! blocking threads of the server one at a time. A thread gives the baton
//! back when it waits for a channel or for a lock (through a waiter of the
//! simulation), when the core syncs the database file, and when it ends.
//!
//! Only the dev-dependency of this crate on itself turns the feature `sim`
//! on, so only test builds have this module.

mod client;
mod common;
mod disk;
mod net;
mod pool;
mod rng;
mod server;
mod tree;
mod world;

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::Path;
use std::sync::{Arc, mpsc};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use zoe_core::vfs::MemVfs;
use zoe_core::{Clock, Oid, Options, Store, Tid};

use crate::protocol::{NamedStorage, Pool, Queues, STORAGE_NAME, Storages};
use client::SimClient;
use disk::{Cache, DATABASE, SimClock, SimVfs};
use pool::SimPoolClient;
use rng::Rng;
pub(crate) use world::handed_off;
use world::{
    Baton, NetFault, Owner, PlannedCrash, Resume, Shared, Task, TaskWaker, Timer, Wait, Why, phase,
};

/// The most steps of one run. A run that needs more is a bug of the
/// simulator or a livelock of the code that it runs.
const MAX_STEPS: u64 = 50_000_000;

/// The longest real time that the driver waits for a thread to give the
/// baton back. A thread that takes longer is stuck, and the run fails with
/// the end of its trace.
const BATON_TIMEOUT: Duration = Duration::from_secs(60);

/// The longest one-way delay of a frame when `Faults::delay` is `None`.
const DEFAULT_DELAY: Duration = Duration::from_millis(2);

/// The directory of the store, as the status of the server names it. It
/// does not exist: the store is on a `MemVfs`, so `get_info` gives the
/// size 0 (`NamedStorage::in_memory`).
const STORE_PATH: &str = "/nonexistent/zoe-sim";

/// The threads of the read pool of the simulated server. A fixed number
/// keeps a run the same on every machine.
const SIM_POOL: std::num::NonZeroUsize = std::num::NonZeroUsize::new(4).unwrap();

/// The faults that a run injects.
#[derive(Clone, Debug)]
pub struct Faults {
    /// The longest one-way delay of a frame on the network. `None` keeps
    /// the default of 2 ms.
    pub delay: Option<Duration>,
    /// The chance that a frame is lost. A lost frame breaks its
    /// connection.
    pub loss: f64,
    /// The number of partitions: each cuts one client from the server for
    /// 100 ms to 3 s.
    pub partitions: usize,
    /// The number of connections that the network closes.
    pub closes: usize,
    /// The first client stalls this long after its first vote, before its
    /// `tpc_finish`, and keeps its locks.
    pub stall_client_for: Option<Duration>,
    /// A deliberate bug for the tests: the server acknowledges
    /// `tpc_finish` before the commit is durable. The sync of the commit
    /// happens 5 to 100 ms later.
    pub ack_before_durable: bool,
    /// A crash is a power loss: the disk keeps the synced writes, a seeded
    /// prefix of the other operations, and a seeded subset of the unsynced
    /// writes to the database file, some of them torn. When false, a crash
    /// is a kill of the process, and the disk keeps every write.
    pub power_loss: bool,
    /// The number of times that the network breaks the listener of a pool
    /// (`Simulation::pool`) in a run. Each time, a finish of that pool has
    /// the reply of the server and did not take its turn yet.
    pub listener_losses: usize,
}

impl Default for Faults {
    fn default() -> Self {
        Self {
            delay: None,
            loss: 0.002,
            partitions: 0,
            closes: 0,
            stall_client_for: None,
            ack_before_durable: false,
            power_loss: true,
            listener_losses: 0,
        }
    }
}

/// What a run gives.
#[derive(Clone, Debug, Default)]
pub struct Report {
    /// The events of the run, in order, as text: each frame on the
    /// network with its method, each fault, each crash. Each line starts
    /// with the simulated time in microseconds.
    pub trace: Vec<String>,
    /// The commits that a client saw acknowledged.
    pub acknowledged: usize,
    /// The acknowledged commits that the store does not hold after the
    /// last crash.
    pub lost: usize,
    /// The loads that gave a serial older than a commit of the oid that a
    /// client saw acknowledged before the load.
    pub stale_reads: usize,
    /// The crashes of the server.
    pub crashes: usize,
    /// The phase of the commit at each crash, in order: `staged` (the
    /// client stored and did not vote), `locked` (between the vote and
    /// `tpc_finish`), `finishing` (at the sync of the database file in
    /// `tpc_finish`) or `acknowledged` (the client got the reply of
    /// `tpc_finish`).
    pub crash_phases: Vec<&'static str>,
    /// The findings on the store after the last crash: those of the checks
    /// of `zoe verify`, a blob file without its blob record, and a store
    /// that does not open. Empty when the store is sound.
    pub verify: Vec<String>,
    /// The simulated time of the run.
    pub elapsed: Duration,
    /// Each break of the rule of the pools (`Simulation::pool`), as text: a
    /// tid delivered twice or after a higher tid, a skipped tid of the
    /// store, a commit of a pool delivered as an invalidation, a finish
    /// that returns without its callback or before the tids below it, a
    /// last transaction above the last delivered tid, a finish with its
    /// reply that fails or runs its callback before the stale mark after a
    /// lost listener, and a pool that stops delivering.
    pub order_faults: Vec<String>,
    /// The tids of other clients that the pools delivered.
    pub foreign_deliveries: usize,
    /// The commits of the pools that they delivered through the callback
    /// of their finish.
    pub own_deliveries: usize,
    /// The tids that a listener held while a finish that counted as in
    /// flight when the tid came waited for its reply.
    pub held: usize,
    /// The finishes whose reply came while their pool held a lower tid.
    pub waited_finishes: usize,
    /// The finishes that had their reply when their pool lost its
    /// listener, and returned their tid after the stale mark.
    pub lost_listener_finishes: usize,
    /// With `Simulation::hot_length`, each break of the history of the hot
    /// `Length`, as text: a record whose value is not the value of the
    /// record before it plus 1, or the first record not 1, or a record that
    /// does not read as a `Length`.
    pub length_faults: Vec<String>,
    /// With `Simulation::split_merges`, each break of the tree that the
    /// check after each recovery and after the run found, as text: a
    /// separator, an order of keys or a next bucket that is wrong, a key of
    /// an acknowledged commit that the tree does not hold, a bucket outside
    /// the tree, and a transaction index that does not agree with the
    /// records of a commit whose vote resolved oids.
    pub tree_faults: Vec<String>,
    /// The checks of the tree of the split writers: one after each
    /// recovery, and one after the run.
    pub tree_checks: usize,
    /// The acknowledged commits of the split writers whose vote wrote an
    /// oid that the transaction did not store: rule 3 of ADR 0049 kept the
    /// bucket of the other side.
    pub unstored_writes: usize,
    /// The oids that acknowledged commits of the split writers stored and
    /// that their votes resolved without a record, as the store holds them
    /// after the run.
    pub dropped: usize,
    /// The transactions of the split writers whose vote wrote an oid that
    /// they did not store and whose `tpc_finish` got no reply: a crash or a
    /// lost connection came between the vote and the reply.
    pub interrupted_split_finishes: usize,
    /// The votes of the split writers that split a bucket and got no
    /// reply: a crash or a lost connection came before the reply.
    pub interrupted_split_votes: usize,
    /// The locks that finishes handed off, summed over the acknowledged
    /// commits of the split writers whose vote wrote an oid that the
    /// commit did not store. In a tree of one node, such a finish hands off
    /// nothing (ADR 0049, section 12), so a count above 0 is a hand-off of
    /// the node or of the new bucket of the transaction, or breaks that
    /// premise.
    pub split_handoffs: usize,
    /// The locks that the finish of an acknowledged commit of a split
    /// writer whose vote resolved oids handed off beyond the records of
    /// its commit, summed over those commits. The finish hands off only
    /// staged oids, so this is a general bound, and it must be 0. It fires
    /// only when a finish hands off more locks than its commit has
    /// records: the observer of the core gives a count for each finish,
    /// not the oids.
    pub excess_handoffs: usize,
    /// The locks that the finishes of the acknowledged commits of the split
    /// writers handed off. The other hand-offs of `handoffs` are those of
    /// finishes whose reply did not come.
    pub acked_handoffs: usize,
    /// The locks that the finishes of the server handed to the vote at the
    /// head of their queues (ADR 0044), over every server process of the
    /// run.
    pub handoffs: u64,
    /// The syncs of commit batches, over every server process of the run,
    /// as `zoe_fsync_batch_size` counts them.
    pub syncs: u64,
    /// The commits that these syncs made durable.
    pub synced_commits: u64,
}

/// A run of the server and its clients.
#[derive(Clone, Debug)]
pub struct Simulation {
    seed: u64,
    clients: usize,
    commits: usize,
    crashes: usize,
    blobs: bool,
    faults: Faults,
    /// The size of the pool of each client, or `None` for clients of one
    /// connection.
    pool: Option<usize>,
    /// The hot oid holds a `BTrees.Length.Length` that each transaction
    /// that writes it increments.
    hot_length: bool,
    /// A sync of the database file takes simulated time, and other tasks
    /// and threads run meanwhile.
    slow_syncs: bool,
    /// The clients are split writers of one tree, with `split_merges` on.
    split_merges: bool,
}

impl Simulation {
    /// A run with the seed `seed`: 2 clients, 20 commits, no crash, no
    /// blob, and the default faults.
    pub fn new(seed: u64) -> Self {
        Self {
            seed,
            clients: 2,
            commits: 20,
            crashes: 0,
            blobs: false,
            faults: Faults::default(),
            pool: None,
            hot_length: false,
            slow_syncs: false,
            split_merges: false,
        }
    }

    /// Run `n` clients, at least 1.
    #[must_use]
    pub fn clients(mut self, n: usize) -> Self {
        self.clients = n.max(1);
        self
    }

    /// Run `n` transactions in all, shared among the clients.
    #[must_use]
    pub fn commits(mut self, n: usize) -> Self {
        self.commits = n;
        self
    }

    /// Crash the server `n` times. The seed gives each crash a phase of
    /// the commit and a number of acknowledged commits. The crash comes at
    /// the first commit that reaches that phase after that number.
    #[must_use]
    pub fn crashes(mut self, n: usize) -> Self {
        self.crashes = n;
        self
    }

    /// With `true`, the clients also commit blobs, as `ZEO.ClientStorage`
    /// uploads them, to oids of their own.
    #[must_use]
    pub fn blobs(mut self, blobs: bool) -> Self {
        self.blobs = blobs;
        self
    }

    /// Run each client on a `zoe_client::Pool`: a listener and at most `n`
    /// connections that commit, at least 1, with `n` transactions at once.
    /// Each pool records what it delivers, and the report checks it
    /// (`Report::order_faults`).
    ///
    /// Do not combine it with `crashes`: a crash can remove a tid that a
    /// pool delivered, so the check against the store gives false faults.
    /// `run` panics on that combination.
    #[must_use]
    pub fn pool(mut self, n: usize) -> Self {
        self.pool = Some(n.max(1));
        self
    }

    /// With `true`, the hot oid of the clients holds a
    /// `BTrees.Length.Length`. Each transaction that writes it reads its
    /// value and stores the value plus 1, so concurrent writers conflict and
    /// tier 1 merges them, with the lock hand-off. The check after the run
    /// reads the history of the oid: each record holds the value of the
    /// record before it plus 1 (`Report::length_faults`).
    #[must_use]
    pub fn hot_length(mut self, hot_length: bool) -> Self {
        self.hot_length = hot_length;
        self
    }

    /// With `true`, a sync of the database file takes 100 µs to 2 ms of
    /// simulated time, as the seed chooses, and the driver runs other tasks
    /// and threads meanwhile. So other votes and finishes run while a
    /// commit batch syncs, as they do on a disk. A sync that follows a
    /// change of the length of the file goes on at once: redb grows the
    /// file under a lock of its own, which a read takes too. Without it,
    /// the thread that syncs goes on at once.
    #[must_use]
    pub fn slow_syncs(mut self, slow_syncs: bool) -> Self {
        self.slow_syncs = slow_syncs;
        self
    }

    /// With `true`, the store holds a merge policy that sets
    /// `split_merges` to `on`, and one `BTrees.IIBTree.IIBTree` with buckets
    /// of at most 4 keys (`tree.rs`). Each client declares `server_writes`,
    /// and each of its transactions inserts one growing key into the tree.
    /// So two clients that read one state of the last bucket both split
    /// it, and the vote of the second merges with rule 3 of ADR 0049: it
    /// writes the bucket and the new bucket of the other side, and resolves
    /// the node and its own new bucket without a record. The clients write
    /// nothing else: no hot oid and no blob. After each recovery and after
    /// the run, the driver checks the tree (`Report::tree_faults`).
    #[must_use]
    pub fn split_merges(mut self, split_merges: bool) -> Self {
        self.split_merges = split_merges;
        self
    }

    /// Inject `faults`.
    #[must_use]
    pub fn faults(mut self, faults: Faults) -> Self {
        self.faults = faults;
        self
    }

    /// Run the simulation to its end. Panic when the run has pools and
    /// crashes (see `pool`).
    pub fn run(self) -> Report {
        assert!(
            self.pool.is_none() || self.crashes == 0,
            "a run with pools has no crash"
        );
        Driver::new(self).run()
    }
}

/// The driver of a run.
struct Driver {
    plan: Simulation,
    shared: Arc<Shared>,
    tasks: BTreeMap<u64, (Owner, Task)>,
    batons: BTreeMap<u64, Baton>,
    yields: mpsc::Receiver<(u64, Why)>,
    /// The disk of the current server process.
    disk: MemVfs,
    /// The cache of the database file of the current server process.
    cache: Arc<Cache>,
    crashes: usize,
    crash_phases: Vec<&'static str>,
    flush_pending: bool,
    /// The numbers of the crash images, apart from the schedule.
    images: Rng,
    /// The lock hand-offs of the server processes that stopped.
    handoffs: u64,
    /// The syncs of commit batches of the server processes that stopped,
    /// and the commits that they made durable.
    syncs: (u64, u64),
    /// The breaks of the tree of the split writers that the checks after
    /// the recoveries found.
    tree_faults: Vec<String>,
    /// The checks of the tree of the split writers that ran.
    tree_checks: usize,
}

impl Driver {
    fn new(plan: Simulation) -> Self {
        let (send, yields) = mpsc::channel();
        let mut rng = Rng::new(plan.seed.rotate_left(32) ^ 0xC0FF_EE00);
        let span = plan.commits.max(1) as u64;
        // A crash comes in the first half of the commits, so that the run
        // reaches it also when some transactions give up.
        let crash_plan: Vec<PlannedCrash> = (0..plan.crashes)
            .map(|_| PlannedCrash {
                phase: phase::ALL[rng.below(phase::ALL.len() as u64) as usize],
                after: rng.below(span / 2 + 1) as usize,
            })
            .collect();
        // A commit sends about 10 frames. The faults of the network start
        // at frames that the seed chooses among those of the commits.
        let frames = span * 10;
        let mut fault_plan: BTreeMap<u64, Vec<NetFault>> = BTreeMap::new();
        let faults = [
            (plan.faults.partitions, NetFault::Partition),
            (plan.faults.closes, NetFault::Close),
        ];
        for (count, fault) in faults {
            for _ in 0..count {
                fault_plan
                    .entry(1 + rng.below(frames))
                    .or_default()
                    .push(fault);
            }
        }
        let shared = Shared::new(plan.seed, plan.faults.clone(), fault_plan, crash_plan, send);
        {
            let mut state = shared.lock();
            state.clients = plan.clients;
            state.listener_losses = plan.faults.listener_losses;
        }
        Self {
            images: rng.fork(),
            plan,
            shared,
            tasks: BTreeMap::new(),
            batons: BTreeMap::new(),
            yields,
            disk: MemVfs::new(),
            cache: Arc::default(),
            crashes: 0,
            crash_phases: Vec::new(),
            flush_pending: false,
            handoffs: 0,
            syncs: (0, 0),
            tree_faults: Vec::new(),
            tree_checks: 0,
        }
    }

    fn run(mut self) -> Report {
        if self.plan.split_merges {
            let root = tree::plant(&self.disk).expect("the tree plants on a new disk");
            self.shared.lock().tree_root = Some(root);
        }
        self.start_server("starts");
        let clients = self.plan.clients;
        for id in 0..clients {
            let transactions =
                self.plan.commits / clients + usize::from(id < self.plan.commits % clients);
            let mut state = self.shared.lock();
            let task: Task = match self.plan.pool {
                Some(size) => Box::pin(
                    SimPoolClient {
                        shared: Arc::clone(&self.shared),
                        id,
                        transactions,
                        rng: state.rng.fork(),
                        size,
                    }
                    .run(),
                ),
                None => Box::pin(
                    SimClient {
                        shared: Arc::clone(&self.shared),
                        id,
                        transactions,
                        rng: state.rng.fork(),
                        stall: self.plan.faults.stall_client_for.filter(|_| id == 0),
                        blobs: self.plan.blobs,
                        hot_length: self.plan.hot_length,
                        split_merges: self.plan.split_merges,
                        conn: None,
                    }
                    .run(),
                ),
            };
            state.spawn(&self.shared.ready, Owner::Client(id), task);
        }
        self.adopt();
        let mut steps = 0u64;
        while self.shared.lock().clients_done < clients {
            steps += 1;
            assert!(steps < MAX_STEPS, "the run does not end:\n{}", self.tail());
            self.fire_timers();
            if !self.step() {
                self.advance();
            }
        }
        // The end of the run is a crash too. Then the store opens from what
        // is durable, and every acknowledged commit must be there.
        self.stop("stops at the end of the run");
        let (lost, verify, tids, length_faults, checked) = self.verify();
        let mut state = self.shared.lock();
        let logs = std::mem::take(&mut state.pools.logs);
        let mut report = state.report();
        drop(state);
        let (order_faults, own) = pool::check_logs(&logs, &tids);
        report.order_faults = order_faults;
        report.own_deliveries = own;
        report.lost = lost;
        report.verify = verify;
        report.length_faults = length_faults;
        report.tree_faults = std::mem::take(&mut self.tree_faults);
        report.tree_faults.extend(checked.faults);
        report.dropped = checked.dropped;
        report.split_handoffs = checked.split_handoffs;
        report.excess_handoffs = checked.excess_handoffs;
        report.acked_handoffs = checked.acked_handoffs;
        report.tree_checks = self.tree_checks;
        report.handoffs = self.handoffs;
        (report.syncs, report.synced_commits) = self.syncs;
        report.crashes = self.crashes;
        report.crash_phases = std::mem::take(&mut self.crash_phases);
        report
    }

    /// Open the store on the current disk, and accept connections.
    fn start_server(&mut self, why: &str) {
        let clock = SimClock(Arc::clone(&self.shared));
        let start = clock.now();
        let vfs = SimVfs::new(self.disk.clone());
        self.cache = Arc::clone(&vfs.cache);
        let opened = Store::open_full(vfs, clock, Options::default());
        let mut state = self.shared.lock();
        match opened {
            Ok(store) => {
                let store = Arc::new(store);
                state.server.store = Some(Arc::clone(&store));
                let storage = NamedStorage::new(
                    STORAGE_NAME,
                    Path::new(STORE_PATH),
                    Arc::clone(&store),
                    Queues::DEFAULT,
                )
                .started_at(start)
                .in_memory();
                let storages = Storages::new(vec![Arc::new(storage)], Queues::DEFAULT.client)
                    .with_pool(Pool::new(SIM_POOL));
                state.server.storages = Some(Arc::new(storages));
                state.server.up = true;
                state.trace(format!("server {why}"));
                let checking = state.tree_root.filter(|_| self.crashes > 0);
                let acked = checking.map(|_| state.acked.clone());
                drop(state);
                if let (Some(root), Some(acked)) = (checking, acked) {
                    self.check_tree(&store, root, &acked);
                }
            }
            Err(error) => {
                state.trace(format!("server {why} and fails: {error}"));
                state.server.halted = true;
            }
        }
    }

    /// Check the tree of the split writers on `store`, which the server
    /// opened after a crash, against the commits `acked` that the clients
    /// saw acknowledged before it. Keep and trace each break.
    fn check_tree(&mut self, store: &Store, root: u64, acked: &[world::Acked]) {
        let checked = tree::check(store, root, acked);
        self.tree_checks += 1;
        let crash = self.crashes;
        let mut state = self.shared.lock();
        for fault in checked.faults {
            let fault = format!("after crash {crash}: {fault}");
            state.trace(format!("check: {fault}"));
            self.tree_faults.push(fault);
        }
    }

    /// Move the tasks and the threads that the last step started into the
    /// driver.
    fn adopt(&mut self) {
        let mut state = self.shared.lock();
        for (id, owner, task) in state.spawned.drain(..) {
            self.tasks.insert(id, (owner, task));
        }
        for (id, baton) in state.new_batons.drain(..) {
            self.batons.insert(id, baton);
        }
    }

    /// Run the timers that are due, in order.
    fn fire_timers(&mut self) {
        loop {
            let timer = self.shared.lock().due();
            let Some(timer) = timer else { break };
            self.fire(timer);
        }
    }

    fn fire(&mut self, timer: Timer) {
        let mut state = self.shared.lock();
        let mut wakers: Vec<Waker> = Vec::new();
        match timer {
            Timer::Wake(waker) => wakers.push(waker),
            Timer::Deliver {
                conn,
                to_client,
                bytes,
            } => wakers.extend(net::deliver(&mut state, conn, to_client, bytes)),
            Timer::Eof { conn, to_client } => wakers.extend(net::eof(&mut state, conn, to_client)),
            Timer::Reset { conn } => {
                if state.conns.get(&conn).is_some_and(|conn| !conn.reset) {
                    state.trace(format!("net breaks connection {conn}"));
                }
                wakers.extend(net::reset(&mut state, conn));
            }
            Timer::Heal { client } => {
                if state.partitioned(client).is_none() && state.partitions.remove(&client).is_some()
                {
                    state.trace(format!("net heals the partition of c{client}"));
                }
            }
            Timer::Restart => {
                drop(state);
                self.start_server("starts again");
                return;
            }
            Timer::Flush { generation } => {
                self.flush_pending = false;
                if state.server.up && state.server.generation == generation {
                    state.trace("server syncs late");
                    drop(state);
                    let _ = self.cache.flush(&self.disk.file(DATABASE));
                }
                return;
            }
        }
        drop(state);
        for waker in wakers {
            waker.wake();
        }
    }

    /// Run one task or one thread that can run, chosen by the seed. Return
    /// false when nothing can run.
    fn step(&mut self) -> bool {
        let tasks = self.shared.ready.list();
        let threads: Vec<u64> = {
            let state = self.shared.lock();
            state
                .threads
                .iter()
                .filter(|(_, thread)| self.runnable(state.now, thread.wait))
                .map(|(id, _)| *id)
                .collect()
        };
        let count = tasks.len() + threads.len();
        if count == 0 {
            return false;
        }
        let pick = self.shared.lock().rng.below(count as u64) as usize;
        if let Some(&task) = tasks.get(pick) {
            self.poll(task);
        } else {
            self.run_thread(threads[pick - tasks.len()]);
        }
        self.adopt();
        // A client reached the phase of a planned crash in this step.
        let reached = self.shared.lock().crash_now.take();
        if let Some(phase) = reached {
            self.crash(phase, &format!("crashes in the phase {phase}"));
        }
        true
    }

    /// Tell if a thread that waits as `wait` can run at the time `now`.
    fn runnable(&self, now: Duration, wait: Wait) -> bool {
        match wait {
            Wait::Ready => true,
            Wait::Syncing { until, .. } => now >= until,
            Wait::Parked { waiter, deadline } => {
                self.shared.permits.contains(waiter) || now >= deadline
            }
        }
    }

    fn poll(&mut self, id: u64) {
        self.shared.ready.take(id);
        let Some((owner, mut task)) = self.tasks.remove(&id) else {
            return;
        };
        let waker = Waker::from(Arc::new(TaskWaker {
            id,
            shared: Arc::clone(&self.shared),
        }));
        let mut cx = Context::from_waker(&waker);
        if task.as_mut().poll(&mut cx) == Poll::Pending {
            self.tasks.insert(id, (owner, task));
        }
    }

    /// Hand the baton to the thread `id` until it gives it back for
    /// something else than a sync. With slow syncs, a sync gives the baton
    /// back too: the thread goes on after the time of the sync.
    fn run_thread(&mut self, id: u64) {
        let mut how = {
            let mut state = self.shared.lock();
            match state.threads.get_mut(&id) {
                Some(thread) => match std::mem::replace(&mut thread.wait, Wait::Ready) {
                    Wait::Syncing { resume, .. } => resume,
                    _ => Resume::Go,
                },
                None => Resume::Go,
            }
        };
        loop {
            match self.resume(id, how) {
                Why::Sync { grows } => match self.on_sync() {
                    Some(next) if self.plan.slow_syncs && !grows => {
                        let mut state = self.shared.lock();
                        let time = state
                            .rng
                            .between(Duration::from_micros(100), Duration::from_millis(2));
                        let until = state.now + time;
                        if let Some(thread) = state.threads.get_mut(&id) {
                            thread.wait = Wait::Syncing {
                                until,
                                resume: next,
                            };
                        }
                        return;
                    }
                    Some(next) => how = next,
                    None => {
                        // The thread syncs at the crash, so the stop ends it
                        // first.
                        let mut state = self.shared.lock();
                        let now = state.now;
                        if let Some(thread) = state.threads.get_mut(&id) {
                            thread.wait = Wait::Syncing {
                                until: now,
                                resume: Resume::Go,
                            };
                        }
                        drop(state);
                        let finishing = phase::FINISHING;
                        self.crash(finishing, &format!("crashes in a sync, phase {finishing}"));
                        return;
                    }
                },
                _ => return,
            }
        }
    }

    /// Hand the baton to `id` with `how`, wait until it gives it back, and
    /// note why.
    fn resume(&mut self, id: u64, how: Resume) -> Why {
        let baton = self.batons.get(&id).expect("a thread of the simulation");
        baton
            .resume
            .send(how)
            .expect("the thread waits for the baton");
        let Ok((from, why)) = self.yields.recv_timeout(BATON_TIMEOUT) else {
            panic!(
                "a thread does not give the baton back within {} s:\n{}",
                BATON_TIMEOUT.as_secs(),
                self.tail()
            );
        };
        assert_eq!(from, id, "only one thread runs at a time");
        let mut state = self.shared.lock();
        match why {
            Why::Parked { waiter, deadline } => {
                let now = state.now;
                if let Some(thread) = state.threads.get_mut(&id) {
                    thread.wait = Wait::Parked { waiter, deadline };
                    let client = thread.client;
                    // A finite deadline is a lock wait of the core.
                    if deadline != Duration::MAX && how != Resume::Killed {
                        let client = client.map_or_else(String::new, |c| format!("c{c} "));
                        let most = deadline.saturating_sub(now).as_millis();
                        state.trace(format!("s {client}waits for a lock, {most} ms at most"));
                    }
                }
            }
            Why::Sync { .. } => {}
            Why::Exited => {
                state.threads.remove(&id);
                drop(state);
                let baton = self.batons.remove(&id).expect("a thread of the simulation");
                if let Err(panic) = baton.join.join() {
                    std::panic::resume_unwind(panic);
                }
            }
        }
        why
    }

    /// Note a sync of the database file: a commit in the phase
    /// `finishing`. Return how the thread goes on, or `None` when the
    /// server crashes now.
    fn on_sync(&mut self) -> Option<Resume> {
        let mut state = self.shared.lock();
        if state.crash_at(phase::FINISHING) {
            return None;
        }
        let generation = state.server.generation;
        if !state.faults.ack_before_durable {
            return Some(Resume::Go);
        }
        if !self.flush_pending {
            self.flush_pending = true;
            let delay = state
                .rng
                .between(Duration::from_millis(5), Duration::from_millis(100));
            state.after(delay, Timer::Flush { generation });
        }
        Some(Resume::Defer)
    }

    /// Crash the server now, in the phase `phase` of a commit: keep a crash
    /// image of its disk, drop the process, and start it again 5 to 100 ms
    /// later.
    fn crash(&mut self, phase: &'static str, why: &str) {
        if !self.shared.lock().server.up {
            return;
        }
        self.crashes += 1;
        self.crash_phases.push(phase);
        self.stop(why);
        let mut state = self.shared.lock();
        let delay = state
            .rng
            .between(Duration::from_millis(5), Duration::from_millis(100));
        state.after(delay, Timer::Restart);
    }

    /// Stop the server process as a crash does. With a power loss, the
    /// disk keeps what was synced, a seeded prefix of the other operations,
    /// and a seeded subset of the cached writes to the database file, some
    /// of them torn; with a kill, it keeps every write. Every connection of
    /// the process breaks, its tasks drop, and its threads end.
    fn stop(&mut self, why: &str) {
        let power_loss = self.shared.lock().faults.power_loss;
        let draw = self.images.next_u64();
        let mut landed = (0, 0);
        let image = self.disk.crash_now(|pending| {
            let n = if power_loss {
                (draw % (pending as u64 + 1)) as usize
            } else {
                pending
            };
            landed = (n, pending);
            n
        });
        let cached = self
            .cache
            .crash(&image, power_loss, &mut self.images)
            .expect("a crash image takes the cached writes");
        // The cached writes that landed are on the disk, as the rest of the
        // image is.
        let image = image.durable_copy();
        let (generation, wakers) = {
            let mut state = self.shared.lock();
            state.trace(format!(
                "server {why}: {} of {} pending writes and {} of {} cached writes of the database land",
                landed.0, landed.1, cached.0, cached.1
            ));
            let generation = state.server.generation;
            if let Some(storage) = state
                .server
                .storages
                .as_ref()
                .and_then(|storages| storages.get(STORAGE_NAME))
            {
                let metrics = storage.exported().metrics;
                self.handoffs += metrics
                    .lock_handoffs
                    .load(std::sync::atomic::Ordering::Relaxed);
                let (syncs, commits) = metrics.fsync_batch_size.totals();
                self.syncs.0 += syncs;
                self.syncs.1 += commits as u64;
            }
            state.server.up = false;
            state.server.storages = None;
            state.server.store = None;
            state.server.generation += 1;
            let conns: Vec<u64> = state
                .conns
                .iter()
                .filter(|(_, conn)| conn.generation == generation)
                .map(|(id, _)| *id)
                .collect();
            let mut wakers = Vec::new();
            for conn in conns {
                wakers.extend(net::reset(&mut state, conn));
            }
            (generation, wakers)
        };
        for waker in wakers {
            waker.wake();
        }
        // The tasks of the process drop first. So each channel of its
        // threads closes, and each thread ends at its next wait.
        self.adopt();
        self.tasks
            .retain(|_, (owner, _)| *owner != Owner::Server(generation));
        let mut resumes = 0u32;
        loop {
            // A thread that syncs ends first: it can lead a commit batch
            // that other threads wait for.
            let next = self
                .shared
                .lock()
                .threads
                .iter()
                .filter(|(_, thread)| thread.generation == generation)
                .min_by_key(|(_, thread)| !matches!(thread.wait, Wait::Syncing { .. }))
                .map(|(id, _)| *id);
            let Some(id) = next else { break };
            resumes += 1;
            assert!(
                resumes < 100_000,
                "a thread of a dead server does not end:\n{}",
                self.tail()
            );
            self.resume(id, Resume::Killed);
            self.adopt();
        }
        self.disk = image;
    }

    /// Open the disk after the last stop. Return the number of the
    /// acknowledged commits that it does not hold whole, records and blobs,
    /// the findings on the store, the tids of the store, the breaks of the
    /// history of the hot `Length`, and the check of the tree of the split
    /// writers.
    ///
    /// With `Simulation::hot_length`, a commit of the hot oid holds the
    /// record that tier 1 merged, not the record of the client: it counts
    /// when the store holds a `Length` at its tid. With
    /// `Simulation::split_merges`, a commit counts when the store holds its
    /// records that the vote did not resolve, and the tree holds its key.
    fn verify(
        &mut self,
    ) -> (
        usize,
        Vec<String>,
        BTreeSet<u64>,
        Vec<String>,
        tree::Checked,
    ) {
        let acked = self.shared.lock().acked.clone();
        let clock = SimClock(Arc::clone(&self.shared));
        let vfs = SimVfs::new(self.disk.clone());
        let store = match Store::open_full(vfs, clock, Options::default()) {
            Ok(store) => store,
            Err(error) => {
                let finding = format!("the store does not open: {error}");
                self.shared.lock().trace(&finding);
                return (
                    acked.len(),
                    vec![finding],
                    BTreeSet::new(),
                    Vec::new(),
                    tree::Checked::default(),
                );
            }
        };
        let tree_root = self.shared.lock().tree_root;
        let checked = match tree_root {
            Some(root) => {
                self.tree_checks += 1;
                tree::check(&store, root, &acked)
            }
            None => tree::Checked::default(),
        };
        let lost = acked
            .iter()
            .filter(|commit| {
                let tid = Tid(commit.tid);
                let hot_length = self.plan.hot_length;
                let records = commit.records.iter().all(|(oid, data)| {
                    store.load_serial(Oid(*oid), tid).is_ok_and(|stored| {
                        if hot_length && *oid == client::HOT {
                            client::length_value(&stored).is_some()
                        } else {
                            &stored == data
                        }
                    })
                });
                let blobs = commit
                    .blobs
                    .iter()
                    .all(|(oid, bytes)| read_blob(&store, Oid(*oid), tid).as_ref() == Some(bytes));
                let key = commit.tree.as_ref().is_none_or(|insert| {
                    checked
                        .keys
                        .as_ref()
                        .is_some_and(|keys| keys.contains(&insert.key))
                });
                !(records && blobs && key)
            })
            .count();
        let mut findings = findings(&store, &self.disk);
        let mut tids = BTreeSet::new();
        match store.iterator(None, None) {
            Ok(transactions) => {
                for transaction in transactions {
                    match transaction {
                        Ok(transaction) => {
                            tids.insert(transaction.tid.0);
                        }
                        Err(error) => {
                            findings.push(format!("a transaction does not read: {error}"))
                        }
                    }
                }
            }
            Err(error) => findings.push(format!("the transactions do not read: {error}")),
        }
        let length_faults = if self.plan.hot_length {
            length_faults(&store)
        } else {
            Vec::new()
        };
        let mut state = self.shared.lock();
        state.trace(format!("verify: {} acknowledged, {lost} lost", acked.len()));
        for finding in findings.iter().chain(&length_faults).chain(&checked.faults) {
            state.trace(format!("verify: {finding}"));
        }
        (lost, findings, tids, length_faults, checked)
    }

    /// Move the clock to the next timer or lock deadline.
    fn advance(&mut self) {
        let mut state = self.shared.lock();
        let deadline = state
            .threads
            .values()
            .filter_map(|thread| match thread.wait {
                Wait::Parked { deadline, .. } if deadline != Duration::MAX => Some(deadline),
                Wait::Syncing { until, .. } => Some(until),
                _ => None,
            })
            .min();
        let next = match (state.next_timer(), deadline) {
            (Some(timer), Some(deadline)) => timer.min(deadline),
            (Some(at), None) | (None, Some(at)) => at,
            (None, None) => {
                drop(state);
                panic!("nothing can run and nothing waits:\n{}", self.tail());
            }
        };
        state.now = state.now.max(next);
    }

    /// Return the end of the trace, for a failure. A thread that is stuck
    /// can hold the state, so the call does not wait for it.
    fn tail(&self) -> String {
        let Some(state) = self.shared.try_lock() else {
            return "(a stuck thread holds the state of the run)".to_owned();
        };
        let start = state.trace.len().saturating_sub(40);
        state.trace[start..].join("\n")
    }
}

/// Return each break of the history of the hot `Length` of `store`. In tid
/// order, the first record holds 1, and each later record holds the value
/// of the record before it plus 1. Each commit of the oid increments it by
/// 1, so a merge that loses or repeats a change breaks the history.
fn length_faults(store: &Store) -> Vec<String> {
    let oid = Oid(client::HOT);
    let mut history = match store.history(oid, usize::MAX) {
        Ok(history) => history,
        Err(zoe_core::ZoeError::NotFound { .. }) => return Vec::new(),
        Err(error) => {
            return vec![format!(
                "the history of the hot Length does not read: {error}"
            )];
        }
    };
    history.reverse();
    let mut faults = Vec::new();
    for (expected, entry) in (1..).zip(history) {
        match store
            .load_serial(oid, entry.tid)
            .map(|data| client::length_value(&data))
        {
            Ok(Some(value)) if value == expected => {}
            Ok(Some(value)) => faults.push(format!(
                "the hot Length is {value} at tid {:016x}, not {expected}",
                entry.tid.0
            )),
            Ok(None) => faults.push(format!(
                "the record of the hot oid at tid {:016x} is not a Length",
                entry.tid.0
            )),
            Err(error) => faults.push(format!(
                "the record of the hot oid at tid {:016x} does not read: {error}",
                entry.tid.0
            )),
        }
    }
    faults
}

/// Read the whole blob that `tid` wrote for `oid`, or `None` when it does
/// not read or does not match its blob record.
fn read_blob(store: &Store, oid: Oid, tid: Tid) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    store
        .open_blob(oid, tid)
        .ok()?
        .read_to_end(&mut bytes)
        .ok()?;
    Some(bytes)
}

/// Return the findings on `store`, whose files are on `disk`: each finding
/// of the checks of `zoe verify`, and each blob file without its blob
/// record.
fn findings(store: &Store, disk: &MemVfs) -> Vec<String> {
    let report = zoe_convert::verify(store, None);
    let mut findings: Vec<String> = Vec::new();
    findings.extend(report.checksums.iter().map(|p| format!("{p:?}")));
    findings.extend(report.blob_files.iter().map(|p| format!("{p:?}")));
    findings.extend(report.references.iter().map(|p| format!("{p:?}")));
    findings.extend(report.pickles.iter().map(|p| format!("{p:?}")));
    for name in disk.names() {
        let Some(path) = name.strip_prefix("blobs/") else {
            continue;
        };
        let Some(path) = path.strip_suffix(".blob") else {
            continue;
        };
        match blob_ids(path) {
            Some((oid, tid)) if matches!(store.load_blob(oid, tid), Ok(Some(_))) => {}
            Some(_) => findings.push(format!("the blob file {name} has no blob record")),
            None => findings.push(format!("the blob file {name} has no name of a blob")),
        }
    }
    findings
}

/// Return the oid and the tid of a blob file from its path in the blob
/// directory, without `.blob`: eight directories `0xNN`, one per byte of
/// the oid, then the tid in hexadecimal with `0x`.
fn blob_ids(path: &str) -> Option<(Oid, Tid)> {
    let parts: Vec<&str> = path.split('/').collect();
    let [dirs @ .., tid] = parts.as_slice() else {
        return None;
    };
    if dirs.len() != 8 {
        return None;
    }
    let mut oid = 0u64;
    for dir in dirs {
        let byte = u8::from_str_radix(dir.strip_prefix("0x")?, 16).ok()?;
        oid = oid << 8 | u64::from(byte);
    }
    let tid = u64::from_str_radix(tid.strip_prefix("0x")?, 16).ok()?;
    Some((Oid(oid), Tid(tid)))
}

impl Drop for Driver {
    /// Drop the tasks, then end the threads that are left, so that no
    /// thread outlives the run.
    fn drop(&mut self) {
        // After a failure, a thread can be stuck and hold the state: the
        // drop does not wait for it.
        let failed = std::thread::panicking();
        self.tasks.clear();
        let state = if failed {
            self.shared.try_lock()
        } else {
            Some(self.shared.lock())
        };
        if let Some(mut state) = state {
            state.spawned.clear();
        }
        for (_, baton) in std::mem::take(&mut self.batons) {
            let _ = baton.resume.send(Resume::Killed);
            drop(baton.resume);
            if !failed {
                let _ = baton.join.join();
            }
        }
    }
}
