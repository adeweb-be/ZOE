//! A deterministic simulator of the server and its clients (N18).
//!
//! The server and its clients run in one process on a simulated network
//! and a simulated clock. The store is on the `MemVfs` of the core. A seed
//! decides the whole schedule: the order of the tasks and of the threads,
//! the delays, losses, partitions and closes of the network, and the
//! points where the server crashes. A crash drops the server process and
//! opens the store again from a crash image of the `MemVfs`. The same seed
//! gives the same run, event for event, so a failing seed reproduces.
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
mod disk;
mod net;
mod rng;
mod server;
mod world;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, mpsc};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use zoe_core::vfs::MemVfs;
use zoe_core::{Clock, Oid, Options, Store, Tid};

use crate::protocol::{NamedStorage, Queues, STORAGE_NAME, Storages};
use client::SimClient;
use disk::{SimClock, SimVfs};
use rng::Rng;
use world::{Baton, NetFault, Owner, Resume, Shared, Task, TaskWaker, Timer, Wait, Why};

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
/// does not exist, so the server sends no sizes.
const STORE_PATH: &str = "/nonexistent/zoe-sim";

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
    /// A crash is a power loss: the disk keeps the synced writes and a
    /// seeded prefix of the others. When false, a crash is a kill of the
    /// process, and the disk keeps every write.
    pub power_loss: bool,
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
    /// The simulated time of the run.
    pub elapsed: Duration,
}

/// A run of the server and its clients.
#[derive(Clone, Debug)]
pub struct Simulation {
    seed: u64,
    clients: usize,
    commits: usize,
    crashes: usize,
    faults: Faults,
}

impl Simulation {
    /// A run with the seed `seed`: 2 clients, 20 commits, no crash, and
    /// the default faults.
    pub fn new(seed: u64) -> Self {
        Self {
            seed,
            clients: 2,
            commits: 20,
            crashes: 0,
            faults: Faults::default(),
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

    /// Crash the server `n` times, each at a sync of the database file or
    /// up to 30 ms after one.
    #[must_use]
    pub fn crashes(mut self, n: usize) -> Self {
        self.crashes = n;
        self
    }

    /// Inject `faults`.
    #[must_use]
    pub fn faults(mut self, faults: Faults) -> Self {
        self.faults = faults;
        self
    }

    /// Run the simulation to its end.
    pub fn run(self) -> Report {
        Driver::new(self).run()
    }
}

/// When a planned crash happens.
#[derive(Clone, Copy, Debug)]
enum CrashAt {
    /// At the sync of the commit: the commit is not durable yet.
    Sync,
    /// This long after the sync of the commit.
    After(Duration),
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
    /// The planned crashes, by the number of the sync of the database.
    crash_plan: BTreeMap<u64, Vec<CrashAt>>,
    syncs: u64,
    crashes: usize,
    flush_pending: bool,
    /// The numbers of the crash images, apart from the schedule.
    images: Rng,
}

impl Driver {
    fn new(plan: Simulation) -> Self {
        let (send, yields) = mpsc::channel();
        let mut rng = Rng::new(plan.seed.rotate_left(32) ^ 0xC0FF_EE00);
        let span = plan.commits.max(1) as u64;
        let mut crash_plan: BTreeMap<u64, Vec<CrashAt>> = BTreeMap::new();
        for _ in 0..plan.crashes {
            let at = 1 + rng.below(span);
            let when = if rng.chance(0.3) {
                CrashAt::Sync
            } else {
                CrashAt::After(rng.between(Duration::ZERO, Duration::from_millis(30)))
            };
            crash_plan.entry(at).or_default().push(when);
        }
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
        let shared = Shared::new(plan.seed, plan.faults.clone(), fault_plan, send);
        shared.lock().clients = plan.clients;
        Self {
            images: rng.fork(),
            plan,
            shared,
            tasks: BTreeMap::new(),
            batons: BTreeMap::new(),
            yields,
            disk: MemVfs::new(),
            crash_plan,
            syncs: 0,
            crashes: 0,
            flush_pending: false,
        }
    }

    fn run(mut self) -> Report {
        self.start_server("starts");
        let clients = self.plan.clients;
        for id in 0..clients {
            let transactions =
                self.plan.commits / clients + usize::from(id < self.plan.commits % clients);
            let mut state = self.shared.lock();
            let client = SimClient {
                shared: Arc::clone(&self.shared),
                id,
                transactions,
                rng: state.rng.fork(),
                stall: self.plan.faults.stall_client_for.filter(|_| id == 0),
            };
            state.spawn(
                &self.shared.ready,
                Owner::Client(id),
                Box::pin(client.run()),
            );
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
        let lost = self.verify();
        let mut report = self.shared.lock().report();
        report.lost = lost;
        report.crashes = self.crashes;
        report
    }

    /// Open the store on the current disk, and accept connections.
    fn start_server(&mut self, why: &str) {
        let clock = SimClock(Arc::clone(&self.shared));
        let start = clock.now();
        let opened = Store::open_full(SimVfs(self.disk.clone()), clock, Options::default());
        let mut state = self.shared.lock();
        match opened {
            Ok(store) => {
                let storage = NamedStorage::new(
                    STORAGE_NAME,
                    Path::new(STORE_PATH),
                    Arc::new(store),
                    Queues::DEFAULT,
                )
                .started_at(start);
                let storages = Storages::new(vec![Arc::new(storage)], Queues::DEFAULT.client);
                state.server.storages = Some(Arc::new(storages));
                state.server.up = true;
                state.trace(format!("server {why}"));
            }
            Err(error) => {
                state.trace(format!("server {why} and fails: {error}"));
                state.server.halted = true;
            }
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
            Timer::Crash { generation } => {
                let current = state.server.up && state.server.generation == generation;
                drop(state);
                if current {
                    self.crash("crashes after a sync");
                }
                return;
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
                    disk::sync_database(&self.disk);
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
        true
    }

    /// Tell if a thread that waits as `wait` can run at the time `now`.
    fn runnable(&self, now: Duration, wait: Wait) -> bool {
        match wait {
            Wait::Ready => true,
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
    /// something else than a sync.
    fn run_thread(&mut self, id: u64) {
        let mut how = Resume::Go;
        loop {
            match self.resume(id, how) {
                Why::Sync => match self.on_sync() {
                    Some(next) => how = next,
                    None => {
                        self.crash("crashes in a sync");
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
            Why::Sync => {}
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

    /// Count a sync of the database file. Return how the thread goes on,
    /// or `None` when the server crashes now.
    fn on_sync(&mut self) -> Option<Resume> {
        self.syncs += 1;
        let plans = self.crash_plan.remove(&self.syncs).unwrap_or_default();
        let mut state = self.shared.lock();
        let generation = state.server.generation;
        for plan in plans {
            match plan {
                CrashAt::Sync => return None,
                CrashAt::After(delay) => state.after(delay, Timer::Crash { generation }),
            }
        }
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

    /// Crash the server now: keep a crash image of its disk, drop the
    /// process, and start it again 5 to 100 ms later.
    fn crash(&mut self, why: &str) {
        if !self.shared.lock().server.up {
            return;
        }
        self.crashes += 1;
        self.stop(why);
        let mut state = self.shared.lock();
        let delay = state
            .rng
            .between(Duration::from_millis(5), Duration::from_millis(100));
        state.after(delay, Timer::Restart);
    }

    /// Stop the server process as a crash does. With a power loss, the
    /// disk keeps what was synced and a seeded prefix of the rest; with a
    /// kill, it keeps every write. Every connection of the process breaks,
    /// its tasks drop, and its threads end.
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
        let (generation, wakers) = {
            let mut state = self.shared.lock();
            state.trace(format!(
                "server {why}: {} of {} pending writes land",
                landed.0, landed.1
            ));
            let generation = state.server.generation;
            state.server.up = false;
            state.server.storages = None;
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
            let next = self
                .shared
                .lock()
                .threads
                .iter()
                .find(|(_, thread)| thread.generation == generation)
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

    /// Open the disk after the last stop and count the acknowledged
    /// commits that it does not hold.
    fn verify(&mut self) -> usize {
        let acked = self.shared.lock().acked.clone();
        let clock = SimClock(Arc::clone(&self.shared));
        let store = match Store::open_full(SimVfs(self.disk.clone()), clock, Options::default()) {
            Ok(store) => store,
            Err(error) => {
                self.shared
                    .lock()
                    .trace(format!("the store does not open: {error}"));
                return acked.len();
            }
        };
        let lost = acked
            .iter()
            .filter(|commit| {
                commit.records.iter().any(|(oid, data)| {
                    !store
                        .load_serial(Oid(*oid), Tid(commit.tid))
                        .is_ok_and(|stored| &stored == data)
                })
            })
            .count();
        self.shared
            .lock()
            .trace(format!("verify: {} acknowledged, {lost} lost", acked.len()));
        lost
    }

    /// Move the clock to the next timer or lock deadline.
    fn advance(&mut self) {
        let mut state = self.shared.lock();
        let deadline = state
            .threads
            .values()
            .filter_map(|thread| match thread.wait {
                Wait::Parked { deadline, .. } if deadline != Duration::MAX => Some(deadline),
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
