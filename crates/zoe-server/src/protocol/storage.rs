//! The stores that a server serves: each one under its name, with its
//! counters, its metrics and its invalidations (`NamedStorage`), and the
//! list of them that all the connections share (`Storages`).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use zoe_core::Store;

use super::compress::Compressor;
use super::invalidation::Invalidations;
use super::metrics::{Exported, Metrics};
use super::pipeline::Pool;
use super::resolution::ResolutionLog;
use super::session;
use super::status::{self, Stats};

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
    pub(super) name: String,
    /// The directory of the store.
    pub(super) path: PathBuf,
    pub(super) store: Arc<Store>,
    pub(super) stats: Arc<Stats>,
    /// The metrics that the status of ZEO does not keep.
    pub(super) metrics: Arc<Metrics>,
    /// The publication of the commits to the connections.
    pub(super) invalidations: Arc<Invalidations>,
    pub(super) queues: Queues,
    /// True when the server opened the store for reads only.
    pub(super) read_only: bool,
    /// True when the clients share the blob directory of the store, so
    /// that `storeBlobShared` is accepted.
    pub(super) shared_blob_dir: bool,
    /// True when the server closes a connection that stays silent, or a
    /// voted transaction that stays open, so that an open transaction
    /// cannot hold its locks forever.
    pub(super) timeout: bool,
    /// The resolution log of the server, if `--resolution-log` names one.
    pub(super) resolution_log: Option<Arc<ResolutionLog>>,
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
    pub(super) in_memory: bool,
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
        }
    }

    /// Return the status of the storage. It reads the store.
    pub(super) fn status(&self) -> status::Status {
        self.stats
            .status(self.store.last_transaction(), self.timeout)
    }

    /// Return the size of the files in the store directory, in bytes, as
    /// `zoe.Storage.getSize` gives it. The blob directory is not counted.
    pub(super) fn size(&self) -> io::Result<u64> {
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
    pub(super) storages: Vec<Arc<NamedStorage>>,
    /// The most invalidations that wait for one client. A connection gets
    /// its queues before `register` selects its storage.
    pub(super) client_queue: usize,
    /// The read pool of all the connections.
    pub(super) pool: Pool,
    /// The compression pool of all the connections.
    pub(super) compressor: Compressor,
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
    pub(super) fn ruok(&self) -> String {
        let statuses: Vec<_> = self
            .storages
            .iter()
            .map(|storage| (storage.name.as_str(), storage.status()))
            .collect();
        status::ruok(&statuses)
    }
}
