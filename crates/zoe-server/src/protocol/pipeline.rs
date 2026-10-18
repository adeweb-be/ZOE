//! The calls of one connection run concurrently.
//!
//! The reader of a connection gives each call to `Calls` in the order of
//! arrival. Each call takes a `Ticket` before it starts: a slot among the
//! calls of its connection that run or wait, and room for its reply in the
//! session queue. A connection has `QUEUE` slots. So at most `QUEUE` calls
//! of a connection run or wait, the replies that wait for a client stay
//! bounded, and a call never waits to put its reply. Each reply carries the
//! message id of its call and goes out when its call ends, so the replies
//! can go out in another order than their calls.
//!
//! - `register` runs first, alone. The reader gives no other call before
//!   it ends. The same holds for `set_client_label`, so each call that
//!   comes after it logs with the label, and for `iterator_gc`, so a later
//!   call for an iterator that it removed fails with `KeyError`. The calls
//!   that started before these two still run.
//! - The calls of the transaction run in order on one thread, from
//!   `tpc_begin` to the end of the transaction (`Lane`). A vote that waits
//!   for a lock stops only the calls of the transaction behind it.
//! - `sendBlob` and `pack` run on a thread of their own, because they wait
//!   for the client or run for a long time. A download keeps its slot until
//!   it ends, and gives back its room after its first frame.
//! - The other calls run on the read pool: at most `--blocking-threads`
//!   calls of all the connections run at once. A call of the read pool
//!   also takes a turn in the window of its connection (`outbox::Turn`),
//!   so a client that reads slowly runs one such call at a time.
//!
//! A call starts only while at most `outbox::WAITING` bytes of frames of
//! the session wait for the client. It waits for that, and for its turn,
//! before it starts, never on a thread of the read pool.
//!
//! A connection with no call that runs and no open transaction holds no
//! thread. The calls reach their tasks and their threads through the
//! `Runtime` of the connection, and a thread waits only through
//! `runtime::block_on`, so the simulator runs the same code.
//!
//! Each call runs in a `connection` span of its own, with the client and
//! the storage that the session has when the call starts, and inside it in
//! the span of the call in the traces (`Session::request`). The thread of
//! the transaction drops the transaction in a `connection` span too.

use std::collections::VecDeque;
use std::future::Future;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, oneshot, watch};

use super::compress::Compressor;
use super::message::Call;
use super::outbox::{Outbox, Room, Turn};
use super::session::{self, Reply, Session, Transaction};
use super::{QUEUE, Stopping, Storages};
use crate::runtime::{Runtime, block_on};

/// The read pool: the threads that run the calls that read. At most its
/// number of threads run these calls at once, on blocking threads of the
/// runtime. All the connections share it.
#[derive(Clone)]
pub(crate) struct Pool(Arc<Semaphore>);

impl Pool {
    /// A pool of `threads` threads.
    pub(crate) fn new(threads: NonZeroUsize) -> Self {
        Self(Arc::new(Semaphore::new(
            threads.get().min(Semaphore::MAX_PERMITS),
        )))
    }

    /// The number of cores of the machine, or 1 when it is not known.
    pub(crate) fn cores() -> NonZeroUsize {
        std::thread::available_parallelism().unwrap_or(NonZeroUsize::MIN)
    }

    /// Run `work` on a blocking thread of `runtime`, when a thread of the
    /// pool is free. Return `None` when `work` panics, or when the runtime
    /// drops it.
    pub(crate) async fn run<T: Send + 'static>(
        &self,
        runtime: &dyn Runtime,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> Option<T> {
        let permit = Arc::clone(&self.0).acquire_owned().await.ok()?;
        let (sender, output) = oneshot::channel();
        // The thread keeps its permit until `work` ends, also when this
        // future drops.
        runtime.spawn_blocking(Box::new(move || {
            let _permit = permit;
            let _ = sender.send(work());
        }));
        output.await.ok()
    }

    /// Start `work` on a blocking thread of `runtime` at once, when a
    /// thread of the pool is free. Otherwise give `work` back. The caller
    /// does not wait for the thread.
    pub(crate) fn try_start<W: FnOnce() + Send + 'static>(
        &self,
        runtime: &dyn Runtime,
        work: W,
    ) -> Result<(), W> {
        let Ok(permit) = Arc::clone(&self.0).try_acquire_owned() else {
            return Err(work);
        };
        runtime.spawn_blocking(Box::new(move || {
            let _permit = permit;
            work();
        }));
        Ok(())
    }
}

/// The signal that a call ended the connection: the client is gone, a
/// call closes the connection, a call without a reply failed, or a call
/// panicked. Then the reader reads no more calls, and no call that waits
/// starts.
#[derive(Clone)]
pub(crate) struct Ended(Arc<watch::Sender<bool>>);

impl Ended {
    fn new() -> Self {
        Self(Arc::new(watch::channel(false).0))
    }

    /// End the connection.
    fn end(&self) {
        self.0.send_replace(true);
    }

    /// Tell if a call ended the connection.
    fn is_set(&self) -> bool {
        *self.0.borrow()
    }

    /// Wait until a call ends the connection.
    pub(crate) async fn wait(&self) {
        let mut ended = self.0.subscribe();
        // The sender lives as long as `self`, so the wait cannot fail.
        let _ = ended.wait_for(|ended| *ended).await;
    }
}

/// End the connection when the runtime drops a call before it runs.
struct EndUnlessRun(Option<Ended>);

impl EndUnlessRun {
    /// The call runs.
    fn run(mut self) {
        self.0 = None;
    }
}

impl Drop for EndUnlessRun {
    fn drop(&mut self) {
        if let Some(ended) = self.0.take() {
            ended.end();
        }
    }
}

/// End the connection when the thread panics.
struct EndOnPanic<'a>(&'a Ended);

impl Drop for EndOnPanic<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.end();
        }
    }
}

/// The count of the tasks and the threads of the calls of a connection
/// that did not end.
#[derive(Clone)]
struct Running(Arc<watch::Sender<usize>>);

/// The mark of one task or thread that runs. Its drop counts its end, also
/// when the runtime drops the task before it runs.
struct Mark(Arc<watch::Sender<usize>>);

impl Running {
    fn new() -> Self {
        Self(Arc::new(watch::channel(0).0))
    }

    /// Count a task or a thread that starts.
    fn start(&self) -> Mark {
        self.0.send_modify(|running| *running += 1);
        Mark(Arc::clone(&self.0))
    }

    /// Wait until each task and thread ended.
    async fn wait(&self) {
        let mut running = self.0.subscribe();
        let _ = running.wait_for(|running| *running == 0).await;
    }
}

impl Drop for Mark {
    fn drop(&mut self) {
        self.0.send_modify(|running| *running -= 1);
    }
}

/// The time of the end of the last call of a connection.
struct Ends {
    /// The clock of the connection.
    runtime: Arc<dyn Runtime>,
    /// The time of the end of the last call, in nanoseconds on the clock
    /// of `runtime`.
    last: AtomicU64,
}

impl Ends {
    /// Mark the end of a call now.
    fn mark(&self) {
        let now = u64::try_from(self.runtime.now().as_nanos()).unwrap_or(u64::MAX);
        self.last.fetch_max(now, Ordering::SeqCst);
    }

    /// Return the time of the end of the last call.
    fn last(&self) -> Duration {
        Duration::from_nanos(self.last.load(Ordering::SeqCst))
    }
}

/// The slot of one call among the calls of its connection that run or
/// wait. Its drop marks the end of the call, then gives the slot back.
struct Slot {
    _permit: OwnedSemaphorePermit,
    ends: Arc<Ends>,
}

impl Drop for Slot {
    fn drop(&mut self) {
        // The permit drops after this, so a reader that finds every slot
        // free also finds the end of the call.
        self.ends.mark();
    }
}

/// What a call takes before it starts: a slot among the calls of its
/// connection that run or wait, a turn in the window when it runs on the
/// read pool, and room for its reply in the session queue. The call gives
/// back its slot and its turn when it ends.
pub(crate) struct Ticket {
    slot: Slot,
    turn: Option<Turn>,
    room: Room,
}

impl Ticket {
    /// Tell if the connection was closed when the call took its room.
    pub(crate) fn is_closed(&self) -> bool {
        self.room.is_closed()
    }
}

/// What the thread of a lane does next.
enum Next {
    /// Run this call.
    Call(Box<(Call, Ticket)>),
    /// The time of `Lane::next` passed with no call.
    Late,
    /// End the thread.
    End,
}

/// The calls of the transaction of a connection that wait for its thread.
struct Lane {
    state: Mutex<LaneState>,
    /// A call arrived, or the connection ends. The thread of the lane is
    /// the only one that waits for it.
    ready: Notify,
}

struct LaneState {
    /// The calls that wait, each with its ticket, in the order of arrival.
    calls: VecDeque<(Call, Ticket)>,
    /// A thread runs the calls.
    running: bool,
    /// The reader gives no more calls. The thread runs the calls that wait,
    /// then ends, and that aborts the open transaction.
    ended: bool,
}

impl Lane {
    fn new() -> Self {
        Self {
            state: Mutex::new(LaneState {
                calls: VecDeque::new(),
                running: false,
                ended: false,
            }),
            ready: Notify::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, LaneState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Add a call. Return true when no thread runs the calls: the caller
    /// starts one. After the end, drop the call.
    fn push(&self, call: Call, ticket: Ticket) -> bool {
        let start = {
            let mut state = self.lock();
            if state.ended {
                return false;
            }
            state.calls.push_back((call, ticket));
            !std::mem::replace(&mut state.running, true)
        };
        self.ready.notify_one();
        start
    }

    /// Return the next call. Wait for it while a transaction is open
    /// (`open`). Return `End` when no call waits and no transaction is
    /// open, or at the end: then the thread ends. With `within`, wait at
    /// most that time on the clock of its runtime, and return `Late` when
    /// no call came.
    fn next(&self, open: bool, within: Option<(Duration, &dyn Runtime)>) -> Next {
        loop {
            {
                let mut state = self.lock();
                if let Some(next) = state.calls.pop_front() {
                    return Next::Call(Box::new(next));
                }
                if !open || state.ended {
                    state.running = false;
                    return Next::End;
                }
            }
            // A notice that comes after the check and before the wait is
            // kept, so the wait ends at once.
            let notified = self.ready.notified();
            let Some((within, runtime)) = within else {
                block_on(notified);
                continue;
            };
            let mut notified = std::pin::pin!(notified);
            let mut timer = runtime.sleep(within);
            let woken = block_on(std::future::poll_fn(|cx| {
                if notified.as_mut().poll(cx).is_ready() {
                    return std::task::Poll::Ready(true);
                }
                timer.as_mut().poll(cx).map(|()| false)
            }));
            if !woken {
                return Next::Late;
            }
        }
    }

    /// The reader gives no more calls.
    fn end(&self) {
        self.lock().ended = true;
        self.ready.notify_one();
    }

    /// Drop the calls that wait, and run no more calls.
    fn stop(&self) {
        let calls = {
            let mut state = self.lock();
            state.ended = true;
            state.running = false;
            std::mem::take(&mut state.calls)
        };
        drop(calls);
    }
}

/// Run the calls of the transaction of `session` that wait in `lane`, in
/// order, until no transaction is open and no call waits. The drop of the
/// state at the end aborts a transaction that is still open, in the span
/// of the connection, so its log lines name the client and the storage.
fn run_lane(session: &Arc<Session>, lane: &Lane, ended: &Ended, peer: &str, compress: Compress) {
    /// The end of the thread on a panic: the connection ends.
    struct Guard<'a>(&'a Lane, &'a Ended);

    impl Drop for Guard<'_> {
        fn drop(&mut self) {
            if std::thread::panicking() {
                self.0.stop();
                self.1.end();
            }
        }
    }

    let _guard = Guard(lane, ended);
    let (compressor, runtime) = compress;
    let mut transaction =
        Transaction::new(Arc::clone(session)).compressing(compressor, Arc::clone(&runtime));
    // The time on the clock of the runtime when the open transaction
    // passed its vote, while the transaction timeout is on.
    let mut voted_at: Option<Duration> = None;
    loop {
        // Intent locks without a `tpc_begin` last at most a bound. A voted
        // transaction stays open at most the transaction timeout.
        let voted_left = transaction
            .voted_timeout()
            .zip(voted_at)
            .map(|(timeout, at)| timeout.saturating_sub(runtime.now().saturating_sub(at)));
        let within = match (transaction.intent_hold_left(), voted_left) {
            (Some(intent), Some(voted)) => Some(intent.min(voted)),
            (intent, voted) => intent.or(voted),
        };
        let next = lane.next(transaction.is_open(), within.map(|left| (left, &*runtime)));
        if let (Some(timeout), Some(at)) = (transaction.voted_timeout(), voted_at)
            && !matches!(next, Next::End)
            && runtime.now().saturating_sub(at) >= timeout
        {
            // As ZEO's server does, close the connection. The calls that
            // wait do not run, and the drop of the transaction aborts it
            // and releases its locks.
            let _entered = session.span(peer).entered();
            tracing::error!(
                seconds = timeout.as_secs(),
                "transaction timeout after the vote: closing the connection"
            );
            lane.stop();
            session.outbox().close();
            break;
        }
        let (
            mut call,
            Ticket {
                slot: _slot, room, ..
            },
        ) = match next {
            Next::Call(next) => *next,
            Next::Late => {
                if transaction
                    .intent_hold_left()
                    .is_some_and(|left| left.is_zero())
                {
                    let _entered = session.span(peer).entered();
                    transaction.drop_intents();
                }
                continue;
            }
            Next::End => break,
        };
        if ended.is_set() {
            lane.stop();
            break;
        }
        let _entered = session.span(peer).entered();
        let _request = session.request(&call).entered();
        let outcome = transaction.call(&mut call);
        voted_at = match transaction.voted_timeout() {
            Some(_) => voted_at.or_else(|| Some(runtime.now())),
            None => None,
        };
        if !Reply::new(room).send(&call, outcome, session.outbox()) {
            lane.stop();
            ended.end();
            break;
        }
    }
    let _entered = session.span(peer).entered();
    drop(transaction);
}

/// The compression pool and the runtime of the jobs of a transaction.
type Compress = (Compressor, Arc<dyn Runtime>);

/// Run a call that is not a call of the transaction, in the span of the
/// connection, and put its reply. A panic ends the connection.
fn run_call(session: &Session, call: &Call, ticket: Ticket, ended: &Ended, peer: &str) {
    let _guard = EndOnPanic(ended);
    let Ticket {
        slot: _slot,
        turn: _turn,
        room,
    } = ticket;
    if ended.is_set() {
        return;
    }
    let _entered = session.span(peer).entered();
    let _request = session.request(call).entered();
    let mut reply = Reply::new(room);
    let outcome = session.call(call, &mut reply);
    if !reply.send(call, outcome, session.outbox()) {
        ended.end();
    }
}

/// The calls of one connection.
pub(crate) struct Calls {
    /// The storages, until `register` selects one.
    storages: Option<Arc<Storages>>,
    /// The session, from `register` on.
    session: Option<Arc<Session>>,
    outbox: Outbox,
    pool: Pool,
    /// The compression pool of the transactions.
    compressor: Compressor,
    /// The tasks and the threads of the calls.
    runtime: Arc<dyn Runtime>,
    /// The client, as the log lines name it until it gives a label.
    peer: Arc<str>,
    /// The stop of the server, for the pack of the session.
    stopping: Stopping,
    /// True when the client may register read-only only.
    read_only: bool,
    lane: Arc<Lane>,
    /// The slots of the calls that run or wait.
    slots: Arc<Semaphore>,
    /// The end of the last call.
    ends: Arc<Ends>,
    /// The tasks and the threads of the calls that did not end.
    running: Running,
    ended: Ended,
}

impl Calls {
    /// The calls of a connection to one of `storages`, whose frames go to
    /// `outbox`. The calls run on `runtime`, and the calls that read on the
    /// read pool of `storages`. `peer` names the client in the log lines.
    /// When `stopping` tells that the server stops, a pack of the session
    /// stops between two batches.
    pub(crate) fn new(
        storages: Arc<Storages>,
        outbox: Outbox,
        runtime: Arc<dyn Runtime>,
        peer: String,
        stopping: Stopping,
    ) -> Self {
        Self {
            pool: storages.pool.clone(),
            compressor: storages.compressor.clone(),
            storages: Some(storages),
            session: None,
            outbox,
            runtime: Arc::clone(&runtime),
            peer: peer.into(),
            stopping,
            read_only: false,
            lane: Arc::new(Lane::new()),
            slots: Arc::new(Semaphore::new(QUEUE)),
            ends: Arc::new(Ends {
                runtime,
                last: AtomicU64::new(0),
            }),
            running: Running::new(),
            ended: Ended::new(),
        }
    }

    /// Let the client register read-only only, when `read_only` is true.
    pub(crate) fn read_only(mut self, read_only: bool) -> Self {
        self.read_only = read_only;
        self
    }

    /// The signal that a call ended the connection.
    pub(crate) fn ended(&self) -> Ended {
        self.ended.clone()
    }

    /// Tell if a call of the connection can have a trace context: the
    /// connection registered, and the server accepted `trace_context`.
    pub(crate) fn traces(&self) -> bool {
        self.session
            .as_ref()
            .is_some_and(|session| session.traces())
    }

    /// Wait for the ticket of `call`: first a slot, then a turn in the
    /// window for a call of the read pool, or else until at most
    /// `outbox::WAITING` bytes wait, then room for its reply. The future
    /// holds no borrow of `self`.
    pub(crate) fn ticket(&self, call: &Call) -> impl Future<Output = Ticket> + Send + 'static {
        let reads = self.session.is_some() && session::on_read_pool(&call.method);
        let slots = Arc::clone(&self.slots);
        let ends = Arc::clone(&self.ends);
        let outbox = self.outbox.clone();
        async move {
            let slot = Slot {
                _permit: slots
                    .acquire_owned()
                    .await
                    .expect("the slots of a connection never close"),
                ends,
            };
            let turn = if reads {
                outbox.turn().await
            } else {
                outbox.below().await;
                None
            };
            let room = outbox.room().await;
            Ticket { slot, turn, room }
        }
    }

    /// Wait until no call of the connection runs. A call runs from its
    /// ticket until it puts its reply, or until it ends when it has no
    /// reply, because it keeps its slot that long. A download keeps its
    /// slot until it ends. The future holds no borrow of `self`. Call it
    /// only while the reader takes no ticket: the future takes each slot,
    /// then gives them back at once.
    #[cfg(test)]
    pub(crate) fn quiet(&self) -> impl Future<Output = ()> + Send + 'static {
        self.quiet_without(0)
    }

    /// Wait until no call of the connection runs, while the reader waits
    /// for a ticket. That ticket holds one slot, or gets the next slot
    /// that a call gives back, so the future takes each other slot. Poll
    /// the ticket first: the semaphore gives the slots in the order of the
    /// waits. The future holds no borrow of `self`.
    pub(crate) fn quiet_beside_ticket(&self) -> impl Future<Output = ()> + Send + 'static {
        self.quiet_without(1)
    }

    /// Wait until the connection is idle for `timeout`: no call runs, and
    /// `timeout` passed since the start of the wait and since the end of
    /// the last call. Wait forever when `timeout` is `None`. The future
    /// holds no borrow of `self`. Call it only while the reader takes no
    /// ticket.
    ///
    /// The end of a call does not wake the future. The future reads the
    /// end of the last call when its timer ends, and then waits again
    /// until `timeout` after that end. It waits for the end of a call
    /// only when a call still runs at the end of its timer.
    pub(crate) fn idle(
        &self,
        timeout: Option<Duration>,
    ) -> impl Future<Output = ()> + Send + 'static {
        let slots = Arc::clone(&self.slots);
        let ends = Arc::clone(&self.ends);
        async move {
            let Some(timeout) = timeout else {
                return std::future::pending().await;
            };
            let runtime = Arc::clone(&ends.runtime);
            let mut from = runtime.now();
            loop {
                let left = from.saturating_add(timeout).saturating_sub(runtime.now());
                if !left.is_zero() {
                    runtime.sleep(left).await;
                }
                if slots.available_permits() < QUEUE {
                    // A call runs. Wait for its end, and for the end of
                    // each other call.
                    let _all = slots
                        .acquire_many(QUEUE as u32)
                        .await
                        .expect("the slots of a connection never close");
                }
                let last = ends.last();
                if last <= from {
                    return;
                }
                from = last;
            }
        }
    }

    /// Take each slot but `kept`, then give them back.
    fn quiet_without(&self, kept: usize) -> impl Future<Output = ()> + Send + 'static {
        let slots = Arc::clone(&self.slots);
        async move {
            let _all = slots
                .acquire_many_owned((QUEUE - kept) as u32)
                .await
                .expect("the slots of a connection never close");
        }
    }

    /// Start `call`, which took `ticket`. Wait for the end of `register`
    /// and of each call of `session::ends_before_next`.
    pub(crate) async fn start(&mut self, call: Call, ticket: Ticket) {
        let Some(session) = self.session.clone() else {
            self.register(call, ticket).await;
            return;
        };
        let ended = self.ended.clone();
        let peer = Arc::clone(&self.peer);
        if session::ends_before_next(&call.method) {
            // The calls that start after a label log with it, and so does
            // the end of the transaction. The calls that start after
            // `iterator_gc` do not find the iterators that it removed.
            let ran = self
                .pool
                .run(&*self.runtime, move || {
                    run_call(&session, &call, ticket, &ended, &peer);
                })
                .await;
            if ran.is_none() {
                self.ended.end();
            }
            return;
        }
        let mark = self.running.start();
        if session::in_transaction(&call.method) {
            if self.lane.push(call, ticket) {
                let lane = Arc::clone(&self.lane);
                let compress = (self.compressor.clone(), Arc::clone(&self.runtime));
                self.runtime.spawn_blocking(Box::new(move || {
                    let _mark = mark;
                    run_lane(&session, &lane, &ended, &peer, compress);
                }));
            }
        } else if session::runs_long(&call.method) {
            self.runtime.spawn_blocking(Box::new(move || {
                let _mark = mark;
                run_call(&session, &call, ticket, &ended, &peer);
            }));
        } else {
            let unrun = EndUnlessRun(Some(ended.clone()));
            let work = move || {
                unrun.run();
                let _mark = mark;
                run_call(&session, &call, ticket, &ended, &peer);
            };
            // A free thread of the read pool runs the call at once.
            // Otherwise a task waits for a free thread, so the reader does
            // not wait for the pool.
            if let Err(work) = self.pool.try_start(&*self.runtime, work) {
                let pool = self.pool.clone();
                let runtime = Arc::clone(&self.runtime);
                self.runtime.spawn(Box::pin(async move {
                    // A panic of the call ends the connection in
                    // `run_call`, and a drop before it runs in `unrun`.
                    let _ = pool.run(&*runtime, work).await;
                }));
            }
        }
    }

    /// Run the first calls of the connection until `register` selects its
    /// storage. After that, the connection keeps only that storage, so a
    /// session that runs past the shutdown timeout keeps only its own store
    /// open.
    async fn register(&mut self, call: Call, ticket: Ticket) {
        let Some(storages) = self.storages.clone() else {
            return;
        };
        let outbox = self.outbox.clone();
        let ended = self.ended.clone();
        let peer = Arc::clone(&self.peer);
        let stopping = self.stopping.clone();
        let read_only = self.read_only;
        let registered = self
            .pool
            .run(&*self.runtime, move || {
                let Ticket {
                    slot: _slot, room, ..
                } = ticket;
                let mut reply = Reply::new(room);
                let (outcome, session) = match session::select(&storages, &call) {
                    Ok(storage) => {
                        let session = Session::new(storage, outbox.clone(), stopping)
                            .read_only_client(read_only);
                        let _entered = tracing::info_span!("connection", client = &*peer).entered();
                        let _request = session.request(&call).entered();
                        let outcome = session.call(&call, &mut reply);
                        (outcome, Some(session).filter(Session::is_registered))
                    }
                    Err(outcome) => (outcome, None),
                };
                let _entered = tracing::info_span!("connection", client = &*peer).entered();
                if !reply.send(&call, outcome, &outbox) {
                    ended.end();
                }
                session
            })
            .await;
        match registered {
            Some(Some(session)) => {
                self.storages = None;
                self.session = Some(Arc::new(session));
            }
            Some(None) => {}
            None => self.ended.end(),
        }
    }

    /// End the lock wait of the vote of the session, and each later one:
    /// the vote fails at once and releases the locks that it took. The end
    /// of the connection calls it, except at a stop of the server.
    pub(crate) fn end_lock_wait(&self) {
        if let Some(session) = &self.session {
            session.end_lock_wait();
        }
    }

    /// Wait until each call that started ends. The thread of the
    /// transaction runs the calls that wait, then ends, and that aborts the
    /// open transaction.
    pub(crate) async fn finish(mut self) {
        self.lane.end();
        self.storages = None;
        self.session = None;
        self.running.wait().await;
    }
}

impl Drop for Calls {
    /// The thread of the transaction ends also when the task of the
    /// connection drops before `finish`, as at the stop of the runtime. A
    /// drop before `finish` ends the lock wait of the session. A drop
    /// inside `finish`, as at the forced close of a stop, ends no lock
    /// wait, because `finish` removes the session first. Then the thread
    /// runs the calls that wait, ends and aborts the open transaction.
    fn drop(&mut self) {
        self.end_lock_wait();
        self.lane.end();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use rmpv::Value;
    use zoe_core::{Store, Tid};

    use super::*;
    use crate::protocol::session::p64;
    use crate::protocol::{NamedStorage, Queues};
    use crate::runtime::Tokio;

    /// Return the storages of a new store for the test `name`.
    fn storages(name: &str) -> (Arc<Storages>, Arc<Store>) {
        let dir = std::env::temp_dir().join(format!("zoe-pipeline-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Arc::new(Store::open(&dir).unwrap());
        let storage = NamedStorage::new("1", &dir, Arc::clone(&store), Queues::DEFAULT);
        let storages = Storages::new(vec![Arc::new(storage)], Queues::DEFAULT.client)
            .with_pool(Pool::new(NonZeroUsize::MIN));
        (Arc::new(storages), store)
    }

    fn call(id: i64, method: &str, args: Vec<Value>) -> Call {
        Call {
            id: Value::from(id),
            oneway: id == 0,
            method: method.into(),
            args,
            context: None,
        }
    }

    fn begin(txn: i64) -> Call {
        let args = vec![Value::from(txn), Value::Nil, Value::Nil, Value::Map(vec![])];
        call(0, "tpc_begin", args)
    }

    fn storea(oid: u64, txn: i64) -> Call {
        let data = Value::Binary(b"pickle".to_vec());
        call(0, "storea", vec![p64(oid), p64(0), data, Value::from(txn)])
    }

    /// Start `calls` on `connection`, each with its ticket.
    async fn start(connection: &mut Calls, calls: Vec<Call>) {
        for call in calls {
            let ticket = connection.ticket(&call).await;
            connection.start(call, ticket).await;
        }
    }

    /// Wait until `done` is true. Fail after 30 seconds.
    async fn wait_until(what: &str, done: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !done() {
            assert!(Instant::now() < deadline, "{what}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn the_read_pool_runs_at_most_its_threads_at_once() {
        let pool = Pool::new(NonZeroUsize::new(2).unwrap());
        let running = Arc::new(AtomicUsize::new(0));
        let most = Arc::new(AtomicUsize::new(0));
        runtime().block_on(async {
            let mut tasks = tokio::task::JoinSet::new();
            for _ in 0..6 {
                let (pool, running, most) = (pool.clone(), running.clone(), most.clone());
                tasks.spawn(async move {
                    pool.run(&Tokio, move || {
                        let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                        most.fetch_max(now, Ordering::SeqCst);
                        std::thread::sleep(Duration::from_millis(50));
                        running.fetch_sub(1, Ordering::SeqCst);
                    })
                    .await
                    .unwrap();
                });
            }
            while tasks.join_next().await.is_some() {}
        });
        assert_eq!(most.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn quiet_waits_until_each_ticket_is_given_back() {
        let (storages, _store) = storages("quiet");
        let (outbox, _inbox, _closing) = Outbox::new(100, 100);
        runtime().block_on(async {
            let connection = Calls::new(
                storages,
                outbox,
                Arc::new(Tokio),
                "test".into(),
                Stopping::new().1,
            );
            let limit = Duration::from_secs(30);
            // No call runs, so the connection is quiet at once.
            tokio::time::timeout(limit, connection.quiet())
                .await
                .unwrap();
            let ping = call(1, "ping", Vec::new());
            let held = connection.ticket(&ping).await;
            // A quiet that stops early gives back the slots that it took.
            let early = tokio::time::timeout(Duration::from_millis(50), connection.quiet()).await;
            assert!(early.is_err());
            let mut others = Vec::new();
            for _ in 1..QUEUE {
                others.push(
                    tokio::time::timeout(limit, connection.ticket(&ping))
                        .await
                        .unwrap(),
                );
            }
            drop(others);
            // The connection is quiet when the last ticket goes.
            let quiet = tokio::spawn(connection.quiet());
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert!(!quiet.is_finished());
            drop(held);
            tokio::time::timeout(limit, quiet).await.unwrap().unwrap();
        });
    }

    /// Return a registered connection to `storages` whose frames go to
    /// `outbox`.
    async fn registered(storages: Arc<Storages>, outbox: Outbox) -> Calls {
        let mut connection = Calls::new(
            storages,
            outbox,
            Arc::new(Tokio),
            "test".into(),
            Stopping::new().1,
        );
        let register = vec![Value::from("1"), Value::Boolean(false)];
        start(&mut connection, vec![call(1, "register", register)]).await;
        assert!(connection.session.is_some());
        connection
    }

    #[test]
    fn quiet_waits_for_a_call_that_waits_for_its_turn() {
        let (storages, _store) = storages("quiet-turn");
        // The test reads no frame, so the window stays at one call.
        let (outbox, _inbox, _closing) = Outbox::new(100, 100);
        runtime().block_on(async {
            let connection = registered(storages, outbox).await;
            let limit = Duration::from_secs(30);
            let load = call(2, "loadBefore", Vec::new());
            let first = tokio::time::timeout(limit, connection.ticket(&load))
                .await
                .unwrap();
            assert!(first.turn.is_some());
            // The second load holds a slot and waits for its turn.
            let waiting = tokio::spawn(connection.ticket(&load));
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert!(!waiting.is_finished());
            let quiet = tokio::spawn(connection.quiet());
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert!(!quiet.is_finished());
            // The first load ends. The second takes its turn and runs.
            drop(first);
            let second = tokio::time::timeout(limit, waiting).await.unwrap().unwrap();
            assert!(second.turn.is_some());
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert!(!quiet.is_finished());
            // No call runs, whatever the window: the connection is quiet.
            drop(second);
            tokio::time::timeout(limit, quiet).await.unwrap().unwrap();
        });
    }

    #[test]
    fn quiet_beside_a_ticket_waits_for_the_calls_that_hold_the_other_slots() {
        let (storages, _store) = storages("quiet-beside-slots");
        let (outbox, _inbox, _closing) = Outbox::new(100, 100);
        runtime().block_on(async {
            let connection = Calls::new(
                storages,
                outbox,
                Arc::new(Tokio),
                "test".into(),
                Stopping::new().1,
            );
            let limit = Duration::from_secs(30);
            let ping = call(1, "ping", Vec::new());
            let mut held = Vec::new();
            for _ in 0..QUEUE {
                held.push(
                    tokio::time::timeout(limit, connection.ticket(&ping))
                        .await
                        .unwrap(),
                );
            }
            // The next ticket waits for a slot, and the quiet waits after it.
            let waiting = tokio::spawn(connection.ticket(&ping));
            tokio::time::sleep(Duration::from_millis(50)).await;
            let quiet = tokio::spawn(connection.quiet_beside_ticket());
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert!(!quiet.is_finished());
            // The first slot that a call gives back goes to the ticket.
            drop(held.pop());
            let ticket = tokio::time::timeout(limit, waiting).await.unwrap().unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert!(!quiet.is_finished());
            // No other call runs: the connection is quiet beside the ticket.
            drop(held);
            tokio::time::timeout(limit, quiet).await.unwrap().unwrap();
            drop(ticket);
        });
    }

    #[test]
    fn quiet_beside_a_ticket_ends_while_the_ticket_waits_for_room() {
        use crate::protocol::outbox::WAITING;

        let (storages, _store) = storages("quiet-beside-room");
        // The test reads no frame, so the bytes of the reply stay counted.
        let (outbox, _inbox, _closing) = Outbox::new(100, 100);
        runtime().block_on(async {
            let connection = registered(storages, outbox).await;
            let limit = Duration::from_secs(30);
            let load = call(2, "loadBefore", Vec::new());
            let first = tokio::time::timeout(limit, connection.ticket(&load))
                .await
                .unwrap();
            // The first load puts a large reply and ends.
            let Ticket { slot, turn, room } = first;
            assert!(room.put(Some(vec![0; 2 * WAITING])));
            drop((slot, turn));
            // The second load holds a slot and waits until the reply goes.
            let waiting = tokio::spawn(connection.ticket(&load));
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert!(!waiting.is_finished());
            // No call runs, so the connection is quiet beside the ticket.
            tokio::time::timeout(limit, connection.quiet_beside_ticket())
                .await
                .unwrap();
            assert!(!waiting.is_finished());
            waiting.abort();
        });
    }

    #[test]
    fn the_ticket_of_a_connection_whose_writer_is_gone_ends() {
        use crate::protocol::outbox::WAITING;

        let (storages, _store) = storages("writer-gone");
        let (outbox, inbox, _closing) = Outbox::new(100, 100);
        runtime().block_on(async {
            let connection = registered(storages, outbox).await;
            let limit = Duration::from_secs(30);
            let load = call(2, "loadBefore", Vec::new());
            let first = tokio::time::timeout(limit, connection.ticket(&load))
                .await
                .unwrap();
            // The client resets, so the writer stops. Then the load puts a
            // large reply, which can stay counted.
            drop(inbox);
            let Ticket { slot, turn, room } = first;
            let _ = room.put(Some(vec![0; 2 * WAITING]));
            drop((slot, turn));
            // The next call does not wait for the count: its ticket ends,
            // and the reader stops on it.
            for method in ["loadBefore", "lastTransaction", "tpc_abort"] {
                let next = call(3, method, Vec::new());
                let ticket = tokio::time::timeout(limit, connection.ticket(&next))
                    .await
                    .unwrap();
                assert!(ticket.is_closed(), "{method}");
            }
        });
    }

    #[test]
    fn the_thread_of_a_transaction_ends_with_the_transaction() {
        let (storages, store) = storages("lane-ends");
        let (outbox, _inbox, _closing) = Outbox::new(100, 100);
        runtime().block_on(async {
            let mut connection = Calls::new(
                storages,
                outbox.clone(),
                Arc::new(Tokio),
                "test".into(),
                Stopping::new().1,
            );
            let register = vec![Value::from("1"), Value::Boolean(false)];
            start(&mut connection, vec![call(1, "register", register)]).await;
            for (txn, oid) in [(7, 1), (8, 2)] {
                let calls = vec![
                    begin(txn),
                    storea(oid, txn),
                    call(2, "vote", vec![Value::from(txn)]),
                    call(3, "tpc_finish", vec![Value::from(txn)]),
                ];
                start(&mut connection, calls).await;
                // The transaction commits, then its thread ends, while the
                // connection stays open.
                wait_until("the transaction did not commit", || {
                    store.load(zoe_core::Oid(oid)).is_ok()
                })
                .await;
                let lane = Arc::clone(&connection.lane);
                wait_until("the thread of the transaction runs", || {
                    !lane.lock().running
                })
                .await;
            }
            connection.finish().await;
        });
        assert!(store.lock_table_is_empty());
    }

    #[test]
    fn the_end_of_the_connection_ends_the_lock_wait_of_its_vote() {
        let (storages, store) = storages("lock-wait-ends");
        // Another transaction holds the lock of oid 2 for the whole test.
        let mut holder = store.tpc_begin(b"", b"", b"").unwrap();
        holder.store(zoe_core::Oid(2), Tid::ZERO, b"held").unwrap();
        holder.vote().unwrap();
        let (outbox, _inbox, _closing) = Outbox::new(100, 100);
        let ended = runtime().block_on(async {
            let mut connection = Calls::new(
                storages,
                outbox.clone(),
                Arc::new(Tokio),
                "test".into(),
                Stopping::new().1,
            );
            let register = vec![Value::from("1"), Value::Boolean(false)];
            // The vote takes the lock of oid 1 and waits for oid 2, longer
            // than the test when nothing ends the wait.
            let calls = vec![
                call(1, "register", register),
                begin(7),
                storea(1, 7),
                storea(2, 7),
                call(2, "vote", vec![Value::from(7)]),
            ];
            start(&mut connection, calls).await;
            tokio::time::sleep(Duration::from_millis(100)).await;
            let started = Instant::now();
            connection.end_lock_wait();
            connection.finish().await;
            started.elapsed()
        });
        assert!(ended < Duration::from_secs(5), "the end took {ended:?}");
        // Only the holder holds a lock.
        holder.abort();
        assert!(store.lock_table_is_empty());
        assert_eq!(store.last_transaction(), Tid::ZERO);
    }

    #[test]
    fn a_failed_call_without_a_reply_stops_the_calls_behind_it() {
        let (storages, store) = storages("lane-stops");
        let (outbox, _inbox, _closing) = Outbox::new(100, 100);
        runtime().block_on(async {
            let mut connection = Calls::new(
                storages,
                outbox.clone(),
                Arc::new(Tokio),
                "test".into(),
                Stopping::new().1,
            );
            let ended = connection.ended();
            let register = vec![Value::from("1"), Value::Boolean(false)];
            // The store names another transaction, so it closes the
            // connection, and the vote and the finish do not run.
            let calls = vec![
                call(1, "register", register),
                begin(7),
                storea(1, 8),
                call(2, "vote", vec![Value::from(7)]),
                call(3, "tpc_finish", vec![Value::from(7)]),
            ];
            start(&mut connection, calls).await;
            tokio::time::timeout(Duration::from_secs(30), ended.wait())
                .await
                .unwrap();
            connection.finish().await;
        });
        assert_eq!(store.last_transaction(), Tid::ZERO);
        assert!(store.lock_table_is_empty());
    }
}
