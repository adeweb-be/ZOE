//! A deterministic simulator of the server and its clients.
//!
//! The server and its clients run in one process on a simulated network
//! and a simulated clock. The store is on the `MemVfs` of the core. A seed
//! decides the whole schedule: the order of the tasks and of the session
//! threads, the delays and losses of the network, and the points where
//! the server crashes. A crash drops the server process and reopens the
//! store from a crash image of the `MemVfs`. The same seed gives the same
//! run, event for event, so a failing seed reproduces.
//!
//! The driver is an executor of its own. It polls the tasks of the
//! clients and of the connections, and it runs the session threads of the
//! server one at a time. A session thread runs the session of the server
//! and the core as they are. It gives the baton back when it waits for a
//! call, when the core waits for a lock (through the waiter of the store
//! clock), when the core syncs the database file, and when it ends.
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
use zoe_core::{Oid, Options, Store, Tid};

use crate::protocol::{NamedStorage, STORAGE_NAME};
use disk::{SimClock, SimVfs};
use rng::Rng;
use world::{Baton, Owner, Resume, Shared, Task, TaskWaker, Timer, Wait, Why};

/// The most steps of one run. A run that needs more is a bug of the
/// simulator.
const MAX_STEPS: u64 = 50_000_000;

/// The faults that a run injects.
#[derive(Clone, Debug)]
pub struct Faults {
    /// The longest one-way delay of a frame on the network.
    pub delay: Duration,
    /// The chance that a frame is lost. A lost frame breaks its
    /// connection.
    pub loss: f64,
    /// The first client stalls this long after its first vote, before its
    /// `tpc_finish`, and keeps its locks.
    pub stall_client_for: Option<Duration>,
    /// A deliberate bug for the tests: the server acknowledges
    /// `tpc_finish` before the commit is durable. The sync of the commit
    /// happens later.
    pub ack_before_durable: bool,
}

impl Default for Faults {
    fn default() -> Self {
        Self {
            delay: Duration::from_millis(2),
            loss: 0.002,
            stall_client_for: None,
            ack_before_durable: false,
        }
    }
}

/// What a run gives.
#[derive(Clone, Debug, Default)]
pub struct Report {
    /// The events of the run, in order, as text.
    pub trace: Vec<String>,
    /// The commits that a client saw acknowledged.
    pub acknowledged: usize,
    /// The acknowledged commits that the reopened store does not hold.
    pub lost: usize,
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

    /// Run `n` clients.
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

    /// Crash the server `n` times.
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
    store: Option<Arc<Store>>,
    /// The planned crashes, by the number of the database sync.
    crash_plan: BTreeMap<u64, Vec<CrashAt>>,
    syncs: u64,
    crashes: usize,
    flush_pending: bool,
    /// The random numbers of the crash images, apart from the schedule.
    images: Rng,
}

impl Driver {
    fn new(plan: Simulation) -> Self {
        let (send, yields) = mpsc::channel();
        let shared = Shared::new(plan.seed, plan.faults.clone(), send);
        let mut rng = Rng::new(plan.seed.rotate_left(32) ^ 0xC0FF_EE00);
        let mut crash_plan: BTreeMap<u64, Vec<CrashAt>> = BTreeMap::new();
        let span = plan.commits.max(1) as u64;
        for _ in 0..plan.crashes {
            let at = 1 + rng.below(span);
            let when = if rng.chance(0.3) {
                CrashAt::Sync
            } else {
                CrashAt::After(rng.between(Duration::ZERO, Duration::from_millis(30)))
            };
            crash_plan.entry(at).or_default().push(when);
        }
        Self {
            images: rng.fork(),
            plan,
            shared,
            tasks: BTreeMap::new(),
            batons: BTreeMap::new(),
            yields,
            disk: MemVfs::new(),
            store: None,
            crash_plan,
            syncs: 0,
            crashes: 0,
            flush_pending: false,
        }
    }

    fn run(mut self) -> Report {
        self.start_server("start");
        let clients = self.plan.clients;
        for client in 0..clients {
            let share =
                self.plan.commits / clients + usize::from(client < self.plan.commits % clients);
            let mut state = self.shared.lock();
            let rng = state.rng.fork();
            let stall = if client == 0 {
                self.plan.faults.stall_client_for
            } else {
                None
            };
            let task = Box::pin(client::run(
                Arc::clone(&self.shared),
                client,
                share,
                rng,
                stall,
            ));
            state.spawn(&self.shared.ready, Owner::Client(client), task);
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
        // The end of the run is a power loss too. Then the store reopens
        // from what is durable, and every acknowledged commit must be there.
        self.stop("end");
        let lost = self.verify();
        let mut state = self.shared.lock();
        Report {
            trace: std::mem::take(&mut state.trace),
            acknowledged: state.acked.len(),
            lost,
            crashes: self.crashes,
            elapsed: state.now,
        }
    }

    /// Open the store on the current disk and accept connections.
    fn start_server(&mut self, why: &str) {
        let clock = SimClock(Arc::clone(&self.shared));
        let opened = Store::open_full(SimVfs(self.disk.clone()), clock, Options::default());
        let mut state = self.shared.lock();
        match opened {
            Ok(store) => {
                let store = Arc::new(store);
                let storage = NamedStorage::new(STORAGE_NAME, Path::new("sim"), Arc::clone(&store));
                state.server.storage = Some(Arc::new(storage));
                state.server.up = true;
                state.trace(format!("server {why}"));
                self.store = Some(store);
            }
            Err(error) => {
                state.trace(format!("server {why} fails: {error}"));
                state.server.halted = true;
            }
        }
    }

    /// Move the tasks and the session threads that the last step started
    /// into the driver.
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
        match timer {
            Timer::Wake(waker) => waker.wake(),
            Timer::Deliver {
                conn,
                to_client,
                bytes,
            } => {
                if let Some(waker) = net::deliver(&mut state, conn, to_client, bytes) {
                    waker.wake();
                }
            }
            Timer::Eof { conn, to_client } => {
                if let Some(waker) = net::eof(&mut state, conn, to_client) {
                    waker.wake();
                }
            }
            Timer::Reset { conn } => {
                if let Some(client) = state
                    .conns
                    .get(&conn)
                    .filter(|c| !c.reset)
                    .map(|c| c.client)
                {
                    state.trace(format!(
                        "net loses a frame of c{client}: connection {conn} breaks"
                    ));
                }
                for waker in net::reset(&mut state, conn) {
                    waker.wake();
                }
            }
            Timer::Crash { generation } => {
                let current = state.server.up && state.server.generation == generation;
                drop(state);
                if current {
                    self.crash("crash after a sync");
                }
            }
            Timer::Restart => {
                drop(state);
                self.start_server("restart");
            }
            Timer::Flush { generation } => {
                self.flush_pending = false;
                if state.server.up && state.server.generation == generation {
                    state.trace("server syncs late");
                    drop(state);
                    disk::sync_database(&self.disk);
                }
            }
        }
    }

    /// Run one task or one session thread that can run, chosen by the
    /// seed. Return false when nothing can run.
    fn step(&mut self) -> bool {
        let tasks = self.shared.ready.list();
        let threads: Vec<u64> = {
            let state = self.shared.lock();
            state
                .threads
                .iter()
                .filter(|(_, thread)| runnable(&state, thread))
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

    /// Hand the baton to the session thread `id` until it gives it back
    /// with something else than a sync.
    fn run_thread(&mut self, id: u64) {
        let mut how = Resume::Go;
        loop {
            match self.resume(id, how) {
                Why::Sync => match self.on_sync() {
                    Some(next) => how = next,
                    None => {
                        self.crash("crash in a sync");
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
        let baton = self.batons.get(&id).expect("a session thread");
        baton.resume.send(how).expect("the session thread waits");
        let (from, why) = self.yields.recv().expect("the session thread answers");
        assert_eq!(from, id, "only one session thread runs at a time");
        let mut state = self.shared.lock();
        match why {
            Why::Idle => {
                if let Some(thread) = state.threads.get_mut(&id) {
                    thread.wait = Wait::Idle;
                }
            }
            Why::Parked { waiter, deadline } => {
                if let Some(thread) = state.threads.get_mut(&id) {
                    thread.wait = Wait::Parked { waiter, deadline };
                    let client = thread.client;
                    let wait = deadline.saturating_sub(state.now);
                    state.trace(format!(
                        "s  c{client} waits for a lock, {} ms at most",
                        wait.as_millis()
                    ));
                }
            }
            Why::Sync => {}
            Why::Exited => {
                state.threads.remove(&id);
                drop(state);
                let baton = self.batons.remove(&id).expect("a session thread");
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
    /// process, and restart it later.
    fn crash(&mut self, why: &str) {
        if !self.shared.lock().server.up {
            return;
        }
        self.crashes += 1;
        self.stop(why);
        let delay = self
            .shared
            .lock()
            .rng
            .between(Duration::from_millis(5), Duration::from_millis(100));
        self.shared.lock().after(delay, Timer::Restart);
    }

    /// Stop the server process as a power loss does. The disk keeps what
    /// was synced and a seeded prefix of the rest. Every connection breaks,
    /// the tasks of the process drop, and its session threads end.
    fn stop(&mut self, why: &str) {
        let draw = self.images.next_u64();
        let mut landed = (0, 0);
        let image = self.disk.crash_now(|pending| {
            let n = (draw % (pending as u64 + 1)) as usize;
            landed = (n, pending);
            n
        });
        let generation = {
            let mut state = self.shared.lock();
            state.trace(format!(
                "server {why}: {} of {} pending writes land",
                landed.0, landed.1
            ));
            let generation = state.server.generation;
            state.server.up = false;
            state.server.storage = None;
            state.server.generation += 1;
            let conns: Vec<u64> = state
                .conns
                .iter()
                .filter(|(_, conn)| conn.generation == generation)
                .map(|(id, _)| *id)
                .collect();
            for conn in conns {
                for waker in net::reset(&mut state, conn) {
                    waker.wake();
                }
            }
            generation
        };
        self.tasks
            .retain(|_, (owner, _)| *owner != Owner::Server(generation));
        // The threads of the dead process end. Each one sees that it is
        // killed, at its next call, lock wait or sync.
        loop {
            let next = self
                .shared
                .lock()
                .threads
                .iter()
                .find(|(_, thread)| thread.generation == generation)
                .map(|(id, _)| *id);
            let Some(id) = next else { break };
            self.resume(id, Resume::Killed);
        }
        self.shared.lock().server.storage = None;
        self.store = None;
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
                    .trace(format!("the store does not reopen: {error}"));
                return acked.len();
            }
        };
        let lost = acked
            .iter()
            .filter(|commit| {
                commit.records.iter().any(|(oid, data)| {
                    store
                        .load_serial(Oid(*oid), Tid(commit.tid))
                        .map_or(true, |stored| &stored != data)
                })
            })
            .count();
        let mut state = self.shared.lock();
        state.trace(format!("verify: {} acknowledged, {lost} lost", acked.len()));
        lost
    }

    /// Move the clock to the next timer or lock deadline.
    fn advance(&mut self) {
        let mut state = self.shared.lock();
        let deadline = state
            .threads
            .values()
            .filter_map(|thread| match thread.wait {
                Wait::Parked { deadline, .. } => Some(deadline),
                Wait::Idle => None,
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

    /// Return the end of the trace, for a failure.
    fn tail(&self) -> String {
        let state = self.shared.lock();
        let start = state.trace.len().saturating_sub(40);
        state.trace[start..].join("\n")
    }
}

/// Tell if a session thread can run: it has a call or its inbox closed,
/// or its lock wait ended.
fn runnable(state: &world::State, thread: &world::ThreadState) -> bool {
    match thread.wait {
        Wait::Idle => !thread.inbox.is_empty() || thread.closed,
        Wait::Parked { waiter, deadline } => {
            state.permits.contains(&waiter) || state.now >= deadline
        }
    }
}

impl Drop for Driver {
    /// End the session threads that are left, so that no thread outlives
    /// the run.
    fn drop(&mut self) {
        let ids: Vec<u64> = self.batons.keys().copied().collect();
        for id in ids {
            if let Some(baton) = self.batons.remove(&id) {
                let _ = baton.resume.send(Resume::Killed);
                drop(baton.resume);
                let _ = baton.join.join();
            }
        }
    }
}
