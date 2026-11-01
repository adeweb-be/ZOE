//! The settings of a server: [`ServeOptions`] and the [`StorageOptions`]
//! of each store, the [`Address`] of the ZEO port, and their defaults.

use std::fmt;
use std::net::{Ipv4Addr, SocketAddr};
use std::num::{NonZeroU32, NonZeroUsize};
use std::path::{Path, PathBuf};
use std::time::Duration;

use zoe_core::{Compression, Options, StructuralMerges};

use super::Error;
use crate::defaults;
use crate::protocol::{self, Pool, Queues};
use crate::tls::TlsOptions;

/// The default of `--max-frame-size`: 256 MiB. A `storea` or `restorea`
/// frame holds a whole record, and the store sets no limit on a record by
/// default. ZODB warns about a record of more than 16 MiB, and a blob chunk
/// is less than 64 KiB. So a stock client stays far below this limit.
pub const DEFAULT_MAX_FRAME_SIZE: NonZeroU32 = NonZeroU32::new(256 << 20).unwrap();

/// The default of `--shutdown-timeout`: 30 seconds.
pub const DEFAULT_SHUTDOWN_TIMEOUT: Duration = whole_seconds(defaults::SHUTDOWN_TIMEOUT);

/// The default of `--idle-timeout`: 300 seconds. A stock client sends a
/// heartbeat every 60 seconds, so it keeps its connection.
pub const DEFAULT_IDLE_TIMEOUT: Duration = whole_seconds(defaults::IDLE_TIMEOUT);

/// Return the time of `text`, a whole number of seconds without a unit.
/// It reads the defaults that the command line shares.
const fn whole_seconds(text: &str) -> Duration {
    let digits = text.as_bytes();
    let mut seconds = 0;
    let mut i = 0;
    while i < digits.len() {
        assert!(
            digits[i].is_ascii_digit(),
            "a default time is whole seconds"
        );
        seconds = seconds * 10 + (digits[i] - b'0') as u64;
        i += 1;
    }
    Duration::from_secs(seconds)
}

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
    pub(super) name: String,
    pub(super) path: PathBuf,
    pub(super) blob_dir: Option<PathBuf>,
    pub(super) read_only: bool,
    pub(super) pack_gc: bool,
}

impl StorageOptions {
    /// Serve the store in `path` under the storage name `name`. The blob
    /// files go to `blobs` in the store directory.
    pub fn new(name: impl Into<String>, path: impl AsRef<Path>) -> Self {
        Self {
            name: name.into(),
            path: path.as_ref().to_path_buf(),
            blob_dir: None,
            read_only: defaults::READ_ONLY,
            pack_gc: defaults::PACK_GC,
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
    /// opens the existing store with `Store::open_read_only_repairing`: it
    /// changes no byte of a store that was closed cleanly, and holds its
    /// lock. It repairs a store that a writer left open, and logs a
    /// warning. A store with blobs must get its blob directory, or the
    /// start fails.
    #[must_use]
    pub fn read_only(mut self, read_only: bool) -> Self {
        self.read_only = read_only;
        self
    }

    /// Pack the store without garbage collection, when `pack_gc` is false:
    /// a pack keeps the objects that no reader can reach, and removes old
    /// revisions only. The store gets `Options::pack_gc` false. Without
    /// this call, a pack collects garbage.
    #[must_use]
    pub fn pack_gc(mut self, pack_gc: bool) -> Self {
        self.pack_gc = pack_gc;
        self
    }
}

/// The settings of a server.
#[derive(Debug, Clone)]
pub struct ServeOptions {
    pub(super) storages: Vec<StorageOptions>,
    pub(super) listen: Address,
    pub(super) admin_listen: Option<SocketAddr>,
    pub(super) queues: Queues,
    pub(super) lock_timeout: Option<Duration>,
    pub(super) cache_size: Option<usize>,
    pub(super) compression: Option<Compression>,
    pub(super) lock_warn: Option<Duration>,
    pub(super) transaction_warn: Option<Duration>,
    pub(super) max_frame_size: NonZeroU32,
    pub(super) shutdown_timeout: Duration,
    pub(super) idle_timeout: Duration,
    pub(super) transaction_timeout: Duration,
    pub(super) blocking_threads: Option<NonZeroUsize>,
    pub(super) read_only: bool,
    pub(super) pack_gc: bool,
    pub(super) shared_blob_dir: bool,
    pub(super) tls: Option<TlsOptions>,
    pub(super) structural_merges: StructuralMerges,
    pub(super) resolution_log: Option<PathBuf>,
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
            compression: None,
            lock_warn: None,
            transaction_warn: None,
            max_frame_size: DEFAULT_MAX_FRAME_SIZE,
            shutdown_timeout: DEFAULT_SHUTDOWN_TIMEOUT,
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
            transaction_timeout: Duration::ZERO,
            blocking_threads: None,
            read_only: defaults::READ_ONLY,
            pack_gc: defaults::PACK_GC,
            shared_blob_dir: false,
            tls: None,
            structural_merges: StructuralMerges::default(),
            resolution_log: None,
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

    /// Let `getInvalidations` cover every commit after a tid whose time is
    /// less than `age` before the clock of the server, past the
    /// invalidation queue size, as the key `invalidation-age` of ZEO.
    /// Zero, the default, turns it off.
    #[must_use]
    pub fn invalidation_age(mut self, age: Duration) -> Self {
        self.queues.invalidation_age = age;
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

    /// Write the new records of each store with `compression`. Without this
    /// call, each store keeps the default of the store, none. A store reads
    /// every compression, whatever this setting.
    #[must_use]
    pub fn compression(mut self, compression: Compression) -> Self {
        self.compression = Some(compression);
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

    /// Close the connection of a transaction that stays open for longer
    /// than `timeout` after a vote that passed, as the key
    /// `transaction-timeout` of ZEO does. The transaction holds its locks
    /// from that vote until it ends. The close aborts it and releases its
    /// locks, and the server logs one ERROR line. The calls of the
    /// transaction that wait do not run. A connection without a voted
    /// transaction stays open, whatever its idle time. A timeout of 0, the
    /// default, closes no connection for this reason.
    #[must_use]
    pub fn transaction_timeout(mut self, timeout: Duration) -> Self {
        self.transaction_timeout = timeout;
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
    pub(super) fn pool_threads(&self) -> NonZeroUsize {
        self.blocking_threads.unwrap_or_else(Pool::cores)
    }

    /// Serve every store for reads only, when `read_only` is true, as
    /// [`StorageOptions::read_only`] does for one store.
    #[must_use]
    pub fn read_only(mut self, read_only: bool) -> Self {
        self.read_only = read_only;
        self
    }

    /// Pack every store without garbage collection, when `pack_gc` is
    /// false, as [`StorageOptions::pack_gc`] does for one store. A store
    /// packs with garbage collection only when both are true.
    #[must_use]
    pub fn pack_gc(mut self, pack_gc: bool) -> Self {
        self.pack_gc = pack_gc;
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

    /// Run the structural merges (tier 1) of each store with `merges`: the
    /// mode of their classes and the rules of ADR 0042. Without this call,
    /// tier 1 is on for every class and the rules are off.
    #[must_use]
    pub fn structural_merges(mut self, merges: StructuralMerges) -> Self {
        self.structural_merges = merges;
        self
    }

    /// Write one JSON line for each result of tier 1 to the file `path`, in
    /// append mode (ADR 0042). The start creates the file, and fails when it
    /// cannot open it. Without this call, the server writes no such line.
    #[must_use]
    pub fn resolution_log(mut self, path: impl AsRef<Path>) -> Self {
        self.resolution_log = Some(path.as_ref().to_path_buf());
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
    pub(super) fn is_read_only(&self, storage: &StorageOptions) -> bool {
        self.read_only || storage.read_only
    }

    /// The options of the store of `storage`.
    pub(super) fn store_options(&self, storage: &StorageOptions) -> Options {
        let mut options = Options::default();
        if let Some(timeout) = self.lock_timeout {
            options.lock_timeout = timeout;
        }
        options.cache_size = self.cache_size;
        if let Some(compression) = self.compression {
            options.compression = compression;
        }
        options.lock_warn = self.lock_warn;
        options.transaction_warn = self.transaction_warn;
        options.blob_dir.clone_from(&storage.blob_dir);
        options.pack_gc = self.pack_gc && storage.pack_gc;
        options.structural_merges = self.structural_merges.clone();
        options.structural_merges.resolution_log = self.resolution_log.is_some();
        options
    }

    /// Check that one storage at least is given, and that no name comes
    /// twice.
    pub(super) fn check_storages(&self) -> Result<(), Error> {
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
