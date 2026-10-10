//! The transaction coordinator: the two-phase commit state machine. This
//! module is a red zone.
//!
//! `store` stages records and makes nothing visible. `vote` takes the
//! per-OID locks in ascending oid order and detects conflicts before any
//! disk write. `finish` allocates the tid and writes the records, the
//! transaction row and the meta update in one backend write transaction,
//! durable before it returns. Dropping a transaction aborts it and
//! releases its locks.

use std::collections::BTreeMap;

use crate::backend::{TxnMeta, TxnStatus};
use crate::clock::timestamp;
use crate::error::{Result, ZoeError};
use crate::store::Store;
use crate::types::{Oid, Tid};

/// A record waiting for the commit.
struct Staged {
    /// The serial that the client read before it changed the object.
    serial: Tid,
    data: Vec<u8>,
}

/// One transaction of a store, from `tpc_begin` to `finish` or `abort`.
pub struct Txn<'s> {
    store: &'s Store,
    user: Vec<u8>,
    description: Vec<u8>,
    extension: Vec<u8>,
    staged: BTreeMap<Oid, Staged>,
    locked: Vec<Oid>,
    voted: bool,
}

impl<'s> Txn<'s> {
    pub(crate) fn new(store: &'s Store, user: &[u8], description: &[u8], extension: &[u8]) -> Self {
        Self {
            store,
            user: user.to_vec(),
            description: description.to_vec(),
            extension: extension.to_vec(),
            staged: BTreeMap::new(),
            locked: Vec::new(),
            voted: false,
        }
    }

    /// Stage a new version of `oid`. `serial` is the serial that the client
    /// read; it is `Tid::ZERO` for a new object.
    pub fn store(&mut self, oid: Oid, serial: Tid, data: &[u8]) -> Result<()> {
        if self.voted {
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

    /// Lock the staged oids and check that no other transaction changed
    /// them since their serial. A conflict releases the locks.
    pub fn vote(&mut self) -> Result<()> {
        if self.voted {
            return Err(ZoeError::Protocol("vote twice"));
        }
        let oids: Vec<Oid> = self.staged.keys().copied().collect();
        self.store.locks.acquire(&oids, self.store.lock_timeout)?;
        self.locked = oids;
        for (&oid, staged) in &self.staged {
            let current = self
                .store
                .backend
                .load_before(oid, Tid::MAX)?
                .map_or(Tid::ZERO, |record| record.tid);
            if current != staged.serial {
                self.release();
                return Err(ZoeError::Conflict { oid });
            }
        }
        self.voted = true;
        Ok(())
    }

    /// Commit the transaction and return its tid. The commit is durable
    /// when this returns.
    pub fn finish(mut self) -> Result<Tid> {
        if !self.voted {
            return Err(ZoeError::Protocol("finish before vote"));
        }
        let mut last = self.store.lock_sequencer();
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

        *last = tid;
        drop(last);
        self.release();
        Ok(tid)
    }

    /// Abort the transaction: drop the staged records and release the
    /// locks.
    pub fn abort(mut self) {
        self.release();
    }

    fn release(&mut self) {
        self.store.locks.release(&self.locked);
        self.locked.clear();
    }
}

impl Drop for Txn<'_> {
    fn drop(&mut self) {
        self.release();
    }
}
