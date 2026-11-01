//! The segment files of the log (ADR 0056, sections 1, 2 and 6): the
//! preparation of spares, the new uses, the appends, the growth, the
//! retirement, the removal and the headers of the close.
//!
//! One mutex holds the state of the segments. The leader takes it under the
//! commit sequencer, and the fold without it. Nobody takes another lock
//! under it, and nobody writes or syncs a file while holding it.
//!
//! The leader writes an entry only within the length of its segment, so
//! its writes never meet a growth of the fold, which writes beyond that
//! length. A growth is marked in the state while it runs: a leader that
//! needs a growth of the same segment waits for it.

use std::collections::VecDeque;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use redb::StorageBackend;

use crate::clock::{Clock, Waiter};
use crate::error::{Result, ZoeError};
use crate::format::wal::{
    self, Entry, SEGMENT_HEADER_LEN, SegmentHeader, SegmentState, encode_segment_header,
};
use crate::types::Tid;
use crate::vfs::Dir;

/// The least headroom that the fold keeps beyond the last entry.
const MIN_HEADROOM: u64 = 1 << 20;

/// The most headroom that the fold keeps beyond the last entry.
const MAX_HEADROOM: u64 = 8 << 20;

/// The most spares that stay.
const SPARES: usize = 2;

/// One segment file.
#[derive(Debug)]
pub(super) struct Seg {
    pub(super) number: u64,
    file: Box<dyn StorageBackend>,
    /// The length of the file. It changes under the mutex of the segments,
    /// after the growth is made.
    len: AtomicU64,
}

impl Seg {
    pub(super) fn new(number: u64, file: Box<dyn StorageBackend>, len: u64) -> Self {
        Self {
            number,
            file,
            len: AtomicU64::new(len),
        }
    }

    fn len(&self) -> u64 {
        self.len.load(Ordering::Acquire)
    }

    fn name(&self) -> String {
        wal::segment_name(self.number)
    }
}

/// A segment ready for a new use, with the header that its file holds.
struct Ready {
    seg: Arc<Seg>,
    header: SegmentHeader,
}

/// The segment of the current use.
struct Current {
    seg: Arc<Seg>,
    epoch: u64,
    /// The end of its last entry.
    end: u64,
    /// The last tid of its entries.
    last: Tid,
}

/// A segment of an earlier use whose entries the tables may not hold yet.
struct Used {
    seg: Arc<Seg>,
    epoch: u64,
    /// The last tid of its entries.
    last: Tid,
    /// Its header failed at the open: the file is never used again.
    damaged: bool,
}

/// The state of the segments.
struct State {
    /// The number of the next new file.
    next_number: u64,
    /// The greatest epoch that the store saw since its open.
    max_epoch: u64,
    current: Option<Current>,
    spares: VecDeque<Ready>,
    /// The segments of earlier uses, in epoch order.
    used: VecDeque<Used>,
    /// The files to remove.
    doomed: Vec<Arc<Seg>>,
    /// The numbers of the segments whose growth runs.
    growing: Vec<u64>,
    /// The leaders that wait for the end of a growth.
    grow_waiters: Vec<Arc<dyn Waiter>>,
}

impl State {
    /// Return a new epoch: one more than each epoch that the store saw.
    fn next_epoch(&mut self) -> Result<u64> {
        let epoch = self.max_epoch.checked_add(1).ok_or_else(|| {
            ZoeError::Io(io::Error::other(
                "no epoch is left for a new use of a segment",
            ))
        })?;
        self.max_epoch = epoch;
        Ok(epoch)
    }

    /// Keep `ready` as a spare, or remove its file when two spares wait.
    fn keep(&mut self, ready: Ready) {
        if self.spares.len() < SPARES {
            self.spares.push_back(ready);
        } else {
            self.doomed.push(ready.seg);
        }
    }
}

/// A segment that the open found, for `Segments::new`.
pub(super) enum Opened {
    /// A segment ready for a new use, with the header of its file.
    Ready(Seg, SegmentHeader),
    /// A segment with commits after the folded tid: its epoch, the last
    /// tid of its entries, and true when its header failed.
    Used(Seg, u64, Tid, bool),
    /// A file to remove.
    Doomed(Seg),
}

/// The mark "a log sync runs" (ADR 0056, sections 5 and 9): an even count
/// while no sync runs. The fold waits for the end of a sync that runs
/// before the commit of a slice.
pub(super) struct SyncMark {
    count: AtomicU64,
    /// The fold waits for the end of a sync.
    fold_waits: AtomicBool,
    /// The waiter of the fold thread.
    fold: Arc<dyn Waiter>,
}

impl SyncMark {
    pub(super) fn new(fold: Arc<dyn Waiter>) -> Self {
        Self {
            count: AtomicU64::new(0),
            fold_waits: AtomicBool::new(false),
            fold,
        }
    }

    /// Mark a sync that starts. The guard clears the mark on each path out,
    /// a panic included.
    fn begin(&self) -> Syncing<'_> {
        self.count.fetch_add(1, Ordering::SeqCst);
        Syncing(self)
    }

    /// Wait for the end of the sync that runs, if one runs, on `waiter`,
    /// the waiter of the fold. Do not wait for the next one.
    pub(super) fn wait_end(&self, waiter: &dyn Waiter) {
        let at = self.count.load(Ordering::SeqCst);
        if at.is_multiple_of(2) {
            return;
        }
        self.fold_waits.store(true, Ordering::SeqCst);
        while self.count.load(Ordering::SeqCst) == at {
            waiter.wait(Duration::from_millis(10));
        }
        self.fold_waits.store(false, Ordering::SeqCst);
    }
}

/// A log sync that runs.
struct Syncing<'m>(&'m SyncMark);

impl Drop for Syncing<'_> {
    fn drop(&mut self) {
        self.0.count.fetch_add(1, Ordering::SeqCst);
        if self.0.fold_waits.load(Ordering::SeqCst) {
            self.0.fold.wake();
        }
    }
}

/// What an append asks of the fold.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Hints {
    /// The current segment passed half of its size and no spare is ready,
    /// or its headroom is low: the fold should prepare or grow.
    pub(super) housekeeping: bool,
}

/// The segments of the log.
pub(super) struct Segments {
    dir: Arc<dyn Dir>,
    segment_bytes: u64,
    state: Mutex<State>,
}

impl Segments {
    /// Return the segments that the open found. `next_number` is one more
    /// than the greatest number of a file of `wal/`, and `max_epoch` the
    /// greatest epoch that the open read.
    pub(super) fn new(
        dir: Arc<dyn Dir>,
        segment_bytes: u64,
        next_number: u64,
        max_epoch: u64,
        found: Vec<Opened>,
    ) -> Self {
        let mut state = State {
            next_number,
            max_epoch,
            current: None,
            spares: VecDeque::new(),
            used: VecDeque::new(),
            doomed: Vec::new(),
            growing: Vec::new(),
            grow_waiters: Vec::new(),
        };
        let mut used = Vec::new();
        for segment in found {
            match segment {
                Opened::Ready(seg, header) => state.keep(Ready {
                    seg: Arc::new(seg),
                    header,
                }),
                Opened::Used(seg, epoch, last, damaged) => used.push(Used {
                    seg: Arc::new(seg),
                    epoch,
                    last,
                    damaged,
                }),
                Opened::Doomed(seg) => state.doomed.push(Arc::new(seg)),
            }
        }
        used.sort_by_key(|used| (used.epoch, used.seg.number));
        state.used = used.into();
        Self {
            dir,
            segment_bytes,
            state: Mutex::new(state),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        crate::lock(&self.state)
    }

    /// Tell if a spare is ready.
    pub(super) fn has_spare(&self) -> bool {
        !self.lock().spares.is_empty()
    }

    /// Tell if the fold should prepare a spare: none is ready, and the
    /// current segment passed half of its size.
    pub(super) fn wants_spare(&self) -> bool {
        let state = self.lock();
        state.spares.is_empty()
            && state
                .current
                .as_ref()
                .is_some_and(|current| current.end > self.segment_bytes / 2)
    }

    /// Prepare a new spare: create `wal/<n>.new`, write a spare header with
    /// a new epoch, grow it and sync it, rename it to `wal/<n>.wal` and sync
    /// `wal/`. So a `.wal` file always had a valid header on the disk.
    pub(super) fn prepare_spare(&self) -> Result<()> {
        let (number, epoch) = {
            let mut state = self.lock();
            let number = state.next_number;
            state.next_number += 1;
            (number, state.next_epoch()?)
        };
        let (new, name) = (wal::new_name(number), wal::segment_name(number));
        let header = SegmentHeader {
            state: SegmentState::Spare,
            epoch,
        };
        let len = self.segment_bytes.clamp(SEGMENT_HEADER_LEN, MIN_HEADROOM);
        {
            let file = self.dir.open_log(&new)?;
            file.write(0, &encode_segment_header(header))?;
            file.set_len(len)?;
            file.sync_data()?;
        }
        self.dir.rename(&new, &name)?;
        self.dir.sync_dir_at(wal::DIR)?;
        // A handle of the old name would write to the old name in a
        // simulated filesystem, so the file opens again.
        let file = self.dir.open_log(&name)?;
        let seg = Arc::new(Seg::new(number, file, len));
        self.lock().keep(Ready { seg, header });
        Ok(())
    }

    /// Write `entry`, sealed with its epoch and `previous`, into the
    /// current segment or into a new use of a spare, then sync it. The
    /// leader calls this under the commit sequencer. `mark` covers the
    /// write and the sync. A leader that needs a growth that the fold runs
    /// waits on a waiter of `clock`.
    pub(super) fn append(
        &self,
        entry: &mut Entry,
        previous: Tid,
        mark: &SyncMark,
        clock: &dyn Clock,
    ) -> Result<Hints> {
        let len = (entry.bytes.len() as u64).next_multiple_of(wal::ALIGN);
        let mut waiter: Option<Arc<dyn Waiter>> = None;
        loop {
            let mut state = self.lock();
            let fits = state
                .current
                .as_ref()
                .is_some_and(|current| current.end + len <= self.segment_bytes);
            let (seg, offset, epoch, new_use) = if fits {
                let current = state.current.as_ref().expect("a current segment");
                (Arc::clone(&current.seg), current.end, current.epoch, false)
            } else {
                let Some(spare) = state.spares.pop_front() else {
                    drop(state);
                    // The fold prepares the spares. A fold that falls behind
                    // costs this preparation on the commit path.
                    self.prepare_spare()?;
                    continue;
                };
                let epoch = state.next_epoch()?;
                if let Some(old) = state.current.take() {
                    state.used.push_back(Used {
                        seg: old.seg,
                        epoch: old.epoch,
                        last: old.last,
                        damaged: false,
                    });
                }
                state.current = Some(Current {
                    seg: Arc::clone(&spare.seg),
                    epoch,
                    end: SEGMENT_HEADER_LEN,
                    last: previous,
                });
                (spare.seg, SEGMENT_HEADER_LEN, epoch, true)
            };
            let need = offset + len;
            let grow = seg.len() < need;
            if grow {
                if state.growing.contains(&seg.number) {
                    // The fold grows the current segment now. A new use takes
                    // a spare, which the fold never grows, so only an append
                    // to the current segment waits here.
                    let waiter = waiter.get_or_insert_with(|| clock.waiter());
                    state.grow_waiters.push(Arc::clone(waiter));
                    drop(state);
                    waiter.wait(Duration::from_millis(100));
                    continue;
                }
                state.growing.push(seg.number);
            }
            drop(state);
            let written = self.write(&seg, entry, previous, epoch, offset, new_use, grow, mark);
            let mut state = self.lock();
            if grow {
                state.growing.retain(|&number| number != seg.number);
                if written.is_ok() {
                    seg.len.fetch_max(need, Ordering::AcqRel);
                }
                for waiter in state.grow_waiters.drain(..) {
                    waiter.wake();
                }
            }
            written?;
            let no_spare = state.spares.is_empty();
            let current = state.current.as_mut().expect("the current segment");
            current.end = need;
            current.last = entry.last();
            let headroom = (current.end - SEGMENT_HEADER_LEN).clamp(MIN_HEADROOM, MAX_HEADROOM);
            let low = seg.len() < (current.end + headroom / 2).min(self.segment_bytes);
            let spare = no_spare && current.end > self.segment_bytes / 2;
            return Ok(Hints {
                housekeeping: low || spare,
            });
        }
    }

    /// Seal `entry` with `epoch` and `previous` and write it at `offset` of
    /// `seg`, after the header of a new use when `new_use`. Grow the file
    /// first when `grow`. Then sync the file once.
    #[allow(clippy::too_many_arguments)]
    fn write(
        &self,
        seg: &Seg,
        entry: &mut Entry,
        previous: Tid,
        epoch: u64,
        offset: u64,
        new_use: bool,
        grow: bool,
        mark: &SyncMark,
    ) -> Result<()> {
        if grow {
            let need = offset + (entry.bytes.len() as u64).next_multiple_of(wal::ALIGN);
            seg.file.set_len(need)?;
        }
        entry.seal(epoch, previous);
        let _syncing = mark.begin();
        if new_use {
            let header = SegmentHeader {
                state: SegmentState::InUse,
                epoch,
            };
            seg.file.write(0, &encode_segment_header(header))?;
        }
        seg.file.write(offset, &entry.bytes)?;
        seg.file.sync_data()?;
        Ok(())
    }

    /// Grow the current segment so that its length stays at least a
    /// headroom beyond its last entry, within the size of a segment. The
    /// headroom is the bytes of the current use, at least 1 MiB and at most
    /// 8 MiB. The fold calls this. Return true when it grew the file.
    pub(super) fn grow(&self) -> Result<bool> {
        let (seg, target) = {
            let mut state = self.lock();
            let Some(current) = &state.current else {
                return Ok(false);
            };
            if state.growing.contains(&current.seg.number) {
                return Ok(false);
            }
            let headroom = (current.end - SEGMENT_HEADER_LEN).clamp(MIN_HEADROOM, MAX_HEADROOM);
            let target = (current.end + headroom).min(self.segment_bytes);
            let low = (current.end + headroom / 2).min(self.segment_bytes);
            if current.seg.len() >= low {
                return Ok(false);
            }
            let seg = Arc::clone(&current.seg);
            state.growing.push(seg.number);
            (seg, target)
        };
        let grown = seg.file.set_len(target).and_then(|()| seg.file.sync_data());
        let mut state = self.lock();
        state.growing.retain(|&number| number != seg.number);
        if grown.is_ok() {
            seg.len.fetch_max(target, Ordering::AcqRel);
        }
        for waiter in state.grow_waiters.drain(..) {
            waiter.wake();
        }
        grown?;
        Ok(true)
    }

    /// Retire each segment of an earlier use whose entries are all at or
    /// before `folded`: it becomes a spare, or its file goes when two
    /// spares wait or its header failed at the open. The fold calls this
    /// after the commits of a slice left the table.
    pub(super) fn retire(&self, folded: Tid) {
        let mut state = self.lock();
        while state.used.front().is_some_and(|used| used.last <= folded) {
            let used = state.used.pop_front().expect("a used segment");
            if used.damaged {
                state.doomed.push(used.seg);
            } else {
                let header = SegmentHeader {
                    state: SegmentState::InUse,
                    epoch: used.epoch,
                };
                state.keep(Ready {
                    seg: used.seg,
                    header,
                });
            }
        }
    }

    /// Tell if files wait for their removal.
    pub(super) fn has_doomed(&self) -> bool {
        !self.lock().doomed.is_empty()
    }

    /// Remove the files of the retired segments beyond the spares and of
    /// the segments whose header failed, with no sync of `wal/`: a removed
    /// file that a crash brings back holds folded entries only.
    pub(super) fn remove_doomed(&self) -> io::Result<()> {
        let doomed = std::mem::take(&mut self.lock().doomed);
        for seg in doomed {
            let name = seg.name();
            drop(seg);
            match self.dir.remove(&name) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Write the state "spare" into each header that says "in use", with
    /// its epoch, and sync each such file. Then keep two spares and remove
    /// the other files that hold no entry that counts. The close calls this
    /// after it folded the whole backlog.
    pub(super) fn close(&self, folded: Tid) -> io::Result<()> {
        self.retire(folded);
        let mut state = self.lock();
        if let Some(current) = state.current.take() {
            let header = SegmentHeader {
                state: SegmentState::InUse,
                epoch: current.epoch,
            };
            state.spares.push_front(Ready {
                seg: current.seg,
                header,
            });
        }
        let mut spares = std::mem::take(&mut state.spares);
        drop(state);
        for ready in &mut spares {
            if ready.header.state == SegmentState::InUse {
                let header = SegmentHeader {
                    state: SegmentState::Spare,
                    epoch: ready.header.epoch,
                };
                ready.seg.file.write(0, &encode_segment_header(header))?;
                ready.seg.file.sync_data()?;
                ready.header = header;
            }
        }
        let mut state = self.lock();
        while spares.len() > SPARES {
            let ready = spares.pop_back().expect("a spare");
            state.doomed.push(ready.seg);
        }
        state.spares = spares;
        drop(state);
        self.remove_doomed()
    }
}
