//! Pack: the removal of the records that no reader at or after the pack
//! tid needs.
//!
//! A pack at `at` packs at T, the lower of `at` and the last tid. It keeps
//! every record after T. For each object that a reader at or after T can
//! reach, it also keeps the newest record at or before T. It removes all
//! the other records at or before T, with their blob records, and then
//! their blob files.
//!
//! The pack runs in three steps:
//!
//! 1. Under the commit sequencer, it reads the last tid, and from then on
//!    each finish notes the oids that it writes.
//! 2. The mark: the roots are the root object and every object that a
//!    record after T and up to that last tid refers to. From each root,
//!    the mark follows the references of the newest record at or before
//!    T. The references come from `zoe_pickle::persistent_ids`, never from
//!    an unpickler. Weak references and references into another database
//!    keep nothing. A record of the mark that does not decode fails the
//!    pack before it removes anything.
//! 3. The batches: a scan of the record keys in key order selects the next
//!    records to remove, at most `batch` of them, the oldest record of an
//!    oid first. One backend write transaction removes them, their blob
//!    records, and lowers the oid count of their transaction rows or
//!    removes the rows that keep no record. The first batch also writes
//!    `pack_watermark`.
//!
//! A finish during the pack writes records after T only, so the records
//! that the mark reads do not change. Before each batch, under the commit
//! sequencer, the pack reads the records that the finishes noted and marks
//! what they refer to. No finish can come between that mark and the
//! removal. So a record committed during the pack that links an object
//! again keeps it, if the record is durable before the batch that removes
//! the object.
//!
//! The blob files go after their blob records. Before the backend commit
//! of a batch, an intent file in `blobs/tmp/` names the blob files that
//! the batch can orphan. After the commit, the pack removes the files,
//! syncs their directories and removes the intent file. A crash between
//! the commit and the removal leaves the intent file, and the next open
//! removes the files that have no blob record.

use std::collections::{BTreeMap, HashSet};
use std::sync::{Mutex, MutexGuard};

use zoe_pickle::{PersistentId, decode_class_name, persistent_ids};

use crate::backend::TxnMeta;
use crate::error::{Result, ZoeError};
use crate::store::Store;
use crate::types::{Oid, Tid};

/// The oid of the root object.
const ROOT: Oid = Oid(0);

/// The number of records that `Store::pack` removes in one batch.
const BATCH: usize = 1000;

/// The number of record keys that one read of the key scan asks for.
const KEY_CHUNK: usize = 4096;

/// What a pack removed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PackReport {
    /// The number of records that the pack removed.
    pub records: u64,
    /// The size of the pickles of these records, in bytes.
    pub bytes: u64,
    /// The number of objects that lost their last record.
    pub objects: u64,
    /// The number of blob records that the pack removed with their files.
    pub blobs: u64,
    /// The size of these blob files, in bytes.
    pub blob_bytes: u64,
    /// The removed records by class. The key is `module.name` from the
    /// class pickle. It is `None` for a deletion, for a persistent class
    /// and for a class pickle that does not decode.
    pub by_class: BTreeMap<Option<String>, ClassReport>,
}

/// What a pack removed of one class.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ClassReport {
    /// The number of records.
    pub records: u64,
    /// The size of their pickles, in bytes.
    pub bytes: u64,
}

/// The tid and the oids of each finish that a pack must read.
type Noted = Vec<(Tid, Vec<Oid>)>;

/// The state that a pack shares with the finishes of its store.
#[derive(Debug, Default)]
pub(crate) struct PackState {
    /// A pack holds this lock from start to end, so one pack runs at a
    /// time.
    running: Mutex<()>,
    /// The tid and the oids of each finish since the pack started. It is
    /// `None` when no pack runs.
    written: Mutex<Option<Noted>>,
}

impl PackState {
    /// Note the oids that the finish `tid` wrote, when a pack runs. A
    /// finish calls this under the commit sequencer, after its backend
    /// commit.
    pub(crate) fn note(&self, tid: Tid, oids: impl Iterator<Item = Oid>) {
        if let Some(written) = lock(&self.written).as_mut() {
            written.push((tid, oids.collect()));
        }
    }

    /// Return the finishes noted since the last call.
    fn take(&self) -> Noted {
        lock(&self.written)
            .as_mut()
            .map(std::mem::take)
            .unwrap_or_default()
    }
}

/// Lock `mutex`. A panic of another thread does not hide the state.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// Stops the notes of the finishes when the pack ends, also on an error.
struct Noting<'s>(&'s PackState);

impl Drop for Noting<'_> {
    fn drop(&mut self) {
        *lock(&self.0.written) = None;
    }
}

impl Store {
    /// Pack the store at `at`: remove the records that no reader at or
    /// after `at` needs, and the objects that such a reader cannot reach.
    /// Return what the pack removed.
    ///
    /// This is `pack_in_batches` with batches of 1,000 records and
    /// nothing between them.
    pub fn pack(&self, at: Tid) -> Result<PackReport> {
        self.pack_in_batches(at, BATCH, || {})
    }

    /// Pack the store at `at`, as `pack` does, in batches of at most
    /// `batch` records. `between` runs after each batch. It runs without
    /// the commit sequencer and without a backend transaction, so reads
    /// and commits can run then. Only a second pack waits. Readers and
    /// commits continue during the whole pack.
    ///
    /// The pack packs at the lower of `at` and the last tid. A pack at a
    /// tid before the last pack does nothing. A batch of 0 is a batch of
    /// 1.
    ///
    /// A record that the mark reads and that does not decode fails the
    /// pack with `Format` before the pack removes anything. Such a
    /// record committed during the pack stops the pack before its next
    /// batch. A halted store refuses the pack with `Halted`, and a
    /// read-only store with `ReadOnly`.
    pub fn pack_in_batches(
        &self,
        at: Tid,
        batch: usize,
        mut between: impl FnMut(),
    ) -> Result<PackReport> {
        self.writable()?;
        let _running = lock(&self.packing.running);
        let (tid, last, watermark) = {
            let sequencer = self.lock_sequencer();
            let last = (*sequencer).ok_or(ZoeError::Halted)?;
            let watermark = self.backend.pack_watermark()?;
            if at < watermark {
                return Ok(PackReport::default());
            }
            *lock(&self.packing.written) = Some(Vec::new());
            (at.min(last), last, watermark)
        };
        let _noting = Noting(&self.packing);
        let mut pack = Pack {
            store: self,
            tid,
            reachable: HashSet::new(),
            written: HashSet::new(),
            cursor: Some(Oid(0)),
            watermark: (tid > watermark).then_some(tid),
            report: PackReport::default(),
        };
        pack.mark_roots(last)?;
        loop {
            let removals = pack.plan(batch.max(1))?;
            if removals.is_empty() {
                if pack.watermark.is_some() {
                    pack.remove(Vec::new())?;
                }
                break;
            }
            pack.remove(removals)?;
            between();
        }
        Ok(pack.report)
    }
}

/// One record that a batch removes.
struct Removal {
    oid: Oid,
    tid: Tid,
    /// The record is the newest record of its oid at or before T.
    newest: bool,
    /// The key scan found no record of the oid after T. With no finish of
    /// the oid during the pack, the removal of the newest record removes
    /// the object.
    no_later: bool,
    /// The size of the pickle.
    size: u64,
    /// The class of the record, for the report.
    class: Option<String>,
    /// The size of the blob file, if the record has a blob.
    blob: Option<u64>,
}

/// One pack in progress.
struct Pack<'s> {
    store: &'s Store,
    /// The pack tid T.
    tid: Tid,
    /// The oids that a reader at or after T can reach.
    reachable: HashSet<Oid>,
    /// The oids that a finish wrote during the pack. Each has a record
    /// after T.
    written: HashSet<Oid>,
    /// The first oid that the key scan has not finished. `None` at the end.
    cursor: Option<Oid>,
    /// The watermark that the next batch writes, if it must write one.
    watermark: Option<Tid>,
    report: PackReport,
}

impl Pack<'_> {
    /// Mark the root and each object that a record after T, up to `last`,
    /// refers to, and what they reach.
    fn mark_roots(&mut self, last: Tid) -> Result<()> {
        let backend = &self.store.backend;
        let mut todo = vec![ROOT];
        let mut from = Some(Oid(0));
        while let Some(start) = from {
            let keys = backend.record_keys(start, KEY_CHUNK)?;
            let Some(&(end, _)) = keys.last() else {
                break;
            };
            for &(oid, tid) in &keys {
                if tid > self.tid
                    && tid <= last
                    && let Some(data) = backend.load_serial(oid, tid)?
                {
                    self.references(oid, tid, &data, &mut todo)?;
                }
            }
            from = end.0.checked_add(1).map(Oid);
        }
        self.mark(todo)
    }

    /// Mark the oids of `todo` and what their newest records at or before
    /// T reach.
    fn mark(&mut self, mut todo: Vec<Oid>) -> Result<()> {
        let before = Tid(self.tid.0.saturating_add(1));
        while let Some(oid) = todo.pop() {
            if !self.reachable.insert(oid) {
                continue;
            }
            if let Some(record) = self.store.backend.load_before(oid, before)? {
                self.references(oid, record.tid, &record.data, &mut todo)?;
            }
        }
        Ok(())
    }

    /// Mark what the records of the finishes since the last call refer to.
    fn mark_written(&mut self) -> Result<()> {
        let mut todo = Vec::new();
        for (tid, oids) in self.store.packing.take() {
            for oid in oids {
                self.written.insert(oid);
                if let Some(data) = self.store.backend.load_serial(oid, tid)? {
                    self.references(oid, tid, &data, &mut todo)?;
                }
            }
        }
        self.mark(todo)
    }

    /// Add to `todo` the oids that the record `data` of `oid` at `tid`
    /// refers to and that are not marked. A deletion refers to nothing.
    fn references(&self, oid: Oid, tid: Tid, data: &[u8], todo: &mut Vec<Oid>) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        let refused = |message: String| {
            ZoeError::Format(format!(
                "pack cannot read the references of the record of oid {oid:?}, tid {tid:?}: {message}"
            ))
        };
        for id in persistent_ids(data).map_err(|e| refused(e.to_string()))? {
            let PersistentId::Oid(bytes) = id else {
                continue;
            };
            let bytes: [u8; 8] = bytes
                .try_into()
                .map_err(|_| refused("an oid is not 8 bytes".to_owned()))?;
            let target = Oid(u64::from_be_bytes(bytes));
            if !self.reachable.contains(&target) {
                todo.push(target);
            }
        }
        Ok(())
    }

    /// Select the next records to remove, at most `batch` of them, in key
    /// order. Of a marked oid, every record at or before T but the newest
    /// goes. Of another oid, every record at or before T goes.
    fn plan(&mut self, batch: usize) -> Result<Vec<Removal>> {
        let mut removals = Vec::new();
        while let Some(from) = self.cursor {
            let keys = self.store.backend.record_keys(from, KEY_CHUNK)?;
            if keys.is_empty() {
                self.cursor = None;
                break;
            }
            for keys in keys.chunk_by(|a, b| a.0 == b.0) {
                let oid = keys[0].0;
                let older: Vec<Tid> = keys
                    .iter()
                    .map(|&(_, tid)| tid)
                    .filter(|&tid| tid <= self.tid)
                    .collect();
                let no_later = keys.iter().all(|&(_, tid)| tid <= self.tid);
                let kept = self.reachable.contains(&oid);
                let count = older.len() - usize::from(kept && !older.is_empty());
                let room = batch - removals.len();
                for &tid in older.iter().take(count.min(room)) {
                    let newest = older.last() == Some(&tid);
                    removals.push(self.removal(oid, tid, newest, no_later)?);
                }
                if count > room {
                    self.cursor = Some(oid);
                    return Ok(removals);
                }
                self.cursor = oid.0.checked_add(1).map(Oid);
                if removals.len() == batch {
                    return Ok(removals);
                }
            }
        }
        Ok(removals)
    }

    /// Read what the report needs of the record of `oid` at `tid`.
    fn removal(&self, oid: Oid, tid: Tid, newest: bool, no_later: bool) -> Result<Removal> {
        let backend = &self.store.backend;
        // Only this pack removes records at or before T, so the record is
        // there.
        let data = backend
            .load_serial(oid, tid)?
            .ok_or(ZoeError::NotFound { oid })?;
        let blob = backend.load_blob(oid, tid)?.map(|blob| blob.size);
        Ok(Removal {
            oid,
            tid,
            newest,
            no_later,
            size: data.len() as u64,
            class: class_name(&data),
            blob,
        })
    }

    /// Remove `removals` in one backend write transaction, then their blob
    /// files. With no removal, write only the watermark.
    fn remove(&mut self, removals: Vec<Removal>) -> Result<()> {
        let store = self.store;
        let blobs = store.blobs();
        let mut by_tid: BTreeMap<Tid, Vec<Oid>> = BTreeMap::new();
        for removal in removals.iter().filter(|r| r.blob.is_some()) {
            by_tid.entry(removal.tid).or_default().push(removal.oid);
        }
        let mut intents = Vec::new();
        for (tid, oids) in &by_tid {
            intents.push(blobs.intend_removal(*tid, oids)?);
        }

        // Most of the marking happens here, outside the commit sequencer.
        self.mark_written()?;
        let removed: Vec<Removal> = {
            let mut sequencer = store.lock_sequencer();
            if sequencer.is_none() {
                return Err(ZoeError::Halted);
            }
            // No finish runs from here to the commit below.
            self.mark_written()?;
            let removed: Vec<Removal> = removals
                .into_iter()
                .filter(|r| !(r.newest && self.reachable.contains(&r.oid)))
                .collect();
            self.commit(&removed, &mut sequencer)?;
            removed
        };
        self.account(&removed);

        let files: Vec<(Oid, Tid)> = removed
            .iter()
            .filter(|r| r.blob.is_some())
            .map(|r| (r.oid, r.tid))
            .collect();
        blobs.remove_files(&files)?;
        for intent in intents {
            blobs.finished(Some(intent));
        }
        Ok(())
    }

    /// Commit the removal of `removed`: the records, the blob records and
    /// the changes to the transaction rows, with the watermark if it is
    /// due. `sequencer` is the commit sequencer, which the caller holds.
    ///
    /// An error of the backend write leaves the outcome unknown, as an
    /// error in `finish` does. So it empties `sequencer`, and the store
    /// refuses every later finish and pack with `Halted` until it is
    /// reopened.
    fn commit(&mut self, removed: &[Removal], sequencer: &mut Option<Tid>) -> Result<()> {
        let backend = &self.store.backend;
        let mut counts: BTreeMap<Tid, u64> = BTreeMap::new();
        for removal in removed {
            *counts.entry(removal.tid).or_default() += 1;
        }
        // Only this pack changes the rows at or before T, so a read
        // outside the write transaction gives the current rows.
        let mut rows = Vec::new();
        for (&tid, &count) in &counts {
            let row = backend
                .iter_txns(tid, tid)
                .next()
                .unwrap_or(Err(ZoeError::CorruptTxn { tid }))?;
            let left = row
                .oid_count
                .checked_sub(count)
                .ok_or(ZoeError::CorruptTxn { tid })?;
            rows.push((row, left));
        }

        let written = (|| {
            let mut write = backend.begin_write()?;
            for removal in removed {
                write.remove_record(removal.oid, removal.tid)?;
                if removal.blob.is_some() {
                    write.remove_blob(removal.oid, removal.tid)?;
                }
            }
            for (row, left) in rows {
                if left == 0 {
                    write.remove_txn(row.tid)?;
                } else {
                    write.replace_txn(&TxnMeta {
                        oid_count: left,
                        ..row
                    })?;
                }
            }
            if let Some(watermark) = self.watermark {
                write.put_pack_watermark(watermark)?;
            }
            write.commit()
        })();
        if let Err(error) = written {
            *sequencer = None;
            return Err(error);
        }
        self.watermark = None;
        Ok(())
    }

    /// Add `removed` to the report.
    fn account(&mut self, removed: &[Removal]) {
        let report = &mut self.report;
        for removal in removed {
            report.records += 1;
            report.bytes += removal.size;
            if removal.newest && removal.no_later && !self.written.contains(&removal.oid) {
                report.objects += 1;
            }
            if let Some(size) = removal.blob {
                report.blobs += 1;
                report.blob_bytes += size;
            }
            let class = report.by_class.entry(removal.class.clone()).or_default();
            class.records += 1;
            class.bytes += removal.size;
        }
    }
}

/// Return `module.name` of the class of the record `data`, or `None` when
/// the class pickle names no class or does not decode. Only the class
/// pickle is decoded, so a large state takes no memory here.
fn class_name(data: &[u8]) -> Option<String> {
    let (module, name) = decode_class_name(data).ok()??;
    Some(format!("{module}.{name}"))
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;

    use super::*;
    use crate::format::blob::{blob_name, oid_dir};
    use crate::vfs::MemVfs;

    /// Return a record of class `m.C` whose state is a dict that holds a
    /// reference to each oid of `refs`.
    fn record(refs: &[Oid]) -> Vec<u8> {
        let mut p = b"\x80\x03cm\nC\nq\x00.\x80\x03}".to_vec();
        if !refs.is_empty() {
            p.push(b'(');
            for (i, oid) in refs.iter().enumerate() {
                p.extend_from_slice(b"K");
                p.push(u8::try_from(i).unwrap());
                p.extend_from_slice(b"C\x08");
                p.extend_from_slice(&oid.0.to_be_bytes());
                p.extend_from_slice(b"h\x00\x86Q");
            }
            p.push(b'u');
        }
        p.push(b'.');
        p
    }

    /// Commit `writes` with the current serial of each oid.
    fn commit(store: &Store, writes: &[(Oid, Vec<u8>)]) -> Tid {
        let mut txn = store.tpc_begin(b"user", b"", b"").unwrap();
        for (oid, data) in writes {
            let serial = store.load(*oid).map_or(Tid::ZERO, |(_, serial)| serial);
            txn.store(*oid, serial, data).unwrap();
        }
        txn.vote().unwrap();
        txn.finish().unwrap()
    }

    fn is_removed(store: &Store, oid: Oid, tid: Tid) -> bool {
        matches!(store.load_serial(oid, tid), Err(ZoeError::NotFound { .. }))
    }

    /// The tid of each transaction that the iterator gives, with the oids
    /// of its records.
    fn transactions(store: &Store) -> Vec<(Tid, Vec<Oid>)> {
        store
            .iterator(None, None)
            .unwrap()
            .map(|txn| {
                let txn = txn.unwrap();
                (txn.tid, txn.records.iter().map(|r| r.oid).collect())
            })
            .collect()
    }

    #[test]
    fn a_record_committed_during_the_pack_keeps_what_it_links_again() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let [a, g1, g2, g3] = [1, 2, 3, 4].map(Oid);
        let t1 = commit(
            &store,
            &[
                (ROOT, record(&[a, g1, g2])),
                (a, record(&[])),
                (g1, record(&[])),
                (g2, record(&[g3])),
                (g3, record(&[])),
            ],
        );
        let t2 = commit(&store, &[(ROOT, record(&[a]))]);

        // After the first batch, a commit links g2 again from a record
        // that it read before the pack. g2 holds g3.
        let mut linked = None;
        let report = store
            .pack_in_batches(t2, 2, || {
                if linked.is_none() {
                    linked = Some(commit(&store, &[(a, record(&[g2]))]));
                }
            })
            .unwrap();

        assert!(is_removed(&store, ROOT, t1));
        assert!(is_removed(&store, g1, t1));
        for oid in [a, g2, g3] {
            assert_eq!(
                store.load_serial(oid, t1).unwrap(),
                store.load_before(oid, Tid(t2.0 + 1)).unwrap().unwrap().data
            );
        }
        let linked = linked.unwrap();
        let read = store.load_before(g2, Tid(linked.0 + 1)).unwrap().unwrap();
        assert_eq!(read.serial, t1);
        assert_eq!((report.records, report.objects), (2, 1), "{report:?}");
    }

    #[test]
    fn the_newest_deletion_of_a_reachable_oid_stays() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let x = Oid(1);
        let t1 = commit(&store, &[(ROOT, record(&[x])), (x, record(&[]))]);
        let t2 = Tid(t1.0 + 1);
        let mut txn = store.tpc_begin_restore(t2, b"", b"", b"").unwrap();
        txn.restore(x, None).unwrap();
        txn.vote().unwrap();
        txn.finish().unwrap();

        let report = store.pack(t2).unwrap();

        assert_eq!(report.records, 1, "{report:?}");
        assert!(is_removed(&store, x, t1));
        let history: Vec<_> = store
            .history(x, 10)
            .unwrap()
            .iter()
            .map(|entry| (entry.tid, entry.size))
            .collect();
        assert_eq!(history, [(t2, 0)]);
        assert!(matches!(store.load(x), Err(ZoeError::NotFound { .. })));
    }

    #[test]
    fn the_transaction_rows_follow_the_records() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let (a, g) = (Oid(1), Oid(2));
        let t1 = commit(
            &store,
            &[(ROOT, record(&[a, g])), (a, record(&[])), (g, record(&[]))],
        );
        commit(&store, &[(ROOT, record(&[a]))]);
        let t3 = commit(&store, &[(ROOT, record(&[a]))]);

        let report = store.pack(t3).unwrap();

        // t1 keeps a. t2 keeps nothing, so its row is gone.
        assert_eq!(transactions(&store), [(t1, vec![a]), (t3, vec![ROOT])]);
        assert_eq!(store.history(ROOT, 10).unwrap().len(), 1);
        let classes: Vec<_> = report.by_class.iter().collect();
        assert_eq!(
            classes,
            [(
                &Some("m.C".to_owned()),
                &ClassReport {
                    records: 3,
                    bytes: report.bytes
                }
            )]
        );
        assert_eq!((report.records, report.objects), (3, 1), "{report:?}");
    }

    #[test]
    fn reads_stay_consistent_while_a_pack_removes_rows() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let oids: Vec<Oid> = (1..=6).map(Oid).collect();
        let mut writes = vec![(ROOT, record(&oids))];
        writes.extend(oids.iter().map(|&oid| (oid, record(&[]))));
        commit(&store, &writes);
        for &oid in &oids {
            commit(&store, &[(oid, record(&[]))]);
        }
        let last = commit(&store, &[(ROOT, record(&oids[..1]))]);

        // An iterator from before the pack, read one transaction per
        // batch, and every history in each batch.
        let mut iterator = store.iterator(None, None).unwrap();
        let mut seen = 0;
        store
            .pack_in_batches(last, 1, || {
                if let Some(txn) = iterator.next() {
                    txn.unwrap();
                    seen += 1;
                }
                for oid in std::iter::once(ROOT).chain(oids.iter().copied()) {
                    match store.history(oid, 10) {
                        Ok(_) | Err(ZoeError::NotFound { .. }) => {}
                        Err(e) => panic!("{oid:?}: {e}"),
                    }
                }
            })
            .unwrap();
        for txn in iterator {
            txn.unwrap();
            seen += 1;
        }
        assert!(seen > 0);
        for (_, records) in transactions(&store) {
            assert!(!records.is_empty());
        }
    }

    #[test]
    fn history_leaves_out_a_record_that_a_pack_removed_with_its_row() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let g = Oid(1);
        let t1 = commit(&store, &[(ROOT, record(&[g])), (g, record(&[]))]);
        let t2 = commit(&store, &[(ROOT, record(&[]))]);
        let t3 = commit(&store, &[(g, record(&[]))]);
        // The versions come from a snapshot before the pack, and the rows
        // from a read after it.
        let versions = store.backend.history(g, 10).unwrap();
        assert_eq!(versions.len(), 2);

        store.pack(t2).unwrap();

        let entries = store.history_entries(g, versions, 10).unwrap();
        let tids: Vec<Tid> = entries.iter().map(|entry| entry.tid).collect();
        assert_eq!(tids, [t3]);
        assert!(is_removed(&store, g, t1));
    }

    #[test]
    fn a_pack_before_the_last_pack_does_nothing() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let a = Oid(1);
        let t1 = commit(&store, &[(ROOT, record(&[a])), (a, record(&[]))]);
        let t2 = commit(&store, &[(a, record(&[]))]);
        let t3 = commit(&store, &[(a, record(&[]))]);

        assert_eq!(store.pack(t2).unwrap().records, 1);
        assert_eq!(store.backend.pack_watermark().unwrap(), t2);
        assert_eq!(store.pack(t1).unwrap(), PackReport::default());
        assert_eq!(store.load_serial(a, t2).unwrap(), record(&[]));
        assert_eq!(store.backend.pack_watermark().unwrap(), t2);

        // A pack at a tid after the last tid packs at the last tid.
        assert_eq!(store.pack(Tid::MAX).unwrap().records, 1);
        assert_eq!(store.backend.pack_watermark().unwrap(), t3);
        assert!(is_removed(&store, a, t2));
    }

    #[test]
    fn an_unreadable_record_committed_during_the_pack_stops_it() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let [a, g1, g2] = [1, 2, 3].map(Oid);
        let t1 = commit(
            &store,
            &[
                (ROOT, record(&[a, g1, g2])),
                (a, record(&[])),
                (g1, record(&[])),
                (g2, record(&[])),
            ],
        );
        let t2 = commit(&store, &[(ROOT, record(&[a]))]);
        let mut unreadable = record(&[g2]);
        unreadable.insert(unreadable.len() - 2, 0xff);

        let mut written = None;
        let result = store.pack_in_batches(t2, 1, || {
            if written.is_none() {
                written = Some(commit(&store, &[(a, unreadable.clone())]));
            }
        });

        let written = written.unwrap();
        let named = format!("oid {a:?}, tid {written:?}");
        assert!(
            matches!(&result, Err(ZoeError::Format(text)) if text.contains(&named)),
            "{result:?}"
        );
        // The first batch removed the old root, and nothing after it.
        assert!(is_removed(&store, ROOT, t1));
        for oid in [g1, g2] {
            assert_eq!(store.load_serial(oid, t1).unwrap(), record(&[]));
        }
    }

    /// Commit `data` for `oid` with a blob that holds `bytes`.
    fn commit_blob(store: &Store, oid: Oid, data: &[u8], bytes: &[u8]) -> Tid {
        let serial = store.load(oid).map_or(Tid::ZERO, |(_, serial)| serial);
        let mut file = store.new_blob_file().unwrap();
        file.write_all(bytes).unwrap();
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.store_blob(oid, serial, data, file).unwrap();
        txn.vote().unwrap();
        txn.finish().unwrap()
    }

    #[test]
    fn a_pack_killed_after_its_commit_ends_at_the_next_open() {
        let vfs = MemVfs::new();
        let (f, g) = (Oid(1), Oid(2));
        let (t1, t2) = {
            let store = Store::open_vfs(vfs.clone()).unwrap();
            commit(&store, &[(ROOT, record(&[f, g]))]);
            let t1 = commit_blob(&store, f, &record(&[]), b"f1");
            commit_blob(&store, g, &record(&[]), b"g1");
            commit(&store, &[(ROOT, record(&[f]))]);
            let t2 = commit_blob(&store, f, &record(&[]), b"f2");
            (t1, t2)
        };
        let vfs = vfs.durable_copy();
        let g1 = store_blob_tid(&vfs, g);

        // The process dies when the pack syncs the directory of a removed
        // blob file, after the backend commit.
        let store = Store::open_vfs(vfs.clone()).unwrap();
        vfs.kill_at_dir_sync(&oid_dir(f));
        assert!(store.pack(t2).is_err());
        std::mem::forget(store);
        vfs.restart();
        vfs.mark("killed");

        // The next open removes the blob files that lost their record.
        let store = Store::open_vfs(vfs.clone()).unwrap();
        for (oid, tid) in [(f, t1), (g, g1)] {
            assert!(is_removed(&store, oid, tid));
            assert!(!vfs.names().contains(&blob_name(oid, tid)), "{oid:?}");
        }
        assert!(vfs.names().iter().all(|name| !name.ends_with(".intent")));
        assert_eq!(store.backend.pack_watermark().unwrap(), t2);
        // A second pack finds nothing left to remove.
        assert_eq!(store.pack(t2).unwrap().records, 0);
        drop(store);

        // Every crash image of the killed pack and the open after it
        // opens, keeps the current blob and has a file for each blob
        // record. After the open, no blob file is left without its blob
        // record.
        let images = vfs.crash_images();
        assert!(images.iter().any(|image| image.is_after("killed")));
        for image in images {
            let image_vfs = image.vfs();
            let store = Store::open_vfs(image_vfs.clone())
                .unwrap_or_else(|e| panic!("{image:?} does not open: {e}"));
            store.verify_blob(f, t2).unwrap();
            for (oid, tid) in [(f, t1), (g, g1)] {
                match store.verify_blob(oid, tid) {
                    Ok(()) => assert!(!image.is_after("killed"), "{image:?}"),
                    Err(ZoeError::NotFound { .. }) => {}
                    Err(e) => panic!("{image:?}: {e}"),
                }
            }
            let left = orphans(&store, &image_vfs, &[(f, t1), (f, t2), (g, g1)]);
            assert!(left.is_empty(), "{image:?} keeps {left:?}");
        }
    }

    /// Return the blob files in `vfs` that have no blob record in `store`.
    /// The blob files of `known` are the only ones that a test can make.
    fn orphans(store: &Store, vfs: &MemVfs, known: &[(Oid, Tid)]) -> Vec<String> {
        let names = vfs.names();
        let mut orphans: Vec<String> = names
            .iter()
            .filter(|name| name.ends_with(".blob"))
            .filter(|name| !known.iter().any(|&(o, t)| blob_name(o, t) == **name))
            .cloned()
            .collect();
        for &(oid, tid) in known {
            let name = blob_name(oid, tid);
            if names.contains(&name) && store.backend.load_blob(oid, tid).unwrap().is_none() {
                orphans.push(name);
            }
        }
        orphans
    }

    #[test]
    fn a_killed_pack_then_an_open_then_a_power_loss() {
        let built = MemVfs::new();
        let f = Oid(1);
        let (t1, t2) = {
            let store = Store::open_vfs(built.clone()).unwrap();
            commit(&store, &[(ROOT, record(&[f]))]);
            let t1 = commit_blob(&store, f, &record(&[]), b"one");
            let t2 = commit_blob(&store, f, &record(&[]), b"two");
            (t1, t2)
        };
        let vfs = built.durable_copy();
        let old = blob_name(f, t1);

        // The pack commits the removal of f at t1, removes its file and
        // dies at the sync of its directory. So the removal of the file is
        // not durable.
        let store = Store::open_vfs(vfs.clone()).unwrap();
        vfs.kill_at_dir_sync(&oid_dir(f));
        assert!(store.pack(t2).is_err());
        std::mem::forget(store);
        vfs.restart();
        // The next open finds no file to remove, and removes the intent
        // file.
        drop(Store::open_vfs(vfs.clone()).unwrap());
        vfs.mark("opened");
        assert!(!vfs.names().contains(&old));

        // A power loss after the open must not bring back a file that no
        // intent file names.
        let mut checked = 0;
        for image in vfs.crash_images() {
            if !image.is_after("opened") {
                continue;
            }
            let image_vfs = image.vfs();
            let store = Store::open_vfs(image_vfs.clone()).unwrap();
            assert!(is_removed(&store, f, t1), "{image:?}");
            let left = orphans(&store, &image_vfs, &[(f, t1), (f, t2)]);
            assert!(left.is_empty(), "{image:?} keeps {left:?}");
            checked += 1;
        }
        assert!(checked > 0);
    }

    /// Return the tid of the only blob record of `oid` in the store in
    /// `vfs`.
    fn store_blob_tid(vfs: &MemVfs, oid: Oid) -> Tid {
        let store = Store::open_vfs(vfs.durable_copy()).unwrap();
        let history = store.backend.history(oid, 10).unwrap();
        let [version] = &history[..] else {
            panic!("{history:?}");
        };
        version.tid
    }

    /// Return a record of class `m.C` that holds `tag` and a reference to
    /// each oid of `refs`.
    fn tagged(refs: &[Oid], tag: u32) -> Vec<u8> {
        let mut p = b"\x80\x03cm\nC\nq\x00.\x80\x03}(X\x01\x00\x00\x00tJ".to_vec();
        p.extend_from_slice(&tag.to_le_bytes());
        for (i, oid) in refs.iter().enumerate() {
            p.extend_from_slice(&[b'K', u8::try_from(i).unwrap(), b'C', 8]);
            p.extend_from_slice(&oid.0.to_be_bytes());
            p.extend_from_slice(b"h\x00\x86Q");
        }
        p.extend_from_slice(b"u.");
        p
    }

    /// Run a pack in batches of one record while another thread links
    /// unreachable objects again, from references that it read before T.
    /// With `targeted`, the thread links the object whose newest record
    /// the pack removes next. Return the objects that the thread linked
    /// while their newest record was still there.
    fn pack_during_stale_links(targeted: bool, first: u64) -> usize {
        const N: u64 = 40;
        let store = Arc::new(Store::open_vfs(MemVfs::new()).unwrap());
        // The holders stay reachable. Each a_i holds b_i, which comes after
        // a_i in key order.
        let holders: Vec<Oid> = (1..=4).map(Oid).collect();
        let a = |i: u64| Oid(100 + 2 * i);
        let b = |i: u64| Oid(101 + 2 * i);
        let mut writes = vec![(ROOT, tagged(&holders, 0))];
        writes.extend(holders.iter().map(|&h| (h, tagged(&[], 0))));
        for i in 0..N {
            writes.push((a(i), tagged(&[b(i)], 0)));
            writes.push((b(i), tagged(&[], 0)));
        }
        let t1 = commit(&store, &writes);
        let mut writes = Vec::new();
        for i in 0..N {
            writes.push((a(i), tagged(&[b(i)], 1)));
            writes.push((b(i), tagged(&[], 1)));
        }
        let t2 = commit(&store, &writes);
        // A holder links every a_i, then drops them at T.
        let all: Vec<Oid> = (0..N).map(a).collect();
        commit(&store, &[(holders[0], tagged(&all, 2))]);
        let t = commit(&store, &[(holders[0], tagged(&[], 3))]);

        let present = |store: &Store, oid: Oid, tid: Tid| match store.load_serial(oid, tid) {
            Ok(_) => true,
            Err(ZoeError::NotFound { .. }) => false,
            Err(e) => panic!("{e}"),
        };
        let done = Arc::new(AtomicBool::new(false));
        let writer = {
            let (store, done) = (Arc::clone(&store), Arc::clone(&done));
            thread::spawn(move || {
                let mut linked = Vec::new();
                let mut i = first;
                let mut tag = 10;
                while !done.load(Ordering::Relaxed) && i < N {
                    if targeted {
                        // The next object of the pack: its first record is
                        // gone, and its newest is not.
                        let next = (0..N)
                            .find(|&j| present(&store, a(j), t2) && !present(&store, a(j), t1));
                        let Some(j) = next else {
                            thread::yield_now();
                            continue;
                        };
                        i = j;
                    }
                    tag += 1;
                    let holder = holders[1 + (i as usize % 3)];
                    commit(&store, &[(holder, tagged(&[a(i)], tag))]);
                    if present(&store, a(i), t2) {
                        linked.push(i);
                    }
                    if !targeted {
                        i += 3;
                    }
                }
                linked
            })
        };
        let report = store.pack_in_batches(t, 1, || {}).unwrap();
        done.store(true, Ordering::Relaxed);
        let linked = writer.join().unwrap();
        assert!(report.records > 0);
        for &i in &linked {
            assert!(
                present(&store, a(i), t2),
                "a_{i} was linked again and removed"
            );
            assert!(
                present(&store, b(i), t2),
                "b_{i}, which a_{i} holds, was removed"
            );
        }
        linked.len()
    }

    #[test]
    fn a_link_that_lands_while_the_pack_waits_for_the_sequencer_keeps_its_target() {
        // A finish can end after the first mark of a batch and before the
        // pack holds the commit sequencer. Only the mark under the
        // sequencer sees it. The targeted runs hit that window.
        let mut linked = 0;
        for first in [0, 2, 4] {
            linked += pack_during_stale_links(true, first);
        }
        linked += pack_during_stale_links(false, 1);
        assert!(linked > 0);
    }

    #[test]
    fn a_corrupt_record_stops_the_pack_and_stays() {
        let vfs = MemVfs::new();
        let g = Oid(1);
        let (t1, t2) = {
            let store = Store::open_vfs(vfs.clone()).unwrap();
            let t1 = commit(&store, &[(ROOT, record(&[g])), (g, record(&[]))]);
            let t2 = commit(&store, &[(ROOT, record(&[]))]);
            (t1, t2)
        };
        // Flip the last bit of the unreachable record of g.
        {
            let records: redb::TableDefinition<&[u8; 16], &[u8]> =
                redb::TableDefinition::new("records");
            let db = redb::Builder::new()
                .create_with_backend(vfs.file("zoe.redb"))
                .unwrap();
            let txn = db.begin_write().unwrap();
            {
                let mut table = txn.open_table(records).unwrap();
                let key = crate::format::record_key(g, t1);
                let mut frame = redb::ReadableTable::get(&table, &key)
                    .unwrap()
                    .unwrap()
                    .value()
                    .to_vec();
                *frame.last_mut().unwrap() ^= 1;
                table.insert(&key, frame.as_slice()).unwrap();
            }
            txn.commit().unwrap();
        }
        let store = Store::open_vfs(vfs).unwrap();

        // A pack must not delete the evidence of a corruption. The error
        // names the record.
        let error = store.pack(t2);
        assert!(
            matches!(error, Err(ZoeError::CorruptRecord { oid, tid }) if oid == g && tid == t1),
            "{error:?}"
        );
        assert_eq!(store.load_serial(ROOT, t1).unwrap(), record(&[g]));
        assert_eq!(store.backend.pack_watermark().unwrap(), Tid::ZERO);
    }

    #[test]
    fn a_pack_of_an_empty_store_removes_nothing() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        assert_eq!(store.pack(Tid::MAX).unwrap(), PackReport::default());
        assert_eq!(store.backend.pack_watermark().unwrap(), Tid::ZERO);
    }
}
