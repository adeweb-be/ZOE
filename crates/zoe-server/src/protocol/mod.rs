//! The protocol layer: msgpack frames, the handshake and the method table.
//!
//! Each connection has one tokio task that reads its frames and starts its
//! calls, and one task that writes its frames. The calls of a connection
//! run concurrently (`pipeline`): the calls of its transaction in order on
//! one thread, from `tpc_begin` to the end of the transaction, and the
//! calls that read on the read pool. No core call runs on a thread of the
//! async runtime. A connection with no call that runs and no open
//! transaction holds no thread.
//! The connection reaches its tasks, its threads and the time through a
//! `Runtime`, so the simulator runs the same code.
//!
//! Each call takes room for its reply in the session queue of its
//! connection before it starts: at most `QUEUE` frames. The reader waits
//! for that room, so at most `QUEUE` calls of a connection run or wait at
//! once. The invalidations with the frames of each commit wait in the hub
//! queue, at most `client-queue-size` frames. They never wait: a full queue
//! closes the connection. The writer task sends the frames of both queues
//! in the order in which they went in.

mod blob;
mod clock;
mod compress;
mod extension;
pub(crate) mod frame;
mod invalidation;
mod iteration;
mod message;
pub(crate) mod metrics;
pub(crate) mod negotiation;
mod outbox;
mod pipeline;
mod resolution;
mod send_queue;
mod session;
mod status;
pub(crate) mod sweep;

/// The entry of the fuzz targets.
#[cfg(any(test, feature = "fuzz"))]
pub mod fuzz;

use std::fs;
use std::future::Future;
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::pin::pin;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};
use tokio::net::{TcpStream, UnixStream};
use tokio::sync::{oneshot, watch};
use zoe_core::Store;
use zoe_wire::PROTOCOL;

use crate::runtime::{BoxFuture, Runtime, Tokio};

use compress::Compressor;
use invalidation::Invalidations;
use metrics::{Exported, Metrics};
use outbox::{Closing, Outbox};
use pipeline::Calls;
pub(crate) use pipeline::Pool;
pub(crate) use resolution::ResolutionLog;
use status::Stats;

/// The payload that a client sends in place of a protocol to ask for the
/// status of the server.
const RUOK: &[u8] = b"ruok";

/// The most calls of a connection that run or wait at once, and the most
/// replies that wait in its session queue. A call takes room for its reply
/// before it starts, and the reader waits for that room. While it waits,
/// it reads no more bytes, so TCP makes the client wait. So a connection
/// holds at most this many decoded calls and the call that the reader
/// holds. An upload or a download holds a few chunks in memory, whatever
/// the size of the blob.
///
/// The writer task empties the queues of the frames without the reader, so
/// the reader, the calls and the writer never wait for each other in a
/// circle. Only a client that sends calls and reads no reply can stop its
/// own connection: its replies fill the session queue, then the reader
/// waits for room. A stock client reads its replies while it sends.
const QUEUE: usize = 4;

/// The longest time that a connection reads and drops bytes at a stop,
/// after its writer sent its last frame and shut down its side, when it
/// cannot read the send queue of its socket. It is also the longest time
/// that a connection reads and drops bytes after the bytes of the client
/// ended it (see `drain`). A stock client closes its
/// side when it reads the end of the connection, and that ends the wait
/// sooner. The wait keeps the socket open while the client can still
/// send. So a byte that the client sends in that time causes no reset,
/// and the kernel sends every frame that waits.
const LINGER: Duration = Duration::from_millis(500);

/// The pause between two reads of the send queue of a socket while the
/// connection waits at a stop for the queue to become empty. A client
/// whose bytes ended the connection is quiet when it sends nothing for
/// this time.
const DRAIN_PAUSE: Duration = Duration::from_millis(50);

/// The size of the buffer into which a connection reads the bytes that it
/// drops.
const DROPPED: usize = 8 * 1024;

/// The name of the storage that `--path` gives. It is the default of ZEO.
pub(crate) const STORAGE_NAME: &str = "1";

/// The bounds of the queues of a storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Queues {
    /// The most transactions that `getInvalidations` covers. Past it, the
    /// reply is nil, except for a tid younger than `invalidation_age`.
    pub(crate) invalidations: usize,
    /// The age under which `getInvalidations` covers every transaction
    /// after a tid, past `invalidations`, as the key `invalidation-age` of
    /// ZEO. Zero turns it off.
    pub(crate) invalidation_age: Duration,
    /// The most invalidations that wait for one client. Past it, the server
    /// closes the connection. The replies of the calls wait in a queue of
    /// `QUEUE` frames, and each call takes its room before it starts.
    pub(crate) client: usize,
}

impl Queues {
    /// The bounds of a server that gets no option: 100,000 transactions,
    /// no invalidation age and 1,000 messages.
    pub(crate) const DEFAULT: Self = Self {
        invalidations: 100_000,
        invalidation_age: Duration::ZERO,
        client: 1_000,
    };
}

/// A store that the server serves under a name, with its counters and its
/// invalidations. All the connections of the store share it.
pub(crate) struct NamedStorage {
    /// The name that a client gives to `register`.
    name: String,
    /// The directory of the store.
    path: PathBuf,
    store: Arc<Store>,
    stats: Arc<Stats>,
    /// The metrics that the status of ZEO does not keep.
    metrics: Arc<Metrics>,
    /// The publication of the commits to the connections.
    invalidations: Arc<Invalidations>,
    queues: Queues,
    /// True when the server opened the store for reads only.
    read_only: bool,
    /// True when the clients share the blob directory of the store, so
    /// that `storeBlobShared` is accepted.
    shared_blob_dir: bool,
    /// True when the server closes a connection that stays silent, or a
    /// voted transaction that stays open, so that an open transaction
    /// cannot hold its locks forever.
    timeout: bool,
    /// The resolution log of the server, if `--resolution-log` names one.
    resolution_log: Option<Arc<ResolutionLog>>,
    /// The longest time that a session holds intent locks without a
    /// `tpc_begin` (ADR 0045).
    pub(crate) intent_hold: std::time::Duration,
    /// The longest time that a transaction stays open after a vote that
    /// passed, as the `transaction-timeout` of ZEO. Past it, the server
    /// closes the connection, and the transaction aborts. `None` closes
    /// no connection for this reason.
    pub(crate) transaction_timeout: Option<Duration>,
    /// True when the store is on a `MemVfs` of the simulator, so it has no
    /// directory to measure.
    #[cfg(feature = "sim")]
    in_memory: bool,
}

impl NamedStorage {
    /// Serve `store`, in the directory `path`, under the name `name`, with
    /// the bounds `queues`. The status starts now. The store reports its
    /// sync batches and its lock waits to the metrics of the storage.
    pub(crate) fn new(name: &str, path: &Path, store: Arc<Store>, queues: Queues) -> Self {
        let metrics = Arc::new(Metrics::default());
        store.set_observer(metrics.clone());
        Self {
            name: name.to_owned(),
            path: path.to_path_buf(),
            invalidations: Arc::new(Invalidations::new(store.last_transaction())),
            store,
            stats: Arc::new(Stats::new(SystemTime::now())),
            metrics,
            queues,
            read_only: false,
            shared_blob_dir: false,
            timeout: false,
            resolution_log: None,
            intent_hold: session::INTENT_HOLD,
            transaction_timeout: None,
            #[cfg(feature = "sim")]
            in_memory: false,
        }
    }

    /// Release the intent locks that a session holds `hold` without a
    /// `tpc_begin`, in place of `INTENT_HOLD`.
    #[cfg(test)]
    pub(crate) fn with_intent_hold(mut self, hold: std::time::Duration) -> Self {
        self.intent_hold = hold;
        self
    }

    /// Start the status at `start`, in place of the time of `new`.
    #[cfg(feature = "sim")]
    pub(crate) fn started_at(mut self, start: SystemTime) -> Self {
        self.stats = Arc::new(Stats::new(start));
        self
    }

    /// Mark the store as one on a `MemVfs` of the simulator. Its size is
    /// then 0, so `get_info` answers without a store directory.
    #[cfg(feature = "sim")]
    pub(crate) fn in_memory(mut self) -> Self {
        self.in_memory = true;
        self
    }

    /// Mark the storage read-only when `read_only` is true. Its store must
    /// then be open for reads only. A `register` that asks to write fails.
    pub(crate) fn read_only(mut self, read_only: bool) -> Self {
        self.read_only = read_only;
        self
    }

    /// Accept `storeBlobShared` when `shared` is true. The blob directory
    /// of the store must then have its layout file, so that a client can
    /// create the directory of an oid there.
    pub(crate) fn shared_blob_dir(mut self, shared: bool) -> Self {
        self.shared_blob_dir = shared;
        self
    }

    /// Mark that the server closes a connection that stays silent, or the
    /// connection of a voted transaction that stays open, when `timeout`
    /// is true. The status then gives `timeout-thread-is-alive`
    /// true, as a ZEO server with a transaction timeout does.
    pub(crate) fn timeout(mut self, timeout: bool) -> Self {
        self.timeout = timeout;
        self
    }

    /// Close the connection of a transaction that stays open for longer
    /// than `timeout` after a vote that passed. `None` turns it off.
    pub(crate) fn transaction_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.transaction_timeout = timeout;
        self
    }

    /// Write the lines of the resolution log to `log`.
    pub(crate) fn resolution_log(mut self, log: Option<Arc<ResolutionLog>>) -> Self {
        self.resolution_log = log;
        self
    }

    /// Return what the admin port reads for the metrics of the storage.
    pub(crate) fn exported(&self) -> Exported {
        Exported {
            name: self.name.clone(),
            stats: Arc::clone(&self.stats),
            metrics: Arc::clone(&self.metrics),
            hub: Arc::clone(&self.invalidations),
            store: Arc::downgrade(&self.store),
        }
    }

    /// Return the status of the storage. It reads the store.
    fn status(&self) -> status::Status {
        self.stats
            .status(self.store.last_transaction(), self.timeout)
    }

    /// Return the size of the files in the store directory, in bytes, as
    /// `zoe.Storage.getSize` gives it. The blob directory is not counted.
    fn size(&self) -> io::Result<u64> {
        #[cfg(feature = "sim")]
        if self.in_memory {
            return Ok(0);
        }
        let mut size = 0;
        for entry in fs::read_dir(&self.path)? {
            let metadata = entry?.metadata()?;
            if metadata.is_file() {
                size += metadata.len();
            }
        }
        Ok(size)
    }
}

/// The stores that a server serves, each under its own name. All the
/// connections share it. A connection selects one store with `register`.
pub(crate) struct Storages {
    /// The storages, in the order of the configuration. No two have the
    /// same name.
    storages: Vec<Arc<NamedStorage>>,
    /// The most invalidations that wait for one client. A connection gets
    /// its queues before `register` selects its storage.
    client_queue: usize,
    /// The read pool of all the connections.
    pool: Pool,
    /// The compression pool of all the connections.
    compressor: Compressor,
}

impl Storages {
    /// Serve `storages`, whose names must differ. At most `client_queue`
    /// invalidations wait for one client. The read pool and the
    /// compression pool each have as many threads as the machine has
    /// cores.
    pub(crate) fn new(storages: Vec<Arc<NamedStorage>>, client_queue: usize) -> Self {
        Self {
            storages,
            client_queue,
            pool: Pool::new(Pool::cores()),
            compressor: Compressor::new(Pool::cores()),
        }
    }

    /// Encode the staged pickles on `compressor`.
    #[cfg(test)]
    pub(crate) fn with_compressor(mut self, compressor: Compressor) -> Self {
        self.compressor = compressor;
        self
    }

    /// Run the calls that read on `pool`.
    pub(crate) fn with_pool(mut self, pool: Pool) -> Self {
        self.pool = pool;
        self
    }

    /// Return the storage named `name`.
    pub(crate) fn get(&self, name: &str) -> Option<&Arc<NamedStorage>> {
        self.storages.iter().find(|storage| storage.name == name)
    }

    /// Return the name of each storage whose store stays open when the
    /// server drops `storages`. A session that runs holds the store of its
    /// storage. A session that did not register yet holds every store.
    pub(crate) fn held(storages: &Arc<Self>) -> Vec<&str> {
        let all = Arc::strong_count(storages) > 1;
        storages
            .storages
            .iter()
            .filter(|storage| all || Arc::strong_count(&storage.store) > 1)
            .map(|storage| storage.name.as_str())
            .collect()
    }

    /// Return the reply to `ruok`: the status of each storage. It reads the
    /// stores.
    fn ruok(&self) -> String {
        let statuses: Vec<_> = self
            .storages
            .iter()
            .map(|storage| (storage.name.as_str(), storage.status()))
            .collect();
        status::ruok(&statuses)
    }
}

/// The signal of a stop of the server, as the tasks that serve see it.
#[derive(Clone)]
pub(crate) struct Stopping(watch::Receiver<bool>);

impl Stopping {
    /// Return the sender of the signal and the signal. Send `true` to stop.
    pub(crate) fn new() -> (watch::Sender<bool>, Self) {
        let (sender, signal) = watch::channel(false);
        (sender, Self(signal))
    }

    /// Tell if the server stops. It does not wait, so a thread of the
    /// blocking pool can call it, as a pack does between two batches.
    pub(crate) fn is_set(&self) -> bool {
        *self.0.borrow()
    }

    /// Wait until the server stops. When the sender drops and sent no
    /// stop, wait forever.
    pub(crate) async fn wait(&mut self) {
        if self.0.wait_for(|stop| *stop).await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

/// Wait for `timeout` on `runtime`, or forever when it is `None`.
async fn idle(runtime: &dyn Runtime, timeout: Option<Duration>) {
    match timeout {
        Some(timeout) => runtime.sleep(timeout).await,
        None => std::future::pending().await,
    }
}

/// Serve one client on `stream` until it closes, until the server closes
/// the connection, or until `stopping` tells that the server stops. The
/// client selects one of `storages` with `register`. A frame longer than
/// `max_frame_size` bytes closes the connection before the server reserves
/// memory for it. The calls that read run on the read pool of `storages`.
///
/// The server closes the connection when the client sends no frame for
/// `idle_timeout`: no protocol after the accept, or no frame after the last
/// frame while no call of the connection runs. A call runs from its ticket
/// until it puts its reply, and a download until it ends. The time counts
/// again from the later of the last frame and the end of the last call.
/// The time also counts while the reader waits for room for a call, so a
/// client that reads nothing is closed too. The time counts from the last
/// whole frame, so a frame that takes longer than the timeout to arrive is
/// cut. This close is an end of the connection, as a close of the
/// hub is: the writer stops at once, and each call ends at the first reply
/// that it cannot queue. `None` closes no idle connection.
///
/// At a stop, the reader reads no more calls. The calls that the reader
/// already started run, so a `tpc_finish` that the server received commits
/// and gets its reply. A vote that waits for a lock goes on waiting, while
/// the other connections abort their transactions. Each other end of the
/// connection ends the lock wait of its vote at once, so a gone client
/// holds no lock. A pack that runs stops between two of its batches.
/// Then the transaction ends, and that aborts it if it is still open. The
/// writer sends the frames that wait, and shuts down its side of the
/// connection. The bytes that the client sends after the stop are read and
/// dropped until the close (see `linger`), so the close is not a reset.
pub(crate) async fn serve_connection(
    stream: TcpStream,
    storages: Arc<Storages>,
    max_frame_size: u32,
    idle_timeout: Option<Duration>,
    stopping: Stopping,
) {
    let peer = stream
        .peer_addr()
        .map_or_else(|_| "a client".to_owned(), |addr| addr.to_string());
    let socket = Some(stream.as_raw_fd());
    let (reader, writer) = stream.into_split();
    let connection = Connection {
        peer,
        runtime: Arc::new(Tokio),
        max_frame_size,
        idle_timeout,
        read_only: false,
        socket,
    };
    serve_stream(reader, writer, connection, storages, stopping).await;
}

/// Serve one client on `stream`, a connection on the unix domain socket at
/// `path`, as `serve_connection` does. The label of the client is
/// `unix_peer`.
pub(crate) async fn serve_unix_connection(
    stream: UnixStream,
    path: Arc<Path>,
    storages: Arc<Storages>,
    max_frame_size: u32,
    idle_timeout: Option<Duration>,
    stopping: Stopping,
) {
    let pid = stream.peer_cred().ok().and_then(|cred| cred.pid());
    let peer = unix_peer(&path, pid);
    let socket = Some(stream.as_raw_fd());
    let (reader, writer) = stream.into_split();
    let connection = Connection {
        peer,
        runtime: Arc::new(Tokio),
        max_frame_size,
        idle_timeout,
        read_only: false,
        socket,
    };
    serve_stream(reader, writer, connection, storages, stopping).await;
}

/// Return the label of a client on the unix domain socket at `path`, as
/// the log lines name it in place of an address: the path as the options
/// give it, then the process id of the client, `pid`. The kernel gives the
/// process id (`SO_PEERCRED`). Without it, the label is the path only.
fn unix_peer(path: &Path, pid: Option<i32>) -> String {
    match pid {
        Some(pid) => format!("{} pid {pid}", path.display()),
        None => path.display().to_string(),
    }
}

/// The settings of one connection.
pub(crate) struct Connection {
    /// The client, as the messages of the log name it.
    pub(crate) peer: String,
    /// The tasks, the threads of the calls and the timers of the
    /// connection.
    pub(crate) runtime: Arc<dyn Runtime>,
    /// The longest frame that the connection takes.
    pub(crate) max_frame_size: u32,
    /// The longest time without a frame and without a call that runs, or
    /// `None` for no limit.
    pub(crate) idle_timeout: Option<Duration>,
    /// True when the client may register read-only only, as the holder of
    /// a certificate of the read-only client CA.
    pub(crate) read_only: bool,
    /// The descriptor of the TCP or unix domain socket under the stream,
    /// or `None` for a stream without a socket. The reader of the stream
    /// holds the socket open. The close at a stop reads the send queue of
    /// this socket.
    pub(crate) socket: Option<RawFd>,
}

/// Serve one client on the two halves of its stream, as
/// `serve_connection` does, on the runtime of `connection`.
pub(crate) async fn serve_stream(
    mut reader: impl AsyncRead + Unpin + Send,
    mut writer: impl AsyncWrite + Unpin + Send + 'static,
    connection: Connection,
    storages: Arc<Storages>,
    mut stopping: Stopping,
) {
    let Connection {
        peer,
        runtime,
        max_frame_size,
        idle_timeout,
        read_only,
        socket,
    } = connection;
    if frame::write(&mut writer, PROTOCOL).await.is_err() {
        return;
    }
    // A client that does not answer the protocol does not hold a stop.
    let protocol = tokio::select! {
        biased;
        () = stopping.wait() => return,
        protocol = frame::read(&mut reader, max_frame_size) => protocol,
        () = idle(&*runtime, idle_timeout) => {
            tracing::info!(client = peer.as_str(), "no protocol within the idle timeout: closing the connection");
            return;
        }
    };
    match protocol {
        Ok(Some(protocol)) if protocol == PROTOCOL => {}
        Ok(Some(protocol)) if protocol == RUOK => {
            // The status reads the stores, so it runs on the read pool.
            let pool = storages.pool.clone();
            let reply = pool.run(&*runtime, move || storages.ruok()).await;
            if let Some(reply) = reply {
                let _ = frame::write(&mut writer, reply.as_bytes()).await;
            }
            let _ = tokio::io::AsyncWriteExt::shutdown(&mut writer).await;
            return;
        }
        Ok(Some(_)) => {
            tracing::warn!(
                client = peer.as_str(),
                "the client does not speak the protocol M5"
            );
            return;
        }
        Ok(None) => return,
        Err(error) => {
            tracing::warn!(client = peer.as_str(), "{error}");
            return;
        }
    }

    let (outbox, mut inbox, mut closing) = Outbox::new(QUEUE, storages.client_queue);
    let mut calls = Calls::new(
        storages,
        outbox.clone(),
        Arc::clone(&runtime),
        peer.clone(),
        stopping.clone(),
    )
    .read_only(read_only);
    let ended = calls.ended();
    // The receiver gets an error when the task of the writer ends, also
    // when the runtime drops it before it runs.
    let (writer_alive, writer_ended) = oneshot::channel::<()>();
    {
        let mut closing = closing.clone();
        runtime.spawn(Box::pin(async move {
            let _alive = writer_alive;
            // The writer stops at once when the server closes the
            // connection. Otherwise it sends every frame, until each call
            // ends.
            loop {
                let frame = tokio::select! {
                    biased;
                    () = closing.wait() => break,
                    frame = inbox.recv() => match frame {
                        Some(frame) => frame,
                        None => break,
                    },
                };
                // The parts go to the socket from their own buffers.
                let parts = frame.parts();
                let written = tokio::select! {
                    biased;
                    () = closing.wait() => break,
                    written = frame::write_parts(&mut writer, &parts) => written,
                };
                drop(parts);
                if written.is_err() {
                    break;
                }
                frame.sent();
            }
            // A call that waits for room in the session queue stops.
            drop(inbox);
            let _ = tokio::io::AsyncWriteExt::shutdown(&mut writer).await;
        }));
    }

    // A stop lets a vote that waits for a lock go on waiting: the other
    // sessions abort their transactions, and a finish that arrived commits.
    let mut stopped = false;
    // True when the reader stops for a reason other than a stop, a close
    // of the hub, the idle timeout or the end of the client's side: a frame
    // that is too long or is not a call, a call that ends the connection,
    // or a connection that failed. The reader then drains the bytes of the
    // client before the close.
    let mut refused = false;
    loop {
        // An end of the connection or a stop wins over a frame that waits.
        // A frame that arrived wins over the idle timeout. The idle time
        // starts again for each frame, after its call took its room. It
        // does not count while a call of the connection runs, and starts
        // again when the last call ends.
        let idle_calls = calls.idle(idle_timeout);
        let read = tokio::select! {
            biased;
            () = ended.wait() => {
                refused = true;
                break;
            }
            () = closing.wait() => break,
            () = stopping.wait() => {
                stopped = true;
                break;
            }
            read = frame::read(&mut reader, max_frame_size) => read,
            () = idle_calls => {
                tracing::info!(client = peer.as_str(), "no frame within the idle timeout: closing the connection");
                // No call runs now. The writer stops at once, also when it
                // waits for a client that reads nothing. Then the
                // transaction aborts.
                outbox.close();
                break;
            }
        };
        let bytes = match read {
            Ok(Some(bytes)) => bytes,
            Ok(None) => break,
            Err(error) => {
                // A frame longer than the limit, or a failed read. After a
                // failed read, the drain ends at its first read.
                tracing::warn!(client = peer.as_str(), "{error}");
                refused = true;
                break;
            }
        };
        let decoded = message::decode(&bytes);
        // The call holds copies of its values. So the bytes of the frame go
        // before the reader waits for room.
        drop(bytes);
        let call = match decoded {
            Ok(call) => call,
            Err(error) => {
                tracing::warn!(client = peer.as_str(), "a frame is not a call: {error}");
                refused = true;
                break;
            }
        };
        // `register` ended before the reader read this frame. So the
        // session tells if the connection declared `trace_context`.
        if call.context.is_some() && !calls.traces() {
            tracing::warn!(
                client = peer.as_str(),
                "a frame is not a call: it has a trace context, and the connection did not declare trace_context"
            );
            refused = true;
            break;
        }
        if call.is_heartbeat() {
            continue;
        }
        // The reader waits while `QUEUE` calls run or wait, while more
        // than `outbox::WAITING` bytes of replies wait, while the window
        // of the read pool is full, and while the session queue is full.
        // The idle time counts during this wait while no call runs. So a
        // client that reads nothing loses its connection and its locks.
        let quiet = calls.quiet_beside_ticket();
        let ticket = tokio::select! {
            biased;
            ticket = calls.ticket(&call) => ticket,
            () = ended.wait() => {
                refused = true;
                break;
            }
            () = closing.wait() => break,
            () = async {
                quiet.await;
                idle(&*runtime, idle_timeout).await;
            } => {
                tracing::info!(client = peer.as_str(), "no frame within the idle timeout while the reader waits for room: closing the connection");
                // No call runs now. The writer stops at once, also when it
                // waits for a client that reads nothing. Then the
                // transaction aborts.
                outbox.close();
                break;
            }
        };
        if ticket.is_closed() {
            break;
        }
        calls.start(call, ticket).await;
    }
    // Any other end of the connection ends the lock wait of its vote, so a
    // gone client holds no lock.
    if !stopped {
        calls.end_lock_wait();
    }
    if refused {
        // The calls end and the transaction aborts while the reader drops
        // the bytes of the client, so the locks go at once. The outbox of
        // the reader keeps the writer open until both end, so the FIN comes
        // after the drain.
        tokio::join!(calls.finish(), drain(&mut reader, &*runtime, &mut closing));
        drop(outbox);
        let _ = writer_ended.await;
        return;
    }
    // The writer ends when every outbox is gone: the outbox of the reader,
    // of the calls and of the session.
    drop(outbox);
    let finished = async move {
        calls.finish().await;
        let _ = writer_ended.await;
    };
    if stopped {
        linger(
            &mut reader,
            &peer,
            socket,
            finished,
            &*runtime,
            &mut closing,
        )
        .await;
    } else {
        finished.await;
    }
}

/// Read and drop the bytes of the client after its bytes ended the
/// connection, until the client sends nothing for `DRAIN_PAUSE`, until it
/// closes its side, or for `LINGER` at most. The connection closes only
/// after that. So the calls that the client sent before it saw the end
/// arrive before the close, and the close is not a reset. A reset makes a
/// write of the client fail, and the asyncio of CPython 3.12 then can lose
/// the next connection of the client. When the hub closed the connection,
/// the writer stopped at once, and no wait follows.
async fn drain(
    reader: &mut (impl AsyncRead + Unpin),
    runtime: &dyn Runtime,
    closing: &mut Closing,
) {
    let mut dropped = vec![0u8; DROPPED];
    let mut deadline = runtime.sleep(LINGER);
    loop {
        tokio::select! {
            biased;
            () = closing.wait() => return,
            () = &mut deadline => return,
            () = runtime.sleep(DRAIN_PAUSE) => return,
            read = reader.read(&mut dropped) => match read {
                Ok(n) if n > 0 => {}
                _ => return,
            },
        }
    }
}

/// Wait for `finished` at a stop of the server. Then keep the connection
/// open until the send queue of `socket` is empty, so that a late byte of
/// the client causes no reset.
///
/// The reader reads no more calls. It reads the bytes that arrive and
/// drops them. It never decodes them, so they take no ticket and get no
/// reply. Each read fills at most a buffer of `DROPPED` bytes. Linux
/// resets a connection that closes with bytes that it did not read, or
/// that gets bytes after its close. A reset deletes the frames that the
/// kernel did not send yet, the reply of a `tpc_finish` among them.
///
/// `finished` ends when each call ended and the writer sent its last frame
/// and shut down its side. Then the reader drops bytes until the send
/// queue of `socket` is empty. The queue holds the bytes that the client
/// did not acknowledge, the FIN included. When it is empty, the client
/// has every frame and the FIN, and a reset deletes nothing of them. On a
/// unix domain socket, the queue holds the bytes that the client did not
/// read. The connection reads the queue at once, then after each pause of
/// `DRAIN_PAUSE`. The check of the queue comes before the drop of the
/// bytes, so a client that sends without end does not delay the close.
/// Without a socket, or when a read of the queue fails, the reader drops
/// bytes for `LINGER` at most.
///
/// The wait ends sooner when the client closes its side or the
/// connection fails, because no byte of the client can arrive after that.
/// When the hub closed the connection, the writer stopped at once, and no
/// wait follows. The forced close of the stop bounds the whole wait.
async fn linger(
    reader: &mut (impl AsyncRead + Unpin),
    peer: &str,
    mut socket: Option<RawFd>,
    finished: impl Future<Output = ()>,
    runtime: &dyn Runtime,
    closing: &mut Closing,
) {
    let mut dropped = vec![0u8; DROPPED];
    let mut finished = pin!(finished);
    loop {
        tokio::select! {
            biased;
            () = &mut finished => break,
            read = reader.read(&mut dropped) => match read {
                Ok(n) if n > 0 => {}
                // The client closed its side, or the connection failed.
                _ => {
                    finished.await;
                    return;
                }
            },
        }
    }
    // With a socket, the timer is the pause before the next read of the
    // queue. Without a socket, it is the end of the wait.
    let mut timer: BoxFuture<()> = match socket {
        Some(_) => Box::pin(std::future::ready(())),
        None => runtime.sleep(LINGER),
    };
    loop {
        tokio::select! {
            biased;
            () = closing.wait() => return,
            () = &mut timer => {
                let Some(fd) = socket else { return };
                match send_queue::unacked(fd) {
                    Ok(0) => return,
                    Ok(_) => timer = runtime.sleep(DRAIN_PAUSE),
                    Err(error) => {
                        tracing::debug!(
                            client = peer,
                            "cannot read the send queue of the socket: {error}"
                        );
                        socket = None;
                        timer = runtime.sleep(LINGER);
                    }
                }
            }
            read = reader.read(&mut dropped) => match read {
                Ok(n) if n > 0 => {}
                _ => return,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpStream as Client;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    use rmpv::Value;
    use zoe_core::Tid;

    use super::invalidation::{Commit, Others};
    use super::*;

    fn read_frame(client: &mut Client) -> Vec<u8> {
        let mut len = [0u8; 4];
        client.read_exact(&mut len).unwrap();
        let mut payload = vec![0u8; u32::from_be_bytes(len) as usize];
        client.read_exact(&mut payload).unwrap();
        payload
    }

    fn write_frame(client: &mut Client, payload: &[u8]) {
        client.write_all(&frame::encode(payload).unwrap()).unwrap();
    }

    /// Return a signal that never tells a stop: its sender is gone.
    fn never_stops() -> Stopping {
        Stopping::new().1
    }

    #[test]
    fn a_client_on_a_unix_domain_socket_is_the_path_and_its_process_id() {
        assert_eq!(
            unix_peer(Path::new("./zeo.sock"), Some(42)),
            "./zeo.sock pid 42"
        );
        assert_eq!(unix_peer(Path::new("/run/zeo.sock"), None), "/run/zeo.sock");
    }

    #[test]
    fn a_connection_that_the_hub_closes_ends_with_its_session() {
        let dir = std::env::temp_dir().join(format!("zoe-protocol-close-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Arc::new(Store::open(&dir).unwrap());
        let storage = Arc::new(NamedStorage::new("1", &dir, store, Queues::DEFAULT));
        let storages = Arc::new(Storages::new(
            vec![Arc::clone(&storage)],
            Queues::DEFAULT.client,
        ));
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let served = Arc::clone(&storages);
        runtime.spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let max = crate::serve::DEFAULT_MAX_FRAME_SIZE.get();
            serve_connection(stream, served, max, None, never_stops()).await;
        });

        let mut client = Client::connect(addr).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        assert_eq!(read_frame(&mut client), PROTOCOL);
        write_frame(&mut client, PROTOCOL);
        let register = Value::Array(vec![
            Value::from(1),
            Value::Boolean(false),
            Value::from("register"),
            Value::Array(vec![Value::from("1"), Value::Boolean(false)]),
        ]);
        let mut bytes = Vec::new();
        rmpv::encode::write_value(&mut bytes, &register).unwrap();
        write_frame(&mut client, &bytes);
        assert_eq!(
            read_frame(&mut client),
            message::reply(Value::from(1), Value::Binary(vec![0; 8]))
        );
        assert_eq!(storage.stats.connections.load(Ordering::Relaxed), 1);

        // A commit whose oids are unknown closes every other connection.
        let (outbox, _inbox, _closing) = Outbox::new(1, 1);
        storage.invalidations.publish(Commit {
            previous: Tid(0),
            tid: Tid(1),
            origin: None,
            outbox,
            own: Vec::new(),
            others: Others::Close,
        });
        // The client reads the end of the connection, and the session ends.
        let mut rest = Vec::new();
        let read = client.read_to_end(&mut rest);
        assert!(
            read.is_ok()
                || read
                    .as_ref()
                    .is_err_and(|e| e.kind() == io::ErrorKind::ConnectionReset),
            "{read:?}"
        );
        assert!(rest.is_empty());
        let deadline = Instant::now() + Duration::from_secs(30);
        while storage.stats.connections.load(Ordering::Relaxed) > 0 {
            assert!(Instant::now() < deadline, "the session did not end");
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(runtime);
    }

    /// Return a storage on a new store for the test `name`.
    fn storage(name: &str) -> Arc<NamedStorage> {
        let dir = std::env::temp_dir().join(format!("zoe-protocol-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Arc::new(Store::open(&dir).unwrap());
        Arc::new(NamedStorage::new("1", &dir, store, Queues::DEFAULT))
    }

    /// Serve `storage` on a free port of 127.0.0.1, one task for each
    /// connection.
    fn serve(storage: &Arc<NamedStorage>) -> (tokio::runtime::Runtime, std::net::SocketAddr) {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let served = Arc::new(Storages::new(
            vec![Arc::clone(storage)],
            Queues::DEFAULT.client,
        ));
        runtime.spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let max = crate::serve::DEFAULT_MAX_FRAME_SIZE.get();
                tokio::spawn(serve_connection(
                    stream,
                    Arc::clone(&served),
                    max,
                    None,
                    never_stops(),
                ));
            }
        });
        (runtime, addr)
    }

    /// The loads that the test of the handoffs counts.
    const COUNTED_LOADS: usize = 500;

    /// A load of 1 KB passes from the reader of the connection to one
    /// blocking thread of the read pool, then to the writer: three
    /// handoffs. The test counts each task and each blocking thread that
    /// starts, and each wake of a task of the connection, from the first
    /// counted load to the reply of the last one. It prints the mean.
    #[test]
    fn a_load_of_1_kb_takes_three_handoffs() {
        let dir =
            std::env::temp_dir().join(format!("zoe-protocol-handoffs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Arc::new(Store::open(&dir).unwrap());
        let record = vec![7u8; 1024];
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.store(zoe_core::Oid(1), Tid(0), &record).unwrap();
        txn.vote().unwrap();
        let tid = txn.finish().unwrap();
        let storage = Arc::new(NamedStorage::new("1", &dir, store, Queues::DEFAULT));
        let storages = Arc::new(Storages::new(vec![storage], Queues::DEFAULT.client));

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let counting = crate::runtime::Counting::default();
        let served = counting.clone();
        runtime.spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            stream.set_nodelay(true).unwrap();
            let (reader, writer) = stream.into_split();
            let connection = Connection {
                peer: "test".to_owned(),
                runtime: Arc::new(served.clone()),
                max_frame_size: crate::serve::DEFAULT_MAX_FRAME_SIZE.get(),
                idle_timeout: Some(crate::serve::DEFAULT_IDLE_TIMEOUT),
                read_only: false,
                socket: None,
            };
            let serving = serve_stream(reader, writer, connection, storages, never_stops());
            served.spawn(Box::pin(serving));
        });

        let mut client = registered(addr);
        client.set_nodelay(true).unwrap();
        let loaded = Value::Array(vec![
            Value::Binary(record),
            zoe_wire::p64(tid.0),
            Value::Nil,
        ]);
        let mut load = |id: i64| {
            let args = vec![zoe_wire::p64(1), zoe_wire::p64(u64::MAX)];
            send(&mut client, id, "loadBefore", args);
            let reply = read_frame(&mut client);
            let reply = rmpv::decode::read_value(&mut reply.as_slice()).unwrap();
            let expected = vec![
                Value::from(id),
                Value::from(0),
                Value::from(".reply"),
                loaded.clone(),
            ];
            assert_eq!(reply, Value::Array(expected));
        };
        // The first loads warm the caches of the store.
        for id in 2..12 {
            load(id);
        }
        let before = counting.handoffs();
        for n in 0..COUNTED_LOADS {
            load(1000 + i64::try_from(n).unwrap());
        }
        let handoffs = counting.handoffs() - before;
        #[allow(clippy::cast_precision_loss)]
        let mean = handoffs as f64 / COUNTED_LOADS as f64;
        println!("handoffs per load of 1 KB: {mean:.2}");
        assert!(mean <= 3.0, "{mean:.2} handoffs per load");
        drop(runtime);
    }

    /// A writer that records the address and the length of each buffer
    /// that the server gives to it, then writes to `inner`.
    struct Recording<W> {
        inner: W,
        seen: Arc<std::sync::Mutex<Vec<(usize, usize)>>>,
    }

    impl<W> Recording<W> {
        fn note(&self, bytes: &[u8]) {
            let mut seen = self.seen.lock().unwrap();
            seen.push((bytes.as_ptr() as usize, bytes.len()));
        }
    }

    impl<W: AsyncWrite + Unpin> AsyncWrite for Recording<W> {
        fn poll_write(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            bytes: &[u8],
        ) -> std::task::Poll<io::Result<usize>> {
            self.note(bytes);
            std::pin::Pin::new(&mut self.inner).poll_write(cx, bytes)
        }

        fn poll_write_vectored(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            parts: &[io::IoSlice<'_>],
        ) -> std::task::Poll<io::Result<usize>> {
            for part in parts {
                self.note(part);
            }
            std::pin::Pin::new(&mut self.inner).poll_write_vectored(cx, parts)
        }

        fn is_write_vectored(&self) -> bool {
            self.inner.is_write_vectored()
        }

        fn poll_flush(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::pin::Pin::new(&mut self.inner).poll_flush(cx)
        }

        fn poll_shutdown(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
        }
    }

    /// The size of the records of the test of the copies. No other test
    /// loads a record of this size.
    const UNCOPIED: usize = 100_003;

    /// The bytes of a record of 100 KB go from the buffer that the store
    /// gives to the write to the socket, with no copy in the server. The
    /// test records the address of each buffer that `Store::load_before`
    /// gives to a load, and the address of each buffer that the writer of
    /// the connection gives to the socket. Each loaded buffer must reach
    /// the socket whole.
    #[test]
    fn the_bytes_of_a_record_of_100_kb_reach_the_socket_without_a_copy() {
        let dir =
            std::env::temp_dir().join(format!("zoe-protocol-uncopied-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Arc::new(Store::open(&dir).unwrap());
        // Bytes that do not compress, so the store keeps them as they are.
        let mut state: u64 = 0x2545_f491_4f6c_dd1d;
        let record: Vec<u8> = (0..UNCOPIED)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect();
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        for oid in 1..=4 {
            txn.store(zoe_core::Oid(oid), Tid(0), &record).unwrap();
        }
        txn.vote().unwrap();
        let tid = txn.finish().unwrap();
        let storage = Arc::new(NamedStorage::new("1", &dir, store, Queues::DEFAULT));
        let storages = Arc::new(Storages::new(vec![storage], Queues::DEFAULT.client));

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let written = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = Arc::clone(&written);
        runtime.spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (reader, writer) = stream.into_split();
            let writer = Recording {
                inner: writer,
                seen,
            };
            let connection = Connection {
                peer: "test".to_owned(),
                runtime: Arc::new(Tokio),
                max_frame_size: crate::serve::DEFAULT_MAX_FRAME_SIZE.get(),
                idle_timeout: None,
                read_only: false,
                socket: None,
            };
            serve_stream(reader, writer, connection, storages, never_stops()).await;
        });

        let mut client = registered(addr);
        let loaded = Value::Array(vec![
            Value::Binary(record.clone()),
            zoe_wire::p64(tid.0),
            Value::Nil,
        ]);
        for id in 2..22 {
            let oid = 1 + u64::try_from(id % 4).unwrap();
            let args = vec![zoe_wire::p64(oid), zoe_wire::p64(u64::MAX)];
            send(&mut client, id, "loadBefore", args);
            let reply = read_frame(&mut client);
            let reply = rmpv::decode::read_value(&mut reply.as_slice()).unwrap();
            let expected = vec![
                Value::from(id),
                Value::from(0),
                Value::from(".reply"),
                loaded.clone(),
            ];
            assert_eq!(reply, Value::Array(expected));
        }
        let loads: Vec<(usize, usize)> = session::LOADED
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, len)| *len == UNCOPIED)
            .copied()
            .collect();
        assert_eq!(loads.len(), 20);
        let written = written.lock().unwrap();
        let uncopied = loads.iter().filter(|load| written.contains(load)).count();
        println!("records of 100 KB that reached the socket without a copy: {uncopied} of 20");
        assert_eq!(uncopied, 20);
        drop(written);
        drop(runtime);
    }

    /// Send a call. The id 0 is a call without a reply.
    fn send(client: &mut Client, id: i64, method: &str, args: Vec<Value>) {
        let call = Value::Array(vec![
            Value::from(id),
            Value::Boolean(id == 0),
            Value::from(method),
            Value::Array(args),
        ]);
        let mut bytes = Vec::new();
        rmpv::encode::write_value(&mut bytes, &call).unwrap();
        write_frame(client, &bytes);
    }

    /// Connect to `addr`, answer the protocol and register.
    fn registered(addr: std::net::SocketAddr) -> Client {
        registered_as(addr, "1")
    }

    /// Connect to `addr`, answer the protocol and register with the
    /// storage `name`.
    fn registered_as(addr: std::net::SocketAddr, name: &str) -> Client {
        let mut client = Client::connect(addr).unwrap();
        register(&mut client, name);
        client
    }

    /// Answer the protocol on `client` and register with the storage
    /// `name`.
    fn register(client: &mut Client, name: &str) {
        client
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        assert_eq!(read_frame(client), PROTOCOL);
        write_frame(client, PROTOCOL);
        send(
            client,
            1,
            "register",
            vec![Value::from(name), Value::Boolean(false)],
        );
        read_frame(client);
    }

    /// Begin the transaction 7 and store `oid` as a new object.
    fn begin_and_store(client: &mut Client, oid: u64) {
        let begin = vec![Value::from(7), Value::Nil, Value::Nil, Value::Map(vec![])];
        send(client, 0, "tpc_begin", begin);
        let data = Value::Binary(b"pickle".to_vec());
        let store = vec![zoe_wire::p64(oid), zoe_wire::p64(0), data, Value::from(7)];
        send(client, 0, "storea", store);
    }

    /// Encode a call. The id 0 is a call without a reply.
    fn call_bytes(id: i64, method: &str, args: Vec<Value>) -> Vec<u8> {
        let call = Value::Array(vec![
            Value::from(id),
            Value::Boolean(id == 0),
            Value::from(method),
            Value::Array(args),
        ]);
        let mut bytes = Vec::new();
        rmpv::encode::write_value(&mut bytes, &call).unwrap();
        bytes
    }

    /// Send `first`, then 10 heartbeats, one each 5 ms, as a client that
    /// goes on sending before it sees the end of the connection. The
    /// server drops the heartbeats until the client is quiet, then closes.
    /// So each write succeeds, and the client gets the end of the
    /// connection, not a reset, and no reply. A server that closes at once
    /// closes with bytes that it did not read, which resets the
    /// connection, and a later write fails.
    fn burst_ends_without_a_reset(name: &str, first: &[u8]) {
        let storage = storage(name);
        let (runtime, addr) = serve(&storage);
        let mut client = registered(addr);
        client.write_all(first).unwrap();
        let heartbeat = frame::encode(&call_bytes(-1, "ping", vec![])).unwrap();
        for n in 0..10 {
            std::thread::sleep(Duration::from_millis(5));
            let written = client.write_all(&heartbeat);
            assert!(written.is_ok(), "write {n}: {written:?}");
        }
        let mut replies = Vec::new();
        let read = client.read_to_end(&mut replies);
        assert!(read.is_ok(), "{read:?}");
        assert!(replies.is_empty(), "{} bytes after the end", replies.len());
        assert!(client.take_error().unwrap().is_none());
        drop(client);
        drop(runtime);
    }

    #[test]
    fn a_frame_that_is_not_a_call_ends_without_a_reset() {
        burst_ends_without_a_reset("not-a-call", &frame::encode(b"salty").unwrap());
    }

    #[test]
    fn an_unknown_method_ends_without_a_reset() {
        let unknown = frame::encode(&call_bytes(0, "foo", vec![])).unwrap();
        burst_ends_without_a_reset("unknown-method", &unknown);
    }

    #[test]
    fn a_frame_that_is_too_long_ends_without_a_reset() {
        let max = crate::serve::DEFAULT_MAX_FRAME_SIZE.get();
        burst_ends_without_a_reset("too-long", &(max + 1).to_be_bytes());
    }

    /// Wait until `done` is true. Fail after 30 seconds.
    fn wait_until(what: &str, done: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !done() {
            assert!(Instant::now() < deadline, "{what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn a_connection_that_the_hub_closes_aborts_its_transaction() {
        let storage = storage("close-voted");
        let (runtime, addr) = serve(&storage);
        let mut client = registered(addr);
        begin_and_store(&mut client, 1);
        send(&mut client, 2, "vote", vec![Value::from(7)]);
        assert_eq!(
            read_frame(&mut client),
            message::reply(Value::from(2), Value::Array(Vec::new()))
        );
        assert!(!storage.store.lock_table_is_empty());

        // The hub closes the connection, as on a full hub queue.
        let (outbox, _inbox, _closing) = Outbox::new(1, 1);
        storage.invalidations.publish(Commit {
            previous: Tid(0),
            tid: Tid(1),
            origin: None,
            outbox,
            own: Vec::new(),
            others: Others::Close,
        });
        let _ = client.read_to_end(&mut Vec::new());

        // The transaction aborts and its locks go.
        let stats = &storage.stats;
        wait_until("the locks stay", || {
            storage.store.lock_table_is_empty() && stats.connections.load(Ordering::Relaxed) == 0
        });
        assert_eq!(stats.active_txns.load(Ordering::Relaxed), 0);
        assert_eq!(stats.aborts.load(Ordering::Relaxed), 1);
        drop(runtime);
    }

    #[test]
    fn a_stop_of_the_server_removes_the_upload_files() {
        let dir = std::env::temp_dir().join(format!("zoe-protocol-stop-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let server = crate::serve::Server::start(crate::serve::ServeOptions::new(&dir)).unwrap();
        let chunk = || vec![Value::Binary(vec![7; 1_000])];
        // One client has an open upload. The other ended its upload, so the
        // core holds the file.
        let mut open = registered(server.addr());
        begin_and_store(&mut open, 1);
        send(&mut open, 0, "storeBlobStart", vec![]);
        send(&mut open, 0, "storeBlobChunk", chunk());
        let mut ended = registered(server.addr());
        begin_and_store(&mut ended, 2);
        send(&mut ended, 0, "storeBlobStart", vec![]);
        send(&mut ended, 0, "storeBlobChunk", chunk());
        let data = Value::Binary(b"pickle".to_vec());
        let end = vec![zoe_wire::p64(2), zoe_wire::p64(0), data, Value::from(7)];
        send(&mut ended, 0, "storeBlobEnd", end);
        // The calls of a transaction run in order. So after the reply to an
        // abort of another transaction, each upload file holds its chunk.
        for client in [&mut open, &mut ended] {
            send(client, 2, "tpc_abort", vec![Value::from(99)]);
            read_frame(client);
        }
        let temporary = dir.join("blobs").join("tmp");
        let files = || std::fs::read_dir(&temporary).unwrap().count();
        assert_eq!(files(), 2);

        server.stop();
        assert_eq!(files(), 0);
    }

    /// Read the frames of the server until it closes the connection.
    /// Return the method and the arguments of each frame.
    fn read_until_closed(client: &mut Client) -> Vec<(String, Value)> {
        let mut frames = Vec::new();
        loop {
            let mut len = [0u8; 4];
            match client.read_exact(&mut len) {
                Ok(()) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset
                    ) =>
                {
                    return frames;
                }
                Err(error) => panic!("the connection failed: {error}"),
            }
            let mut payload = vec![0u8; u32::from_be_bytes(len) as usize];
            client.read_exact(&mut payload).unwrap();
            let message = rmpv::decode::read_value(&mut payload.as_slice()).unwrap();
            let Value::Array(parts) = message else {
                panic!("not a message: {message:?}");
            };
            let [_, _, method, args] = <[Value; 4]>::try_from(parts).unwrap();
            frames.push((method.as_str().unwrap().to_owned(), args));
        }
    }

    #[test]
    fn a_stop_answers_a_finish_that_it_received_and_aborts_the_other_transactions() {
        let dir =
            std::env::temp_dir().join(format!("zoe-protocol-stop-finish-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let server = crate::serve::Server::start(crate::serve::ServeOptions::new(&dir)).unwrap();
        // One client voted on the oid 1, so it holds the lock of the oid.
        let mut holder = registered(server.addr());
        begin_and_store(&mut holder, 1);
        send(&mut holder, 2, "vote", vec![Value::from(7)]);
        assert_eq!(
            read_frame(&mut holder),
            message::reply(Value::from(2), Value::Array(Vec::new()))
        );
        // One client only stored the oid 2.
        let mut staged = registered(server.addr());
        begin_and_store(&mut staged, 2);
        send(&mut staged, 2, "ping", vec![]);
        read_frame(&mut staged);
        // One client also votes on the oid 1. Its vote waits for the lock,
        // and its finish waits behind the vote.
        let mut committer = registered(server.addr());
        begin_and_store(&mut committer, 1);
        send(&mut committer, 2, "vote", vec![Value::from(7)]);
        send(&mut committer, 3, "tpc_finish", vec![Value::from(7)]);
        // Give the reader of the connection time to read the finish.
        std::thread::sleep(Duration::from_millis(200));

        server.stop();
        // The stop aborts the transaction that holds the lock. So the vote
        // gets the lock, and the finish commits and gets its reply before
        // the close.
        let frames = read_until_closed(&mut committer);
        let methods: Vec<&str> = frames.iter().map(|(method, _)| method.as_str()).collect();
        assert_eq!(methods, [".reply", "info", ".reply"], "{frames:?}");
        assert_eq!(frames[0].1, Value::Array(Vec::new()));
        let Value::Binary(tid) = &frames[2].1 else {
            panic!("the finish gives no tid: {frames:?}");
        };
        let tid = Tid(u64::from_be_bytes(tid.as_slice().try_into().unwrap()));
        for client in [&mut holder, &mut staged] {
            read_until_closed(client);
        }
        // The store closed cleanly. It holds the commit, and nothing of the
        // aborted transactions.
        let store = Store::open_read_only(&dir).unwrap();
        assert_eq!(
            store.load(zoe_core::Oid(1)).unwrap(),
            (b"pickle".to_vec(), tid)
        );
        assert!(store.load(zoe_core::Oid(2)).is_err());
    }

    #[test]
    fn a_stop_drops_the_bytes_that_arrive_and_closes_without_a_reset() {
        let storage = storage("linger");
        let storages = Arc::new(Storages::new(
            vec![Arc::clone(&storage)],
            Queues::DEFAULT.client,
        ));
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop, stopping) = Stopping::new();
        let served = runtime.spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let max = crate::serve::DEFAULT_MAX_FRAME_SIZE.get();
            serve_connection(stream, storages, max, None, stopping).await;
        });
        // The client has a receive buffer of 4 KiB. So a reply of 1 MiB
        // that it does not read stays in the send queue of the server, and
        // the server keeps the connection open until the client reads it.
        let stream = runtime.block_on(async {
            let socket = tokio::net::TcpSocket::new_v4().unwrap();
            socket.set_recv_buffer_size(4096).unwrap();
            socket.connect(addr).await.unwrap()
        });
        let mut client = stream.into_std().unwrap();
        client.set_nonblocking(false).unwrap();
        register(&mut client, "1");
        let begin = vec![Value::from(1), Value::Nil, Value::Nil, Value::Map(vec![])];
        send(&mut client, 0, "tpc_begin", begin);
        let large = Value::Binary(vec![7; 1 << 20]);
        let store = vec![zoe_wire::p64(1), zoe_wire::p64(0), large, Value::from(1)];
        send(&mut client, 0, "storea", store);
        send(&mut client, 2, "vote", vec![Value::from(1)]);
        read_frame(&mut client);
        send(&mut client, 3, "tpc_finish", vec![Value::from(1)]);
        // The `info` of the commit, then the reply of the finish.
        read_frame(&mut client);
        read_frame(&mut client);

        // The client loads the large record and reads nothing. The first
        // bytes of the reply arrive, so the reader read the load before
        // the stop.
        let load = vec![zoe_wire::p64(1), zoe_wire::p64(u64::MAX >> 1)];
        send(&mut client, 4, "loadBefore", load);
        client.peek(&mut [0; 1]).unwrap();
        stop.send_replace(true);
        // The session ends only after the reader left its loop at the
        // stop. So the bytes below arrive after the reader saw the stop,
        // and it reads no call from them.
        wait_until("the session did not end", || {
            storage.stats.connections.load(Ordering::Relaxed) == 0
        });
        for _ in 0..16 {
            send(&mut client, 5, "ping", vec![]);
        }
        client.write_all(&[7; 100_000]).unwrap();

        // The client gets the reply of the load, then the end of the
        // connection, not a reset, and no other reply.
        let mut rest = Vec::new();
        let read = client.read_to_end(&mut rest);
        assert!(read.is_ok(), "{read:?}");
        let fin = Instant::now();
        assert!(rest.len() >= 4, "no reply of the load: {rest:?}");
        let (len, rest) = rest.split_at(4);
        let (reply, rest) = rest.split_at(u32::from_be_bytes(len.try_into().unwrap()) as usize);
        let reply = rmpv::decode::read_value(&mut &reply[..]).unwrap();
        let Value::Array(parts) = reply else {
            panic!("not a message: {reply:?}");
        };
        assert_eq!(parts[0], Value::from(4), "{parts:?}");
        assert_eq!(parts[2], Value::from(".reply"), "{parts:?}");
        assert!(rest.is_empty(), "{} bytes after the reply", rest.len());

        // The connection ends within the wait, while the client keeps its
        // side open.
        wait_until("the connection did not end", || served.is_finished());
        let waited = fin.elapsed();
        assert!(waited < Duration::from_secs(5), "the wait took {waited:?}");
        drop(client);
        drop(runtime);
    }

    #[test]
    fn a_session_past_the_shutdown_timeout_keeps_only_the_store_of_its_storage() {
        use crate::serve::{ServeOptions, Server, StorageOptions};

        let dir =
            std::env::temp_dir().join(format!("zoe-protocol-stop-two-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let options = ServeOptions::with_storages([
            StorageOptions::new("a", dir.join("a")),
            StorageOptions::new("b", dir.join("b")),
        ])
        .lock_timeout(Duration::from_secs(60))
        .shutdown_timeout(Duration::from_millis(400));
        // The test keeps the store of a, so it can hold a lock there.
        let mut kept = None;
        let server = Server::start_with(options, |name, path, options| {
            let store = Arc::new(Store::open_with(path, options).unwrap());
            if name == "a" {
                kept = Some(Arc::clone(&store));
            }
            Ok(store)
        })
        .unwrap();
        let a = kept.unwrap();
        let mut holder = a.tpc_begin(b"", b"", b"").unwrap();
        holder
            .store(zoe_core::Oid(1), Tid::ZERO, b"pickle")
            .unwrap();
        holder.vote().unwrap();
        // A session on a votes on the same oid. Its vote waits for the lock
        // past the shutdown timeout. A session on b ends at the stop.
        let mut waiter = registered_as(server.addr(), "a");
        begin_and_store(&mut waiter, 1);
        send(&mut waiter, 2, "vote", vec![Value::from(7)]);
        let _idle = registered_as(server.addr(), "b");
        std::thread::sleep(Duration::from_millis(200));

        server.stop();
        // The store of b closed cleanly.
        Store::open_read_only(dir.join("b")).unwrap();
        // The abort of the holder ends the lock wait, so the session ends,
        // and the store of a closes too.
        drop(holder);
        drop(a);
        wait_until("the store of a stays open", || {
            Store::open_read_only(dir.join("a")).is_ok()
        });
    }

    #[test]
    fn the_idle_time_does_not_count_while_the_calls_of_the_connection_wait() {
        use crate::serve::{ServeOptions, Server};

        let dir = std::env::temp_dir().join(format!("zoe-protocol-idle-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let options = ServeOptions::new(&dir)
            .lock_timeout(Duration::from_secs(60))
            .idle_timeout(Duration::from_millis(600));
        // The test keeps the store, so it can hold a lock there.
        let mut kept = None;
        let server = Server::start_with(options, |_, path, options| {
            let store = Arc::new(Store::open_with(path, options).unwrap());
            kept = Some(Arc::clone(&store));
            Ok(store)
        })
        .unwrap();
        let store = kept.unwrap();
        let mut holder = store.tpc_begin(b"", b"", b"").unwrap();
        holder
            .store(zoe_core::Oid(1), Tid::ZERO, b"pickle")
            .unwrap();
        holder.vote().unwrap();
        // The vote of the client waits for the lock. The calls of the
        // transaction behind it wait for it, so they fill the session queue
        // with their rooms, and the reader waits for room.
        let mut client = registered(server.addr());
        begin_and_store(&mut client, 1);
        send(&mut client, 2, "vote", vec![Value::from(7)]);
        std::thread::sleep(Duration::from_millis(100));
        for id in 3..=3 + QUEUE as i64 {
            // An abort of another transaction does nothing.
            send(&mut client, id, "tpc_abort", vec![Value::from(99)]);
        }
        // The client sends nothing for more than twice the timeout. The
        // connection stays open, because the reader waits for room.
        std::thread::sleep(Duration::from_millis(1_500));
        drop(holder);
        let frames = read_until_closed(&mut client);
        let methods: Vec<&str> = frames.iter().map(|(method, _)| method.as_str()).collect();
        assert_eq!(methods, [".reply"; 2 + QUEUE], "{frames:?}");
        assert_eq!(frames[0].1, Value::Array(Vec::new()));
        // Then the idle connection closes, and its transaction aborts.
        wait_until("the locks stay", || store.lock_table_is_empty());
        drop(store);
        server.stop();
    }

    #[test]
    fn an_idle_close_aborts_the_transaction_of_a_client_that_reads_nothing() {
        use crate::serve::{ServeOptions, Server};

        let dir =
            std::env::temp_dir().join(format!("zoe-protocol-idle-unread-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let options = ServeOptions::new(&dir).idle_timeout(Duration::from_millis(600));
        let mut kept = None;
        let server = Server::start_with(options, |_, path, options| {
            let store = Arc::new(Store::open_with(path, options).unwrap());
            kept = Some(Arc::clone(&store));
            Ok(store)
        })
        .unwrap();
        let store = kept.unwrap();
        // A record of 8 MiB: its reply is longer than the largest send
        // buffer of Linux by default, 4 MiB.
        let large = Value::Binary(vec![7; 8 << 20]);
        let mut setup = registered(server.addr());
        let begin = vec![Value::from(1), Value::Nil, Value::Nil, Value::Map(vec![])];
        send(&mut setup, 0, "tpc_begin", begin);
        let store_large = vec![zoe_wire::p64(1), zoe_wire::p64(0), large, Value::from(1)];
        send(&mut setup, 0, "storea", store_large);
        send(&mut setup, 2, "vote", vec![Value::from(1)]);
        read_frame(&mut setup);
        send(&mut setup, 3, "tpc_finish", vec![Value::from(1)]);
        let frames = read_until_closed(&mut setup);
        assert_eq!(frames.first().map(|(m, _)| m.as_str()), Some("info"));

        // The client has a receive buffer of 4 KiB. It votes on the oid 2,
        // then loads the large record 5 times and reads nothing. The writer
        // waits on the first reply, and the session queue holds the 4
        // others. Each load took its room, so the reader waits for a frame.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let stream = runtime.block_on(async {
            let socket = tokio::net::TcpSocket::new_v4().unwrap();
            socket.set_recv_buffer_size(4096).unwrap();
            socket.connect(server.addr()).await.unwrap()
        });
        let mut client = stream.into_std().unwrap();
        client.set_nonblocking(false).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        assert_eq!(read_frame(&mut client), PROTOCOL);
        write_frame(&mut client, PROTOCOL);
        send(
            &mut client,
            1,
            "register",
            vec![Value::from("1"), Value::Boolean(false)],
        );
        read_frame(&mut client);
        begin_and_store(&mut client, 2);
        send(&mut client, 2, "vote", vec![Value::from(7)]);
        assert_eq!(
            read_frame(&mut client),
            message::reply(Value::from(2), Value::Array(Vec::new()))
        );
        assert!(!store.lock_table_is_empty());
        for id in 3..=7 {
            let load = vec![zoe_wire::p64(1), zoe_wire::p64(u64::MAX >> 1)];
            send(&mut client, id, "loadBefore", load);
        }
        // The idle close stops the writer, so the session ends and the
        // transaction aborts, although the client reads nothing.
        wait_until("the locks stay", || store.lock_table_is_empty());
        drop(client);
        drop(store);
        server.stop();
    }

    #[test]
    fn the_held_storages_are_those_whose_store_a_session_keeps() {
        let named = |name: &str| {
            let storage = storage(&format!("held-{name}"));
            let store = Arc::clone(&storage.store);
            Arc::new(NamedStorage::new(
                name,
                &storage.path,
                store,
                Queues::DEFAULT,
            ))
        };
        let (a, b) = (named("a"), named("b"));
        let storages = Arc::new(Storages::new(
            vec![Arc::clone(&a), Arc::clone(&b)],
            Queues::DEFAULT.client,
        ));
        assert!(Storages::held(&storages).is_empty());
        // A session on a keeps the store of a.
        let session = Arc::clone(&a.store);
        assert_eq!(Storages::held(&storages), ["a"]);
        drop(session);
        // A session that did not register yet keeps every store.
        let unregistered = Arc::clone(&storages);
        assert_eq!(Storages::held(&storages), ["a", "b"]);
        drop(unregistered);
        assert!(Storages::held(&storages).is_empty());
    }

    /// Serve `storage` on a new TCP listener. Return the runtime, the
    /// address and the task of the one connection that it serves.
    fn serving_one(
        storage: Arc<NamedStorage>,
        dispatch: Option<tracing::Dispatch>,
    ) -> (
        tokio::runtime::Runtime,
        std::net::SocketAddr,
        tokio::task::JoinHandle<()>,
    ) {
        let mut builder = tokio::runtime::Builder::new_multi_thread();
        builder.enable_all();
        if let Some(dispatch) = dispatch {
            builder.on_thread_start(move || {
                std::mem::forget(tracing::dispatcher::set_default(&dispatch));
            });
        }
        let runtime = builder.build().unwrap();
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let storages = Arc::new(Storages::new(vec![storage], Queues::DEFAULT.client));
        let served = runtime.spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (reader, writer) = stream.into_split();
            let connection = Connection {
                peer: "test".to_owned(),
                runtime: Arc::new(Tokio),
                max_frame_size: crate::serve::DEFAULT_MAX_FRAME_SIZE.get(),
                idle_timeout: None,
                read_only: false,
                socket: None,
            };
            serve_stream(reader, writer, connection, storages, never_stops()).await;
        });
        (runtime, addr, served)
    }

    /// Connect to `addr`, answer the protocol, register with the storage
    /// "1" and declare the extension `lock_intent`, then send
    /// `lock_intent` of `oid` with the id 2.
    fn locking_intent(addr: std::net::SocketAddr, oid: u64) -> Client {
        let mut client = Client::connect(addr).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        assert_eq!(read_frame(&mut client), PROTOCOL);
        write_frame(&mut client, PROTOCOL);
        let declaration = Value::Array(vec![Value::from("lock_intent")]);
        let register = vec![Value::from("1"), Value::Boolean(false), declaration];
        send(&mut client, 1, "register", register);
        read_frame(&mut client);
        let oids = Value::Array(vec![zoe_wire::p64(oid)]);
        send(&mut client, 2, "lock_intent", vec![oids]);
        client
    }

    /// The bytes of the log lines of a test.
    #[derive(Clone, Default)]
    struct Logs(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Logs {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn the_server_releases_intent_locks_held_past_the_bound_and_warns() {
        let dir =
            std::env::temp_dir().join(format!("zoe-protocol-intent-hold-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Arc::new(Store::open(&dir).unwrap());
        let hold = Duration::from_millis(200);
        let storage = NamedStorage::new("1", &dir, Arc::clone(&store), Queues::DEFAULT)
            .with_intent_hold(hold);
        let logs = Logs::default();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_writer({
                let logs = logs.clone();
                move || logs.clone()
            })
            .finish();
        // With one live dispatcher, tracing takes the interest of a callsite
        // from the default of the thread that registers it first, which can
        // be the thread of another test, without a subscriber. With two,
        // it asks each dispatcher.
        let _other = tracing::Dispatch::new(tracing_subscriber::registry());
        let dispatch = tracing::Dispatch::new(subscriber);
        let (runtime, addr, _served) = serving_one(Arc::new(storage), Some(dispatch));

        let started = Instant::now();
        let mut client = locking_intent(addr, 3);
        let reply = read_frame(&mut client);
        let reply = rmpv::decode::read_value(&mut reply.as_slice()).unwrap();
        assert_eq!(reply.as_array().unwrap()[2], Value::from(".reply"));
        assert!(!store.lock_table_is_empty());
        // A callsite that another test registers while the subscriber
        // registers can keep an interest of "never". The warning comes at
        // the bound, after this rebuild.
        tracing::callsite::rebuild_interest_cache();
        // The connection stays open and sends nothing: the thread of the
        // transaction releases the locks at the bound.
        let deadline = started + Duration::from_secs(30);
        while !store.lock_table_is_empty() {
            assert!(Instant::now() < deadline, "the locks stay held");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(started.elapsed() >= hold);
        // The warning comes after the release.
        let line = loop {
            let text = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
            if let Some(line) = text
                .lines()
                .find(|line| line.contains("released the intent locks"))
            {
                break line.to_owned();
            }
            assert!(Instant::now() < deadline, "no warning in:\n{text}");
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(line.contains("\"level\":\"WARN\""), "{line}");
        assert!(line.contains("\"first\":\"0x0000000000000003\""), "{line}");
        // The session goes on: another lock_intent comes after the release.
        let oids = Value::Array(vec![zoe_wire::p64(3)]);
        send(&mut client, 3, "lock_intent", vec![oids]);
        read_frame(&mut client);
        assert!(!store.lock_table_is_empty());
        drop(client);
        drop(runtime);
    }

    #[test]
    fn the_end_of_a_connection_ends_its_lock_intent_that_waits() {
        let dir =
            std::env::temp_dir().join(format!("zoe-protocol-intent-wait-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let options = zoe_core::Options {
            lock_timeout: Duration::from_secs(120),
            ..zoe_core::Options::default()
        };
        let store = Arc::new(Store::open_with(&dir, options).unwrap());
        // Another owner holds the intent lock of oid 3 for the whole test.
        let mut holder = zoe_core::IntentLocks::new(Arc::clone(&store));
        holder.lock(&[zoe_core::Oid(3)]).unwrap();
        let storage = Arc::new(NamedStorage::new(
            "1",
            &dir,
            Arc::clone(&store),
            Queues::DEFAULT,
        ));
        let (runtime, addr, served) = serving_one(storage, None);

        let client = locking_intent(addr, 3);
        std::thread::sleep(Duration::from_millis(200));
        assert!(!served.is_finished());
        let started = Instant::now();
        client.shutdown(std::net::Shutdown::Both).unwrap();
        drop(client);
        // The connection ends before the lock timeout: its wait ends.
        runtime
            .block_on(async { tokio::time::timeout(Duration::from_secs(20), served).await })
            .expect("the connection waits for the lock")
            .unwrap();
        assert!(started.elapsed() < Duration::from_secs(20));
        holder.release();
        assert!(store.lock_table_is_empty());
        drop(runtime);
    }
}
