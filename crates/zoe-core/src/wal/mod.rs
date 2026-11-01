//! The logged backend: a write-ahead log with a background fold over the
//! redb backend (ADR 0056).
//!
//! A finish appends its commit batch to a segment of `wal/` and syncs that
//! segment alone. The batch goes into the unfolded table, which each read
//! reads before the tables of redb. The fold thread writes the commits of
//! the backlog into the tables later, in slices, each a durable backend
//! write. A batch of mostly large records, a batch larger than half of the
//! bound of the backlog and a batch of deferred restores go into the tables
//! directly.
//!
//! `segments` holds the files of the log, `table` the unfolded table,
//! `replay` the reading of the log at an open, `fold` the fold, and `read`
//! the read paths of the backend.

mod fold;
mod read;
mod replay;
mod segments;
mod table;

use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, RwLock, RwLockReadGuard};
use std::time::Duration;

use crate::backend::{Backend, BatchPlan, BlobRecord, NewPayload, Payload, TxnMeta, WriteTxn};
use crate::clock::{Clock, Waiter};
use crate::commit::Observer;
use crate::error::{Result, ZoeError};
use crate::format::wal::{self, EntryBuilder, FileName, OpKind, SegmentHeader, SegmentState};
use crate::format::{self, Codec, Compression, KeySpace};
use crate::kv::{Flavour, Swept};
use crate::redb::RedbBackend;
use crate::store::WriteLog;
use crate::types::{Oid, Tid};
use crate::vfs::Dir;

use replay::Kind;
use segments::{Opened, Seg, Segments, SyncMark};
use table::{Counters, LogEntry, Table};

/// The name of the fold thread.
const FOLD_THREAD: &str = "zoe-fold";

/// The requests to the fold and the waiters of its progress.
struct Requests {
    /// Fold each commit up to this tid.
    target: Tid,
    /// The close stops the fold thread.
    stop: bool,
    /// The waiters that each slice wakes.
    waiters: Vec<Arc<dyn Waiter>>,
}

/// The end of the fold thread: the close waits for it on a waiter.
struct Ended {
    done: AtomicBool,
    waiter: Arc<dyn Waiter>,
}

/// The state that the store and the fold thread share. The fold thread
/// holds it and never the store, so the drop of the store runs.
pub(super) struct Core {
    redb: RedbBackend,
    settings: WriteLog,
    clock: Arc<dyn Clock>,
    observer: OnceLock<Arc<dyn Observer>>,
    table: RwLock<Table>,
    counters: Counters,
    /// The published tid P: the last tid whose commit the table holds
    /// whole, or that a direct batch wrote.
    published: AtomicU64,
    segments: Segments,
    mark: SyncMark,
    /// The waiter of the fold thread.
    fold_waiter: Arc<dyn Waiter>,
    requests: Mutex<Requests>,
    /// The fold failed: the store halts.
    failed: AtomicBool,
    /// A slice runs.
    slicing: AtomicBool,
}

impl Core {
    /// Return the published tid.
    fn published(&self) -> Tid {
        Tid(self.published.load(Ordering::Acquire))
    }

    /// Tell if the backlog holds no commit.
    fn empty(&self) -> bool {
        self.counters.commits.load(Ordering::Acquire) == 0
    }

    /// Return the bytes of the backlog.
    fn backlog_bytes(&self) -> u64 {
        self.counters.bytes.load(Ordering::Acquire)
    }

    fn read_table(&self) -> RwLockReadGuard<'_, Table> {
        self.table.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn requests(&self) -> MutexGuard<'_, Requests> {
        crate::lock(&self.requests)
    }

    /// Ask the fold to fold each commit up to `tid`, and wake it.
    fn request(&self, tid: Tid) {
        {
            let mut requests = self.requests();
            requests.target = requests.target.max(tid);
        }
        self.fold_waiter.wake();
    }

    /// Wake each waiter of the progress of the fold.
    fn wake_waiters(&self) {
        let waiters = std::mem::take(&mut self.requests().waiters);
        for waiter in waiters {
            waiter.wake();
        }
    }

    /// Mark the fold failed, log `error`, and wake each waiter. The store
    /// halts at the next lock of the commit sequencer.
    fn fail(&self, error: &dyn std::fmt::Display) {
        self.failed.store(true, Ordering::Release);
        tracing::warn!(%error, "the fold of the write-ahead log failed: the store halts");
        self.wake_waiters();
    }

    /// Ask the fold to fold each commit up to `tid`, and wait until `done`
    /// is true, on a waiter of the store clock. A failed fold gives
    /// `Halted`. Return the time of the wait, or `None` when `done` was
    /// true at once.
    fn wait_for(&self, tid: Tid, done: impl Fn(&Self) -> bool) -> Result<Option<Duration>> {
        if done(self) {
            return Ok(None);
        }
        let started = self.clock.monotonic();
        let waiter = self.clock.waiter();
        loop {
            if self.failed.load(Ordering::Acquire) {
                return Err(ZoeError::Halted);
            }
            {
                let mut requests = self.requests();
                requests.target = requests.target.max(tid);
                requests.waiters.push(Arc::clone(&waiter));
            }
            self.fold_waiter.wake();
            // A wake after the push and before the wait is not lost.
            if done(self) {
                break;
            }
            if self.failed.load(Ordering::Acquire) {
                return Err(ZoeError::Halted);
            }
            waiter.wait(Duration::MAX);
            if done(self) {
                break;
            }
        }
        Ok(Some(self.clock.monotonic().saturating_sub(started)))
    }

    /// Tell if no commit up to `tid` is left in the table, and no slice
    /// runs.
    fn folded_to(&self, tid: Tid) -> bool {
        !self.slicing.load(Ordering::Acquire)
            && (self.empty() || self.read_table().front().is_none_or(|front| front > tid))
    }

    /// Give the time of a wait of a leader for the fold to the observer.
    fn waited(&self, wait: Option<Duration>) {
        if let (Some(wait), Some(observer)) = (wait, self.observer.get()) {
            observer.backlog_waited(wait);
        }
    }

    /// Insert the commits `commits` of `entry`, an entry of the log that is
    /// durable, into the table and the backlog, and count `new_oids` new
    /// oids. Then store the published tid.
    fn publish(&self, entry: LogEntry, commits: &[wal::Commit], new_oids: u64) {
        let Some(last) = commits.last().map(|commit| commit.tid) else {
            return;
        };
        let now = self.clock.monotonic();
        {
            let mut table = self.table.write().unwrap_or_else(PoisonError::into_inner);
            table.publish(Arc::new(entry), commits, now, &self.counters);
            self.redb.add_objects(new_oids);
        }
        self.published.fetch_max(last.0, Ordering::AcqRel);
    }
}

/// The logged backend of a writable store.
pub(crate) struct Logged {
    core: Arc<Core>,
    ended: Arc<Ended>,
    /// The next oid that no record of the store or of the log uses.
    next_oid: Oid,
    /// The cut entry of the open.
    cut: Option<(Tid, Tid)>,
    /// The close ran.
    closed: AtomicBool,
}

/// Return the records of `commits`, commits of `entry`, whose previous
/// serial is `Tid::ZERO`: the new oids that they add to the object count.
fn new_oids(entry: &LogEntry, commits: &[wal::Commit]) -> u64 {
    commits
        .iter()
        .flat_map(|commit| &entry.ops[commit.ops.clone()])
        .filter(|op| op.kind == OpKind::Record && format::frame_prev(entry.frame(op)) == Tid::ZERO)
        .count() as u64
}

/// Check that the settings of the log hold together. Else fail with an
/// `InvalidInput` I/O error.
pub(crate) fn check_settings(settings: &WriteLog) -> Result<()> {
    let invalid = |message: &str| {
        let message = format!("the settings of the write-ahead log: {message}");
        Err(ZoeError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            message,
        )))
    };
    if settings.segment_bytes < 4096 {
        return invalid("segment_bytes is below 4,096");
    }
    if settings.fold_bytes == 0 {
        return invalid("fold_bytes is 0");
    }
    if settings.slice_bytes == 0 {
        return invalid("slice_bytes is 0");
    }
    if settings.backlog_bytes < settings.fold_bytes {
        return invalid("backlog_bytes is below fold_bytes");
    }
    Ok(())
}

impl Logged {
    /// Open the log of the store in `dir` over `redb`, the tables, whose
    /// format the caller checked (ADR 0056, section 11). Create `wal/` when
    /// it is missing, remove the `.new` files that a crash left, replay the
    /// commits after the folded tid, prepare a spare when none is ready and
    /// start the fold thread.
    pub(crate) fn open(
        redb: RedbBackend,
        dir: Arc<dyn Dir>,
        clock: Arc<dyn Clock>,
        settings: WriteLog,
    ) -> Result<Self> {
        let folded = redb.last_tid();
        if !dir.exists(wal::DIR)? {
            dir.create_dir(wal::DIR)?;
            dir.sync_dir_at("")?;
        }
        let mut next_number = 0;
        let mut files: Vec<(u64, Box<dyn redb::StorageBackend>)> = Vec::new();
        for name in dir.list_files(wal::DIR)? {
            match wal::parse_name(&name) {
                Some(FileName::New(number)) => {
                    next_number = next_number.max(number.saturating_add(1));
                    dir.remove(&wal::new_name(number))?;
                }
                Some(FileName::Segment(number)) => {
                    next_number = next_number.max(number.saturating_add(1));
                    files.push((number, dir.open_log(&wal::segment_name(number))?));
                }
                None => {}
            }
        }
        let replayed = replay::replay(files, folded, true)?;
        if let Some((previous, last)) = replayed.cut {
            tracing::error!(
                previous = previous.0,
                last = last.0,
                "the open cut the last entry of the write-ahead log: its header passed and its \
                 body did not; the commits after the previous tid up to the last tid are lost"
            );
        }

        let mut table = Table::default();
        let counters = Counters::default();
        let now = clock.monotonic();
        // The fold starts at once with the replayed commits whose tid is
        // older than `fold_age`: the open asks for their fold.
        let aged = clock
            .now()
            .checked_sub(settings.fold_age)
            .map_or(Tid::ZERO, crate::clock::timestamp);
        let mut target = Tid::ZERO;
        let mut max_oid: Option<Oid> = None;
        let mut new = 0;
        for (entry, commits) in replayed.entries {
            for commit in &commits {
                if commit.tid <= aged {
                    target = target.max(commit.tid);
                }
            }
            new += new_oids(&entry, &commits);
            for commit in &commits {
                for op in &entry.ops[commit.ops.clone()] {
                    if matches!(op.kind, OpKind::Record | OpKind::Blob) {
                        max_oid = max_oid.max(Some(op.oid));
                    }
                }
            }
            table.publish(Arc::new(entry), &commits, now, &counters);
        }
        redb.add_objects(new);
        let stored_next = redb.next_oid()?;
        let next_oid = match max_oid {
            Some(oid) => stored_next.max(Oid(oid.0.saturating_add(1))),
            None => stored_next,
        };

        let found = replayed
            .files
            .into_iter()
            .map(|found| {
                let seg = Seg::new(found.number, found.file, found.len);
                match found.kind {
                    Kind::Spare(epoch) => Opened::Ready(
                        seg,
                        SegmentHeader {
                            state: SegmentState::Spare,
                            epoch,
                        },
                    ),
                    Kind::InUse {
                        epoch,
                        damaged,
                        last,
                        unfolded,
                    } => {
                        if unfolded {
                            Opened::Used(seg, epoch, last, damaged)
                        } else if damaged {
                            Opened::Doomed(seg)
                        } else {
                            Opened::Ready(
                                seg,
                                SegmentHeader {
                                    state: SegmentState::InUse,
                                    epoch,
                                },
                            )
                        }
                    }
                    Kind::Damaged => Opened::Doomed(seg),
                }
            })
            .collect();
        let segments = Segments::new(
            dir,
            settings.segment_bytes,
            next_number,
            replayed.max_epoch,
            found,
        );
        let fold_waiter = clock.waiter();
        let core = Arc::new(Core {
            redb,
            settings,
            observer: OnceLock::new(),
            table: RwLock::new(table),
            counters,
            published: AtomicU64::new(replayed.last.0),
            segments,
            mark: SyncMark::new(Arc::clone(&fold_waiter)),
            fold_waiter,
            requests: Mutex::new(Requests {
                target,
                stop: false,
                waiters: Vec::new(),
            }),
            failed: AtomicBool::new(false),
            slicing: AtomicBool::new(false),
            clock,
        });
        if !core.segments.has_spare() {
            core.segments.prepare_spare()?;
        }
        let ended = Arc::new(Ended {
            done: AtomicBool::new(false),
            waiter: core.clock.waiter(),
        });
        let (thread_core, thread_ended) = (Arc::clone(&core), Arc::clone(&ended));
        core.clock.spawn(
            FOLD_THREAD,
            Box::new(move || fold::run(thread_core, thread_ended)),
        )?;
        Ok(Self {
            core,
            ended,
            next_oid,
            cut: replayed.cut,
            closed: AtomicBool::new(false),
        })
    }

    /// Return the next oid that no record of the store or of the log uses.
    pub(crate) fn next_oid(&self) -> Oid {
        self.next_oid
    }

    /// Stop the fold thread after its slice, and wait on a waiter of the
    /// store clock until it ended and dropped its references.
    fn stop(&self) {
        self.core.requests().stop = true;
        self.core.fold_waiter.wake();
        while !self.ended.done.load(Ordering::Acquire) {
            self.ended.waiter.wait(Duration::MAX);
        }
    }

    /// Fold the whole backlog on the calling thread, slice by slice, then
    /// write the state "spare" into each header that says "in use", and
    /// remove the spares beyond two (ADR 0056, section 13).
    fn fold_all(&self) -> Result<()> {
        let core = &self.core;
        while !core.empty() {
            if fold::slice(core, &*core.fold_waiter)?.is_none() {
                break;
            }
        }
        core.segments.close(core.published())?;
        Ok(())
    }
}

impl Drop for Logged {
    fn drop(&mut self) {
        Backend::close(self, false);
    }
}

/// Read the log of the store in `dir` for a read-only open, through
/// positional reads of `std::fs`, after the folded tid `folded` (ADR 0056,
/// section 14). A missing `wal/` is an empty log. Return true when the log
/// holds a commit after the folded tid. A damaged log gives `CorruptTxn`
/// and a log of another version `Format`.
pub(crate) fn read_only_check(dir: &Path, folded: Tid) -> Result<bool> {
    let files = read_only_files(dir)?;
    Ok(replay::replay(files, folded, false)?.unfolded)
}

/// Open the segment files of the store in `dir` read-only.
fn read_only_files(dir: &Path) -> Result<Vec<(u64, std::fs::File)>> {
    let wal_dir = dir.join(wal::DIR);
    let names = match crate::vfs::list_dir(&wal_dir, false) {
        Ok(names) => names,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut files = Vec::new();
    for name in names {
        if let Some(FileName::Segment(number)) = wal::parse_name(&name) {
            files.push((number, std::fs::File::open(wal_dir.join(&name))?));
        }
    }
    Ok(files)
}

/// Fold the log of the store in `dir` into `redb`, the tables of the store
/// open for writes, for the repair of the open of the tools (ADR 0056,
/// section 14). Read the log through `std::fs` and write only the tables,
/// slice by slice, each with a sync. A damaged log gives `CorruptTxn` and
/// folds nothing.
pub(crate) fn fold_into(dir: &Path, redb: &RedbBackend, slice_bytes: u64) -> Result<()> {
    let files = read_only_files(dir)?;
    let replayed = replay::replay(files, redb.last_tid(), true)?;
    let mut table = Table::default();
    let counters = Counters::default();
    for (entry, commits) in replayed.entries {
        table.publish(Arc::new(entry), &commits, Duration::ZERO, &counters);
    }
    loop {
        let picked = table.pick(Tid::MAX, slice_bytes);
        if picked.is_empty() {
            return Ok(());
        }
        fold::check(&picked)?;
        let mut write = redb.begin_write()?;
        fold::write_commits(&mut *write, &picked)?;
        write.commit()?;
        table.remove_front(picked.len(), &counters);
    }
}

/// The write of a commit batch on the log path: it encodes the ops of the
/// batch into one entry, then appends the entry to the log, syncs it and
/// publishes the batch.
struct LogWrite<'a> {
    core: &'a Core,
    builder: EntryBuilder,
    /// The last tid of the store before the batch.
    previous: Tid,
    compression: Compression,
    /// The records with the previous serial `Tid::ZERO`: new oids.
    new_oids: u64,
    /// A batch of large records that found a backlog asks for a fold.
    fold_after: bool,
}

/// Return the error of an op that the format refuses.
fn refused(refused: wal::Refused) -> ZoeError {
    ZoeError::Backend(format!("a commit batch breaks the format of the log: {}", refused.0).into())
}

/// Return the error of a write that the log path does not take.
fn not_on_the_log(what: &str) -> ZoeError {
    ZoeError::Backend(format!("{what} is not a write of a commit batch").into())
}

impl WriteTxn for LogWrite<'_> {
    fn put_record(&mut self, oid: Oid, tid: Tid, prev: Tid, data: &[u8]) -> Result<()> {
        let payload = Payload::Pickle(std::borrow::Cow::Borrowed(data));
        self.put_payloads(tid, &mut std::iter::once(Ok((oid, prev, payload))))
    }

    fn put_payloads(
        &mut self,
        tid: Tid,
        records: &mut dyn Iterator<Item = Result<NewPayload<'_>>>,
    ) -> Result<()> {
        let compression = self.compression;
        for record in records {
            let (oid, prev, payload) = record?;
            let key = format::record_key(oid, tid);
            self.builder
                .record(OpKind::Record, oid, tid, |out| match &payload {
                    Payload::Pickle(data) => format::append_frame_with(
                        out,
                        KeySpace::Records,
                        &key,
                        prev,
                        data,
                        compression,
                    ),
                    Payload::Encoded(codec, stored) => format::append_frame_payload(
                        out,
                        KeySpace::Records,
                        &key,
                        format::MAIN_BRANCH,
                        prev,
                        *codec,
                        stored,
                    ),
                })
                .map_err(refused)?;
            if prev == Tid::ZERO {
                self.new_oids += 1;
            }
        }
        Ok(())
    }

    fn put_blob(&mut self, oid: Oid, tid: Tid, blob: &BlobRecord) -> Result<()> {
        let key = format::record_key(oid, tid);
        let payload = format::blob::encode_blob(blob);
        self.builder
            .record(OpKind::Blob, oid, tid, |out| {
                format::append_frame_payload(
                    out,
                    KeySpace::Blobs,
                    &key,
                    format::MAIN_BRANCH,
                    Tid::ZERO,
                    Codec::None,
                    &payload,
                );
            })
            .map_err(refused)
    }

    fn put_txn(&mut self, meta: &TxnMeta) -> Result<()> {
        let key = format::txn_key(meta.tid);
        let payload = format::encode_txn(meta);
        self.builder
            .txn(meta.tid, |out| {
                format::append_frame_payload(
                    out,
                    KeySpace::Txns,
                    &key,
                    format::MAIN_BRANCH,
                    Tid::ZERO,
                    Codec::None,
                    &payload,
                );
            })
            .map_err(refused)
    }

    fn remove_record(&mut self, _: Oid, _: Tid) -> Result<()> {
        Err(not_on_the_log("the removal of a record"))
    }

    fn remove_blob(&mut self, _: Oid, _: Tid) -> Result<()> {
        Err(not_on_the_log("the removal of a blob record"))
    }

    fn replace_txn(&mut self, _: &TxnMeta) -> Result<()> {
        Err(not_on_the_log("a new value of a transaction row"))
    }

    fn remove_txn(&mut self, _: Tid) -> Result<()> {
        Err(not_on_the_log("the removal of a transaction row"))
    }

    fn put_pack_watermark(&mut self, _: Tid) -> Result<()> {
        Err(not_on_the_log("the pack watermark"))
    }

    fn put_sequence_end(&mut self, _: &str, _: u64) -> Result<()> {
        Err(not_on_the_log("a sequence row"))
    }

    fn put_kv(&mut self, flavour: Flavour, row: &[u8], payload: &[u8]) -> Result<()> {
        self.builder
            .kv(flavour, row, |out| {
                format::append_frame_payload(
                    out,
                    flavour.space(),
                    row,
                    format::MAIN_BRANCH,
                    Tid::ZERO,
                    Codec::None,
                    payload,
                );
            })
            .map_err(refused)
    }

    fn remove_kv(&mut self, flavour: Flavour, row: &[u8]) -> Result<()> {
        self.builder.removal(flavour, row).map_err(refused)
    }

    fn put_kv_expiry(&mut self, key: &[u8]) -> Result<()> {
        self.builder.expiry(key).map_err(refused)
    }

    fn sweep_kv(&mut self, _: u64, _: usize) -> Result<Swept> {
        Err(not_on_the_log("a sweep step"))
    }

    /// Append the entry to the log with one write and one sync, then
    /// publish the batch (ADR 0056, section 5). An error leaves the table
    /// without any member of the batch.
    fn commit(self: Box<Self>) -> Result<()> {
        let core = self.core;
        let mut entry = self.builder.finish();
        if entry.commits.is_empty() {
            return Ok(());
        }
        let hints = core
            .segments
            .append(&mut entry, self.previous, &core.mark, &*core.clock)?;
        let last = entry.last();
        let wal::Entry {
            bytes,
            ops,
            commits,
        } = entry;
        let before = core.backlog_bytes();
        core.publish(LogEntry { bytes, ops }, &commits, self.new_oids);
        let after = core.backlog_bytes();
        let fold_bytes = core.settings.fold_bytes;
        if self.fold_after {
            core.request(last);
        } else if hints.housekeeping || (before < fold_bytes && after >= fold_bytes) {
            core.fold_waiter.wake();
        }
        Ok(())
    }
}

impl Logged {
    /// Return the write of a commit batch (ADR 0056, sections 4 and 10).
    fn batch(&self, plan: &BatchPlan) -> Result<Box<dyn WriteTxn + '_>> {
        let core = &*self.core;
        if core.failed.load(Ordering::Acquire) {
            return Err(ZoeError::Halted);
        }
        let settings = &core.settings;
        let direct = !settings.enabled
            || plan.deferred
            || core.redb.unsynced()
            || plan.estimate > settings.backlog_bytes / 2;
        if direct {
            // A direct batch needs an empty backlog, so that the tables hold
            // each commit before it.
            let waited = core.wait_for(core.published(), Core::empty)?;
            core.waited(waited);
            return core.redb.begin_write();
        }
        let large = plan.large_bytes > 0 && plan.large_bytes.saturating_mul(2) >= plan.pickle_bytes;
        if large && core.empty() {
            return core.redb.begin_write();
        }
        let bound = settings.backlog_bytes;
        let estimate = plan.estimate;
        let waited = core.wait_for(core.published(), |core| {
            core.empty() || core.backlog_bytes().saturating_add(estimate) <= bound
        })?;
        core.waited(waited);
        let capacity = usize::try_from(estimate).unwrap_or(usize::MAX).min(1 << 30);
        Ok(Box::new(LogWrite {
            core,
            builder: EntryBuilder::new(capacity),
            previous: plan.previous,
            compression: core.redb.compression(),
            new_oids: 0,
            fold_after: large,
        }))
    }
}
