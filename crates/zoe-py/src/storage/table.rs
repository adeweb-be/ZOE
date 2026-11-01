//! The table of the open transactions of a storage, keyed by the address of
//! the ZODB transaction object, and the part of a storage that the watcher
//! of the signals shares with it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock, TryLockError};
use std::thread;
use std::time::Duration;

use pyo3::prelude::*;
use zoe_client::lock;
use zoe_core::{Store, ZoeError};

use super::{Owned, SetOnDrop, SharedTxn, StoreRef};
use crate::errors::Failure;

/// One open transaction.
pub(super) struct Entry {
    /// The ZODB transaction object. The entry keeps it alive, so that its
    /// address stays its key.
    pub(super) _transaction: Py<PyAny>,
    /// The transaction of the core. It is `None` after `tpc_finish` or
    /// `tpc_abort` took it.
    pub(super) txn: Mutex<Option<SharedTxn>>,
}

impl Entry {
    fn lock(&self) -> MutexGuard<'_, Option<SharedTxn>> {
        lock(&self.txn)
    }

    /// Call `f` with the transaction of the core. Fail when `tpc_finish`
    /// or `tpc_abort` took it.
    pub(super) fn with<T>(
        &self,
        f: impl FnOnce(&mut SharedTxn) -> Result<T, Failure>,
    ) -> Result<T, Failure> {
        match self.lock().as_mut() {
            Some(txn) => f(txn),
            None => Err(ZoeError::Protocol("the transaction is finished or aborted").into()),
        }
    }

    /// Call `f` with the transaction of the core, as `with` does, if no
    /// other call uses it now. Return `None` when another call uses it.
    pub(super) fn try_with<T>(
        &self,
        f: impl FnOnce(&mut SharedTxn) -> Result<T, Failure>,
    ) -> Option<Result<T, Failure>> {
        let mut txn = match self.txn.try_lock() {
            Ok(txn) => txn,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => return None,
        };
        Some(match txn.as_mut() {
            Some(txn) => f(txn),
            None => Err(ZoeError::Protocol("the transaction is finished or aborted").into()),
        })
    }

    /// Take the transaction of the core out of the entry.
    pub(super) fn take(&self) -> Option<SharedTxn> {
        self.lock().take()
    }

    /// Take the transaction of the core out of the entry, if no call uses
    /// it now. Fail when a call uses it.
    fn try_take(&self) -> Result<Option<SharedTxn>, ()> {
        match self.txn.try_lock() {
            Ok(mut txn) => Ok(txn.take()),
            Err(TryLockError::Poisoned(poisoned)) => Ok(poisoned.into_inner().take()),
            Err(TryLockError::WouldBlock) => Err(()),
        }
    }
}

/// The open transactions of a storage.
#[derive(Default)]
pub(super) struct Table {
    /// True after `close` or the close at a signal. The table then refuses
    /// a new transaction.
    pub(super) closed: bool,
    /// The open transactions, keyed by the address of the ZODB
    /// transaction object.
    pub(super) entries: HashMap<usize, Arc<Entry>>,
    /// The entries that the close at a signal took out of `entries`. They
    /// hold Python references, so they stay here until `close` or the drop
    /// of the storage removes them.
    closing: Vec<Arc<Entry>>,
}

impl Table {
    /// Remove every entry and return them.
    pub(super) fn drain(&mut self) -> Vec<Arc<Entry>> {
        let mut entries: Vec<Arc<Entry>> = self.entries.drain().map(|(_, entry)| entry).collect();
        entries.append(&mut self.closing);
        entries
    }
}

/// The parts of a storage that the watcher of the signals shares with it.
///
/// The watcher runs no Python code, so it drops no Python reference. The
/// entries of the table hold Python references: the watcher takes the
/// transaction of the core out of an entry and leaves the entry in the
/// table. `close` and the drop of the storage remove the entries.
pub(crate) struct Shared {
    /// The store. It is `None` after `close`, the drop or the close at a
    /// signal.
    pub(super) store: RwLock<Option<StoreRef>>,
    table: Mutex<Table>,
    /// True when the last share of the store dropped. A read through a
    /// weak handle of the core can still hold the database then.
    database_closed: Arc<AtomicBool>,
}

impl Shared {
    pub(super) fn new(store: Store) -> Self {
        let database_closed = Arc::new(AtomicBool::new(false));
        let store = StoreRef(Arc::new(Owned {
            store,
            _closed: SetOnDrop(Arc::clone(&database_closed)),
        }));
        Self {
            store: RwLock::new(Some(store)),
            table: Mutex::new(Table::default()),
            database_closed,
        }
    }

    pub(super) fn table(&self) -> MutexGuard<'_, Table> {
        lock(&self.table)
    }

    /// Take the store out of the storage. Return `None` when it is already
    /// out.
    pub(super) fn take_store(&self) -> Option<StoreRef> {
        self.store
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }

    /// Mark the storage closed and take its store, as `close` does. Return
    /// `None` when `close` or the drop took the store first.
    ///
    /// The open transactions leave `entries`, so a later call with one of
    /// them raises the error that it raises after `close`.
    pub(crate) fn mark_closed(&self) -> Option<StoreRef> {
        {
            let mut table = self.table();
            table.closed = true;
            let entries: Vec<Arc<Entry>> = table.entries.drain().map(|(_, entry)| entry).collect();
            table.closing.extend(entries);
        }
        self.take_store()
    }

    /// Abort each transaction that the close at a signal took out of
    /// `entries` and that no call uses now. Leave the entries in the table.
    pub(crate) fn abort_idle(&self) {
        // The transactions drop after the guard of the table, because an
        // abort can take time.
        let idle: Vec<SharedTxn> = self
            .table()
            .closing
            .iter()
            .filter_map(|entry| entry.try_take().ok().flatten())
            .collect();
        drop(idle);
    }

    /// Tell if the last share of the store dropped.
    pub(crate) fn database_closed(&self) -> bool {
        self.database_closed.load(Ordering::Acquire)
    }
}

/// Abort the transactions of `entries`.
///
/// A vote that waits for a lock uses its entry until the wait ends. So the
/// transactions that no call uses go first: their abort releases their
/// locks, and a vote that waits for one of them ends. Then the others go,
/// as soon as their calls end.
pub(super) fn abort_all(entries: &[Arc<Entry>]) {
    let mut busy: Vec<&Arc<Entry>> = entries.iter().collect();
    loop {
        busy.retain(|entry| match entry.try_take() {
            Ok(txn) => {
                drop(txn);
                false
            }
            Err(()) => true,
        });
        if busy.is_empty() {
            return;
        }
        thread::sleep(Duration::from_millis(1));
    }
}
