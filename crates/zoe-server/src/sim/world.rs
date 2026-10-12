//! The shared state of a run: the simulated time, the timers, the tasks
//! that are ready, the session threads and the connections.
//!
//! One thread runs at a time. The driver runs the tasks and hands the
//! baton to one session thread at a time. A session thread gives the
//! baton back when it waits for a call, when it waits for a lock, when it
//! syncs the database file, and when it ends. So the schedule depends only
//! on the seed.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, mpsc};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::JoinHandle;
use std::time::Duration;

use super::Faults;
use super::net::Conn;
use super::rng::Rng;
use crate::protocol::NamedStorage;
use crate::protocol::message::Call;

/// A task of the simulation.
pub(crate) type Task = Pin<Box<dyn Future<Output = ()> + Send>>;

/// The actor that a task belongs to. A crash drops the tasks of its
/// server generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Owner {
    Client(usize),
    Server(u32),
}

/// What a session thread does when it gives the baton back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Why {
    /// It waits for a call.
    Idle,
    /// It waits for a lock, until the waiter wakes or the deadline.
    Parked { waiter: u64, deadline: Duration },
    /// It syncs the database file.
    Sync,
    /// It ended.
    Exited,
}

/// What the driver tells a session thread when it hands the baton back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Resume {
    /// Go on.
    Go,
    /// Go on, but skip the sync: the deliberate durability bug.
    Defer,
    /// The process is dead: end.
    Killed,
}

/// The state of a session thread, as the driver sees it.
#[derive(Debug)]
pub(crate) struct ThreadState {
    pub(crate) generation: u32,
    pub(crate) client: usize,
    pub(crate) wait: Wait,
    pub(crate) inbox: VecDeque<Call>,
    pub(crate) closed: bool,
}

/// Why a session thread does not run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Wait {
    /// It waits for its first call, or for the next one.
    Idle,
    /// It waits for a lock.
    Parked { waiter: u64, deadline: Duration },
}

/// An event at a time of the simulated clock.
pub(crate) enum Timer {
    Wake(Waker),
    Deliver {
        conn: u64,
        to_client: bool,
        bytes: Vec<u8>,
    },
    Eof {
        conn: u64,
        to_client: bool,
    },
    Reset {
        conn: u64,
    },
    Crash {
        generation: u32,
    },
    Restart,
    Flush {
        generation: u32,
    },
}

/// The driver side of a session thread.
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
    pub(crate) storage: Option<Arc<NamedStorage>>,
    /// The reopen after a crash failed: the run stops.
    pub(crate) halted: bool,
}

/// A commit that a client saw acknowledged: its tid and its records.
#[derive(Clone, Debug)]
pub(crate) struct Acked {
    pub(crate) tid: u64,
    pub(crate) records: Vec<(u64, Vec<u8>)>,
}

pub(crate) struct State {
    pub(crate) now: Duration,
    pub(crate) rng: Rng,
    pub(crate) faults: Faults,
    seq: u64,
    timers: BTreeMap<(Duration, u64), Timer>,
    next_task: u64,
    pub(crate) spawned: Vec<(u64, Owner, Task)>,
    pub(crate) threads: BTreeMap<u64, ThreadState>,
    next_thread: u64,
    pub(crate) new_batons: Vec<(u64, Baton)>,
    pub(crate) yields: mpsc::Sender<(u64, Why)>,
    pub(crate) permits: BTreeSet<u64>,
    next_waiter: u64,
    pub(crate) conns: BTreeMap<u64, Conn>,
    next_conn: u64,
    pub(crate) server: ServerState,
    pub(crate) trace: Vec<String>,
    pub(crate) acked: Vec<Acked>,
    pub(crate) clients_done: usize,
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
        let at = self.now + delay;
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
        let high = self.faults.delay.max(Duration::from_micros(20));
        self.rng.between(Duration::from_micros(20), high)
    }
}

/// The tasks that are ready to run. Wakers fill it from any thread, so
/// it has its own mutex.
#[derive(Default)]
pub(crate) struct Ready(Mutex<BTreeSet<u64>>);

impl Ready {
    pub(crate) fn insert(&self, task: u64) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(task);
    }

    pub(crate) fn take(&self, task: u64) -> bool {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&task)
    }

    pub(crate) fn list(&self) -> Vec<u64> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .copied()
            .collect()
    }
}

/// The state of a run, shared by the driver, the tasks and the session
/// threads.
pub(crate) struct Shared {
    state: Mutex<State>,
    pub(crate) ready: Ready,
}

impl Shared {
    pub(crate) fn new(seed: u64, faults: Faults, yields: mpsc::Sender<(u64, Why)>) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                now: Duration::ZERO,
                rng: Rng::new(seed),
                faults,
                seq: 0,
                timers: BTreeMap::new(),
                next_task: 0,
                spawned: Vec::new(),
                threads: BTreeMap::new(),
                next_thread: 0,
                new_batons: Vec::new(),
                yields,
                permits: BTreeSet::new(),
                next_waiter: 0,
                conns: BTreeMap::new(),
                next_conn: 0,
                server: ServerState::default(),
                trace: Vec::new(),
                acked: Vec::new(),
                clients_done: 0,
            }),
            ready: Ready::default(),
        })
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
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
    let until = shared.now() + delay;
    Sleep {
        shared: Arc::clone(shared),
        until,
        armed: false,
    }
}

/// The context of a session thread.
struct BatonContext {
    id: u64,
    generation: u32,
    yields: mpsc::Sender<(u64, Why)>,
    resume: mpsc::Receiver<Resume>,
}

thread_local! {
    static BATON: RefCell<Option<BatonContext>> = const { RefCell::new(None) };
}

/// Give the baton back to the driver with `why`, and wait until the
/// driver hands it back. Only a session thread calls it.
pub(crate) fn give_back(why: Why) -> Resume {
    BATON.with_borrow(|context| {
        let context = context
            .as_ref()
            .expect("only a session thread gives the baton back");
        if context.yields.send((context.id, why)).is_err() {
            return Resume::Killed;
        }
        context.resume.recv().unwrap_or(Resume::Killed)
    })
}

/// Return the server generation of the session thread that calls it, or
/// `None` on another thread.
pub(crate) fn session_generation() -> Option<u32> {
    BATON.with_borrow(|context| context.as_ref().map(|context| context.generation))
}

/// Start a session thread for a connection of `client`. The
/// thread waits for the baton before it runs `body`. When `body` returns,
/// the thread tells the driver that it ended.
pub(crate) fn spawn_session(
    state: &mut State,
    client: usize,
    body: impl FnOnce(u64) + Send + 'static,
) -> u64 {
    state.next_thread += 1;
    let id = state.next_thread;
    let generation = state.server.generation;
    let (resume_send, resume) = mpsc::channel();
    let yields = state.yields.clone();
    state.threads.insert(
        id,
        ThreadState {
            generation,
            client,
            wait: Wait::Idle,
            inbox: VecDeque::new(),
            closed: false,
        },
    );
    let join = std::thread::Builder::new()
        .name(format!("zoe-sim-session-{id}"))
        .spawn(move || {
            let context = BatonContext {
                id,
                generation,
                yields,
                resume,
            };
            let first = context.resume.recv().unwrap_or(Resume::Killed);
            BATON.with_borrow_mut(|slot| *slot = Some(context));
            // The guard tells the driver that the thread ended, also when
            // the body panics.
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
            if first != Resume::Killed {
                body(id);
            }
        })
        .expect("a session thread starts");
    state.new_batons.push((
        id,
        Baton {
            resume: resume_send,
            join,
        },
    ));
    id
}
