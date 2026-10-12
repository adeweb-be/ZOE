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
//! A transaction iterator of the core holds only a weak reference to the
//! store. A record iterator shares the transaction that the transaction
//! iterator gave last.

use std::collections::HashMap;
use std::sync::Arc;

use zoe_core::{RecordInfo, Tid, Transactions, TxnInfo, ZoeError};

/// The most iterators that one connection keeps open, of both kinds. A
/// client that never sends `iterator_gc` cannot grow the memory of the
/// server without a bound. A stock client sends `iterator_gc` at its next
/// `tpc_finish` or `tpc_abort`, and a client that reads each iterator to
/// its end keeps one or two open.
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
        /// The last transaction that the iterator gave. A record iterator
        /// starts from it.
        last: Option<Arc<TxnInfo>>,
    },
    Records {
        txn: Arc<TxnInfo>,
        /// The index of the next record.
        next: usize,
    },
}

/// The open iterators of one connection.
#[derive(Default)]
pub(crate) struct Iterators {
    open: HashMap<u64, Open>,
    /// The id of the next iterator.
    next: u64,
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
        match transactions.next() {
            None => {
                self.open.remove(&id);
                Ok(None)
            }
            Some(Ok(txn)) => {
                let txn = Arc::new(txn);
                *last = Some(Arc::clone(&txn));
                Ok(Some(Ok(txn)))
            }
            Some(Err(error)) => Ok(Some(Err(error))),
        }
    }

    /// Start a record iterator over the records of `tid`, the last
    /// transaction that the transaction iterator `id` gave. Return the id
    /// of the record iterator.
    pub(crate) fn start_records(&mut self, id: u64, tid: Tid) -> Result<u64, Refused> {
        let Some(Open::Transactions {
            last: Some(txn), ..
        }) = self.open.get(&id)
        else {
            return Err(Refused::Unknown);
        };
        if txn.tid != tid {
            return Err(Refused::OutOfOrder(tid));
        }
        let txn = Arc::clone(txn);
        self.add(Open::Records { txn, next: 0 })
    }

    /// Return the next record of the record iterator `id`, or `None` at its
    /// end. At its end, the iterator is gone.
    pub(crate) fn next_record(&mut self, id: u64) -> Result<Option<RecordInfo>, Refused> {
        let Some(Open::Records { txn, next }) = self.open.get_mut(&id) else {
            return Err(Refused::Unknown);
        };
        match txn.records.get(*next) {
            Some(record) => {
                *next += 1;
                Ok(Some(record.clone()))
            }
            None => {
                self.open.remove(&id);
                Ok(None)
            }
        }
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

#[cfg(test)]
mod tests {
    use std::sync::Weak;

    use zoe_core::{Oid, Store};

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

    #[test]
    fn the_ids_of_both_kinds_share_one_counter_from_0() {
        let store = store("ids", 2);
        let mut iterators = Iterators::default();
        let first = iterators
            .start(store.iterator(None, None).unwrap())
            .unwrap();
        assert_eq!(first, 0);
        let txn = next(&mut iterators, first).unwrap();
        let records = iterators.start_records(first, txn.tid).unwrap();
        assert_eq!(records, 1);
        let second = iterators
            .start(store.iterator(None, None).unwrap())
            .unwrap();
        assert_eq!(second, 2);
        // A transaction id is not a record id, and the other way round.
        assert_eq!(iterators.next_record(first), Err(Refused::Unknown));
        assert!(matches!(
            iterators.next_transaction(records),
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
        let records = iterators.start_records(id, txn.tid).unwrap();
        let oids: Vec<_> = std::iter::from_fn(|| iterators.next_record(records).unwrap())
            .map(|record| record.oid)
            .collect();
        assert_eq!(oids, [Oid(1), Oid(2)]);
        assert_eq!(iterators.next_record(records), Err(Refused::Unknown));
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
            iterators.start_records(id, Tid::ZERO),
            Err(Refused::Unknown)
        );
        let first = next(&mut iterators, id).unwrap();
        let second = next(&mut iterators, id).unwrap();
        assert_eq!(
            iterators.start_records(id, first.tid),
            Err(Refused::OutOfOrder(first.tid))
        );
        // Two record iterators of the same transaction are independent.
        let a = iterators.start_records(id, second.tid).unwrap();
        let b = iterators.start_records(id, second.tid).unwrap();
        assert_eq!(iterators.next_record(a).unwrap().unwrap().oid, Oid(1));
        assert_eq!(iterators.next_record(b).unwrap().unwrap().oid, Oid(1));
        assert_eq!(iterators.next_record(a).unwrap().unwrap().oid, Oid(2));
    }

    #[test]
    fn the_end_of_the_connection_frees_its_iterators() {
        let store = store("drop", 1);
        let mut iterators = Iterators::default();
        let id = iterators
            .start(store.iterator(None, None).unwrap())
            .unwrap();
        let txn = next(&mut iterators, id).unwrap();
        iterators.start_records(id, txn.tid).unwrap();
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
        let records = iterators.start_records(id, txn.tid).unwrap();
        let weak: Weak<TxnInfo> = Arc::downgrade(&txn);
        drop(txn);
        // Ids that never existed, and an id twice.
        iterators.gc([7, records, 99, records, u64::MAX]);
        assert_eq!(iterators.next_record(records), Err(Refused::Unknown));
        assert_eq!(iterators.len(), 1);
        iterators.gc([id]);
        assert_eq!(iterators.len(), 0);
        // Nothing holds the transaction any more.
        assert!(weak.upgrade().is_none());
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
        assert_eq!(iterators.start_records(ids[0], txn.tid), Err(Refused::Full));
        // An iterator at its end frees its place.
        assert!(next(&mut iterators, ids[1]).is_some());
        assert!(next(&mut iterators, ids[1]).is_none());
        let records = iterators.start_records(ids[0], txn.tid).unwrap();
        assert_eq!(records, MAX_OPEN as u64);
        // So does gc.
        iterators.gc([ids[2]]);
        assert!(iterators.start(store.iterator(None, None).unwrap()).is_ok());
    }
}
