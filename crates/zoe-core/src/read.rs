//! The read and copy paths of a store: `load_before`, `load_serial`,
//! `history`, the transaction iterator and the current-record iteration.
//!
//! A record with an empty pickle is a deletion. A load of a deletion
//! fails with `NotFound`. Each backend read verifies the checksum of every
//! frame that it returns.

use std::collections::BTreeMap;

use crate::backend::{Record, TxnIter, TxnMeta};
use crate::error::{Result, ZoeError};
use crate::store::Store;
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
pub struct Transactions<'s> {
    store: &'s Store,
    rows: TxnIter<'s>,
    oids: BTreeMap<Tid, Vec<Oid>>,
}

impl Iterator for Transactions<'_> {
    type Item = Result<TxnInfo>;

    fn next(&mut self) -> Option<Result<TxnInfo>> {
        let row = match self.rows.next()? {
            Ok(row) => row,
            Err(error) => return Some(Err(error)),
        };
        let oids = self.oids.remove(&row.tid).unwrap_or_default();
        Some(self.txn_info(row, oids))
    }
}

impl Transactions<'_> {
    /// Read the records of the transaction `row`.
    fn txn_info(&self, row: TxnMeta, oids: Vec<Oid>) -> Result<TxnInfo> {
        let tid = row.tid;
        let records = oids
            .into_iter()
            .map(|oid| {
                let data = self
                    .store
                    .backend
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
    /// in tid order. `None` leaves a bound open.
    ///
    /// The iterator stops at the last tid at the time of this call. A
    /// later transaction can commit while the iterator runs, so it is left
    /// out. Each transaction comes with all of its records.
    pub fn iterator(&self, start: Option<Tid>, stop: Option<Tid>) -> Result<Transactions<'_>> {
        let start = start.unwrap_or(Tid::ZERO);
        let stop = stop.unwrap_or(Tid::MAX).min(self.last_transaction());
        let (rows, oids): (TxnIter<'_>, _) = if start > stop {
            (Box::new(std::iter::empty()), BTreeMap::new())
        } else {
            let rows = self.backend.iter_txns(start, stop);
            (rows, self.backend.txn_oids(start, stop)?)
        };
        Ok(Transactions {
            store: self,
            rows,
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

    /// Flip the last bit of the record of `oid` at `tid` in the store in
    /// `vfs`. The store must be closed.
    fn corrupt(vfs: &MemVfs, oid: Oid, tid: Tid) {
        let records: TableDefinition<&[u8; 16], &[u8]> = TableDefinition::new("records");
        let db = redb::Builder::new()
            .create_with_backend(vfs.file("zoe.redb"))
            .unwrap();
        let txn = db.begin_write().unwrap();
        {
            let mut table = txn.open_table(records).unwrap();
            let key = format::record_key(oid, tid);
            let mut frame = table.get(&key).unwrap().unwrap().value().to_vec();
            *frame.last_mut().unwrap() ^= 1;
            table.insert(&key, frame.as_slice()).unwrap();
        }
        txn.commit().unwrap();
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
