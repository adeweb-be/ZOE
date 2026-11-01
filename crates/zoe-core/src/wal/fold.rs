//! The fold (ADR 0056, section 9): the thread that writes the commits of
//! the backlog into the tables, in slices.
//!
//! The fold starts when the backlog reaches `fold_bytes`, when its oldest
//! commit reaches `fold_age`, or on a request. A slice takes the oldest
//! commits at or before the published tid, whole commits, up to
//! `slice_bytes`. It checks each frame, writes the frames as they are in
//! one backend write, waits for the end of a log sync that runs, and
//! commits with a sync. Then its commits leave the table, the segments
//! whose entries are all folded retire, and the waiters wake. The fold
//! also prepares the spares and grows the current segment.
//!
//! The fold never takes the commit sequencer, and holds no lock across a
//! write, a sync or a wait. A failed slice, or a panic of the thread, marks
//! the fold failed: the store halts. The observer gets each slice before
//! the waiters wake, and a panic of the observer does not reach that guard.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::backend::{Backend, Framed, WriteTxn};
use crate::clock::Waiter;
use crate::error::{Result, ZoeError};
use crate::format::wal::OpKind;
use crate::format::{self, KeySpace};
use crate::kv::Flavour;

use super::table::{LogEntry, Picked};
use super::{Core, Ended};

/// The wait of the fold when the backlog holds only commits that the
/// leader did not publish yet.
const UNPUBLISHED: Duration = Duration::from_millis(1);

/// What one slice did, for the observer.
pub(super) struct Report {
    commits: usize,
    bytes: u64,
    duration: Duration,
}

/// Run the fold thread on `core` until the close stops it or the fold
/// fails. Then drop `core` and tell `ended`.
pub(super) fn run(core: Arc<Core>, ended: Arc<Ended>) {
    // The signal drops last, also during a panic: the thread drops its
    // reference to the core before the close wakes.
    let _signal = Signal(ended);
    body(core);
}

/// The end of the fold thread.
struct Signal(Arc<Ended>);

impl Drop for Signal {
    fn drop(&mut self) {
        self.0.done.store(true, Ordering::Release);
        self.0.waiter.wake();
    }
}

/// Marks the fold failed when the thread unwinds a panic.
struct Failing<'c> {
    core: &'c Core,
    armed: bool,
}

impl Drop for Failing<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.core.fail(&"the fold thread panicked");
        }
    }
}

/// What the fold does next.
enum Step {
    Folded,
    Wait(Duration),
}

fn body(core: Arc<Core>) {
    let waiter = Arc::clone(&core.fold_waiter);
    loop {
        if core.requests().stop || core.failed.load(Ordering::Acquire) {
            return;
        }
        let mut guard = Failing {
            core: &core,
            armed: true,
        };
        let step = step(&core, &*waiter);
        guard.armed = false;
        drop(guard);
        match step {
            Err(error) => {
                core.fail(&error);
                return;
            }
            Ok(Step::Folded) => {}
            Ok(Step::Wait(timeout)) => waiter.wait(timeout),
        }
    }
}

/// Give a slice to the observer. A panic of the observer stays here: it
/// does not reach the guard of the fold and does not stop the fold.
fn observe(core: &Core, report: &Report) {
    if let Some(observer) = core.observer.get() {
        let _ = catch_unwind(AssertUnwindSafe(|| {
            observer.slice_folded(report.commits, report.bytes, report.duration);
        }));
    }
}

/// Prepare and grow the segments, then fold one slice when one is due.
fn step(core: &Core, waiter: &dyn Waiter) -> Result<Step> {
    if core.segments.wants_spare() {
        core.segments.prepare_spare()?;
    }
    core.segments.grow()?;
    remove_doomed(core);
    match due(core) {
        Some(timeout) => Ok(Step::Wait(timeout)),
        None => match slice(core, waiter)? {
            Some(_) => Ok(Step::Folded),
            None => Ok(Step::Wait(UNPUBLISHED)),
        },
    }
}

/// Remove the files that wait for it. A failed removal leaves the file to
/// a later pass.
fn remove_doomed(core: &Core) {
    if core.segments.has_doomed()
        && let Err(error) = core.segments.remove_doomed()
    {
        tracing::warn!(%error, "the fold could not remove a segment file of the write-ahead log");
    }
}

/// Return `None` when a slice is due now, else the time until the oldest
/// commit of the backlog reaches `fold_age`.
fn due(core: &Core) -> Option<Duration> {
    if core.empty() {
        return Some(Duration::MAX);
    }
    let settings = &core.settings;
    if core.backlog_bytes() >= settings.fold_bytes {
        return None;
    }
    let (front, oldest) = {
        let table = core.read_table();
        (table.front(), table.oldest())
    };
    let (Some(front), Some(oldest)) = (front, oldest) else {
        return Some(Duration::MAX);
    };
    if front <= core.requests().target {
        return None;
    }
    let age = core.clock.monotonic().saturating_sub(oldest);
    settings
        .fold_age
        .checked_sub(age)
        .filter(|left| !left.is_zero())
}

/// Fold one slice: the oldest commits of the backlog at or before the
/// published tid, up to `slice_bytes`. Return `None` when no commit is
/// published. `waiter` is the waiter of the fold, for the wait for the end
/// of a log sync that runs.
///
/// The observer gets the slice before the slice ends and the waiters wake.
/// So a caller of `Store::fold` sees the slice in the observer.
pub(super) fn slice(core: &Core, waiter: &dyn Waiter) -> Result<Option<Report>> {
    core.slicing.store(true, Ordering::SeqCst);
    let folded = fold_slice(core, waiter);
    if let Ok(Some(report)) = &folded {
        observe(core, report);
    }
    core.slicing.store(false, Ordering::SeqCst);
    if let Ok(Some(_)) = folded {
        core.wake_waiters();
    }
    folded
}

fn fold_slice(core: &Core, waiter: &dyn Waiter) -> Result<Option<Report>> {
    let picked = core
        .read_table()
        .pick(core.published(), core.settings.slice_bytes);
    let Some(last) = picked.last().map(|commit| commit.tid) else {
        return Ok(None);
    };
    let commits = picked.len();
    let bytes = picked.iter().map(|commit| commit.bytes).sum();
    let span = tracing::info_span!("fold", commits);
    let _entered = span.enter();
    let started = core.clock.monotonic();
    check(&picked)?;
    let mut write = core.redb.begin_write()?;
    write_commits(&mut *write, &picked)?;
    // The sync of a slice does not start during the sync of a finish.
    core.mark.wait_end(waiter);
    write.commit()?;
    let duration = core.clock.monotonic().saturating_sub(started);
    drop(picked);
    core.table
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove_front(commits, &core.counters);
    core.segments.retire(last);
    remove_doomed(core);
    Ok(Some(Report {
        commits,
        bytes,
        duration,
    }))
}

/// Check the checksum of each frame of `picked` under its key space and its
/// key. A frame that fails gives the corruption error of its key space.
pub(super) fn check(picked: &[Picked]) -> Result<()> {
    for commit in picked {
        let entry: &LogEntry = &commit.entry;
        for op in &entry.ops[commit.ops.clone()] {
            let frame = entry.frame(op);
            let checked = match op.kind {
                OpKind::Txn => {
                    format::verify_frame(KeySpace::Txns, &format::txn_key(op.tid), frame)
                        .map_err(|_| ZoeError::CorruptTxn { tid: op.tid })
                }
                OpKind::Record => {
                    let key = format::record_key(op.oid, op.tid);
                    format::verify_frame(KeySpace::Records, &key, frame).map_err(|_| {
                        ZoeError::CorruptRecord {
                            oid: op.oid,
                            tid: op.tid,
                        }
                    })
                }
                OpKind::Blob => {
                    let key = format::record_key(op.oid, op.tid);
                    format::verify_frame(KeySpace::Blobs, &key, frame).map_err(|_| {
                        ZoeError::CorruptBlob {
                            oid: op.oid,
                            tid: op.tid,
                        }
                    })
                }
                OpKind::Ephemeral | OpKind::Durable => {
                    let flavour = op.kind.flavour().expect("a namespace op");
                    let key = entry.key(op);
                    format::verify_frame(flavour.space(), key, frame)
                        .map_err(|_| crate::kv::corrupt(flavour, key))
                }
                OpKind::RemoveEphemeral | OpKind::RemoveDurable | OpKind::Expiry => Ok(()),
            };
            checked?;
        }
    }
    Ok(())
}

/// Stage the frames and the ops of `picked` in `write`, as they are.
pub(super) fn write_commits(write: &mut dyn WriteTxn, picked: &[Picked]) -> Result<()> {
    for commit in picked {
        let entry: &LogEntry = &commit.entry;
        for op in &entry.ops[commit.ops.clone()] {
            let frame = entry.frame(op);
            match op.kind {
                OpKind::Txn => write.put_frame(Framed::Txn(op.tid, frame))?,
                OpKind::Record => write.put_frame(Framed::Record(op.oid, op.tid, frame))?,
                OpKind::Blob => write.put_frame(Framed::Blob(op.oid, op.tid, frame))?,
                OpKind::Ephemeral => {
                    write.put_frame(Framed::Kv(Flavour::Ephemeral, entry.key(op), frame))?;
                }
                OpKind::Durable => {
                    write.put_frame(Framed::Kv(Flavour::Durable, entry.key(op), frame))?;
                }
                OpKind::RemoveEphemeral => write.remove_kv(Flavour::Ephemeral, entry.key(op))?,
                OpKind::RemoveDurable => write.remove_kv(Flavour::Durable, entry.key(op))?,
                OpKind::Expiry => write.put_kv_expiry(entry.key(op))?,
            }
        }
    }
    Ok(())
}
