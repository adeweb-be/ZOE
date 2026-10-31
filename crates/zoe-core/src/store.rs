//! A ZOE store: the entry point of the core.

use std::io;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use crate::backend::{Backend, BlobRecord, RecordEncoding};
use crate::blob::{self, BlobFile, BlobReader, BlobStore, PendingBlob};
use crate::clock::{Clock, SystemClock};
use crate::commit::{GroupCommit, StagingArea, Txn};
use crate::error::{Result, ZoeError};
use crate::format::Compression;
use crate::guard::Guardrails;
use crate::lock::LockManager;
use crate::merge::StructuralMerges;
use crate::merge::policy::{Policy, PolicyCache};
use crate::pack::PackState;
use crate::redb::RedbBackend;
use crate::sequence::{Sequence, Sequences};
use crate::types::{Oid, Tid};
use crate::vfs::{Dir, MemVfs, OsVfs, Vfs};

/// The name of the database file in the store directory.
const DATABASE: &str = "zoe.redb";

/// The name under which a new database is written before it is renamed.
const NEW_DATABASE: &str = "zoe.redb.new";

/// Open the database file `path` read-only, with a page cache of
/// `cache_size` bytes, or the default of redb.
fn read_only_database(
    path: &Path,
    cache_size: Option<usize>,
) -> std::result::Result<redb::ReadOnlyDatabase, redb::DatabaseError> {
    let mut builder = redb::Builder::new();
    if let Some(bytes) = cache_size {
        builder.set_cache_size(bytes);
    }
    builder.open_read_only(path)
}

/// Repair the database file `path` that a writer left open, with a page
/// cache of `cache_size` bytes, or the default of redb.
///
/// redb repairs the file when it opens it for writes. The open creates no
/// file. The drop of the database closes it cleanly: it writes the
/// allocator state and clears the recovery flag.
fn repair_database(path: &Path, cache_size: Option<usize>) -> Result<()> {
    let mut builder = redb::Builder::new();
    if let Some(bytes) = cache_size {
        builder.set_cache_size(bytes);
    }
    drop(builder.open(path).map_err(repair_failed)?);
    Ok(())
}

/// Return the `RepairFailed` error of the redb error `error`. An error of
/// the system stays as it is, so the text gives it without the words of
/// redb.
fn repair_failed(error: redb::DatabaseError) -> ZoeError {
    match error {
        redb::DatabaseError::Storage(redb::StorageError::Io(e)) => {
            ZoeError::RepairFailed(Box::new(e))
        }
        e => ZoeError::RepairFailed(Box::new(e)),
    }
}

/// Return the directory that holds the blob directory `blob_dir`, and the
/// name of the blob directory there.
///
/// A relative path is taken from the current directory. The path must end
/// in a name that is UTF-8, as every name of the core. An existing entry at
/// `blob_dir` must be a directory. The call creates nothing.
fn blob_location(blob_dir: &Path) -> Result<(PathBuf, String)> {
    let invalid = |message: &str| {
        let message = format!("blob directory {}: {message}", blob_dir.display());
        ZoeError::Io(io::Error::new(io::ErrorKind::InvalidInput, message))
    };
    let blob_dir = std::path::absolute(blob_dir)?;
    let (Some(parent), Some(name)) = (blob_dir.parent(), blob_dir.file_name()) else {
        return Err(invalid("the path does not end in a name"));
    };
    let name = name
        .to_str()
        .ok_or_else(|| invalid("the name is not UTF-8"))?;
    match std::fs::metadata(&blob_dir) {
        Ok(metadata) if !metadata.is_dir() => Err(invalid("not a directory")),
        Ok(_) => Ok((parent.to_owned(), name.to_owned())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok((parent.to_owned(), name.to_owned())),
        Err(e) => Err(e.into()),
    }
}

/// The directory that holds the blob directory of a store, and the name of
/// the blob directory there. `None` keeps the blob files in `blobs` in the
/// store directory.
type Relocation = Option<(Box<dyn Dir>, String)>;

/// Return the relocation of the blob directory that `options` gives, with
/// `open` to open the directory that holds it. Without `blob_dir`, return
/// `None` and open nothing. `blob_location` checks the path first.
fn relocation(
    options: &Options,
    open: impl FnOnce(PathBuf) -> Result<Box<dyn Dir>>,
) -> Result<Relocation> {
    let Some(blob_dir) = options.blob_dir.as_deref() else {
        return Ok(None);
    };
    let (parent, name) = blob_location(blob_dir)?;
    Ok(Some((open(parent)?, name)))
}

/// The options of a store.
///
/// Build it from the defaults, for example
/// `Options { lock_timeout, ..Options::default() }`. Then a new option does
/// not break the caller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    /// The longest time that a vote waits for its locks. A longer wait
    /// fails the vote with `Conflict`. This is `lock-timeout` in the
    /// configuration.
    pub lock_timeout: Duration,
    /// How the store writes new records. The default is `None`: the records
    /// are raw (ADR 0041).
    /// The store reads records of every codec, whatever this option.
    pub compression: Compression,
    /// The most bytes of staged pickles that one transaction keeps in
    /// memory. The other pickles of the transaction go to a spill file
    /// under `staging/` in the store directory. This is `staging-memory`
    /// in the configuration.
    ///
    /// The limit counts the staged pickles only. A payload that a skin
    /// attaches with `Txn::set_encoded` comes in addition. `zoe serve`
    /// also holds a copy of each pickle in memory until its compression
    /// job runs. So the memory of one transaction of `zoe serve` can reach
    /// about three times this limit: the pickles, their copies in the
    /// queue, and the encoded payloads, which zstd keeps only when they are
    /// smaller than their pickles.
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
    /// The directory of the blob files. `None` is `blobs` in the store
    /// directory. This is `blob-dir` in the configuration.
    ///
    /// The store does not record this directory: each open must give the
    /// same one. A writer open refuses a store with blob records whose
    /// blob directory has no layout file, with `MissingBlobDir`.
    pub blob_dir: Option<PathBuf>,
    /// The most bytes that the page cache of the database holds: the pages
    /// that reads keep, and the pages that a write buffers before it goes
    /// to the file. `None`, the default, keeps the default of redb, 1 GiB.
    /// This is `cache-size` in the configuration.
    pub cache_size: Option<usize>,
    /// Pack with garbage collection when true, the default. When false, a
    /// pack removes old revisions only: of each object, it keeps the
    /// newest record at or before the pack tid, also when no reader can
    /// reach the object. When that record is a deletion, the object goes.
    /// Such a pack reads no reference. The store does not record this
    /// option. This is `pack-gc` in the configuration.
    pub pack_gc: bool,
    /// The structural merges (tier 1) of the conflict resolution: the
    /// mode of their classes, and the rules of ADR 0042. The default is
    /// tier 1 on for every class, and the rules off.
    pub structural_merges: StructuralMerges,
    /// The write-ahead log of the store (ADR 0056). The skins do not
    /// expose these settings: tests and benchmarks set them.
    pub write_log: WriteLog,
    /// Not an option. This field makes `..Options::default()` necessary
    /// when a caller sets every option.
    #[doc(hidden)]
    pub _more: (),
}

/// The settings of the write-ahead log of a store (ADR 0056).
///
/// A finish appends its commit batch to the log and syncs the log. The
/// fold writes the commits of the log into the tables later, in slices.
/// The open checks the settings: an open with settings that do not hold
/// together fails with an `InvalidInput` I/O error.
///
/// Build it from the defaults, for example
/// `WriteLog { fold_bytes, ..WriteLog::default() }`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriteLog {
    /// True, the default, sends commit batches to the log. False writes
    /// each batch into the tables, as a direct batch. The open replays a
    /// log that exists, also when this is false.
    pub enabled: bool,
    /// The size at which a segment file stops to grow and the next batch
    /// goes into another segment. A segment holds at least one entry, so
    /// a larger entry makes its segment larger. At least 4,096. The
    /// default is 64 MiB.
    pub segment_bytes: u64,
    /// The backlog at which the fold starts: the bytes of the commits that
    /// the log holds and the tables do not. The default is 16 MiB.
    pub fold_bytes: u64,
    /// The age of the oldest commit of the backlog at which the fold
    /// starts, on the monotonic time of the store clock. It bounds the time
    /// that a commit stays in the log alone. The default is 5 s.
    pub fold_age: Duration,
    /// The most bytes of log entries that one backend write of the fold
    /// takes. A slice takes at least one commit. The default is 4 MiB.
    pub slice_bytes: u64,
    /// The bound of the backlog. A finish that would go past it waits for
    /// the fold. At least `fold_bytes`. The default is 256 MiB.
    pub backlog_bytes: u64,
    /// The pickle size from which a record is large. A batch whose large
    /// records hold at least half of its pickle bytes goes into the tables
    /// directly when the backlog is empty. The default is 64 KiB.
    pub large_record: u64,
    /// Not an option. This field makes `..WriteLog::default()` necessary
    /// when a caller sets every option.
    #[doc(hidden)]
    pub _more: (),
}

impl Default for WriteLog {
    fn default() -> Self {
        Self {
            enabled: true,
            segment_bytes: 64 << 20,
            fold_bytes: 16 << 20,
            fold_age: Duration::from_secs(5),
            slice_bytes: 4 << 20,
            backlog_bytes: 256 << 20,
            large_record: 64 << 10,
            _more: (),
        }
    }
}

impl Default for Options {
    fn default() -> Self {
        Self {
            lock_timeout: Duration::from_secs(10),
            compression: Compression::None,
            staging_memory: 16 << 20,
            max_record_size: None,
            max_transaction_size: None,
            max_blob_size: None,
            lock_warn: None,
            transaction_warn: None,
            blob_dir: None,
            cache_size: None,
            pack_gc: true,
            structural_merges: StructuralMerges::default(),
            write_log: WriteLog::default(),
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
    /// The compression of the records that the store writes.
    compression: Compression,
    /// The tid of the last commit. Its lock is the commit sequencer. It is
    /// `None` after a failed finish, because the outcome of that commit is
    /// unknown.
    sequencer: Mutex<Option<Tid>>,
    /// The commit batches. A batch leader takes the commit sequencer.
    pub(crate) group: GroupCommit,
    next_oid: AtomicU64,
    /// True when the store is open for reads only.
    read_only: bool,
    /// True when a pack collects garbage. This is `Options::pack_gc`.
    pub(crate) pack_gc: bool,
    /// True when the open repaired the store.
    repaired: bool,
    /// The state that a pack shares with the finishes. A finish takes its
    /// lock under the commit sequencer.
    pub(crate) packing: PackState,
    /// The reserved values of the sequences that this store uses.
    pub(crate) sequences: Sequences,
    /// The structural merges (tier 1) of the conflict resolution.
    pub(crate) structural_merges: StructuralMerges,
    /// The merge policy that the store read last (ADR 0046).
    policy: Mutex<PolicyCache>,
}

impl Store {
    /// Tell if the directory `dir` holds a store: its database file is a
    /// regular file. A read-only open of a directory without one fails.
    pub fn exists(dir: impl AsRef<Path>) -> bool {
        dir.as_ref().join(DATABASE).is_file()
    }

    /// Open the store in the directory `dir` with the default options, and
    /// create it if necessary.
    pub fn open(dir: impl AsRef<Path>) -> Result<Self> {
        Self::open_with(dir, Options::default())
    }

    /// Open the store in the directory `dir` with `options`, and create it
    /// if necessary.
    ///
    /// With `options.blob_dir`, the blob files live in that directory. The
    /// call checks it before it creates anything: the path must end in a
    /// name, and an existing entry must be a directory. Then it creates the
    /// directory that holds the blob directory, and then the store
    /// directory, each with its missing parents. So a blob directory that
    /// fails leaves no lock file and no database. The blob directory itself
    /// comes with the first blob.
    pub fn open_with(dir: impl AsRef<Path>, options: Options) -> Result<Self> {
        let relocated = relocation(&options, |parent| Ok(Box::new(OsVfs::unlocked(&parent)?)))?;
        let vfs = OsVfs::new(dir.as_ref())?;
        Self::open_parts(vfs, SystemClock, options, relocated)
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
    /// costs. Only the feature `testing` builds it.
    #[cfg(feature = "testing")]
    #[doc(hidden)]
    pub fn open_vfs_uncached(vfs: MemVfs) -> Result<Self> {
        let options = Options {
            cache_size: Some(0),
            ..Options::default()
        };
        Self::open_full(vfs, SystemClock, options)
    }

    /// Open the store in the simulated filesystem `vfs`, with its blob
    /// directory `name` in the simulated filesystem `parent`.
    #[cfg(test)]
    pub(crate) fn open_relocated(vfs: MemVfs, parent: MemVfs, name: &str) -> Result<Self> {
        let parent: Box<dyn Dir> = Box::new(parent);
        let relocated = Some((parent, name.to_owned()));
        Self::open_parts(vfs, SystemClock, Options::default(), relocated)
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
        Self::open_parts(vfs, clock, options, None)
    }

    /// Open the store in `vfs` as `open_full` does. `relocated` is the
    /// directory that holds the blob directory, and the name of the blob
    /// directory there. `None` keeps the blob files in `blobs` in `vfs`.
    pub(crate) fn open_parts<V: Vfs>(
        vfs: V,
        clock: impl Clock + 'static,
        options: Options,
        relocated: Relocation,
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
        if let Some(bytes) = options.cache_size {
            builder.set_cache_size(bytes);
        }
        let db = builder
            .create_with_backend(vfs.open(DATABASE)?)
            .map_err(|e| ZoeError::Backend(Box::new(e)))?;
        let backend = RedbBackend::open(db)?.with_compression(options.compression);
        Self::assemble(backend, vfs, Box::new(clock), options, false, relocated)
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
    /// rebuilds the free-space map of its database. This open is strict:
    /// it never repairs, so it tells if a store was closed cleanly. A tool
    /// that reads a store uses `open_read_only_repairing`.
    pub fn open_read_only(dir: impl AsRef<Path>) -> Result<Self> {
        Self::open_read_only_with(dir, Options::default())
    }

    /// Open the existing store in the directory `dir` for reads only, with
    /// `options`, as `open_read_only` does.
    ///
    /// With `options.blob_dir`, the store reads its blob files in that
    /// directory. The call changes no byte and creates nothing in the blob
    /// directory or in the directory that holds it, and neither need exist.
    /// The path must end in a name that is UTF-8, and an existing entry
    /// there must be a directory. As `open_read_only`, the call does not
    /// check that the blob directory is the one of the store:
    /// `check_blob_dir` does.
    pub fn open_read_only_with(dir: impl AsRef<Path>, options: Options) -> Result<Self> {
        Self::open_reader(dir.as_ref(), options, false)
    }

    /// Open the existing store in the directory `dir` for reads only, with
    /// `options`, as `open_read_only_with` does. Repair the store first
    /// when a writer left it open.
    ///
    /// The call takes the lock of the store before it reads the database,
    /// and the store holds the lock until it closes. So no other open comes
    /// between the repair and the read-only open. On a store that was
    /// closed cleanly, the call is `open_read_only_with`: it changes no
    /// byte. On a store that a writer left open, the read-only open of redb
    /// fails. Then the call opens `zoe.redb` for writes, and redb repairs
    /// the file. The call closes that database, which writes the allocator
    /// state and clears the recovery flag. Then it opens the store
    /// read-only. `repaired` tells that the open repaired the store. The
    /// core logs nothing for it: the caller tells the operator.
    ///
    /// The repair writes only `zoe.redb`, and keeps every commit that was
    /// durable. It runs no recovery of the store: it removes no
    /// `zoe.redb.new` and no file in `blobs/tmp/` or `staging/`, replays
    /// no intent file, and writes nothing in the blob directory. The next
    /// writer open does these. The repair reads every page of the database
    /// file, with `options.cache_size` as its page cache.
    ///
    /// A failure of the open for writes or of the repair fails with
    /// `RepairFailed`. A read-only file system, or a database file without
    /// write permission, fails the open for writes before redb writes a
    /// byte. Another holder of the lock fails the call with `Locked`
    /// before any write.
    pub fn open_read_only_repairing(dir: impl AsRef<Path>, options: Options) -> Result<Self> {
        Self::open_reader(dir.as_ref(), options, true)
    }

    /// Open the existing store in `dir` for reads only, with `options`.
    /// With `repair`, repair a store that a writer left open, as
    /// `open_read_only_repairing` tells. Without it, such a store fails
    /// with `NotClosedCleanly`.
    fn open_reader(dir: &Path, options: Options, repair: bool) -> Result<Self> {
        let relocated = relocation(&options, |parent| {
            Ok(Box::new(blob::ReadOnlyDir::new(parent)))
        })?;
        let dir = OsVfs::existing(dir, DATABASE)?;
        let path = dir.path(DATABASE);
        let (db, repaired) = match read_only_database(&path, options.cache_size) {
            Ok(db) => (db, false),
            Err(redb::DatabaseError::RepairAborted) if repair => {
                repair_database(&path, options.cache_size)?;
                let db = read_only_database(&path, options.cache_size).map_err(repair_failed)?;
                (db, true)
            }
            Err(redb::DatabaseError::RepairAborted) => return Err(ZoeError::NotClosedCleanly),
            Err(e) => return Err(ZoeError::Backend(Box::new(e))),
        };
        let backend = RedbBackend::open_read_only(db)?;
        let mut store = Self::assemble(
            backend,
            dir,
            Box::new(SystemClock),
            options,
            true,
            relocated,
        )?;
        store.repaired = repaired;
        Ok(store)
    }

    /// Tell if the open of this store repaired it:
    /// `open_read_only_repairing` found a store that a writer left open,
    /// and repaired its database file. Every other open gives false.
    pub fn repaired(&self) -> bool {
        self.repaired
    }

    /// Return the message of the repair of the store in `dir`, for the
    /// line that a tool writes when `repaired` is true:
    /// `<dir>: the store was not closed cleanly: repaired the store`.
    pub fn repair_message(dir: impl AsRef<Path>) -> String {
        format!(
            "{}: the store was not closed cleanly: repaired the store",
            dir.as_ref().display()
        )
    }

    /// Make a store from an open backend and the directory that holds its
    /// lock. The blob store and the staging area share the directory.
    /// `relocated` puts the blob directory outside it, as `open_full`
    /// tells: the staging area stays in the store directory.
    fn assemble<V: Vfs>(
        backend: RedbBackend,
        dir: V,
        clock: Box<dyn Clock>,
        options: Options,
        read_only: bool,
        relocated: Relocation,
    ) -> Result<Self> {
        let next_oid = backend.next_oid()?;
        let last_tid = backend.last_tid();
        // The store directory is held once. The blob store and the staging
        // area share it.
        let dir: Arc<dyn Dir> = Arc::new(dir);
        let blob_dir: Arc<dyn Dir> = match relocated {
            Some((parent, name)) => Arc::new(blob::Relocated::new(Arc::clone(&dir), parent, name)),
            None => Arc::clone(&dir),
        };
        let opened = Arc::new(Opened {
            backend: Box::new(backend),
            blobs: BlobStore::new(blob_dir),
            staging: StagingArea::new(dir, options.staging_memory, read_only),
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
            compression: options.compression,
            sequencer: Mutex::new(Some(last_tid)),
            group: GroupCommit::default(),
            next_oid: AtomicU64::new(next_oid.0),
            read_only,
            pack_gc: options.pack_gc,
            repaired: false,
            packing: PackState::default(),
            sequences: Sequences::default(),
            structural_merges: options.structural_merges,
            policy: Mutex::new(PolicyCache::default()),
        })
    }

    /// Return the merge policy of the store (ADR 0046): the object
    /// `root['zoe.merge_policy']`. The store reads it again when the serial
    /// of the root or of the policy changed since its last read. Only a vote
    /// with a conflict calls this.
    pub(crate) fn merge_policy(&self) -> Result<Option<Arc<Policy>>> {
        crate::lock(&self.policy).get(&**self.backend, &self.structural_merges)
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

    /// Return the sequence `name` of the store.
    ///
    /// `Sequence::next` gives values that are unique and strictly
    /// increasing per store and per name, from 1. Each crash or reopen can
    /// skip up to 100 values, and the skips of reopens in a row add up. An
    /// allocation is not part of a transaction: an abort does not give its
    /// values back.
    pub fn sequence(&self, name: &str) -> Sequence<'_> {
        Sequence::new(self, name)
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

    /// Begin a restore transaction that borrows the store, as
    /// `tpc_begin_restore` does, whose finish commits without a sync of
    /// the database file. `Txn::begin_restore_deferred` tells what the
    /// finish does. `sync` makes the commit durable.
    pub fn tpc_begin_restore_deferred(
        &self,
        tid: Tid,
        user: &[u8],
        description: &[u8],
        extension: &[u8],
    ) -> Result<Txn<&Self>> {
        self.writable()?;
        Txn::begin_restore_deferred(self, tid, user, description, extension)
    }

    /// Make every commit before this call durable.
    ///
    /// The call runs the barrier `Backend::sync` under the commit
    /// sequencer: one backend write transaction of its own, committed with
    /// a sync. It writes no record, no transaction row and
    /// no last tid, so the last tid does not change. Then it removes the
    /// intent files that the deferred commits kept in `blobs/tmp/`.
    ///
    /// A read-only store fails with `ReadOnly`, and a halted store with
    /// `Halted`. Another error leaves the store halted, as a failed
    /// `finish` does.
    pub fn sync(&self) -> Result<()> {
        self.writable()?;
        let mut sequencer = self.lock_sequencer();
        let Some(last) = *sequencer else {
            return Err(ZoeError::Halted);
        };
        // The sequencer stays empty if the commit below fails.
        *sequencer = None;
        self.backend.sync()?;
        *sequencer = Some(last);
        self.blobs().release_kept();
        Ok(())
    }

    /// Fold every commit that the log holds into the tables, and wait for
    /// the end of that fold (ADR 0056). The commits after the call can
    /// stay in the log.
    ///
    /// A store with nothing to fold, a read-only store included, returns
    /// at once. A store that halted, or whose fold failed, fails with
    /// `Halted`.
    pub fn fold(&self) -> Result<()> {
        unimplemented!("the write-ahead log of ADR 0056")
    }

    /// Return the bytes of the backlog: the commits that the log holds and
    /// the tables do not, as the bound `WriteLog::backlog_bytes` counts
    /// them (ADR 0056). The call reads no file.
    pub fn backlog_bytes(&self) -> u64 {
        unimplemented!("the write-ahead log of ADR 0056")
    }

    /// Return the tids of the log entry that the open cut while its header
    /// passed and its body did not: the previous tid and the last tid of
    /// the entry (ADR 0056, section 12). The commits of that entry are lost:
    /// a crash stopped their finish, or a damage of the disk destroyed
    /// them after their sync. The open cannot tell which. `None` when the
    /// open cut no such entry.
    pub fn cut_at_open(&self) -> Option<(Tid, Tid)> {
        unimplemented!("the write-ahead log of ADR 0056")
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

    /// Return the compression of the records that the store writes: the
    /// `compression` option of its open. `Encoded::encode` with it gives
    /// the payload that the finish writes.
    pub fn compression(&self) -> Compression {
        self.compression
    }

    /// Return the tid of the last committed transaction.
    pub fn last_transaction(&self) -> Tid {
        self.backend.last_tid()
    }

    /// Remove the key `(tid, oid)` of the transaction index and keep the
    /// record. A test uses it to damage the store. Fail with `NotFound`
    /// when the key does not exist. Only the feature `testing` builds it.
    #[cfg(feature = "testing")]
    #[doc(hidden)]
    pub fn remove_index_key_for_test(&self, tid: Tid, oid: Oid) -> Result<()> {
        self.backend.remove_index_key_for_test(tid, oid)
    }

    /// Remove the record of `oid` at `tid` and keep its key in the
    /// transaction index. A test uses it to damage the store. Fail with
    /// `NotFound` when the record does not exist. Only the feature
    /// `testing` builds it.
    #[cfg(feature = "testing")]
    #[doc(hidden)]
    pub fn remove_record_for_test(&self, oid: Oid, tid: Tid) -> Result<()> {
        self.backend.remove_record_for_test(oid, tid)
    }

    /// Tell if no transaction holds or waits for a lock. The method changes
    /// nothing.
    ///
    /// The lock table of a store that the process just opened is empty, so
    /// it holds no stale entry. For this reason, the readiness of
    /// `zoe serve` reads the method once after the open of each store. A
    /// running transaction holds locks, and these locks are not stale. The
    /// lock table lives in the memory of one process, so the method tells
    /// nothing about other processes.
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

    /// Return the path of the blob temporary directory, `blobs/tmp/`.
    ///
    /// A client can write a new blob into a file there and give its path to
    /// `Txn::store_blob_at` or `Txn::restore_blob_at`, which take the file
    /// by rename. The call creates the blob store when it does not exist. A
    /// read-only store fails with `ReadOnly` and creates nothing.
    pub fn blob_temp_dir(&self) -> Result<PathBuf> {
        self.writable()?;
        self.blobs().temp_dir()
    }

    /// Take the file at `path` in the blob temporary directory by rename,
    /// and return the blob, ready for a commit. A read-only store fails with
    /// `ReadOnly`. `BlobStore::take` tells which files it takes.
    pub(crate) fn take_blob_file(&self, path: &Path) -> Result<PendingBlob> {
        self.writable()?;
        self.blobs().take(Arc::downgrade(&self.backend), path)
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

    /// Fail with `MissingBlobDir` when the store has blob records and its
    /// blob directory has no layout file. Then the blob directory of the
    /// open is not the one of the store: the option is missing or wrong, or
    /// the directory is not mounted. The call changes nothing.
    ///
    /// A writer open makes this check itself. A read-only open does not, so
    /// a reader that needs the blob files calls it.
    pub fn check_blob_dir(&self) -> Result<()> {
        self.blobs().check_present(&**self.backend)
    }

    /// Tell if the blob file that `tid` wrote for `oid` exists. This reads
    /// no byte of the file. A missing blob record is `NotFound`.
    pub fn has_blob_file(&self, oid: Oid, tid: Tid) -> Result<bool> {
        self.blob_record(oid, tid)?;
        self.blobs().has_file(oid, tid)
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
        crate::lock(&self.sequencer)
    }

    /// Make the next oid greater than `oid`, so that `new_oid` never gives
    /// an oid that a commit wrote.
    pub(crate) fn raise_next_oid(&self, oid: Oid) {
        self.next_oid
            .fetch_max(oid.0.saturating_add(1), Ordering::Relaxed);
    }
}

/// The text of a compression setting: `none`, `zstd` (zstd at its default
/// level), or `zstd:LEVEL` with a whole level, in any case. `zoe serve`
/// and the in-process storage take it.
impl std::str::FromStr for Compression {
    type Err = String;

    fn from_str(text: &str) -> std::result::Result<Self, String> {
        match text.trim().to_ascii_lowercase().as_str() {
            "none" => Ok(Self::None),
            "zstd" => Ok(Self::default()),
            other => other
                .strip_prefix("zstd:")
                .and_then(|level| level.parse::<i32>().ok())
                .map(|level| Self::Zstd { level })
                .ok_or_else(|| format!("{text} is not none, zstd or zstd:LEVEL")),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicBool;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    #[test]
    fn a_compression_parses_from_none_zstd_or_zstd_and_a_level() {
        for (text, compression) in [
            ("none", Compression::None),
            (" NONE ", Compression::None),
            ("zstd", Compression::default()),
            ("zstd:1", Compression::Zstd { level: 1 }),
            ("ZSTD:-3", Compression::Zstd { level: -3 }),
        ] {
            assert_eq!(text.parse::<Compression>(), Ok(compression), "{text}");
        }
        for text in ["", "lz4", "zstd:", "zstd:high", "zstd 3"] {
            let error = text.parse::<Compression>().unwrap_err();
            assert!(error.contains("none, zstd or zstd:LEVEL"), "{error}");
        }
    }

    use super::*;
    use crate::backend::{BlobRecord, Bytes, Record, RecordMeta, TxnIter, TxnRecords, WriteTxn};
    use crate::format;
    use crate::format::Codec;
    use crate::kv::{Flavour, Swept};
    use crate::testing::{fresh_dir, try_commit as commit};
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
        /// `indexed_tids` fails.
        index: AtomicBool,
        /// The commit of a write transaction writes, then fails.
        landed: AtomicBool,
        /// `current_serials` gives one serial fewer than it got oids.
        short: AtomicBool,
    }

    /// A backend that fails when a test turns a fault on.
    struct Faulty {
        inner: Arc<Opened>,
        faults: Arc<Faults>,
    }

    fn injected() -> ZoeError {
        ZoeError::Backend("injected fault".into())
    }

    /// A write transaction whose commit writes, then fails.
    struct Landed<'a>(Box<dyn WriteTxn + 'a>);

    impl WriteTxn for Landed<'_> {
        fn put_record(&mut self, oid: Oid, tid: Tid, prev: Tid, data: &[u8]) -> Result<()> {
            self.0.put_record(oid, tid, prev, data)
        }

        fn put_blob(&mut self, oid: Oid, tid: Tid, blob: &BlobRecord) -> Result<()> {
            self.0.put_blob(oid, tid, blob)
        }

        fn put_txn(&mut self, meta: &crate::backend::TxnMeta) -> Result<()> {
            self.0.put_txn(meta)
        }

        fn remove_record(&mut self, oid: Oid, tid: Tid) -> Result<()> {
            self.0.remove_record(oid, tid)
        }

        fn remove_blob(&mut self, oid: Oid, tid: Tid) -> Result<()> {
            self.0.remove_blob(oid, tid)
        }

        fn replace_txn(&mut self, meta: &crate::backend::TxnMeta) -> Result<()> {
            self.0.replace_txn(meta)
        }

        fn remove_txn(&mut self, tid: Tid) -> Result<()> {
            self.0.remove_txn(tid)
        }

        fn put_pack_watermark(&mut self, tid: Tid) -> Result<()> {
            self.0.put_pack_watermark(tid)
        }

        fn put_sequence_end(&mut self, name: &str, end: u64) -> Result<()> {
            self.0.put_sequence_end(name, end)
        }

        fn put_kv(&mut self, flavour: Flavour, row: &[u8], payload: &[u8]) -> Result<()> {
            self.0.put_kv(flavour, row, payload)
        }

        fn remove_kv(&mut self, flavour: Flavour, row: &[u8]) -> Result<()> {
            self.0.remove_kv(flavour, row)
        }

        fn put_kv_expiry(&mut self, key: &[u8]) -> Result<()> {
            self.0.put_kv_expiry(key)
        }

        fn sweep_kv(&mut self, now: u64, limit: usize) -> Result<Swept> {
            self.0.sweep_kv(now, limit)
        }

        fn commit(self: Box<Self>) -> Result<()> {
            self.0.commit()?;
            Err(injected())
        }
    }

    impl Backend for Faulty {
        fn load_before(&self, oid: Oid, tid: Tid) -> Result<Option<Record>> {
            if self.faults.load.load(Ordering::Relaxed) {
                return Err(injected());
            }
            self.inner.load_before(oid, tid)
        }

        fn current_serials(&self, oids: &mut dyn Iterator<Item = Oid>) -> Result<Vec<Option<Tid>>> {
            let mut serials: Vec<_> = oids
                .map(|oid| Ok(self.load_before(oid, Tid::MAX)?.map(|record| record.tid)))
                .collect::<Result<_>>()?;
            if self.faults.short.load(Ordering::Relaxed) {
                serials.pop();
            }
            Ok(serials)
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

        fn has_blobs(&self) -> Result<bool> {
            self.inner.has_blobs()
        }

        fn last_tid(&self) -> Tid {
            self.inner.last_tid()
        }

        fn begin_write(&self) -> Result<Box<dyn WriteTxn + '_>> {
            if self.faults.write.load(Ordering::Relaxed) {
                return Err(injected());
            }
            let write = self.inner.begin_write()?;
            if self.faults.landed.load(Ordering::Relaxed) {
                return Ok(Box::new(Landed(write)));
            }
            Ok(write)
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

        fn indexed_tids(&self, from: Tid, to: Tid, limit: usize) -> Result<Vec<Tid>> {
            if self.faults.index.load(Ordering::Relaxed) {
                return Err(injected());
            }
            self.inner.indexed_tids(from, to, limit)
        }

        fn current_from(&self, from: Oid) -> Result<Option<(Oid, Record)>> {
            self.inner.current_from(from)
        }

        fn pack_watermark(&self) -> Result<Tid> {
            self.inner.pack_watermark()
        }

        fn record_keys(&self, from: Oid, limit: usize) -> Result<Vec<(Oid, Tid)>> {
            self.inner.record_keys(from, limit)
        }

        fn txn_records(&self, tid: Tid) -> Result<Option<TxnRecords>> {
            self.inner.txn_records(tid)
        }

        fn sync(&self) -> Result<()> {
            if self.faults.write.load(Ordering::Relaxed) {
                return Err(injected());
            }
            self.inner.sync()
        }

        fn sequence_end(&self, name: &str) -> Result<Option<u64>> {
            if self.faults.load.load(Ordering::Relaxed) {
                return Err(injected());
            }
            self.inner.sequence_end(name)
        }

        fn sequence_names(&self) -> Result<Vec<String>> {
            if self.faults.load.load(Ordering::Relaxed) {
                return Err(injected());
            }
            self.inner.sequence_names()
        }

        fn kv_row(&self, flavour: Flavour, row: &[u8]) -> Result<Option<Bytes>> {
            self.inner.kv_row(flavour, row)
        }

        fn kv_rows(
            &self,
            flavour: Flavour,
            from: std::ops::Bound<&[u8]>,
            to: std::ops::Bound<&[u8]>,
            limit: usize,
        ) -> Result<Vec<(Bytes, Bytes)>> {
            self.inner.kv_rows(flavour, from, to, limit)
        }

        fn kv_expiry_has(&self, key: &[u8]) -> Result<bool> {
            self.inner.kv_expiry_has(key)
        }

        fn has_kv(&self, flavour: Flavour) -> Result<bool> {
            self.inner.has_kv(flavour)
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
            blobs: BlobStore::new(Arc::new(MemVfs::new())),
            staging: StagingArea::new(Arc::new(MemVfs::new()), 0, false),
        });
        (Store { backend, ..store }, faults)
    }

    /// An observer that keeps the count of each `intent_locks_acquired`.
    #[derive(Default)]
    struct Intents(std::sync::Mutex<Vec<usize>>);

    impl crate::Observer for Intents {
        fn intent_locks_acquired(&self, locks: usize, _: std::time::Duration) {
            self.0.lock().unwrap().push(locks);
        }
    }

    #[test]
    fn a_failed_read_of_the_serials_keeps_and_counts_no_intent_lock() {
        let (store, faults) = faulty_store();
        let observed = Arc::new(Intents::default());
        assert!(store.set_observer(observed.clone()));
        let oid = store.new_oid().unwrap();
        let tid = commit(&store, oid, Tid::ZERO, b"a").unwrap();
        let mut intents = crate::IntentLocks::new(&store);
        faults.load.store(true, Ordering::Relaxed);
        assert!(matches!(intents.lock(&[oid]), Err(ZoeError::Backend(_))));
        assert!(intents.is_empty());
        assert!(store.lock_table_is_empty());
        faults.load.store(false, Ordering::Relaxed);
        assert_eq!(intents.lock(&[oid]).unwrap(), vec![tid]);
        assert_eq!(*observed.0.lock().unwrap(), vec![0, 1]);
        intents.release();
        assert!(store.lock_table_is_empty());
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
    fn a_vote_fails_when_the_backend_skips_a_current_record() {
        let (store, faults) = faulty_store();
        let (low, high) = (store.new_oid().unwrap(), store.new_oid().unwrap());
        commit(&store, low, Tid::ZERO, b"a").unwrap();
        commit(&store, high, Tid::ZERO, b"b").unwrap();
        // The serial of `high` is stale, and the backend skips its record.
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.store(low, store.load(low).unwrap().1, b"c").unwrap();
        txn.store(high, Tid::ZERO, b"d").unwrap();
        faults.short.store(true, Ordering::Relaxed);
        let error = txn.vote().unwrap_err();
        assert!(matches!(error, ZoeError::Backend(_)), "{error}");
        assert_eq!(
            error.to_string(),
            "backend error: the backend gave 1 current serials for 2 oids"
        );
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

    /// Commit five transactions of one object. Return their tids.
    fn five_commits(store: &Store) -> Vec<Tid> {
        let oid = store.new_oid().unwrap();
        let mut tids = vec![commit(store, oid, Tid::ZERO, b"a").unwrap()];
        for data in [b"b", b"c", b"d", b"e"] {
            let serial = *tids.last().unwrap();
            tids.push(commit(store, oid, serial, data).unwrap());
        }
        tids
    }

    /// Iterate over every transaction of `store`, with slices of `slice`
    /// or, for `None`, the slices of `Store::iterator`. The scan of the
    /// index fails in each call after the iterator gave `before` items.
    /// Check that the iterator gives the transactions of `tids` before the
    /// fault, the error once, the transactions after it, then the end.
    fn one_error_for_a_failed_index_scan(
        store: &Store,
        faults: &Faults,
        tids: &[Tid],
        slice: Option<usize>,
        before: usize,
    ) {
        faults.index.store(false, Ordering::Relaxed);
        let mut iterator = match slice {
            Some(slice) => store.iterator_in_slices(None, None, slice),
            None => store.iterator(None, None).unwrap(),
        };
        let mut given = Vec::new();
        loop {
            faults.index.store(given.len() >= before, Ordering::Relaxed);
            let Some(txn) = iterator.next() else {
                break;
            };
            given.push(txn.map(|txn| txn.tid).map_err(|e| e.to_string()));
        }
        assert!(iterator.next().is_none());
        let mut expected: Vec<_> = tids.iter().map(|&tid| Ok(tid)).collect();
        expected.insert(before, Err(injected().to_string()));
        assert_eq!(given, expected, "slices of {slice:?}, fault after {before}");
    }

    #[test]
    fn a_failed_index_scan_gives_one_iterator_error_and_the_rest_of_the_range() {
        let (store, faults) = faulty_store();
        let tids = five_commits(&store);
        // The first index scan fails, so the error comes first.
        one_error_for_a_failed_index_scan(&store, &faults, &tids, None, 0);
        one_error_for_a_failed_index_scan(&store, &faults, &tids, Some(1), 0);
    }

    #[test]
    fn a_failed_scan_of_a_later_index_slice_gives_the_rows_around_its_error() {
        let (store, faults) = faulty_store();
        let tids = five_commits(&store);
        // The scan of the second index slice fails, or of a later one.
        one_error_for_a_failed_index_scan(&store, &faults, &tids, Some(1), 1);
        one_error_for_a_failed_index_scan(&store, &faults, &tids, Some(1), 3);
        one_error_for_a_failed_index_scan(&store, &faults, &tids, Some(2), 2);
        one_error_for_a_failed_index_scan(&store, &faults, &tids, Some(2), 4);
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
    fn a_failed_sync_halts_the_store() {
        let (store, faults) = faulty_store();
        let oid = store.new_oid().unwrap();
        let first = commit(&store, oid, Tid::ZERO, b"a").unwrap();
        faults.write.store(true, Ordering::Relaxed);
        assert!(matches!(store.sync(), Err(ZoeError::Backend(_))));
        faults.write.store(false, Ordering::Relaxed);

        assert!(matches!(store.sync(), Err(ZoeError::Halted)));
        let error = commit(&store, oid, first, b"b");
        assert!(matches!(error, Err(ZoeError::Halted)), "{error:?}");
        assert_eq!(store.last_transaction(), first);
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
        let error = store.pack_dry_run(Tid::MAX);
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
            let frame = format::encode_stored_frame(
                format::KeySpace::Records,
                &key,
                format::CODEC_ZSTD,
                payload,
            );
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
    fn a_reservation_that_fails_before_its_write_loses_no_value() {
        let (store, faults) = faulty_store();
        let sequence = store.sequence("intids");
        // The first use reads the stored end, then the reservation writes.
        for fault in [&faults.load, &faults.write] {
            fault.store(true, Ordering::Relaxed);
            let error = sequence.next();
            assert!(matches!(error, Err(ZoeError::Backend(_))), "{error:?}");
            fault.store(false, Ordering::Relaxed);
        }
        let values: Vec<u64> = (0..101).map(|_| sequence.next().unwrap()).collect();
        assert_eq!(values, (1..=101).collect::<Vec<_>>());

        // A reservation that fails at the block end keeps the next value.
        for _ in 102..=200 {
            sequence.next().unwrap();
        }
        faults.write.store(true, Ordering::Relaxed);
        assert!(sequence.next().is_err());
        faults.write.store(false, Ordering::Relaxed);
        assert_eq!(sequence.next().unwrap(), 201);
    }

    #[test]
    fn a_reservation_that_fails_after_its_write_loses_no_value() {
        let (store, faults) = faulty_store();
        let sequence = store.sequence("intids");
        faults.landed.store(true, Ordering::Relaxed);
        for _ in 0..2 {
            assert!(matches!(sequence.next(), Err(ZoeError::Backend(_))));
        }
        faults.landed.store(false, Ordering::Relaxed);
        // The retries wrote the same end, so a crash now skips at most one
        // block.
        assert_eq!(store.backend.sequence_end("intids").unwrap(), Some(101));
        assert_eq!(sequence.next().unwrap(), 1);
        assert_eq!(store.backend.sequence_end("intids").unwrap(), Some(101));
    }

    #[test]
    fn a_second_process_cannot_open_a_locked_store() {
        let dir = fresh_dir("locked");
        let _store = Store::open(&dir).unwrap();
        assert!(matches!(Store::open(&dir), Err(ZoeError::Locked)));
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
        assert!(matches!(
            store.tpc_begin_restore_deferred(Tid(tid.0 + 1), b"", b"", b""),
            Err(ZoeError::ReadOnly)
        ));
        assert!(matches!(store.sync(), Err(ZoeError::ReadOnly)));
        assert!(matches!(
            store.sequence("intids").next(),
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
    fn only_the_open_that_repairs_a_store_says_that_it_repaired_it() {
        let open = fresh_dir("repaired-open");
        let copy = fresh_dir("repaired-copy");
        let (oid, _) = write_store(&open);
        let store = Store::open(&open).unwrap();
        assert!(!store.repaired());
        std::fs::copy(open.join(DATABASE), copy.join(DATABASE)).unwrap();
        drop(store);
        assert!(!Store::open_read_only(&open).unwrap().repaired());
        let clean = Store::open_read_only_repairing(&open, Options::default()).unwrap();
        assert!(!clean.repaired());
        drop(clean);

        let store = Store::open_read_only_repairing(&copy, Options::default()).unwrap();
        assert!(store.repaired());
        assert_eq!(store.load(oid).unwrap().0, b"a");
        drop(store);
        let store = Store::open_read_only_repairing(&copy, Options::default()).unwrap();
        assert!(!store.repaired());
    }

    #[test]
    fn a_failed_repair_gives_the_error_of_the_system_and_creates_no_file() {
        let system = io::Error::from_raw_os_error(libc::EROFS);
        let error = repair_failed(redb::DatabaseError::Storage(redb::StorageError::Io(system)));
        assert!(matches!(error, ZoeError::RepairFailed(_)), "{error:?}");
        assert_eq!(
            error.to_string(),
            "the store was not closed cleanly, and its repair failed: \
             Read-only file system (os error 30)"
        );

        // The open for writes of the repair creates no database file.
        let dir = fresh_dir("repair-no-file");
        let result = repair_database(&dir.join(DATABASE), None);
        let refused = |e: &ZoeError| matches!(e, ZoeError::RepairFailed(s) if s.to_string().contains("os error 2"));
        assert!(result.as_ref().is_err_and(refused), "{result:?}");
        assert!(files(&dir).is_empty());
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

    /// Return every entry under `dir`, by path: the bytes of a file, `None`
    /// for a directory.
    fn tree(dir: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
        let mut entries = BTreeMap::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path.clone());
                    entries.insert(path, None);
                } else {
                    let bytes = std::fs::read(&path).unwrap();
                    entries.insert(path, Some(bytes));
                }
            }
        }
        entries
    }

    #[test]
    fn a_read_only_open_reads_a_blob_directory_elsewhere_and_creates_nothing() {
        let root = fresh_dir("read-only-blob-dir");
        let (dir, blobs) = (root.join("store"), root.join("blobs-elsewhere"));
        let with = |blob_dir: &Path| Options {
            blob_dir: Some(blob_dir.to_path_buf()),
            ..Options::default()
        };
        let (oid, tid) = {
            let store = Store::open_with(&dir, with(&blobs)).unwrap();
            let mut file = store.new_blob_file().unwrap();
            io::Write::write_all(&mut file, b"bytes").unwrap();
            let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
            txn.store_blob(Oid(1), Tid::ZERO, b"a", file).unwrap();
            txn.vote().unwrap();
            (Oid(1), txn.finish().unwrap())
        };
        let before = tree(&root);

        let store = Store::open_read_only_with(&dir, with(&blobs)).unwrap();
        store.check_blob_dir().unwrap();
        store.verify_blob(oid, tid).unwrap();
        assert!(store.has_blob_file(oid, tid).unwrap());
        assert!(store.blob_path(oid, tid).unwrap().starts_with(&blobs));
        assert!(matches!(store.new_blob_file(), Err(ZoeError::ReadOnly)));
        drop(store);

        // The default blob directory, an empty one and one whose parent
        // does not exist are not the blob directory of the store.
        let empty = root.join("empty");
        std::fs::create_dir(&empty).unwrap();
        let before = {
            let mut before = before;
            before.insert(empty.clone(), None);
            before
        };
        let wrong = [
            None,
            Some(empty.clone()),
            Some(root.join("no").join("blobs")),
        ];
        for blob_dir in wrong {
            let expected = blob_dir.clone().unwrap_or_else(|| dir.join("blobs"));
            let options = Options {
                blob_dir,
                ..Options::default()
            };
            let store = Store::open_read_only_with(&dir, options).unwrap();
            match store.check_blob_dir() {
                Err(ZoeError::MissingBlobDir { path }) => assert_eq!(path, expected),
                other => panic!("{other:?}"),
            }
            assert!(!store.has_blob_file(oid, tid).unwrap());
        }
        assert_eq!(tree(&root), before);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_cache_size_bounds_the_page_cache_of_the_database() {
        let vfs = MemVfs::new();
        {
            let store = Store::open_vfs(vfs.clone()).unwrap();
            let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
            txn.store(Oid(1), Tid::ZERO, &[7; 4096]).unwrap();
            txn.vote().unwrap();
            txn.finish().unwrap();
        }
        // Return the bytes that a second load of the same record reads
        // from the file, after an open with `cache_size`.
        let second_load = |cache_size| {
            let options = Options {
                cache_size,
                ..Options::default()
            };
            let store = Store::open_full(vfs.clone(), SystemClock, options).unwrap();
            store.load(Oid(1)).unwrap();
            let before = vfs.bytes_read();
            store.load(Oid(1)).unwrap();
            vfs.bytes_read() - before
        };
        assert_eq!(Options::default().cache_size, None);
        assert_eq!(second_load(None), 0);
        assert_eq!(second_load(Some(1 << 20)), 0);
        assert!(second_load(Some(0)) > 0);
    }
}
