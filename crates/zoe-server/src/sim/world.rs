//! The shared state of a run: the simulated time, the timers, the tasks
//! that are ready, the threads and the connections.
//!
//! One thread runs at a time. The driver polls the tasks and hands the
//! baton to one thread at a time. A thread gives the baton back when it
//! waits on a waiter (for a channel or for a lock), when it syncs the
//! database file, and when it ends. So the schedule depends only on the
//! seed.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, mpsc};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::JoinHandle;
use std::time::Duration;

use zoe_core::Store;

use super::net::Conn;
use super::pool::PoolLog;
use super::rng::Rng;
use super::{Faults, Report};
use crate::protocol::Storages;

/// A task of the simulation.
pub(crate) type Task = Pin<Box<dyn Future<Output = ()> + Send>>;

/// The actor that a task belongs to. A crash drops the tasks of its
/// server generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Owner {
    Client(usize),
    Server(u32),
}

/// Why a thread gives the baton back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Why {
    /// It waits on the waiter `waiter` until a wake or the deadline.
    Parked { waiter: u64, deadline: Duration },
    /// It syncs the database file. `grows` is true when the sync follows a
    /// change of the length of the file with no write between: redb grows
    /// the file under a lock of its own, so other threads must not run.
    Sync { grows: bool },
    /// It ended.
    Exited,
}

/// What the driver tells a thread when it hands the baton back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Resume {
    /// Go on.
    Go,
    /// Go on, but skip the sync: the deliberate durability bug.
    Defer,
    /// The process of the thread is dead: end.
    Killed,
}

/// A thread as the driver sees it.
#[derive(Debug)]
pub(crate) struct ThreadState {
    /// The server generation of the thread.
    pub(crate) generation: u32,
    /// The client of the connection of the thread, for the trace.
    pub(crate) client: Option<usize>,
    pub(crate) wait: Wait,
}

/// Whether a thread can run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Wait {
    /// It can run: it is new, or the driver resumes it.
    Ready,
    /// It waits on a waiter until a wake or the deadline.
    Parked { waiter: u64, deadline: Duration },
    /// It syncs the database file until `until`, and then goes on with
    /// `resume`. Other tasks and threads run meanwhile.
    Syncing { until: Duration, resume: Resume },
}

/// An event at a time of the simulated clock.
pub(crate) enum Timer {
    /// Wake a task.
    Wake(Waker),
    /// Bytes reach one end of a connection.
    Deliver {
        conn: u64,
        to_client: bool,
        bytes: Vec<u8>,
    },
    /// The end of the stream reaches one end of a connection.
    Eof { conn: u64, to_client: bool },
    /// A lost frame breaks a connection.
    Reset { conn: u64 },
    /// The partition of a client ends.
    Heal { client: usize },
    /// The server starts again.
    Restart,
    /// The late sync of the deliberate durability bug.
    Flush { generation: u32 },
}

/// The driver side of a thread.
pub(crate) struct Baton {
    pub(crate) resume: mpsc::Sender<Resume>,
    pub(crate) join: JoinHandle<()>,
}

/// The server as the clients and the connections see it.
#[derive(Default)]
pub(crate) struct ServerState {
    /// The generation of the server process. A crash starts the next one.
    pub(crate) generation: u32,
    pub(crate) up: bool,
    pub(crate) storages: Option<Arc<Storages>>,
    /// The store of the server process, while it is up.
    pub(crate) store: Option<Arc<Store>>,
    /// The store did not open again: the run stops.
    pub(crate) halted: bool,
}

/// A commit that a client saw acknowledged: its tid, its records and its
/// blobs. A commit of a split writer has only the records that its vote
/// did not resolve, and its insert into the tree.
#[derive(Clone, Debug)]
pub(crate) struct Acked {
    pub(crate) tid: u64,
    pub(crate) records: Vec<(u64, Vec<u8>)>,
    pub(crate) blobs: Vec<(u64, Vec<u8>)>,
    pub(crate) tree: Option<Box<TreeCommit>>,
}

/// The insert of a split writer into the tree (`Simulation::split_merges`).
#[derive(Clone, Debug)]
pub(crate) struct TreeCommit {
    /// The key that the transaction inserted.
    pub(crate) key: i128,
    /// The oids that the transaction stored.
    pub(crate) stored: Vec<u64>,
    /// The new bucket that the transaction stored, when it split one.
    pub(crate) created: Vec<u64>,
    /// The oids that the vote resolved.
    pub(crate) resolved: Vec<u64>,
}

impl TreeCommit {
    /// Tell if the vote wrote an oid that the transaction did not store.
    pub(crate) fn wrote_unstored(&self) -> bool {
        self.resolved.iter().any(|oid| !self.stored.contains(oid))
    }
}

/// The phase of a commit when the server crashes.
pub(crate) mod phase {
    /// The client stored its records and did not vote yet.
    pub(crate) const STAGED: &str = "staged";
    /// The vote passed: the server holds the locks, and the client did not
    /// send `tpc_finish` yet.
    pub(crate) const LOCKED: &str = "locked";
    /// The server syncs the database file in `tpc_finish`.
    pub(crate) const FINISHING: &str = "finishing";
    /// The client got the reply of `tpc_finish`.
    pub(crate) const ACKNOWLEDGED: &str = "acknowledged";
    /// Every phase, in the order of a commit.
    pub(crate) const ALL: [&str; 4] = [STAGED, LOCKED, FINISHING, ACKNOWLEDGED];
}

/// A planned crash: the server crashes at the first commit that reaches
/// `phase` after the clients saw `after` commits acknowledged.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PlannedCrash {
    pub(crate) phase: &'static str,
    pub(crate) after: usize,
}

/// The mutable state of a run.
pub(crate) struct State {
    pub(crate) now: Duration,
    /// The numbers of the schedule and of the network.
    pub(crate) rng: Rng,
    pub(crate) faults: Faults,
    /// The longest one-way delay of a frame.
    pub(crate) delay: Duration,
    seq: u64,
    timers: BTreeMap<(Duration, u64), Timer>,
    next_task: u64,
    pub(crate) spawned: Vec<(u64, Owner, Task)>,
    pub(crate) threads: BTreeMap<u64, ThreadState>,
    next_thread: u64,
    pub(crate) new_batons: Vec<(u64, Baton)>,
    yields: mpsc::Sender<(u64, Why)>,
    next_waiter: u64,
    pub(crate) conns: BTreeMap<u64, Conn>,
    next_conn: u64,
    /// The end of the partition of each client that is cut from the
    /// server.
    pub(crate) partitions: BTreeMap<usize, Duration>,
    /// The number of clients of the run.
    pub(crate) clients: usize,
    /// The frames that the network carried, and the frame counts at which
    /// a partition or a close starts.
    pub(crate) frames: u64,
    pub(crate) fault_plan: BTreeMap<u64, Vec<NetFault>>,
    pub(crate) server: ServerState,
    pub(crate) trace: Vec<String>,
    pub(crate) acked: Vec<Acked>,
    /// The newest acknowledged tid of each oid.
    pub(crate) known: BTreeMap<u64, u64>,
    pub(crate) stale_reads: usize,
    pub(crate) clients_done: usize,
    /// The crashes that did not happen yet.
    pub(crate) crash_plan: Vec<PlannedCrash>,
    /// The phase of a crash that a client reached. The driver crashes the
    /// server after the step of that client.
    pub(crate) crash_now: Option<&'static str>,
    /// The listeners of pools that the network can still break.
    pub(crate) listener_losses: usize,
    /// What the pools of the run delivered.
    pub(crate) pools: Pools,
    /// The node of the tree of the split writers, when the run has them.
    pub(crate) tree_root: Option<u64>,
    /// The transactions of the split writers whose vote wrote an oid that
    /// they did not store, and whose `tpc_finish` got no reply.
    pub(crate) interrupted_split_finishes: usize,
    /// The votes of the split writers that split a bucket and got no
    /// reply.
    pub(crate) interrupted_split_votes: usize,
}

/// What the pools of a run delivered, and the counts of their holds.
#[derive(Debug, Default)]
pub(crate) struct Pools {
    /// The log of each pool, at its close.
    pub(crate) logs: Vec<PoolLog>,
    /// The tids of other clients that the pools delivered.
    pub(crate) foreign: usize,
    /// The tids that a listener held.
    pub(crate) held: usize,
    /// The finishes whose reply came while their pool held a lower tid.
    pub(crate) waited: usize,
    /// The finishes that had their reply when their listener was lost, and
    /// returned their tid.
    pub(crate) lost_listener_finishes: usize,
}

/// A fault of the network that starts at a planned frame.
#[derive(Clone, Copy, Debug)]
pub(crate) enum NetFault {
    /// Cut a client from the server for a time.
    Partition,
    /// Close a connection.
    Close,
}

impl State {
    /// Add `text` to the trace, at the time now.
    pub(crate) fn trace(&mut self, text: impl AsRef<str>) {
        let line = format!("{:>12} {}", self.now.as_micros(), text.as_ref());
        self.trace.push(line);
    }

    /// Run `timer` at `at`. Timers at the same time run in the order of
    /// their creation.
    pub(crate) fn at(&mut self, at: Duration, timer: Timer) {
        self.seq += 1;
        self.timers.insert((at, self.seq), timer);
    }

    /// Run `timer` after `delay`.
    pub(crate) fn after(&mut self, delay: Duration, timer: Timer) {
        let at = self.now.saturating_add(delay);
        self.at(at, timer);
    }

    /// Remove and return the first timer that is due.
    pub(crate) fn due(&mut self) -> Option<Timer> {
        let key = *self.timers.keys().next()?;
        if key.0 > self.now {
            return None;
        }
        self.timers.remove(&key)
    }

    /// Return the time of the next timer.
    pub(crate) fn next_timer(&self) -> Option<Duration> {
        self.timers.keys().next().map(|key| key.0)
    }

    /// Spawn `task` for `owner`. The driver adopts it after the current
    /// step.
    pub(crate) fn spawn(&mut self, ready: &Ready, owner: Owner, task: Task) {
        self.next_task += 1;
        let id = self.next_task;
        self.spawned.push((id, owner, task));
        ready.insert(id);
    }

    pub(crate) fn new_waiter(&mut self) -> u64 {
        self.next_waiter += 1;
        self.next_waiter
    }

    pub(crate) fn new_conn(&mut self, conn: Conn) -> u64 {
        self.next_conn += 1;
        self.conns.insert(self.next_conn, conn);
        self.next_conn
    }

    /// Return a one-way delay of the network.
    pub(crate) fn latency(&mut self) -> Duration {
        let low = Duration::from_micros(20);
        let high = self.delay.max(low);
        self.rng.between(low, high)
    }

    /// Return the end of the partition of `client`, if it is cut now.
    pub(crate) fn partitioned(&self, client: usize) -> Option<Duration> {
        self.partitions
            .get(&client)
            .copied()
            .filter(|until| *until > self.now)
    }

    /// Tell if a commit that reaches `phase` now crashes the server, and
    /// take that crash from the plan.
    pub(crate) fn crash_at(&mut self, phase: &'static str) -> bool {
        if !self.server.up || self.crash_now.is_some() {
            return false;
        }
        let acked = self.acked.len();
        let Some(i) = self
            .crash_plan
            .iter()
            .position(|plan| plan.phase == phase && plan.after <= acked)
        else {
            return false;
        };
        self.crash_plan.remove(i);
        true
    }

    /// Return the report of the run so far.
    pub(crate) fn report(&mut self) -> Report {
        Report {
            trace: std::mem::take(&mut self.trace),
            acknowledged: self.acked.len(),
            lost: 0,
            stale_reads: self.stale_reads,
            crashes: 0,
            crash_phases: Vec::new(),
            verify: Vec::new(),
            elapsed: self.now,
            order_faults: Vec::new(),
            foreign_deliveries: self.pools.foreign,
            own_deliveries: 0,
            held: self.pools.held,
            waited_finishes: self.pools.waited,
            lost_listener_finishes: self.pools.lost_listener_finishes,
            length_faults: Vec::new(),
            tree_faults: Vec::new(),
            tree_checks: 0,
            unstored_writes: self
                .acked
                .iter()
                .filter(|acked| {
                    acked
                        .tree
                        .as_ref()
                        .is_some_and(|commit| commit.wrote_unstored())
                })
                .count(),
            dropped: 0,
            interrupted_split_finishes: self.interrupted_split_finishes,
            interrupted_split_votes: self.interrupted_split_votes,
            handoffs: 0,
            syncs: 0,
            synced_commits: 0,
        }
    }
}

/// A set of ids that any thread fills. It has its own mutex, so a waker
/// never takes the mutex of the state.
#[derive(Default)]
pub(crate) struct IdSet(Mutex<BTreeSet<u64>>);

impl IdSet {
    fn lock(&self) -> MutexGuard<'_, BTreeSet<u64>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn insert(&self, id: u64) {
        self.lock().insert(id);
    }

    pub(crate) fn take(&self, id: u64) -> bool {
        self.lock().remove(&id)
    }

    pub(crate) fn contains(&self, id: u64) -> bool {
        self.lock().contains(&id)
    }

    pub(crate) fn list(&self) -> Vec<u64> {
        self.lock().iter().copied().collect()
    }
}

/// The tasks that are ready to run, in the order of their ids.
pub(crate) type Ready = IdSet;

/// The state of a run, shared by the driver, the tasks and the threads.
pub(crate) struct Shared {
    state: Mutex<State>,
    pub(crate) ready: Ready,
    /// The waiters that got a wake and did not use it yet.
    pub(crate) permits: IdSet,
}

impl Shared {
    pub(crate) fn new(
        seed: u64,
        faults: Faults,
        fault_plan: BTreeMap<u64, Vec<NetFault>>,
        crash_plan: Vec<PlannedCrash>,
        yields: mpsc::Sender<(u64, Why)>,
    ) -> Arc<Self> {
        let delay = faults.delay.unwrap_or(super::DEFAULT_DELAY);
        Arc::new(Self {
            state: Mutex::new(State {
                now: Duration::ZERO,
                rng: Rng::new(seed),
                faults,
                delay,
                seq: 0,
                timers: BTreeMap::new(),
                next_task: 0,
                spawned: Vec::new(),
                threads: BTreeMap::new(),
                next_thread: 0,
                new_batons: Vec::new(),
                yields,
                next_waiter: 0,
                conns: BTreeMap::new(),
                next_conn: 0,
                partitions: BTreeMap::new(),
                clients: 0,
                frames: 0,
                fault_plan,
                server: ServerState::default(),
                trace: Vec::new(),
                acked: Vec::new(),
                known: BTreeMap::new(),
                stale_reads: 0,
                clients_done: 0,
                crash_plan,
                crash_now: None,
                listener_losses: 0,
                pools: Pools::default(),
                tree_root: None,
                interrupted_split_finishes: 0,
                interrupted_split_votes: 0,
            }),
            ready: Ready::default(),
            permits: IdSet::default(),
        })
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Return the state if no other thread holds it.
    pub(crate) fn try_lock(&self) -> Option<MutexGuard<'_, State>> {
        match self.state.try_lock() {
            Ok(state) => Some(state),
            Err(std::sync::TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => None,
        }
    }

    /// Return the time now.
    pub(crate) fn now(&self) -> Duration {
        self.lock().now
    }
}

/// The waker of a task: it marks the task ready.
pub(crate) struct TaskWaker {
    pub(crate) id: u64,
    pub(crate) shared: Arc<Shared>,
}

impl Wake for TaskWaker {
    fn wake(self: Arc<Self>) {
        self.shared.ready.insert(self.id);
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.shared.ready.insert(self.id);
    }
}

/// A future that ends at a time of the simulated clock.
pub(crate) struct Sleep {
    shared: Arc<Shared>,
    until: Duration,
    armed: bool,
}

impl Future for Sleep {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let mut state = self.shared.lock();
        if state.now >= self.until {
            return Poll::Ready(());
        }
        if !self.armed {
            state.at(self.until, Timer::Wake(cx.waker().clone()));
            drop(state);
            self.armed = true;
        }
        Poll::Pending
    }
}

/// Sleep for `delay` of simulated time.
pub(crate) fn sleep(shared: &Arc<Shared>, delay: Duration) -> Sleep {
    let until = shared.now().saturating_add(delay);
    Sleep {
        shared: Arc::clone(shared),
        until,
        armed: false,
    }
}

/// The context of a thread of the simulation.
struct BatonContext {
    id: u64,
    generation: u32,
    /// The driver told the thread that its process is dead.
    killed: Cell<bool>,
    yields: mpsc::Sender<(u64, Why)>,
    resume: mpsc::Receiver<Resume>,
}

thread_local! {
    static BATON: RefCell<Option<BatonContext>> = const { RefCell::new(None) };
}

/// Give the baton back to the driver with `why`, and wait until the
/// driver hands it back. Only a thread of the simulation calls it.
pub(crate) fn give_back(why: Why) -> Resume {
    BATON.with_borrow(|context| {
        let context = context
            .as_ref()
            .expect("only a thread of the simulation gives the baton back");
        if context.killed.get() {
            return Resume::Killed;
        }
        let resume = match context.yields.send((context.id, why)) {
            Ok(()) => context.resume.recv().unwrap_or(Resume::Killed),
            Err(_) => Resume::Killed,
        };
        if resume == Resume::Killed {
            context.killed.set(true);
        }
        resume
    })
}

/// Return the server generation of the thread that calls it, or `None` on
/// a thread that is not a thread of the simulation.
pub(crate) fn thread_generation() -> Option<u32> {
    BATON.with_borrow(|context| context.as_ref().map(|context| context.generation))
}

/// Tell if the driver told the thread that calls it that its process is
/// dead. From then on, the thread gives the baton back no more: each of
/// its waits ends at once.
pub(crate) fn killed() -> bool {
    BATON.with_borrow(|context| context.as_ref().is_some_and(|context| context.killed.get()))
}

/// Start a thread of the server generation `generation` for a connection
/// of `client`. The thread waits for the baton before it runs `work`. It
/// waits for its channels on a waiter of the simulation. When `work`
/// returns, the thread tells the driver that it ended.
pub(crate) fn spawn_thread(
    shared: &Arc<Shared>,
    generation: u32,
    client: Option<usize>,
    work: Box<dyn FnOnce() + Send>,
) {
    let mut state = shared.lock();
    state.next_thread += 1;
    let id = state.next_thread;
    let waiter = super::disk::SimWaiter::new(&mut state, shared);
    let (resume_send, resume) = mpsc::channel();
    let yields = state.yields.clone();
    state.threads.insert(
        id,
        ThreadState {
            generation,
            client,
            wait: Wait::Ready,
        },
    );
    let join = std::thread::Builder::new()
        .name(format!("zoe-sim-thread-{id}"))
        .spawn(move || {
            let context = BatonContext {
                id,
                generation,
                killed: Cell::new(false),
                yields,
                resume,
            };
            let first = context.resume.recv().unwrap_or(Resume::Killed);
            context.killed.set(first == Resume::Killed);
            BATON.with_borrow_mut(|slot| *slot = Some(context));
            crate::runtime::set_waiter(Arc::new(waiter));
            // The guard tells the driver that the thread ended, also when
            // the work panics.
            struct Ended(u64);
            impl Drop for Ended {
                fn drop(&mut self) {
                    BATON.with_borrow(|context| {
                        if let Some(context) = context {
                            let _ = context.yields.send((self.0, Why::Exited));
                        }
                    });
                }
            }
            let _ended = Ended(id);
            if first == Resume::Killed {
                drop(work);
            } else {
                work();
            }
        })
        .expect("a thread of the simulation starts");
    state.new_batons.push((
        id,
        Baton {
            resume: resume_send,
            join,
        },
    ));
}
