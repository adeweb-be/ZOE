//! A ZOE store: the entry point of the core.

use std::io;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use crate::backend::{Backend, BlobRecord, RecordEncoding};
use crate::blob::{BlobFile, BlobReader, BlobStore};
use crate::clock::{Clock, SystemClock};
use crate::commit::{StagingArea, Txn};
use crate::error::{Result, ZoeError};
use crate::format::Compression;
use crate::guard::Guardrails;
use crate::lock::LockManager;
use crate::pack::PackState;
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
    /// The most bytes of staged pickles that one transaction keeps in
    /// memory. The other pickles of the transaction go to a spill file
    /// under `staging/` in the store directory. This is `staging-memory`
    /// in the configuration.
    pub staging_memory: usize,
    /// The longest pickle that `store` and `store_blob` take, in bytes. A
    /// longer pickle fails with `TooLarge`. `None`, the default, sets no
    /// limit. This is `max-record-size` in the configuration.
    pub max_record_size: Option<u64>,
    /// The largest sum of the pickles that `store` and `store_blob` stage
    /// in one transaction, in bytes. Blob bytes do not count. The call
    /// that goes past it fails with `TooLarge`. `None`, the default, sets
    /// no limit. This is `max-transaction-size` in the configuration.
    pub max_transaction_size: Option<u64>,
    /// The longest blob that `store_blob` takes, in bytes. A longer blob
    /// fails with `TooLarge`. `None`, the default, sets no limit. This is
    /// `max-blob-size` in the configuration.
    pub max_blob_size: Option<u64>,
    /// A lock held longer than this is logged when it is released, with
    /// its oid and the time that it was held. `None`, the default, logs no
    /// lock. This is `lock-warn` in the configuration.
    pub lock_warn: Option<Duration>,
    /// A transaction open longer than this is logged when it ends, with
    /// the time that it was open. `None`, the default, logs no
    /// transaction. This is `transaction-warn` in the configuration.
    pub transaction_warn: Option<Duration>,
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
            staging_memory: 16 << 20,
            max_record_size: None,
            max_transaction_size: None,
            max_blob_size: None,
            lock_warn: None,
            transaction_warn: None,
            _more: (),
        }
    }
}

/// The open database of a store, its blob store and its staging area.
/// The blob store and the staging area share the directory, and the
/// directory holds the lock of the store. Nothing else holds the
/// directory.
///
/// The fields drop in the order of their declaration. So the database
/// closes, and writes its last bytes, before the directory releases the
/// lock.
pub(crate) struct Opened {
    backend: Box<dyn Backend>,
    pub(crate) blobs: BlobStore,
    pub(crate) staging: StagingArea,
}

impl Deref for Opened {
    type Target = dyn Backend;

    fn deref(&self) -> &Self::Target {
        &*self.backend
    }
}

/// An open store.
pub struct Store {
    /// The database, the blob store and the directory. A transaction
    /// iterator and a blob file hold only a weak reference, so dropping
    /// the `Store` closes the store.
    pub(crate) backend: Arc<Opened>,
    pub(crate) clock: Box<dyn Clock>,
    pub(crate) locks: LockManager,
    pub(crate) lock_timeout: Duration,
    pub(crate) guardrails: Guardrails,
    /// The tid of the last commit. Its lock is the commit sequencer. It is
    /// `None` after a failed finish, because the outcome of that commit is
    /// unknown.
    sequencer: Mutex<Option<Tid>>,
    next_oid: AtomicU64,
    /// True when the store is open for reads only.
    read_only: bool,
    /// The state that a pack shares with the finishes. A finish takes its
    /// lock under the commit sequencer.
    pub(crate) packing: PackState,
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

    /// Open the store in the simulated filesystem `vfs` with no page
    /// cache, and create it if necessary. Each page that a read needs then
    /// comes from the file, so a test can count the bytes that a read
    /// costs.
    #[doc(hidden)]
    pub fn open_vfs_uncached(vfs: MemVfs) -> Result<Self> {
        Self::open_cached(vfs, SystemClock, Options::default(), Some(0))
    }

    /// Open the store in `vfs` with the clock `clock` and `options`, and
    /// create it if necessary. The store reads the time only from `clock`:
    /// the tids, and the durations of `lock-warn` and `transaction-warn`.
    ///
    /// A new store is written under a temporary name and renamed into place
    /// after it is durable, so a crash during the creation leaves either
    /// no store or a complete empty one.
    pub fn open_full<V: Vfs>(
        vfs: V,
        clock: impl Clock + 'static,
        options: Options,
    ) -> Result<Self> {
        Self::open_cached(vfs, clock, options, None)
    }

    /// Open the store as `open_full` does. `cache_size` sets the page cache
    /// of the database in bytes. `None` keeps the default of redb.
    fn open_cached<V: Vfs>(
        vfs: V,
        clock: impl Clock + 'static,
        options: Options,
        cache_size: Option<usize>,
    ) -> Result<Self> {
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
        }
        // An earlier open can stop after the rename and before its sync.
        // This sync makes that rename durable. It also covers a new store.
        vfs.sync_dir()?;

        let mut builder = redb::Builder::new();
        if let Some(bytes) = cache_size {
            builder.set_cache_size(bytes);
        }
        let db = builder
            .create_with_backend(vfs.open(DATABASE)?)
            .map_err(|e| ZoeError::Backend(Box::new(e)))?;
        let backend = RedbBackend::open(db)?.with_compression(options.compression);
        Self::assemble(backend, vfs, Box::new(clock), options, false)
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
    /// crash left open fails with `NotClosedCleanly`: a writer open
    /// rebuilds the free-space map of its database.
    pub fn open_read_only(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = OsVfs::existing(dir.as_ref(), DATABASE)?;
        let db = redb::Builder::new()
            .open_read_only(dir.path(DATABASE))
            .map_err(|e| match e {
                redb::DatabaseError::RepairAborted => ZoeError::NotClosedCleanly,
                e => ZoeError::Backend(Box::new(e)),
            })?;
        let backend = RedbBackend::open_read_only(db)?;
        let options = Options::default();
        Self::assemble(backend, dir, Box::new(SystemClock), options, true)
    }

    /// Make a store from an open backend and the directory that holds its
    /// lock. The blob store and the staging area share the directory.
    fn assemble<V: Vfs>(
        backend: RedbBackend,
        dir: V,
        clock: Box<dyn Clock>,
        options: Options,
        read_only: bool,
    ) -> Result<Self> {
        let next_oid = backend.next_oid()?;
        let last_tid = backend.last_tid();
        let dir = Arc::new(dir);
        let opened = Arc::new(Opened {
            backend: Box::new(backend),
            blobs: BlobStore::new(Box::new(Arc::clone(&dir))),
            staging: StagingArea::new(Box::new(dir), options.staging_memory, read_only),
        });
        // `Opened` holds the database and the directory here, so an error
        // drops them in order: the database closes before the lock goes.
        opened.blobs.open(&**opened, read_only)?;
        // The staging area of a read-only store removes no leftover and
        // creates no spill file.
        opened.staging.open()?;
        Ok(Self {
            backend: opened,
            clock,
            locks: LockManager::default(),
            lock_timeout: options.lock_timeout,
            guardrails: Guardrails::new(&options),
            sequencer: Mutex::new(Some(last_tid)),
            next_oid: AtomicU64::new(next_oid.0),
            read_only,
            packing: PackState::default(),
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

    /// Remove the key `(tid, oid)` of the transaction index and keep the
    /// record. A test uses it to damage the store. Fail with `NotFound`
    /// when the key does not exist.
    #[doc(hidden)]
    pub fn remove_index_key_for_test(&self, tid: Tid, oid: Oid) -> Result<()> {
        self.backend.remove_index_key_for_test(tid, oid)
    }

    /// Remove the record of `oid` at `tid` and keep its key in the
    /// transaction index. A test uses it to damage the store. Fail with
    /// `NotFound` when the record does not exist.
    #[doc(hidden)]
    pub fn remove_record_for_test(&self, oid: Oid, tid: Tid) -> Result<()> {
        self.backend.remove_record_for_test(oid, tid)
    }

    /// Tell if no transaction holds or waits for a lock. A test of a skin
    /// uses it to show that every lock is released.
    #[doc(hidden)]
    pub fn lock_table_is_empty(&self) -> bool {
        self.locks.is_empty()
    }

    /// Return a new, empty blob file. Write the blob into it, then give it
    /// to `Txn::store_blob`. Dropping a file that no transaction took
    /// removes it. A read-only store fails with `ReadOnly` and creates
    /// nothing.
    pub fn new_blob_file(&self) -> Result<BlobFile> {
        self.writable()?;
        self.blobs().new_file(Arc::downgrade(&self.backend))
    }

    /// Open the blob that `tid` wrote for `oid`.
    ///
    /// The reader checks the size and the BLAKE3 hash of the blob when it
    /// reaches the end of the file. A mismatch fails that read with
    /// `InvalidData`. A missing file is `CorruptBlob`.
    pub fn open_blob(&self, oid: Oid, tid: Tid) -> Result<BlobReader> {
        let record = self.blob_record(oid, tid)?;
        self.blobs().reader(oid, tid, record)
    }

    /// Read the whole blob that `tid` wrote for `oid`, and check its size
    /// and its BLAKE3 hash. A mismatch is `CorruptBlob`.
    pub fn verify_blob(&self, oid: Oid, tid: Tid) -> Result<()> {
        let mut reader = self.open_blob(oid, tid)?;
        match io::copy(&mut reader, &mut io::sink()) {
            Ok(_) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                Err(ZoeError::CorruptBlob { oid, tid })
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Return the path of the blob file that `tid` wrote for `oid`. The
    /// file is read-only. A read through the path checks no hash.
    pub fn blob_path(&self, oid: Oid, tid: Tid) -> Result<PathBuf> {
        self.blob_record(oid, tid)?;
        Ok(self.blobs().path(oid, tid))
    }

    /// Return the blob store.
    pub(crate) fn blobs(&self) -> &BlobStore {
        &self.backend.blobs
    }

    /// Return the blob record that `tid` wrote for `oid`, or `NotFound`.
    fn blob_record(&self, oid: Oid, tid: Tid) -> Result<BlobRecord> {
        self.backend
            .load_blob(oid, tid)?
            .ok_or(ZoeError::NotFound { oid })
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
    use crate::backend::{
        BlobRecord, Bytes, PackStats, Record, RecordMeta, TxnIter, TxnRecords, WriteTxn,
    };
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

        fn load_blob(&self, oid: Oid, tid: Tid) -> Result<Option<BlobRecord>> {
            self.inner.load_blob(oid, tid)
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

        fn pack_watermark(&self) -> Result<Tid> {
            self.inner.pack_watermark()
        }

        fn record_keys(&self, from: Oid, limit: usize) -> Result<Vec<(Oid, Tid)>> {
            self.inner.record_keys(from, limit)
        }

        fn txn_records(&self, tid: Tid, oids: &[Oid]) -> Result<Option<TxnRecords>> {
            self.inner.txn_records(tid, oids)
        }

        fn sync(&self) -> Result<()> {
            self.inner.sync()
        }
    }

    /// Open a store whose backend fails when a test turns a fault on.
    fn faulty_store() -> (Store, Arc<Faults>) {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let faults = Arc::new(Faults::default());
        // The inner store keeps the directory. The tests of this store use
        // no blob, so its blob store has an empty directory of its own. So
        // has its staging area, which spills each pickle.
        let faulty = Faulty {
            inner: store.backend,
            faults: Arc::clone(&faults),
        };
        let backend = Arc::new(Opened {
            backend: Box::new(faulty),
            blobs: BlobStore::new(Box::new(MemVfs::new())),
            staging: StagingArea::new(Box::new(MemVfs::new()), 0, false),
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
    fn a_halted_store_refuses_a_pack() {
        let (store, faults) = faulty_store();
        let oid = store.new_oid().unwrap();
        let first = commit(&store, oid, Tid::ZERO, b"a").unwrap();
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.store(oid, first, b"b").unwrap();
        txn.vote().unwrap();
        faults.write.store(true, Ordering::Relaxed);
        assert!(txn.finish().is_err());
        faults.write.store(false, Ordering::Relaxed);

        let error = store.pack(Tid::MAX);
        assert!(matches!(error, Err(ZoeError::Halted)), "{error:?}");
        assert_eq!(store.load(oid).unwrap(), (b"a".to_vec(), first));
    }

    #[test]
    fn a_failed_pack_write_halts_the_store() {
        let (store, faults) = faulty_store();
        let oid = store.new_oid().unwrap();
        let first = commit(&store, oid, Tid::ZERO, b"a").unwrap();
        let second = commit(&store, oid, first, b"b").unwrap();
        faults.write.store(true, Ordering::Relaxed);
        let error = store.pack(second);
        assert!(matches!(error, Err(ZoeError::Backend(_))), "{error:?}");
        faults.write.store(false, Ordering::Relaxed);

        // The outcome of the pack write is unknown, as after a failed
        // finish.
        let error = commit(&store, oid, second, b"c");
        assert!(matches!(error, Err(ZoeError::Halted)), "{error:?}");
        assert!(matches!(store.pack(second), Err(ZoeError::Halted)));
        assert!(store.locks.is_empty());
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
    fn a_read_only_store_refuses_a_pack() {
        let dir = fresh_dir("read-only-pack");
        // The root does not decode, so a pack that reads it fails with
        // another error.
        let tid = {
            let store = Store::open(&dir).unwrap();
            commit(&store, Oid(0), Tid::ZERO, b"a").unwrap()
        };
        let before = files(&dir);

        let store = Store::open_read_only(&dir).unwrap();
        let error = store.pack(Tid::MAX);
        assert!(matches!(error, Err(ZoeError::ReadOnly)), "{error:?}");
        assert_eq!(store.load(Oid(0)).unwrap(), (b"a".to_vec(), tid));
        drop(store);
        assert!(files(&dir) == before, "the refused pack changed a file");
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
    fn a_store_left_open_opens_read_only_only_after_a_writer() {
        let open = fresh_dir("read-only-open");
        let copy = fresh_dir("read-only-copy");
        let (oid, _) = write_store(&open);
        // A copy of the database of an open store is what a crash leaves.
        let store = Store::open(&open).unwrap();
        std::fs::copy(open.join(DATABASE), copy.join(DATABASE)).unwrap();
        drop(store);

        let result = Store::open_read_only(&copy);
        assert!(
            matches!(result, Err(ZoeError::NotClosedCleanly)),
            "{:?}",
            result.err()
        );
        drop(Store::open(&copy).unwrap());
        let store = Store::open_read_only(&copy).unwrap();
        assert_eq!(store.load(oid).unwrap().0, b"a");
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
