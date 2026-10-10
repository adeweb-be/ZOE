//! A ZOE store: the entry point of the core.

use std::any::Any;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use crate::backend::Backend;
use crate::clock::{Clock, SystemClock};
use crate::commit::Txn;
use crate::error::{Result, ZoeError};
use crate::lock::LockManager;
use crate::redb_backend::RedbBackend;
use crate::types::{Oid, Tid};
use crate::vfs::{MemVfs, OsVfs, Vfs};

/// The name of the database file in the store directory.
const DATABASE: &str = "zoe.redb";

/// The name under which a new database is written before it is renamed.
const NEW_DATABASE: &str = "zoe.redb.new";

/// The options of a store.
///
/// Build it from the defaults, for example
/// `Options { lock_timeout, ..Options::default() }`. Then a new option does
/// not break the caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Options {
    /// The longest time that a vote waits for its locks. A longer wait
    /// fails the vote with `Conflict`. This is `lock-timeout` in the
    /// configuration.
    pub lock_timeout: Duration,
    /// Not an option. This field makes `..Options::default()` necessary
    /// when a caller sets every option.
    #[doc(hidden)]
    pub _more: (),
}

impl Default for Options {
    fn default() -> Self {
        Self {
            lock_timeout: Duration::from_secs(10),
            _more: (),
        }
    }
}

/// An open store.
pub struct Store {
    pub(crate) backend: Box<dyn Backend>,
    pub(crate) clock: Box<dyn Clock>,
    pub(crate) locks: LockManager,
    pub(crate) lock_timeout: Duration,
    /// The tid of the last commit. Its lock is the commit sequencer. It is
    /// `None` after a failed finish, because the outcome of that commit is
    /// unknown.
    sequencer: Mutex<Option<Tid>>,
    next_oid: AtomicU64,
    /// The directory. It holds the single-writer lock while the store is
    /// open.
    _vfs: Box<dyn Any + Send + Sync>,
}

impl Store {
    /// Open the store in the directory `dir` with the default options, and
    /// create it if necessary.
    pub fn open(dir: impl AsRef<Path>) -> Result<Self> {
        Self::open_with(dir, Options::default())
    }

    /// Open the store in the directory `dir` with `options`, and create it
    /// if necessary.
    pub fn open_with(dir: impl AsRef<Path>, options: Options) -> Result<Self> {
        Self::open_full(OsVfs::new(dir.as_ref())?, SystemClock, options)
    }

    /// Open the store in the simulated filesystem `vfs`, and create it if
    /// necessary.
    pub fn open_vfs(vfs: MemVfs) -> Result<Self> {
        Self::open_in(vfs, SystemClock)
    }

    /// Open the store in `vfs` with the clock `clock` and the default
    /// options, and create it if necessary.
    pub fn open_in<V: Vfs>(vfs: V, clock: impl Clock + 'static) -> Result<Self> {
        Self::open_full(vfs, clock, Options::default())
    }

    /// Open the store in `vfs` with the clock `clock` and `options`, and
    /// create it if necessary.
    ///
    /// A new store is written under a temporary name and renamed into place
    /// after it is durable, so a crash during the creation leaves either
    /// no store or a complete empty one.
    fn open_full<V: Vfs>(vfs: V, clock: impl Clock + 'static, options: Options) -> Result<Self> {
        if !vfs.exists(DATABASE)? {
            if vfs.exists(NEW_DATABASE)? {
                vfs.remove(NEW_DATABASE)?;
            }
            let db = redb::Builder::new()
                .create_with_backend(vfs.open(NEW_DATABASE)?)
                .map_err(|e| ZoeError::Backend(Box::new(e)))?;
            RedbBackend::initialise(&db)?;
            drop(db);
            vfs.rename(NEW_DATABASE, DATABASE)?;
            vfs.sync_dir()?;
        }

        let db = redb::Builder::new()
            .create_with_backend(vfs.open(DATABASE)?)
            .map_err(|e| ZoeError::Backend(Box::new(e)))?;
        let backend = RedbBackend::open(db)?;
        let next_oid = backend.next_oid()?;
        let last_tid = backend.last_tid();
        Ok(Self {
            backend: Box::new(backend),
            clock: Box::new(clock),
            locks: LockManager::default(),
            lock_timeout: options.lock_timeout,
            sequencer: Mutex::new(Some(last_tid)),
            next_oid: AtomicU64::new(next_oid.0),
            _vfs: Box::new(vfs),
        })
    }

    /// Return a new oid. An oid that no commit uses can come back after
    /// the store reopens.
    pub fn new_oid(&self) -> Result<Oid> {
        Ok(Oid(self.next_oid.fetch_add(1, Ordering::Relaxed)))
    }

    /// Begin a transaction.
    pub fn tpc_begin(&self, user: &[u8], description: &[u8], extension: &[u8]) -> Result<Txn<'_>> {
        Ok(Txn::new(self, user, description, extension))
    }

    /// Return the current pickle of `oid` and its serial.
    pub fn load(&self, oid: Oid) -> Result<(Vec<u8>, Tid)> {
        match self.backend.load_before(oid, Tid::MAX)? {
            Some(record) if !record.data.is_empty() => Ok((record.data, record.tid)),
            _ => Err(ZoeError::NotFound { oid }),
        }
    }

    /// Return the tid of the last committed transaction.
    pub fn last_transaction(&self) -> Tid {
        self.backend.last_tid()
    }

    pub(crate) fn lock_sequencer(&self) -> MutexGuard<'_, Option<Tid>> {
        self.sequencer.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::backend::{Bytes, PackStats, Record, RecordMeta, TxnIter, WriteTxn};

    /// A clock that never moves.
    struct FrozenClock;

    impl Clock for FrozenClock {
        fn now(&self) -> SystemTime {
            UNIX_EPOCH + Duration::from_secs(1_000_000_000)
        }
    }

    /// The faults that a test turns on and off.
    #[derive(Default)]
    struct Faults {
        /// `load_before` fails.
        load: AtomicBool,
        /// `begin_write` fails.
        write: AtomicBool,
    }

    /// A backend that fails when a test turns a fault on.
    struct Faulty {
        inner: Box<dyn Backend>,
        faults: Arc<Faults>,
    }

    fn injected() -> ZoeError {
        ZoeError::Backend("injected fault".into())
    }

    impl Backend for Faulty {
        fn load_before(&self, oid: Oid, tid: Tid) -> Result<Option<Record>> {
            if self.faults.load.load(Ordering::Relaxed) {
                return Err(injected());
            }
            self.inner.load_before(oid, tid)
        }

        fn load_serial(&self, oid: Oid, tid: Tid) -> Result<Option<Bytes>> {
            self.inner.load_serial(oid, tid)
        }

        fn last_tid(&self) -> Tid {
            self.inner.last_tid()
        }

        fn begin_write(&self) -> Result<Box<dyn WriteTxn + '_>> {
            if self.faults.write.load(Ordering::Relaxed) {
                return Err(injected());
            }
            self.inner.begin_write()
        }

        fn iter_txns(&self, from: Tid, to: Tid) -> TxnIter<'_> {
            self.inner.iter_txns(from, to)
        }

        fn history(&self, oid: Oid, limit: usize) -> Result<Vec<RecordMeta>> {
            self.inner.history(oid, limit)
        }

        fn pack(&self, before: Tid, reachable: &dyn Fn(Oid, Tid) -> bool) -> Result<PackStats> {
            self.inner.pack(before, reachable)
        }

        fn sync(&self) -> Result<()> {
            self.inner.sync()
        }
    }

    /// Open a store whose backend fails when a test turns a fault on.
    fn faulty_store() -> (Store, Arc<Faults>) {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let faults = Arc::new(Faults::default());
        let backend = Box::new(Faulty {
            inner: store.backend,
            faults: Arc::clone(&faults),
        });
        (Store { backend, ..store }, faults)
    }

    fn commit(store: &Store, oid: Oid, serial: Tid, data: &[u8]) -> Result<Tid> {
        let mut txn = store.tpc_begin(b"", b"", b"")?;
        txn.store(oid, serial, data)?;
        txn.vote()?;
        txn.finish()
    }

    #[test]
    fn tids_increase_when_the_clock_stands_still() {
        let store = Store::open_in(MemVfs::new(), FrozenClock).unwrap();
        let oid = store.new_oid().unwrap();
        let first = commit(&store, oid, Tid::ZERO, b"a").unwrap();
        let second = commit(&store, oid, first, b"b").unwrap();
        assert_eq!(second, Tid(first.0 + 1));
    }

    #[test]
    fn a_stale_serial_is_a_conflict() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let oid = store.new_oid().unwrap();
        commit(&store, oid, Tid::ZERO, b"a").unwrap();
        let error = commit(&store, oid, Tid::ZERO, b"b");
        assert!(matches!(error, Err(ZoeError::Conflict { .. })));
        assert!(store.locks.is_empty());
    }

    #[test]
    fn a_backend_error_in_vote_releases_the_locks() {
        let (mut store, faults) = faulty_store();
        // A zero lock timeout turns every wait into a conflict.
        store.lock_timeout = Duration::ZERO;
        let oid = store.new_oid().unwrap();
        let mut failed = store.tpc_begin(b"", b"", b"").unwrap();
        failed.store(oid, Tid::ZERO, b"a").unwrap();
        faults.load.store(true, Ordering::Relaxed);
        assert!(matches!(failed.vote(), Err(ZoeError::Backend(_))));
        assert!(store.locks.is_empty());
        faults.load.store(false, Ordering::Relaxed);

        // Another transaction locks the same oid at once.
        let mut other = store.tpc_begin(b"", b"", b"").unwrap();
        other.store(oid, Tid::ZERO, b"b").unwrap();
        other.vote().unwrap();
        other.abort();

        // A second vote does not wait on a lock of the first one.
        failed.vote().unwrap();
        failed.finish().unwrap();
    }

    #[test]
    fn a_failed_finish_halts_the_store() {
        let (store, faults) = faulty_store();
        let oid = store.new_oid().unwrap();
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.store(oid, Tid::ZERO, b"a").unwrap();
        txn.vote().unwrap();
        faults.write.store(true, Ordering::Relaxed);
        assert!(matches!(txn.finish(), Err(ZoeError::Backend(_))));
        assert!(store.locks.is_empty());

        faults.write.store(false, Ordering::Relaxed);
        let error = commit(&store, oid, Tid::ZERO, b"b");
        assert!(matches!(error, Err(ZoeError::Halted)), "{error:?}");
        assert!(store.locks.is_empty());
        assert!(matches!(store.load(oid), Err(ZoeError::NotFound { .. })));
    }

    #[test]
    fn an_aborted_transaction_leaves_nothing() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let oid = store.new_oid().unwrap();
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.store(oid, Tid::ZERO, b"a").unwrap();
        txn.vote().unwrap();
        txn.abort();
        assert!(matches!(store.load(oid), Err(ZoeError::NotFound { .. })));
        assert!(store.locks.is_empty());
        assert_eq!(store.last_transaction(), Tid::ZERO);
    }

    #[test]
    fn oids_continue_after_a_reopen() {
        let vfs = MemVfs::new();
        let last = {
            let store = Store::open_vfs(vfs.clone()).unwrap();
            let oid = store.new_oid().unwrap();
            commit(&store, oid, Tid::ZERO, b"a").unwrap();
            oid
        };
        let store = Store::open_vfs(vfs).unwrap();
        assert!(store.new_oid().unwrap() > last);
    }

    #[test]
    fn the_transaction_row_is_in_the_log() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let oid = store.new_oid().unwrap();
        let mut txn = store.tpc_begin(b"user", b"description", b"").unwrap();
        txn.store(oid, Tid::ZERO, b"a").unwrap();
        txn.vote().unwrap();
        let tid = txn.finish().unwrap();

        let rows: Vec<_> = store.backend.iter_txns(Tid::ZERO, Tid::MAX).collect();
        assert_eq!(rows.len(), 1);
        let row = rows[0].as_ref().unwrap();
        assert_eq!((row.tid, row.oid_count), (tid, 1));
        assert_eq!(row.user, b"user");
        assert_eq!(store.backend.history(oid, 10).unwrap().len(), 1);
    }

    #[test]
    fn a_second_process_cannot_open_a_locked_store() {
        let dir = std::env::temp_dir().join(format!("zoe-locked-{}", std::process::id()));
        let _store = Store::open(&dir).unwrap();
        assert!(matches!(Store::open(&dir), Err(ZoeError::Locked)));
    }
}
