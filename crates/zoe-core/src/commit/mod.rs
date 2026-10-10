//! The transaction coordinator: the two-phase commit state machine.
//!
//! `store` stages records and makes nothing visible.
//! `check_current_serial` adds an oid to the read-current set. `vote` takes
//! the per-OID locks in ascending oid order over the write and read-current
//! sets together, then detects conflicts before any disk write. `finish`
//! allocates the tid and writes the records, the transaction row and the
//! meta update in one backend write transaction, durable before it returns.
//! `finish` and `abort` release the locks. Dropping a transaction aborts
//! it.

use std::collections::BTreeMap;

use crate::backend::{TxnMeta, TxnStatus};
use crate::clock::timestamp;
use crate::error::{Result, ZoeError};
use crate::lock::{Locks, Mode};
use crate::store::Store;
use crate::types::{Oid, Tid};

/// A record waiting for the commit.
struct Staged {
    /// The serial that the client read before it changed the object.
    serial: Tid,
    data: Vec<u8>,
}

/// One transaction of a store, from `tpc_begin` to `finish` or `abort`.
///
/// Dropping a transaction aborts it.
pub struct Txn<'s> {
    store: &'s Store,
    user: Vec<u8>,
    description: Vec<u8>,
    extension: Vec<u8>,
    staged: BTreeMap<Oid, Staged>,
    /// The oids that the transaction read as current, with the serial that
    /// it read.
    read_current: BTreeMap<Oid, Tid>,
    /// The locks of a successful vote. The transaction holds them until
    /// `finish` or `abort`.
    locks: Option<Locks<'s>>,
}

impl<'s> Txn<'s> {
    pub(crate) fn new(store: &'s Store, user: &[u8], description: &[u8], extension: &[u8]) -> Self {
        Self {
            store,
            user: user.to_vec(),
            description: description.to_vec(),
            extension: extension.to_vec(),
            staged: BTreeMap::new(),
            read_current: BTreeMap::new(),
            locks: None,
        }
    }

    fn voted(&self) -> bool {
        self.locks.is_some()
    }

    /// Stage a new version of `oid`. `serial` is the serial that the client
    /// read; it is `Tid::ZERO` for a new object.
    pub fn store(&mut self, oid: Oid, serial: Tid, data: &[u8]) -> Result<()> {
        if self.voted() {
            return Err(ZoeError::Protocol("store after vote"));
        }
        self.staged.insert(
            oid,
            Staged {
                serial,
                data: data.to_vec(),
            },
        );
        Ok(())
    }

    /// Declare that the transaction read `oid` at `serial` and depends on
    /// it. This is `checkCurrentSerialInTransaction` of ZODB.
    ///
    /// The vote takes a shared lock on `oid` and checks that `serial` is
    /// still current. A stale serial fails the vote with `ReadConflict`.
    /// The lock stays until `finish` or `abort`, so no other transaction
    /// can change `oid` before this one finishes.
    pub fn check_current_serial(&mut self, oid: Oid, serial: Tid) -> Result<()> {
        if self.voted() {
            return Err(ZoeError::Protocol("check_current_serial after vote"));
        }
        self.read_current.insert(oid, serial);
        Ok(())
    }

    /// Lock the written and the read-current oids, then check that no
    /// other transaction changed them since their serial.
    ///
    /// The vote waits for a lock that another transaction holds, at most
    /// the lock timeout of the store. A longer wait fails with `Conflict`.
    /// A failed vote releases every lock that it took.
    pub fn vote(&mut self) -> Result<()> {
        if self.voted() {
            return Err(ZoeError::Protocol("vote twice"));
        }
        let locks = self
            .store
            .locks
            .acquire(&self.lock_requests(), self.store.lock_timeout)?;
        // An error drops `locks`, and that releases them.
        self.check_serials()?;
        self.locks = Some(locks);
        Ok(())
    }

    /// Return the lock requests of the vote in ascending oid order: an
    /// exclusive lock for each written oid, and a shared lock for each other
    /// read-current oid.
    fn lock_requests(&self) -> Vec<(Oid, Mode)> {
        let mut requests: BTreeMap<Oid, Mode> = self
            .read_current
            .keys()
            .map(|&oid| (oid, Mode::Shared))
            .collect();
        requests.extend(self.staged.keys().map(|&oid| (oid, Mode::Exclusive)));
        requests.into_iter().collect()
    }

    /// Check that the current serial of each written oid and of each
    /// read-current oid is the serial that the client read.
    fn check_serials(&self) -> Result<()> {
        for (&oid, staged) in &self.staged {
            if self.current_serial(oid)? != staged.serial {
                return Err(ZoeError::Conflict { oid });
            }
        }
        for (&oid, &serial) in &self.read_current {
            if self.current_serial(oid)? != serial {
                return Err(ZoeError::ReadConflict { oid });
            }
        }
        Ok(())
    }

    /// Return the serial of the current record of `oid`, or `Tid::ZERO`
    /// when `oid` has no record.
    fn current_serial(&self, oid: Oid) -> Result<Tid> {
        Ok(self
            .store
            .backend
            .load_before(oid, Tid::MAX)?
            .map_or(Tid::ZERO, |record| record.tid))
    }

    /// Commit the transaction and return its tid. The commit is durable
    /// when this returns.
    ///
    /// An error other than `Protocol` and `Halted` leaves the outcome of
    /// the commit unknown until the store is reopened: the commit can be
    /// on disk or not. After such an error, the store refuses every later
    /// finish with `Halted`. Reopen the store to read the outcome and to
    /// commit again.
    pub fn finish(mut self) -> Result<Tid> {
        if !self.voted() {
            return Err(ZoeError::Protocol("finish before vote"));
        }
        let mut sequencer = self.store.lock_sequencer();
        // The sequencer stays empty if the write below fails.
        let last = sequencer.take().ok_or(ZoeError::Halted)?;
        let tid = timestamp(self.store.clock.now()).max(Tid(last.0 + 1));

        let mut write = self.store.backend.begin_write()?;
        for (&oid, staged) in &self.staged {
            write.put_record(oid, tid, staged.serial, &staged.data)?;
        }
        write.put_txn(&TxnMeta {
            tid,
            user: std::mem::take(&mut self.user),
            description: std::mem::take(&mut self.description),
            extension: std::mem::take(&mut self.extension),
            oid_count: self.staged.len() as u64,
            status: TxnStatus::Committed,
        })?;
        write.commit()?;

        *sequencer = Some(tid);
        drop(sequencer);
        self.release();
        Ok(tid)
    }

    /// Abort the transaction: drop the staged records and release the
    /// locks.
    pub fn abort(mut self) {
        self.release();
    }

    fn release(&mut self) {
        self.locks = None;
    }
}
