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
    /// The tid of a restore transaction. A normal transaction gets its tid
    /// at finish.
    tid: Option<Tid>,
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
            tid: None,
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

    /// Make this transaction a restore transaction that commits under
    /// `tid`.
    pub(crate) fn with_tid(mut self, tid: Tid) -> Self {
        self.tid = Some(tid);
        self
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

    /// Stage a copy of a version of `oid` in a restore transaction. `None`
    /// copies a deletion. The version gets the tid of the transaction.
    ///
    /// The previous serial of the version is the current serial of `oid` in
    /// this store. The vote takes an exclusive lock on `oid`, as for
    /// `store`. If another transaction changes `oid` before the vote, the
    /// vote fails with `Conflict`.
    pub fn restore(&mut self, oid: Oid, data: Option<&[u8]>) -> Result<()> {
        if self.tid.is_none() {
            return Err(ZoeError::Protocol("restore outside a restore transaction"));
        }
        if self.voted() {
            return Err(ZoeError::Protocol("restore after vote"));
        }
        let current = self.current_serial(oid)?;
        self.staged.insert(
            oid,
            Staged {
                serial: current,
                data: data.unwrap_or_default().to_vec(),
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
    /// A restore transaction commits under its own tid. If that tid is not
    /// after the last tid, `finish` fails with `Protocol`. A normal
    /// transaction gets a tid after the last tid, below `Tid::MAX`. When no
    /// such tid is left, `finish` fails with `Protocol`. The store does
    /// not halt for these errors.
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
        let last = sequencer.ok_or(ZoeError::Halted)?;
        let tid = match self.tid {
            Some(tid) => {
                check_restore_tid(tid, last)?;
                tid
            }
            None => next_tid(timestamp(self.store.clock.now()), last)?,
        };
        // The sequencer stays empty if the write below fails.
        *sequencer = None;
        if let Some(&oid) = self.staged.keys().next_back() {
            self.store.raise_next_oid(oid);
        }

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

/// Return the tid of a normal commit after the tid `last`: `now`, or the
/// tid after `last` if `now` is not after it. `Tid::MAX` is never given,
/// because `load` reads the versions before it. When no tid is left, fail
/// with `Protocol`.
fn next_tid(now: Tid, last: Tid) -> Result<Tid> {
    match last.0.checked_add(1) {
        Some(after) if after < Tid::MAX.0 => Ok(now.max(Tid(after)).min(Tid(Tid::MAX.0 - 1))),
        _ => Err(ZoeError::Protocol("no tid is left after the last tid")),
    }
}

/// Check that a restore transaction can commit under `tid` when `last` is
/// the last tid. A failed check is `Protocol`.
///
/// `Tid::MAX` is refused, because `load` reads the versions before it.
pub(crate) fn check_restore_tid(tid: Tid, last: Tid) -> Result<()> {
    if tid == Tid::MAX {
        return Err(ZoeError::Protocol("restore tid is the maximum tid"));
    }
    if tid <= last {
        return Err(ZoeError::Protocol("restore tid is not after the last tid"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::MemVfs;

    fn commit(store: &Store, oid: Oid, serial: Tid, data: &[u8]) -> Tid {
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.store(oid, serial, data).unwrap();
        txn.vote().unwrap();
        txn.finish().unwrap()
    }

    /// Begin a restore at `tid` that restores `data` for `oid`, and vote.
    fn voted_restore<'s>(store: &'s Store, tid: Tid, oid: Oid, data: &[u8]) -> Txn<'s> {
        let mut txn = store.tpc_begin_restore(tid, b"", b"", b"").unwrap();
        txn.restore(oid, Some(data)).unwrap();
        txn.vote().unwrap();
        txn
    }

    #[test]
    fn a_restore_over_an_existing_oid_replaces_it() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let oid = store.new_oid().unwrap();
        let first = commit(&store, oid, Tid::ZERO, b"a");
        let tid = Tid(first.0 + 10);
        voted_restore(&store, tid, oid, b"b").finish().unwrap();

        assert_eq!(store.load(oid).unwrap(), (b"b".to_vec(), tid));
        let restored = store.backend.load_before(oid, Tid::MAX).unwrap().unwrap();
        // The restored record points to the record that it replaced, and
        // that record stays readable.
        assert_eq!(restored.prev, first);
        let replaced = store.backend.load_before(oid, tid).unwrap().unwrap();
        assert_eq!((replaced.tid, replaced.data), (first, b"a".to_vec()));
    }

    #[test]
    fn a_restore_tid_behind_a_later_commit_fails_at_finish() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let early = voted_restore(&store, Tid(200), Oid(1), b"a");
        let late = voted_restore(&store, Tid(300), Oid(2), b"b");
        assert_eq!(late.finish().unwrap(), Tid(300));
        assert!(matches!(early.finish(), Err(ZoeError::Protocol(_))));
        assert!(store.locks.is_empty());

        // The store does not halt.
        assert_eq!(store.last_transaction(), Tid(300));
        voted_restore(&store, Tid(400), Oid(1), b"c")
            .finish()
            .unwrap();
        commit(&store, Oid(2), Tid(300), b"d");
    }

    #[test]
    fn a_normal_commit_never_gets_the_maximum_tid() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let oid = store.new_oid().unwrap();
        voted_restore(&store, Tid(u64::MAX - 2), oid, b"a")
            .finish()
            .unwrap();
        let tid = commit(&store, oid, Tid(u64::MAX - 2), b"b");
        assert_eq!(tid, Tid(u64::MAX - 1));
        assert_eq!(store.load(oid).unwrap(), (b"b".to_vec(), tid));
    }

    #[test]
    fn a_commit_after_a_restore_at_the_last_tid_fails() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let oid = store.new_oid().unwrap();
        let last = Tid(u64::MAX - 1);
        voted_restore(&store, last, oid, b"a").finish().unwrap();

        // No tid is left. The store refuses each finish and does not halt.
        for _ in 0..2 {
            let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
            txn.store(oid, last, b"b").unwrap();
            txn.vote().unwrap();
            assert!(matches!(txn.finish(), Err(ZoeError::Protocol(_))));
            assert!(store.locks.is_empty());
        }
        assert!(store.tpc_begin_restore(Tid::MAX, b"", b"", b"").is_err());
        assert_eq!(store.load(oid).unwrap(), (b"a".to_vec(), last));
        assert_eq!(store.last_transaction(), last);
    }

    #[test]
    fn the_next_tid_is_after_the_last_and_below_the_maximum() {
        let max = u64::MAX;
        for (now, last, tid) in [
            (10, 5, 10),
            (10, 10, 11),
            (10, 20, 21),
            (max, 5, max - 1),
            (max, max - 2, max - 1),
        ] {
            assert_eq!(
                next_tid(Tid(now), Tid(last)).unwrap(),
                Tid(tid),
                "{now} {last}"
            );
        }
        for last in [max - 1, max] {
            assert!(matches!(
                next_tid(Tid(0), Tid(last)),
                Err(ZoeError::Protocol(_))
            ));
        }
    }

    #[test]
    fn the_maximum_tid_is_refused() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let result = store.tpc_begin_restore(Tid::MAX, b"", b"", b"");
        assert!(matches!(result, Err(ZoeError::Protocol(_))));
    }

    #[test]
    fn restore_needs_a_restore_transaction_before_the_vote() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        assert!(matches!(
            txn.restore(Oid(1), Some(b"a")),
            Err(ZoeError::Protocol(_))
        ));

        let mut txn = voted_restore(&store, Tid(100), Oid(1), b"a");
        assert!(matches!(
            txn.restore(Oid(2), Some(b"b")),
            Err(ZoeError::Protocol(_))
        ));
    }

    #[test]
    fn a_change_between_restore_and_vote_is_a_conflict() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let oid = store.new_oid().unwrap();
        let mut txn = store
            .tpc_begin_restore(Tid(u64::MAX - 1), b"", b"", b"")
            .unwrap();
        txn.restore(oid, Some(b"a")).unwrap();
        commit(&store, oid, Tid::ZERO, b"b");
        assert!(matches!(txn.vote(), Err(ZoeError::Conflict { .. })));
        assert!(store.locks.is_empty());
    }

    #[test]
    fn new_oids_stay_above_a_restored_oid() {
        let vfs = MemVfs::new();
        {
            let store = Store::open_vfs(vfs.clone()).unwrap();
            voted_restore(&store, Tid(100), Oid(1000), b"a")
                .finish()
                .unwrap();
            assert!(store.new_oid().unwrap() > Oid(1000));
        }
        let store = Store::open_vfs(vfs).unwrap();
        assert!(store.new_oid().unwrap() > Oid(1000));
    }
}
