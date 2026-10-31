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
//!
//! `options` holds the settings of a server, and `listener` the ZEO port:
//! its socket, the accept loop and the count of the connections.

mod listener;
mod options;

use std::fmt;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::runtime::Runtime;
use tokio::sync::watch;
use zoe_core::{Options, Store, ZoeError};

use crate::admin::{self, Health, StoreHealth};
use crate::protocol::{self, NamedStorage, Pool, ResolutionLog, Stopping, Storages};
use crate::tls::Tls;
pub use crate::tls::TlsOptions;
use listener::{Listener, SocketFile, Tasks, accept, bind_tcp};
pub use options::{
    Address, DEFAULT_IDLE_TIMEOUT, DEFAULT_MAX_FRAME_SIZE, DEFAULT_SHUTDOWN_TIMEOUT, ServeOptions,
    StorageOptions,
};

/// The last part of the shutdown timeout, in which a stop closes the
/// connections that did not end and waits for their sessions. When the
/// timeout is shorter than twice this part, the stop uses the last half of
/// the timeout.
const FORCED_CLOSE: Duration = Duration::from_secs(1);

/// The longest shutdown timeout: 2^32 - 1 seconds, about 136 years. A
/// longer timeout counts as this one, so the deadline of a stop never
/// overflows the clock.
const LONGEST_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(4_294_967_295);

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
    /// The resolution log does not open.
    ResolutionLog(PathBuf, io::Error),
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
            Self::ResolutionLog(path, error) => {
                write!(f, "the resolution log {}: {error}", path.display())
            }
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
        Self::start_with(options, |_, path, options| open_writer(path, options))
    }

    /// Start as [`Server::start`] does, but handle SIGTERM and SIGINT
    /// before the start creates anything on disk. A signal during the
    /// start does not stop the start: each store open ends, and the start
    /// ends or fails. Then [`Server::serve_until_signal`] stops at once.
    pub(crate) fn start_with_signals(options: ServeOptions) -> Result<Self, Error> {
        options.check_storages()?;
        let runtime = new_runtime()?;
        let signals = Signals::handle(&runtime).map_err(Error::Runtime)?;
        Self::start_on(runtime, Some(signals), options, |_, path, options| {
            open_writer(path, options)
        })
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
        // The resolution log opens before the stores, so a log that does
        // not open creates no store.
        let resolution_log = match &options.resolution_log {
            Some(path) => {
                Some(Arc::new(ResolutionLog::open(path).map_err(|error| {
                    Error::ResolutionLog(path.clone(), error)
                })?))
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
                open_read_only(&storage.name, &storage.path, options.store_options(storage))?
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
                .resolution_log(resolution_log.clone())
                .shared_blob_dir(options.shared_blob_dir)
                .timeout(!options.idle_timeout.is_zero() || !options.transaction_timeout.is_zero())
                .transaction_timeout(
                    (!options.transaction_timeout.is_zero()).then_some(options.transaction_timeout),
                );
            storages.push(Arc::new(named));
        }
        health.measured(storages.iter().map(|storage| storage.exported()).collect());
        let storages = Storages::new(storages, options.queues.client)
            .with_pool(Pool::new(options.pool_threads()));
        let storages = Arc::new(storages);
        // The expired keys of the ephemeral namespaces go once a minute.
        runtime.spawn(protocol::sweep::run(
            protocol::sweep::stores(&storages),
            stopping.clone(),
        ));
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
fn open_writer(path: &Path, options: Options) -> Result<Arc<Store>, Error> {
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

/// Open the store of the storage `name` in `path` read-only with
/// `options`.
///
/// The open repairs a store that a writer left open (ADR 0032), and logs
/// one WARN line that names the storage and holds `repaired the store`. A
/// failed repair fails the start with `RepairFailed`. A read-only open does
/// not check the blob directory, so this call checks it, as `zoe verify`
/// does: a store with blobs whose blob directory has no layout file fails,
/// and the error names that directory.
fn open_read_only(name: &str, path: &Path, options: Options) -> Result<Arc<Store>, Error> {
    let failed = |error| Error::Store(path.to_path_buf(), error);
    let store = Store::open_read_only_repairing(path, options).map_err(failed)?;
    if store.repaired() {
        tracing::warn!(storage = name, "{}", Store::repair_message(path));
    }
    store.check_blob_dir().map_err(failed)?;
    Ok(Arc::new(store))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::net::Ipv4Addr;

    use super::*;
    use crate::protocol::testing::{fresh_dir, read_frame};

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
    fn a_store_packs_with_gc_unless_the_server_or_its_table_says_false() {
        let pack_gc = |server: bool, store: bool| {
            let storage = StorageOptions::new("1", "store").pack_gc(store);
            let options = ServeOptions::with_storages([storage.clone()]).pack_gc(server);
            options.store_options(&storage).pack_gc
        };
        let default = StorageOptions::new("1", "store");
        assert!(ServeOptions::new("store").store_options(&default).pack_gc);
        assert!(pack_gc(true, true));
        assert!(!pack_gc(false, true));
        assert!(!pack_gc(true, false));
        assert!(!pack_gc(false, false));
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
