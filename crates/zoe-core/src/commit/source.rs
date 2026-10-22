//! The records of a transaction and of its store, as the split merge and
//! the order merge of the vote read them.

use std::cell::{Cell, RefCell};
use std::ops::Deref;

use super::Txn;
use crate::error::Result;
use crate::lock::Held;
use crate::merge::{Records, order, split};
use crate::store::Store;
use crate::types::{Oid, Tid};

/// The records of a transaction and of its store, as the merges read them.
pub(super) struct TxnSource<'t, S: Deref<Target = Store>> {
    txn: &'t Txn<S>,
    /// The locks that the rules of the split merge took.
    pub(super) locked: RefCell<Vec<Held>>,
    /// True while the rules of the split merge run in mode `shadow`: they
    /// take no lock.
    pub(super) shadow: Cell<bool>,
}

impl<'t, S: Deref<Target = Store>> TxnSource<'t, S> {
    pub(super) fn new(txn: &'t Txn<S>) -> Self {
        TxnSource {
            txn,
            locked: RefCell::new(Vec::new()),
            shadow: Cell::new(false),
        }
    }

    /// Release the locks that the rules took after the first `kept`.
    pub(super) fn release_from(&self, kept: usize) {
        let mut locked = self.locked.borrow_mut();
        if locked.len() > kept {
            let released = locked.split_off(kept);
            self.txn.store.locks.release(&released);
        }
    }
}

impl<S: Deref<Target = Store>> Records for TxnSource<'_, S> {
    fn staged(&self, oid: Oid) -> Result<Option<(Tid, Vec<u8>)>> {
        match self.txn.staged.record(oid) {
            Some(staged) if !staged.restored && staged.pickle_len() > 0 => {}
            _ => return Ok(None),
        }
        Ok(self
            .txn
            .staged
            .get(oid)?
            .map(|(serial, pickle)| (serial, pickle.into_owned())))
    }

    fn current(&self, oid: Oid) -> Result<Option<(Tid, Vec<u8>)>> {
        Ok(self
            .txn
            .store
            .backend
            .load_before(oid, Tid::MAX)?
            .filter(|record| !record.data.is_empty())
            .map(|record| (record.tid, record.data)))
    }

    fn at(&self, oid: Oid, serial: Tid) -> Result<Option<Vec<u8>>> {
        self.txn.store.backend.load_serial(oid, serial)
    }
}

impl<S: Deref<Target = Store>> order::Source for TxnSource<'_, S> {
    fn read_current(&self, oid: Oid) -> Option<Tid> {
        self.txn.read_current.get(&oid).copied()
    }

    fn stored(&self) -> Vec<Oid> {
        self.txn.staged.iter().map(|(oid, _)| oid).collect()
    }
}

impl<S: Deref<Target = Store>> split::Source for TxnSource<'_, S> {
    fn stores(&self, oid: Oid) -> bool {
        self.txn.staged.record(oid).is_some()
    }

    fn read_current(&self, oid: Oid) -> bool {
        self.txn.read_current.contains_key(&oid)
    }

    fn server_writes(&self) -> bool {
        self.txn.server_writes
    }

    fn lock(&self, oid: Oid) -> Result<bool> {
        if self.shadow.get() {
            return Ok(true);
        }
        let mut locked = self.locked.borrow_mut();
        if locked.iter().any(|held| held.oid == oid) {
            return Ok(true);
        }
        let store: &Store = &self.txn.store;
        match store.locks.try_exclusive(oid, &*store.clock) {
            Some(held) => {
                locked.push(held);
                Ok(true)
            }
            None => Ok(false),
        }
    }
}
