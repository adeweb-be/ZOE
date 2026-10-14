//! The iterators of one connection: the transaction iterators of
//! `iterator_start` and the record iterators of `iterator_record_start`.
//!
//! An iterator id belongs to its connection. The ids of both kinds come
//! from one counter, from 0, as in ZEO. An iterator at its end is gone.
//! `iterator_gc` removes the iterators that the client dropped, and the end
//! of the connection removes the others. `iterator_gc` can name an id that
//! is gone already: the client lists each record iterator that it dropped,
//! at its end or not. So `gc` ignores an id that it does not know.
//!
//! Between two calls, an open iterator holds no pickle. A transaction
//! iterator keeps the tid of the last transaction that it gave. The
//! iterator of the core under it holds a slice of tids and only a weak
//! reference to the store. A record iterator keeps its tid, the oids of its
//! transaction and its place. It reads each record when the client asks
//! for it.
//!
//! The connection keeps the one transaction that it read last, with its
//! records. A stock client reads a transaction, then its records. So it
//! gets each record from that transaction, and the store reads each record
//! once.

use std::collections::HashMap;
use std::sync::Arc;

use zoe_core::{Oid, RecordInfo, Store, Tid, Transactions, TxnInfo, ZoeError};

/// The most iterators that one connection keeps open, of both kinds. An
/// open iterator holds no pickle, so the iterators of a connection hold at
/// most the slices of tids and the oids of the transactions of the record
/// iterators, plus the one transaction that the connection read last. A
/// stock client sends `iterator_gc` only at its next `tpc_finish` or
/// `tpc_abort`, so a read-only stock client never sends it, and only the
/// end of its connection frees its iterators. A client that reads each
/// iterator to its end keeps one or two open.
pub(crate) const MAX_OPEN: usize = 1_000;

/// Why a call on the iterators fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refused {
    /// No iterator of the kind has the id.
    Unknown,
    /// The tid of `iterator_record_start`, which is not the tid of the
    /// last transaction that the transaction iterator gave.
    OutOfOrder(Tid),
    /// The connection has `MAX_OPEN` iterators open.
    Full,
}

/// One open iterator.
enum Open {
    Transactions {
        transactions: Transactions,
        /// The tid of the last transaction that the iterator gave. A record
        /// iterator starts from it.
        last: Option<Tid>,
    },
    Records {
        /// The tid of the transaction.
        tid: Tid,
        /// The oids that the transaction wrote, in ascending order.
        oids: Vec<Oid>,
        /// The index in `oids` of the next record.
        next: usize,
    },
}

/// The open iterators of one connection.
#[derive(Default)]
pub(crate) struct Iterators {
    open: HashMap<u64, Open>,
    /// The id of the next iterator.
    next: u64,
    /// The transaction that the connection read last, with its records.
    read: Option<Arc<TxnInfo>>,
}

impl Iterators {
    /// Keep `transactions` open, and return its id.
    pub(crate) fn start(&mut self, transactions: Transactions) -> Result<u64, Refused> {
        self.add(Open::Transactions {
            transactions,
            last: None,
        })
    }

    /// Return the next transaction of the transaction iterator `id`, or
    /// `None` at its end. At its end, the iterator is gone.
    ///
    /// An error of the core leaves the iterator open, so the client can
    /// ask for the next transaction or drop the iterator.
    pub(crate) fn next_transaction(
        &mut self,
        id: u64,
    ) -> Result<Option<Result<Arc<TxnInfo>, ZoeError>>, Refused> {
        let Some(Open::Transactions { transactions, last }) = self.open.get_mut(&id) else {
            return Err(Refused::Unknown);
        };
        // Free the transaction read last before the next one is read.
        self.read = None;
        match transactions.next() {
            None => {
                self.open.remove(&id);
                Ok(None)
            }
            Some(Ok(txn)) => {
                let txn = Arc::new(txn);
                *last = Some(txn.tid);
                self.read = Some(Arc::clone(&txn));
                Ok(Some(Ok(txn)))
            }
            Some(Err(error)) => Ok(Some(Err(error))),
        }
    }

    /// Start a record iterator over the records of `tid`, the last
    /// transaction that the transaction iterator `id` gave. Return the id
    /// of the record iterator.
    ///
    /// The oids come from the transaction that the connection read last,
    /// or else from the transaction index of `store`. An error of the core
    /// starts no iterator.
    pub(crate) fn start_records(
        &mut self,
        store: &Store,
        id: u64,
        tid: Tid,
    ) -> Result<Result<u64, ZoeError>, Refused> {
        let Some(Open::Transactions {
            last: Some(last), ..
        }) = self.open.get(&id)
        else {
            return Err(Refused::Unknown);
        };
        if *last != tid {
            return Err(Refused::OutOfOrder(tid));
        }
        if self.open.len() >= MAX_OPEN {
            return Err(Refused::Full);
        }
        let oids = match self.read.as_deref() {
            Some(txn) if txn.tid == tid => txn.records.iter().map(|record| record.oid).collect(),
            _ => match store.written_oids(tid, tid, 1) {
                Ok(oids) => oids.unwrap_or_default(),
                Err(error) => return Ok(Err(error)),
            },
        };
        self.add(Open::Records { tid, oids, next: 0 }).map(Ok)
    }

    /// Return the next record of the record iterator `id`, or `None` at its
    /// end. At its end, the iterator is gone.
    ///
    /// The record comes from the transaction that the connection read
    /// last, or else from `store`. The data of a deletion is `None`. The
    /// iterator skips a record that a pack removed. An error of the core
    /// leaves the iterator at its place.
    pub(crate) fn next_record(
        &mut self,
        store: &Store,
        id: u64,
    ) -> Result<Option<Result<RecordInfo, ZoeError>>, Refused> {
        let Some(Open::Records { tid, oids, next }) = self.open.get_mut(&id) else {
            return Err(Refused::Unknown);
        };
        let tid = *tid;
        while let Some(&oid) = oids.get(*next) {
            let found = match self.read.as_deref() {
                Some(txn) if txn.tid == tid => Ok(txn
                    .records
                    .binary_search_by_key(&oid, |record| record.oid)
                    .ok()
                    .map(|at| txn.records[at].clone())),
                _ => read_record(store, oid, tid),
            };
            match found {
                Ok(Some(record)) => {
                    *next += 1;
                    return Ok(Some(Ok(record)));
                }
                // A pack removed the record.
                Ok(None) => *next += 1,
                Err(error) => return Ok(Some(Err(error))),
            }
        }
        self.open.remove(&id);
        Ok(None)
    }

    /// Remove the iterators `ids`. An id that names no open iterator does
    /// nothing.
    pub(crate) fn gc(&mut self, ids: impl IntoIterator<Item = u64>) {
        for id in ids {
            self.open.remove(&id);
        }
    }

    /// Return the number of open iterators.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.open.len()
    }

    fn add(&mut self, open: Open) -> Result<u64, Refused> {
        if self.open.len() >= MAX_OPEN {
            return Err(Refused::Full);
        }
        let id = self.next;
        self.next += 1;
        self.open.insert(id, open);
        Ok(id)
    }
}

/// Read the record that `tid` wrote for `oid` from `store`. The data of a
/// deletion is `None`. Return `None` when the record does not exist, as
/// after a pack that removed it.
fn read_record(store: &Store, oid: Oid, tid: Tid) -> Result<Option<RecordInfo>, ZoeError> {
    let data = match store.load_serial(oid, tid) {
        Ok(data) => Some(data),
        // A deletion and a removed record both load as `NotFound`. Only a
        // deletion has a stored record.
        Err(ZoeError::NotFound { .. }) => match store.record_info(oid, tid) {
            Ok(_) => None,
            Err(ZoeError::NotFound { .. }) => return Ok(None),
            Err(error) => return Err(error),
        },
        Err(error) => return Err(error),
    };
    Ok(Some(RecordInfo { oid, tid, data }))
}

#[cfg(test)]
mod tests {
    use std::sync::Weak;

    use super::*;

    /// A store with `n` transactions. Each writes the oids 1 and 2.
    fn store(name: &str, n: usize) -> Store {
        let dir = std::env::temp_dir().join(format!("zoe-iteration-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(&dir).unwrap();
        let mut serials = [Tid::ZERO; 2];
        for _ in 0..n {
            let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
            for (at, serial) in serials.iter().enumerate() {
                txn.store(Oid(at as u64 + 1), *serial, b"data").unwrap();
            }
            txn.vote().unwrap();
            let tid = txn.finish().unwrap();
            serials = [tid; 2];
        }
        store
    }

    /// Return the next transaction, which must read.
    fn next(iterators: &mut Iterators, id: u64) -> Option<Arc<TxnInfo>> {
        iterators
            .next_transaction(id)
            .unwrap()
            .map(|txn| txn.unwrap())
    }

    /// Start a record iterator, which must start.
    fn records(iterators: &mut Iterators, store: &Store, id: u64, tid: Tid) -> u64 {
        iterators.start_records(store, id, tid).unwrap().unwrap()
    }

    /// Return the next record, which must read.
    fn record(iterators: &mut Iterators, store: &Store, id: u64) -> Option<RecordInfo> {
        iterators
            .next_record(store, id)
            .unwrap()
            .map(|record| record.unwrap())
    }

    /// Return the rest of the records of the record iterator `id`.
    fn rest(iterators: &mut Iterators, store: &Store, id: u64) -> Vec<RecordInfo> {
        std::iter::from_fn(|| record(iterators, store, id)).collect()
    }

    #[test]
    fn the_ids_of_both_kinds_share_one_counter_from_0() {
        let store = store("ids", 2);
        let mut iterators = Iterators::default();
        let first = iterators
            .start(store.iterator(None, None).unwrap())
            .unwrap();
        assert_eq!(first, 0);
        let txn = next(&mut iterators, first).unwrap();
        let rid = records(&mut iterators, &store, first, txn.tid);
        assert_eq!(rid, 1);
        let second = iterators
            .start(store.iterator(None, None).unwrap())
            .unwrap();
        assert_eq!(second, 2);
        // A transaction id is not a record id, and the other way round.
        assert_eq!(
            iterators.next_record(&store, first).err(),
            Some(Refused::Unknown)
        );
        assert!(matches!(
            iterators.next_transaction(rid),
            Err(Refused::Unknown)
        ));
    }

    #[test]
    fn an_iterator_at_its_end_is_gone() {
        let store = store("end", 1);
        let mut iterators = Iterators::default();
        let id = iterators
            .start(store.iterator(None, None).unwrap())
            .unwrap();
        let txn = next(&mut iterators, id).unwrap();
        let rid = records(&mut iterators, &store, id, txn.tid);
        let oids: Vec<_> = rest(&mut iterators, &store, rid)
            .into_iter()
            .map(|record| record.oid)
            .collect();
        assert_eq!(oids, [Oid(1), Oid(2)]);
        assert_eq!(
            iterators.next_record(&store, rid).err(),
            Some(Refused::Unknown)
        );
        assert!(next(&mut iterators, id).is_none());
        assert!(matches!(
            iterators.next_transaction(id),
            Err(Refused::Unknown)
        ));
        assert_eq!(iterators.len(), 0);
    }

    #[test]
    fn a_record_iterator_starts_only_from_the_last_transaction() {
        let store = store("out-of-order", 2);
        let mut iterators = Iterators::default();
        let id = iterators
            .start(store.iterator(None, None).unwrap())
            .unwrap();
        // No transaction yet.
        assert_eq!(
            iterators.start_records(&store, id, Tid::ZERO).err(),
            Some(Refused::Unknown)
        );
        let first = next(&mut iterators, id).unwrap();
        let second = next(&mut iterators, id).unwrap();
        assert_eq!(
            iterators.start_records(&store, id, first.tid).err(),
            Some(Refused::OutOfOrder(first.tid))
        );
        // Two record iterators of the same transaction are independent.
        let a = records(&mut iterators, &store, id, second.tid);
        let b = records(&mut iterators, &store, id, second.tid);
        assert_eq!(record(&mut iterators, &store, a).unwrap().oid, Oid(1));
        assert_eq!(record(&mut iterators, &store, b).unwrap().oid, Oid(1));
        assert_eq!(record(&mut iterators, &store, a).unwrap().oid, Oid(2));
    }

    #[test]
    fn an_open_iterator_holds_no_transaction() {
        let store = store("no-pickle", 2);
        let mut iterators = Iterators::default();
        let a = iterators
            .start(store.iterator(None, None).unwrap())
            .unwrap();
        let b = iterators
            .start(store.iterator(None, None).unwrap())
            .unwrap();
        let txn = next(&mut iterators, a).unwrap();
        let tid = txn.tid;
        let rid = records(&mut iterators, &store, a, tid);
        let weak: Weak<TxnInfo> = Arc::downgrade(&txn);
        drop(txn);
        // The connection keeps the transaction that it read last.
        assert!(weak.upgrade().is_some());
        // A read of the next transaction frees it, and the record iterator
        // stays open.
        let other = next(&mut iterators, b).unwrap();
        assert_eq!(other.tid, tid);
        drop(other);
        next(&mut iterators, b).unwrap();
        assert!(weak.upgrade().is_none());
        // The record iterator reads its records from the store.
        let expected: Vec<_> = [Oid(1), Oid(2)]
            .into_iter()
            .map(|oid| RecordInfo {
                oid,
                tid,
                data: Some(b"data".to_vec()),
            })
            .collect();
        assert_eq!(rest(&mut iterators, &store, rid), expected);
        // A record iterator that starts when the connection read another
        // transaction reads its oids from the store.
        let c = iterators
            .start(store.iterator(None, None).unwrap())
            .unwrap();
        assert_eq!(next(&mut iterators, c).unwrap().tid, tid);
        assert!(next(&mut iterators, b).is_none());
        let rid = records(&mut iterators, &store, c, tid);
        assert_eq!(rest(&mut iterators, &store, rid), expected);
    }

    #[test]
    fn a_record_iterator_gives_a_deletion_with_no_data() {
        let store = store("deletion", 1);
        let serial = store.last_transaction();
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.store(Oid(1), serial, b"new").unwrap();
        txn.delete(Oid(2), serial).unwrap();
        txn.vote().unwrap();
        let tid = txn.finish().unwrap();
        let expected = [
            RecordInfo {
                oid: Oid(1),
                tid,
                data: Some(b"new".to_vec()),
            },
            RecordInfo {
                oid: Oid(2),
                tid,
                data: None,
            },
        ];
        let mut iterators = Iterators::default();
        let id = iterators
            .start(store.iterator(Some(tid), None).unwrap())
            .unwrap();
        let other = iterators
            .start(store.iterator(None, None).unwrap())
            .unwrap();
        next(&mut iterators, id).unwrap();
        // From the transaction that the connection read last.
        let cached = records(&mut iterators, &store, id, tid);
        assert_eq!(rest(&mut iterators, &store, cached), expected);
        // From the store, when the connection read another transaction.
        assert_ne!(next(&mut iterators, other).unwrap().tid, tid);
        let read = records(&mut iterators, &store, id, tid);
        assert_eq!(rest(&mut iterators, &store, read), expected);
    }

    #[test]
    fn a_record_iterator_skips_the_records_that_a_pack_removed() {
        let store = store("pack", 2);
        let mut iterators = Iterators::default();
        let id = iterators
            .start(store.iterator(None, None).unwrap())
            .unwrap();
        let first = next(&mut iterators, id).unwrap().tid;
        let rid = records(&mut iterators, &store, id, first);
        next(&mut iterators, id).unwrap();
        // The pack removes the records of the first transaction, which are
        // not current.
        store.pack(store.last_transaction()).unwrap();
        assert!(matches!(
            store.record_info(Oid(1), first),
            Err(ZoeError::NotFound { .. })
        ));
        assert_eq!(rest(&mut iterators, &store, rid), []);
        assert_eq!(iterators.len(), 1);
    }

    #[test]
    fn the_end_of_the_connection_frees_its_iterators() {
        let store = store("drop", 1);
        let mut iterators = Iterators::default();
        let id = iterators
            .start(store.iterator(None, None).unwrap())
            .unwrap();
        let txn = next(&mut iterators, id).unwrap();
        records(&mut iterators, &store, id, txn.tid);
        let weak = Arc::downgrade(&txn);
        drop(txn);
        assert!(weak.upgrade().is_some());
        // A session owns the iterators of its connection.
        drop(iterators);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn gc_removes_the_ids_and_ignores_unknown_ids() {
        let store = store("gc", 1);
        let mut iterators = Iterators::default();
        let id = iterators
            .start(store.iterator(None, None).unwrap())
            .unwrap();
        let txn = next(&mut iterators, id).unwrap();
        let rid = records(&mut iterators, &store, id, txn.tid);
        // Ids that never existed, and an id twice.
        iterators.gc([7, rid, 99, rid, u64::MAX]);
        assert_eq!(
            iterators.next_record(&store, rid).err(),
            Some(Refused::Unknown)
        );
        assert_eq!(iterators.len(), 1);
        iterators.gc([id]);
        assert_eq!(iterators.len(), 0);
        // A new id never reuses an old one.
        let again = iterators
            .start(store.iterator(None, None).unwrap())
            .unwrap();
        assert_eq!(again, 2);
    }

    #[test]
    fn a_connection_keeps_at_most_max_open_iterators() {
        let store = store("bound", 1);
        let mut iterators = Iterators::default();
        let ids: Vec<_> = (0..MAX_OPEN)
            .map(|_| {
                iterators
                    .start(store.iterator(None, None).unwrap())
                    .unwrap()
            })
            .collect();
        assert_eq!(
            iterators.start(store.iterator(None, None).unwrap()),
            Err(Refused::Full)
        );
        let txn = next(&mut iterators, ids[0]).unwrap();
        assert_eq!(
            iterators.start_records(&store, ids[0], txn.tid).err(),
            Some(Refused::Full)
        );
        // An iterator at its end frees its place.
        assert!(next(&mut iterators, ids[1]).is_some());
        assert!(next(&mut iterators, ids[1]).is_none());
        let rid = records(&mut iterators, &store, ids[0], txn.tid);
        assert_eq!(rid, MAX_OPEN as u64);
        // So does gc.
        iterators.gc([ids[2]]);
        assert!(iterators.start(store.iterator(None, None).unwrap()).is_ok());
    }
}
