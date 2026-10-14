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
//! The store and the table live in a `Shared` part, which the watcher of
//! the signals also holds (see `signals`). The store closes once: in
//! `close`, in the drop or in the watcher, whichever takes it first.

use std::collections::HashMap;
use std::collections::hash_map::Entry::{Occupied, Vacant};
use std::ffi::OsString;
use std::fs::{self, File};
use std::io;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock, TryLockError};
use std::thread;
use std::time::Duration;

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyString};
use zoe_core::{BlobFile, Oid, Options, Store, Tid, Txn, ZoeError};

use crate::errors::{self, Failure, p64, u64_of};
use crate::publish::{Publication, Published};
use crate::sequence::Sequence;
use crate::signals;

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

/// One open transaction.
struct Entry {
    /// The ZODB transaction object. The entry keeps it alive, so that its
    /// address stays its key.
    _transaction: Py<PyAny>,
    /// The transaction of the core. It is `None` after `tpc_finish` or
    /// `tpc_abort` took it.
    txn: Mutex<Option<SharedTxn>>,
}

impl Entry {
    fn lock(&self) -> MutexGuard<'_, Option<SharedTxn>> {
        self.txn.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Call `f` with the transaction of the core. Fail when `tpc_finish`
    /// or `tpc_abort` took it.
    fn with<T>(&self, f: impl FnOnce(&mut SharedTxn) -> Result<T, Failure>) -> Result<T, Failure> {
        match self.lock().as_mut() {
            Some(txn) => f(txn),
            None => Err(ZoeError::Protocol("the transaction is finished or aborted").into()),
        }
    }

    /// Call `f` with the transaction of the core, as `with` does, if no
    /// other call uses it now. Return `None` when another call uses it.
    fn try_with<T>(
        &self,
        f: impl FnOnce(&mut SharedTxn) -> Result<T, Failure>,
    ) -> Option<Result<T, Failure>> {
        let mut txn = match self.txn.try_lock() {
            Ok(txn) => txn,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => return None,
        };
        Some(match txn.as_mut() {
            Some(txn) => f(txn),
            None => Err(ZoeError::Protocol("the transaction is finished or aborted").into()),
        })
    }

    /// Take the transaction of the core out of the entry.
    fn take(&self) -> Option<SharedTxn> {
        self.lock().take()
    }

    /// Take the transaction of the core out of the entry, if no call uses
    /// it now. Fail when a call uses it.
    fn try_take(&self) -> Result<Option<SharedTxn>, ()> {
        match self.txn.try_lock() {
            Ok(mut txn) => Ok(txn.take()),
            Err(TryLockError::Poisoned(poisoned)) => Ok(poisoned.into_inner().take()),
            Err(TryLockError::WouldBlock) => Err(()),
        }
    }
}

/// The open transactions of a storage.
#[derive(Default)]
struct Table {
    /// True after `close` or the close at a signal. The table then refuses
    /// a new transaction.
    closed: bool,
    /// The open transactions, keyed by the address of the ZODB
    /// transaction object.
    entries: HashMap<usize, Arc<Entry>>,
    /// The entries that the close at a signal took out of `entries`. They
    /// hold Python references, so they stay here until `close` or the drop
    /// of the storage removes them.
    closing: Vec<Arc<Entry>>,
}

impl Table {
    /// Remove every entry and return them.
    fn drain(&mut self) -> Vec<Arc<Entry>> {
        let mut entries: Vec<Arc<Entry>> = self.entries.drain().map(|(_, entry)| entry).collect();
        entries.append(&mut self.closing);
        entries
    }
}

/// The parts of a storage that the watcher of the signals shares with it.
///
/// The watcher runs no Python code, so it drops no Python reference. The
/// entries of the table hold Python references: the watcher takes the
/// transaction of the core out of an entry and leaves the entry in the
/// table. `close` and the drop of the storage remove the entries.
pub(crate) struct Shared {
    /// The store. It is `None` after `close`, the drop or the close at a
    /// signal.
    store: RwLock<Option<StoreRef>>,
    table: Mutex<Table>,
    /// True when the last share of the store dropped. A read through a
    /// weak handle of the core can still hold the database then.
    database_closed: Arc<AtomicBool>,
}

impl Shared {
    fn new(store: Store) -> Self {
        let database_closed = Arc::new(AtomicBool::new(false));
        let store = StoreRef(Arc::new(Owned {
            store,
            _closed: SetOnDrop(Arc::clone(&database_closed)),
        }));
        Self {
            store: RwLock::new(Some(store)),
            table: Mutex::new(Table::default()),
            database_closed,
        }
    }

    fn table(&self) -> MutexGuard<'_, Table> {
        self.table.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Take the store out of the storage. Return `None` when it is already
    /// out.
    fn take_store(&self) -> Option<StoreRef> {
        self.store
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }

    /// Mark the storage closed and take its store, as `close` does. Return
    /// `None` when `close` or the drop took the store first.
    ///
    /// The open transactions leave `entries`, so a later call with one of
    /// them raises the error that it raises after `close`.
    pub(crate) fn mark_closed(&self) -> Option<StoreRef> {
        {
            let mut table = self.table();
            table.closed = true;
            let entries: Vec<Arc<Entry>> = table.entries.drain().map(|(_, entry)| entry).collect();
            table.closing.extend(entries);
        }
        self.take_store()
    }

    /// Abort each transaction that the close at a signal took out of
    /// `entries` and that no call uses now. Leave the entries in the table.
    pub(crate) fn abort_idle(&self) {
        // The transactions drop after the guard of the table, because an
        // abort can take time.
        let idle: Vec<SharedTxn> = self
            .table()
            .closing
            .iter()
            .filter_map(|entry| entry.try_take().ok().flatten())
            .collect();
        drop(idle);
    }

    /// Tell if the last share of the store dropped.
    pub(crate) fn database_closed(&self) -> bool {
        self.database_closed.load(Ordering::Acquire)
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
}

impl Storage {
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

    /// Return the next value of the sequence `name`. A read-only storage
    /// refuses the call.
    pub(crate) fn next_value(&self, py: Python<'_>, name: &str) -> PyResult<u64> {
        if self.read_only {
            return Err(errors::read_only(py));
        }
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
    /// `<path>/blobs`. The core takes it as `Options::blob_dir`.
    ///
    /// The open installs the handlers of SIGTERM and SIGHUP that close the
    /// stores of the process, each only when its action is the default
    /// (see `signals`).
    #[new]
    #[pyo3(signature = (path, *, read_only = false, lock_timeout = None, blob_dir = None))]
    fn new(
        py: Python<'_>,
        path: PathBuf,
        read_only: bool,
        lock_timeout: Option<f64>,
        blob_dir: Option<PathBuf>,
    ) -> PyResult<Self> {
        let mut options = Options::default();
        if let Some(seconds) = lock_timeout {
            options.lock_timeout = Duration::try_from_secs_f64(seconds).map_err(|_| {
                PyValueError::new_err("lock_timeout is a number of seconds, 0 or more")
            })?;
        }
        options.blob_dir = blob_dir;
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
        Ok(Self {
            shared,
            path: path.to_string_lossy().into_owned(),
            read_only,
            published: Published::new(last),
        })
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

    /// Return 0. IStorage asks only for an approximate length, and the
    /// store keeps no object count.
    fn __len__(&self) -> usize {
        0
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
        if self.read_only {
            return Err(errors::read_only(py));
        }
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
    /// The whole pack runs without the GIL. Other threads read and commit
    /// while it runs. A read-only storage refuses the pack.
    #[pyo3(name = "_pack")]
    fn pack(&self, py: Python<'_>, tid: &Bound<'_, PyAny>) -> PyResult<()> {
        if self.read_only {
            return Err(errors::read_only(py));
        }
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
    #[pyo3(signature = (transaction, tid = None, status = None))]
    fn tpc_begin(
        &self,
        py: Python<'_>,
        transaction: &Bound<'_, PyAny>,
        tid: Option<&Bound<'_, PyAny>>,
        status: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<()> {
        if self.read_only {
            return Err(errors::read_only(py));
        }
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
        let txn = py
            .detach(move || match tid {
                None => Ok(Txn::begin(store, &user, &description, &extension)),
                Some(tid) => Txn::begin_restore(store, tid, &user, &description, &extension),
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
        if self.read_only {
            return Err(errors::read_only(py));
        }
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
        if self.read_only {
            return Err(errors::read_only(py));
        }
        let entry = self.entry(py, transaction)?;
        let oid = Oid(u64_of(oid)?);
        let serial = serial_of(oldserial)?;
        py.detach(|| entry.with(|txn| Ok(txn.delete(oid, serial)?)))
            .map_err(|failure| failure.into_py(py))
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
        if self.read_only {
            return Err(errors::read_only(py));
        }
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
        if self.read_only {
            return Err(errors::read_only(py));
        }
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
        if self.read_only {
            return Err(errors::read_only(py));
        }
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
        if self.read_only {
            return Err(errors::read_only(py));
        }
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
        py.detach(|| entry.with(|txn| txn.vote().map_err(|error| vote_failure(txn, error))))
            .map_err(|failure| failure.into_py(py))
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
        let conflicts = py
            .detach(|| {
                let read = entry.with(|txn| {
                    let conflicts = txn
                        .vote_resolving()
                        .map_err(|error| vote_failure(txn, error))?;
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
            })?
            .map_err(|error| unreadable_pickle(py, error))?;
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
        let entries: Vec<Arc<Entry>> = {
            let mut table = self.table();
            table.closed = true;
            table.drain()
        };
        let store = self.shared.take_store();
        let entries = py.detach(move || {
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
        let store = self.shared.take_store();
        let entries: Vec<Arc<Entry>> = self.shared.table().drain();
        let txns: Vec<SharedTxn> = entries.iter().filter_map(|entry| entry.take()).collect();
        let close = move || drop((txns, store));
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
        let next = py.detach(|| {
            self.transactions
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .next()
        });
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

/// Abort the transactions of `entries`.
///
/// A vote that waits for a lock uses its entry until the wait ends. So the
/// transactions that no call uses go first: their abort releases their
/// locks, and a vote that waits for one of them ends. Then the others go,
/// as soon as their calls end.
fn abort_all(entries: &[Arc<Entry>]) {
    let mut busy: Vec<&Arc<Entry>> = entries.iter().collect();
    loop {
        busy.retain(|entry| match entry.try_take() {
            Ok(txn) => {
                drop(txn);
                false
            }
            Err(()) => true,
        });
        if busy.is_empty() {
            return;
        }
        thread::sleep(Duration::from_millis(1));
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

/// Stage the blob in the file `path` in the transaction of `entry`.
///
/// A file in the blob temporary directory of `store`, or in one of its
/// subdirectories, goes to `at`, which takes it by rename. A file of the
/// store directory `store_dir` or of the blob directory outside that
/// temporary directory, the database or a committed blob file for example,
/// is refused with `Protocol`: the store owns it. Another file is copied
/// into a new blob file of `store`, which goes to `copied`, and then it is
/// removed. A refused call leaves the file in its place.
fn take_blob(
    store: &Store,
    store_dir: &Path,
    entry: &Entry,
    path: &Path,
    at: impl FnOnce(&mut SharedTxn, &Path) -> zoe_core::Result<()>,
    copied: impl FnOnce(&mut SharedTxn, BlobFile) -> zoe_core::Result<()>,
) -> Result<(), Failure> {
    match blob_source(store, store_dir, path)? {
        Source::Temporary(temporary) => entry.with(|txn| Ok(at(txn, &temporary)?)),
        Source::Store => {
            Err(ZoeError::Protocol("the blob file is a file of the store, not a new blob").into())
        }
        Source::Elsewhere => {
            let mut file = store.new_blob_file()?;
            io::copy(&mut File::open(path)?, &mut file)?;
            entry.with(|txn| Ok(copied(txn, file)?))?;
            fs::remove_file(path)?;
            Ok(())
        }
    }
}

/// Where a blob file that a client gives is.
enum Source {
    /// In the blob temporary directory or in one of its subdirectories,
    /// with the path that the core gives to the file.
    Temporary(PathBuf),
    /// In the store directory or in the blob directory, outside the blob
    /// temporary directory.
    Store,
    /// Elsewhere.
    Elsewhere,
}

/// Tell where the file `path` is. `store_dir` is the real path of the
/// store directory.
///
/// The directories compare by their real paths, so a path through a
/// symbolic link or a relative path is found too. The name of the file
/// stays as it is: the core refuses a symbolic link.
fn blob_source(store: &Store, store_dir: &Path, path: &Path) -> Result<Source, Failure> {
    let temp = store.blob_temp_dir()?;
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return Ok(Source::Elsewhere);
    };
    let dir = if dir.as_os_str().is_empty() {
        Path::new(".")
    } else {
        dir
    };
    let (Ok(dir), Ok(real_temp)) = (fs::canonicalize(dir), fs::canonicalize(&temp)) else {
        return Ok(Source::Elsewhere);
    };
    if let Ok(rest) = dir.strip_prefix(&real_temp) {
        return Ok(Source::Temporary(temp.join(rest).join(name)));
    }
    let in_blob_dir = real_temp
        .parent()
        .is_some_and(|blobs| dir.starts_with(blobs));
    if in_blob_dir || dir.starts_with(store_dir) {
        return Ok(Source::Store);
    }
    Ok(Source::Elsewhere)
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
