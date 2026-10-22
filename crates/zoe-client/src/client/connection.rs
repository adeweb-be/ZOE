//! The connection of a client: its calls, its replies and the calls of the
//! server.
//!
//! Three tasks serve one connection. The writer sends the frames of the
//! calls in the order of a channel. The reader gives each reply to its
//! call by the message id, and gives each invalidation to the receivers
//! of invalidations. The heartbeat task sends a heartbeat every 60
//! seconds. When one of the tasks stops, the connection closes: each call
//! that waits for its reply fails with `Error::Disconnected`, and so does
//! each later call.
//!
//! On a connection that got the extension `trace_context`, each call that
//! runs inside `traced` carries its W3C traceparent.
//!
//! `Client::send_blob` downloads a blob. The server answers `sendBlob` with
//! `receiveBlobStart`, one `receiveBlobChunk` per chunk and
//! `receiveBlobStop`, then the reply. The reader gives these calls to the
//! download of their oid and serial.

use std::any::Any;
use std::collections::HashMap;
use std::future::Future;
use std::io::{self, Write};
use std::mem;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use rmpv::Value;
use tokio::io::{AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::sync::{Notify, broadcast, mpsc, oneshot, watch};
use zoe_core::{Oid, Tid};
use zoe_wire::{p64, u64_of};

use super::{HEARTBEAT_INTERVAL, Invalidation, bad, merge};
use crate::protocol::{self, Message};
use crate::runtime::{Runtime, Stream};
use crate::{Error, lock};

/// The number of frames that wait for the writer. A call waits while the
/// channel is full.
const OUTGOING: usize = 64;

/// The number of invalidations that wait for a slow receiver. A receiver
/// that falls further behind gets `Error::Lagged`.
const INVALIDATIONS: usize = 1024;

/// The number of pieces of a download that wait for the download. The
/// reader waits while the channel is full.
const PIECES: usize = 16;

tokio::task_local! {
    /// The W3C traceparent of the calls of the current `traced` future.
    static TRACEPARENT: Option<String>;
}

/// Run `future` with the W3C traceparent `traceparent`. On a connection
/// that got the extension `trace_context`, each call that `future` sends
/// carries it, in a fifth element of its message. A call outside `traced`,
/// or with `None`, has four elements. The heartbeat and the abort of a
/// dropped `Transaction` carry no traceparent.
pub fn traced<F: Future>(
    traceparent: Option<String>,
    future: F,
) -> impl Future<Output = F::Output> {
    TRACEPARENT.scope(traceparent, future)
}

/// The state that the client, its transactions and its tasks share.
pub(super) struct Inner {
    runtime: Arc<dyn Runtime>,
    pub(super) calls: Mutex<Calls>,
    /// The frames for the writer.
    outgoing: mpsc::Sender<Vec<u8>>,
    /// `true` once the connection closes. Each task stops then.
    pub(super) closing: watch::Sender<bool>,
    /// The last tid that the client knows: from `register`, from each
    /// invalidation, and from each commit of the client.
    pub(super) last_tid: AtomicU64,
    /// The sender of invalidations, until the connection closes.
    pub(super) invalidations: Mutex<Option<broadcast::Sender<Invalidation>>>,
    /// One transaction at a time, as the commit lock of `ZEO.ClientStorage`.
    pub(super) commit: Arc<tokio::sync::Mutex<()>>,
    /// The id of the next transaction.
    pub(super) next_txn: AtomicU64,
    /// `true` when the server accepted `trace_context`. Only then does a
    /// call carry a traceparent.
    pub(super) traces: AtomicBool,
    /// Wakes the downloads that wait for the end of another download of
    /// their blob.
    downloads_done: Notify,
    /// The map of the `info` calls of the server, merged, with the stamp of
    /// the last one. `None` until the first one.
    pub(super) pushed: Mutex<Option<(u64, Value)>>,
}

/// The stamp of the last `info` call of a server, over all the clients of
/// the process. A higher stamp is a later call.
static INFO_STAMP: AtomicU64 = AtomicU64::new(0);

/// The calls that wait for their replies.
pub(super) struct Calls {
    pub(super) closed: bool,
    next_id: i64,
    pending: HashMap<i64, Pending>,
    /// The downloads of blobs that run, by oid and serial. An entry stays
    /// until the reply of its `sendBlob`, also when its download stops
    /// before: so the connection never runs two downloads of one blob.
    downloads: HashMap<(u64, u64), Download>,
}

/// A download of a blob that runs on the connection.
struct Download {
    /// The message id of its `sendBlob`.
    id: i64,
    /// The channel of its pieces, or `None` when the download stopped
    /// before the reply. The reader then drops the pieces.
    pieces: Option<mpsc::Sender<Piece>>,
}

/// A call of the server for a download.
#[derive(Debug)]
enum Piece {
    Start,
    Chunk(Vec<u8>),
    Stop,
}

/// A call that waits for its reply.
struct Pending {
    /// The reply and the position of the connection when the reply came.
    sender: oneshot::Sender<(Result<Value, Error>, Tid)>,
    /// `true` for `tpc_finish`. The reader raises the last tid to the tid
    /// of its reply before it gives the reply.
    finish: bool,
}

impl Inner {
    /// Call `method` and wait for its reply.
    pub(super) async fn call(&self, method: &str, args: Vec<Value>) -> Result<Value, Error> {
        self.call_at(method, args, false).await.0
    }

    /// Call `method` and wait for its reply. Return the reply and the
    /// position of the connection when the reader got it. `finish` marks a
    /// `tpc_finish`, whose tid the reader adds to the position.
    pub(super) async fn call_at(
        &self,
        method: &str,
        args: Vec<Value>,
        finish: bool,
    ) -> (Result<Value, Error>, Tid) {
        let (sender, reply) = oneshot::channel();
        let id = {
            let mut calls = lock(&self.calls);
            if calls.closed {
                return (Err(Error::Disconnected), self.position());
            }
            let id = calls.next_id;
            calls.next_id += 1;
            calls.pending.insert(id, Pending { sender, finish });
            id
        };
        // The guard forgets the call when its future is dropped.
        let _forget = Forget { inner: self, id };
        let frame = match protocol::call(id, method, args, self.traceparent().as_deref()) {
            Ok(frame) => frame,
            Err(error) => return (Err(error), self.position()),
        };
        if self.outgoing.send(frame).await.is_err() {
            return (Err(Error::Disconnected), self.position());
        }
        reply
            .await
            .unwrap_or_else(|_| (Err(Error::Disconnected), self.position()))
    }

    /// Return the last tid that the connection read from its stream.
    fn position(&self) -> Tid {
        Tid(self.last_tid.load(Ordering::SeqCst))
    }

    /// Send `method` as a call without a reply.
    pub(super) async fn notify(&self, method: &str, args: Vec<Value>) -> Result<(), Error> {
        if lock(&self.calls).closed {
            return Err(Error::Disconnected);
        }
        let frame = protocol::notify(method, args, self.traceparent().as_deref())?;
        self.outgoing
            .send(frame)
            .await
            .map_err(|_| Error::Disconnected)
    }

    /// Download a blob into `sink`. See `Client::send_blob`.
    pub(super) async fn send_blob<W: Write>(
        &self,
        oid: Oid,
        serial: Tid,
        sink: &mut W,
    ) -> Result<u64, Error> {
        let key = (oid.0, serial.0);
        // Take the blob on this connection, or wait for the download that
        // has it.
        let (id, mut pieces, reply) = loop {
            let done = self.downloads_done.notified();
            {
                let mut calls = lock(&self.calls);
                if calls.closed {
                    return Err(Error::Disconnected);
                }
                if !calls.downloads.contains_key(&key) {
                    let id = calls.next_id;
                    calls.next_id += 1;
                    let (sender, reply) = oneshot::channel();
                    calls.pending.insert(
                        id,
                        Pending {
                            sender,
                            finish: false,
                        },
                    );
                    let (pieces, receiver) = mpsc::channel(PIECES);
                    let pieces = Some(pieces);
                    calls.downloads.insert(key, Download { id, pieces });
                    break (id, receiver, reply);
                }
            }
            done.await;
        };
        let mut guard = Downloading {
            inner: self,
            key,
            id,
            sent: false,
        };
        let args = vec![p64(oid.0), p64(serial.0)];
        let frame = protocol::call(id, "sendBlob", args, self.traceparent().as_deref())?;
        if self.outgoing.send(frame).await.is_err() {
            return Err(Error::Disconnected);
        }
        guard.sent = true;
        // The reader drops the sender of the pieces at the reply, after the
        // last piece. So the loop ends at the reply, or at the close.
        let mut size = 0;
        let mut order = Ok(());
        let (mut started, mut stopped) = (false, false);
        let mut written: io::Result<()> = Ok(());
        while let Some(piece) = pieces.recv().await {
            match piece {
                Piece::Start if !started => started = true,
                Piece::Chunk(bytes) if started && !stopped => {
                    size += bytes.len() as u64;
                    if written.is_ok() {
                        written = sink.write_all(&bytes);
                    }
                }
                Piece::Stop if started && !stopped => stopped = true,
                piece => {
                    order = Err(Error::Protocol(format!(
                        "{piece:?} out of order in the download of {oid:?} at {serial:?}"
                    )));
                }
            }
        }
        let (reply, _) = reply
            .await
            .unwrap_or_else(|_| (Err(Error::Disconnected), self.position()));
        reply?;
        order?;
        if !stopped {
            return Err(Error::Protocol(format!(
                "sendBlob of {oid:?} at {serial:?} replied before receiveBlobStop"
            )));
        }
        written?;
        Ok(size)
    }

    /// Return the traceparent of the current `traced` future when the
    /// server accepted `trace_context`, else `None`.
    fn traceparent(&self) -> Option<String> {
        if !self.traces.load(Ordering::SeqCst) {
            return None;
        }
        TRACEPARENT.try_with(Clone::clone).ok().flatten()
    }

    /// Send `method` with `args` as a call whose reply nothing waits for,
    /// without a wait for room in the queue: a drop sends it. The reader
    /// drops the reply, because no call waits for its id. `keep` drops only
    /// after the frame is in the channel.
    pub(super) fn send_later(&self, method: &str, args: Vec<Value>, keep: Box<dyn Any + Send>) {
        let id = {
            let mut calls = lock(&self.calls);
            if calls.closed {
                return;
            }
            let id = calls.next_id;
            calls.next_id += 1;
            id
        };
        let Ok(frame) = protocol::call(id, method, args, None) else {
            return;
        };
        if let Err(mpsc::error::TrySendError::Full(frame)) = self.outgoing.try_send(frame) {
            let outgoing = self.outgoing.clone();
            self.runtime.spawn(Box::pin(async move {
                let _ = outgoing.send(frame).await;
                drop(keep);
            }));
        }
    }

    /// Handle one message from the server. An invalidation in a bad shape
    /// fails with `Error::Protocol`: the reader then closes the connection,
    /// so that no receiver misses a commit without a signal.
    ///
    /// A call of a download returns the channel of its download and the
    /// piece: the reader sends it, and waits while the channel is full.
    fn dispatch(&self, message: Message) -> Result<Option<(mpsc::Sender<Piece>, Piece)>, Error> {
        match message {
            Message::Reply { id, result } => {
                let (pending, download) = {
                    let mut calls = lock(&self.calls);
                    // Most replies come with no download that runs.
                    let download = if calls.downloads.is_empty() {
                        None
                    } else {
                        calls
                            .downloads
                            .iter()
                            .find(|(_, download)| download.id == id)
                            .map(|(key, _)| *key)
                    };
                    let download = download.and_then(|key| calls.downloads.remove(&key));
                    (calls.pending.remove(&id), download)
                };
                // The end of a download frees its blob for the next one.
                if download.is_some() {
                    drop(download);
                    self.downloads_done.notify_waiters();
                }
                if let Some(pending) = pending {
                    if pending.finish
                        && let Ok(reply) = &result
                        && let Some(committed) = u64_of(reply)
                    {
                        self.last_tid.fetch_max(committed, Ordering::SeqCst);
                    }
                    let _ = pending.sender.send((result, self.position()));
                }
            }
            Message::Call { method, args } if method == "invalidateTransaction" => {
                let Some(invalidation) = invalidation(&args) else {
                    return Err(bad("invalidateTransaction", &args));
                };
                self.last_tid
                    .fetch_max(invalidation.tid.0, Ordering::SeqCst);
                if let Some(sender) = &*lock(&self.invalidations) {
                    let _ = sender.send(invalidation);
                }
            }
            Message::Call { method, args } if method == "info" => {
                // An `info` in a bad shape changes nothing, as the client of
                // ZEO logs it and goes on.
                if let Some([Value::Map(entries)]) = args.as_array().map(Vec::as_slice) {
                    let stamp = INFO_STAMP.fetch_add(1, Ordering::SeqCst) + 1;
                    let mut pushed = lock(&self.pushed);
                    let mut merged = match pushed.take() {
                        Some((_, Value::Map(old))) => old,
                        _ => Vec::new(),
                    };
                    merge(&mut merged, entries);
                    *pushed = Some((stamp, Value::Map(merged)));
                }
            }
            Message::Call { method, args }
                if matches!(
                    method.as_str(),
                    "receiveBlobStart" | "receiveBlobChunk" | "receiveBlobStop"
                ) =>
            {
                let (key, piece) = piece(&method, args)?;
                let calls = lock(&self.calls);
                // The pieces of a download that stopped, or that is not of
                // this client, go nowhere.
                let pieces = calls
                    .downloads
                    .get(&key)
                    .and_then(|download| download.pieces.clone());
                return Ok(pieces.map(|pieces| (pieces, piece)));
            }
            // `info` and the other calls of the server change nothing here.
            Message::Call { .. } | Message::Other => {}
        }
        Ok(None)
    }

    /// Close the connection: stop the tasks, and fail each call that waits.
    fn shut(&self) {
        self.closing.send_replace(true);
        let (pending, downloads) = {
            let mut calls = lock(&self.calls);
            calls.closed = true;
            (
                mem::take(&mut calls.pending),
                mem::take(&mut calls.downloads),
            )
        };
        drop(downloads);
        self.downloads_done.notify_waiters();
        let position = self.position();
        for pending in pending.into_values() {
            let _ = pending.sender.send((Err(Error::Disconnected), position));
        }
        lock(&self.invalidations).take();
    }
}

/// Start the three tasks of a connection on `stream`. Return the state
/// that they share, and the receiver that ends when every task stopped.
pub(super) fn start(
    runtime: Arc<dyn Runtime>,
    stream: Box<dyn Stream>,
) -> (Arc<Inner>, mpsc::Receiver<()>) {
    let (outgoing, frames) = mpsc::channel(OUTGOING);
    let (closing, _) = watch::channel(false);
    let (invalidations, _) = broadcast::channel(INVALIDATIONS);
    let inner = Arc::new(Inner {
        runtime: runtime.clone(),
        calls: Mutex::new(Calls {
            closed: false,
            next_id: 1,
            pending: HashMap::new(),
            downloads: HashMap::new(),
        }),
        outgoing,
        closing,
        last_tid: AtomicU64::new(0),
        invalidations: Mutex::new(Some(invalidations)),
        commit: Arc::new(tokio::sync::Mutex::new(())),
        next_txn: AtomicU64::new(1),
        traces: AtomicBool::new(false),
        downloads_done: Notify::new(),
        pushed: Mutex::new(None),
    });
    let (alive, tasks) = mpsc::channel(1);
    let (reader, writer) = tokio::io::split(stream);
    runtime.spawn(Box::pin(read(inner.clone(), reader, alive.clone())));
    runtime.spawn(Box::pin(write(
        inner.clone(),
        writer,
        frames,
        alive.clone(),
    )));
    runtime.spawn(Box::pin(beat(inner.clone(), alive)));
    (inner, tasks)
}

/// Removes a call from the pending calls when its future ends.
struct Forget<'a> {
    inner: &'a Inner,
    id: i64,
}

impl Drop for Forget<'_> {
    fn drop(&mut self) {
        lock(&self.inner.calls).pending.remove(&self.id);
    }
}

/// Ends the hold of a download on its blob when its future ends. A
/// download whose `sendBlob` went out keeps the blob until the reply, and
/// its pieces go nowhere. A download whose `sendBlob` never went out frees
/// it at once.
struct Downloading<'a> {
    inner: &'a Inner,
    key: (u64, u64),
    id: i64,
    sent: bool,
}

impl Drop for Downloading<'_> {
    fn drop(&mut self) {
        let removed = {
            let mut calls = lock(&self.inner.calls);
            calls.pending.remove(&self.id);
            match calls.downloads.get_mut(&self.key) {
                Some(download) if download.id == self.id && self.sent => {
                    download.pieces = None;
                    None
                }
                Some(download) if download.id == self.id => calls.downloads.remove(&self.key),
                _ => None,
            }
        };
        if removed.is_some() {
            drop(removed);
            self.inner.downloads_done.notify_waiters();
        }
    }
}

/// Wait until the connection closes.
pub(super) async fn closed(closing: &mut watch::Receiver<bool>) {
    let _ = closing.wait_for(|closing| *closing).await;
}

/// The reader task: read the frames of the server until the end of the
/// stream, an error, a bad frame or the close of the connection.
async fn read(inner: Arc<Inner>, mut stream: ReadHalf<Box<dyn Stream>>, _alive: mpsc::Sender<()>) {
    let mut closing = inner.closing.subscribe();
    loop {
        let frame = tokio::select! {
            biased;
            () = closed(&mut closing) => break,
            frame = protocol::read_payload(&mut stream) => frame,
        };
        let Ok(Some(payload)) = frame else { break };
        match protocol::decode_payload(payload).and_then(|message| inner.dispatch(message)) {
            Ok(None) => {}
            Ok(Some((pieces, piece))) => {
                // A download that stopped drops its receiver: the send then
                // fails, and the reader goes on.
                tokio::select! {
                    biased;
                    () = closed(&mut closing) => break,
                    _ = pieces.send(piece) => {}
                }
            }
            Err(_) => break,
        }
    }
    inner.shut();
}

/// The writer task: write the frames of the channel in order until the
/// close of the connection or an error.
async fn write(
    inner: Arc<Inner>,
    mut stream: WriteHalf<Box<dyn Stream>>,
    mut frames: mpsc::Receiver<Vec<u8>>,
    _alive: mpsc::Sender<()>,
) {
    let mut closing = inner.closing.subscribe();
    loop {
        let frame = tokio::select! {
            biased;
            () = closed(&mut closing) => break,
            frame = frames.recv() => frame,
        };
        let Some(frame) = frame else { break };
        let written = tokio::select! {
            biased;
            () = closed(&mut closing) => break,
            written = stream.write_all(&frame) => written,
        };
        if written.is_err() {
            break;
        }
    }
    inner.shut();
    let _ = stream.shutdown().await;
}

/// The heartbeat task: send a heartbeat every `HEARTBEAT_INTERVAL`, busy
/// or idle, as a stock client does.
async fn beat(inner: Arc<Inner>, _alive: mpsc::Sender<()>) {
    let mut closing = inner.closing.subscribe();
    loop {
        tokio::select! {
            biased;
            () = closed(&mut closing) => break,
            () = inner.runtime.sleep(HEARTBEAT_INTERVAL) => {}
        }
        let sent = tokio::select! {
            biased;
            () = closed(&mut closing) => break,
            sent = inner.outgoing.send(protocol::heartbeat()) => sent,
        };
        if sent.is_err() {
            break;
        }
    }
}

/// Read the args `[tid, [oid, ...]]` of `invalidateTransaction`.
pub(super) fn invalidation(args: &Value) -> Option<Invalidation> {
    let [tid, oids] = <&[Value; 2]>::try_from(args.as_array()?.as_slice()).ok()?;
    Some(Invalidation {
        tid: Tid(u64_of(tid)?),
        oids: oids
            .as_array()?
            .iter()
            .map(|oid| u64_of(oid).map(Oid))
            .collect::<Option<_>>()?,
    })
}

/// Read the args of a call of a download: `[oid, serial]`, and the chunk
/// for `receiveBlobChunk`. Return the oid and the serial, and the piece.
fn piece(method: &str, args: Value) -> Result<((u64, u64), Piece), Error> {
    let Value::Array(mut items) = args else {
        return Err(bad(method, &args));
    };
    let piece = match (method, items.len()) {
        ("receiveBlobStart", 2) => Piece::Start,
        ("receiveBlobStop", 2) => Piece::Stop,
        ("receiveBlobChunk", 3) => match items.pop() {
            Some(Value::Binary(bytes)) => Piece::Chunk(bytes),
            _ => return Err(bad(method, &Value::Array(items))),
        },
        _ => return Err(bad(method, &Value::Array(items))),
    };
    match (u64_of(&items[0]), u64_of(&items[1])) {
        (Some(oid), Some(serial)) => Ok(((oid, serial), piece)),
        _ => Err(bad(method, &Value::Array(items))),
    }
}
