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
//! The ZEO port is a TCP address or a unix domain socket ([`Address`]). The
//! server removes a stale socket file at its start, and its own socket file
//! at its stop.
//!
//! With [`TlsOptions`], the ZEO port speaks TLS. A client CA makes a client
//! certificate necessary, and the holder of a certificate of the read-only
//! client CA registers read-only only. A failed handshake closes only its
//! connection. TLS needs a TCP address.
//!
//! A stop takes at most the shutdown timeout. The server stops accepting
//! connections, and each connection reads no more calls. The calls that
//! the server already received run, so a `tpc_finish` that the
//! server received commits and gets its reply. A running pack stops between
//! two of its batches. Then the open transaction of
//! each connection aborts, and each connection sends the frames that wait
//! and shuts down its side. Each connection reads and drops the bytes that
//! its client sends after the stop, so its close is not a reset. It closes
//! when the client closes, or a short time after its last frame. Then the
//! stores close.

use std::fmt;
use std::fs;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::num::{NonZeroU32, NonZeroUsize};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use tokio::net::{TcpListener, TcpSocket, TcpStream, UnixListener, UnixStream};
use tokio::runtime::Runtime;
use tokio::sync::watch;
use zoe_core::{Options, Store, ZoeError};

use crate::admin::{self, Health, StoreHealth};
use crate::protocol::{self, Connection, NamedStorage, Pool, Queues, Stopping, Storages};
use crate::runtime::Tokio;
use crate::tls::Tls;
pub use crate::tls::TlsOptions;

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

/// The address of the ZEO port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Address {
    /// A TCP address. A port of 0 takes a free port.
    Tcp(SocketAddr),
    /// The path of a unix domain socket. The server binds the path as it
    /// is, so a relative path is relative to the current directory, and
    /// the limit of 108 bytes of a socket address applies to the path as
    /// given.
    Unix(PathBuf),
}

impl From<SocketAddr> for Address {
    fn from(addr: SocketAddr) -> Self {
        Self::Tcp(addr)
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tcp(addr) => addr.fmt(f),
            Self::Unix(path) => path.display().fmt(f),
        }
    }
}

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
    /// opens the existing store with `Store::open_read_only_with`: it
    /// changes no byte of the store and holds its lock. A store with blobs
    /// must get its blob directory, or the start fails.
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
    listen: Address,
    admin_listen: Option<SocketAddr>,
    queues: Queues,
    lock_timeout: Option<Duration>,
    cache_size: Option<usize>,
    lock_warn: Option<Duration>,
    transaction_warn: Option<Duration>,
    max_frame_size: NonZeroU32,
    shutdown_timeout: Duration,
    idle_timeout: Duration,
    blocking_threads: Option<NonZeroUsize>,
    read_only: bool,
    shared_blob_dir: bool,
    tls: Option<TlsOptions>,
}

impl ServeOptions {
    /// Serve the store in `path` under the storage name `1`, the default of
    /// ZEO. Listen on a free port of 127.0.0.1, with frames of at most
    /// [`DEFAULT_MAX_FRAME_SIZE`] bytes. The invalidation queue covers
    /// 100,000 transactions, and 1,000 invalidations can wait for one
    /// client. A stop takes at most [`DEFAULT_SHUTDOWN_TIMEOUT`]. A
    /// connection that sends no frame for [`DEFAULT_IDLE_TIMEOUT`], while
    /// no call of the connection runs, closes.
    /// The calls that read run on as many threads as the machine has
    /// cores.
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self::with_storages([StorageOptions::new(protocol::STORAGE_NAME, path)])
    }

    /// Serve `storages`, with the defaults of [`ServeOptions::new`]. The
    /// names must differ, and one storage at least is necessary.
    pub fn with_storages(storages: impl IntoIterator<Item = StorageOptions>) -> Self {
        Self {
            storages: storages.into_iter().collect(),
            listen: Address::Tcp(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))),
            admin_listen: None,
            queues: Queues::DEFAULT,
            lock_timeout: None,
            cache_size: None,
            lock_warn: None,
            transaction_warn: None,
            max_frame_size: DEFAULT_MAX_FRAME_SIZE,
            shutdown_timeout: DEFAULT_SHUTDOWN_TIMEOUT,
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
            blocking_threads: None,
            read_only: false,
            shared_blob_dir: false,
            tls: None,
        }
    }

    /// Listen on `addr`: a `SocketAddr`, or an [`Address`]. A port of 0
    /// takes a free port. On a unix domain socket, the start removes a
    /// socket file at the path on which no process accepts connections,
    /// and the stop removes the socket file of the server.
    #[must_use]
    pub fn listen(mut self, addr: impl Into<Address>) -> Self {
        self.listen = addr.into();
        self
    }

    /// Open the admin port on `addr`: an HTTP port with `/healthz`,
    /// `/readyz` and `/metrics`. A port of 0 takes a free port. Without this call, the
    /// server opens no admin port.
    #[must_use]
    pub fn admin_listen(mut self, addr: SocketAddr) -> Self {
        self.admin_listen = Some(addr);
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

    /// Let the page cache of each store hold at most `bytes`. Without this
    /// call, each store keeps the default of the store, 1 GiB. The cache
    /// does not grow with the size of the store.
    #[must_use]
    pub fn cache_size(mut self, bytes: usize) -> Self {
        self.cache_size = Some(bytes);
        self
    }

    /// Log each lock held longer than `threshold`, with its oid, its client
    /// and its storage, when the lock is released. Without this call, the
    /// server logs no lock.
    #[must_use]
    pub fn lock_warn(mut self, threshold: Duration) -> Self {
        self.lock_warn = Some(threshold);
        self
    }

    /// Log each transaction open longer than `threshold`, with its client
    /// and its storage, when it ends. Without this call, the server logs
    /// no transaction.
    #[must_use]
    pub fn transaction_warn(mut self, threshold: Duration) -> Self {
        self.transaction_warn = Some(threshold);
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

    /// Close a connection that sends no frame for `timeout` while no call
    /// of the connection runs. The time counts from the accept for the
    /// protocol of the client. Then it counts from the later of two times:
    /// the last frame that the server read, a heartbeat included, and the
    /// end of the last call of the connection. A call runs until it puts
    /// its reply, and a download until it ends. The close stops
    /// the writer at once, as a close of the hub does. The calls that
    /// arrived run, up to the first reply that cannot be queued, then the
    /// open transaction aborts. A timeout of 0 closes no connection.
    #[must_use]
    pub fn idle_timeout(mut self, timeout: Duration) -> Self {
        self.idle_timeout = timeout;
        self
    }

    /// Run the calls that read, such as `loadBefore`, on at most `threads`
    /// threads at once, for all the connections together. Without this
    /// call, the server uses as many threads as the machine has cores. A
    /// transaction, a download and a pack each run on a thread of their
    /// own, so a vote that waits for a lock holds no thread of the pool.
    #[must_use]
    pub fn blocking_threads(mut self, threads: NonZeroUsize) -> Self {
        self.blocking_threads = Some(threads);
        self
    }

    /// The number of threads of the read pool: the option, or the number of
    /// cores, or 1 when that number is not known.
    fn pool_threads(&self) -> NonZeroUsize {
        self.blocking_threads.unwrap_or_else(Pool::cores)
    }

    /// Serve every store for reads only, when `read_only` is true, as
    /// [`StorageOptions::read_only`] does for one store.
    #[must_use]
    pub fn read_only(mut self, read_only: bool) -> Self {
        self.read_only = read_only;
        self
    }

    /// Accept `storeBlobShared` when `shared` is true: the clients share
    /// the blob directory of each store, and give the name of a blob file
    /// in the directory of its oid. `Server::start` then makes the blob
    /// directory of each store that is not read-only ready: the layout file
    /// and `tmp/`. So a client can create the directory of an oid before
    /// the first blob of the store. Without this call, `storeBlobShared`
    /// fails the vote with an error that names `shared-blob-dir`.
    #[must_use]
    pub fn shared_blob_dir(mut self, shared: bool) -> Self {
        self.shared_blob_dir = shared;
        self
    }

    /// Speak TLS on the ZEO port with the files of `tls`. `Server::start`
    /// reads them before it listens, so a file that does not read creates
    /// no store. TLS needs a TCP address: on a unix domain socket, the
    /// start fails. Without this call, the ZEO port speaks plain TCP.
    #[must_use]
    pub fn tls(mut self, tls: TlsOptions) -> Self {
        self.tls = Some(tls);
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
        options.cache_size = self.cache_size;
        options.lock_warn = self.lock_warn;
        options.transaction_warn = self.transaction_warn;
        options.blob_dir.clone_from(&storage.blob_dir);
        options
    }

    /// Check that one storage at least is given, and that no name comes
    /// twice.
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
        }
        Ok(())
    }
}

/// Why a server did not start.
#[derive(Debug)]
pub enum Error {
    /// The options give no storage, or give a storage name twice.
    Storages(String),
    /// The server cannot listen on the address. On a unix domain socket,
    /// the error is `AddrInUse` when another process accepts connections on
    /// the path, and `AlreadyExists` when the path is not a socket.
    Listen(Address, io::Error),
    /// The server cannot listen on the address of the admin port.
    AdminListen(SocketAddr, io::Error),
    /// The store does not open.
    Store(PathBuf, ZoeError),
    /// The async runtime does not start.
    Runtime(io::Error),
    /// A file of TLS does not read or does not fit, or the address is a
    /// unix domain socket.
    Tls(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Storages(message) => f.write_str(message),
            Self::Listen(addr, error) => write!(f, "cannot listen on {addr}: {error}"),
            Self::AdminListen(addr, error) => {
                write!(f, "cannot listen on the admin address {addr}: {error}")
            }
            Self::Store(path, error) => write!(f, "{}: {error}", path.display()),
            Self::Runtime(error) => write!(f, "cannot start the runtime: {error}"),
            Self::Tls(message) => write!(f, "TLS: {message}"),
        }
    }
}

impl std::error::Error for Error {}

/// A running server. Dropping it stops it.
pub struct Server {
    runtime: Option<Runtime>,
    storages: Option<Arc<Storages>>,
    /// The address that the server listens on, with the port that it took.
    address: Address,
    /// The socket file of a unix domain socket. The stop removes it.
    socket_file: Option<SocketFile>,
    /// The address of the admin port, when the server has one.
    admin: Option<SocketAddr>,
    /// The sender of the stop to the accept loop and to each connection.
    stop_signal: watch::Sender<bool>,
    /// The accept loop and the connections that run.
    tasks: Arc<Tasks>,
    shutdown_timeout: Duration,
    /// SIGTERM and SIGINT, when the start handled them.
    signals: Option<Signals>,
    /// The times of the stop. The stop sets them once.
    #[cfg(test)]
    stop_times: Arc<std::sync::OnceLock<StopTimes>>,
}

/// The times of a stop that the shutdown timeout bounds. The tests of the
/// crate read them.
#[cfg(test)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct StopTimes {
    /// The start of the stop, before the stop signal.
    pub(crate) start: Instant,
    /// The end of the wait for the sessions, before the close of the
    /// stores.
    pub(crate) waited: Instant,
}

impl Server {
    /// Listen, open the stores and accept clients. Return once the server
    /// accepts connections. When a store does not open, close the stores
    /// that opened and fail.
    pub fn start(options: ServeOptions) -> Result<Self, Error> {
        Self::start_with(options, open_writer)
    }

    /// Start as [`Server::start`] does, but handle SIGTERM and SIGINT
    /// before the start creates anything on disk. A signal during the
    /// start does not stop the start: each store open ends, and the start
    /// ends or fails. Then [`Server::serve_until_signal`] stops at once.
    pub(crate) fn start_with_signals(options: ServeOptions) -> Result<Self, Error> {
        options.check_storages()?;
        let runtime = new_runtime()?;
        let signals = Signals::handle(&runtime).map_err(Error::Runtime)?;
        Self::start_on(runtime, Some(signals), options, open_writer)
    }

    /// Start as [`Server::start`] does, but open the store of each storage
    /// that is not read-only with `open`. It gets the storage name, the
    /// store directory and the options of the store.
    pub(crate) fn start_with(
        options: ServeOptions,
        open: impl FnMut(&str, &Path, Options) -> Result<Arc<Store>, Error>,
    ) -> Result<Self, Error> {
        options.check_storages()?;
        Self::start_on(new_runtime()?, None, options, open)
    }

    /// Start in `runtime`, with the signals that the server keeps for
    /// [`Server::serve_until_signal`], as [`Server::start_with`] does.
    fn start_on(
        runtime: Runtime,
        signals: Option<Signals>,
        options: ServeOptions,
        mut open: impl FnMut(&str, &Path, Options) -> Result<Arc<Store>, Error>,
    ) -> Result<Self, Error> {
        if let (Some(_), Address::Unix(path)) = (&options.tls, &options.listen) {
            return Err(Error::Tls(format!(
                "TLS needs a TCP address, not the unix domain socket {}",
                path.display()
            )));
        }
        // A file of TLS that does not read changes nothing on disk.
        let tls = options
            .tls
            .as_ref()
            .map(Tls::load)
            .transpose()
            .map_err(Error::Tls)?
            .map(Arc::new);
        // Listen first: a server that cannot listen changes nothing on
        // disk, except a stale socket file that goes. A start that fails
        // later drops the socket file, which removes it.
        let (listener, socket_file) = {
            let _enter = runtime.enter();
            Listener::bind(&options.listen)
                .map_err(|error| Error::Listen(options.listen.clone(), error))?
        };
        let address = listener
            .address()
            .map_err(|error| Error::Listen(options.listen.clone(), error))?;
        let (stop_signal, stopping) = Stopping::new();
        // The admin port answers from now on, 503 until the stores are
        // open. It is not a task of the stop: it stays open until the
        // runtime stops.
        let health = Health::new(stop_signal.subscribe());
        let admin = match options.admin_listen {
            Some(admin_listen) => {
                let _enter = runtime.enter();
                let listener = bind_tcp(admin_listen)
                    .map_err(|error| Error::AdminListen(admin_listen, error))?;
                let admin = listener
                    .local_addr()
                    .map_err(|error| Error::AdminListen(admin_listen, error))?;
                runtime.spawn(admin::serve(listener, Arc::clone(&health)));
                Some(admin)
            }
            None => None,
        };
        // This thread is not a thread of the runtime. Each store has its
        // own lock manager, and each storage its own invalidations.
        let mut storages = Vec::with_capacity(options.storages.len());
        let mut stores = Vec::with_capacity(options.storages.len());
        for storage in &options.storages {
            let read_only = options.is_read_only(storage);
            let store = if read_only {
                open_read_only(&storage.path, options.store_options(storage))?
            } else {
                open(&storage.name, &storage.path, options.store_options(storage))?
            };
            // The blob directory gets its layout file before a client of
            // the shared directory creates the directory of an oid there.
            if options.shared_blob_dir && !read_only {
                store
                    .blob_temp_dir()
                    .map_err(|error| Error::Store(storage.path.clone(), error))?;
            }
            stores.push(StoreHealth {
                name: storage.name.clone(),
                no_stale_locks: store.lock_table_is_empty(),
            });
            let named = NamedStorage::new(&storage.name, &storage.path, store, options.queues);
            let named = named
                .read_only(read_only)
                .shared_blob_dir(options.shared_blob_dir);
            storages.push(Arc::new(named));
        }
        health.measured(storages.iter().map(|storage| storage.exported()).collect());
        let storages = Storages::new(storages, options.queues.client)
            .with_pool(Pool::new(options.pool_threads()));
        let storages = Arc::new(storages);
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
            tls,
        );
        runtime.spawn(async move {
            accept.await;
            drop(accepting);
        });
        // Each open loaded the last tid of its store.
        health.opened(stores);
        Ok(Self {
            runtime: Some(runtime),
            storages: Some(storages),
            address,
            socket_file,
            admin,
            stop_signal,
            tasks,
            shutdown_timeout: options.shutdown_timeout,
            signals,
            #[cfg(test)]
            stop_times: Arc::default(),
        })
    }

    /// The TCP address that the server listens on.
    ///
    /// # Panics
    ///
    /// Panics when the server listens on a unix domain socket. Then
    /// [`Server::address`] gives its path.
    pub fn addr(&self) -> SocketAddr {
        match &self.address {
            Address::Tcp(addr) => *addr,
            Address::Unix(path) => panic!(
                "the server listens on the unix domain socket {}",
                path.display()
            ),
        }
    }

    /// The address that the server listens on: the TCP address with the
    /// port that it took, or the path of its unix domain socket.
    pub fn address(&self) -> &Address {
        &self.address
    }

    /// The address of the admin port, when the server has one.
    pub fn admin_addr(&self) -> Option<SocketAddr> {
        self.admin
    }

    /// Stop the server within its shutdown timeout. Accept no more
    /// connections. Commit each `tpc_finish` that the server received and
    /// send its reply. Abort the other open transactions, close every
    /// connection and close the stores.
    pub fn stop(mut self) {
        self.shutdown();
    }

    /// Return the cell that the stop fills with its times.
    #[cfg(test)]
    pub(crate) fn stop_times(&self) -> Arc<std::sync::OnceLock<StopTimes>> {
        Arc::clone(&self.stop_times)
    }

    /// Call `ready` with the address, then serve until SIGTERM or SIGINT.
    /// Then stop. A signal that came after the start handled the signals
    /// stops the server at once.
    pub(crate) fn serve_until_signal(
        mut self,
        ready: impl FnOnce(&Address) -> io::Result<()>,
    ) -> io::Result<()> {
        let runtime = self.runtime.as_ref().expect("the server runs");
        let mut signals = match self.signals.take() {
            Some(signals) => signals,
            // A server from `Server::start` handles the signals from here.
            None => Signals::handle(runtime)?,
        };
        ready(&self.address)?;
        signals.wait(runtime);
        self.shutdown();
        Ok(())
    }

    fn shutdown(&mut self) {
        let Some(runtime) = self.runtime.take() else {
            return;
        };
        let timeout = self.shutdown_timeout.min(LONGEST_SHUTDOWN_TIMEOUT);
        let start = Instant::now();
        let deadline = start + timeout;
        // The accept loop drops the listener, and each connection reads no
        // more calls. The sessions run the calls that wait, then end. Each
        // connection drops the bytes that arrive until it closes.
        self.stop_signal.send_replace(true);
        let forced = timeout - FORCED_CLOSE.min(timeout / 2);
        if self.tasks.wait(forced) > 0 {
            tracing::warn!("the shutdown timeout ends: closing the connections that remain");
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
        // The listener is gone. The drop removes the socket file.
        drop(self.socket_file.take());
        #[cfg(test)]
        let _ = self.stop_times.set(StopTimes {
            start,
            waited: Instant::now(),
        });
        // A session that runs holds the store of its storage. This drop
        // closes every other store.
        if let Some(storages) = self.storages.take() {
            for name in Storages::held(&storages) {
                tracing::warn!(
                    storage = name,
                    "a session still runs at the end of the shutdown timeout: \
                     the store of the storage does not close cleanly"
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

/// Build the multi-thread runtime of a server.
fn new_runtime() -> Result<Runtime, Error> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("zoe-serve")
        .build()
        .map_err(Error::Runtime)
}

/// Open the store in `path` for writes with `options`. Create a store that
/// does not exist.
fn open_writer(_name: &str, path: &Path, options: Options) -> Result<Arc<Store>, Error> {
    Store::open_with(path, options)
        .map(Arc::new)
        .map_err(|error| Error::Store(path.to_path_buf(), error))
}

/// SIGTERM and SIGINT, handled through the runtime of the server. The
/// process keeps the handlers until it exits, so a later signal never
/// kills it.
struct Signals {
    term: tokio::signal::unix::Signal,
    interrupt: tokio::signal::unix::Signal,
}

impl Signals {
    /// Handle SIGTERM and SIGINT from now on. A signal that comes after
    /// this call waits until [`Signals::wait`] reads it.
    fn handle(runtime: &Runtime) -> io::Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};

        let _enter = runtime.enter();
        Ok(Self {
            term: signal(SignalKind::terminate())?,
            interrupt: signal(SignalKind::interrupt())?,
        })
    }

    /// Wait in `runtime` for the first SIGTERM or SIGINT.
    fn wait(&mut self, runtime: &Runtime) {
        runtime.block_on(async {
            tokio::select! {
                biased;
                _ = self.term.recv() => {}
                _ = self.interrupt.recv() => {}
            }
        });
    }
}

/// Open the store in `path` read-only with `options`. A read-only open does
/// not check the blob directory, so this call checks it, as `zoe verify`
/// does: a store with blobs whose blob directory has no layout file fails,
/// and the error names that directory.
fn open_read_only(path: &Path, options: Options) -> Result<Arc<Store>, Error> {
    Store::open_read_only_with(path, options)
        .and_then(|store| store.check_blob_dir().map(|()| Arc::new(store)))
        .map_err(|error| Error::Store(path.to_path_buf(), error))
}

/// The listening socket of the ZEO port.
enum Listener {
    Tcp(TcpListener),
    /// A unix domain socket, with its path as the options give it.
    Unix(UnixListener, Arc<Path>),
}

/// A connection that the listener accepted.
enum Accepted {
    Tcp(TcpStream, SocketAddr),
    /// A connection on a unix domain socket, with the path of the socket.
    Unix(UnixStream, Arc<Path>),
}

impl Listener {
    /// Listen on `address`. Return the socket file of a unix domain
    /// socket too: its drop removes the file. The call must run in the
    /// context of a runtime.
    fn bind(address: &Address) -> io::Result<(Self, Option<SocketFile>)> {
        match address {
            Address::Tcp(addr) => Ok((Self::Tcp(bind_tcp(*addr)?), None)),
            Address::Unix(path) => {
                let (listener, file) = bind_unix(path)?;
                Ok((Self::Unix(listener, Arc::from(path.as_path())), file))
            }
        }
    }

    /// The address of the listener: a TCP address with the port that it
    /// took, or the path of the socket as the options give it.
    fn address(&self) -> io::Result<Address> {
        match self {
            Self::Tcp(listener) => listener.local_addr().map(Address::Tcp),
            Self::Unix(_, path) => Ok(Address::Unix(path.to_path_buf())),
        }
    }

    /// Accept the next connection.
    async fn accept(&self) -> io::Result<Accepted> {
        match self {
            Self::Tcp(listener) => {
                let (stream, peer) = listener.accept().await?;
                Ok(Accepted::Tcp(stream, peer))
            }
            Self::Unix(listener, path) => {
                let (stream, _) = listener.accept().await?;
                Ok(Accepted::Unix(stream, Arc::clone(path)))
            }
        }
    }
}

/// The socket file of the unix domain socket of a server. Its drop removes
/// the file, but only when the file at the path is still the one that the
/// server bound. So it never removes the socket of another server that
/// took the path meanwhile.
struct SocketFile {
    path: PathBuf,
    device: u64,
    inode: u64,
}

impl SocketFile {
    /// Return the socket file at `path`, which the server just bound.
    fn bound(path: &Path) -> io::Result<Self> {
        let metadata = fs::symlink_metadata(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
}

impl Drop for SocketFile {
    fn drop(&mut self) {
        let same = fs::symlink_metadata(&self.path)
            .is_ok_and(|metadata| metadata.dev() == self.device && metadata.ino() == self.inode);
        if same && let Err(error) = fs::remove_file(&self.path) {
            tracing::warn!(
                path = %self.path.display(),
                "cannot remove the socket file: {error}"
            );
        }
    }
}

/// Listen on the unix domain socket `path`. The call binds the path as it
/// is, so a relative path is relative to the current directory. When a
/// socket file is at the path, the call connects to it first, as runzeo
/// does. When a process accepts the connection, the call fails with
/// `AddrInUse`, and the file stays. When the connection is refused, the
/// file is stale and goes. A path that is not a socket, a symbolic link
/// included, fails with `AlreadyExists` and stays.
fn bind_unix(path: &Path) -> io::Result<(UnixListener, Option<SocketFile>)> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => {
            match std::os::unix::net::UnixStream::connect(path) {
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::AddrInUse,
                        "another process accepts connections on the socket",
                    ));
                }
                Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {
                    match fs::remove_file(path) {
                        Err(error) if error.kind() != io::ErrorKind::NotFound => {
                            return Err(error);
                        }
                        _ => {}
                    }
                }
                Err(error) => return Err(error),
            }
        }
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "the path exists and is not a socket",
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let listener = UnixListener::bind(path)?;
    // Without its device and inode, the stop cannot tell that the file is
    // still the socket of the server, so the file stays.
    let file = SocketFile::bound(path).ok();
    Ok((listener, file))
}

/// Listen on `addr`. The socket takes SO_REUSEADDR, so a restarted server
/// binds the same port at once.
fn bind_tcp(addr: SocketAddr) -> io::Result<TcpListener> {
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
/// takes frames of at most
/// `max_frame_size` bytes, closes after `idle_timeout` without a frame and
/// without a call that runs, and
/// counts in `tasks` while it runs. With `tls`, each connection on TCP
/// starts with the TLS handshake.
async fn accept(
    listener: Listener,
    storages: Arc<Storages>,
    max_frame_size: u32,
    idle_timeout: Option<Duration>,
    mut stopping: Stopping,
    tasks: Arc<Tasks>,
    tls: Option<Arc<Tls>>,
) {
    loop {
        let accepted = tokio::select! {
            biased;
            () = stopping.wait() => break,
            accepted = listener.accept() => accepted,
        };
        match accepted {
            Ok(Accepted::Unix(stream, path)) => {
                let running = tasks.start();
                let connection = protocol::serve_unix_connection(
                    stream,
                    path,
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
            Ok(Accepted::Tcp(stream, peer)) => {
                let _ = stream.set_nodelay(true);
                let running = tasks.start();
                if let Some(tls) = &tls {
                    let connection = serve_tls(
                        stream,
                        peer.to_string(),
                        Arc::clone(tls),
                        Arc::clone(&storages),
                        max_frame_size,
                        idle_timeout,
                        stopping.clone(),
                    );
                    tokio::spawn(async move {
                        connection.await;
                        drop(running);
                    });
                    continue;
                }
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
                tracing::warn!("accept failed: {error}");
                tokio::task::yield_now().await;
            }
        }
    }
}

/// Run the TLS handshake of `tls` on `stream`, then serve the client
/// `peer` as `protocol::serve_connection` does. The handshake must end
/// within `idle_timeout`, as the protocol of a plain client must. A stop
/// ends it at once. A failed handshake closes only this connection.
async fn serve_tls(
    stream: TcpStream,
    peer: String,
    tls: Arc<Tls>,
    storages: Arc<Storages>,
    max_frame_size: u32,
    idle_timeout: Option<Duration>,
    mut stopping: Stopping,
) {
    let idle = async {
        match idle_timeout {
            Some(timeout) => tokio::time::sleep(timeout).await,
            None => std::future::pending().await,
        }
    };
    let handshake = tokio::select! {
        biased;
        () = stopping.wait() => return,
        handshake = tls.accept(stream) => handshake,
        () = idle => {
            tracing::info!(client = peer.as_str(), "no TLS handshake within the idle timeout: closing the connection");
            return;
        }
    };
    let (stream, read_only) = match handshake {
        Ok(accepted) => accepted,
        Err(error) => {
            tracing::warn!(client = peer.as_str(), "the TLS handshake failed: {error}");
            return;
        }
    };
    let socket = Some(stream.get_ref().0.as_raw_fd());
    let (reader, writer) = tokio::io::split(stream);
    let connection = Connection {
        peer,
        runtime: Arc::new(Tokio),
        max_frame_size,
        idle_timeout,
        read_only,
        socket,
    };
    protocol::serve_stream(reader, writer, connection, storages, stopping).await;
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

    /// Send `GET path` to the admin port `addr`. Return the status.
    fn admin_get(addr: SocketAddr, path: &str) -> io::Result<u16> {
        use std::io::{Read, Write};
        use std::net::TcpStream;

        let mut stream = TcpStream::connect(addr)?;
        stream.set_read_timeout(Some(Duration::from_secs(30)))?;
        write!(stream, "GET {path} HTTP/1.1\r\nHost: zoe\r\n\r\n")?;
        let mut response = String::new();
        stream.read_to_string(&mut response)?;
        Ok(response[9..12].parse().unwrap())
    }

    #[test]
    fn the_admin_port_is_not_ready_from_the_stop_on_and_closes_at_its_end() {
        let dir = fresh_dir("admin-stop");
        let options = ServeOptions::new(&dir)
            .admin_listen(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
            .shutdown_timeout(Duration::from_secs(30));
        let server = Server::start(options).unwrap();
        let admin = server.admin_addr().unwrap();
        assert_eq!(admin_get(admin, "/readyz").unwrap(), 200);

        // A task that runs holds the stop. The admin port is not one.
        let holding = server.tasks.start();
        let stop = std::thread::spawn(move || server.stop());
        let deadline = Instant::now() + Duration::from_secs(10);
        while admin_get(admin, "/readyz").unwrap() != 503 {
            assert!(Instant::now() < deadline, "/readyz stays 200 in the stop");
            std::thread::sleep(Duration::from_millis(10));
        }
        // The stores are open until the end of the stop.
        assert_eq!(admin_get(admin, "/healthz").unwrap(), 200);
        assert!(!stop.is_finished());

        drop(holding);
        stop.join().unwrap();
        assert!(admin_get(admin, "/healthz").is_err());
        Store::open_read_only(&dir).unwrap();
    }

    #[test]
    fn a_server_that_cannot_open_its_admin_port_creates_no_store() {
        let taken = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let dir = fresh_dir("admin-taken");
        let options = ServeOptions::new(&dir).admin_listen(taken.local_addr().unwrap());
        let error = Server::start(options).err().unwrap();
        assert!(matches!(error, Error::AdminListen(..)), "{error}");
        assert!(error.to_string().contains("admin address"), "{error}");
        assert!(!dir.exists());
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
    fn a_shared_blob_dir_gets_its_layout_file_at_the_start() {
        let dir = fresh_dir("shared-start");
        let blobs = dir.join("shared-blobs");
        let options = ServeOptions::with_storages([
            StorageOptions::new("1", dir.join("store")).blob_dir(&blobs)
        ])
        .shared_blob_dir(true);
        let server = Server::start(options).unwrap();
        assert_eq!(std::fs::read(blobs.join(".layout")).unwrap(), b"bushy");
        assert!(blobs.join("tmp").is_dir());
        server.stop();

        // A read-only store gets no layout file: the server writes nothing
        // there.
        let other = dir.join("other");
        Store::open(&other).unwrap();
        let options = ServeOptions::new(&other)
            .read_only(true)
            .shared_blob_dir(true);
        Server::start(options).unwrap().stop();
        assert!(!other.join("blobs").exists());

        // Without the option, the server makes no blob directory.
        let plain = dir.join("plain");
        Server::start(ServeOptions::new(&plain)).unwrap().stop();
        assert!(!plain.join("blobs").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_read_only_store_must_exist_and_its_open_creates_nothing() {
        let dir = fresh_dir("read-only-start");
        for options in [
            ServeOptions::new(&dir).read_only(true),
            ServeOptions::with_storages([StorageOptions::new("a", &dir)
                .blob_dir(dir.join("blobs"))
                .read_only(true)]),
        ] {
            let missing = Server::start(options).err();
            assert!(matches!(missing, Some(Error::Store(..))), "{missing:?}");
            assert!(!dir.exists());
        }
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
    fn a_server_whose_tls_files_do_not_read_creates_no_store() {
        let dir = fresh_dir("no-tls");
        let tls = TlsOptions::new(dir.join("missing.pem"), dir.join("missing.key"));
        let error = Server::start(ServeOptions::new(&dir).tls(tls))
            .err()
            .unwrap();
        assert!(matches!(error, Error::Tls(_)), "{error}");
        assert!(error.to_string().contains("missing.pem"), "{error}");
        assert!(!dir.exists());
    }

    #[test]
    fn a_server_on_a_unix_domain_socket_serves_and_removes_its_socket_file_at_the_stop() {
        use std::io::Read;
        use std::os::unix::net::UnixStream;

        let dir = fresh_dir("unix");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("zeo.sock");
        let options = ServeOptions::new(dir.join("store")).listen(Address::Unix(path.clone()));
        let server = Server::start(options).unwrap();
        assert_eq!(server.address(), &Address::Unix(path.clone()));
        assert_eq!(server.address().to_string(), path.display().to_string());
        let mut client = UnixStream::connect(&path).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        let mut protocol = [0u8; 6];
        client.read_exact(&mut protocol).unwrap();
        assert_eq!(&protocol, b"\0\0\0\x02M5");

        let started = Instant::now();
        server.stop();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(client.read(&mut protocol).unwrap(), 0);
        assert!(!path.exists());
        Store::open_read_only(dir.join("store")).unwrap();
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_stale_socket_file_goes_and_a_live_socket_or_another_file_stays() {
        use std::os::unix::net::{UnixListener, UnixStream};

        let dir = fresh_dir("unix-stale");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("zeo.sock");
        let on_path =
            |store: &str| ServeOptions::new(dir.join(store)).listen(Address::Unix(path.clone()));
        // No process accepts connections on a socket file whose listener
        // is gone.
        drop(UnixListener::bind(&path).unwrap());
        assert!(UnixStream::connect(&path).is_err());
        let server = Server::start(on_path("store")).unwrap();
        UnixStream::connect(&path).unwrap();

        // A second server on the path fails, creates no store and leaves
        // the socket of the first.
        let error = Server::start(on_path("other")).err().unwrap();
        assert!(
            matches!(&error, Error::Listen(_, e) if e.kind() == io::ErrorKind::AddrInUse),
            "{error}"
        );
        assert!(
            error.to_string().contains(&*path.to_string_lossy()),
            "{error}"
        );
        assert!(!dir.join("other").exists());
        UnixStream::connect(&path).unwrap();
        server.stop();
        assert!(!path.exists());

        // A path that is not a socket stays.
        fs::write(&path, b"data").unwrap();
        let error = Server::start(on_path("other")).err().unwrap();
        assert!(
            matches!(&error, Error::Listen(_, e) if e.kind() == io::ErrorKind::AlreadyExists),
            "{error}"
        );
        assert_eq!(fs::read(&path).unwrap(), b"data");
        assert!(!dir.join("other").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_start_that_fails_on_a_unix_domain_socket_leaves_no_socket_file() {
        let dir = fresh_dir("unix-fail");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("zeo.sock");
        // A read-only store that does not exist fails after the bind.
        let options = ServeOptions::new(dir.join("missing"))
            .read_only(true)
            .listen(Address::Unix(path.clone()));
        let error = Server::start(options).err();
        assert!(matches!(error, Some(Error::Store(..))), "{error:?}");
        assert!(!path.exists());

        // TLS needs a TCP address.
        let tls = TlsOptions::new(dir.join("c.pem"), dir.join("k.pem"));
        let options = ServeOptions::new(dir.join("store"))
            .listen(Address::Unix(path.clone()))
            .tls(tls);
        let error = Server::start(options).err().unwrap();
        assert!(matches!(error, Error::Tls(_)), "{error}");
        assert!(error.to_string().contains("unix domain socket"), "{error}");
        assert!(!path.exists());
        assert!(!dir.join("store").exists());
        fs::remove_dir_all(&dir).unwrap();
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
