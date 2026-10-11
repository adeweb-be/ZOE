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
//! gives the path of a committed blob file. A `blob_dir` other than
//! `<path>/blobs` becomes the symbolic link `<path>/blobs` to it.
//!
//! `_vote_resolving` votes and keeps the locks when it finds conflicts.
//! `zoe.storage.Storage.tpc_vote` resolves them in Python, stores the
//! resolved pickles and calls `tpc_vote` of this class. `tpc_abort` and
//! `close` can release the locks while the resolution runs, because the
//! resolution holds no mutex of the transaction.

use std::collections::HashMap;
use std::collections::hash_map::Entry::{Occupied, Vacant};
use std::ffi::OsString;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock, TryLockError};
use std::thread;
use std::time::Duration;

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyString};
use zoe_core::{BlobFile, Oid, Options, Store, Tid, Txn, ZoeError};

use crate::errors::{self, Failure, p64, u64_of};
use crate::publish::{Publication, Published};

/// A transaction of the core that shares its store.
type SharedTxn = Txn<Arc<Store>>;

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
    /// True after `close`. The table then refuses a new transaction.
    closed: bool,
    /// The open transactions, keyed by the address of the ZODB
    /// transaction object.
    entries: HashMap<usize, Arc<Entry>>,
}

/// The in-process storage. It implements ZODB's `IStorage`.
#[pyclass(subclass, frozen, module = "zoe._zoe", name = "Storage")]
pub(crate) struct Storage {
    /// The store. It is `None` after `close`.
    store: RwLock<Option<Arc<Store>>>,
    /// The absolute path of the store directory.
    path: String,
    read_only: bool,
    table: Mutex<Table>,
    published: Published,
}

impl Storage {
    /// Return the store, or fail when the storage is closed.
    fn opened(&self, py: Python<'_>) -> PyResult<Arc<Store>> {
        // The guard drops at the end of this statement. Building the
        // exception runs Python code, so it must come after.
        let store = self
            .store
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        store.ok_or_else(|| errors::closed(py))
    }

    fn table(&self) -> MutexGuard<'_, Table> {
        self.table.lock().unwrap_or_else(PoisonError::into_inner)
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
}

#[pymethods]
impl Storage {
    /// Open the store in the directory `path`, and create it if necessary.
    /// `lock_timeout` is the longest wait of a vote for its locks, in
    /// seconds. `blob_dir` is the directory of the blob files: `None` keeps
    /// the blob directory of the store, `<path>/blobs` for a new store.
    /// `link_blob_dir` tells what another directory means.
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
        let (store, last, path) = py
            .detach(|| {
                // A read-only storage creates no directory. It opens the
                // store for writes all the same, because the core has no
                // read-only open yet. When the core gets one, a read-only
                // storage uses it.
                if read_only && !path.is_dir() {
                    let missing = format!("no store directory: {}", path.display());
                    return Err(io::Error::new(io::ErrorKind::NotFound, missing).into());
                }
                // The blob paths that the storage gives do not depend on the
                // current directory.
                let path = std::path::absolute(&path)?;
                let store = Store::open_with(&path, options)?;
                if let Some(blob_dir) = &blob_dir {
                    link_blob_dir(&path, blob_dir, read_only)?;
                }
                let last = store.last_transaction();
                let path = fs::canonicalize(&path)?;
                Ok::<_, Opening>((store, last, path))
            })
            .map_err(|error| error.into_py(py))?;
        Ok(Self {
            store: RwLock::new(Some(Arc::new(store))),
            path: path.to_string_lossy().into_owned(),
            read_only,
            table: Mutex::new(Table::default()),
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

    /// Begin a two-phase commit of `transaction`. The calling thread
    /// drives it.
    ///
    /// With `tid`, the transaction is a restore transaction: a copy of a
    /// transaction of another storage, which commits under `tid`. It takes
    /// `restoreBlob`. `tid` must be after the last tid of the store.
    #[pyo3(signature = (transaction, tid = None))]
    fn tpc_begin(
        &self,
        py: Python<'_>,
        transaction: &Bound<'_, PyAny>,
        tid: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<()> {
        if self.read_only {
            return Err(errors::read_only(py));
        }
        let key = transaction.as_ptr() as usize;
        let begun = self.table().entries.contains_key(&key);
        if begun {
            return Err(errors::duplicate_transaction(py));
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
        let serial = if serial.is_none() {
            Tid::ZERO
        } else {
            Tid(u64_of(serial)?)
        };
        py.detach(|| entry.with(|txn| Ok(txn.store(oid, serial, data)?)))
            .map_err(|failure| failure.into_py(py))
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

    /// Stage a new pickle of `oid` with the blob in the file `blobfilename`.
    /// `serial` is the serial that the transaction read, or `None` for a
    /// new object.
    ///
    /// The storage takes the file. A file of `temporaryDirectory()`, or of
    /// one of its subdirectories, moves into the store by rename, never by
    /// copy. A file from elsewhere is copied, then removed. A refused call
    /// leaves the file in its place. An abort removes a file that the
    /// transaction took.
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
        let serial = if serial.is_none() {
            Tid::ZERO
        } else {
            Tid(u64_of(serial)?)
        };
        // The entry holds a Python reference, so it drops while the thread
        // is attached. The store drops while it is detached.
        py.detach(|| {
            let store = store;
            take_blob(
                &store,
                &entry,
                &blobfilename,
                |txn, path| txn.store_blob_at(oid, serial, data, path),
                |txn, file| txn.store_blob(oid, serial, data, file),
            )
        })
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
    /// transaction wrote no blob for `oid`.
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
            .detach(move || store.blob_path(oid, serial))
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
            table.entries.drain().map(|(_, entry)| entry).collect()
        };
        let store = self
            .store
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
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
        let store = self
            .store
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let txns: Vec<SharedTxn> = self
            .table
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .entries
            .values()
            .filter_map(|entry| entry.take())
            .collect();
        let close = move || drop((txns, store));
        Python::try_attach(|py| py.detach(close));
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
/// subdirectories, goes to `at`, which takes it by rename. Another file is
/// copied into a new blob file of `store`, which goes to `copied`. Then the
/// file is removed, unless it is a file of the blob directory: the store
/// keeps its committed blob files. A refused call leaves the file in its
/// place.
fn take_blob(
    store: &Store,
    entry: &Entry,
    path: &Path,
    at: impl FnOnce(&mut SharedTxn, &Path) -> zoe_core::Result<()>,
    copied: impl FnOnce(&mut SharedTxn, BlobFile) -> zoe_core::Result<()>,
) -> Result<(), Failure> {
    let source = blob_source(store, path)?;
    if let Source::Temporary(temporary) = source {
        return entry.with(|txn| Ok(at(txn, &temporary)?));
    }
    let mut file = store.new_blob_file()?;
    io::copy(&mut File::open(path)?, &mut file)?;
    entry.with(|txn| Ok(copied(txn, file)?))?;
    if let Source::Elsewhere = source {
        fs::remove_file(path)?;
    }
    Ok(())
}

/// Where a blob file that a client gives is.
enum Source {
    /// In the blob temporary directory or in one of its subdirectories,
    /// with the path that the core gives to the file.
    Temporary(PathBuf),
    /// In the blob directory, outside its temporary directory: a committed
    /// blob file, for example.
    BlobDir,
    /// Elsewhere.
    Elsewhere,
}

/// Tell where the file `path` is.
///
/// The directories compare by their real paths, so a path through a
/// symbolic link or a relative path is found too. The name of the file
/// stays as it is: the core refuses a symbolic link.
fn blob_source(store: &Store, path: &Path) -> Result<Source, Failure> {
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
    match real_temp.parent() {
        Some(blobs) if dir.starts_with(blobs) => Ok(Source::BlobDir),
        _ => Ok(Source::Elsewhere),
    }
}

/// An error of `Storage.__new__`.
enum Opening {
    /// The core failed.
    Core(ZoeError),
    /// The store refuses `blob_dir`.
    BlobDir(String),
}

impl From<ZoeError> for Opening {
    fn from(error: ZoeError) -> Self {
        Self::Core(error)
    }
}

impl From<io::Error> for Opening {
    fn from(error: io::Error) -> Self {
        Self::Core(error.into())
    }
}

impl Opening {
    fn into_py(self, py: Python<'_>) -> PyErr {
        match self {
            Self::Core(error) => core(py, error),
            Self::BlobDir(message) => errors::blob_dir(py, message),
        }
    }
}

/// The blob directory in the store directory. The core keeps the blob
/// files there.
const BLOBS: &str = "blobs";

/// Make `blob_dir` the blob directory of the store in `dir`.
///
/// The core keeps the blob files in `<dir>/blobs`. Another `blob_dir`
/// becomes the symbolic link `<dir>/blobs` to it. So the blob files and
/// their temporary directory live in `blob_dir`, on its device, and a
/// commit renames each file there. The link stays: a later open without
/// `blob_dir` finds the blobs.
///
/// A store that has a blob directory refuses another `blob_dir`. A new link
/// needs a missing or empty `blob_dir`, so two stores never share a blob
/// directory. `blob_dir` is created and synced before the link, and the
/// store directory is synced after it. A read-only storage creates nothing.
fn link_blob_dir(dir: &Path, blob_dir: &Path, read_only: bool) -> Result<(), Opening> {
    let link = dir.join(BLOBS);
    let blob_dir = resolved(blob_dir)?;
    if blob_dir == resolved(&link)? {
        return Ok(());
    }
    match fs::symlink_metadata(&link) {
        Ok(_) => {
            let current = fs::canonicalize(&link).unwrap_or(link);
            if fs::canonicalize(&blob_dir).is_ok_and(|wanted| wanted == current) {
                return Ok(());
            }
            Err(Opening::BlobDir(format!(
                "blob_dir {}: the store keeps its blobs in {}",
                blob_dir.display(),
                current.display()
            )))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if read_only {
                return Ok(());
            }
            create_dir_durable(&blob_dir)?;
            if fs::read_dir(&blob_dir)?.next().is_some() {
                return Err(Opening::BlobDir(format!(
                    "blob_dir {} is not empty, and the store has no blob directory yet",
                    blob_dir.display()
                )));
            }
            std::os::unix::fs::symlink(&blob_dir, &link)?;
            File::open(dir)?.sync_all()?;
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

/// Return the absolute form of `path`, with the symbolic links of its
/// parent resolved. The last part stays as it is, so the path of a missing
/// directory or of a link resolves too.
fn resolved(path: &Path) -> io::Result<PathBuf> {
    let path = std::path::absolute(path)?;
    if let (Some(parent), Some(name)) = (path.parent(), path.file_name())
        && let Ok(parent) = fs::canonicalize(parent)
    {
        return Ok(parent.join(name));
    }
    Ok(path)
}

/// Create the directory `dir` and its missing parents, one level at a
/// time. Sync the parent of each new directory, and the parent of `dir`
/// also when `dir` exists: a process that stopped before its sync can
/// leave a directory that is not durable.
fn create_dir_durable(dir: &Path) -> io::Result<()> {
    let missing: Vec<&Path> = dir
        .ancestors()
        .take_while(|path| !path.as_os_str().is_empty() && !path.is_dir())
        .collect();
    for path in missing.into_iter().rev() {
        match fs::create_dir(path) {
            Ok(()) => {}
            // Another process can create the directory first.
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists && path.is_dir() => {}
            Err(e) => return Err(e),
        }
        sync_parent(path)?;
    }
    sync_parent(dir)
}

/// Sync the directory that holds `path`.
fn sync_parent(path: &Path) -> io::Result<()> {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => File::open(parent)?.sync_all(),
        _ => Ok(()),
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
