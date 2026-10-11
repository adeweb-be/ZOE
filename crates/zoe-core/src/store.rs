//! A ZOE store: the entry point of the core.

use std::any::Any;
use std::ops::Deref;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use crate::backend::{Backend, RecordEncoding};
use crate::clock::{Clock, SystemClock};
use crate::commit::Txn;
use crate::error::{Result, ZoeError};
use crate::format::Compression;
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
    /// How the store writes new records. The default is zstd at level 3.
    /// The store reads records of every codec, whatever this option.
    pub compression: Compression,
    /// Not an option. This field makes `..Options::default()` necessary
    /// when a caller sets every option.
    #[doc(hidden)]
    pub _more: (),
}

impl Default for Options {
    fn default() -> Self {
        Self {
            lock_timeout: Duration::from_secs(10),
            compression: Compression::default(),
            _more: (),
        }
    }
}

/// The open database of a store and the directory that holds its lock.
///
/// The fields drop in the order of their declaration. So the database
/// closes, and writes its last bytes, before the directory releases the
/// lock.
pub(crate) struct Opened {
    backend: Box<dyn Backend>,
    _dir: Box<dyn Any + Send + Sync>,
}

impl Deref for Opened {
    type Target = dyn Backend;

    fn deref(&self) -> &Self::Target {
        &*self.backend
    }
}

/// An open store.
pub struct Store {
    /// The database and the directory. A transaction iterator holds only a
    /// weak reference, so dropping the `Store` closes the store.
    pub(crate) backend: Arc<Opened>,
    pub(crate) clock: Box<dyn Clock>,
    pub(crate) locks: LockManager,
    pub(crate) lock_timeout: Duration,
    /// The tid of the last commit. Its lock is the commit sequencer. It is
    /// `None` after a failed finish, because the outcome of that commit is
    /// unknown.
    sequencer: Mutex<Option<Tid>>,
    next_oid: AtomicU64,
    /// True when the store is open for reads only.
    read_only: bool,
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
        let backend = RedbBackend::open(db)?.with_compression(options.compression);
        Self::assemble(backend, Box::new(vfs), Box::new(clock), options, false)
    }

    /// Open the existing store in the directory `dir` for reads only.
    ///
    /// The call takes the lock of the store, so a writer that opens the
    /// store later gets `Locked`. It opens the database file read-only and
    /// changes no byte in `dir`. It creates the lock file only when the
    /// store has none. A directory without a store fails, and the call
    /// creates nothing in it. Every write call of the store fails with
    /// `ReadOnly`.
    ///
    /// redb opens a database read-only only from a path. So this call
    /// reaches the files directly, not through a `Vfs`. A store that a
    /// crash left open does not open read-only: `open` repairs it first.
    pub fn open_read_only(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = OsVfs::existing(dir.as_ref(), DATABASE)?;
        let db = redb::Builder::new()
            .open_read_only(dir.path(DATABASE))
            .map_err(|e| ZoeError::Backend(Box::new(e)))?;
        let backend = RedbBackend::open_read_only(db)?;
        let options = Options::default();
        Self::assemble(backend, Box::new(dir), Box::new(SystemClock), options, true)
    }

    /// Make a store from an open backend and the directory that holds its
    /// lock.
    fn assemble(
        backend: RedbBackend,
        dir: Box<dyn Any + Send + Sync>,
        clock: Box<dyn Clock>,
        options: Options,
        read_only: bool,
    ) -> Result<Self> {
        let next_oid = backend.next_oid()?;
        let last_tid = backend.last_tid();
        Ok(Self {
            backend: Arc::new(Opened {
                backend: Box::new(backend),
                _dir: dir,
            }),
            clock,
            locks: LockManager::default(),
            lock_timeout: options.lock_timeout,
            sequencer: Mutex::new(Some(last_tid)),
            next_oid: AtomicU64::new(next_oid.0),
            read_only,
        })
    }

    /// Fail with `ReadOnly` when the store is open for reads only.
    pub(crate) fn writable(&self) -> Result<()> {
        if self.read_only {
            return Err(ZoeError::ReadOnly);
        }
        Ok(())
    }

    /// Return a new oid. An oid that no commit uses can come back after
    /// the store reopens. When no oid below `u64::MAX` is left, fail with
    /// `Protocol`. A read-only store fails with `ReadOnly`.
    pub fn new_oid(&self) -> Result<Oid> {
        self.writable()?;
        self.next_oid
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
                (next < u64::MAX).then(|| next + 1)
            })
            .map(Oid)
            .map_err(|_| ZoeError::Protocol("no oid is left"))
    }

    /// Begin a transaction that borrows the store, driven by the calling
    /// thread. `Txn::begin` with an `Arc<Store>` gives a transaction that
    /// borrows nothing. A read-only store fails with `ReadOnly`.
    pub fn tpc_begin(
        &self,
        user: &[u8],
        description: &[u8],
        extension: &[u8],
    ) -> Result<Txn<&Self>> {
        self.writable()?;
        Ok(Txn::begin(self, user, description, extension))
    }

    /// Begin a restore transaction that borrows the store: a copy of a
    /// transaction with its own tid. `finish` commits it under `tid` and
    /// allocates no tid.
    ///
    /// `tid` must be after the last tid of the store. Otherwise the call
    /// fails with `Protocol`. `finish` checks this again under the commit
    /// sequencer. A read-only store fails with `ReadOnly`.
    pub fn tpc_begin_restore(
        &self,
        tid: Tid,
        user: &[u8],
        description: &[u8],
        extension: &[u8],
    ) -> Result<Txn<&Self>> {
        self.writable()?;
        Txn::begin_restore(self, tid, user, description, extension)
    }

    /// Return the current pickle of `oid` and its serial.
    pub fn load(&self, oid: Oid) -> Result<(Vec<u8>, Tid)> {
        match self.backend.load_before(oid, Tid::MAX)? {
            Some(record) if !record.data.is_empty() => Ok((record.data, record.tid)),
            _ => Err(ZoeError::NotFound { oid }),
        }
    }

    /// Return how the record that `tid` wrote for `oid` is stored: its
    /// codec, its stored length and the length of its pickle.
    pub fn record_info(&self, oid: Oid, tid: Tid) -> Result<RecordEncoding> {
        self.backend
            .record_info(oid, tid)?
            .ok_or(ZoeError::NotFound { oid })
    }

    /// Return the tid of the last committed transaction.
    pub fn last_transaction(&self) -> Tid {
        self.backend.last_tid()
    }

    /// Tell if no transaction holds or waits for a lock. A test of a skin
    /// uses it to show that every lock is released.
    #[doc(hidden)]
    pub fn lock_table_is_empty(&self) -> bool {
        self.locks.is_empty()
    }

    pub(crate) fn lock_sequencer(&self) -> MutexGuard<'_, Option<Tid>> {
        self.sequencer.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Make the next oid greater than `oid`, so that `new_oid` never gives
    /// an oid that a commit wrote.
    pub(crate) fn raise_next_oid(&self, oid: Oid) {
        self.next_oid
            .fetch_max(oid.0.saturating_add(1), Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicBool;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::backend::{Bytes, PackStats, Record, RecordMeta, TxnIter, WriteTxn};
    use crate::format;
    use crate::format::Codec;
    use crate::vfs::LOCK;

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
        /// `iter_txns` fails as a whole.
        rows: AtomicBool,
    }

    /// A backend that fails when a test turns a fault on.
    struct Faulty {
        inner: Arc<Opened>,
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

        fn record_info(&self, oid: Oid, tid: Tid) -> Result<Option<RecordEncoding>> {
            self.inner.record_info(oid, tid)
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
            if self.faults.rows.load(Ordering::Relaxed) {
                return Box::new(std::iter::once(Err(injected())));
            }
            self.inner.iter_txns(from, to)
        }

        fn history(&self, oid: Oid, limit: usize) -> Result<Vec<RecordMeta>> {
            self.inner.history(oid, limit)
        }

        fn load_before_next(&self, oid: Oid, tid: Tid) -> Result<(Option<Record>, Option<Tid>)> {
            self.inner.load_before_next(oid, tid)
        }

        fn txn_oids(&self, from: Tid, to: Tid) -> Result<BTreeMap<Tid, Vec<Oid>>> {
            self.inner.txn_oids(from, to)
        }

        fn current_from(&self, from: Oid) -> Result<Option<(Oid, Record)>> {
            self.inner.current_from(from)
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
        // The inner store keeps the directory.
        let faulty = Faulty {
            inner: store.backend,
            faults: Arc::clone(&faults),
        };
        let backend = Arc::new(Opened {
            backend: Box::new(faulty),
            _dir: Box::new(()),
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
    fn a_failed_transaction_log_gives_one_iterator_error() {
        let (store, faults) = faulty_store();
        let oid = store.new_oid().unwrap();
        let first = commit(&store, oid, Tid::ZERO, b"a").unwrap();
        commit(&store, oid, first, b"b").unwrap();
        faults.rows.store(true, Ordering::Relaxed);
        let txns: Vec<_> = store.iterator(None, None).unwrap().collect();
        assert!(matches!(txns[..], [Err(ZoeError::Backend(_))]), "{txns:?}");
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
    fn a_dropped_transaction_releases_its_locks() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let oid = store.new_oid().unwrap();
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.check_current_serial(Oid(oid.0 + 1), Tid::ZERO).unwrap();
        txn.store(oid, Tid::ZERO, b"a").unwrap();
        txn.vote().unwrap();
        assert!(!store.locks.is_empty());
        drop(txn);
        assert!(store.locks.is_empty());
    }

    #[test]
    fn a_lock_timeout_in_vote_releases_every_lock() {
        let mut store = Store::open_vfs(MemVfs::new()).unwrap();
        store.lock_timeout = Duration::ZERO;
        let (low, high) = (store.new_oid().unwrap(), store.new_oid().unwrap());
        let mut holder = store.tpc_begin(b"", b"", b"").unwrap();
        holder.store(high, Tid::ZERO, b"a").unwrap();
        holder.vote().unwrap();

        // The vote takes the lock on `low`, then waits for `high`.
        let mut waiter = store.tpc_begin(b"", b"", b"").unwrap();
        waiter.check_current_serial(low, Tid::ZERO).unwrap();
        waiter.store(high, Tid::ZERO, b"b").unwrap();
        let error = waiter.vote();
        assert!(
            matches!(error, Err(ZoeError::Conflict { oid }) if oid == high),
            "{error:?}"
        );

        // The lock on `low` is free.
        let mut other = store.tpc_begin(b"", b"", b"").unwrap();
        other.store(low, Tid::ZERO, b"c").unwrap();
        other.vote().unwrap();
        other.abort();
        holder.abort();
        assert!(store.locks.is_empty());
    }

    #[test]
    fn an_oid_read_and_written_takes_an_exclusive_lock() {
        let mut store = Store::open_vfs(MemVfs::new()).unwrap();
        store.lock_timeout = Duration::ZERO;
        let oid = store.new_oid().unwrap();
        let mut writer = store.tpc_begin(b"", b"", b"").unwrap();
        writer.check_current_serial(oid, Tid::ZERO).unwrap();
        writer.store(oid, Tid::ZERO, b"a").unwrap();
        writer.vote().unwrap();

        let mut reader = store.tpc_begin(b"", b"", b"").unwrap();
        reader.check_current_serial(oid, Tid::ZERO).unwrap();
        assert!(matches!(reader.vote(), Err(ZoeError::Conflict { .. })));
        writer.finish().unwrap();
        assert!(store.locks.is_empty());
    }

    #[test]
    fn a_stale_written_serial_wins_over_a_stale_read_serial() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let (low, high) = (store.new_oid().unwrap(), store.new_oid().unwrap());
        let mut create = store.tpc_begin(b"", b"", b"").unwrap();
        create.store(low, Tid::ZERO, b"a").unwrap();
        create.store(high, Tid::ZERO, b"a").unwrap();
        create.vote().unwrap();
        create.finish().unwrap();

        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.check_current_serial(low, Tid::ZERO).unwrap();
        txn.store(high, Tid::ZERO, b"b").unwrap();
        let error = txn.vote();
        assert!(
            matches!(error, Err(ZoeError::Conflict { oid }) if oid == high),
            "{error:?}"
        );
        assert!(store.locks.is_empty());
    }

    #[test]
    fn calls_after_the_vote_are_refused() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let oid = store.new_oid().unwrap();
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.store(oid, Tid::ZERO, b"a").unwrap();
        txn.vote().unwrap();
        assert!(matches!(txn.vote(), Err(ZoeError::Protocol(_))));
        assert!(matches!(
            txn.store(oid, Tid::ZERO, b"b"),
            Err(ZoeError::Protocol(_))
        ));
        assert!(matches!(
            txn.check_current_serial(oid, Tid::ZERO),
            Err(ZoeError::Protocol(_))
        ));
        txn.finish().unwrap();
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
    fn new_oid_fails_when_no_oid_is_left() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        store.next_oid.store(u64::MAX - 1, Ordering::Relaxed);
        assert_eq!(store.new_oid().unwrap(), Oid(u64::MAX - 1));
        for _ in 0..2 {
            assert!(matches!(store.new_oid(), Err(ZoeError::Protocol(_))));
        }
    }

    #[test]
    fn record_info_of_a_missing_record_is_not_found() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let oid = store.new_oid().unwrap();
        let tid = commit(&store, oid, Tid::ZERO, b"a").unwrap();
        let info = store.record_info(oid, tid).unwrap();
        assert_eq!(info.codec, Codec::None);
        assert_eq!((info.stored_len, info.data_len), (1, 1));
        let error = store.record_info(oid, Tid(tid.0 + 1));
        assert!(matches!(error, Err(ZoeError::NotFound { .. })), "{error:?}");
    }

    #[test]
    fn a_zstd_record_that_states_no_data_is_corrupt_not_deleted() {
        let records: redb::TableDefinition<&[u8; 16], &[u8]> =
            redb::TableDefinition::new("records");
        for payload in format::EMPTY_ZSTD_PAYLOADS {
            let vfs = MemVfs::new();
            let (oid, tid) = {
                let store = Store::open_vfs(vfs.clone()).unwrap();
                let oid = store.new_oid().unwrap();
                (oid, commit(&store, oid, Tid::ZERO, b"pickle").unwrap())
            };
            let db = redb::Builder::new()
                .create_with_backend(vfs.file("zoe.redb"))
                .unwrap();
            let txn = db.begin_write().unwrap();
            let key = format::record_key(oid, tid);
            let frame = format::encode_stored_frame(&key, format::CODEC_ZSTD, payload);
            txn.open_table(records)
                .unwrap()
                .insert(&key, frame.as_slice())
                .unwrap();
            txn.commit().unwrap();
            drop(db);

            let store = Store::open_vfs(vfs).unwrap();
            let error = store.load(oid);
            assert!(
                matches!(error, Err(ZoeError::CorruptRecord { .. })),
                "{payload:02x?}: {error:?}"
            );
        }
    }

    #[test]
    fn a_second_process_cannot_open_a_locked_store() {
        let dir = std::env::temp_dir().join(format!("zoe-locked-{}", std::process::id()));
        let _store = Store::open(&dir).unwrap();
        assert!(matches!(Store::open(&dir), Err(ZoeError::Locked)));
    }

    /// Return a new empty directory for one test.
    fn fresh_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zoe-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Write a store with one object to `dir` and close it. Return the oid
    /// and the tid of the commit.
    fn write_store(dir: &Path) -> (Oid, Tid) {
        let store = Store::open(dir).unwrap();
        let oid = store.new_oid().unwrap();
        (oid, commit(&store, oid, Tid::ZERO, b"a").unwrap())
    }

    /// Return the bytes of each file in `dir`, by name.
    fn files(dir: &Path) -> BTreeMap<String, Vec<u8>> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| {
                let path = entry.unwrap().path();
                let name = path.file_name().unwrap().to_string_lossy().into_owned();
                (name, std::fs::read(&path).unwrap())
            })
            .collect()
    }

    #[test]
    fn a_read_only_store_reads_and_changes_no_byte() {
        let dir = fresh_dir("read-only");
        let (oid, tid) = write_store(&dir);
        let before = files(&dir);

        let store = Store::open_read_only(&dir).unwrap();
        assert_eq!(store.last_transaction(), tid);
        assert_eq!(store.load(oid).unwrap(), (b"a".to_vec(), tid));
        assert_eq!(store.history(oid, 10).unwrap().len(), 1);
        assert_eq!(store.iterator(None, None).unwrap().count(), 1);
        drop(store);

        assert!(files(&dir) == before, "the read-only store changed a file");
    }

    #[test]
    fn a_read_only_store_refuses_every_write() {
        let dir = fresh_dir("read-only-writes");
        let (_, tid) = write_store(&dir);
        let store = Store::open_read_only(&dir).unwrap();
        assert!(matches!(store.new_oid(), Err(ZoeError::ReadOnly)));
        assert!(matches!(
            store.tpc_begin(b"", b"", b""),
            Err(ZoeError::ReadOnly)
        ));
        assert!(matches!(
            store.tpc_begin_restore(Tid(tid.0 + 1), b"", b"", b""),
            Err(ZoeError::ReadOnly)
        ));
        assert!(matches!(
            store.backend.begin_write(),
            Err(ZoeError::ReadOnly)
        ));
        // A transaction that a skin begins on the store itself votes no.
        let mut txn = Txn::begin(&store, b"", b"", b"");
        txn.store(Oid(1), tid, b"b").unwrap();
        assert!(matches!(txn.vote(), Err(ZoeError::ReadOnly)));
        assert!(matches!(txn.vote_resolving(), Err(ZoeError::ReadOnly)));
        assert!(store.locks.is_empty());
    }

    #[test]
    fn a_read_only_store_and_a_writer_exclude_each_other() {
        let dir = fresh_dir("read-only-lock");
        write_store(&dir);

        let reader = Store::open_read_only(&dir).unwrap();
        assert!(matches!(Store::open(&dir), Err(ZoeError::Locked)));
        assert!(matches!(Store::open_read_only(&dir), Err(ZoeError::Locked)));
        drop(reader);

        let writer = Store::open(&dir).unwrap();
        assert!(matches!(Store::open_read_only(&dir), Err(ZoeError::Locked)));
        drop(writer);
        Store::open_read_only(&dir).unwrap();
    }

    #[test]
    fn a_read_only_open_without_a_store_fails_and_creates_nothing() {
        let dir = fresh_dir("read-only-none");
        let missing = dir.join("missing");
        assert!(Store::open_read_only(&missing).is_err());
        assert!(!missing.exists());

        assert!(Store::open_read_only(&dir).is_err());
        assert!(files(&dir).is_empty());

        let file = dir.join("file");
        std::fs::write(&file, b"data").unwrap();
        assert!(Store::open_read_only(&file).is_err());
        assert_eq!(std::fs::read(&file).unwrap(), b"data");
    }

    #[test]
    fn a_read_only_open_creates_a_missing_lock_file() {
        let dir = fresh_dir("read-only-no-lock");
        write_store(&dir);
        std::fs::remove_file(dir.join(LOCK)).unwrap();

        let _reader = Store::open_read_only(&dir).unwrap();
        assert!(dir.join(LOCK).is_file());
        assert!(matches!(Store::open(&dir), Err(ZoeError::Locked)));
    }
}
