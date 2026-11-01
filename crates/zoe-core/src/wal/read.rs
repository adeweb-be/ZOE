//! The read paths of the logged backend (ADR 0056, section 8).
//!
//! Each read loads the published tid P first, and ignores each version,
//! row, key and commit of the table after it. When the backlog holds no
//! commit, a read takes no lock: it reads one counter and goes to the
//! tables. Else it reads the table first, then the tables: a commit leaves
//! the table only after the tables hold it, so a read that misses a commit
//! in the table finds it in the tables. A scan merges the two sources in
//! key order, and a commit or a version that both give counts once. Each
//! frame of the table passes its checksum before a read uses it.

use std::collections::{BTreeMap, VecDeque};
use std::ops::Bound::{self, Excluded, Included};
use std::sync::{Arc, RwLockReadGuard};

use crate::backend::{
    Backend, BatchPlan, BlobRecord, Bytes, Record, RecordEncoding, RecordMeta, TxnIter, TxnMeta,
    TxnRecords, WriteTxn,
};
use crate::commit::Observer;
use crate::error::{Result, ZoeError};
use crate::format::wal::OpKind;
use crate::format::{self, KeySpace};
use crate::kv::Flavour;
use crate::types::{Oid, Tid};

use super::table::{LogEntry, Table};
use super::{Core, Logged};

/// Decode the record frame `bytes` of `oid` at `tid`, and check it.
fn record(oid: Oid, tid: Tid, bytes: &[u8]) -> Result<Record> {
    let key = format::record_key(oid, tid);
    let frame = format::decode_frame(KeySpace::Records, &key, bytes).map_err(|e| {
        refused(
            e,
            &format!("record of oid {oid:?}, tid {tid:?}"),
            ZoeError::CorruptRecord { oid, tid },
        )
    })?;
    Ok(Record {
        tid,
        prev: frame.prev,
        data: frame.data.into_owned(),
    })
}

/// Return the error for a frame that `decode_frame` refused: a frame of a
/// newer format is a format error, any other refusal is `corrupt`.
fn refused(error: format::FrameError, row: &str, corrupt: ZoeError) -> ZoeError {
    match error {
        format::FrameError::Unsupported { offset, value } => ZoeError::Format(format!(
            "{row}: header byte {offset} is {value:#04x}, which this build cannot read"
        )),
        _ => corrupt,
    }
}

/// Decode the transaction row of the commit `tid` of the backlog.
fn txn_row(entry: &LogEntry, ops: &std::ops::Range<usize>, tid: Tid) -> Result<TxnMeta> {
    let op = &entry.ops[ops.start];
    debug_assert_eq!(op.kind, OpKind::Txn);
    let key = format::txn_key(tid);
    format::decode_frame(KeySpace::Txns, &key, entry.frame(op))
        .and_then(|frame| format::decode_txn(tid, &frame.data))
        .map_err(|e| {
            refused(
                e,
                &format!("transaction row {tid:?}"),
                ZoeError::CorruptTxn { tid },
            )
        })
}

impl Core {
    /// Return the published tid and the read lock of the table, or `None`
    /// when the backlog holds no commit: the read then goes to the tables
    /// alone, with no lock.
    fn view(&self) -> Option<(Tid, RwLockReadGuard<'_, Table>)> {
        let p = self.published();
        if self.empty() {
            return None;
        }
        Some((p, self.read_table()))
    }
}

/// Return the lower of two serials, or the one that exists.
fn lower(a: Option<Tid>, b: Option<Tid>) -> Option<Tid> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// The transaction rows of the tables merged with those of the backlog, in
/// tid order. The backlog rows are a copy of the backlog in the range: a
/// row of the tables in their tids is the same commit, which a slice moved.
struct Merged<'a> {
    tables: TxnIter<'a>,
    backlog: VecDeque<(Tid, Result<TxnMeta>)>,
    first: Tid,
    last: Tid,
    /// A row of the tables after the backlog, held until the backlog rows
    /// are given.
    held: Option<Result<TxnMeta>>,
    tables_done: bool,
}

impl Iterator for Merged<'_> {
    type Item = Result<TxnMeta>;

    fn next(&mut self) -> Option<Result<TxnMeta>> {
        loop {
            if self.held.is_some() || self.tables_done {
                if let Some((_, row)) = self.backlog.pop_front() {
                    return Some(row);
                }
                if let Some(held) = self.held.take() {
                    return Some(held);
                }
                if self.tables_done {
                    return None;
                }
            }
            match self.tables.next() {
                None => self.tables_done = true,
                Some(Ok(row)) if row.tid < self.first => return Some(Ok(row)),
                Some(Ok(row)) if row.tid <= self.last => {}
                Some(Ok(row)) => self.held = Some(Ok(row)),
                Some(Err(error)) => return Some(Err(error)),
            }
        }
    }
}

impl Backend for Logged {
    fn load_before(&self, oid: Oid, tid: Tid) -> Result<Option<Record>> {
        let core = &self.core;
        if let Some((p, table)) = core.view()
            && let Some((serial, frame)) = table.newest_before(oid, tid, p)
        {
            return record(oid, serial, frame).map(Some);
        }
        core.redb.load_before(oid, tid)
    }

    fn current_serials(&self, oids: &mut dyn Iterator<Item = Oid>) -> Result<Vec<Option<Tid>>> {
        let core = &self.core;
        let Some((p, table)) = core.view() else {
            return core.redb.current_serials(oids);
        };
        let mut serials = Vec::new();
        let mut missing = Vec::new();
        for oid in oids {
            match table.newest_before(oid, Tid::MAX, p) {
                Some((serial, _)) => serials.push(Some(serial)),
                None => {
                    missing.push((serials.len(), oid));
                    serials.push(None);
                }
            }
        }
        drop(table);
        if missing.is_empty() {
            return Ok(serials);
        }
        let found = core
            .redb
            .current_serials(&mut missing.iter().map(|&(_, oid)| oid))?;
        if found.len() != missing.len() {
            return Err(ZoeError::Backend(
                format!(
                    "the backend gave {} current serials for {} oids",
                    found.len(),
                    missing.len()
                )
                .into(),
            ));
        }
        for ((at, _), serial) in missing.into_iter().zip(found) {
            serials[at] = serial;
        }
        Ok(serials)
    }

    fn load_serial(&self, oid: Oid, tid: Tid) -> Result<Option<Bytes>> {
        let core = &self.core;
        if let Some((p, table)) = core.view()
            && let Some(frame) = table.record(oid, tid, p)
        {
            return record(oid, tid, frame).map(|record| Some(record.data));
        }
        core.redb.load_serial(oid, tid)
    }

    fn record_info(&self, oid: Oid, tid: Tid) -> Result<Option<RecordEncoding>> {
        let core = &self.core;
        if let Some((p, table)) = core.view()
            && let Some(bytes) = table.record(oid, tid, p)
        {
            let key = format::record_key(oid, tid);
            let frame = format::decode_frame(KeySpace::Records, &key, bytes).map_err(|e| {
                refused(
                    e,
                    &format!("record of oid {oid:?}, tid {tid:?}"),
                    ZoeError::CorruptRecord { oid, tid },
                )
            })?;
            return Ok(Some(RecordEncoding {
                codec: frame.codec,
                stored_len: frame.stored_len as u64,
                data_len: frame.data.len() as u64,
            }));
        }
        core.redb.record_info(oid, tid)
    }

    fn load_blob(&self, oid: Oid, tid: Tid) -> Result<Option<BlobRecord>> {
        let core = &self.core;
        if let Some((p, table)) = core.view()
            && let Some(bytes) = table.blob(oid, tid, p)
        {
            let key = format::record_key(oid, tid);
            let blob = format::decode_frame(KeySpace::Blobs, &key, bytes)
                .and_then(|frame| format::blob::decode_blob(&frame.data))
                .map_err(|e| {
                    refused(
                        e,
                        &format!("blob record of oid {oid:?}, tid {tid:?}"),
                        ZoeError::CorruptBlob { oid, tid },
                    )
                })?;
            return Ok(Some(blob));
        }
        core.redb.load_blob(oid, tid)
    }

    fn has_blobs(&self) -> Result<bool> {
        let core = &self.core;
        if core
            .counters
            .blobs
            .load(std::sync::atomic::Ordering::Acquire)
            > 0
        {
            return Ok(true);
        }
        core.redb.has_blobs()
    }

    fn last_tid(&self) -> Tid {
        self.core.redb.last_tid().max(self.core.published())
    }

    fn begin_write(&self) -> Result<Box<dyn WriteTxn + '_>> {
        self.core.redb.begin_write()
    }

    fn begin_batch(&self, plan: &BatchPlan) -> Result<Box<dyn WriteTxn + '_>> {
        self.batch(plan)
    }

    fn fold(&self, tid: Tid) -> Result<()> {
        let core = &self.core;
        core.wait_for(tid, |core| core.folded_to(tid)).map(drop)
    }

    fn backlog_bytes(&self) -> u64 {
        self.core.backlog_bytes()
    }

    fn cut_at_open(&self) -> Option<(Tid, Tid)> {
        self.cut
    }

    fn fold_failed(&self) -> bool {
        self.core.failed.load(std::sync::atomic::Ordering::Acquire)
    }

    fn set_observer(&self, observer: Arc<dyn Observer>) {
        let _ = self.core.observer.set(observer);
    }

    fn close(&self, fold: bool) {
        if self.closed.swap(true, std::sync::atomic::Ordering::AcqRel) {
            return;
        }
        self.stop();
        if !fold || self.fold_failed() {
            return;
        }
        if let Err(error) = self.fold_all() {
            tracing::warn!(
                %error,
                "the close could not fold the write-ahead log: the next open replays it"
            );
        }
    }

    fn iter_txns(&self, from: Tid, to: Tid) -> TxnIter<'_> {
        let core = &self.core;
        let backlog: VecDeque<(Tid, Result<TxnMeta>)> = match core.view() {
            None => VecDeque::new(),
            Some((p, table)) => table
                .commits(from, to, p)
                .map(|(entry, commit)| (commit.tid, txn_row(entry, &commit.ops, commit.tid)))
                .collect(),
        };
        let (Some(&(first, _)), Some(&(last, _))) = (backlog.front(), backlog.back()) else {
            return core.redb.iter_txns(from, to);
        };
        Box::new(Merged {
            tables: core.redb.iter_txns(from, to),
            backlog,
            first,
            last,
            held: None,
            tables_done: false,
        })
    }

    fn txns_before(&self, before: Tid, limit: usize) -> Result<Vec<TxnMeta>> {
        let core = &self.core;
        if before == Tid::ZERO || limit == 0 {
            return Ok(Vec::new());
        }
        let mut rows = Vec::new();
        let mut first = before;
        if let Some((p, table)) = core.view() {
            for (entry, commit) in table.commits(Tid::ZERO, Tid(before.0 - 1), p).rev() {
                if rows.len() == limit {
                    break;
                }
                rows.push(txn_row(entry, &commit.ops, commit.tid)?);
                first = commit.tid;
            }
        }
        if rows.len() < limit {
            rows.extend(core.redb.txns_before(first, limit - rows.len())?);
        }
        Ok(rows)
    }

    fn history(&self, oid: Oid, limit: usize) -> Result<Vec<RecordMeta>> {
        let core = &self.core;
        let mut history = Vec::new();
        let mut oldest = Tid::MAX;
        if let Some((p, table)) = core.view() {
            for (tid, frame) in table.versions(oid, p).rev() {
                if history.len() == limit {
                    return Ok(history);
                }
                let read = record(oid, tid, frame)?;
                history.push(RecordMeta {
                    tid,
                    prev: read.prev,
                    size: read.data.len() as u64,
                });
                oldest = tid;
            }
        }
        if history.len() < limit {
            // The tables can hold the versions of the table too, while a
            // slice moves them: those come once.
            let left = limit - history.len();
            history.extend(
                core.redb
                    .history(oid, limit)?
                    .into_iter()
                    .filter(|version| version.tid < oldest)
                    .take(left),
            );
        }
        Ok(history)
    }

    fn load_before_next(&self, oid: Oid, tid: Tid) -> Result<(Option<Record>, Option<Tid>)> {
        let core = &self.core;
        let mut next = None;
        if let Some((p, table)) = core.view() {
            next = table.first_from(oid, tid, p);
            if let Some((serial, frame)) = table.newest_before(oid, tid, p) {
                // The versions of the table are newer than each version that
                // only the tables hold: no lookup in redb.
                return Ok((Some(record(oid, serial, frame)?), next));
            }
        }
        let (before, after) = core.redb.load_before_next(oid, tid)?;
        Ok((before, lower(next, after)))
    }

    fn txn_oids(&self, from: Tid, to: Tid) -> Result<BTreeMap<Tid, Vec<Oid>>> {
        let core = &self.core;
        let mut backlog: BTreeMap<Tid, Vec<Oid>> = BTreeMap::new();
        if from <= to
            && let Some((p, table)) = core.view()
        {
            for (entry, commit) in table.commits(from, to, p) {
                let mut oids: Vec<Oid> = entry.ops[commit.ops.clone()]
                    .iter()
                    .filter(|op| op.kind == OpKind::Record)
                    .map(|op| op.oid)
                    .collect();
                if !oids.is_empty() {
                    oids.sort_unstable();
                    backlog.insert(commit.tid, oids);
                }
            }
        }
        let mut oids = core.redb.txn_oids(from, to)?;
        oids.extend(backlog);
        Ok(oids)
    }

    fn indexed_tids(&self, from: Tid, to: Tid, limit: usize) -> Result<Vec<Tid>> {
        let core = &self.core;
        let mut backlog = Vec::new();
        if from <= to
            && limit > 0
            && let Some((p, table)) = core.view()
        {
            for (_, commit) in table.commits(from, to, p) {
                if commit.records > 0 {
                    backlog.push(commit.tid);
                    if backlog.len() == limit {
                        break;
                    }
                }
            }
        }
        let mut tids = core.redb.indexed_tids(from, to, limit)?;
        if backlog.is_empty() {
            return Ok(tids);
        }
        tids.extend(backlog);
        tids.sort_unstable();
        tids.dedup();
        tids.truncate(limit);
        Ok(tids)
    }

    fn current_from(&self, from: Oid) -> Result<Option<(Oid, Record)>> {
        let core = &self.core;
        let mut found: Option<(Oid, Record)> = None;
        if let Some((p, table)) = core.view()
            && let Some(oid) = table.first_oid_from(from, p)
        {
            let (serial, frame) = table
                .newest_before(oid, Tid::MAX, p)
                .expect("the oid has a version");
            found = Some((oid, record(oid, serial, frame)?));
        }
        if let Some((oid, _)) = &found
            && *oid == from
        {
            return Ok(found);
        }
        let tables = core.redb.current_from(from)?;
        Ok(match (found, tables) {
            (Some(table), Some(tables)) if tables.0 < table.0 => Some(tables),
            (Some(table), _) => Some(table),
            (None, tables) => tables,
        })
    }

    fn pack_watermark(&self) -> Result<Tid> {
        self.core.redb.pack_watermark()
    }

    fn object_count(&self) -> Option<u64> {
        self.core.redb.object_count()
    }

    fn record_keys(&self, from: Oid, limit: usize) -> Result<Vec<(Oid, Tid)>> {
        let core = &self.core;
        let backlog = match core.view() {
            Some((p, table)) => table.keys(from, limit, p),
            None => Vec::new(),
        };
        let tables = core.redb.record_keys(from, limit)?;
        if backlog.is_empty() {
            return Ok(tables);
        }
        let mut merged: Vec<(Oid, Tid)> = Vec::with_capacity(tables.len() + backlog.len());
        let (mut a, mut b) = (
            tables.into_iter().peekable(),
            backlog.into_iter().peekable(),
        );
        loop {
            let key = match (a.peek(), b.peek()) {
                (Some(x), Some(y)) if x == y => {
                    b.next();
                    a.next()
                }
                (Some(x), Some(y)) if x < y => a.next(),
                (Some(_), Some(_)) | (None, Some(_)) => b.next(),
                (Some(_), None) => a.next(),
                (None, None) => break,
            };
            let key = key.expect("a key");
            if merged.len() >= limit && merged.last().is_some_and(|&(last, _)| last != key.0) {
                break;
            }
            merged.push(key);
        }
        Ok(merged)
    }

    fn txn_records(&self, tid: Tid) -> Result<Option<TxnRecords>> {
        let core = &self.core;
        if let Some((p, table)) = core.view()
            && let Some((entry, commit)) = table.commit(tid, p)
        {
            let row = txn_row(entry, &commit.ops, tid)?;
            let mut records = Vec::with_capacity(commit.records);
            for op in &entry.ops[commit.ops.clone()] {
                if op.kind == OpKind::Record {
                    records.push((op.oid, record(op.oid, tid, entry.frame(op))?.data));
                }
            }
            records.sort_by_key(|&(oid, _)| oid);
            return Ok(Some((row, records)));
        }
        core.redb.txn_records(tid)
    }

    fn sync(&self) -> Result<()> {
        self.core.redb.sync()
    }

    #[cfg(feature = "testing")]
    fn read_stored(&self, key: crate::testing::Key) -> Result<Option<Bytes>> {
        self.core.redb.read_stored(key)
    }

    #[cfg(feature = "testing")]
    fn write_stored(&self, key: crate::testing::Key, bytes: &[u8]) -> Result<()> {
        self.core.redb.write_stored(key, bytes)
    }

    #[cfg(feature = "testing")]
    fn write_meta_stored(&self, key: &str, value: Option<&[u8]>) -> Result<()> {
        self.core.redb.write_meta_stored(key, value)
    }

    #[cfg(feature = "testing")]
    fn write_kv_stored(
        &self,
        flavour: Option<Flavour>,
        row: &[u8],
        bytes: Option<&[u8]>,
    ) -> Result<()> {
        self.core.redb.write_kv_stored(flavour, row, bytes)
    }

    #[cfg(feature = "testing")]
    fn remove_index_key_for_test(&self, tid: Tid, oid: Oid) -> Result<()> {
        self.core.redb.remove_index_key_for_test(tid, oid)
    }

    #[cfg(feature = "testing")]
    fn remove_record_for_test(&self, oid: Oid, tid: Tid) -> Result<()> {
        self.core.redb.remove_record_for_test(oid, tid)
    }

    fn sequence_end(&self, name: &str) -> Result<Option<u64>> {
        self.core.redb.sequence_end(name)
    }

    fn sequence_names(&self) -> Result<Vec<String>> {
        self.core.redb.sequence_names()
    }

    fn kv_row(&self, flavour: Flavour, row: &[u8]) -> Result<Option<Bytes>> {
        let core = &self.core;
        if let Some((p, table)) = core.view()
            && let Some(op) = table.row(flavour, row, p)
        {
            return Ok(op.map(<[u8]>::to_vec));
        }
        core.redb.kv_row(flavour, row)
    }

    fn kv_rows(
        &self,
        flavour: Flavour,
        from: Bound<&[u8]>,
        to: Bound<&[u8]>,
        limit: usize,
    ) -> Result<Vec<(Bytes, Bytes)>> {
        let core = &self.core;
        if limit == 0 {
            return Ok(Vec::new());
        }
        // The table first: its ops up to `limit` writes.
        let (ops, cut) = match core.view() {
            Some((p, table)) => table.rows(flavour, from, to, p, limit),
            None => (Vec::new(), false),
        };
        if ops.is_empty() {
            return core.redb.kv_rows(flavour, from, to, limit);
        }
        // When the table gave `limit` writes, no row after the last of them
        // can be in the result.
        let last_op = ops.last().map(|(key, _)| key.clone()).expect("an op");
        let bound: Bound<&[u8]> = if cut { Included(&last_op) } else { to };
        let mut out: Vec<(Bytes, Bytes)> = Vec::new();
        let mut ops = ops.into_iter().peekable();
        let mut chunk: VecDeque<(Bytes, Bytes)> = VecDeque::new();
        let mut cursor: Option<Bytes> = None;
        let mut tables_done = false;
        while out.len() < limit {
            if chunk.is_empty() && !tables_done {
                let start = match &cursor {
                    Some(key) => Excluded(key.as_slice()),
                    None => from,
                };
                let rows = if super::table::empty_range(start, bound) {
                    Vec::new()
                } else {
                    core.redb.kv_rows(flavour, start, bound, limit)?
                };
                tables_done = rows.len() < limit;
                if let Some((key, _)) = rows.last() {
                    cursor = Some(key.clone());
                }
                chunk.extend(rows);
            }
            let take_op = match (chunk.front(), ops.peek()) {
                (None, None) => break,
                (Some(row), Some((key, _))) => {
                    if row.0 == *key {
                        chunk.pop_front();
                        true
                    } else {
                        row.0 > *key
                    }
                }
                (None, Some(_)) => true,
                (Some(_), None) => false,
            };
            if take_op {
                let (key, frame) = ops.next().expect("an op");
                if let Some(frame) = frame {
                    out.push((key, frame));
                }
            } else {
                out.push(chunk.pop_front().expect("a row"));
            }
        }
        Ok(out)
    }

    fn kv_expiry_has(&self, key: &[u8]) -> Result<bool> {
        let core = &self.core;
        if let Some((p, table)) = core.view()
            && table.has_expiry(key, p)
        {
            return Ok(true);
        }
        core.redb.kv_expiry_has(key)
    }

    fn has_kv(&self, flavour: Flavour) -> Result<bool> {
        let core = &self.core;
        let index = match flavour {
            Flavour::Ephemeral => 0,
            Flavour::Durable => 1,
        };
        if core.counters.kv[index].load(std::sync::atomic::Ordering::Acquire) > 0 {
            return Ok(true);
        }
        core.redb.has_kv(flavour)
    }
}
