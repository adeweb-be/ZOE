//! `zoe serve` as a library: [`Server`] serves stores with the ZEO 6
//! msgpack protocol, each under its own storage name.
//!
//! A client selects a store with the storage name that it gives to
//! `register`. Each store has its own lock manager and its own
//! invalidations, so commits on different stores never wait for each
//! other.
//!
//! The binary runs it until SIGTERM or SIGINT. A test runs it in its own
//! process with [`Server::start`] and [`Server::stop`].
//!
//! A stop takes at most the shutdown timeout. The server stops accepting
//! connections, and each connection reads no more calls. Each session runs
//! the calls that the server already received, so a `tpc_finish` that the
//! server received commits and gets its reply. Then each session ends and
//! aborts its open transaction, and each connection sends the frames that
//! wait and closes. Then the stores close.

use std::fmt;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use tokio::net::{TcpListener, TcpSocket};
use tokio::runtime::Runtime;
use tokio::sync::watch;
use zoe_core::{Options, Store, ZoeError};

use crate::protocol::{self, NamedStorage, Queues, Stopping, Storages};

/// The default of `--max-frame-size`: 256 MiB. A `storea` or `restorea`
/// frame holds a whole record, and the store sets no limit on a record by
/// default. ZODB warns about a record of more than 16 MiB, and a blob chunk
/// is less than 64 KiB. So a stock client stays far below this limit.
pub const DEFAULT_MAX_FRAME_SIZE: NonZeroU32 = NonZeroU32::new(256 << 20).unwrap();

/// The default of `--shutdown-timeout`: 30 seconds.
pub const DEFAULT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);

/// The default of `--idle-timeout`: 300 seconds. A stock client sends a
/// heartbeat every 60 seconds, so it keeps its connection.
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// The last part of the shutdown timeout, in which a stop closes the
/// connections that did not end and waits for their sessions. When the
/// timeout is shorter than twice this part, the stop uses the last half of
/// the timeout.
const FORCED_CLOSE: Duration = Duration::from_secs(1);

/// The longest shutdown timeout: 2^32 - 1 seconds, about 136 years. A
/// longer timeout counts as this one, so the deadline of a stop never
/// overflows the clock.
const LONGEST_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(4_294_967_295);

/// A store that a server serves under a name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageOptions {
    name: String,
    path: PathBuf,
    blob_dir: Option<PathBuf>,
    read_only: bool,
}

impl StorageOptions {
    /// Serve the store in `path` under the storage name `name`. The blob
    /// files go to `blobs` in the store directory.
    pub fn new(name: impl Into<String>, path: impl AsRef<Path>) -> Self {
        Self {
            name: name.into(),
            path: path.as_ref().to_path_buf(),
            blob_dir: None,
            read_only: false,
        }
    }

    /// Keep the blob files of the store in `dir`. Each open of the store
    /// must give the same directory.
    #[must_use]
    pub fn blob_dir(mut self, dir: impl AsRef<Path>) -> Self {
        self.blob_dir = Some(dir.as_ref().to_path_buf());
        self
    }

    /// Serve the store for reads only, when `read_only` is true. The server
    /// opens the existing store with `Store::open_read_only`: it changes no
    /// byte of the store and holds its lock. A read-only store keeps its
    /// blob files in `blobs` in the store directory, so it takes no blob
    /// directory.
    #[must_use]
    pub fn read_only(mut self, read_only: bool) -> Self {
        self.read_only = read_only;
        self
    }
}

/// The settings of a server.
#[derive(Debug, Clone)]
pub struct ServeOptions {
    storages: Vec<StorageOptions>,
    listen: SocketAddr,
    queues: Queues,
    lock_timeout: Option<Duration>,
    max_frame_size: NonZeroU32,
    shutdown_timeout: Duration,
    idle_timeout: Duration,
    read_only: bool,
}

impl ServeOptions {
    /// Serve the store in `path` under the storage name `1`, the default of
    /// ZEO. Listen on a free port of 127.0.0.1, with frames of at most
    /// [`DEFAULT_MAX_FRAME_SIZE`] bytes. The invalidation queue covers
    /// 100,000 transactions, and 1,000 invalidations can wait for one
    /// client. A stop takes at most [`DEFAULT_SHUTDOWN_TIMEOUT`]. A
    /// connection that sends no frame for [`DEFAULT_IDLE_TIMEOUT`] closes.
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self::with_storages([StorageOptions::new(protocol::STORAGE_NAME, path)])
    }

    /// Serve `storages`, with the defaults of [`ServeOptions::new`]. The
    /// names must differ, and one storage at least is necessary.
    pub fn with_storages(storages: impl IntoIterator<Item = StorageOptions>) -> Self {
        Self {
            storages: storages.into_iter().collect(),
            listen: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            queues: Queues::DEFAULT,
            lock_timeout: None,
            max_frame_size: DEFAULT_MAX_FRAME_SIZE,
            shutdown_timeout: DEFAULT_SHUTDOWN_TIMEOUT,
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
            read_only: false,
        }
    }

    /// Listen on `addr`. A port of 0 takes a free port.
    #[must_use]
    pub fn listen(mut self, addr: SocketAddr) -> Self {
        self.listen = addr;
        self
    }

    /// Let `getInvalidations` cover at most `transactions` commits. A
    /// client that is further behind gets nil and verifies its cache.
    #[must_use]
    pub fn invalidation_queue_size(mut self, transactions: usize) -> Self {
        self.queues.invalidations = transactions;
        self
    }

    /// Let at most `messages` invalidations wait for one client. Past it,
    /// the server closes the connection of the client. A size of 0 lets 1
    /// message wait.
    #[must_use]
    pub fn client_queue_size(mut self, messages: usize) -> Self {
        self.queues.client = messages;
        self
    }

    /// Fail a vote that waits longer than `timeout` for the lock of an
    /// object with `ConflictError`. Without this call, the store keeps its
    /// default.
    #[must_use]
    pub fn lock_timeout(mut self, timeout: Duration) -> Self {
        self.lock_timeout = Some(timeout);
        self
    }

    /// Close a connection that sends a frame longer than `bytes`, before
    /// the server reserves memory for the frame.
    #[must_use]
    pub fn max_frame_size(mut self, bytes: NonZeroU32) -> Self {
        self.max_frame_size = bytes;
        self
    }

    /// Let a stop of the server take at most `timeout`. A stop first waits
    /// for each connection to end by itself. At the end of the timeout less
    /// 1 second, or less half the timeout when that is shorter, it drops
    /// the connections that remain. Then it waits for their sessions until
    /// the end of the timeout. A session that still runs then keeps the
    /// store of that session open: that store does not close cleanly. The
    /// other stores close.
    #[must_use]
    pub fn shutdown_timeout(mut self, timeout: Duration) -> Self {
        self.shutdown_timeout = timeout;
        self
    }

    /// Close a connection that sends no frame for `timeout`. The time counts
    /// from the last frame that the server read, a heartbeat included, and
    /// from the accept for the protocol of the client. The time while the
    /// call queue of the connection is full does not count. The close stops
    /// the writer at once, as a close of the hub does. The session runs the
    /// calls that arrived, up to the first reply that it cannot queue, then
    /// aborts its open transaction. A timeout of 0 closes no connection.
    #[must_use]
    pub fn idle_timeout(mut self, timeout: Duration) -> Self {
        self.idle_timeout = timeout;
        self
    }

    /// Serve every store for reads only, when `read_only` is true, as
    /// [`StorageOptions::read_only`] does for one store.
    #[must_use]
    pub fn read_only(mut self, read_only: bool) -> Self {
        self.read_only = read_only;
        self
    }

    /// Tell if the server serves the store of `storage` for reads only.
    fn is_read_only(&self, storage: &StorageOptions) -> bool {
        self.read_only || storage.read_only
    }

    /// The options of the store of `storage`.
    fn store_options(&self, storage: &StorageOptions) -> Options {
        let mut options = Options::default();
        if let Some(timeout) = self.lock_timeout {
            options.lock_timeout = timeout;
        }
        options.blob_dir.clone_from(&storage.blob_dir);
        options
    }

    /// Check that one storage at least is given, that no name comes twice,
    /// and that no read-only storage has a blob directory.
    fn check_storages(&self) -> Result<(), Error> {
        if self.storages.is_empty() {
            return Err(Error::Storages("no storage to serve".to_owned()));
        }
        for (n, storage) in self.storages.iter().enumerate() {
            if self.storages[..n].iter().any(|s| s.name == storage.name) {
                return Err(Error::Storages(format!(
                    "the storage name {} comes twice",
                    storage.name
                )));
            }
            if self.is_read_only(storage) && storage.blob_dir.is_some() {
                return Err(Error::Storages(read_only_blob_dir(&storage.name)));
            }
        }
        Ok(())
    }
}

/// The error of the read-only storage `name` that has a blob directory.
pub(crate) fn read_only_blob_dir(name: &str) -> String {
    format!(
        "the storage {name} is read-only, and a read-only store keeps its blob files \
         in blobs in the store directory: it takes no blob directory"
    )
}

/// Why a server did not start.
#[derive(Debug)]
pub enum Error {
    /// The options give no storage, or give a storage name twice.
    Storages(String),
    /// The server cannot listen on the address.
    Listen(SocketAddr, io::Error),
    /// The store does not open.
    Store(PathBuf, ZoeError),
    /// The async runtime does not start.
    Runtime(io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Storages(message) => f.write_str(message),
            Self::Listen(addr, error) => write!(f, "cannot listen on {addr}: {error}"),
            Self::Store(path, error) => write!(f, "{}: {error}", path.display()),
            Self::Runtime(error) => write!(f, "cannot start the runtime: {error}"),
        }
    }
}

impl std::error::Error for Error {}

/// A running server. Dropping it stops it.
pub struct Server {
    runtime: Option<Runtime>,
    storages: Option<Arc<Storages>>,
    addr: SocketAddr,
    /// The sender of the stop to the accept loop and to each connection.
    stop_signal: watch::Sender<bool>,
    /// The accept loop and the connections that run.
    tasks: Arc<Tasks>,
    shutdown_timeout: Duration,
}

impl Server {
    /// Listen, open the stores and accept clients. Return once the server
    /// accepts connections. When a store does not open, close the stores
    /// that opened and fail.
    pub fn start(options: ServeOptions) -> Result<Self, Error> {
        Self::start_with(options, |_, path, store_options| {
            Store::open_with(path, store_options)
                .map(Arc::new)
                .map_err(|error| Error::Store(path.to_path_buf(), error))
        })
    }

    /// Start as [`Server::start`] does, but open the store of each storage
    /// that is not read-only with `open`. It gets the storage name, the
    /// store directory and the options of the store.
    pub(crate) fn start_with(
        options: ServeOptions,
        mut open: impl FnMut(&str, &Path, Options) -> Result<Arc<Store>, Error>,
    ) -> Result<Self, Error> {
        options.check_storages()?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("zoe-serve")
            .build()
            .map_err(Error::Runtime)?;
        // Listen first: a server that cannot listen changes nothing on
        // disk.
        let listener = {
            let _enter = runtime.enter();
            bind(options.listen).map_err(|error| Error::Listen(options.listen, error))?
        };
        let addr = listener
            .local_addr()
            .map_err(|error| Error::Listen(options.listen, error))?;
        // This thread is not a thread of the runtime. Each store has its
        // own lock manager, and each storage its own invalidations.
        let mut storages = Vec::with_capacity(options.storages.len());
        for storage in &options.storages {
            let read_only = options.is_read_only(storage);
            let store = if read_only {
                Store::open_read_only(&storage.path)
                    .map(Arc::new)
                    .map_err(|error| Error::Store(storage.path.clone(), error))?
            } else {
                open(&storage.name, &storage.path, options.store_options(storage))?
            };
            let named = NamedStorage::new(&storage.name, &storage.path, store, options.queues);
            storages.push(Arc::new(named.read_only(read_only)));
        }
        let storages = Arc::new(Storages::new(storages, options.queues.client));
        let (stop_signal, stopping) = Stopping::new();
        let tasks = Arc::new(Tasks::default());
        // The accept loop counts as a task from now on, so a stop waits for
        // it.
        let accepting = tasks.start();
        let accept = accept(
            listener,
            Arc::clone(&storages),
            options.max_frame_size.get(),
            (!options.idle_timeout.is_zero()).then_some(options.idle_timeout),
            stopping,
            Arc::clone(&tasks),
        );
        runtime.spawn(async move {
            accept.await;
            drop(accepting);
        });
        Ok(Self {
            runtime: Some(runtime),
            storages: Some(storages),
            addr,
            stop_signal,
            tasks,
            shutdown_timeout: options.shutdown_timeout,
        })
    }

    /// The address that the server listens on.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Stop the server within its shutdown timeout. Accept no more
    /// connections. Commit each `tpc_finish` that the server received and
    /// send its reply. Abort the other open transactions, close every
    /// connection and close the stores.
    pub fn stop(mut self) {
        self.shutdown();
    }

    /// Call `ready` with the address, then serve until SIGTERM or SIGINT.
    /// Then stop.
    pub(crate) fn serve_until_signal(
        mut self,
        ready: impl FnOnce(SocketAddr) -> io::Result<()>,
    ) -> io::Result<()> {
        use tokio::signal::unix::{SignalKind, signal};

        let runtime = self.runtime.as_ref().expect("the server runs");
        let (mut term, mut interrupt) = {
            let _enter = runtime.enter();
            (
                signal(SignalKind::terminate())?,
                signal(SignalKind::interrupt())?,
            )
        };
        ready(self.addr)?;
        runtime.block_on(async {
            tokio::select! {
                _ = term.recv() => {}
                _ = interrupt.recv() => {}
            }
        });
        self.shutdown();
        Ok(())
    }

    fn shutdown(&mut self) {
        let Some(runtime) = self.runtime.take() else {
            return;
        };
        let timeout = self.shutdown_timeout.min(LONGEST_SHUTDOWN_TIMEOUT);
        let deadline = Instant::now() + timeout;
        // The accept loop drops the listener, and each connection reads no
        // more calls. The sessions run the calls that wait, then end.
        self.stop_signal.send_replace(true);
        let forced = timeout - FORCED_CLOSE.min(timeout / 2);
        if self.tasks.wait(forced) > 0 {
            eprintln!("zoe serve: the shutdown timeout ends: closing the connections that remain");
        }
        // The stop of the runtime drops the tasks that remain, so their
        // connections close and their sessions end after the current call.
        // It waits for the sessions until the deadline, and not longer.
        // Tokio refuses that wait on a thread of an async runtime, as in an
        // async test, so another thread waits.
        let rest = deadline.saturating_duration_since(Instant::now());
        std::thread::scope(|scope| {
            scope.spawn(move || runtime.shutdown_timeout(rest));
        });
        // A session that runs holds the store of its storage. This drop
        // closes every other store.
        if let Some(storages) = self.storages.take() {
            for name in Storages::held(&storages) {
                eprintln!(
                    "zoe serve: a session still runs at the end of the shutdown timeout; \
                     the store of the storage {name} does not close cleanly"
                );
            }
            drop(storages);
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Listen on `addr`. The socket takes SO_REUSEADDR, so a restarted server
/// binds the same port at once.
fn bind(addr: SocketAddr) -> io::Result<TcpListener> {
    let socket = if addr.is_ipv4() {
        TcpSocket::new_v4()?
    } else {
        TcpSocket::new_v6()?
    };
    socket.set_reuseaddr(true)?;
    socket.bind(addr)?;
    socket.listen(1024)
}

/// Accept clients until `stopping` tells that the server stops. Then drop
/// the listener, so the server accepts no more connections. Each connection
/// takes frames of at most `max_frame_size` bytes, closes after
/// `idle_timeout` without a frame, and counts in `tasks` while it runs.
async fn accept(
    listener: TcpListener,
    storages: Arc<Storages>,
    max_frame_size: u32,
    idle_timeout: Option<Duration>,
    mut stopping: Stopping,
    tasks: Arc<Tasks>,
) {
    loop {
        let accepted = tokio::select! {
            biased;
            () = stopping.wait() => break,
            accepted = listener.accept() => accepted,
        };
        match accepted {
            Ok((stream, _)) => {
                let _ = stream.set_nodelay(true);
                let running = tasks.start();
                let connection = protocol::serve_connection(
                    stream,
                    Arc::clone(&storages),
                    max_frame_size,
                    idle_timeout,
                    stopping.clone(),
                );
                tokio::spawn(async move {
                    connection.await;
                    drop(running);
                });
            }
            Err(error) => {
                eprintln!("zoe serve: accept failed: {error}");
                tokio::task::yield_now().await;
            }
        }
    }
}

/// The count of the tasks that serve: the accept loop and one task for
/// each connection. A stop waits until no task runs.
#[derive(Default)]
struct Tasks {
    running: Mutex<usize>,
    ended: Condvar,
}

/// The mark of a task that runs. Its drop counts the end of the task. When
/// the stop of the runtime drops the task, it drops the mark too.
struct Running(Arc<Tasks>);

impl Tasks {
    /// Count a task that starts.
    fn start(self: &Arc<Self>) -> Running {
        *self.lock() += 1;
        Running(Arc::clone(self))
    }

    /// Wait at most `timeout` until no task runs. Return the number of
    /// tasks that still run.
    fn wait(&self, timeout: Duration) -> usize {
        let (running, _) = self
            .ended
            .wait_timeout_while(self.lock(), timeout, |running| *running > 0)
            .unwrap_or_else(PoisonError::into_inner);
        *running
    }

    fn lock(&self) -> MutexGuard<'_, usize> {
        self.running.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let mut running = self.0.lock();
        *running -= 1;
        if *running == 0 {
            self.0.ended.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zoe-serve-lib-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn stop_closes_the_store_also_in_an_async_context() {
        let dir = fresh_dir("async-stop");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let server = Server::start(ServeOptions::new(&dir)).unwrap();
            assert!(server.addr().ip().is_loopback());
            assert_ne!(server.addr().port(), 0);
            server.stop();
        });
        // The lock of the store is free again.
        Store::open(&dir).unwrap();
    }

    #[test]
    fn a_client_that_does_not_answer_the_protocol_does_not_hold_a_stop() {
        use std::io::Read;
        use std::net::TcpStream;

        let dir = fresh_dir("silent");
        let server = Server::start(ServeOptions::new(&dir)).unwrap();
        let mut silent = TcpStream::connect(server.addr()).unwrap();
        silent
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        let mut protocol = [0u8; 6];
        silent.read_exact(&mut protocol).unwrap();
        assert_eq!(&protocol, b"\0\0\0\x02M5");

        let started = Instant::now();
        server.stop();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        // The server closed the connection and the store.
        assert_eq!(silent.read(&mut protocol).unwrap(), 0);
        Store::open_read_only(&dir).unwrap();
    }

    #[test]
    fn a_stop_with_a_timeout_of_0_closes_the_connections_at_once() {
        use std::io::Read;
        use std::net::TcpStream;

        let dir = fresh_dir("timeout-0");
        let options = ServeOptions::new(&dir).shutdown_timeout(Duration::ZERO);
        let server = Server::start(options).unwrap();
        let mut client = TcpStream::connect(server.addr()).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        let mut protocol = [0u8; 6];
        client.read_exact(&mut protocol).unwrap();
        server.stop();
        // The connection ends, and the store closes when its last session
        // ends.
        let read = client.read(&mut protocol);
        assert!(
            matches!(&read, Ok(0))
                || read
                    .as_ref()
                    .is_err_and(|e| e.kind() == io::ErrorKind::ConnectionReset),
            "{read:?}"
        );
        let deadline = Instant::now() + Duration::from_secs(30);
        while Store::open_read_only(&dir).is_err() {
            assert!(Instant::now() < deadline, "the store stays open");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn a_shutdown_timeout_past_the_clock_counts_as_the_longest() {
        let dir = fresh_dir("long-timeout");
        let options = ServeOptions::new(&dir).shutdown_timeout(Duration::MAX);
        let server = Server::start(options).unwrap();
        server.stop();
        Store::open_read_only(&dir).unwrap();
    }

    #[test]
    fn ruok_gives_the_status_as_json_and_closes() {
        use std::io::{Read, Write};
        use std::net::TcpStream;

        fn read_frame(stream: &mut TcpStream) -> Vec<u8> {
            let mut len = [0u8; 4];
            stream.read_exact(&mut len).unwrap();
            let mut payload = vec![0u8; u32::from_be_bytes(len) as usize];
            stream.read_exact(&mut payload).unwrap();
            payload
        }

        let server = Server::start(ServeOptions::new(fresh_dir("ruok"))).unwrap();
        let mut stream = TcpStream::connect(server.addr()).unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(30)))
            .unwrap();
        assert_eq!(read_frame(&mut stream), b"M5");
        stream.write_all(b"\0\0\0\x04ruok").unwrap();
        let reply = String::from_utf8(read_frame(&mut stream)).unwrap();
        assert!(
            reply.starts_with(r#"{"1": {"connections": 0, "loads": 0, "#),
            "{reply}"
        );
        assert!(
            reply.ends_with(r#", "last-transaction": "0000000000000000"}}"#),
            "{reply}"
        );
        // The server closes the connection after the status.
        let mut rest = Vec::new();
        assert_eq!(stream.read_to_end(&mut rest).unwrap(), 0);
        server.stop();
    }

    #[test]
    fn the_storages_need_a_store_and_names_that_differ() {
        let dir = fresh_dir("names");
        let none = Server::start(ServeOptions::with_storages([])).err();
        assert!(matches!(none, Some(Error::Storages(_))), "{none:?}");
        let twice = Server::start(ServeOptions::with_storages([
            StorageOptions::new("a", dir.join("a")),
            StorageOptions::new("a", dir.join("b")),
        ]))
        .err();
        assert!(matches!(twice, Some(Error::Storages(_))), "{twice:?}");
        assert!(!dir.exists());
    }

    #[test]
    fn ruok_gives_the_status_of_each_storage_in_their_order() {
        use std::io::{Read, Write};
        use std::net::TcpStream;

        let dir = fresh_dir("ruok-storages");
        let server = Server::start(ServeOptions::with_storages([
            StorageOptions::new("main", dir.join("main")),
            StorageOptions::new("catalog", dir.join("catalog")),
        ]))
        .unwrap();
        let mut stream = TcpStream::connect(server.addr()).unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(30)))
            .unwrap();
        stream.write_all(b"\0\0\0\x04ruok").unwrap();
        let mut reply = Vec::new();
        stream.read_to_end(&mut reply).unwrap();
        // The frame of the protocol, then the frame of the status.
        let reply = String::from_utf8_lossy(&reply[10..]).into_owned();
        let main = reply.find(r#"{"main": {"connections": 0, "#);
        let catalog = reply.find(r#"}, "catalog": {"connections": 0, "#);
        assert!(matches!((main, catalog), (Some(0), Some(_))), "{reply}");
        server.stop();
    }

    #[test]
    fn a_read_only_store_must_exist_and_takes_no_blob_directory() {
        let dir = fresh_dir("read-only-start");
        let missing = Server::start(ServeOptions::new(&dir).read_only(true)).err();
        assert!(matches!(missing, Some(Error::Store(..))), "{missing:?}");
        assert!(!dir.exists());
        for options in [
            ServeOptions::with_storages([StorageOptions::new("a", &dir)
                .blob_dir(dir.join("blobs"))
                .read_only(true)]),
            ServeOptions::with_storages([
                StorageOptions::new("a", &dir).blob_dir(dir.join("blobs"))
            ])
            .read_only(true),
        ] {
            let error = Server::start(options).err();
            assert!(matches!(error, Some(Error::Storages(_))), "{error:?}");
        }
        assert!(!dir.exists());
    }

    #[test]
    fn a_read_only_server_holds_the_lock_of_the_store() {
        let dir = fresh_dir("read-only-lock");
        drop(Store::open(&dir).unwrap());
        let server = Server::start(ServeOptions::with_storages([StorageOptions::new(
            "a", &dir,
        )
        .read_only(true)]))
        .unwrap();
        let writer = Store::open(&dir).err();
        assert!(matches!(writer, Some(ZoeError::Locked)), "{writer:?}");
        server.stop();
        drop(Store::open(&dir).unwrap());
    }

    #[test]
    fn a_server_that_cannot_listen_creates_no_store() {
        let dir = fresh_dir("no-listen");
        let first = Server::start(ServeOptions::new(fresh_dir("listening"))).unwrap();
        let taken = first.addr();
        // The port is taken by a listening socket, so the bind fails.
        let error = Server::start(ServeOptions::new(&dir).listen(taken))
            .err()
            .expect("the port is taken");
        assert!(matches!(error, Error::Listen(..)), "{error}");
        assert!(!dir.exists());
    }
}
