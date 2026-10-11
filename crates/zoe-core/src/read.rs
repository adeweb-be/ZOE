//! The read and copy paths of a store: `load_before`, `load_serial`,
//! `history`, the transaction iterator and the current-record iteration.
//!
//! A record with an empty pickle is a deletion. A load of a deletion
//! fails with `NotFound`. Each backend read verifies the checksum of every
//! frame that it returns.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Weak};

use crate::backend::{Backend, Record, TxnMeta};
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

/// An iterator over transactions in tid order. Each item holds the
/// records of its transaction.
///
/// The iterator does not borrow the store, and it can move to another
/// thread. It does not keep the store open: after the `Store` is dropped,
/// the next call gives `Closed`, then the iterator ends.
pub struct Transactions {
    store: Weak<Opened>,
    rows: VecDeque<Result<TxnMeta>>,
    oids: BTreeMap<Tid, Vec<Oid>>,
}

impl Iterator for Transactions {
    type Item = Result<TxnInfo>;

    fn next(&mut self) -> Option<Result<TxnInfo>> {
        if self.rows.is_empty() && self.oids.is_empty() {
            return None;
        }
        // The store stays open until this call returns.
        let Some(store) = self.store.upgrade() else {
            self.rows.clear();
            self.oids.clear();
            return Some(Err(ZoeError::Closed));
        };
        // Records of a tid before the next row, or after the last row, have
        // no row. They come in tid order, as an error.
        if let Some(&tid) = self.oids.keys().next() {
            let no_row = match self.rows.front() {
                Some(Ok(row)) => tid < row.tid,
                Some(Err(_)) => false,
                None => true,
            };
            if no_row {
                self.oids.remove(&tid);
                return Some(Err(ZoeError::CorruptTxn { tid }));
            }
        }
        let row = match self.rows.pop_front()? {
            Ok(row) => row,
            Err(error) => {
                // A row that does not read can own any records before the
                // next row that reads. Drop them, so that they give no
                // second error.
                let next = self.rows.iter().find_map(|row| row.as_ref().ok());
                match next {
                    Some(row) => self.oids = self.oids.split_off(&row.tid),
                    None => self.oids.clear(),
                }
                return Some(Err(error));
            }
        };
        let oids = self.oids.remove(&row.tid).unwrap_or_default();
        Some(txn_info(&**store, row, oids))
    }
}

/// Read the records of the transaction `row` from `backend`. The records
/// must match the oid count of the row.
fn txn_info(backend: &dyn Backend, row: TxnMeta, oids: Vec<Oid>) -> Result<TxnInfo> {
    let tid = row.tid;
    if oids.len() as u64 != row.oid_count {
        return Err(ZoeError::CorruptTxn { tid });
    }
    let records = oids
        .into_iter()
        .map(|oid| {
            let data = backend
                .load_serial(oid, tid)?
                .ok_or(ZoeError::NotFound { oid })?;
            let data = (!data.is_empty()).then_some(data);
            Ok(RecordInfo { oid, tid, data })
        })
        .collect::<Result<_>>()?;
    Ok(TxnInfo {
        tid,
        user: row.user,
        description: row.description,
        extension: row.extension,
        records,
    })
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
    pub fn history(&self, oid: Oid, size: usize) -> Result<Vec<HistoryEntry>> {
        let versions = self.backend.history(oid, size.max(1))?;
        if versions.is_empty() {
            return Err(ZoeError::NotFound { oid });
        }
        versions
            .into_iter()
            .take(size)
            .map(|version| {
                let tid = version.tid;
                let row = self
                    .backend
                    .iter_txns(tid, tid)
                    .next()
                    .unwrap_or(Err(ZoeError::CorruptTxn { tid }))?;
                Ok(HistoryEntry {
                    tid,
                    user: row.user,
                    description: row.description,
                    size: version.size,
                })
            })
            .collect()
    }

    /// Iterate over the transactions from `start` to `stop`, both included,
    /// in tid order. `None` leaves a bound open. Each transaction comes
    /// with all of its records.
    ///
    /// The iterator stops at the last tid at the time of this call. The
    /// iterator leaves out a transaction that commits after this call.
    ///
    /// A transaction whose records do not match the oid count of its row
    /// gives `CorruptTxn`. Records of a tid that has no row give
    /// `CorruptTxn` too, but only when the range has a row. A row that does
    /// not read gives its own error once, and the records before the next
    /// row that reads give no error.
    pub fn iterator(&self, start: Option<Tid>, stop: Option<Tid>) -> Result<Transactions> {
        let start = start.unwrap_or(Tid::ZERO);
        let stop = stop.unwrap_or(Tid::MAX).min(self.last_transaction());
        let rows: Vec<_> = if start > stop {
            Vec::new()
        } else {
            self.backend.iter_txns(start, stop).collect()
        };
        // A range with no row needs no scan of the records.
        let oids = if rows.is_empty() {
            BTreeMap::new()
        } else {
            self.backend.txn_oids(start, stop)?
        };
        Ok(Transactions {
            store: Arc::downgrade(&self.backend),
            rows: rows.into(),
            oids,
        })
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
    use redb::{ReadableTable, TableDefinition};
    use xxhash_rust::xxh3::Xxh3;

    use super::*;
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

    /// Give the `records` table of the closed store in `vfs` to `change`,
    /// and commit the change.
    fn change_records(vfs: &MemVfs, change: impl FnOnce(&mut redb::Table<&[u8; 16], &[u8]>)) {
        let records: TableDefinition<&[u8; 16], &[u8]> = TableDefinition::new("records");
        let db = redb::Builder::new()
            .create_with_backend(vfs.file("zoe.redb"))
            .unwrap();
        let txn = db.begin_write().unwrap();
        change(&mut txn.open_table(records).unwrap());
        txn.commit().unwrap();
    }

    /// Flip the last bit of the record of `oid` at `tid` in the closed
    /// store in `vfs`.
    fn corrupt(vfs: &MemVfs, oid: Oid, tid: Tid) {
        change_records(vfs, |table| {
            let key = format::record_key(oid, tid);
            let mut frame = table.get(&key).unwrap().unwrap().value().to_vec();
            *frame.last_mut().unwrap() ^= 1;
            table.insert(&key, frame.as_slice()).unwrap();
        });
    }

    /// Write a valid record of `oid` at `tid` that no transaction row
    /// counts, in the closed store in `vfs`.
    fn add_record(vfs: &MemVfs, oid: Oid, tid: Tid) {
        change_records(vfs, |table| {
            let key = format::record_key(oid, tid);
            let frame = format::encode_frame(&key, format::MAIN_BRANCH, Tid::ZERO, b"x");
            table.insert(&key, frame.as_slice()).unwrap();
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
            let mut hasher = Xxh3::new();
            hasher.update(&key);
            hasher.update(&frame[..24]);
            hasher.update(&frame[format::HEADER_LEN..]);
            frame[24..32].copy_from_slice(&hasher.digest().to_le_bytes());
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
    fn the_iterator_checks_the_records_against_the_rows() {
        let vfs = MemVfs::new();
        drop(three_records_in(&vfs));
        // A second record for the transaction at 200, and records at 150
        // and 250, where no transaction row is.
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
        // A range with no row reads no record, so it finds no error.
        let range = store.iterator(Some(Tid(140)), Some(Tid(160))).unwrap();
        assert_eq!(range.count(), 0);
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
}
