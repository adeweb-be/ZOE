//! The unfolded table: the records, the blob records, the namespace ops and
//! the expiry keys of the backlog in memory, and the backlog itself (ADR
//! 0056, section 7). One `RwLock` holds the table. The leader inserts a
//! whole batch under one write lock, and a slice of the fold leaves it
//! under one write lock.
//!
//! The frames stay in the bytes of their entry. The commits of an entry
//! share these bytes, and the table keeps the places of the frames. An
//! entry leaves memory when its last commit leaves the table.

use std::collections::{BTreeMap, VecDeque};
use std::ops::{Bound, Range};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::format::wal::{Commit as EntryCommit, Op, OpKind};
use crate::kv::Flavour;
use crate::types::{Oid, Tid};

/// The bytes that the indexes of one op count in the backlog.
pub(super) const OP_INDEX_BYTES: u64 = 64;

/// The last op of the backlog on each row of a range: the key of the row,
/// and the frame of a write or `None` for a removal.
pub(super) type RowOps = Vec<(Vec<u8>, Option<Vec<u8>>)>;

/// One entry of the log in memory: its bytes and its ops.
#[derive(Debug)]
pub(super) struct LogEntry {
    pub(super) bytes: Vec<u8>,
    pub(super) ops: Vec<Op>,
}

impl LogEntry {
    /// Return the frame of `op`.
    pub(super) fn frame(&self, op: &Op) -> &[u8] {
        &self.bytes[op.frame.clone()]
    }

    /// Return the key of the namespace op `op`.
    pub(super) fn key(&self, op: &Op) -> &[u8] {
        &self.bytes[op.key.clone()]
    }
}

/// The place of a frame: the number of its entry and its bytes there.
#[derive(Clone, Debug)]
struct Place {
    entry: u64,
    frame: Range<usize>,
}

/// An op of the backlog on a namespace row: a write with its frame, or a
/// removal.
#[derive(Clone, Debug)]
struct RowOp {
    tid: Tid,
    frame: Option<Place>,
}

/// One commit of the backlog.
#[derive(Clone, Debug)]
pub(super) struct Commit {
    pub(super) tid: Tid,
    /// The number of its entry.
    entry: u64,
    /// Its ops, as indexes in the ops of its entry. The first one is its
    /// transaction row.
    pub(super) ops: Range<usize>,
    /// The bytes of its ops in the entry.
    pub(super) bytes: u64,
    /// The number of its records.
    pub(super) records: usize,
    /// The monotonic time of its publication.
    pub(super) published: Duration,
}

/// A commit that a slice of the fold takes: its entry, its ops and its
/// size.
pub(super) struct Picked {
    pub(super) tid: Tid,
    pub(super) entry: Arc<LogEntry>,
    pub(super) ops: Range<usize>,
    pub(super) bytes: u64,
}

/// An entry that the table holds, with the number of its commits that are
/// still in the table.
struct Held {
    entry: Arc<LogEntry>,
    live: usize,
}

/// The counters of the backlog, beside the lock of the table. The table
/// changes them under its write lock. A read that finds no commit takes no
/// lock.
#[derive(Debug, Default)]
pub(super) struct Counters {
    /// The commits of the backlog.
    pub(super) commits: AtomicU64,
    /// The bytes of the backlog: each entry in memory, whole, and
    /// `OP_INDEX_BYTES` for each op of the table.
    pub(super) bytes: AtomicU64,
    /// The blob records of the backlog.
    pub(super) blobs: AtomicU64,
    /// The namespace ops of the backlog, ephemeral then durable.
    pub(super) kv: [AtomicU64; 2],
}

/// Return the index of `flavour` in the arrays of the table.
fn index(flavour: Flavour) -> usize {
    match flavour {
        Flavour::Ephemeral => 0,
        Flavour::Durable => 1,
    }
}

/// Return the end of the tids that a read at the published tid `p` sees,
/// excluded: the tid after `p`.
fn after(p: Tid) -> Tid {
    Tid(p.0.saturating_add(1))
}

/// The unfolded table.
#[derive(Default)]
pub(super) struct Table {
    /// The entries whose commits the table holds, oldest first. `base` is
    /// the number of the first one.
    entries: VecDeque<Held>,
    base: u64,
    records: BTreeMap<(Oid, Tid), Place>,
    blobs: BTreeMap<(Oid, Tid), Place>,
    /// The ops on the namespace rows, ephemeral then durable, by key of
    /// the row, in tid order.
    rows: [BTreeMap<Vec<u8>, Vec<RowOp>>; 2],
    /// The keys of the index of the expiries, with the tids that wrote them.
    expiry: BTreeMap<Vec<u8>, Vec<Tid>>,
    /// The commits after the folded tid, in tid order.
    backlog: VecDeque<Commit>,
}

impl Table {
    /// Return the entry `number`.
    fn entry(&self, number: u64) -> &LogEntry {
        &self.entries[(number - self.base) as usize].entry
    }

    /// Return the bytes at `place`.
    fn bytes(&self, place: &Place) -> &[u8] {
        &self.entry(place.entry).bytes[place.frame.clone()]
    }

    /// Insert `commits`, commits of `entry`, into the table and the
    /// backlog, at the monotonic time `now`, and count them in `counters`.
    /// The commits come in tid order, after each commit of the backlog.
    pub(super) fn publish(
        &mut self,
        entry: Arc<LogEntry>,
        commits: &[EntryCommit],
        now: Duration,
        counters: &Counters,
    ) {
        if commits.is_empty() {
            return;
        }
        let number = self.base + self.entries.len() as u64;
        let (mut ops, mut blobs, mut kv) = (0u64, 0u64, [0u64; 2]);
        for commit in commits {
            let mut records = 0;
            for op in &entry.ops[commit.ops.clone()] {
                let place = Place {
                    entry: number,
                    frame: op.frame.clone(),
                };
                match op.kind {
                    OpKind::Txn => {}
                    OpKind::Record => {
                        self.records.insert((op.oid, op.tid), place);
                        records += 1;
                    }
                    OpKind::Blob => {
                        self.blobs.insert((op.oid, op.tid), place);
                        blobs += 1;
                    }
                    OpKind::Expiry => {
                        let key = entry.key(op);
                        match self.expiry.get_mut(key) {
                            Some(tids) => tids.push(op.tid),
                            None => {
                                self.expiry.insert(key.to_vec(), vec![op.tid]);
                            }
                        }
                    }
                    kind => {
                        let flavour = kind.flavour().expect("a namespace op");
                        let write = matches!(kind, OpKind::Ephemeral | OpKind::Durable);
                        let row = RowOp {
                            tid: op.tid,
                            frame: write.then_some(place),
                        };
                        let key = entry.key(op);
                        let rows = &mut self.rows[index(flavour)];
                        match rows.get_mut(key) {
                            Some(ops) => ops.push(row),
                            None => {
                                rows.insert(key.to_vec(), vec![row]);
                            }
                        }
                        kv[index(flavour)] += 1;
                    }
                }
            }
            ops += commit.ops.len() as u64;
            self.backlog.push_back(Commit {
                tid: commit.tid,
                entry: number,
                ops: commit.ops.clone(),
                bytes: commit.bytes.len() as u64,
                records,
                published: now,
            });
        }
        let size = entry.bytes.len() as u64 + OP_INDEX_BYTES * ops;
        self.entries.push_back(Held {
            entry,
            live: commits.len(),
        });
        counters.bytes.fetch_add(size, Ordering::AcqRel);
        counters.blobs.fetch_add(blobs, Ordering::AcqRel);
        for (count, added) in counters.kv.iter().zip(kv) {
            count.fetch_add(added, Ordering::AcqRel);
        }
        counters
            .commits
            .fetch_add(commits.len() as u64, Ordering::AcqRel);
    }

    /// Return the oldest commits of the backlog at or before `p`, whole
    /// commits, until their bytes reach `slice_bytes`, and at least one.
    pub(super) fn pick(&self, p: Tid, slice_bytes: u64) -> Vec<Picked> {
        let mut picked: Vec<Picked> = Vec::new();
        let mut bytes = 0;
        for commit in &self.backlog {
            if commit.tid > p || (!picked.is_empty() && bytes >= slice_bytes) {
                break;
            }
            bytes += commit.bytes;
            let held = &self.entries[(commit.entry - self.base) as usize];
            picked.push(Picked {
                tid: commit.tid,
                entry: Arc::clone(&held.entry),
                ops: commit.ops.clone(),
                bytes: commit.bytes,
            });
        }
        picked
    }

    /// Remove the `n` oldest commits of the backlog from the table, with
    /// each of their records, blob records, namespace ops and expiry keys.
    /// An entry leaves memory with its last commit.
    pub(super) fn remove_front(&mut self, n: usize, counters: &Counters) {
        let (mut ops, mut blobs, mut kv, mut freed) = (0u64, 0u64, [0u64; 2], 0u64);
        for _ in 0..n {
            let Some(commit) = self.backlog.pop_front() else {
                break;
            };
            let at = (commit.entry - self.base) as usize;
            let entry = Arc::clone(&self.entries[at].entry);
            for op in &entry.ops[commit.ops.clone()] {
                match op.kind {
                    OpKind::Txn => {}
                    OpKind::Record => {
                        self.records.remove(&(op.oid, op.tid));
                    }
                    OpKind::Blob => {
                        self.blobs.remove(&(op.oid, op.tid));
                        blobs += 1;
                    }
                    OpKind::Expiry => {
                        let key = entry.key(op);
                        if let Some(tids) = self.expiry.get_mut(key) {
                            if let Some(i) = tids.iter().position(|&tid| tid == op.tid) {
                                tids.remove(i);
                            }
                            if tids.is_empty() {
                                self.expiry.remove(key);
                            }
                        }
                    }
                    kind => {
                        let flavour = kind.flavour().expect("a namespace op");
                        let rows = &mut self.rows[index(flavour)];
                        let key = entry.key(op);
                        if let Some(row) = rows.get_mut(key) {
                            if let Some(i) = row.iter().position(|row| row.tid == op.tid) {
                                row.remove(i);
                            }
                            if row.is_empty() {
                                rows.remove(key);
                            }
                        }
                        kv[index(flavour)] += 1;
                    }
                }
            }
            ops += commit.ops.len() as u64;
            self.entries[at].live -= 1;
            while self.entries.front().is_some_and(|held| held.live == 0) {
                let held = self.entries.pop_front().expect("an entry");
                freed += held.entry.bytes.len() as u64;
                self.base += 1;
            }
            counters.commits.fetch_sub(1, Ordering::AcqRel);
        }
        counters
            .bytes
            .fetch_sub(freed + OP_INDEX_BYTES * ops, Ordering::AcqRel);
        counters.blobs.fetch_sub(blobs, Ordering::AcqRel);
        for (count, removed) in counters.kv.iter().zip(kv) {
            count.fetch_sub(removed, Ordering::AcqRel);
        }
    }

    /// Return the tid of the oldest commit of the backlog.
    pub(super) fn front(&self) -> Option<Tid> {
        self.backlog.front().map(|commit| commit.tid)
    }

    /// Return the monotonic time of the publication of the oldest commit
    /// of the backlog.
    pub(super) fn oldest(&self) -> Option<Duration> {
        self.backlog.front().map(|commit| commit.published)
    }

    /// Return the newest version of `oid` before `tid` and at or before
    /// `p`, with its frame.
    pub(super) fn newest_before(&self, oid: Oid, tid: Tid, p: Tid) -> Option<(Tid, &[u8])> {
        let end = tid.min(after(p));
        self.records
            .range((oid, Tid::ZERO)..(oid, end))
            .next_back()
            .map(|(&(_, tid), place)| (tid, self.bytes(place)))
    }

    /// Return the serial of the oldest version of `oid` at or after `tid`
    /// and at or before `p`.
    pub(super) fn first_from(&self, oid: Oid, tid: Tid, p: Tid) -> Option<Tid> {
        if tid > p {
            return None;
        }
        self.records
            .range((oid, tid)..=(oid, p))
            .next()
            .map(|(&(_, tid), _)| tid)
    }

    /// Return the frame of the record of `oid` at `tid`, when `tid` is at
    /// or before `p`.
    pub(super) fn record(&self, oid: Oid, tid: Tid, p: Tid) -> Option<&[u8]> {
        if tid > p {
            return None;
        }
        self.records.get(&(oid, tid)).map(|place| self.bytes(place))
    }

    /// Return the frame of the blob record of `oid` at `tid`, when `tid` is
    /// at or before `p`.
    pub(super) fn blob(&self, oid: Oid, tid: Tid, p: Tid) -> Option<&[u8]> {
        if tid > p {
            return None;
        }
        self.blobs.get(&(oid, tid)).map(|place| self.bytes(place))
    }

    /// Return the versions of `oid` at or before `p`, oldest first, with
    /// their frames.
    pub(super) fn versions(
        &self,
        oid: Oid,
        p: Tid,
    ) -> impl DoubleEndedIterator<Item = (Tid, &[u8])> + '_ {
        self.records
            .range((oid, Tid::ZERO)..(oid, after(p)))
            .map(|(&(_, tid), place)| (tid, self.bytes(place)))
    }

    /// Return the first oid at or after `from` with a version at or before
    /// `p`.
    pub(super) fn first_oid_from(&self, from: Oid, p: Tid) -> Option<Oid> {
        self.records
            .range((from, Tid::ZERO)..)
            .find(|((_, tid), _)| *tid <= p)
            .map(|(&(oid, _), _)| oid)
    }

    /// Return the keys of the versions at or before `p` of the oids from
    /// `from` up, in key order. The list stops after the first oid that
    /// brings it to `limit` keys or more.
    pub(super) fn keys(&self, from: Oid, limit: usize, p: Tid) -> Vec<(Oid, Tid)> {
        let mut keys: Vec<(Oid, Tid)> = Vec::new();
        for &(oid, tid) in self.records.range((from, Tid::ZERO)..).map(|(key, _)| key) {
            if tid > p {
                continue;
            }
            if keys.len() >= limit && keys.last().is_some_and(|&(last, _)| last != oid) {
                break;
            }
            keys.push((oid, tid));
        }
        keys
    }

    /// Return the commit `tid` of the backlog when it is at or before `p`,
    /// with its entry.
    pub(super) fn commit(&self, tid: Tid, p: Tid) -> Option<(&LogEntry, &Commit)> {
        if tid > p {
            return None;
        }
        let at = self.backlog.partition_point(|commit| commit.tid < tid);
        let commit = self.backlog.get(at).filter(|commit| commit.tid == tid)?;
        Some((self.entry(commit.entry), commit))
    }

    /// Return the commits of the backlog from `from` to `to`, both
    /// included, at or before `p`, in tid order, with their entries.
    pub(super) fn commits(
        &self,
        from: Tid,
        to: Tid,
        p: Tid,
    ) -> impl DoubleEndedIterator<Item = (&LogEntry, &Commit)> + '_ {
        let start = self.backlog.partition_point(|commit| commit.tid < from);
        let end = self
            .backlog
            .partition_point(|commit| commit.tid <= to.min(p));
        self.backlog
            .range(start..end.max(start))
            .map(|commit| (self.entry(commit.entry), commit))
    }

    /// Return the last op at or before `p` on the row `row` of `flavour`:
    /// `Some(Some(frame))` for a write, `Some(None)` for a removal, `None`
    /// when the backlog has no such op.
    pub(super) fn row(&self, flavour: Flavour, row: &[u8], p: Tid) -> Option<Option<&[u8]>> {
        let ops = self.rows[index(flavour)].get(row)?;
        let op = ops.iter().rev().find(|op| op.tid <= p)?;
        Some(op.frame.as_ref().map(|place| self.bytes(place)))
    }

    /// Return the last op at or before `p` on each row of `flavour` from
    /// `from` to `to`, in key order: the key with the frame of a write, or
    /// with `None` for a removal. The list stops after `limit` writes.
    /// Return the list, and true when it stopped there.
    pub(super) fn rows(
        &self,
        flavour: Flavour,
        from: Bound<&[u8]>,
        to: Bound<&[u8]>,
        p: Tid,
        limit: usize,
    ) -> (RowOps, bool) {
        let mut out = Vec::new();
        if empty_range(from, to) {
            return (out, false);
        }
        let mut writes = 0;
        for (key, ops) in self.rows[index(flavour)].range::<[u8], _>((from, to)) {
            let Some(op) = ops.iter().rev().find(|op| op.tid <= p) else {
                continue;
            };
            let frame = op.frame.as_ref().map(|place| self.bytes(place).to_vec());
            if frame.is_some() {
                writes += 1;
            }
            out.push((key.clone(), frame));
            if writes == limit {
                return (out, true);
            }
        }
        (out, false)
    }

    /// Tell if the backlog wrote `key` of the index of the expiries at or
    /// before `p`.
    pub(super) fn has_expiry(&self, key: &[u8], p: Tid) -> bool {
        self.expiry
            .get(key)
            .is_some_and(|tids| tids.iter().any(|&tid| tid <= p))
    }
}

/// Tell if no key can be inside the bounds `from` and `to`. A `BTreeMap`
/// panics on a range whose start is after its end.
pub(super) fn empty_range(from: Bound<&[u8]>, to: Bound<&[u8]>) -> bool {
    use Bound::{Excluded, Included};
    match (from, to) {
        (Included(from), Included(to)) => from > to,
        (Included(from) | Excluded(from), Excluded(to)) | (Excluded(from), Included(to)) => {
            from >= to
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::wal::EntryBuilder;

    /// Return an entry of two commits, tids 5 and 6, each with a record of
    /// oid 1 and a write of the durable row `k`, and an expiry key in the
    /// first.
    fn entry() -> (LogEntry, Vec<EntryCommit>) {
        let mut builder = EntryBuilder::new(0);
        for (tid, data) in [(Tid(5), b"five"), (Tid(6), b"sixx")] {
            builder
                .txn(tid, |out| out.extend_from_slice(b"row"))
                .unwrap();
            builder
                .record(OpKind::Record, Oid(1), tid, |out| {
                    out.extend_from_slice(data)
                })
                .unwrap();
            builder
                .kv(Flavour::Durable, b"k", |out| out.extend_from_slice(data))
                .unwrap();
            if tid == Tid(5) {
                builder.expiry(b"x").unwrap();
            }
        }
        let mut entry = builder.finish();
        entry.seal(1, Tid(4));
        let commits = entry.commits.clone();
        (
            LogEntry {
                bytes: entry.bytes,
                ops: entry.ops,
            },
            commits,
        )
    }

    #[test]
    fn a_read_ignores_what_is_after_the_published_tid() {
        let (entry, commits) = entry();
        let counters = Counters::default();
        let mut table = Table::default();
        table.publish(Arc::new(entry), &commits, Duration::ZERO, &counters);
        assert_eq!(counters.commits.load(Ordering::SeqCst), 2);

        let newest = |p| {
            table
                .newest_before(Oid(1), Tid::MAX, Tid(p))
                .map(|(t, f)| (t, f.to_vec()))
        };
        assert_eq!(newest(6), Some((Tid(6), b"sixx".to_vec())));
        assert_eq!(newest(5), Some((Tid(5), b"five".to_vec())));
        assert_eq!(newest(4), None);
        assert_eq!(table.first_from(Oid(1), Tid(6), Tid(5)), None);
        assert_eq!(table.first_from(Oid(1), Tid(6), Tid(6)), Some(Tid(6)));
        assert_eq!(
            table.row(Flavour::Durable, b"k", Tid(5)),
            Some(Some(&b"five"[..]))
        );
        assert_eq!(table.row(Flavour::Durable, b"k", Tid(4)), None);
        assert!(table.has_expiry(b"x", Tid(5)));
        assert!(!table.has_expiry(b"x", Tid(4)));
        assert_eq!(table.commits(Tid::ZERO, Tid::MAX, Tid(5)).count(), 1);
        assert_eq!(
            table.keys(Oid(0), 10, Tid(6)),
            [(Oid(1), Tid(5)), (Oid(1), Tid(6))]
        );
    }

    #[test]
    fn an_entry_counts_whole_until_its_last_commit_leaves() {
        let (entry, commits) = entry();
        let len = entry.bytes.len() as u64;
        let ops = entry.ops.len() as u64;
        let counters = Counters::default();
        let mut table = Table::default();
        table.publish(Arc::new(entry), &commits, Duration::ZERO, &counters);
        assert_eq!(
            counters.bytes.load(Ordering::SeqCst),
            len + OP_INDEX_BYTES * ops
        );
        let picked = table.pick(Tid(6), 1);
        assert_eq!(picked.len(), 1, "a slice takes at least one commit");
        table.remove_front(1, &counters);
        let left = commits[1].ops.len() as u64;
        assert_eq!(
            counters.bytes.load(Ordering::SeqCst),
            len + OP_INDEX_BYTES * left
        );
        assert_eq!(table.front(), Some(Tid(6)));
        assert!(!table.has_expiry(b"x", Tid(6)));
        assert_eq!(
            table.row(Flavour::Durable, b"k", Tid(6)),
            Some(Some(&b"sixx"[..]))
        );
        table.remove_front(1, &counters);
        assert_eq!(counters.bytes.load(Ordering::SeqCst), 0);
        assert_eq!(counters.commits.load(Ordering::SeqCst), 0);
        assert_eq!(table.row(Flavour::Durable, b"k", Tid(6)), None);
        assert_eq!(table.newest_before(Oid(1), Tid::MAX, Tid(6)), None);
    }
}
