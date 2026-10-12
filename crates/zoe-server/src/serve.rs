//! `zoe serve` as a library: [`Server`] serves stores with the ZEO 6
//! msgpack protocol, each under its own storage name.
//!
//! A client selects a store with the storage name that it gives to
//! `register`. Each store has its own lock manager and its own
//! invalidations, so commits on different stores never wait for each
//! other.
//!
//! The binary runs it until SIGTERM. A test runs it in its own process
//! with [`Server::start`] and [`Server::stop`].

use std::fmt;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::net::{TcpListener, TcpSocket};
use tokio::runtime::Runtime;
use zoe_core::{Options, Store, ZoeError};

use crate::protocol::{self, NamedStorage, Queues, Storages};

/// The default of `--max-frame-size`: 256 MiB. A `storea` or `restorea`
/// frame holds a whole record, and the store sets no limit on a record by
/// default. ZODB warns about a record of more than 16 MiB, and a blob chunk
/// is less than 64 KiB. So a stock client stays far below this limit.
pub const DEFAULT_MAX_FRAME_SIZE: NonZeroU32 = NonZeroU32::new(256 << 20).unwrap();

/// A store that a server serves under a name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageOptions {
    name: String,
    path: PathBuf,
    blob_dir: Option<PathBuf>,
}

impl StorageOptions {
    /// Serve the store in `path` under the storage name `name`. The blob
    /// files go to `blobs` in the store directory.
    pub fn new(name: impl Into<String>, path: impl AsRef<Path>) -> Self {
        Self {
            name: name.into(),
            path: path.as_ref().to_path_buf(),
            blob_dir: None,
        }
    }

    /// Keep the blob files of the store in `dir`. Each open of the store
    /// must give the same directory.
    #[must_use]
    pub fn blob_dir(mut self, dir: impl AsRef<Path>) -> Self {
        self.blob_dir = Some(dir.as_ref().to_path_buf());
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
}

impl ServeOptions {
    /// Serve the store in `path` under the storage name `1`, the default of
    /// ZEO. Listen on a free port of 127.0.0.1, with frames of at most
    /// [`DEFAULT_MAX_FRAME_SIZE`] bytes. The invalidation queue covers
    /// 100,000 transactions, and 1,000 invalidations can wait for one
    /// client.
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

    /// The options of the store of `storage`.
    fn store_options(&self, storage: &StorageOptions) -> Options {
        let mut options = Options::default();
        if let Some(timeout) = self.lock_timeout {
            options.lock_timeout = timeout;
        }
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
}

impl Server {
    /// Listen, open the stores and accept clients. Return once the server
    /// accepts connections. When a store does not open, close the stores
    /// that opened and fail.
    pub fn start(options: ServeOptions) -> Result<Self, Error> {
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
            let store = Store::open_with(&storage.path, options.store_options(storage))
                .map_err(|error| Error::Store(storage.path.clone(), error))?;
            storages.push(Arc::new(NamedStorage::new(
                &storage.name,
                &storage.path,
                Arc::new(store),
                options.queues,
            )));
        }
        let storages = Arc::new(Storages::new(storages, options.queues.client));
        runtime.spawn(accept(
            listener,
            Arc::clone(&storages),
            options.max_frame_size.get(),
        ));
        Ok(Self {
            runtime: Some(runtime),
            storages: Some(storages),
            addr,
        })
    }

    /// The address that the server listens on.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Stop the server: close every connection, abort the open
    /// transactions and close the store. A commit that is in `finish`
    /// completes first.
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
        // Dropping the runtime drops every connection task. Each session
        // then ends after its current call, and the drop waits for it.
        // Tokio refuses that drop on a thread of an async runtime, as in an
        // async test, so another thread drops it.
        if let Some(runtime) = self.runtime.take() {
            std::thread::scope(|scope| {
                scope.spawn(move || drop(runtime));
            });
        }
        // The sessions dropped their references, so this closes the stores.
        drop(self.storages.take());
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

/// Accept clients until the runtime stops. Each connection takes frames of
/// at most `max_frame_size` bytes.
async fn accept(listener: TcpListener, storages: Arc<Storages>, max_frame_size: u32) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let _ = stream.set_nodelay(true);
                tokio::spawn(protocol::serve_connection(
                    stream,
                    Arc::clone(&storages),
                    max_frame_size,
                ));
            }
            Err(error) => {
                eprintln!("zoe serve: accept failed: {error}");
                tokio::task::yield_now().await;
            }
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
