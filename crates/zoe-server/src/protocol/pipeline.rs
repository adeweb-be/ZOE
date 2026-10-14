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
//!   comes after it logs with the label.
//! - The calls of the transaction run in order on one thread, from
//!   `tpc_begin` to the end of the transaction (`Lane`). A vote that waits
//!   for a lock stops only the calls of the transaction behind it.
//! - `sendBlob` and `pack` run on a thread of their own, because they wait
//!   for the client or run for a long time. A download keeps its slot until
//!   it ends, and gives back its room after its first frame.
//! - The other calls run on the read pool: at most `--blocking-threads`
//!   calls of all the connections run at once.
//!
//! A connection with no call that runs and no open transaction holds no
//! thread. The calls reach their tasks and their threads through the
//! `Runtime` of the connection, and a thread waits only through
//! `runtime::block_on`, so the simulator runs the same code.
//!
//! Each call runs in a `connection` span of its own, with the client and
//! the storage that the session has when the call starts. The thread of
//! the transaction drops the transaction in such a span too.

use std::collections::VecDeque;
use std::future::Future;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, oneshot, watch};

use super::message::Call;
use super::outbox::{Outbox, Room};
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

/// What a call takes before it starts: a slot among the calls of its
/// connection that run or wait, and room for its reply in the session
/// queue. The call gives back its slot when it ends.
pub(crate) struct Ticket {
    slot: OwnedSemaphorePermit,
    room: Room,
}

impl Ticket {
    /// Tell if the connection was closed when the call took its room.
    pub(crate) fn is_closed(&self) -> bool {
        self.room.is_closed()
    }
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
    /// (`open`). Return `None` when no call waits and no transaction is
    /// open, or at the end: then the thread ends.
    fn next(&self, open: bool) -> Option<(Call, Ticket)> {
        loop {
            {
                let mut state = self.lock();
                if let Some(next) = state.calls.pop_front() {
                    return Some(next);
                }
                if !open || state.ended {
                    state.running = false;
                    return None;
                }
            }
            // A notice that comes after the check and before the wait is
            // kept, so the wait ends at once.
            block_on(self.ready.notified());
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
fn run_lane(session: &Arc<Session>, lane: &Lane, ended: &Ended, peer: &str) {
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
    let mut transaction = Transaction::new(Arc::clone(session));
    while let Some((call, Ticket { slot: _slot, room })) = lane.next(transaction.is_open()) {
        if ended.is_set() {
            lane.stop();
            break;
        }
        let _entered = session.span(peer).entered();
        let outcome = transaction.call(&call);
        if !Reply::new(room).send(&call, outcome, session.outbox()) {
            lane.stop();
            ended.end();
            break;
        }
    }
    let _entered = session.span(peer).entered();
    drop(transaction);
}

/// Run a call that is not a call of the transaction, in the span of the
/// connection, and put its reply. A panic ends the connection.
fn run_call(session: &Session, call: &Call, ticket: Ticket, ended: &Ended, peer: &str) {
    let _guard = EndOnPanic(ended);
    let Ticket { slot: _slot, room } = ticket;
    if ended.is_set() {
        return;
    }
    let _entered = session.span(peer).entered();
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
    /// The tasks and the threads of the calls.
    runtime: Arc<dyn Runtime>,
    /// The client, as the log lines name it until it gives a label.
    peer: Arc<str>,
    /// The stop of the server, for the pack of the session.
    stopping: Stopping,
    lane: Arc<Lane>,
    /// The slots of the calls that run or wait.
    slots: Arc<Semaphore>,
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
            storages: Some(storages),
            session: None,
            outbox,
            runtime,
            peer: peer.into(),
            stopping,
            lane: Arc::new(Lane::new()),
            slots: Arc::new(Semaphore::new(QUEUE)),
            running: Running::new(),
            ended: Ended::new(),
        }
    }

    /// The signal that a call ended the connection.
    pub(crate) fn ended(&self) -> Ended {
        self.ended.clone()
    }

    /// Wait for the ticket of the next call: first a slot, then room for
    /// its reply. The future holds no borrow of `self`.
    pub(crate) fn ticket(&self) -> impl Future<Output = Ticket> + Send + 'static {
        let slots = Arc::clone(&self.slots);
        let outbox = self.outbox.clone();
        async move {
            let slot = slots
                .acquire_owned()
                .await
                .expect("the slots of a connection never close");
            let room = outbox.room().await;
            Ticket { slot, room }
        }
    }

    /// Wait until no call of the connection runs. A call runs from its
    /// ticket until it puts its reply, or until it ends when it has no
    /// reply, because it keeps its slot that long. A download keeps its
    /// slot until it ends. The future holds no borrow of `self`. Call it
    /// only while the reader takes no ticket: the future takes each slot,
    /// then gives them back at once.
    pub(crate) fn quiet(&self) -> impl Future<Output = ()> + Send + 'static {
        let slots = Arc::clone(&self.slots);
        async move {
            let _all = slots
                .acquire_many_owned(QUEUE as u32)
                .await
                .expect("the slots of a connection never close");
        }
    }

    /// Start `call`, which took `ticket`. Wait only for `register`.
    pub(crate) async fn start(&mut self, call: Call, ticket: Ticket) {
        let Some(session) = self.session.clone() else {
            self.register(call, ticket).await;
            return;
        };
        let ended = self.ended.clone();
        let peer = Arc::clone(&self.peer);
        if call.method == "set_client_label" {
            // The calls that start after the label log with it, and so
            // does the end of the transaction.
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
                self.runtime.spawn_blocking(Box::new(move || {
                    let _mark = mark;
                    run_lane(&session, &lane, &ended, &peer);
                }));
            }
        } else if session::runs_long(&call.method) {
            self.runtime.spawn_blocking(Box::new(move || {
                let _mark = mark;
                run_call(&session, &call, ticket, &ended, &peer);
            }));
        } else {
            let pool = self.pool.clone();
            let runtime = Arc::clone(&self.runtime);
            self.runtime.spawn(Box::pin(async move {
                let _mark = mark;
                let work = {
                    let ended = ended.clone();
                    move || run_call(&session, &call, ticket, &ended, &peer)
                };
                if pool.run(&*runtime, work).await.is_none() {
                    ended.end();
                }
            }));
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
        let registered = self
            .pool
            .run(&*self.runtime, move || {
                let Ticket { slot: _slot, room } = ticket;
                let mut reply = Reply::new(room);
                let (outcome, session) = match session::select(&storages, &call) {
                    Ok(storage) => {
                        let session = Session::new(storage, outbox.clone(), stopping);
                        let _entered = tracing::info_span!("connection", client = &*peer).entered();
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
    /// connection drops before `finish`, as at the stop of the runtime: it
    /// runs the calls that wait, then ends and aborts the open transaction.
    fn drop(&mut self) {
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
            let ticket = connection.ticket().await;
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
