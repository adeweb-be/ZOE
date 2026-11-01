//! `zoe._zoe.Storage`: the ZODB storage API over a core store.
//!
//! `zoe.storage.Storage` subclasses it in Python. The class releases the
//! GIL around every call into the core, and holds no mutex while Python
//! code runs. The per-OID locks of a vote are not mutexes: they stay held
//! while `_p_resolveConflict` runs.
//!
//! The storage keeps a table of the open transactions, keyed by the
//! address of the ZODB transaction object. Each entry keeps the object
//! alive, so no other object gets its address while the entry exists.
//!
//! A lock guard never lives into Python code: a lookup binds its result
//! to a local before an exception is built. The last reference to the
//! store drops without the GIL, so the database closes without it.
//!
//! `storeBlob` and `restoreBlob` take a file of `temporaryDirectory()` by
//! rename, through the core, and copy a file from elsewhere. `loadBlob`
//! gives the path of a committed blob file. The core keeps the blob files
//! in `blob_dir`, `<path>/blobs` by default.
//!
//! `_iterator` reads one transaction at a time through the transaction
//! index of the core. `restore` and `restoreBlob` copy a record into a
//! restore transaction under the tid of the transaction.
//!
//! `_vote_resolving` votes and keeps the locks when it finds conflicts.
//! `zoe.storage.Storage.tpc_vote` resolves them in Python, stores the
//! resolved pickles and calls `tpc_vote` of this class. `tpc_abort` and
//! `close` can release the locks while the resolution runs, because the
//! resolution holds no mutex of the transaction.
//!
//! `_undo` stages the undo of a transaction through the core, which merges
//! with tiers 1 and 2, and writes the lines of its merges to the resolution
//! log when it ends. `zoe.storage.Storage.undo` resolves in Python the oids
//! that no tier resolves. `_undo_log` reads the transactions that an undo can
//! undo.
//!
//! The store and the table live in a `Shared` part, which the watcher of
//! the signals also holds (see `signals`). The store closes once: in
//! `close`, in the drop or in the watcher, whichever takes it first.
//!
//! The KV namespaces (ADR 0043): `kv_get`, `kv_scan` and `kv_generation`
//! read them, and `kv_set` and `kv_delete` stage a write in a transaction,
//! as `store` stages a pickle. A writable store that has the table of the
//! ephemeral namespaces runs a sweep step once a minute, on a thread of
//! the extension that runs no Python. The thread starts at the open of
//! such a store, or at its first ephemeral write, and stops at the close.
//!
//! `table` holds the table of the open transactions and the part of the
//! storage that the watcher shares. `resolution_log` holds the resolution
//! log, and `blob` the take of a blob file.

mod blob;
mod resolution_log;
mod table;

use std::collections::HashMap;
use std::collections::hash_map::Entry::{Occupied, Vacant};
use std::ffi::OsString;
use std::fs;
use std::io;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak};
use std::thread;
use std::time::Duration;

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyString};
use zoe_client::lock;
use zoe_core::{
    Compression, Flavour, IntentLocks, Mode, Oid, Options, SWEEP_INTERVAL, SWEEP_LIMIT, Store, Tid,
    Txn, ZoeError,
};

use crate::errors::{self, Failure, p64, u64_of};
use crate::kv;
use crate::publish::{Publication, Published};
use crate::sequence::Sequence;
use crate::signals;
use blob::take_blob;
use resolution_log::{ResolutionLog, warn_log_failure};
pub(crate) use table::Shared;
use table::{Entry, Table, abort_all};

/// A transaction of the core that shares its store.
type SharedTxn = Txn<StoreRef>;

/// A share of a store. The database closes when the last share drops.
#[derive(Clone)]
pub(crate) struct StoreRef(Arc<Owned>);

/// A store and the flag that tells when its database is closed.
struct Owned {
    store: Store,
    /// The fields drop in the order of their declaration. So this flag is
    /// set after the store drops. A read through a weak handle of the core,
    /// such as an iterator, can still hold the database then.
    _closed: SetOnDrop,
}

/// A flag that its drop sets.
struct SetOnDrop(Arc<AtomicBool>);

impl Drop for SetOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

impl Deref for StoreRef {
    type Target = Store;

    fn deref(&self) -> &Store {
        &self.0.store
    }
}

impl StoreRef {
    /// Drop the store if this is its last share: the database closes.
    /// Give the share back when another share exists.
    pub(crate) fn try_close(self) -> Result<(), Self> {
        Arc::try_unwrap(self.0).map(drop).map_err(Self)
    }
}

/// An owner of early write-intent locks of a `Storage` (ADR 0045). The
/// drop releases its locks.
#[pyclass(frozen, module = "zoe._zoe", name = "IntentLocks")]
pub(crate) struct Intents {
    /// The locks, from the first `lock_intent`, with their share of the
    /// store.
    intents: Mutex<Option<IntentLocks<StoreRef>>>,
}

impl Intents {
    fn lock(&self) -> MutexGuard<'_, Option<IntentLocks<StoreRef>>> {
        lock(&self.intents)
    }
}

/// The thread that runs the sweep steps of the ephemeral namespaces of a
/// store, once a minute. It holds a weak reference to the store, so it
/// never keeps the store open between two steps, and it runs no Python.
/// `stop` stops it and waits for its end, so a step that runs ends before
/// the store closes. The drop only tells it to stop.
struct Sweeper {
    /// True when the thread stops, and the signal that wakes it.
    stop: Arc<(Mutex<bool>, Condvar)>,
    /// The thread, until `stop` waits for it.
    thread: Option<thread::JoinHandle<()>>,
}

impl Sweeper {
    /// Start the thread of `store`. Return `None` when the system gives no
    /// thread: the expired keys then stay until a later open.
    fn start(store: &StoreRef) -> Option<Self> {
        let weak = Arc::downgrade(&store.0);
        let stop = Arc::new((Mutex::new(false), Condvar::new()));
        let signal = Arc::clone(&stop);
        let thread = thread::Builder::new()
            .name("zoe-sweep".to_owned())
            .spawn(move || sweep(&weak, &signal))
            .ok()?;
        Some(Self {
            stop,
            thread: Some(thread),
        })
    }

    /// Tell the thread to stop.
    fn signal(&self) {
        let (stopped, changed) = &*self.stop;
        *lock(stopped) = true;
        changed.notify_all();
    }

    /// Stop the thread and wait for its end. Call it without the GIL: the
    /// thread runs no Python, and a step that runs ends first.
    fn stop(mut self) {
        self.signal();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Sweeper {
    fn drop(&mut self) {
        self.signal();
    }
}

/// Run a sweep step of `store` once a minute, until `stop` or until the
/// store closes. When a step takes `SWEEP_LIMIT` keys, the next step runs
/// at once. A step that fails waits for the next minute: a read-only or a
/// halted store runs no step.
fn sweep(store: &Weak<Owned>, stop: &(Mutex<bool>, Condvar)) {
    let (stopped, changed) = stop;
    let is_stopped = || *lock(stopped);
    loop {
        {
            let guard = lock(stopped);
            let (guard, _) = changed
                .wait_timeout_while(guard, SWEEP_INTERVAL, |stopped| !*stopped)
                .unwrap_or_else(PoisonError::into_inner);
            if *guard {
                return;
            }
        }
        loop {
            let Some(owned) = store.upgrade() else {
                return;
            };
            let full =
                matches!(owned.store.sweep(SWEEP_LIMIT), Ok(swept) if swept.taken == SWEEP_LIMIT);
            // A close during the step leaves the last share here, so the
            // database closes on this thread.
            drop(owned);
            if !full || is_stopped() {
                break;
            }
        }
    }
}

/// The in-process storage. It implements ZODB's `IStorage`.
#[pyclass(subclass, frozen, module = "zoe._zoe", name = "Storage")]
pub(crate) struct Storage {
    /// The store and the table of the open transactions.
    shared: Arc<Shared>,
    /// The absolute path of the store directory.
    path: String,
    read_only: bool,
    published: Published,
    /// The thread of the sweep steps, from the open of a store with
    /// ephemeral namespaces or from the first ephemeral write.
    sweeper: Mutex<Option<Sweeper>>,
    /// The resolution log (ADR 0042), if `resolution_log` names one.
    resolution_log: Option<Arc<ResolutionLog>>,
    /// True when the database of the storage gives each Connection the
    /// oids of a commit that the vote wrote without a store (ADR 0049).
    server_writes: AtomicBool,
}

impl Storage {
    /// Fail with `ReadOnlyError` when the storage is read-only.
    fn writable(&self, py: Python<'_>) -> PyResult<()> {
        if self.read_only {
            return Err(errors::read_only(py));
        }
        Ok(())
    }

    /// Return the store, or fail when the storage is closed.
    fn opened(&self, py: Python<'_>) -> PyResult<StoreRef> {
        // The guard drops at the end of this statement. Building the
        // exception runs Python code, so it must come after.
        let store = self
            .shared
            .store
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        store.ok_or_else(|| errors::closed(py))
    }

    /// Fail when the storage is closed. Take no reference to the store, so
    /// that the store never drops while the thread is attached.
    fn check_open(&self, py: Python<'_>) -> PyResult<()> {
        // The guard drops at the end of this statement. Building the
        // exception runs Python code, so it must come after.
        let open = self
            .shared
            .store
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some();
        if open {
            Ok(())
        } else {
            Err(errors::closed(py))
        }
    }

    fn table(&self) -> MutexGuard<'_, Table> {
        self.shared.table()
    }

    /// Return the entry of `transaction`. Fail when the storage did not
    /// begin it.
    fn entry(&self, py: Python<'_>, transaction: &Bound<'_, PyAny>) -> PyResult<Arc<Entry>> {
        let key = transaction.as_ptr() as usize;
        // The guard drops at the end of this statement, before the
        // exception runs Python code.
        let found = self.table().entries.get(&key).cloned();
        found.ok_or_else(|| errors::unknown_transaction(py))
    }

    /// Remove the entry of `transaction` from the table and return it.
    fn remove(&self, transaction: &Bound<'_, PyAny>) -> Option<Arc<Entry>> {
        let key = transaction.as_ptr() as usize;
        self.table().entries.remove(&key)
    }

    /// Start the thread of the sweep steps, once. A read-only storage runs
    /// none.
    fn start_sweeper(&self, py: Python<'_>) {
        if self.read_only {
            return;
        }
        let mut sweeper = lock(&self.sweeper);
        if sweeper.is_some() {
            return;
        }
        let store = self
            .shared
            .store
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(store) = &store {
            *sweeper = Sweeper::start(store);
        }
        drop(sweeper);
        // The share drops without the GIL, as each share of a call does.
        py.detach(move || drop(store));
    }

    /// Take the thread of the sweep steps out of the storage. The caller
    /// stops it without the GIL.
    fn take_sweeper(&self) -> Option<Sweeper> {
        lock(&self.sweeper).take()
    }

    /// Stage a namespace write in `transaction` with `stage`, as `store`
    /// stages a pickle. A read-only storage refuses the call.
    fn stage_kv(
        &self,
        py: Python<'_>,
        transaction: &Bound<'_, PyAny>,
        stage: impl Fn(&mut SharedTxn) -> Result<(), Failure> + Send + Sync,
    ) -> PyResult<()> {
        self.writable(py)?;
        let entry = self.entry(py, transaction)?;
        let staged = match entry.try_with(&stage) {
            Some(staged) => staged,
            None => py.detach(|| entry.with(&stage)),
        };
        staged.map_err(|failure| failure.into_py(py))
    }

    /// Return the next value of the sequence `name`. A read-only storage
    /// refuses the call.
    pub(crate) fn next_value(&self, py: Python<'_>, name: &str) -> PyResult<u64> {
        self.writable(py)?;
        let store = self.opened(py)?;
        py.detach(move || store.sequence(name).next())
            .map_err(|e| core(py, e))
    }
}

#[pymethods]
impl Storage {
    /// Open the store in the directory `path`, and create it if necessary.
    /// `lock_timeout` is the longest wait of a vote for its locks, in
    /// seconds. `blob_dir` is the directory of the blob files, `None` for
    /// `<path>/blobs`. The core takes it as `Options::blob_dir`. With
    /// `pack_gc` false, a pack keeps the objects that no reader can reach:
    /// the core takes it as `Options::pack_gc`. `compression` is the
    /// compression of the records that the store writes, as text: `none`,
    /// `zstd` or `zstd:LEVEL`; `None` keeps the default of the store. The
    /// core takes it as `Options::compression`.
    ///
    /// The open installs the handlers of SIGTERM and SIGHUP that close the
    /// stores of the process, each only when its action is the default
    /// (see `signals`).
    #[new]
    #[pyo3(signature = (
        path, *, read_only = false, lock_timeout = None, blob_dir = None, pack_gc = true,
        compression = None, structural_merges = None, structural_merge_classes = None,
        posting_dicts = None, resolution_log = None
    ))]
    #[allow(clippy::too_many_arguments)]
    fn new(
        py: Python<'_>,
        path: PathBuf,
        read_only: bool,
        lock_timeout: Option<f64>,
        blob_dir: Option<PathBuf>,
        pack_gc: bool,
        compression: Option<&str>,
        structural_merges: Option<&str>,
        structural_merge_classes: Option<HashMap<String, String>>,
        posting_dicts: Option<&str>,
        resolution_log: Option<PathBuf>,
    ) -> PyResult<Self> {
        let mut options = Options::default();
        if let Some(text) = structural_merges {
            options.structural_merges.mode = text
                .parse::<Mode>()
                .map_err(|e| PyValueError::new_err(format!("structural_merges: {e}")))?;
        }
        for (class, mode) in structural_merge_classes.unwrap_or_default() {
            let (class, mode) = zoe_core::merge::class_mode(&format!("{class}={mode}"))
                .map_err(|e| PyValueError::new_err(format!("structural_merge_classes: {e}")))?;
            options.structural_merges.classes.insert(class, mode);
        }
        if let Some(text) = posting_dicts {
            options.structural_merges.posting_dicts = text
                .parse::<Mode>()
                .map_err(|e| PyValueError::new_err(format!("posting_dicts: {e}")))?;
        }
        // In-process, the resolution log is the only place where shadow
        // mode says what it found.
        let merges = &options.structural_merges;
        let shadow = merges.mode == Mode::Shadow
            || merges.posting_dicts == Mode::Shadow
            || merges.classes.values().any(|&mode| mode == Mode::Shadow);
        if shadow && resolution_log.is_none() {
            return Err(PyValueError::new_err(
                "shadow mode needs resolution_log: without it, nothing tells what it found",
            ));
        }
        options.structural_merges.resolution_log = resolution_log.is_some();
        let resolution_log = match resolution_log {
            Some(path) => Some(Arc::new(
                ResolutionLog::open(&path).map_err(|error| core(py, error.into()))?,
            )),
            None => None,
        };
        if let Some(text) = compression {
            options.compression = text
                .parse::<Compression>()
                .map_err(|e| PyValueError::new_err(format!("compression {e}")))?;
        }
        if let Some(seconds) = lock_timeout {
            options.lock_timeout = Duration::try_from_secs_f64(seconds).map_err(|_| {
                PyValueError::new_err("lock_timeout is a number of seconds, 0 or more")
            })?;
        }
        options.blob_dir = blob_dir;
        options.pack_gc = pack_gc;
        let (store, last, path) = py
            .detach(|| {
                // A read-only storage creates no directory. It does not use
                // the read-only open of the core: it opens the store for
                // writes and takes the exclusive lock.
                if read_only {
                    let blob_parent = match &options.blob_dir {
                        Some(blob_dir) => {
                            std::path::absolute(blob_dir)?.parent().map(Path::to_owned)
                        }
                        None => None,
                    };
                    for dir in std::iter::once(path.clone()).chain(blob_parent) {
                        if !dir.is_dir() {
                            let missing = format!("no directory: {}", dir.display());
                            return Err(io::Error::new(io::ErrorKind::NotFound, missing).into());
                        }
                    }
                }
                // The blob paths that the storage gives do not depend on the
                // current directory.
                let path = std::path::absolute(&path)?;
                let store = Store::open_with(&path, options)?;
                let last = store.last_transaction();
                let path = fs::canonicalize(&path)?;
                Ok::<_, ZoeError>((store, last, path))
            })
            .map_err(|error| core(py, error))?;
        let shared = Arc::new(Shared::new(store));
        signals::register(&shared);
        if let Err(error) = signals::install(py) {
            // The table is empty, so `shared` holds no Python reference.
            // The store closes without the GIL.
            py.detach(move || drop(shared));
            return Err(error);
        }
        let storage = Self {
            shared,
            path: path.to_string_lossy().into_owned(),
            read_only,
            published: Published::new(last),
            sweeper: Mutex::new(None),
            resolution_log,
            server_writes: AtomicBool::new(false),
        };
        let store = storage.opened(py)?;
        let ephemeral = py.detach(move || store.has_ephemeral_namespaces().unwrap_or(false));
        if ephemeral {
            storage.start_sweeper(py);
        }
        Ok(storage)
    }

    /// Return the name of the storage: the path of its directory.
    #[pyo3(name = "getName")]
    fn get_name(&self) -> &str {
        &self.path
    }

    /// Return a key that orders storages in a two-phase commit. It is
    /// unique per directory.
    #[pyo3(name = "sortKey")]
    fn sort_key(&self) -> &str {
        &self.path
    }

    /// Return the size of the files of the store directory, in bytes.
    #[pyo3(name = "getSize")]
    fn get_size(&self, py: Python<'_>) -> PyResult<u64> {
        Ok(py.detach(|| directory_size(Path::new(&self.path)))?)
    }

    /// Return the object count of the store: the oids that have a record,
    /// a deletion included, as the `len()` of FileStorage. A closed
    /// storage gives 0, so that the truth value of a storage never raises.
    fn __len__(&self, py: Python<'_>) -> usize {
        let Ok(store) = self.opened(py) else {
            return 0;
        };
        // The reference drops without the GIL, as in every other call.
        let count = py.detach(move || store.object_count());
        usize::try_from(count.unwrap_or(0)).unwrap_or(usize::MAX)
    }

    #[pyo3(name = "isReadOnly")]
    fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// Return the last tid that ZODB can see: the tid of the last commit
    /// whose `tpc_finish` callback ran.
    #[pyo3(name = "lastTransaction")]
    fn last_transaction<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyBytes>> {
        let store = self.opened(py)?;
        let tid = py.detach(move || self.published.wait_for(store.last_transaction()));
        Ok(p64(py, tid.0))
    }

    /// Return a new oid.
    fn new_oid<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyBytes>> {
        self.writable(py)?;
        let store = self.opened(py)?;
        let oid = py
            .detach(move || store.new_oid())
            .map_err(|e| core(py, e))?;
        Ok(p64(py, oid.0))
    }

    /// Return the current pickle of `oid` and its serial.
    #[pyo3(signature = (oid, version = None))]
    fn load<'py>(
        &self,
        py: Python<'py>,
        oid: &Bound<'py, PyAny>,
        version: Option<&Bound<'py, PyAny>>,
    ) -> PyResult<(Bound<'py, PyBytes>, Bound<'py, PyBytes>)> {
        let _ = version;
        let oid = Oid(u64_of(oid)?);
        let store = self.opened(py)?;
        let (data, serial) = py
            .detach(move || store.load(oid))
            .map_err(|e| core(py, e))?;
        Ok((PyBytes::new(py, &data), p64(py, serial.0)))
    }

    /// Return the record of `oid` that was current before `tid`: its
    /// pickle, its serial and the serial of the next record. Return `None`
    /// when `oid` has no record before `tid`.
    #[pyo3(name = "loadBefore")]
    fn load_before<'py>(
        &self,
        py: Python<'py>,
        oid: &Bound<'py, PyAny>,
        tid: &Bound<'py, PyAny>,
    ) -> PyResult<Option<LoadedRow<'py>>> {
        let (oid, tid) = (Oid(u64_of(oid)?), Tid(u64_of(tid)?));
        let store = self.opened(py)?;
        let record = py
            .detach(move || store.load_before(oid, tid))
            .map_err(|e| core(py, e))?;
        Ok(record.map(|record| {
            (
                PyBytes::new(py, &record.data),
                p64(py, record.serial.0),
                record.next.map(|next| p64(py, next.0)),
            )
        }))
    }

    /// Return the pickle that the transaction `serial` wrote for `oid`.
    #[pyo3(name = "loadSerial")]
    fn load_serial<'py>(
        &self,
        py: Python<'py>,
        oid: &Bound<'py, PyAny>,
        serial: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyBytes>> {
        let (oid, serial) = (Oid(u64_of(oid)?), Tid(u64_of(serial)?));
        let store = self.opened(py)?;
        let data = py
            .detach(move || store.load_serial(oid, serial))
            .map_err(|e| core(py, e))?;
        Ok(PyBytes::new(py, &data))
    }

    /// Return at most `size` records of `oid`, newest first, as tuples of
    /// the tid, the user name, the description and the size of the pickle.
    /// `zoe.storage.Storage.history` makes the dictionaries of ZODB.
    #[pyo3(name = "_history")]
    fn history<'py>(
        &self,
        py: Python<'py>,
        oid: &Bound<'py, PyAny>,
        size: usize,
    ) -> PyResult<Vec<HistoryRow<'py>>> {
        let oid = Oid(u64_of(oid)?);
        let store = self.opened(py)?;
        let entries = py
            .detach(move || store.history(oid, size))
            .map_err(|e| core(py, e))?;
        Ok(entries
            .into_iter()
            .map(|entry| {
                (
                    p64(py, entry.tid.0),
                    PyBytes::new(py, &entry.user),
                    PyBytes::new(py, &entry.description),
                    entry.size,
                )
            })
            .collect())
    }

    /// Return an iterator over the transactions from `start` to `stop`,
    /// both included, in tid order. `None` leaves a bound open. Each item
    /// is a tuple of the tid, the user name, the description, the pickled
    /// extension and the records. Each record is a tuple of the oid, the
    /// tid and the pickle, which is `None` for a deletion.
    /// `zoe.storage.Storage.iterator` makes the records of ZODB.
    ///
    /// The iterator stops at the last tid at the time of this call. It
    /// holds one transaction and a slice of tids at a time, whatever the
    /// length of the range.
    #[pyo3(name = "_iterator", signature = (start = None, stop = None))]
    fn iterator(
        &self,
        py: Python<'_>,
        start: Option<&Bound<'_, PyAny>>,
        stop: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Transactions> {
        let start = start.map(u64_of).transpose()?.map(Tid);
        let stop = stop.map(u64_of).transpose()?.map(Tid);
        let store = self.opened(py)?;
        let transactions = py
            .detach(move || store.iterator(start, stop))
            .map_err(|e| core(py, e))?;
        Ok(Transactions {
            transactions: Mutex::new(transactions),
        })
    }

    /// Return the current record of the first object at or after the oid
    /// `next`, and the next oid that has a current record: a tuple of the
    /// oid, the serial, the pickle and the next oid. The next oid is `None`
    /// at the end. `None` starts at the first oid. A deleted object has no
    /// current record. Raise `ValueError` when no object at or after `next`
    /// has one, as FileStorage does.
    #[pyo3(signature = (next = None))]
    fn record_iternext<'py>(
        &self,
        py: Python<'py>,
        next: Option<&Bound<'py, PyAny>>,
    ) -> PyResult<CurrentRow<'py>> {
        let next = next.map(u64_of).transpose()?.map(Oid);
        let store = self.opened(py)?;
        let record = py
            .detach(move || store.record_iternext(next))
            .map_err(|e| core(py, e))?
            .ok_or_else(|| PyValueError::new_err("no object at or after the oid"))?;
        Ok((
            p64(py, record.oid.0),
            p64(py, record.tid.0),
            PyBytes::new(py, &record.data),
            record.next.map(|oid| p64(py, oid.0)),
        ))
    }

    /// Pack the store at `tid`: remove the records that no reader at or
    /// after `tid` needs, with their blob files. `zoe.storage.Storage.pack`
    /// gives the tid of the pack time.
    ///
    /// A storage opened with `pack_gc` false keeps the objects that no
    /// reader can reach, except an object whose newest record at or before
    /// `tid` is a deletion.
    ///
    /// The whole pack runs without the GIL. Other threads read and commit
    /// while it runs. A read-only storage refuses the pack.
    #[pyo3(name = "_pack")]
    fn pack(&self, py: Python<'_>, tid: &Bound<'_, PyAny>) -> PyResult<()> {
        self.writable(py)?;
        let tid = Tid(u64_of(tid)?);
        let store = self.opened(py)?;
        py.detach(move || store.pack(tid))
            .map(|_report| ())
            .map_err(|error| errors::pack_failure(py, error))
    }

    /// Return the sequence `name` of the store. Its `next()` gives values
    /// that are unique and strictly increasing per store and per name.
    fn sequence(slf: &Bound<'_, Self>, name: String) -> PyResult<Sequence> {
        slf.get().check_open(slf.py())?;
        Ok(Sequence::new(slf.clone().unbind(), name))
    }

    /// Return the value of `key` in the namespace `name` of `flavour`,
    /// `"ephemeral"` or `"durable"`, as bytes, or `None` when the key has
    /// no value or is expired.
    ///
    /// The read gives the last committed value, not the value at the
    /// snapshot of a connection: the namespaces keep no history. A bad
    /// name or key raises `ValueError`. A row that fails its checksum
    /// raises `StorageSystemError`.
    fn kv_get<'py>(
        &self,
        py: Python<'py>,
        flavour: &str,
        name: &str,
        key: &[u8],
    ) -> PyResult<Option<Bound<'py, PyBytes>>> {
        let flavour = kv::flavour(flavour)?;
        let store = self.opened(py)?;
        let value = py
            .detach(move || store.kv_get(flavour, name, key))
            .map_err(|e| core(py, e))?;
        Ok(value.map(|value| PyBytes::new(py, &value)))
    }

    /// Return the pairs `(key, value)` of the namespace `name` of
    /// `flavour`, in the byte order of the keys, from `start` (included) to
    /// `stop` (excluded). `None` is an open end. The list holds at most
    /// `limit` pairs, and at most 1,000, without the expired keys. A
    /// `limit` below 1 raises `ValueError`.
    #[pyo3(signature = (flavour, name, start = None, stop = None, limit = 1000))]
    fn kv_scan<'py>(
        &self,
        py: Python<'py>,
        flavour: &str,
        name: &str,
        start: Option<&[u8]>,
        stop: Option<&[u8]>,
        limit: i64,
    ) -> PyResult<Vec<(Bound<'py, PyBytes>, Bound<'py, PyBytes>)>> {
        let flavour = kv::flavour(flavour)?;
        let limit = usize::try_from(limit)
            .map_err(|_| PyValueError::new_err("the limit of a scan is less than 1"))?;
        let store = self.opened(py)?;
        let pairs = py
            .detach(move || store.kv_scan(flavour, name, start, stop, limit))
            .map_err(|e| core(py, e))?;
        Ok(pairs
            .iter()
            .map(|(key, value)| (PyBytes::new(py, key), PyBytes::new(py, value)))
            .collect())
    }

    /// Tell if the storage has KV namespaces: a `zoe.Storage` always has.
    fn kv_supported(&self) -> bool {
        true
    }

    /// Return the generation of the durable namespace `name`, as the 8
    /// bytes of a tid: the tid of the last commit that wrote it, or 8 zero
    /// bytes before the first one. A cache of the namespace stays valid
    /// while its generation stays the same.
    fn kv_generation<'py>(&self, py: Python<'py>, name: &str) -> PyResult<Bound<'py, PyBytes>> {
        let store = self.opened(py)?;
        let tid = py
            .detach(move || store.kv_generation(name))
            .map_err(|e| core(py, e))?;
        Ok(p64(py, tid.0))
    }

    /// Stage a write of `value` for `key` in the namespace `name` of
    /// `flavour` in `transaction`, with the rules of `store`. An ephemeral
    /// key needs `ttl`, an int or a float of seconds, more than 0 and at
    /// most 2^32 - 1; a durable key takes `None`. The write commits with
    /// the transaction, takes no lock, and the commit with the later tid
    /// wins. A bad name, key or TTL raises `ValueError`. A read-only
    /// storage refuses the call.
    #[allow(clippy::too_many_arguments)]
    fn kv_set(
        &self,
        py: Python<'_>,
        flavour: &str,
        name: &str,
        key: &[u8],
        value: &[u8],
        ttl: &Bound<'_, PyAny>,
        transaction: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        let flavour = kv::flavour(flavour)?;
        let ttl = kv::ttl(ttl)?;
        self.stage_kv(py, transaction, |txn| {
            Ok(txn.kv_set(flavour, name, key, value, ttl)?)
        })?;
        if flavour == Flavour::Ephemeral {
            self.start_sweeper(py);
        }
        Ok(())
    }

    /// Stage a delete of `key` in the namespace `name` of `flavour` in
    /// `transaction`, with the rules of `kv_set`. A delete of a key that
    /// has no value does nothing.
    fn kv_delete(
        &self,
        py: Python<'_>,
        flavour: &str,
        name: &str,
        key: &[u8],
        transaction: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        let flavour = kv::flavour(flavour)?;
        self.stage_kv(
            py,
            transaction,
            |txn| Ok(txn.kv_delete(flavour, name, key)?),
        )
    }

    /// Return a new owner of early write-intent locks (ADR 0045). It holds
    /// no lock and no share of the store.
    #[pyo3(name = "_intents")]
    fn intents(&self) -> Intents {
        Intents {
            intents: Mutex::new(None),
        }
    }

    /// Take the early write-intent lock of each oid of `oids`, in ascending
    /// order, for `owner`, and return the current serial of each oid, when
    /// the store holds them all. An owner with locks keeps a share of the
    /// store until `unlock_intent` or the `tpc_begin` that adopts them.
    ///
    /// An oid at or below an oid that `owner` holds raises
    /// `StorageTransactionError` and takes nothing. A wait past the lock
    /// timeout raises `ConflictError`. A read-only storage raises
    /// `ReadOnlyError`.
    #[pyo3(name = "_lock_intent")]
    fn lock_intent<'py>(
        &self,
        py: Python<'py>,
        owner: &Intents,
        oids: Vec<Bound<'py, PyAny>>,
    ) -> PyResult<Vec<Bound<'py, PyBytes>>> {
        self.writable(py)?;
        let oids = oids
            .iter()
            .map(|oid| u64_of(oid).map(Oid))
            .collect::<PyResult<Vec<Oid>>>()?;
        let store = self.opened(py)?;
        let serials = py
            .detach(|| {
                let mut intents = owner.lock();
                let intents = intents.get_or_insert_with(|| IntentLocks::new(store));
                intents.lock(&oids)
            })
            .map_err(|e| core(py, e))?;
        Ok(serials.into_iter().map(|tid| p64(py, tid.0)).collect())
    }

    /// Release every intent lock of `owner`, and its share of the store.
    /// Without locks, do nothing.
    #[pyo3(name = "_unlock_intent")]
    fn unlock_intent(&self, py: Python<'_>, owner: &Intents) {
        py.detach(|| drop(owner.lock().take()));
    }

    /// Begin a two-phase commit of `transaction`. The calling thread
    /// drives it.
    ///
    /// With `tid`, the transaction is a restore transaction: a copy of a
    /// transaction of another storage, which commits under `tid`. It takes
    /// `restoreBlob`. `tid` must be after the last tid of the store.
    ///
    /// `status` is the status of the copied transaction, as ZODB's copy
    /// functions give it: `" "` or `"p"`, as text or bytes. The store keeps
    /// no transaction status, so the status is not kept. Any other status
    /// raises `StorageTransactionError`.
    ///
    /// With `intents`, an owner of early write-intent locks of this
    /// storage, the transaction adopts its locks: the vote counts them as
    /// its own, and the end of the transaction releases them.
    #[pyo3(signature = (transaction, tid = None, status = None, intents = None))]
    fn tpc_begin(
        &self,
        py: Python<'_>,
        transaction: &Bound<'_, PyAny>,
        tid: Option<&Bound<'_, PyAny>>,
        status: Option<&Bound<'_, PyAny>>,
        intents: Option<&Intents>,
    ) -> PyResult<()> {
        self.writable(py)?;
        let key = transaction.as_ptr() as usize;
        let begun = self.table().entries.contains_key(&key);
        if begun {
            return Err(errors::duplicate_transaction(py));
        }
        if let Some(status) = status {
            check_status(status)?;
        }
        let tid = tid.map(u64_of).transpose()?.map(Tid);
        let user = text(&transaction.getattr("user")?)?;
        let description = text(&transaction.getattr("description")?)?;
        let extension = text(&transaction.getattr("extension_bytes")?)?;
        let store = self.opened(py)?;
        let server_writes = self.server_writes.load(Ordering::Acquire);
        let txn = py
            .detach(move || {
                let mut txn = match tid {
                    None => Txn::begin(store, &user, &description, &extension),
                    Some(tid) => Txn::begin_restore(store, tid, &user, &description, &extension)?,
                };
                txn.set_server_writes(server_writes);
                // An owner whose lock waits on another thread has no locks
                // to give yet: the transaction begins without them.
                if let Some(owner) = intents
                    && let Ok(mut held) = owner.intents.try_lock()
                    && let Some(held) = held.as_mut()
                {
                    txn.adopt_intents(held)?;
                }
                Ok(txn)
            })
            .map_err(|e| core(py, e))?;
        let entry = Arc::new(Entry {
            _transaction: transaction.clone().unbind(),
            txn: Mutex::new(Some(txn)),
        });
        // `close` can run since the store was read. The table refuses the
        // transaction then, so that no transaction keeps the store open
        // after `close`.
        let refused: Option<fn(Python<'_>) -> PyErr> = {
            let mut table = self.table();
            if table.closed {
                Some(errors::closed)
            } else {
                match table.entries.entry(key) {
                    Occupied(_) => Some(errors::duplicate_transaction),
                    Vacant(vacant) => {
                        vacant.insert(Arc::clone(&entry));
                        None
                    }
                }
            }
        };
        match refused {
            None => Ok(()),
            Some(error) => {
                py.detach(|| drop(entry.take()));
                Err(error(py))
            }
        }
    }

    /// Stage a new pickle of `oid` in `transaction`. `serial` is the serial
    /// that the transaction read, or `None` for a new object. A read-only
    /// storage refuses the call.
    fn store(
        &self,
        py: Python<'_>,
        oid: &Bound<'_, PyAny>,
        serial: &Bound<'_, PyAny>,
        data: &[u8],
        version: &Bound<'_, PyAny>,
        transaction: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        self.writable(py)?;
        let entry = self.entry(py, transaction)?;
        if version.is_truthy()? {
            return Err(errors::version(py));
        }
        let oid = Oid(u64_of(oid)?);
        let serial = serial_of(serial)?;
        let stage = |txn: &mut SharedTxn| Ok(txn.store(oid, serial, data)?);
        // Staging never waits for a lock that a thread with the GIL needs,
        // and it does not sync. A pickle that spills goes to the spill file
        // with the GIL held. So store keeps the GIL when no other call uses
        // the transaction. A detach at each store would give the GIL to
        // another thread at each object of a commit.
        let staged = match entry.try_with(stage) {
            Some(staged) => staged,
            None => py.detach(|| entry.with(stage)),
        };
        staged.map_err(|failure| failure.into_py(py))
    }

    /// Declare that `transaction` read `oid` at `serial` and depends on it.
    /// The vote fails with `ReadConflictError` if `serial` is stale.
    #[pyo3(name = "checkCurrentSerialInTransaction")]
    fn check_current_serial_in_transaction(
        &self,
        py: Python<'_>,
        oid: &Bound<'_, PyAny>,
        serial: &Bound<'_, PyAny>,
        transaction: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        let entry = self.entry(py, transaction)?;
        let (oid, serial) = (Oid(u64_of(oid)?), Tid(u64_of(serial)?));
        py.detach(|| entry.with(|txn| Ok(txn.check_current_serial(oid, serial)?)))
            .map_err(|failure| failure.into_py(py))
    }

    /// Stage a deletion of `oid` in `transaction`: a record with an empty
    /// pickle. This is `deleteObject` of ZODB's `IExternalGC`. `oldserial`
    /// is the serial that the transaction read. The vote checks it as it
    /// does for `store`: a stale serial raises `ConflictError`, and the
    /// vote never resolves it. After the commit, `load` of `oid` raises
    /// `POSKeyError`, and `loadBefore` still gives the older records until
    /// a pack removes them. A read-only storage refuses the call.
    #[pyo3(name = "deleteObject")]
    fn delete_object(
        &self,
        py: Python<'_>,
        oid: &Bound<'_, PyAny>,
        oldserial: &Bound<'_, PyAny>,
        transaction: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        self.writable(py)?;
        let entry = self.entry(py, transaction)?;
        let oid = Oid(u64_of(oid)?);
        let serial = serial_of(oldserial)?;
        py.detach(|| entry.with(|txn| Ok(txn.delete(oid, serial)?)))
            .map_err(|failure| failure.into_py(py))
    }

    /// Stage the undo of the transaction `tid` in `transaction`: for each
    /// oid of `tid`, the state before `tid`. Return the oids of `tid`, and
    /// the conflicts that no tier of the core resolves. Each conflict is a
    /// tuple: the oid, the serial of the current state, the current pickle
    /// and the pickle of the state before `tid`. `zoe.storage.Storage.undo`
    /// resolves them in Python and stores each result over its serial. The
    /// lines of the merges of the undo go to the resolution log when the
    /// call ends, also when it fails; the vote writes only its own.
    ///
    /// An undo that the core refuses raises `UndoError`: a transaction at
    /// or before the last pack, or one that the store does not have, or a
    /// later change that tier 1 refuses. A read-only storage refuses the
    /// call.
    #[pyo3(name = "_undo")]
    fn undo<'py>(
        &self,
        py: Python<'py>,
        tid: &Bound<'py, PyAny>,
        transaction: &Bound<'py, PyAny>,
    ) -> PyResult<(Vec<Bound<'py, PyBytes>>, Vec<UndoRow<'py>>)> {
        self.writable(py)?;
        let entry = self.entry(py, transaction)?;
        let tid = Tid(u64_of(tid)?);
        let log = self.resolution_log.clone();
        let mut log_failure = None;
        let undone = py.detach(|| {
            entry.with(|txn| {
                let before = txn.undo_resolution_count();
                let undone = txn.undo(tid);
                // The merges of the undo go to the log when it ends, also
                // when it fails: the vote writes only its own.
                if let Some(log) = &log {
                    log_failure = log.write_undo(txn, before..txn.undo_resolution_count());
                }
                Ok(undone?)
            })
        });
        if let Some(error) = log_failure {
            warn_log_failure(py, &error);
        }
        let undone = undone.map_err(|failure: Failure| failure.into_py(py))?;
        let oids = undone.oids.iter().map(|oid| p64(py, oid.0)).collect();
        let conflicts = undone
            .conflicts
            .iter()
            .map(|conflict| {
                (
                    p64(py, conflict.oid.0),
                    p64(py, conflict.serial.0),
                    PyBytes::new(py, &conflict.current),
                    PyBytes::new(py, &conflict.before),
                )
            })
            .collect();
        Ok((oids, conflicts))
    }

    /// Return at most `limit` transactions before the tid `before` that an
    /// undo can undo, newest first: the transactions after the last pack.
    /// `None` starts at the last transaction. Each item is a tuple of the
    /// tid, the user name, the description and the pickled extension.
    /// `zoe.storage.Storage.undoLog` makes the dictionaries of ZODB.
    #[pyo3(name = "_undo_log", signature = (before, limit))]
    fn undo_log<'py>(
        &self,
        py: Python<'py>,
        before: Option<&Bound<'py, PyAny>>,
        limit: usize,
    ) -> PyResult<Vec<UndoLogRow<'py>>> {
        let before = before.map(u64_of).transpose()?.map_or(Tid::MAX, Tid);
        let store = self.opened(py)?;
        let rows = py
            .detach(move || store.undo_log(before, limit))
            .map_err(|e| core(py, e))?;
        Ok(rows
            .into_iter()
            .map(|row| {
                (
                    p64(py, row.tid.0),
                    PyBytes::new(py, &row.user),
                    PyBytes::new(py, &row.description),
                    PyBytes::new(py, &row.extension),
                )
            })
            .collect())
    }

    /// Stage a new pickle of `oid` with the blob in the file `blobfilename`.
    /// `serial` is the serial that the transaction read, or `None` for a
    /// new object.
    ///
    /// The storage takes the file. A file of `temporaryDirectory()`, or of
    /// one of its subdirectories, moves into the store by rename, never by
    /// copy. Another file of the store directory or of the blob directory
    /// is refused with `StorageTransactionError`. A file from elsewhere is
    /// copied, then removed. A refused call leaves the file in its place.
    /// An abort removes a file that the transaction took.
    #[pyo3(name = "storeBlob")]
    #[allow(clippy::too_many_arguments)]
    fn store_blob(
        &self,
        py: Python<'_>,
        oid: &Bound<'_, PyAny>,
        serial: &Bound<'_, PyAny>,
        data: &[u8],
        blobfilename: PathBuf,
        version: &Bound<'_, PyAny>,
        transaction: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        self.writable(py)?;
        let store = self.opened(py)?;
        let entry = self.entry(py, transaction)?;
        if version.is_truthy()? {
            return Err(errors::version(py));
        }
        let oid = Oid(u64_of(oid)?);
        let serial = serial_of(serial)?;
        // The entry holds a Python reference, so it drops while the thread
        // is attached. The store drops while it is detached.
        py.detach(|| {
            let store = store;
            take_blob(
                &store,
                Path::new(&self.path),
                &entry,
                &blobfilename,
                |txn, path| txn.store_blob_at(oid, serial, data, path),
                |txn, file| txn.store_blob(oid, serial, data, file),
            )
        })
        .map_err(|failure| failure.into_py(py))
    }

    /// Stage a copy of a version of `oid` in a restore transaction: a
    /// transaction that `tpc_begin` began with a tid. The version gets that
    /// tid. `data` is the pickle, or `None` for a deletion.
    ///
    /// The previous serial of the version is the current serial of `oid` in
    /// the store, not `serial`. If another transaction changes `oid` before
    /// the vote, the vote raises `ConflictError`. The store keeps every
    /// pickle whole, so `prev_txn` is not used. A read-only storage refuses
    /// the call.
    #[allow(clippy::too_many_arguments)]
    fn restore(
        &self,
        py: Python<'_>,
        oid: &Bound<'_, PyAny>,
        serial: &Bound<'_, PyAny>,
        data: Option<&[u8]>,
        version: &Bound<'_, PyAny>,
        prev_txn: &Bound<'_, PyAny>,
        transaction: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        let _ = (serial, prev_txn);
        self.writable(py)?;
        let entry = self.entry(py, transaction)?;
        if version.is_truthy()? {
            return Err(errors::version(py));
        }
        let oid = Oid(u64_of(oid)?);
        py.detach(|| entry.with(|txn| Ok(txn.restore(oid, data)?)))
            .map_err(|failure| failure.into_py(py))
    }

    /// Stage a copy of a version of `oid` with the blob in the file
    /// `blobfilename`, in a restore transaction: a transaction that
    /// `tpc_begin` began with a tid. The version gets that tid. The storage
    /// takes the file as `storeBlob` does.
    ///
    /// The previous serial of the version is the current serial of `oid` in
    /// the store, not `serial`. If another transaction changes `oid` before
    /// the vote, the vote raises `ConflictError`. The store keeps every
    /// pickle whole, so `prev_txn` is not used.
    #[pyo3(name = "restoreBlob")]
    #[allow(clippy::too_many_arguments)]
    fn restore_blob(
        &self,
        py: Python<'_>,
        oid: &Bound<'_, PyAny>,
        serial: &Bound<'_, PyAny>,
        data: Option<&[u8]>,
        blobfilename: PathBuf,
        prev_txn: &Bound<'_, PyAny>,
        transaction: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        let _ = (serial, prev_txn);
        self.writable(py)?;
        let store = self.opened(py)?;
        let entry = self.entry(py, transaction)?;
        let oid = Oid(u64_of(oid)?);
        let data = data.unwrap_or_default();
        // The entry holds a Python reference, so it drops while the thread
        // is attached. The store drops while it is detached.
        py.detach(|| {
            let store = store;
            take_blob(
                &store,
                Path::new(&self.path),
                &entry,
                &blobfilename,
                |txn, path| txn.restore_blob_at(oid, data, path),
                |txn, file| txn.restore_blob(oid, data, file),
            )
        })
        .map_err(|failure| failure.into_py(py))
    }

    /// Return the path of the blob file that the transaction `serial` wrote
    /// for `oid`. The file is read-only. Raise `POSKeyError` when that
    /// transaction wrote no blob for `oid`, or when its file is missing, as
    /// ZODB's blob storages do.
    #[pyo3(name = "loadBlob")]
    fn load_blob(
        &self,
        py: Python<'_>,
        oid: &Bound<'_, PyAny>,
        serial: &Bound<'_, PyAny>,
    ) -> PyResult<OsString> {
        let (oid, serial) = (Oid(u64_of(oid)?), Tid(u64_of(serial)?));
        let store = self.opened(py)?;
        let path = py
            .detach(move || {
                let path = store.blob_path(oid, serial)?;
                match fs::exists(&path)? {
                    true => Ok(path),
                    false => Err(ZoeError::NotFound { oid }),
                }
            })
            .map_err(|e| core(py, e))?;
        Ok(path.into_os_string())
    }

    /// Return the directory for the files of uncommitted blobs: `tmp` in
    /// the blob directory. `storeBlob` takes a file of this directory by
    /// rename. A read-only storage raises `ReadOnlyError`.
    #[pyo3(name = "temporaryDirectory")]
    fn temporary_directory(&self, py: Python<'_>) -> PyResult<OsString> {
        self.writable(py)?;
        let store = self.opened(py)?;
        let path = py
            .detach(move || store.blob_temp_dir())
            .map_err(|e| core(py, e))?;
        Ok(path.into_os_string())
    }

    /// Lock the objects of `transaction` and check their serials. The vote
    /// waits for a lock at most the lock timeout.
    ///
    /// After `_vote_resolving` gave conflicts, the vote takes no lock. It
    /// checks the serials again with the locks of `_vote_resolving`.
    fn tpc_vote(&self, py: Python<'_>, transaction: &Bound<'_, PyAny>) -> PyResult<()> {
        let entry = self.entry(py, transaction)?;
        let log = self.resolution_log.clone();
        let mut log_failure = None;
        let voted = py
            .detach(|| {
                entry.with(|txn| {
                    let voted = txn.vote();
                    // The vote after `_vote_resolving` compares the merges
                    // of shadow mode with the resolved records: log what it
                    // found, also when the vote failed.
                    if let Some(log) = &log {
                        log_failure = log.write_verifications(txn);
                    }
                    voted.map_err(|error| vote_failure(txn, error))
                })
            })
            .map_err(|failure| failure.into_py(py));
        if let Some(error) = log_failure {
            warn_log_failure(py, &error);
        }
        voted
    }

    /// Tell the votes of the later transactions whether the database takes
    /// records that a transaction did not store (ADR 0049). The Python
    /// class sets it from `registerDB`.
    #[pyo3(name = "_set_server_writes")]
    fn set_server_writes(&self, on: bool) {
        self.server_writes.store(on, Ordering::Release);
    }

    /// Return every oid that the commit of `transaction` writes, when its
    /// last vote wrote a record that the transaction did not store. Else
    /// `None`, also for a transaction that this storage does not know.
    #[pyo3(name = "_unstored_commit")]
    fn unstored_commit<'py>(
        &self,
        py: Python<'py>,
        transaction: &Bound<'py, PyAny>,
    ) -> Option<Vec<Bound<'py, PyBytes>>> {
        let key = transaction.as_ptr() as usize;
        let entry = self.table().entries.get(&key).cloned()?;
        let oids = py
            .detach(|| {
                entry.with(|txn| {
                    Ok(txn
                        .wrote_unstored()
                        .then(|| txn.staged_oids().collect::<Vec<_>>()))
                })
            })
            .ok()??;
        Some(oids.into_iter().map(|oid| p64(py, oid.0)).collect())
    }

    /// Return the oids that tier 1 of the core merged in the last vote of
    /// `transaction` that took the locks and passed: the vote stored each
    /// merged record over the committed serial. An empty list for a
    /// transaction that this storage does not know, or that ended.
    #[pyo3(name = "_natively_resolved")]
    fn natively_resolved<'py>(
        &self,
        py: Python<'py>,
        transaction: &Bound<'py, PyAny>,
    ) -> Vec<Bound<'py, PyBytes>> {
        let key = transaction.as_ptr() as usize;
        // The guard drops at the end of this statement.
        let entry = self.table().entries.get(&key).cloned();
        let oids = match entry {
            Some(entry) => py
                .detach(|| entry.with(|txn| Ok(txn.natively_resolved())))
                .unwrap_or_default(),
            None => Vec::new(),
        };
        oids.into_iter().map(|oid| p64(py, oid.0)).collect()
    }

    /// Vote on `transaction` as `tpc_vote` does, but keep the locks when
    /// the vote finds conflicts that Python can resolve. An empty list
    /// means that the vote succeeded.
    ///
    /// Each conflict is a tuple: the oid, the serial of the committed
    /// record, the serial that the transaction read, the committed pickle
    /// and the pickle of the transaction. Store a resolved pickle of each
    /// oid with the committed serial, then call `tpc_vote`.
    ///
    /// `untransform` gives the pickle of a stored record, as the
    /// `_crs_untransform_record_data` of ZODB does. A `ConflictError` reads
    /// the class name of the object from that pickle.
    #[pyo3(name = "_vote_resolving")]
    fn vote_resolving<'py>(
        &self,
        py: Python<'py>,
        transaction: &Bound<'py, PyAny>,
        untransform: &Bound<'py, PyAny>,
    ) -> PyResult<Vec<ConflictRow<'py>>> {
        let entry = self.entry(py, transaction)?;
        let log = self.resolution_log.clone();
        let mut log_failure = None;
        let conflicts = py
            .detach(|| {
                let read = entry.with(|txn| {
                    let voted = txn.vote_resolving();
                    // The vote that takes the locks runs tier 1: log its
                    // results, also when the vote failed.
                    if let Some(log) = &log {
                        log_failure = log.write(txn);
                    }
                    let conflicts = voted.map_err(|error| vote_failure(txn, error))?;
                    // A staged pickle in the spill file of the transaction
                    // is read from the file, and that read can fail.
                    Ok(conflicts
                        .into_iter()
                        .map(|(oid, committed)| {
                            let (read, data) = txn
                                .staged(oid)?
                                .map_or((Tid::ZERO, Vec::new()), |(read, data)| {
                                    (read, data.to_vec())
                                });
                            Ok((oid, committed, read, data))
                        })
                        .collect::<Result<Vec<_>, ZoeError>>())
                })?;
                if read.is_err() {
                    // The vote keeps its locks for the resolution. The abort
                    // releases them before the error leaves `tpc_vote`.
                    if let Some(txn) = entry.take() {
                        txn.abort();
                    }
                }
                Ok(read)
            })
            .map_err(|mut failure: Failure| {
                failure.data = failure
                    .data
                    .and_then(|data| untransformed(untransform, &data));
                failure.into_py(py)
            });
        if let Some(error) = log_failure {
            warn_log_failure(py, &error);
        }
        let conflicts = conflicts?.map_err(|error| unreadable_pickle(py, error))?;
        Ok(conflicts
            .into_iter()
            .map(|(oid, committed, read, data)| {
                (
                    p64(py, oid.0),
                    p64(py, committed.tid.0),
                    p64(py, read.0),
                    PyBytes::new(py, &committed.data),
                    PyBytes::new(py, &data),
                )
            })
            .collect())
    }

    /// Commit `transaction` and return its tid. Call `func` with the tid
    /// before ZODB can see the commit.
    #[pyo3(signature = (transaction, func = None))]
    fn tpc_finish<'py>(
        &self,
        py: Python<'py>,
        transaction: &Bound<'py, PyAny>,
        func: Option<&Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyBytes>> {
        // The commit could not be published in commit order: it would wait
        // for the commit of the callback that runs it.
        if self.published.in_callback() {
            return Err(errors::commit_in_callback(py));
        }
        let entry = self
            .remove(transaction)
            .ok_or_else(|| errors::unknown_transaction(py))?;
        let publication = py
            .detach(|| {
                let txn = entry
                    .take()
                    .ok_or(ZoeError::Protocol("the transaction is finished or aborted"))?;
                let finished = txn.finish_after()?;
                Ok::<_, ZoeError>(Publication::after(
                    &self.published,
                    finished.previous,
                    finished.tid,
                ))
            })
            .map_err(|e| core(py, e))?;
        let tid = p64(py, publication.tid().0);
        if let Some(func) = func {
            publication.run(|| func.call1((&tid,)))?;
        }
        drop(publication);
        Ok(tid)
    }

    /// Abort `transaction`. A transaction that the storage does not know is
    /// ignored.
    fn tpc_abort(&self, py: Python<'_>, transaction: &Bound<'_, PyAny>) {
        if let Some(entry) = self.remove(transaction) {
            // Dropping the transaction of the core aborts it.
            py.detach(|| drop(entry.take()));
        }
    }

    /// Tell if no transaction holds or waits for a lock. This is a private
    /// hook for the tests.
    #[pyo3(name = "_lock_table_is_empty")]
    fn lock_table_is_empty(&self, py: Python<'_>) -> PyResult<bool> {
        let store = self.opened(py)?;
        Ok(py.detach(|| store.lock_table_is_empty()))
    }

    /// Close the storage: abort its open transactions and close the store.
    /// The storage refuses every later call. A call that runs when `close`
    /// starts keeps the store open until it returns.
    fn close(&self, py: Python<'_>) {
        let sweeper = self.take_sweeper();
        let entries: Vec<Arc<Entry>> = {
            let mut table = self.table();
            table.closed = true;
            table.drain()
        };
        let store = self.shared.take_store();
        let entries = py.detach(move || {
            // A sweep step that runs holds the store: it ends first, so the
            // store closes before `close` returns.
            if let Some(sweeper) = sweeper {
                sweeper.stop();
            }
            abort_all(&entries);
            drop(store);
            entries
        });
        // The entries hold Python references, so they drop while the thread
        // is attached.
        drop(entries);
    }
}

impl Drop for Storage {
    /// Close the store without the GIL when Python frees a storage that
    /// was not closed. This also holds during the shutdown of the
    /// interpreter, on the thread that finalizes it.
    fn drop(&mut self) {
        let sweeper = self.take_sweeper();
        let store = self.shared.take_store();
        let entries: Vec<Arc<Entry>> = self.shared.table().drain();
        let txns: Vec<SharedTxn> = entries.iter().filter_map(|entry| entry.take()).collect();
        let close = move || {
            if let Some(sweeper) = sweeper {
                sweeper.stop();
            }
            drop((txns, store));
        };
        Python::try_attach(|py| py.detach(close));
        // The entries hold Python references. They drop here, so that the
        // watcher of the signals, which can hold the last share of
        // `shared`, never drops one.
        drop(entries);
    }
}

/// The transactions that `_iterator` gives, in tid order.
///
/// The iterator holds only a weak reference to the store. After `close`,
/// the next call raises `StorageError`, then the iterator ends.
#[pyclass(frozen, module = "zoe._zoe", name = "Transactions")]
pub(crate) struct Transactions {
    transactions: Mutex<zoe_core::Transactions>,
}

#[pymethods]
impl Transactions {
    fn __iter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    /// Return the next transaction as a tuple of the tid, the user name,
    /// the description, the pickled extension and the records.
    fn __next__<'py>(&self, py: Python<'py>) -> PyResult<Option<TxnRow<'py>>> {
        // The mutex is taken without the GIL, and no Python code runs while
        // it is held.
        let next = py.detach(|| lock(&self.transactions).next());
        let txn = match next {
            None => return Ok(None),
            Some(txn) => txn.map_err(|e| core(py, e))?,
        };
        let records = txn
            .records
            .into_iter()
            .map(|record| {
                (
                    p64(py, record.oid.0),
                    p64(py, record.tid.0),
                    record.data.map(|data| PyBytes::new(py, &data)),
                )
            })
            .collect();
        Ok(Some((
            p64(py, txn.tid.0),
            PyBytes::new(py, &txn.user),
            PyBytes::new(py, &txn.description),
            PyBytes::new(py, &txn.extension),
            records,
        )))
    }
}

/// A record that `loadBefore` gives: the pickle, its serial and the
/// serial of the next record.
type LoadedRow<'py> = (
    Bound<'py, PyBytes>,
    Bound<'py, PyBytes>,
    Option<Bound<'py, PyBytes>>,
);

/// A conflict that `_vote_resolving` gives: the oid, the committed serial,
/// the serial that the transaction read, the committed pickle and the
/// pickle of the transaction.
type ConflictRow<'py> = (
    Bound<'py, PyBytes>,
    Bound<'py, PyBytes>,
    Bound<'py, PyBytes>,
    Bound<'py, PyBytes>,
    Bound<'py, PyBytes>,
);

/// A conflict that `_undo` gives: the oid, the serial of the current
/// state, the current pickle and the pickle of the state before the undone
/// transaction.
type UndoRow<'py> = (
    Bound<'py, PyBytes>,
    Bound<'py, PyBytes>,
    Bound<'py, PyBytes>,
    Bound<'py, PyBytes>,
);

/// A transaction of `_undo_log`: the tid, the user name, the description
/// and the pickled extension.
type UndoLogRow<'py> = (
    Bound<'py, PyBytes>,
    Bound<'py, PyBytes>,
    Bound<'py, PyBytes>,
    Bound<'py, PyBytes>,
);

/// A row of `_history`: the tid, the user name, the description and the
/// size of the pickle.
type HistoryRow<'py> = (
    Bound<'py, PyBytes>,
    Bound<'py, PyBytes>,
    Bound<'py, PyBytes>,
    u64,
);

/// A record that `record_iternext` gives: the oid, the serial, the pickle
/// and the next oid.
type CurrentRow<'py> = (
    Bound<'py, PyBytes>,
    Bound<'py, PyBytes>,
    Bound<'py, PyBytes>,
    Option<Bound<'py, PyBytes>>,
);

/// A record of a transaction of `_iterator`: the oid, the tid and the
/// pickle, `None` for a deletion.
type RecordRow<'py> = (
    Bound<'py, PyBytes>,
    Bound<'py, PyBytes>,
    Option<Bound<'py, PyBytes>>,
);

/// A transaction of `_iterator`: the tid, the user name, the description,
/// the pickled extension and the records.
type TxnRow<'py> = (
    Bound<'py, PyBytes>,
    Bound<'py, PyBytes>,
    Bound<'py, PyBytes>,
    Bound<'py, PyBytes>,
    Vec<RecordRow<'py>>,
);

/// Return the `StorageError` for a staged pickle that the core cannot read
/// back from the spill file. The error of the core is its cause, so its
/// kind and its message stay.
fn unreadable_pickle(py: Python<'_>, error: ZoeError) -> PyErr {
    let message = format!("a staged pickle cannot be read back: {error}");
    let cause = Failure::from(error).into_py(py);
    let raised = py
        .import("ZODB.POSException")
        .and_then(|module| module.getattr("StorageError"))
        .and_then(|class| class.call1((message,)))
        .map_or_else(|e| e, PyErr::from_value);
    raised.set_cause(py, Some(cause));
    raised
}

/// Return the failure of a vote of `txn` that ended with `error`. ZODB
/// reads the class name of the object in conflict from the pickle that the
/// transaction stored.
fn vote_failure(txn: &SharedTxn, error: ZoeError) -> Failure {
    let data = match error {
        // The pickle only gives the class name, so a failed read gives no
        // data.
        ZoeError::Conflict { oid } => txn
            .staged(oid)
            .ok()
            .flatten()
            .map(|(_, data)| data.to_vec()),
        _ => None,
    };
    Failure {
        error,
        serials: txn.conflict_serials(),
        data,
    }
}

/// Return `untransform` applied to the stored record `data`: the pickle of
/// the record. Return `None` when `untransform` fails.
fn untransformed(untransform: &Bound<'_, PyAny>, data: &[u8]) -> Option<Vec<u8>> {
    let pickle = untransform
        .call1((PyBytes::new(untransform.py(), data),))
        .ok()?;
    let pickle = pickle.cast::<PyBytes>().ok()?;
    Some(pickle.as_bytes().to_vec())
}

/// Return the exception of ZODB for a core error.
fn core(py: Python<'_>, error: ZoeError) -> PyErr {
    Failure::from(error).into_py(py)
}

/// Check the status that ZODB gives to a copied transaction: `" "` for a
/// normal one, `"p"` for one that a pack changed. Raise
/// `StorageTransactionError` for any other status.
fn check_status(status: &Bound<'_, PyAny>) -> PyResult<()> {
    let value = text(status)?;
    if value == b" " || value == b"p" {
        return Ok(());
    }
    Err(errors::status(status.py(), status))
}

/// Return the serial that a transaction read for an object: `None` is
/// `Tid::ZERO` and 8 bytes are a tid. A `str` of 8 characters below 256
/// gives its Latin-1 bytes, as FileStorage takes the serial `'\0' * 8` that
/// ZODB's blob tests pass.
fn serial_of(serial: &Bound<'_, PyAny>) -> PyResult<Tid> {
    if serial.is_none() {
        return Ok(Tid::ZERO);
    }
    let Ok(text) = serial.cast::<PyString>() else {
        return Ok(Tid(u64_of(serial)?));
    };
    let bytes: Option<Vec<u8>> = text
        .to_str()?
        .chars()
        .map(|c| u8::try_from(u32::from(c)).ok())
        .collect();
    let array: [u8; 8] = bytes
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(|| PyValueError::new_err("a serial has 8 bytes"))?;
    Ok(Tid(u64::from_be_bytes(array)))
}

/// Return the bytes of a metadata field of a transaction. A `str` gives
/// its UTF-8 bytes.
fn text(value: &Bound<'_, PyAny>) -> PyResult<Vec<u8>> {
    match value.cast::<PyString>() {
        Ok(string) => Ok(string.to_str()?.as_bytes().to_vec()),
        Err(_) => Ok(value.cast::<PyBytes>()?.as_bytes().to_vec()),
    }
}

/// Return the total size of the files in the directory `dir`, in bytes.
fn directory_size(dir: &Path) -> io::Result<u64> {
    let mut size = 0;
    for entry in fs::read_dir(dir)? {
        let metadata = entry?.metadata()?;
        if metadata.is_file() {
            size += metadata.len();
        }
    }
    Ok(size)
}
