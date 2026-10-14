//! The protocol layer: msgpack frames, the handshake and the method table.
//!
//! Each connection has one tokio task that reads its frames, one task that
//! writes them, and one thread of the blocking pool that runs its calls in
//! order. That thread runs every core call of the connection, so one
//! thread drives each core transaction, and no core call runs on a thread
//! of the async runtime.
//!
//! The calls of a client wait in a queue of `QUEUE` calls. The frames for
//! the client wait in two queues: the replies of the session, at most
//! `QUEUE` frames, and the invalidations with the frames of each commit, at
//! most `client-queue-size` frames. The reader waits for room for the calls,
//! and the session waits for room for its replies. So an upload or a
//! download holds a few chunks in memory. The invalidations never wait: a
//! full queue closes the connection. The writer task sends the frames of
//! both queues in the order in which they went in.

mod blob;
mod clock;
mod extension;
pub(crate) mod frame;
mod invalidation;
mod iteration;
mod message;
mod outbox;
mod session;
mod status;

/// The entry of the fuzz targets.
#[cfg(any(test, feature = "fuzz"))]
pub mod fuzz;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio::net::TcpStream;
use tokio::sync::{Notify, mpsc, watch};
use zoe_core::Store;

use invalidation::Invalidations;
use outbox::Outbox;
use status::Stats;

/// The protocol that the server announces and the only one it takes: ZEO 6
/// with msgpack.
pub(crate) const PROTOCOL: &[u8] = b"M5";

/// The payload that a client sends in place of a protocol to ask for the
/// status of the server.
const RUOK: &[u8] = b"ruok";

/// The most calls that wait for the session of a connection, decoded, and
/// the most replies that wait in its session queue. A full queue makes its
/// sender wait. When the call queue is full, the reader reads no more
/// bytes, so TCP makes the client wait. So a connection holds at most this
/// many decoded calls, the call that the reader holds, and the call that
/// the session runs. An upload or a download holds a few chunks in memory,
/// whatever the size of the blob. The session runs one call at a time and
/// needs only the next calls ready: the bytes of a burst wait in the
/// buffers of TCP.
///
/// The writer task empties the queues of the frames without the reader, so
/// the reader, the session and the writer never wait for each other in a
/// circle. Only a client that sends calls and reads no reply can stop its
/// own connection: its replies fill the session queue, then its calls fill
/// the call queue. A stock client reads its replies while it sends.
const QUEUE: usize = 4;

/// The name of the storage that `--path` gives. It is the default of ZEO.
pub(crate) const STORAGE_NAME: &str = "1";

/// The bounds of the queues of a storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Queues {
    /// The most transactions that `getInvalidations` covers. Past it, the
    /// reply is nil.
    pub(crate) invalidations: usize,
    /// The most invalidations that wait for one client. Past it, the server
    /// closes the connection. The replies of the session wait in a queue of
    /// `QUEUE` frames, and the session waits for room there.
    pub(crate) client: usize,
}

impl Queues {
    /// The bounds of a server that gets no option: 100,000 transactions
    /// and 1,000 messages.
    pub(crate) const DEFAULT: Self = Self {
        invalidations: 100_000,
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
    stats: Stats,
    /// The publication of the commits to the connections.
    invalidations: Invalidations,
    queues: Queues,
    /// True when the server opened the store for reads only.
    read_only: bool,
    /// True when the clients share the blob directory of the store, so
    /// that `storeBlobShared` is accepted.
    shared_blob_dir: bool,
}

impl NamedStorage {
    /// Serve `store`, in the directory `path`, under the name `name`, with
    /// the bounds `queues`. The status starts now.
    pub(crate) fn new(name: &str, path: &Path, store: Arc<Store>, queues: Queues) -> Self {
        Self {
            name: name.to_owned(),
            path: path.to_path_buf(),
            invalidations: Invalidations::new(store.last_transaction()),
            store,
            stats: Stats::new(SystemTime::now()),
            queues,
            read_only: false,
            shared_blob_dir: false,
        }
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

    /// Return the status of the storage. It reads the store.
    fn status(&self) -> status::Status {
        self.stats.status(self.store.last_transaction())
    }

    /// Return the size of the files in the store directory, in bytes, as
    /// `zoe.Storage.getSize` gives it. The blob directory is not counted.
    fn size(&self) -> io::Result<u64> {
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
}

impl Storages {
    /// Serve `storages`, whose names must differ. At most `client_queue`
    /// invalidations wait for one client.
    pub(crate) fn new(storages: Vec<Arc<NamedStorage>>, client_queue: usize) -> Self {
        Self {
            storages,
            client_queue,
        }
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

    /// Wait until the server stops. When the sender drops and sent no
    /// stop, wait forever.
    pub(crate) async fn wait(&mut self) {
        if self.0.wait_for(|stop| *stop).await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

/// Wait for `timeout`, or forever when it is `None`.
async fn idle(timeout: Option<Duration>) {
    match timeout {
        Some(timeout) => tokio::time::sleep(timeout).await,
        None => std::future::pending().await,
    }
}

/// Serve one client on `stream` until it closes, until the server closes
/// the connection, or until `stopping` tells that the server stops. The
/// client selects one of `storages` with `register`. A frame longer than
/// `max_frame_size` bytes closes the connection before the server reserves
/// memory for it.
///
/// The server closes the connection when the client sends no frame for
/// `idle_timeout`: no protocol after the accept, or no frame after the last
/// frame. The time while the reader waits for room in the call queue does
/// not count, because the client cannot send then. The time counts from
/// the last whole frame, so a frame that takes longer than the timeout to
/// arrive is cut. This close is an end of the connection, as a close of the
/// hub is: the writer stops at once, and the session ends at the first
/// reply that it cannot queue. `None` closes no idle connection.
///
/// At a stop, the reader reads no more frames. The session runs the calls
/// that the reader already decoded, so a `tpc_finish` that the server
/// received commits and gets its reply. Then the session ends, and that
/// aborts its open transaction. The writer sends the frames that wait,
/// and closes the connection.
pub(crate) async fn serve_connection(
    stream: TcpStream,
    storages: Arc<Storages>,
    max_frame_size: u32,
    idle_timeout: Option<Duration>,
    mut stopping: Stopping,
) {
    let peer = stream
        .peer_addr()
        .map_or_else(|_| "a client".to_owned(), |addr| addr.to_string());
    let (mut reader, mut writer) = stream.into_split();
    if frame::write(&mut writer, PROTOCOL).await.is_err() {
        return;
    }
    // A client that does not answer the protocol does not hold a stop.
    let protocol = tokio::select! {
        protocol = frame::read(&mut reader, max_frame_size) => protocol,
        () = stopping.wait() => return,
        () = idle(idle_timeout) => {
            eprintln!("zoe serve: {peer}: no protocol within the idle timeout: closing the connection");
            return;
        }
    };
    match protocol {
        Ok(Some(protocol)) if protocol == PROTOCOL => {}
        Ok(Some(protocol)) if protocol == RUOK => {
            // The status reads the stores, so it runs on the blocking pool.
            let reply = tokio::task::spawn_blocking(move || storages.ruok()).await;
            if let Ok(reply) = reply {
                let _ = frame::write(&mut writer, reply.as_bytes()).await;
            }
            let _ = tokio::io::AsyncWriteExt::shutdown(&mut writer).await;
            return;
        }
        Ok(Some(_)) => {
            eprintln!("zoe serve: {peer}: the client does not speak the protocol M5");
            return;
        }
        Ok(None) => return,
        Err(error) => {
            eprintln!("zoe serve: {peer}: {error}");
            return;
        }
    }

    let (calls, received) = mpsc::channel(QUEUE);
    let (outbox, mut inbox, mut closing) = Outbox::new(QUEUE, storages.client_queue);
    // The reader closes the connection with this outbox at the idle
    // timeout. It drops it before it waits for the writer, because the
    // writer ends only when every outbox is gone.
    let idle_close = outbox.clone();
    let closed = Arc::new(Notify::new());
    let worker = {
        let closed = Arc::clone(&closed);
        tokio::task::spawn_blocking(move || {
            session::run(storages, received, outbox);
            closed.notify_one();
        })
    };
    let writing = {
        let mut closing = closing.clone();
        tokio::spawn(async move {
            // The writer stops at once when the server closes the
            // connection. Otherwise it sends every frame, until the session
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
                let written = tokio::select! {
                    biased;
                    () = closing.wait() => break,
                    written = frame::write(&mut writer, frame.as_slice()) => written,
                };
                if written.is_err() {
                    break;
                }
            }
            // A session that waits for room in its queue stops.
            drop(inbox);
            let _ = tokio::io::AsyncWriteExt::shutdown(&mut writer).await;
        })
    };

    loop {
        // An end of the connection or a stop wins over a frame that waits.
        // A frame that arrived wins over the idle timeout. The idle time
        // starts again for each frame, after the call queue took the call.
        let read = tokio::select! {
            biased;
            () = closed.notified() => break,
            () = closing.wait() => break,
            () = stopping.wait() => break,
            read = frame::read(&mut reader, max_frame_size) => read,
            () = idle(idle_timeout) => {
                eprintln!("zoe serve: {peer}: no frame within the idle timeout: closing the connection");
                // The writer stops at once. So a session that waits for
                // room in its queue stops too, and its transaction aborts,
                // also when the client reads nothing.
                idle_close.close();
                break;
            }
        };
        let bytes = match read {
            Ok(Some(bytes)) => bytes,
            Ok(None) => break,
            Err(error) => {
                eprintln!("zoe serve: {peer}: {error}");
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
                eprintln!("zoe serve: {peer}: a frame is not a call: {error}");
                break;
            }
        };
        if call.is_heartbeat() {
            continue;
        }
        // The reader waits while `QUEUE` calls wait for the session.
        let sent = tokio::select! {
            sent = calls.send(call) => sent.is_ok(),
            () = closed.notified() => false,
            () = closing.wait() => false,
        };
        if !sent {
            break;
        }
    }
    // The session ends when its channel closes. Dropping its transaction
    // aborts it.
    drop(calls);
    drop(idle_close);
    let _ = worker.await;
    let _ = writing.await;
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
        client
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        assert_eq!(read_frame(&mut client), PROTOCOL);
        write_frame(&mut client, PROTOCOL);
        send(
            &mut client,
            1,
            "register",
            vec![Value::from(name), Value::Boolean(false)],
        );
        read_frame(&mut client);
        client
    }

    /// Begin the transaction 7 and store `oid` as a new object.
    fn begin_and_store(client: &mut Client, oid: u64) {
        let begin = vec![Value::from(7), Value::Nil, Value::Nil, Value::Map(vec![])];
        send(client, 0, "tpc_begin", begin);
        let data = Value::Binary(b"pickle".to_vec());
        let store = vec![session::p64(oid), session::p64(0), data, Value::from(7)];
        send(client, 0, "storea", store);
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
        let end = vec![session::p64(2), session::p64(0), data, Value::from(7)];
        send(&mut ended, 0, "storeBlobEnd", end);
        for client in [&mut open, &mut ended] {
            send(client, 2, "ping", vec![]);
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
    fn the_idle_time_does_not_count_while_the_call_queue_is_full() {
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
        // The vote of the client waits for the lock. Its next calls fill the
        // call queue, and the reader waits for room.
        let mut client = registered(server.addr());
        begin_and_store(&mut client, 1);
        send(&mut client, 2, "vote", vec![Value::from(7)]);
        std::thread::sleep(Duration::from_millis(100));
        for id in 3..=3 + QUEUE as i64 {
            send(&mut client, id, "ping", vec![]);
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
        let store_large = vec![session::p64(1), session::p64(0), large, Value::from(1)];
        send(&mut setup, 0, "storea", store_large);
        send(&mut setup, 2, "vote", vec![Value::from(1)]);
        read_frame(&mut setup);
        send(&mut setup, 3, "tpc_finish", vec![Value::from(1)]);
        let frames = read_until_closed(&mut setup);
        assert_eq!(frames.first().map(|(m, _)| m.as_str()), Some("info"));

        // The client has a receive buffer of 4 KiB. It votes on the oid 2,
        // then loads the large record 6 times and reads nothing. The writer
        // waits on the first reply, the session queue holds 4, and the
        // session waits for room with the last. The call queue is empty, so
        // the reader waits for a frame.
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
        for id in 3..=8 {
            let load = vec![session::p64(1), session::p64(u64::MAX >> 1)];
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
}
