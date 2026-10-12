//! The read and copy paths of a store: `load_before`, `load_serial`,
//! `history`, the transaction iterator, the oids that a tid range wrote
//! and the current-record iteration.
//!
//! A record with an empty pickle is a deletion. A load of a deletion
//! fails with `NotFound`. Each backend read verifies the checksum of every
//! frame that it returns.
//!
//! A pack can remove records and transaction rows between two reads. It
//! removes a record and the row of its transaction in one write, and it
//! removes a row only when no record of the transaction is left. So
//! `history` drops a record that it finds without a row only when the
//! record is gone too, and the iterator reads each transaction, its index
//! keys and its records in one snapshot.

use std::collections::{BTreeSet, VecDeque};
use std::sync::{Arc, Weak};

use crate::backend::{Backend, BlobRecord, Record, RecordMeta, TxnMeta};
use crate::error::{Result, ZoeError};
use crate::store::{Opened, Store};
use crate::types::{Oid, Tid};

/// A record that `load_before` found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadedRecord {
    /// The pickle.
    pub data: Vec<u8>,
    /// The serial of the record.
    pub serial: Tid,
    /// The serial of the next record of the same oid. It is `None` when
    /// the record is current.
    pub next: Option<Tid>,
}

/// One record of an object, as `history` gives it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryEntry {
    /// The tid that wrote the record.
    pub tid: Tid,
    /// The user name of the transaction.
    pub user: Vec<u8>,
    /// The description of the transaction.
    pub description: Vec<u8>,
    /// The size of the pickle in bytes. A deletion has size 0.
    pub size: u64,
}

/// One transaction that the transaction iterator gives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TxnInfo {
    /// The tid of the transaction.
    pub tid: Tid,
    /// The user name of the transaction.
    pub user: Vec<u8>,
    /// The description of the transaction.
    pub description: Vec<u8>,
    /// The pickled extension data of the transaction.
    pub extension: Vec<u8>,
    /// The records that the transaction wrote, in ascending oid order.
    pub records: Vec<RecordInfo>,
}

/// One record of a transaction that the transaction iterator gives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordInfo {
    /// The oid of the record.
    pub oid: Oid,
    /// The tid of the transaction that wrote the record.
    pub tid: Tid,
    /// The pickle. It is `None` for a deletion.
    pub data: Option<Vec<u8>>,
}

/// A current record that `record_iternext` found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CurrentRecord {
    /// The oid of the record.
    pub oid: Oid,
    /// The serial of the record.
    pub tid: Tid,
    /// The pickle.
    pub data: Vec<u8>,
    /// The next oid that has a current record. It is `None` at the end.
    pub next: Option<Oid>,
}

/// The number of rows, and of tids of the transaction index, that the
/// transaction iterator reads in one slice.
const SLICE: usize = 1024;

/// An iterator over transactions in tid order. Each item holds the
/// records of its transaction.
///
/// The iterator does not borrow the store, and it can move to another
/// thread. It does not keep the store open: after the `Store` is dropped,
/// the next call gives `Closed`, then the iterator ends.
///
/// It reads the rows and the tids of the transaction index a slice at a
/// time, as it goes. So it holds one transaction and one slice of each,
/// whatever the length of its range. A slice of rows ends only after a row
/// that reads. So a run of rows that do not read stays in one slice, and
/// the iterator holds the error of each row of that run.
pub struct Transactions {
    store: Weak<Opened>,
    /// The last tid of the range.
    stop: Tid,
    /// The most rows, and the most tids of the index, in one slice.
    slice: usize,
    /// The slice of rows: the tid of each row that reads, or the error of
    /// a row that does not.
    rows: VecDeque<Result<Tid>>,
    /// The first tid of the next slice of rows. It is `None` when the
    /// iterator has read every row of the range.
    next_row: Option<Tid>,
    /// The last tid that the slices of rows cover. The iterator reads the
    /// index only up to this tid, so it reads the index of a tid after its
    /// rows.
    covered: Option<Tid>,
    /// The range has a row. Only then does the iterator read the index.
    any_row: bool,
    /// The slice of the tids that the transaction index gives. The
    /// iterator uses them to find index keys of a tid that has no row.
    indexed: VecDeque<Tid>,
    /// The first tid of the next slice of the index. It is `None` when
    /// the iterator reads no more of the index.
    next_indexed: Option<Tid>,
    /// The iterator gave a row that does not read. Index keys before the
    /// next row that reads give no error.
    unread: bool,
}

/// Return the tid after `tid` when it is not after `stop`.
fn after(tid: Tid, stop: Tid) -> Option<Tid> {
    tid.0.checked_add(1).map(Tid).filter(|&next| next <= stop)
}

impl Transactions {
    fn new(store: Weak<Opened>, start: Tid, stop: Tid, slice: usize) -> Self {
        let first = (start <= stop).then_some(start);
        Self {
            store,
            stop,
            slice: slice.max(1),
            rows: VecDeque::new(),
            next_row: first,
            covered: None,
            any_row: false,
            indexed: VecDeque::new(),
            next_indexed: first,
            unread: false,
        }
    }

    /// Stop the iterator. The next call gives `None`.
    fn end(&mut self) {
        self.rows.clear();
        self.indexed.clear();
        self.next_row = None;
        self.next_indexed = None;
    }

    fn ended(&self) -> bool {
        self.rows.is_empty()
            && self.indexed.is_empty()
            && self.next_row.is_none()
            && self.next_indexed.is_none()
    }

    /// Read the next slice of rows, in one snapshot. The slice ends after
    /// a row that reads, so the next slice starts after its tid. The error
    /// of a row that does not read does not always name its tid, and an
    /// error with a tid can come from a failed scan. So the slice does not
    /// end after an error.
    fn read_rows(&mut self, backend: &dyn Backend) {
        let Some(from) = self.next_row else {
            return;
        };
        self.next_row = None;
        self.covered = Some(self.stop);
        for row in backend.iter_txns(from, self.stop) {
            let row = row.map(|row| row.tid);
            let last = row.as_ref().ok().copied();
            self.rows.push_back(row);
            if let Some(tid) = last
                && self.rows.len() >= self.slice
            {
                self.next_row = after(tid, self.stop);
                if self.next_row.is_some() {
                    self.covered = Some(tid);
                }
                break;
            }
        }
        self.any_row |= !self.rows.is_empty();
    }

    /// Read the next slice of the index, in one snapshot, up to the last
    /// tid that the rows cover.
    fn read_indexed(&mut self, backend: &dyn Backend) -> Result<()> {
        let (Some(from), Some(to)) = (self.next_indexed, self.covered) else {
            return Ok(());
        };
        if from > to {
            return Ok(());
        }
        let tids = backend.indexed_tids(from, to, self.slice)?;
        self.next_indexed = match tids.last() {
            Some(&last) if tids.len() >= self.slice => after(last, self.stop),
            _ => after(to, self.stop),
        };
        self.indexed.extend(tids);
        Ok(())
    }
}

impl Iterator for Transactions {
    type Item = Result<TxnInfo>;

    fn next(&mut self) -> Option<Result<TxnInfo>> {
        loop {
            if self.ended() {
                return None;
            }
            // The store stays open until this call returns.
            let Some(store) = self.store.upgrade() else {
                self.end();
                return Some(Err(ZoeError::Closed));
            };
            if self.rows.is_empty() {
                self.read_rows(&**store);
            }
            // A range with no row needs no read of the index.
            if !self.any_row {
                self.end();
                return None;
            }
            if self.indexed.is_empty()
                && let Err(error) = self.read_indexed(&**store)
            {
                // The index of the rest of the range does not read. The
                // rows still give their transactions.
                self.next_indexed = None;
                return Some(Err(error));
            }
            let indexed = self.indexed.front().copied();
            match self.rows.pop_front() {
                // A row that does not read can own any index keys before
                // the next row that reads. They give no second error.
                Some(Err(error)) => {
                    self.unread = true;
                    return Some(Err(error));
                }
                Some(Ok(tid)) => {
                    // Index keys of a tid before the row have no row. They
                    // come in tid order, as an error.
                    if let Some(first) = indexed
                        && first < tid
                    {
                        self.rows.push_front(Ok(tid));
                        self.indexed.pop_front();
                        if self.unread {
                            continue;
                        }
                        return Some(Err(ZoeError::CorruptTxn { tid: first }));
                    }
                    self.unread = false;
                    if indexed == Some(tid) {
                        self.indexed.pop_front();
                    }
                    match txn_info(&**store, tid) {
                        Ok(Some(txn)) => return Some(Ok(txn)),
                        // A pack removed the transaction after the iterator
                        // read its row.
                        Ok(None) => {}
                        Err(error) => return Some(Err(error)),
                    }
                }
                // The rows of the range are all read. Index keys after the
                // last row have no row.
                None => match indexed {
                    Some(first) if !self.unread => {
                        self.indexed.pop_front();
                        return Some(Err(ZoeError::CorruptTxn { tid: first }));
                    }
                    _ => self.end(),
                },
            }
        }
    }
}

/// Read the transaction `tid`, its index keys and their records from
/// `backend`, in one snapshot. The records must match the oid count of the
/// row. Return `None` when a pack removed the transaction after the
/// iterator read its row. A transaction that is gone with no pack to
/// explain it raises `CorruptTxn`.
///
/// A record without its index key leaves the records short of the count.
/// The backend gives `CorruptTxn` for an index key without its record.
fn txn_info(backend: &dyn Backend, tid: Tid) -> Result<Option<TxnInfo>> {
    let Some((row, records)) = backend.txn_records(tid)? else {
        // The scan of the rows gave this row. Only a pack can remove it
        // since. Otherwise the database is damaged.
        if packed(backend, tid)? {
            return Ok(None);
        }
        return Err(ZoeError::CorruptTxn { tid });
    };
    if records.len() as u64 != row.oid_count {
        return Err(ZoeError::CorruptTxn { tid });
    }
    let records = records
        .into_iter()
        .map(|(oid, data)| RecordInfo {
            oid,
            tid,
            data: (!data.is_empty()).then_some(data),
        })
        .collect();
    Ok(Some(TxnInfo {
        tid,
        user: row.user,
        description: row.description,
        extension: row.extension,
        records,
    }))
}

/// Tell if a pack can have removed records or the row of the transaction
/// `tid`. A pack removes only records at or before its tid, and its first
/// write also writes that tid as the pack watermark.
fn packed(backend: &dyn Backend, tid: Tid) -> Result<bool> {
    Ok(tid <= backend.pack_watermark()?)
}

impl Store {
    /// Return the record of `oid` that was current just before `tid`: the
    /// newest record with a tid lower than `tid`. This is `loadBefore` of
    /// ZODB.
    ///
    /// Return `None` when `oid` has records, but none before `tid`. Fail
    /// with `NotFound` when `oid` has no record, or when the record is a
    /// deletion.
    pub fn load_before(&self, oid: Oid, tid: Tid) -> Result<Option<LoadedRecord>> {
        match self.backend.load_before_next(oid, tid)? {
            (Some(record), _) if record.data.is_empty() => Err(ZoeError::NotFound { oid }),
            (Some(record), next) => Ok(Some(LoadedRecord {
                data: record.data,
                serial: record.tid,
                next,
            })),
            (None, Some(_)) => Ok(None),
            (None, None) => Err(ZoeError::NotFound { oid }),
        }
    }

    /// Return the pickle that the transaction `tid` wrote for `oid`. Fail
    /// with `NotFound` when that record does not exist or is a deletion.
    pub fn load_serial(&self, oid: Oid, tid: Tid) -> Result<Vec<u8>> {
        match self.backend.load_serial(oid, tid)? {
            Some(data) if !data.is_empty() => Ok(data),
            _ => Err(ZoeError::NotFound { oid }),
        }
    }

    /// Return at most `size` records of `oid`, newest first, with the user
    /// and the description of their transactions. Fail with `NotFound`
    /// when `oid` has no record.
    ///
    /// A record without a transaction row is `CorruptTxn`. But when a pack
    /// removed the record and the row after this call read the record, the
    /// record is left out.
    pub fn history(&self, oid: Oid, size: usize) -> Result<Vec<HistoryEntry>> {
        let versions = self.backend.history(oid, size.max(1))?;
        if versions.is_empty() {
            return Err(ZoeError::NotFound { oid });
        }
        self.history_entries(oid, versions, size)
    }

    /// Return the history entries of the first `size` records of
    /// `versions`, the records of `oid` that an earlier snapshot read.
    ///
    /// A record that is gone with its row since that snapshot is left out
    /// only when a pack can have removed it. Otherwise the call raises
    /// `CorruptTxn`.
    pub(crate) fn history_entries(
        &self,
        oid: Oid,
        versions: Vec<RecordMeta>,
        size: usize,
    ) -> Result<Vec<HistoryEntry>> {
        let mut entries = Vec::new();
        for version in versions.into_iter().take(size) {
            let tid = version.tid;
            let row = match self.backend.iter_txns(tid, tid).next() {
                Some(row) => row?,
                // A pack removed the record and its row since the snapshot.
                None if self.backend.load_serial(oid, tid)?.is_none()
                    && packed(&**self.backend, tid)? =>
                {
                    continue;
                }
                None => return Err(ZoeError::CorruptTxn { tid }),
            };
            entries.push(HistoryEntry {
                tid,
                user: row.user,
                description: row.description,
                size: version.size,
            });
        }
        if entries.is_empty() && size > 0 {
            return Err(ZoeError::NotFound { oid });
        }
        Ok(entries)
    }

    /// Iterate over the transactions from `start` to `stop`, both included,
    /// in tid order. `None` leaves a bound open. Each transaction comes
    /// with all of its records.
    ///
    /// The iterator stops at the last tid at the time of this call. The
    /// iterator leaves out a transaction that commits after this call.
    ///
    /// The oids of each transaction come from the transaction index. The
    /// iterator reads the rows of the range a slice at a time, then the
    /// tids of the index that these rows cover, a slice at a time. Then it
    /// reads each transaction with its index keys and its records in one
    /// snapshot. Each slice is one snapshot. So the iterator holds one
    /// transaction and a slice of tids, whatever the length of the range.
    /// A slice ends only after a row that reads, so a run of rows that do
    /// not read stays in one slice, with the error of each row.
    /// It never reads every record key, so a read of one transaction costs
    /// the size of its result. A transaction that a pack removes after
    /// this call is left out.
    ///
    /// A transaction whose index keys do not match the oid count of its row
    /// gives `CorruptTxn`: a record without its index key does this. An
    /// index key without its record gives `CorruptTxn` too. Index keys of a
    /// tid that has no row give `CorruptTxn`, but only when the range has a
    /// row. A row that does not read gives its own error once, and the
    /// index keys before the next row that reads give no error. A failed
    /// scan of the rows gives its error and ends the iterator. A failed
    /// scan of the index gives its error once, and the iterator then gives
    /// the transactions of the rest of the range without that check.
    pub fn iterator(&self, start: Option<Tid>, stop: Option<Tid>) -> Result<Transactions> {
        Ok(self.iterator_in_slices(start, stop, SLICE))
    }

    /// Iterate as `iterator` does, with slices of at most `slice` rows and
    /// `slice` tids of the index.
    pub(crate) fn iterator_in_slices(
        &self,
        start: Option<Tid>,
        stop: Option<Tid>,
        slice: usize,
    ) -> Transactions {
        let start = start.unwrap_or(Tid::ZERO);
        let stop = stop.unwrap_or(Tid::MAX).min(self.last_transaction());
        Transactions::new(Arc::downgrade(&self.backend), start, stop, slice)
    }

    /// Return each oid that a transaction from `from` to `to`, both
    /// included, wrote: each oid once, in ascending order. Return `None`
    /// when more than `limit` transactions of the range wrote a record.
    ///
    /// The oids come from the transaction index. The call reads no record
    /// and no transaction row, so it costs the size of the index keys of
    /// the range. A transaction that wrote no record has no index key, so
    /// it does not count. The call reads the index a slice of transactions
    /// at a time, and each slice is one snapshot. So it holds the oids of
    /// its result and one slice.
    ///
    /// A pack between two slices can remove a record of the range: then
    /// its oid is left out when no other record of the range names it.
    pub fn written_oids(&self, from: Tid, to: Tid, limit: usize) -> Result<Option<Vec<Oid>>> {
        self.written_oids_in_slices(from, to, limit, SLICE)
    }

    /// Return the oids as `written_oids` does, with slices of at most
    /// `slice` transactions.
    pub(crate) fn written_oids_in_slices(
        &self,
        from: Tid,
        to: Tid,
        limit: usize,
        slice: usize,
    ) -> Result<Option<Vec<Oid>>> {
        let slice = slice.max(1);
        let mut oids = BTreeSet::new();
        let mut counted = 0usize;
        let mut next = (from <= to).then_some(from);
        while let Some(start) = next {
            let tids = self.backend.indexed_tids(start, to, slice)?;
            let (Some(&first), Some(&last)) = (tids.first(), tids.last()) else {
                break;
            };
            counted = counted.saturating_add(tids.len());
            if counted > limit {
                return Ok(None);
            }
            for written in self.backend.txn_oids(first, last)?.into_values() {
                oids.extend(written);
            }
            next = if tids.len() < slice {
                None
            } else {
                after(last, to)
            };
        }
        Ok(Some(oids.into_iter().collect()))
    }

    /// Return the current record of the first oid at or after `next` that
    /// has one, and the oid after it. `None` starts at the first oid. This
    /// is `record_iternext` of ZODB.
    ///
    /// A deleted oid has no current record, so the walk skips it. Return
    /// `None` when no oid at or after `next` has a current record.
    pub fn record_iternext(&self, next: Option<Oid>) -> Result<Option<CurrentRecord>> {
        let Some((oid, record)) = self.current_at_or_after(next.unwrap_or(Oid(0)))? else {
            return Ok(None);
        };
        let next = match oid.0.checked_add(1) {
            Some(after) => self.current_at_or_after(Oid(after))?.map(|(oid, _)| oid),
            None => None,
        };
        Ok(Some(CurrentRecord {
            oid,
            tid: record.tid,
            data: record.data,
            next,
        }))
    }

    /// Return the keys of the records of the oids from `from` up, in key
    /// order, deletions too. The list stops after the first oid that
    /// brings it to `limit` keys or more, so it holds all the keys of each
    /// oid in it. The call reads no frame. A walk of the frames gives each
    /// key to `load_serial` and `load_blob`.
    pub fn record_keys(&self, from: Oid, limit: usize) -> Result<Vec<(Oid, Tid)>> {
        self.backend.record_keys(from, limit)
    }

    /// Return every transaction row, in tid order. Each row comes with its
    /// own result, so a row that does not read does not hide the others.
    /// The call reads no record.
    pub fn txn_rows(&self) -> Vec<Result<TxnMeta>> {
        self.backend.iter_txns(Tid::ZERO, Tid::MAX).collect()
    }

    /// Return the blob record that `tid` wrote for `oid`, or `None`. A
    /// blob record that does not pass its checksum is `CorruptBlob`. The
    /// call does not read the blob file: `verify_blob` reads it.
    pub fn load_blob(&self, oid: Oid, tid: Tid) -> Result<Option<BlobRecord>> {
        self.backend.load_blob(oid, tid)
    }

    /// Return the names of the sequences that have a row, in key order.
    /// The call reads no row: `sequence_end` reads the row of a name. A
    /// name that does not decode fails the whole call with an error.
    pub fn sequence_names(&self) -> Result<Vec<String>> {
        self.backend.sequence_names()
    }

    /// Return the end of the row of the sequence `name`: the first value
    /// that the store has not reserved. Return `None` when the name has no
    /// row. A row that does not pass its checksum is `CorruptSequence`.
    /// The call reserves no value, so a read-only store takes it.
    pub fn sequence_end(&self, name: &str) -> Result<Option<u64>> {
        self.backend.sequence_end(name)
    }

    /// Return the first oid at or after `from` whose current record is not
    /// a deletion, with that record.
    fn current_at_or_after(&self, mut from: Oid) -> Result<Option<(Oid, Record)>> {
        loop {
            match self.backend.current_from(from)? {
                Some((oid, record)) if record.data.is_empty() => match oid.0.checked_add(1) {
                    Some(after) => from = Oid(after),
                    None => return Ok(None),
                },
                found => return Ok(found),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use redb::{ReadableTable, TableDefinition};

    use super::*;
    use crate::backend::TxnMeta;
    use crate::format;
    use crate::vfs::MemVfs;

    /// The pickle of version `n` of an object.
    fn pickle(n: u8) -> Vec<u8> {
        vec![b'p', n]
    }

    /// Commit a restore transaction at `tid` that writes `writes`.
    fn restore(store: &Store, tid: u64, writes: &[(Oid, Option<Vec<u8>>)]) {
        let mut txn = store
            .tpc_begin_restore(Tid(tid), b"user", format!("at {tid}").as_bytes(), b"ext")
            .unwrap();
        for (oid, data) in writes {
            txn.restore(*oid, data.as_deref()).unwrap();
        }
        txn.vote().unwrap();
        assert_eq!(txn.finish().unwrap(), Tid(tid));
    }

    /// Return a store in `vfs` where oid 1 has records at tids 100, 200
    /// and 300.
    fn three_records_in(vfs: &MemVfs) -> Store {
        let store = Store::open_vfs(vfs.clone()).unwrap();
        for (tid, n) in [(100, 1), (200, 2), (300, 3)] {
            restore(&store, tid, &[(Oid(1), Some(pickle(n)))]);
        }
        store
    }

    fn three_records() -> Store {
        three_records_in(&MemVfs::new())
    }

    fn loaded(data: Vec<u8>, serial: u64, next: Option<u64>) -> Option<LoadedRecord> {
        Some(LoadedRecord {
            data,
            serial: Tid(serial),
            next: next.map(Tid),
        })
    }

    #[test]
    fn an_oid_with_one_record_loads_it_as_current() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        restore(&store, 100, &[(Oid(1), Some(pickle(1)))]);
        let current = loaded(pickle(1), 100, None);
        assert_eq!(store.load_before(Oid(1), Tid::MAX).unwrap(), current);
        assert_eq!(store.load_before(Oid(1), Tid(101)).unwrap(), current);
        assert_eq!(store.load_before(Oid(1), Tid(100)).unwrap(), None);
        assert_eq!(store.load_before(Oid(1), Tid::ZERO).unwrap(), None);
    }

    #[test]
    fn a_tid_between_records_loads_the_older_one() {
        let store = three_records();
        let oid = Oid(1);
        assert_eq!(store.load_before(oid, Tid(99)).unwrap(), None);
        for (tid, serial, next) in [(150, 100, 200), (200, 100, 200), (201, 200, 300)] {
            assert_eq!(
                store.load_before(oid, Tid(tid)).unwrap(),
                loaded(pickle((serial / 100) as u8), serial, Some(next)),
                "tid {tid}"
            );
        }
        assert_eq!(
            store.load_before(oid, Tid(301)).unwrap(),
            loaded(pickle(3), 300, None)
        );
    }

    #[test]
    fn an_unknown_oid_is_not_found() {
        let store = three_records();
        let oid = Oid(2);
        assert!(matches!(
            store.load_before(oid, Tid::MAX),
            Err(ZoeError::NotFound { oid: Oid(2) })
        ));
        assert!(matches!(
            store.load_serial(oid, Tid(100)),
            Err(ZoeError::NotFound { .. })
        ));
        assert!(matches!(
            store.history(oid, 10),
            Err(ZoeError::NotFound { .. })
        ));
    }

    #[test]
    fn load_serial_reads_only_the_exact_record() {
        let store = three_records();
        assert_eq!(store.load_serial(Oid(1), Tid(200)).unwrap(), pickle(2));
        for tid in [99, 150, 301] {
            assert!(matches!(
                store.load_serial(Oid(1), Tid(tid)),
                Err(ZoeError::NotFound { .. })
            ));
        }
    }

    /// A store where oid 1 is created at tid 100 and deleted at tid 200,
    /// and oid 2 is created at tid 100.
    fn deleted() -> Store {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        restore(
            &store,
            100,
            &[(Oid(1), Some(pickle(1))), (Oid(2), Some(pickle(1)))],
        );
        restore(&store, 200, &[(Oid(1), None)]);
        store
    }

    #[test]
    fn a_deletion_loads_as_not_found() {
        let store = deleted();
        let oid = Oid(1);
        assert!(matches!(store.load(oid), Err(ZoeError::NotFound { .. })));
        for tid in [Tid(201), Tid::MAX] {
            assert!(matches!(
                store.load_before(oid, tid),
                Err(ZoeError::NotFound { .. })
            ));
        }
        assert!(matches!(
            store.load_serial(oid, Tid(200)),
            Err(ZoeError::NotFound { .. })
        ));
        // The record before the deletion stays readable.
        assert_eq!(
            store.load_before(oid, Tid(200)).unwrap(),
            loaded(pickle(1), 100, Some(200))
        );
        assert_eq!(store.load_serial(oid, Tid(100)).unwrap(), pickle(1));
    }

    #[test]
    fn history_lists_a_deletion_with_size_zero() {
        let store = deleted();
        let history = store.history(Oid(1), 10).unwrap();
        let entries: Vec<_> = history.iter().map(|e| (e.tid, e.size)).collect();
        assert_eq!(entries, [(Tid(200), 0), (Tid(100), 2)]);
        assert_eq!(history[0].user, b"user");
        assert_eq!(history[0].description, b"at 200");
    }

    #[test]
    fn history_stops_at_the_size() {
        let store = three_records();
        assert!(store.history(Oid(1), 0).unwrap().is_empty());
        let newest = store.history(Oid(1), 1).unwrap();
        assert_eq!(newest.len(), 1);
        assert_eq!(newest[0].tid, Tid(300));
        assert_eq!(newest[0].description, b"at 300");
    }

    fn tids(store: &Store, start: Option<u64>, stop: Option<u64>) -> Vec<Tid> {
        store
            .iterator(start.map(Tid), stop.map(Tid))
            .unwrap()
            .map(|txn| txn.unwrap().tid)
            .collect()
    }

    #[test]
    fn the_iterator_gives_a_deletion_without_data() {
        let store = deleted();
        let txns: Vec<_> = store
            .iterator(None, None)
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(txns.len(), 2);
        assert_eq!(
            txns[1].records,
            [RecordInfo {
                oid: Oid(1),
                tid: Tid(200),
                data: None
            }]
        );
    }

    #[test]
    fn the_iterator_gives_records_in_oid_order() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let writes: Vec<_> = [3, 1, 2]
            .map(|oid| (Oid(oid), Some(pickle(oid as u8))))
            .to_vec();
        restore(&store, 100, &writes);
        let txn = store.iterator(None, None).unwrap().next().unwrap().unwrap();
        let oids: Vec<Oid> = txn.records.iter().map(|r| r.oid).collect();
        assert_eq!(oids, [Oid(1), Oid(2), Oid(3)]);
        assert_eq!(txn.extension, b"ext");
    }

    #[test]
    fn empty_ranges_give_no_transaction() {
        let empty = Store::open_vfs(MemVfs::new()).unwrap();
        assert!(tids(&empty, None, None).is_empty());

        let store = three_records();
        assert_eq!(tids(&store, None, None), [Tid(100), Tid(200), Tid(300)]);
        assert!(tids(&store, Some(201), Some(299)).is_empty());
        assert!(tids(&store, Some(301), None).is_empty());
        assert!(tids(&store, None, Some(99)).is_empty());
        assert!(tids(&store, Some(300), Some(100)).is_empty());
        assert_eq!(tids(&store, Some(150), Some(300)), [Tid(200), Tid(300)]);
    }

    #[test]
    fn the_iterator_moves_to_another_thread() {
        let store = three_records();
        let iterator = store.iterator(None, None).unwrap();
        let tids =
            std::thread::spawn(move || iterator.map(|txn| txn.unwrap().tid).collect::<Vec<Tid>>())
                .join()
                .unwrap();
        assert_eq!(tids, [Tid(100), Tid(200), Tid(300)]);
    }

    #[test]
    fn a_lingering_iterator_fails_after_the_store_closes() {
        let dir = std::env::temp_dir().join(format!("zoe-iterator-closed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(&dir).unwrap();
        for tid in [100, 200] {
            restore(&store, tid, &[(Oid(1), Some(pickle(1)))]);
        }
        let mut iterator = store.iterator(None, None).unwrap();
        assert_eq!(iterator.next().unwrap().unwrap().tid, Tid(100));

        // The iterator does not keep the store locked.
        drop(store);
        let reopened = Store::open(&dir).unwrap();
        assert!(matches!(iterator.next(), Some(Err(ZoeError::Closed))));
        assert!(iterator.next().is_none());
        assert_eq!(reopened.last_transaction(), Tid(200));
    }

    #[test]
    fn the_iterator_leaves_out_a_later_transaction() {
        let store = three_records();
        let iterator = store.iterator(None, None).unwrap();
        restore(&store, 400, &[(Oid(1), Some(pickle(4)))]);
        let tids: Vec<Tid> = iterator.map(|txn| txn.unwrap().tid).collect();
        assert_eq!(tids, [Tid(100), Tid(200), Tid(300)]);
    }

    fn walk(store: &Store) -> Vec<(Oid, Tid, Option<Oid>)> {
        let mut seen = Vec::new();
        let mut next = None;
        while let Some(record) = store.record_iternext(next).unwrap() {
            seen.push((record.oid, record.tid, record.next));
            match record.next {
                Some(oid) => next = Some(oid),
                None => break,
            }
        }
        seen
    }

    #[test]
    fn record_iternext_skips_a_deleted_oid() {
        let store = deleted();
        assert_eq!(walk(&store), [(Oid(2), Tid(100), None)]);

        restore(&store, 300, &[(Oid(3), Some(pickle(1)))]);
        assert_eq!(
            walk(&store),
            [(Oid(2), Tid(100), Some(Oid(3))), (Oid(3), Tid(300), None)]
        );
    }

    #[test]
    fn record_iternext_at_the_end_gives_none() {
        let empty = Store::open_vfs(MemVfs::new()).unwrap();
        assert_eq!(empty.record_iternext(None).unwrap(), None);

        let store = three_records();
        let last = store.record_iternext(Some(Oid(1))).unwrap().unwrap();
        assert_eq!(
            (last.tid, last.data, last.next),
            (Tid(300), pickle(3), None)
        );
        assert_eq!(store.record_iternext(Some(Oid(2))).unwrap(), None);
        assert_eq!(store.record_iternext(Some(Oid(u64::MAX))).unwrap(), None);
    }

    /// The `records` table.
    type Records<'a> = redb::Table<'a, &'static [u8; 16], &'static [u8]>;

    /// The table of the transaction index.
    type Index<'a> = redb::Table<'a, &'static [u8; 16], ()>;

    /// Give the `records` table and the transaction index of the closed
    /// store in `vfs` to `change`, and commit the change.
    fn change_tables(vfs: &MemVfs, change: impl FnOnce(&mut Records, &mut Index)) {
        let records: TableDefinition<&[u8; 16], &[u8]> = TableDefinition::new("records");
        let index: TableDefinition<&[u8; 16], ()> = TableDefinition::new("txn_oids");
        let db = redb::Builder::new()
            .create_with_backend(vfs.file("zoe.redb"))
            .unwrap();
        let txn = db.begin_write().unwrap();
        change(
            &mut txn.open_table(records).unwrap(),
            &mut txn.open_table(index).unwrap(),
        );
        txn.commit().unwrap();
    }

    /// Flip the last bit of the record of `oid` at `tid` in the closed
    /// store in `vfs`.
    fn corrupt(vfs: &MemVfs, oid: Oid, tid: Tid) {
        change_tables(vfs, |records, _| {
            let key = format::record_key(oid, tid);
            let mut frame = records.get(&key).unwrap().unwrap().value().to_vec();
            *frame.last_mut().unwrap() ^= 1;
            records.insert(&key, frame.as_slice()).unwrap();
        });
    }

    /// Write a valid record of `oid` at `tid` and its index key, in the
    /// closed store in `vfs`. No transaction row counts the record.
    fn add_record(vfs: &MemVfs, oid: Oid, tid: Tid) {
        change_tables(vfs, |records, index| {
            let key = format::record_key(oid, tid);
            let frame = format::encode_frame(
                format::KeySpace::Records,
                &key,
                format::MAIN_BRANCH,
                Tid::ZERO,
                b"x",
            );
            records.insert(&key, frame.as_slice()).unwrap();
            index.insert(&format::txn_oid_key(tid, oid), ()).unwrap();
        });
    }

    /// Set the flags byte of the transaction row of `tid` in the closed
    /// store in `vfs`, with a valid checksum. A newer format writes such a
    /// row.
    fn flag_row(vfs: &MemVfs, tid: Tid) {
        let txns: TableDefinition<&[u8; 8], &[u8]> = TableDefinition::new("txns");
        let db = redb::Builder::new()
            .create_with_backend(vfs.file("zoe.redb"))
            .unwrap();
        let txn = db.begin_write().unwrap();
        {
            let mut table = txn.open_table(txns).unwrap();
            let key = format::txn_key(tid);
            let mut frame = table.get(&key).unwrap().unwrap().value().to_vec();
            frame[1] = 1;
            format::seal(format::KeySpace::Txns, &key, &mut frame);
            table.insert(&key, frame.as_slice()).unwrap();
        }
        txn.commit().unwrap();
    }

    #[test]
    fn a_row_of_a_newer_format_gives_one_error() {
        let vfs = MemVfs::new();
        drop(three_records_in(&vfs));
        flag_row(&vfs, Tid(200));
        let store = Store::open_vfs(vfs).unwrap();

        let txns: Vec<_> = store.iterator(None, None).unwrap().collect();
        assert_eq!(txns.len(), 3, "{txns:?}");
        assert_eq!(txns[0].as_ref().unwrap().tid, Tid(100));
        assert!(matches!(txns[1], Err(ZoeError::Format(_))), "{txns:?}");
        assert_eq!(txns[2].as_ref().unwrap().tid, Tid(300));
    }

    /// Return the tid of each transaction of the iterator, or the tid of
    /// its `CorruptTxn` error.
    fn checked(store: &Store, stop: Option<u64>) -> Vec<std::result::Result<Tid, Tid>> {
        store
            .iterator(None, stop.map(Tid))
            .unwrap()
            .map(|txn| match txn {
                Ok(txn) => Ok(txn.tid),
                Err(ZoeError::CorruptTxn { tid }) => Err(tid),
                Err(error) => panic!("{error:?}"),
            })
            .collect()
    }

    #[test]
    fn the_iterator_checks_the_index_against_the_rows() {
        let vfs = MemVfs::new();
        drop(three_records_in(&vfs));
        // A second record for the transaction at 200, and records at 150
        // and 250, where no transaction row is. Each has its index key.
        add_record(&vfs, Oid(2), Tid(200));
        add_record(&vfs, Oid(2), Tid(150));
        add_record(&vfs, Oid(3), Tid(250));
        let store = Store::open_vfs(vfs).unwrap();

        let (ok, err) = (Ok::<Tid, Tid>, Err::<Tid, Tid>);
        assert_eq!(
            checked(&store, None),
            [
                ok(Tid(100)),
                err(Tid(150)),
                err(Tid(200)),
                err(Tid(250)),
                ok(Tid(300))
            ]
        );
        // Records after the last row of the range come last.
        assert_eq!(
            checked(&store, Some(260)),
            [ok(Tid(100)), err(Tid(150)), err(Tid(200)), err(Tid(250))]
        );
        // A range with no row reads no index key, so it finds no error.
        let range = store.iterator(Some(Tid(140)), Some(Tid(160))).unwrap();
        assert_eq!(range.count(), 0);
    }

    #[test]
    fn a_record_without_its_index_key_is_corrupt() {
        let vfs = MemVfs::new();
        drop(three_records_in(&vfs));
        change_tables(&vfs, |_, index| {
            let key = format::txn_oid_key(Tid(200), Oid(1));
            assert!(index.remove(&key).unwrap().is_some());
        });
        let store = Store::open_vfs(vfs).unwrap();

        // The index gives no oid for the row at 200, which counts one.
        let (ok, err) = (Ok::<Tid, Tid>, Err::<Tid, Tid>);
        assert_eq!(
            checked(&store, None),
            [ok(Tid(100)), err(Tid(200)), ok(Tid(300))]
        );
        assert_eq!(store.load_serial(Oid(1), Tid(200)).unwrap(), pickle(2));
    }

    #[test]
    fn an_index_key_without_its_record_is_corrupt() {
        let vfs = MemVfs::new();
        drop(three_records_in(&vfs));
        change_tables(&vfs, |records, _| {
            let key = format::record_key(Oid(1), Tid(200));
            assert!(records.remove(&key).unwrap().is_some());
        });
        let store = Store::open_vfs(vfs).unwrap();

        // The count of the row matches the index, but a record is missing.
        let (ok, err) = (Ok::<Tid, Tid>, Err::<Tid, Tid>);
        assert_eq!(
            checked(&store, None),
            [ok(Tid(100)), err(Tid(200)), ok(Tid(300))]
        );
        let one: Vec<_> = store
            .iterator(Some(Tid(200)), Some(Tid(200)))
            .unwrap()
            .collect();
        assert!(
            matches!(one[..], [Err(ZoeError::CorruptTxn { tid: Tid(200) })]),
            "{one:?}"
        );
    }

    #[test]
    fn an_index_key_that_a_removal_left_behind_is_corrupt() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let both = [(Oid(1), Some(pickle(1))), (Oid(2), Some(pickle(1)))];
        restore(&store, 100, &both);
        restore(&store, 200, &[(Oid(1), Some(pickle(2)))]);
        // A removal of the record of oid 2 at 100 that lowers the oid count
        // of its row, as a pack does, but keeps the index key.
        store.remove_record_for_test(Oid(2), Tid(100)).unwrap();
        let row = store
            .backend
            .iter_txns(Tid(100), Tid(100))
            .next()
            .unwrap()
            .unwrap();
        let mut write = store.backend.begin_write().unwrap();
        write
            .replace_txn(&TxnMeta {
                oid_count: 1,
                ..row
            })
            .unwrap();
        write.commit().unwrap();

        let (ok, err) = (Ok::<Tid, Tid>, Err::<Tid, Tid>);
        assert_eq!(checked(&store, None), [err(Tid(100)), ok(Tid(200))]);
    }

    #[test]
    fn a_blob_commit_and_a_blob_restore_index_their_records() {
        use std::io::Write;

        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let oid = store.new_oid().unwrap();
        let blob = |bytes: &[u8]| {
            let mut file = store.new_blob_file().unwrap();
            file.write_all(bytes).unwrap();
            file
        };
        let mut txn = store.tpc_begin(b"user", b"blob", b"").unwrap();
        txn.store_blob(oid, Tid::ZERO, &pickle(1), blob(b"first"))
            .unwrap();
        txn.vote().unwrap();
        let committed = txn.finish().unwrap();

        // A restore copies a new version of the object with its blob.
        let restored = Tid(committed.0 + 1);
        let mut txn = store
            .tpc_begin_restore(restored, b"user", b"copy", b"")
            .unwrap();
        txn.store_blob(oid, committed, &pickle(2), blob(b"second"))
            .unwrap();
        txn.restore(oid, Some(&pickle(2))).unwrap();
        txn.vote().unwrap();
        txn.finish().unwrap();

        // The blob record shares the index key of its record.
        let expected = BTreeMap::from([(committed, vec![oid]), (restored, vec![oid])]);
        assert_eq!(
            store.backend.txn_oids(committed, restored).unwrap(),
            expected
        );
        let txns: Vec<_> = store
            .iterator(None, None)
            .unwrap()
            .map(|txn| {
                let txn = txn.unwrap();
                (txn.tid, txn.records.iter().map(|r| r.oid).collect())
            })
            .collect();
        assert_eq!(BTreeMap::from_iter(txns), expected);
    }

    /// The tid of a transaction that the iterator gives, or the text of
    /// its error.
    fn tid_or_error(txn: Result<TxnInfo>) -> std::result::Result<Tid, String> {
        txn.map(|txn| txn.tid).map_err(|e| format!("{e:?}"))
    }

    /// The tid of each transaction that the iterator gives with slices of
    /// `slice`, or the text of each error.
    fn sliced(
        store: &Store,
        start: Option<u64>,
        stop: Option<u64>,
        slice: usize,
    ) -> Vec<std::result::Result<Tid, String>> {
        store
            .iterator_in_slices(start.map(Tid), stop.map(Tid), slice)
            .map(tid_or_error)
            .collect()
    }

    /// Check that slices of 1 to 4 rows give what one slice gives.
    fn same_in_every_slice(store: &Store, start: Option<u64>, stop: Option<u64>) {
        let whole = sliced(store, start, stop, SLICE);
        for slice in 1..=4 {
            assert_eq!(
                sliced(store, start, stop, slice),
                whole,
                "slices of {slice}, from {start:?} to {stop:?}"
            );
        }
    }

    #[test]
    fn small_slices_give_every_transaction_in_order() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let tids: Vec<u64> = (1..=10).map(|n| n * 100).collect();
        for &tid in &tids {
            restore(&store, tid, &[(Oid(tid), Some(pickle(1)))]);
        }
        for slice in 1..=4 {
            assert_eq!(
                sliced(&store, None, None, slice),
                tids.iter().map(|&t| Ok(Tid(t))).collect::<Vec<_>>()
            );
            assert_eq!(
                sliced(&store, Some(250), Some(700), slice),
                [300, 400, 500, 600, 700].map(|t| Ok(Tid(t)))
            );
        }
    }

    #[test]
    fn small_slices_give_the_errors_of_one_slice() {
        let vfs = MemVfs::new();
        {
            let store = Store::open_vfs(vfs.clone()).unwrap();
            for tid in [100, 200, 300, 400, 500, 600] {
                restore(&store, tid, &[(Oid(1), Some(pickle(1)))]);
            }
        }
        // Index keys without a row at 150, 250, 450 and 550, a second
        // record at 200, and a row of a newer format at 400.
        add_record(&vfs, Oid(2), Tid(150));
        add_record(&vfs, Oid(2), Tid(200));
        add_record(&vfs, Oid(2), Tid(250));
        add_record(&vfs, Oid(2), Tid(450));
        add_record(&vfs, Oid(2), Tid(550));
        flag_row(&vfs, Tid(400));
        let store = Store::open_vfs(vfs).unwrap();

        let corrupt = |tid| Err(format!("{:?}", ZoeError::CorruptTxn { tid: Tid(tid) }));
        let whole = sliced(&store, None, None, SLICE);
        assert_eq!(
            whole[..5],
            [
                Ok(Tid(100)),
                corrupt(150),
                corrupt(200),
                corrupt(250),
                Ok(Tid(300)),
            ]
        );
        // The row at 400 gives its own error, and the index key at 450
        // gives none.
        assert!(
            whole[5].as_ref().is_err_and(|e| e.starts_with("Format")),
            "{whole:?}"
        );
        assert_eq!(whole[6..], [Ok(Tid(500)), corrupt(550), Ok(Tid(600))]);

        for (start, stop) in [
            (None, None),
            (None, Some(560)),
            (Some(150), Some(450)),
            (Some(420), None),
            (Some(140), Some(160)),
        ] {
            same_in_every_slice(&store, start, stop);
        }
    }

    /// A pickle of class `m.C` with an empty state. It refers to no
    /// object.
    const LEAF: &[u8] = b"\x80\x03cm\nC\nq\x00.\x80\x03}.";

    #[test]
    fn a_pack_between_two_slices_gives_no_error() {
        // The root at 100. Then, in turn, a transaction with no record and
        // a transaction of an object that nothing refers to. The pack
        // removes each such object with its row, one in each batch. A row
        // with no record has no index key, so an index read past it can
        // name the tid of a row that the next batch removes.
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        restore(&store, 100, &[(Oid(0), Some(LEAF.to_vec()))]);
        let mut written = vec![Tid(100)];
        for n in 1..=8u8 {
            let tid = 200 * u64::from(n);
            restore(&store, tid, &[]);
            restore(&store, tid + 100, &[(Oid(n.into()), Some(pickle(n)))]);
            written.extend([Tid(tid), Tid(tid + 100)]);
        }
        let last = store.last_transaction();

        // Each call reads a new slice of one row. Each batch of the pack
        // runs between two calls.
        let mut iterator = store.iterator_in_slices(None, None, 1);
        let mut given = Vec::new();
        let report = store
            .pack_in_batches(last, 1, || {
                if let Some(txn) = iterator.next() {
                    given.push(tid_or_error(txn));
                }
            })
            .unwrap();
        let during = given.len();
        given.extend(iterator.map(tid_or_error));

        assert_eq!(report.records, 8, "{report:?}");
        assert!(during >= 2, "{during} calls during the pack");
        assert!(given.len() > during, "no call after the pack: {given:?}");
        let given: Vec<Tid> = given
            .iter()
            .map(|txn| txn.clone().unwrap_or_else(|e| panic!("{e} in {given:?}")))
            .collect();
        assert!(given.windows(2).all(|pair| pair[0] < pair[1]), "{given:?}");
        assert!(given.iter().all(|tid| written.contains(tid)), "{given:?}");
        // The root and the transactions with no record survive the pack.
        let survivors: Vec<Tid> = (0..=8).map(|n| Tid(100u64.max(200 * n))).collect();
        assert_eq!(tids(&store, None, None), survivors);
        for tid in &survivors {
            assert!(given.contains(tid), "{tid:?} is missing from {given:?}");
        }
    }

    fn is_corrupt<T: std::fmt::Debug>(result: Result<T>, tid: u64) {
        assert!(
            matches!(result, Err(ZoeError::CorruptRecord { oid: Oid(1), tid: t }) if t == Tid(tid)),
            "{result:?}"
        );
    }

    #[test]
    fn every_read_path_verifies_the_checksum() {
        let vfs = MemVfs::new();
        drop(three_records_in(&vfs));
        corrupt(&vfs, Oid(1), Tid(300));
        corrupt(&vfs, Oid(1), Tid(100));
        let store = Store::open_vfs(vfs).unwrap();

        is_corrupt(store.load_before(Oid(1), Tid::MAX), 300);
        // The next record is read, so its checksum is verified too.
        is_corrupt(store.load_before(Oid(1), Tid(300)), 300);
        is_corrupt(store.load_before(Oid(1), Tid(101)), 100);
        is_corrupt(store.load_serial(Oid(1), Tid(100)), 100);
        is_corrupt(store.history(Oid(1), 10), 300);
        is_corrupt(store.record_iternext(None), 300);
        let txns: Vec<_> = store.iterator(None, None).unwrap().collect();
        is_corrupt(txns.into_iter().next().unwrap(), 100);
    }

    /// Remove the row of `tid` and, when `record` is set, the record of
    /// oid 1 at `tid`, with no pack. A damaged index of the database looks
    /// the same to a read.
    fn remove_outside_a_pack(store: &Store, tid: Tid, record: bool) {
        let mut write = store.backend.begin_write().unwrap();
        if record {
            write.remove_record(Oid(1), tid).unwrap();
        }
        write.remove_txn(tid).unwrap();
        write.commit().unwrap();
    }

    #[test]
    fn a_row_that_is_gone_with_no_pack_is_a_corrupt_transaction() {
        let store = three_records();
        let iterator = store.iterator(None, None).unwrap();
        remove_outside_a_pack(&store, Tid(200), false);

        let txns: Vec<std::result::Result<Tid, String>> = iterator
            .map(|txn| txn.map(|txn| txn.tid).map_err(|e| format!("{e:?}")))
            .collect();
        let corrupt = format!("{:?}", ZoeError::CorruptTxn { tid: Tid(200) });
        assert_eq!(txns, [Ok(Tid(100)), Err(corrupt), Ok(Tid(300))]);
    }

    /// Return a store with these transactions: 100 writes oids 3 and 1,
    /// 200 writes oid 1, 300 writes oid 2, 400 writes nothing, and 500
    /// deletes oid 3.
    fn written() -> Store {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        restore(
            &store,
            100,
            &[(Oid(3), Some(pickle(1))), (Oid(1), Some(pickle(1)))],
        );
        restore(&store, 200, &[(Oid(1), Some(pickle(2)))]);
        restore(&store, 300, &[(Oid(2), Some(pickle(1)))]);
        restore(&store, 400, &[]);
        restore(&store, 500, &[(Oid(3), None)]);
        store
    }

    /// The oids that `written_oids` gives with slices of 1 to 4
    /// transactions. Check that each slice gives the same.
    fn oids_written(store: &Store, from: u64, to: u64, limit: usize) -> Option<Vec<u64>> {
        let whole = store.written_oids(Tid(from), Tid(to), limit).unwrap();
        for slice in 1..=4 {
            let sliced = store
                .written_oids_in_slices(Tid(from), Tid(to), limit, slice)
                .unwrap();
            assert_eq!(sliced, whole, "slices of {slice}, from {from} to {to}");
        }
        whole.map(|oids| oids.into_iter().map(|oid| oid.0).collect())
    }

    #[test]
    fn written_oids_gives_each_oid_once_in_ascending_order() {
        let store = written();
        assert_eq!(oids_written(&store, 0, u64::MAX, 10), Some(vec![1, 2, 3]));
        assert_eq!(oids_written(&store, 100, 100, 10), Some(vec![1, 3]));
        assert_eq!(oids_written(&store, 101, 300, 10), Some(vec![1, 2]));
        // A deletion is a written oid.
        assert_eq!(oids_written(&store, 450, 500, 10), Some(vec![3]));
        // A transaction that wrote nothing gives no oid.
        assert_eq!(oids_written(&store, 301, 499, 10), Some(vec![]));
        assert_eq!(oids_written(&store, 501, u64::MAX, 10), Some(vec![]));
        assert_eq!(oids_written(&store, 300, 200, 10), Some(vec![]));
    }

    #[test]
    fn written_oids_gives_none_past_the_limit() {
        let store = written();
        // Four transactions wrote records. The one that wrote nothing does
        // not count.
        assert_eq!(oids_written(&store, 0, u64::MAX, 4), Some(vec![1, 2, 3]));
        assert_eq!(oids_written(&store, 0, u64::MAX, 3), None);
        assert_eq!(oids_written(&store, 101, u64::MAX, 3), Some(vec![1, 2, 3]));
        assert_eq!(oids_written(&store, 101, u64::MAX, 2), None);
        assert_eq!(oids_written(&store, 301, 499, 0), Some(vec![]));
        assert_eq!(oids_written(&store, 300, 300, 0), None);
    }

    #[test]
    fn a_history_entry_that_is_gone_with_no_pack_is_a_corrupt_transaction() {
        let store = three_records();
        let versions = store.backend.history(Oid(1), 10).unwrap();
        remove_outside_a_pack(&store, Tid(200), true);

        let history = store.history_entries(Oid(1), versions, 10);
        assert!(
            matches!(history, Err(ZoeError::CorruptTxn { tid: Tid(200) })),
            "{history:?}"
        );
    }

    #[test]
    fn the_sequence_rows_list_and_read_without_a_reservation() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        assert!(store.sequence_names().unwrap().is_empty());
        store.sequence("lexicon").next().unwrap();
        store.sequence("intids").next().unwrap();
        assert_eq!(store.sequence_names().unwrap(), ["intids", "lexicon"]);
        // The first reservation reserves the values 1 to 100.
        assert_eq!(store.sequence_end("intids").unwrap(), Some(101));
        assert_eq!(store.sequence_end("absent").unwrap(), None);
        // The read reserved nothing.
        assert_eq!(store.sequence("intids").next().unwrap(), 2);
        assert_eq!(store.sequence_end("intids").unwrap(), Some(101));
    }
}
