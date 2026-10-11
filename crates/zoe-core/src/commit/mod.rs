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
//!
//! `vote_resolving` lets the caller resolve conflicts with the locks held.
//! It returns the stale written oids with their committed records, and
//! keeps the locks. The caller stores a resolved pickle of each oid and
//! calls `vote` again. That vote takes no lock: it checks the serials
//! again with the locks that the first vote took.
//!
//! The thread that begins a transaction drives it: `store`, `store_blob`,
//! `restore`, `check_current_serial`, `vote` and `vote_resolving` from
//! another thread fail with `WrongThread`. `finish` and `abort` work from
//! any thread.
//!
//! `store_blob` stages a record with a synced blob file. The blob stays
//! with its oid until finish or abort: a later `store` of the oid, the
//! store of a resolved conflict too, replaces only the pickle, as in ZODB.
//! `finish` places the blob files before the backend commit, and the same
//! backend write transaction holds their blob records. Dropping a
//! transaction removes the blob files that it did not place.
//!
//! The staged records live in a staging buffer: in memory up to the
//! `staging_memory` option of the store, and in a spill file under
//! `staging/` beyond it. `staging` describes the buffer.

mod staging;

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Deref;
use std::thread::{self, ThreadId};

use crate::backend::{Record, TxnMeta, TxnStatus};
use crate::blob::{BlobFile, PendingBlob};
use crate::clock::timestamp;
use crate::error::{Result, ZoeError};
use crate::lock::Mode;
use crate::store::Store;
use crate::types::{Oid, Tid};
use staging::Staging;
pub(crate) use staging::StagingArea;

/// A commit that `finish_after` made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Finished {
    /// The tid of the commit.
    pub tid: Tid,
    /// The tid of the commit just before this one in commit order: the
    /// last tid of the store when this commit started to write.
    pub previous: Tid,
}

/// One transaction of a store, from `tpc_begin` to `finish` or `abort`.
///
/// `S` is how the transaction reaches its store: `&Store` borrows it, and
/// `Arc<Store>` shares it. A skin keeps a transaction with `Arc<Store>`
/// across calls, because it borrows nothing.
///
/// Dropping a transaction aborts it.
pub struct Txn<S: Deref<Target = Store>> {
    /// The blob files of the staged oids that have one.
    ///
    /// The fields drop in the order of their declaration. The blobs drop
    /// before `store`, so a transaction that holds the last share of its
    /// store still removes their temporary files.
    blobs: BTreeMap<Oid, PendingBlob>,
    store: S,
    /// The thread that began the transaction.
    thread: ThreadId,
    /// The tid of a restore transaction. A normal transaction gets its tid
    /// at finish.
    tid: Option<Tid>,
    user: Vec<u8>,
    description: Vec<u8>,
    extension: Vec<u8>,
    /// The staged records. `Drop` removes their spill file while `store`
    /// is still there.
    staged: Staging,
    /// The oids that the transaction read as current, with the serial that
    /// it read.
    read_current: BTreeMap<Oid, Tid>,
    /// The oids that a vote locked. The transaction holds the locks until
    /// `finish`, `abort` or a failed vote.
    locks: Option<Vec<Oid>>,
    /// The oids that `vote_resolving` gave as conflicts. The set is empty
    /// except between that call and the next vote.
    resolving: BTreeSet<Oid>,
    /// The current serial and the read serial of the stale oid that failed
    /// the last vote.
    conflict: Option<(Tid, Tid)>,
}

/// What the serial check of a vote found.
enum Check {
    /// No serial fails the vote. Each stale written oid can be resolved:
    /// the check gives it with its committed record.
    Passed(Vec<(Oid, Record)>),
    /// A stale serial fails the vote.
    Failed(Stale),
}

/// An oid whose current serial is not the serial that the transaction
/// read.
struct Stale {
    oid: Oid,
    current: Tid,
    read: Tid,
    /// True when the transaction read the oid as current and does not
    /// write it.
    read_only: bool,
}

impl<S: Deref<Target = Store>> Txn<S> {
    /// Begin a transaction on `store`, driven by the calling thread.
    pub fn begin(store: S, user: &[u8], description: &[u8], extension: &[u8]) -> Self {
        Self {
            blobs: BTreeMap::new(),
            store,
            thread: thread::current().id(),
            tid: None,
            user: user.to_vec(),
            description: description.to_vec(),
            extension: extension.to_vec(),
            staged: Staging::default(),
            read_current: BTreeMap::new(),
            locks: None,
            resolving: BTreeSet::new(),
            conflict: None,
        }
    }

    /// Begin a restore transaction on `store`: a copy of a transaction with
    /// its own tid. `finish` commits it under `tid` and allocates no tid.
    ///
    /// `tid` must be after the last tid of the store. Otherwise the call
    /// fails with `Protocol`. `finish` checks this again under the commit
    /// sequencer. A read-only store fails with `ReadOnly`.
    pub fn begin_restore(
        store: S,
        tid: Tid,
        user: &[u8],
        description: &[u8],
        extension: &[u8],
    ) -> Result<Self> {
        store.writable()?;
        check_restore_tid(tid, store.last_transaction())?;
        let mut txn = Self::begin(store, user, description, extension);
        txn.tid = Some(tid);
        Ok(txn)
    }

    /// Tell if a vote holds the locks of the transaction.
    fn locked(&self) -> bool {
        self.locks.is_some()
    }

    /// Tell if a vote succeeded. The transaction can then finish.
    fn voted(&self) -> bool {
        self.locked() && self.resolving.is_empty()
    }

    /// Refuse a call from a thread that did not begin the transaction.
    fn check_thread(&self) -> Result<()> {
        if thread::current().id() != self.thread {
            return Err(ZoeError::WrongThread);
        }
        Ok(())
    }

    /// Stage a new version of `oid`. `serial` is the serial that the client
    /// read; it is `Tid::ZERO` for a new object.
    ///
    /// An empty pickle marks a deletion, so `store` refuses it with
    /// `Protocol`. A restore transaction takes `store` too, with the same
    /// serial check at the vote.
    ///
    /// After a vote, `store` takes only an oid that `vote_resolving` gave
    /// as a conflict. Then `serial` is the serial of the committed record.
    ///
    /// A blob that `store_blob` staged for `oid` stays, as in ZODB.
    pub fn store(&mut self, oid: Oid, serial: Tid, data: &[u8]) -> Result<()> {
        self.check_store(oid, data)?;
        let area = &self.store.backend.staging;
        self.staged.put(area, oid, serial, data, false)
    }

    /// Check that `store` or `store_blob` can stage `data` for `oid`. A
    /// refused call changes nothing.
    fn check_store(&self, oid: Oid, data: &[u8]) -> Result<()> {
        self.check_thread()?;
        if self.voted() {
            return Err(ZoeError::Protocol("store after vote"));
        }
        if self.locked() && !self.resolving.contains(&oid) {
            return Err(ZoeError::Protocol(
                "store after vote of an oid without conflict",
            ));
        }
        if data.is_empty() {
            return Err(ZoeError::Protocol("store of an empty pickle"));
        }
        check_oid(oid)
    }

    /// Return the serial that the transaction read for `oid` and the pickle
    /// that it staged. Return `None` when the transaction did not stage
    /// `oid`.
    ///
    /// A pickle in the spill file is read from the file. A read that fails,
    /// or whose bytes do not match the hash of the pickle, fails the call
    /// with `Io`.
    pub fn staged(&self, oid: Oid) -> Result<Option<(Tid, Cow<'_, [u8]>)>> {
        self.staged.get(oid)
    }

    /// Return the bytes of the staged pickles that the transaction keeps in
    /// memory. The other staged pickles are in its spill file.
    pub fn staged_in_memory(&self) -> usize {
        self.staged.in_memory()
    }

    /// Stage a new version of `oid` with the blob in `file`, as `store`
    /// does, with the same checks. Then sync the file. `finish` renames it
    /// into the blob store. A second blob of `oid` replaces the first.
    ///
    /// `file` must come from `new_blob_file` of this store. A file of
    /// another store is refused with `Protocol`, and a file of a closed
    /// store with `Closed`. A refused call stages nothing and drops
    /// `file`, which removes it.
    pub fn store_blob(&mut self, oid: Oid, serial: Tid, data: &[u8], file: BlobFile) -> Result<()> {
        self.check_store(oid, data)?;
        let blob = file.close(&self.store.backend)?;
        let area = &self.store.backend.staging;
        self.staged.put(area, oid, serial, data, false)?;
        self.blobs.insert(oid, blob);
        Ok(())
    }

    /// Stage a copy of a version of `oid` in a restore transaction. `None`
    /// copies a deletion. The version gets the tid of the transaction.
    ///
    /// The previous serial of the version is the current serial of `oid` in
    /// this store. The vote takes an exclusive lock on `oid`, as for
    /// `store`. If another transaction changes `oid` before the vote, the
    /// vote fails with `Conflict`.
    ///
    /// A blob that `store_blob` staged for `oid` stays, unless `data` is a
    /// deletion: a deleted object has no blob.
    pub fn restore(&mut self, oid: Oid, data: Option<&[u8]>) -> Result<()> {
        self.check_thread()?;
        if self.tid.is_none() {
            return Err(ZoeError::Protocol("restore outside a restore transaction"));
        }
        if self.locked() {
            return Err(ZoeError::Protocol("restore after vote"));
        }
        check_oid(oid)?;
        let current = self.current_serial(oid)?;
        let area = &self.store.backend.staging;
        self.staged
            .put(area, oid, current, data.unwrap_or_default(), true)?;
        if data.is_none() {
            self.blobs.remove(&oid);
        }
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
        self.check_thread()?;
        if self.locked() {
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
    /// A failed vote releases every lock that it took. On a read-only store
    /// the vote fails with `ReadOnly` and takes no lock.
    ///
    /// After `vote_resolving` gave conflicts, `vote` takes no lock. It
    /// checks every serial again with the locks that `vote_resolving` took.
    /// An oid that is still stale fails the vote, and the vote releases the
    /// locks.
    pub fn vote(&mut self) -> Result<()> {
        self.check_thread()?;
        if self.voted() {
            return Err(ZoeError::Protocol("vote twice"));
        }
        self.conflict = None;
        if !self.locked() {
            self.lock()?;
        }
        self.resolving.clear();
        self.check(false).map(drop)
    }

    /// Vote as `vote` does, but keep the locks when the vote finds stale
    /// written oids that the caller can resolve. Return each such oid with
    /// its committed record. An empty list means that the vote succeeded.
    ///
    /// The caller can resolve a stale written oid when `store` staged it
    /// with a serial other than `Tid::ZERO`, and its committed record holds
    /// a pickle. Any other stale written oid fails the vote with
    /// `Conflict`. Then a stale read-current oid fails the vote with
    /// `ReadConflict`: a read-current oid is never resolved. A failed vote
    /// releases every lock that it took.
    ///
    /// After a list of conflicts, the transaction keeps its locks and is
    /// not voted. For each oid of the list, the caller stores the resolved
    /// pickle with the serial of the committed record. Then it calls
    /// `vote`. A conflict that the caller does not resolve stays stale, so
    /// that vote fails and releases the locks.
    pub fn vote_resolving(&mut self) -> Result<Vec<(Oid, Record)>> {
        self.check_thread()?;
        if self.locked() {
            return Err(ZoeError::Protocol("vote twice"));
        }
        self.conflict = None;
        self.lock()?;
        let conflicts = self.check(true)?;
        self.resolving = conflicts.iter().map(|&(oid, _)| oid).collect();
        Ok(conflicts)
    }

    /// Take the locks of the vote, and keep them until `finish`, `abort` or
    /// a failed vote. A lock timeout releases the locks that the call took.
    /// A read-only store fails with `ReadOnly` and gives no lock.
    fn lock(&mut self) -> Result<()> {
        self.store.writable()?;
        let store: &Store = &self.store;
        let locks = store
            .locks
            .acquire(&self.lock_requests(), store.lock_timeout)?;
        self.locks = Some(locks.keep());
        Ok(())
    }

    /// Check the serials with the locks held. Return the stale written oids
    /// that the caller can resolve, with their committed records. When
    /// `resolve` is false, the caller can resolve none.
    ///
    /// A stale serial that the caller cannot resolve, or an error, fails
    /// the check and releases the locks.
    fn check(&mut self, resolve: bool) -> Result<Vec<(Oid, Record)>> {
        let error = match self.check_serials(resolve) {
            Ok(Check::Passed(conflicts)) => return Ok(conflicts),
            Ok(Check::Failed(stale)) => {
                self.conflict = Some((stale.current, stale.read));
                let oid = stale.oid;
                if stale.read_only {
                    ZoeError::ReadConflict { oid }
                } else {
                    ZoeError::Conflict { oid }
                }
            }
            Err(error) => error,
        };
        self.release();
        self.resolving.clear();
        Err(error)
    }

    /// Return the serials of the stale oid that failed the last vote: the
    /// current serial of the oid, then the serial that this transaction
    /// read. These are the `serials` of a ZODB `ConflictError`.
    ///
    /// Return `None` when the last vote did not fail on a stale serial, for
    /// example when it waited past the lock timeout.
    pub fn conflict_serials(&self) -> Option<(Tid, Tid)> {
        self.conflict
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
        requests.extend(self.staged.iter().map(|(oid, _)| (oid, Mode::Exclusive)));
        requests.into_iter().collect()
    }

    /// Compare the serial that the client read of each oid with its current
    /// serial. The written oids come first, then the read-current oids.
    ///
    /// The first stale oid fails the check, unless `resolve` is true and
    /// the oid is a written oid that the caller can resolve. The check
    /// gives each such oid with its committed record.
    fn check_serials(&self, resolve: bool) -> Result<Check> {
        let mut conflicts = Vec::new();
        for (oid, staged) in self.staged.iter() {
            let record = self.store.backend.load_before(oid, Tid::MAX)?;
            let current = record.as_ref().map_or(Tid::ZERO, |record| record.tid);
            if current == staged.serial {
                continue;
            }
            match record {
                Some(record)
                    if resolve
                        && !staged.restored
                        && staged.serial != Tid::ZERO
                        && !record.data.is_empty() =>
                {
                    conflicts.push((oid, record));
                }
                _ => {
                    return Ok(Check::Failed(Stale {
                        oid,
                        current,
                        read: staged.serial,
                        read_only: false,
                    }));
                }
            }
        }
        for (&oid, &read) in &self.read_current {
            let current = self.current_serial(oid)?;
            if current != read {
                return Ok(Check::Failed(Stale {
                    oid,
                    current,
                    read,
                    read_only: true,
                }));
            }
        }
        Ok(Check::Passed(conflicts))
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
    pub fn finish(self) -> Result<Tid> {
        self.finish_after().map(|finished| finished.tid)
    }

    /// Commit the transaction as `finish` does, and return its tid and the
    /// tid of the commit before it.
    ///
    /// Each commit names the commit before it, so a skin can run its
    /// post-commit work in commit order, without a lock across the
    /// commits.
    pub fn finish_after(mut self) -> Result<Finished> {
        if !self.voted() {
            return Err(ZoeError::Protocol("finish before vote"));
        }
        let store: &Store = &self.store;
        let mut sequencer = store.lock_sequencer();
        let last = sequencer.ok_or(ZoeError::Halted)?;
        let tid = match self.tid {
            Some(tid) => {
                check_restore_tid(tid, last)?;
                tid
            }
            None => next_tid(timestamp(store.clock.now()), last)?,
        };
        // The sequencer stays empty if the write below fails.
        *sequencer = None;
        if let Some(oid) = self.staged.last_oid() {
            store.raise_next_oid(oid);
        }

        // The blob files are durable in their place before the backend
        // commit, so a blob record never lacks its file.
        let intent = store.blobs().place(tid, &mut self.blobs)?;
        let mut write = store.backend.begin_write()?;
        // A spilled pickle is read back here, one at a time.
        for record in self.staged.records() {
            let (oid, serial, data) = record?;
            write.put_record(oid, tid, serial, &data)?;
            if let Some(blob) = self.blobs.get(&oid) {
                write.put_blob(oid, tid, blob.record())?;
            }
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
        store.blobs().finished(intent);
        // A running pack marks what these records refer to before its next
        // removal.
        store.packing.note(tid, self.staged.keys().copied());

        *sequencer = Some(tid);
        drop(sequencer);
        self.release();
        Ok(Finished {
            tid,
            previous: last,
        })
    }

    /// Abort the transaction: drop the staged records, remove the spill
    /// file and release the locks.
    pub fn abort(mut self) {
        self.release();
    }

    fn release(&mut self) {
        if let Some(oids) = self.locks.take() {
            self.store.locks.release(&oids);
        }
    }
}

impl<S: Deref<Target = Store>> Drop for Txn<S> {
    fn drop(&mut self) {
        self.release();
        self.staged.remove_spill(&self.store.backend.staging);
    }
}

/// Check that a transaction can write `oid`. `Oid(u64::MAX)` is refused
/// with `Protocol`, because the next oid must stay greater than every
/// written oid.
fn check_oid(oid: Oid) -> Result<()> {
    if oid == Oid(u64::MAX) {
        return Err(ZoeError::Protocol("oid u64::MAX is reserved"));
    }
    Ok(())
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
    use std::io::{Read, Write};
    use std::sync::Arc;
    use std::time::Duration;

    use redb::StorageBackend;

    use super::*;
    use crate::clock::SystemClock;
    use crate::store::Options;
    use crate::vfs::{MemVfs, Vfs};

    fn commit(store: &Store, oid: Oid, serial: Tid, data: &[u8]) -> Tid {
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.store(oid, serial, data).unwrap();
        txn.vote().unwrap();
        txn.finish().unwrap()
    }

    /// Begin a restore at `tid` that restores `data` for `oid`, and vote.
    fn voted_restore<'s>(store: &'s Store, tid: Tid, oid: Oid, data: &[u8]) -> Txn<&'s Store> {
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
    fn store_refuses_an_empty_pickle() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        for mut txn in [
            store.tpc_begin(b"", b"", b"").unwrap(),
            store.tpc_begin_restore(Tid(100), b"", b"", b"").unwrap(),
        ] {
            let error = txn.store(Oid(1), Tid::ZERO, b"");
            assert!(matches!(error, Err(ZoeError::Protocol(_))));
        }
    }

    #[test]
    fn store_in_a_restore_transaction_checks_the_serial() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let mut txn = store.tpc_begin_restore(Tid(100), b"", b"", b"").unwrap();
        txn.store(Oid(1), Tid::ZERO, b"a").unwrap();
        txn.restore(Oid(2), Some(b"b")).unwrap();
        txn.vote().unwrap();
        assert_eq!(txn.finish().unwrap(), Tid(100));
        assert_eq!(store.load(Oid(1)).unwrap(), (b"a".to_vec(), Tid(100)));

        let mut stale = store.tpc_begin_restore(Tid(200), b"", b"", b"").unwrap();
        stale.store(Oid(1), Tid::ZERO, b"c").unwrap();
        assert!(matches!(stale.vote(), Err(ZoeError::Conflict { .. })));
        assert!(store.locks.is_empty());
    }

    #[test]
    fn the_maximum_oid_is_refused() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        let error = txn.store(Oid(u64::MAX), Tid::ZERO, b"a");
        assert!(matches!(error, Err(ZoeError::Protocol(_))));
        let mut txn = store.tpc_begin_restore(Tid(100), b"", b"", b"").unwrap();
        let error = txn.restore(Oid(u64::MAX), Some(b"a"));
        assert!(matches!(error, Err(ZoeError::Protocol(_))));

        // The highest oid that a commit can write uses up the oids.
        voted_restore(&store, Tid(100), Oid(u64::MAX - 1), b"a")
            .finish()
            .unwrap();
        assert!(matches!(store.new_oid(), Err(ZoeError::Protocol(_))));
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
    fn a_restored_oid_takes_an_exclusive_lock_at_the_vote() {
        let mut store = Store::open_vfs(MemVfs::new()).unwrap();
        // A zero lock timeout turns every wait into a conflict.
        store.lock_timeout = Duration::ZERO;
        let oid = Oid(1);
        let first = voted_restore(&store, Tid(100), oid, b"a");

        // Neither a store nor a second restore on the same oid gets the
        // lock while the first restore holds it.
        let mut stored = store.tpc_begin(b"", b"", b"").unwrap();
        stored.store(oid, Tid::ZERO, b"b").unwrap();
        let mut restored = store.tpc_begin_restore(Tid(200), b"", b"", b"").unwrap();
        restored.restore(oid, Some(b"c")).unwrap();
        for txn in [&mut stored, &mut restored] {
            let error = txn.vote();
            assert!(
                matches!(error, Err(ZoeError::Conflict { oid: Oid(1) })),
                "{error:?}"
            );
        }
        first.finish().unwrap();
        assert!(store.locks.is_empty());

        // The second restore read the serial before the first one finished,
        // so it cannot commit on top of it.
        assert!(matches!(restored.vote(), Err(ZoeError::Conflict { .. })));
        assert_eq!(store.load(oid).unwrap(), (b"a".to_vec(), Tid(100)));
        assert!(store.locks.is_empty());
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

    #[test]
    fn another_thread_cannot_store_check_restore_or_vote() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let (mine, theirs) = (store.new_oid().unwrap(), store.new_oid().unwrap());
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.store(mine, Tid::ZERO, b"a").unwrap();
        let mut restore = store.tpc_begin_restore(Tid(100), b"", b"", b"").unwrap();
        thread::scope(|scope| {
            scope.spawn(|| {
                let refused = [
                    txn.store(theirs, Tid::ZERO, b"b"),
                    txn.check_current_serial(theirs, Tid(5)),
                    txn.vote(),
                    txn.vote_resolving().map(drop),
                    restore.restore(theirs, Some(b"c")),
                ];
                for result in refused {
                    assert!(matches!(result, Err(ZoeError::WrongThread)), "{result:?}");
                }
            });
        });
        assert!(store.lock_table_is_empty());

        // The refused calls changed nothing: the stale read check of the
        // other thread does not fail the vote, and only `mine` commits.
        txn.vote().unwrap();
        let tid = txn.finish().unwrap();
        assert_eq!(store.load(mine).unwrap(), (b"a".to_vec(), tid));
        assert!(matches!(store.load(theirs), Err(ZoeError::NotFound { .. })));
        restore.abort();
    }

    #[test]
    fn another_thread_can_finish_or_abort() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let oid = store.new_oid().unwrap();
        let mut aborted = store.tpc_begin(b"", b"", b"").unwrap();
        aborted.store(oid, Tid::ZERO, b"a").unwrap();
        aborted.vote().unwrap();
        thread::scope(|scope| {
            scope.spawn(move || aborted.abort());
        });
        assert!(store.lock_table_is_empty());

        let mut finished = store.tpc_begin(b"", b"", b"").unwrap();
        finished.store(oid, Tid::ZERO, b"b").unwrap();
        finished.vote().unwrap();
        let tid = thread::scope(|scope| scope.spawn(move || finished.finish()).join())
            .unwrap()
            .unwrap();
        assert_eq!(store.load(oid).unwrap(), (b"b".to_vec(), tid));
        assert!(store.lock_table_is_empty());
    }

    #[test]
    fn a_stale_vote_gives_the_current_and_the_read_serial() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let (read, written) = (store.new_oid().unwrap(), store.new_oid().unwrap());
        let first = commit(&store, read, Tid::ZERO, b"a");
        let second = commit(&store, read, first, b"b");

        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.check_current_serial(read, first).unwrap();
        txn.store(written, Tid::ZERO, b"c").unwrap();
        assert!(matches!(txn.vote(), Err(ZoeError::ReadConflict { oid }) if oid == read));
        assert_eq!(txn.conflict_serials(), Some((second, first)));

        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.store(read, first, b"c").unwrap();
        assert!(matches!(txn.vote(), Err(ZoeError::Conflict { oid }) if oid == read));
        assert_eq!(txn.conflict_serials(), Some((second, first)));
        let staged = txn.staged(read).unwrap();
        assert_eq!(staged, Some((first, Cow::Borrowed(&b"c"[..]))));
        assert_eq!(txn.staged(written).unwrap(), None);

        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.store(read, second, b"c").unwrap();
        txn.vote().unwrap();
        assert_eq!(txn.conflict_serials(), None);
    }

    #[test]
    fn a_lock_timeout_gives_no_serials() {
        let mut store = Store::open_vfs(MemVfs::new()).unwrap();
        store.lock_timeout = Duration::ZERO;
        let oid = store.new_oid().unwrap();
        let mut holder = store.tpc_begin(b"", b"", b"").unwrap();
        holder.store(oid, Tid::ZERO, b"a").unwrap();
        holder.vote().unwrap();
        let mut waiter = store.tpc_begin(b"", b"", b"").unwrap();
        waiter.store(oid, Tid::ZERO, b"b").unwrap();
        assert!(matches!(waiter.vote(), Err(ZoeError::Conflict { .. })));
        assert_eq!(waiter.conflict_serials(), None);
    }

    #[test]
    fn each_commit_names_the_commit_before_it() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let oid = store.new_oid().unwrap();
        let first = commit(&store, oid, Tid::ZERO, b"a");
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.store(oid, first, b"b").unwrap();
        txn.vote().unwrap();
        let finished = txn.finish_after().unwrap();
        assert_eq!(finished.previous, first);
        assert!(finished.tid > first);
        assert_eq!(store.last_transaction(), finished.tid);
    }

    #[test]
    fn a_transaction_that_shares_the_store_borrows_nothing() {
        let store = Arc::new(Store::open_vfs(MemVfs::new()).unwrap());
        let oid = store.new_oid().unwrap();
        let mut txn = Txn::begin(Arc::clone(&store), b"user", b"", b"");
        txn.store(oid, Tid::ZERO, b"a").unwrap();
        txn.vote().unwrap();
        assert!(!store.lock_table_is_empty());
        drop(txn);
        assert!(store.lock_table_is_empty());

        let mut txn = Txn::begin(Arc::clone(&store), b"user", b"", b"");
        txn.store(oid, Tid::ZERO, b"b").unwrap();
        txn.vote().unwrap();
        let tid = txn.finish().unwrap();
        assert_eq!(store.load(oid).unwrap(), (b"b".to_vec(), tid));
        assert!(store.lock_table_is_empty());
        assert_eq!(Arc::strong_count(&store), 1);
    }

    /// Commit the records of `writes` in one transaction: each is an oid,
    /// the serial that the transaction read and a pickle.
    fn commit_all(store: &Store, writes: &[(Oid, Tid, &[u8])]) -> Tid {
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        for &(oid, serial, data) in writes {
            txn.store(oid, serial, data).unwrap();
        }
        txn.vote().unwrap();
        txn.finish().unwrap()
    }

    #[test]
    fn a_resolving_vote_gives_the_committed_record_and_keeps_the_locks() {
        let mut store = Store::open_vfs(MemVfs::new()).unwrap();
        // A zero lock timeout turns every wait into a conflict.
        store.lock_timeout = Duration::ZERO;
        let (stale, fresh) = (store.new_oid().unwrap(), store.new_oid().unwrap());
        let first = commit(&store, stale, Tid::ZERO, b"a");
        let second = commit(&store, stale, first, b"b");

        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.store(stale, first, b"c").unwrap();
        txn.store(fresh, Tid::ZERO, b"d").unwrap();
        let conflicts = txn.vote_resolving().unwrap();
        let committed = Record {
            tid: second,
            prev: first,
            data: b"b".to_vec(),
        };
        assert_eq!(conflicts, [(stale, committed)]);
        let staged = txn.staged(stale).unwrap();
        assert_eq!(staged, Some((first, Cow::Borrowed(&b"c"[..]))));
        assert_eq!(txn.conflict_serials(), None);

        // The transaction keeps the lock: another vote on the oid cannot
        // take it.
        let mut other = store.tpc_begin(b"", b"", b"").unwrap();
        other.store(stale, second, b"e").unwrap();
        assert!(matches!(other.vote(), Err(ZoeError::Conflict { .. })));
        assert_eq!(other.conflict_serials(), None);

        // The transaction stores the resolved pickle over the committed
        // serial, and votes again.
        txn.store(stale, second, b"bc").unwrap();
        txn.vote().unwrap();
        let tid = txn.finish().unwrap();
        assert_eq!(store.load(stale).unwrap(), (b"bc".to_vec(), tid));
        assert_eq!(store.load(fresh).unwrap(), (b"d".to_vec(), tid));
        assert!(store.lock_table_is_empty());
    }

    #[test]
    fn a_resolving_vote_without_conflicts_is_a_vote() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let oid = store.new_oid().unwrap();
        let first = commit(&store, oid, Tid::ZERO, b"a");
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.store(oid, first, b"b").unwrap();
        assert!(txn.vote_resolving().unwrap().is_empty());
        assert!(matches!(
            txn.store(oid, first, b"c"),
            Err(ZoeError::Protocol(_))
        ));
        assert!(matches!(txn.vote(), Err(ZoeError::Protocol(_))));
        let tid = txn.finish().unwrap();
        assert_eq!(store.load(oid).unwrap(), (b"b".to_vec(), tid));
        assert!(store.lock_table_is_empty());
    }

    #[test]
    fn a_conflict_left_unresolved_fails_the_second_vote() {
        let mut store = Store::open_vfs(MemVfs::new()).unwrap();
        store.lock_timeout = Duration::ZERO;
        let (low, high) = (store.new_oid().unwrap(), store.new_oid().unwrap());
        let first = commit_all(&store, &[(low, Tid::ZERO, b"a"), (high, Tid::ZERO, b"a")]);
        let second = commit_all(&store, &[(low, first, b"b"), (high, first, b"b")]);

        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.store(low, first, b"c").unwrap();
        txn.store(high, first, b"c").unwrap();
        let conflicts = txn.vote_resolving().unwrap();
        let oids: Vec<_> = conflicts.iter().map(|&(oid, _)| oid).collect();
        assert_eq!(oids, [low, high]);

        // The transaction resolves only `low`.
        txn.store(low, second, b"bc").unwrap();
        let error = txn.vote();
        assert!(
            matches!(error, Err(ZoeError::Conflict { oid }) if oid == high),
            "{error:?}"
        );
        assert_eq!(txn.conflict_serials(), Some((second, first)));
        assert!(store.lock_table_is_empty());

        // The failed vote released the locks, so another transaction
        // commits at once.
        commit(&store, high, second, b"d");
        txn.abort();
    }

    #[test]
    fn a_read_current_conflict_is_never_resolved() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let (written, read) = (store.new_oid().unwrap(), store.new_oid().unwrap());
        let first = commit_all(
            &store,
            &[(written, Tid::ZERO, b"a"), (read, Tid::ZERO, b"a")],
        );
        let second = commit_all(&store, &[(written, first, b"b"), (read, first, b"b")]);

        // A stale read-current oid fails the vote, even when the stale
        // written oid can be resolved.
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.store(written, first, b"c").unwrap();
        txn.check_current_serial(read, first).unwrap();
        let error = txn.vote_resolving();
        assert!(
            matches!(error, Err(ZoeError::ReadConflict { oid }) if oid == read),
            "{error:?}"
        );
        assert_eq!(txn.conflict_serials(), Some((second, first)));
        assert!(store.lock_table_is_empty());

        // A read-current oid alone fails the same way.
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.store(written, second, b"c").unwrap();
        txn.check_current_serial(read, first).unwrap();
        assert!(matches!(
            txn.vote_resolving(),
            Err(ZoeError::ReadConflict { .. })
        ));
        assert!(store.lock_table_is_empty());
    }

    #[test]
    fn a_conflict_without_an_old_and_a_committed_state_cannot_be_resolved() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let (created, deleted, restored, missing) = (Oid(1), Oid(2), Oid(3), Oid(4));
        let first = commit_all(
            &store,
            &[
                (created, Tid::ZERO, b"a"),
                (deleted, Tid::ZERO, b"a"),
                (restored, Tid::ZERO, b"a"),
            ],
        );
        let mut delete = store
            .tpc_begin_restore(Tid(first.0 + 1), b"", b"", b"")
            .unwrap();
        delete.restore(deleted, None).unwrap();
        delete.vote().unwrap();
        let gone = delete.finish().unwrap();

        // The transaction read no serial of `created`, `deleted` has no
        // committed pickle and `missing` has no record. A restored record
        // is a copy.
        let mut copy = store
            .tpc_begin_restore(Tid(u64::MAX - 1), b"", b"", b"")
            .unwrap();
        copy.restore(restored, Some(b"b")).unwrap();
        let changed = commit(&store, restored, first, b"c");
        for (oid, read, current, txn) in [
            (created, Tid::ZERO, first, None),
            (deleted, first, gone, None),
            (missing, first, Tid::ZERO, None),
            (restored, first, changed, Some(copy)),
        ] {
            let mut txn = txn.unwrap_or_else(|| {
                let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
                txn.store(oid, read, b"d").unwrap();
                txn
            });
            let error = txn.vote_resolving();
            assert!(
                matches!(error, Err(ZoeError::Conflict { oid: failed }) if failed == oid),
                "{oid:?} {error:?}"
            );
            assert_eq!(txn.conflict_serials(), Some((current, read)), "{oid:?}");
            assert!(store.lock_table_is_empty());
        }
    }

    #[test]
    fn calls_between_the_two_votes() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let (stale, other) = (store.new_oid().unwrap(), store.new_oid().unwrap());
        let first = commit(&store, stale, Tid::ZERO, b"a");
        let second = commit(&store, stale, first, b"b");
        let resolving = || {
            let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
            txn.store(stale, first, b"c").unwrap();
            assert_eq!(txn.vote_resolving().unwrap().len(), 1);
            txn
        };

        // Between the votes, the transaction takes only a store of an oid
        // in conflict.
        let mut txn = resolving();
        for refused in [
            txn.store(other, Tid::ZERO, b"d"),
            txn.check_current_serial(other, Tid::ZERO),
            txn.vote_resolving().map(drop),
        ] {
            assert!(matches!(refused, Err(ZoeError::Protocol(_))), "{refused:?}");
        }
        txn.store(stale, second, b"bc").unwrap();
        txn.vote().unwrap();
        assert!(matches!(
            txn.store(stale, second, b"bcd"),
            Err(ZoeError::Protocol(_))
        ));
        txn.abort();
        assert!(store.lock_table_is_empty());

        // The transaction cannot finish before the second vote. The failed
        // finish releases the locks.
        let txn = resolving();
        assert!(matches!(txn.finish(), Err(ZoeError::Protocol(_))));
        assert!(store.lock_table_is_empty());
    }

    #[test]
    fn a_lock_timeout_in_a_resolving_vote_gives_no_serials() {
        let mut store = Store::open_vfs(MemVfs::new()).unwrap();
        store.lock_timeout = Duration::ZERO;
        let oid = store.new_oid().unwrap();
        let first = commit(&store, oid, Tid::ZERO, b"a");
        let mut holder = store.tpc_begin(b"", b"", b"").unwrap();
        holder.store(oid, first, b"b").unwrap();
        holder.vote().unwrap();
        let mut waiter = store.tpc_begin(b"", b"", b"").unwrap();
        waiter.store(oid, Tid::ZERO, b"c").unwrap();
        assert!(matches!(
            waiter.vote_resolving(),
            Err(ZoeError::Conflict { .. })
        ));
        assert_eq!(waiter.conflict_serials(), None);
        holder.abort();
        assert!(store.lock_table_is_empty());
    }

    #[test]
    fn a_waiting_vote_gets_the_lock_after_the_resolved_commit() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let oid = store.new_oid().unwrap();
        let first = commit(&store, oid, Tid::ZERO, b"a");
        let second = commit(&store, oid, first, b"b");
        let mut resolving = store.tpc_begin(b"", b"", b"").unwrap();
        resolving.store(oid, first, b"c").unwrap();
        assert_eq!(resolving.vote_resolving().unwrap().len(), 1);

        let (voted, done) = std::sync::mpsc::channel();
        thread::scope(|scope| {
            scope.spawn(|| {
                let mut later = store.tpc_begin(b"", b"", b"").unwrap();
                later.store(oid, second, b"d").unwrap();
                voted
                    .send(later.vote_resolving().map(|conflicts| conflicts.len()))
                    .unwrap();
            });
            // The later vote waits while the first transaction resolves.
            assert!(done.recv_timeout(Duration::from_millis(100)).is_err());
            resolving.store(oid, second, b"bc").unwrap();
            resolving.vote().unwrap();
            resolving.finish().unwrap();
            // Then the later vote gets the lock and sees the resolved
            // commit as a conflict.
            assert_eq!(done.recv().unwrap().unwrap(), 1);
        });
        assert!(store.lock_table_is_empty());
    }

    /// Open a store in a new filesystem. Its transactions keep `memory`
    /// bytes of pickles in memory and spill the rest.
    fn spilling_store(memory: usize) -> (MemVfs, Store) {
        let vfs = MemVfs::new();
        let options = Options {
            staging_memory: memory,
            ..Options::default()
        };
        let store = Store::open_full(vfs.clone(), SystemClock, options).unwrap();
        (vfs, store)
    }

    /// Tell if `vfs` holds a spill file.
    fn spills(vfs: &MemVfs) -> bool {
        vfs.names().iter().any(|name| name.starts_with("staging/"))
    }

    #[test]
    fn staged_reads_a_pickle_in_memory_or_in_the_spill_file() {
        let (vfs, store) = spilling_store(1);
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.store(Oid(1), Tid::ZERO, b"a").unwrap();
        txn.store(Oid(2), Tid(5), b"bb").unwrap();
        assert_eq!(txn.staged_in_memory(), 1);
        assert!(spills(&vfs));

        let (serial, data) = txn.staged(Oid(1)).unwrap().unwrap();
        assert!(matches!(data, Cow::Borrowed(b"a")));
        assert_eq!(serial, Tid::ZERO);
        let (serial, data) = txn.staged(Oid(2)).unwrap().unwrap();
        assert_eq!((serial, &*data), (Tid(5), &b"bb"[..]));
        assert!(txn.staged(Oid(3)).unwrap().is_none());
    }

    #[test]
    fn the_vote_checks_the_serials_of_spilled_records() {
        let (vfs, store) = spilling_store(0);
        let oid = store.new_oid().unwrap();
        let first = commit(&store, oid, Tid::ZERO, b"a");
        let mut stale = store.tpc_begin(b"", b"", b"").unwrap();
        stale.store(oid, Tid::ZERO, b"b").unwrap();
        assert!(matches!(stale.vote(), Err(ZoeError::Conflict { .. })));
        assert!(store.locks.is_empty());
        drop(stale);
        assert!(!spills(&vfs));

        let tid = commit(&store, oid, first, b"c");
        assert_eq!(store.load(oid).unwrap(), (b"c".to_vec(), tid));
    }

    #[test]
    fn a_restore_transaction_commits_spilled_records() {
        let (vfs, store) = spilling_store(0);
        let first = commit(&store, Oid(1), Tid::ZERO, b"a");
        let tid = Tid(first.0 + 10);
        let mut txn = store.tpc_begin_restore(tid, b"", b"", b"").unwrap();
        txn.restore(Oid(1), None).unwrap();
        txn.restore(Oid(2), Some(b"b")).unwrap();
        txn.store(Oid(3), Tid::ZERO, b"c").unwrap();
        assert_eq!(txn.staged_in_memory(), 0);
        assert!(spills(&vfs));
        txn.vote().unwrap();
        assert_eq!(txn.finish().unwrap(), tid);

        assert!(!spills(&vfs));
        assert!(matches!(store.load(Oid(1)), Err(ZoeError::NotFound { .. })));
        assert_eq!(store.load(Oid(2)).unwrap(), (b"b".to_vec(), tid));
        assert_eq!(store.load(Oid(3)).unwrap(), (b"c".to_vec(), tid));
        assert!(store.new_oid().unwrap() > Oid(3));
    }

    /// A blob stays with its oid when the pickle spills, and when a later
    /// `store` of the oid spills a new pickle.
    #[test]
    fn a_spilled_record_keeps_its_blob() {
        let (vfs, store) = spilling_store(0);
        let (twice, once) = (store.new_oid().unwrap(), store.new_oid().unwrap());
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        for (oid, bytes) in [(twice, &b"twice"[..]), (once, b"once")] {
            let mut file = store.new_blob_file().unwrap();
            file.write_all(bytes).unwrap();
            txn.store_blob(oid, Tid::ZERO, b"first", file).unwrap();
        }
        txn.store(twice, Tid::ZERO, b"second").unwrap();
        assert!(spills(&vfs));
        txn.vote().unwrap();
        let tid = txn.finish().unwrap();

        assert!(!spills(&vfs));
        assert_eq!(store.load(twice).unwrap(), (b"second".to_vec(), tid));
        assert_eq!(store.load(once).unwrap(), (b"first".to_vec(), tid));
        for (oid, bytes) in [(twice, &b"twice"[..]), (once, b"once")] {
            let mut read = Vec::new();
            store
                .open_blob(oid, tid)
                .unwrap()
                .read_to_end(&mut read)
                .unwrap();
            assert_eq!(read, bytes);
        }
    }

    /// A conflict on a spilled record resolves as one in memory: `staged`
    /// reads the pickle from the spill file, and the resolved pickle
    /// replaces it.
    #[test]
    fn a_spilled_record_can_be_resolved() {
        let (vfs, store) = spilling_store(0);
        let oid = store.new_oid().unwrap();
        let first = commit(&store, oid, Tid::ZERO, b"a");
        let second = commit(&store, oid, first, b"b");

        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.store(oid, first, b"c").unwrap();
        assert!(spills(&vfs));
        let conflicts = txn.vote_resolving().unwrap();
        assert_eq!(conflicts.len(), 1);
        let (serial, data) = txn.staged(oid).unwrap().unwrap();
        assert!(matches!(data, Cow::Owned(_)));
        assert_eq!((serial, &*data), (first, &b"c"[..]));

        txn.store(oid, second, b"bc").unwrap();
        txn.vote().unwrap();
        let tid = txn.finish().unwrap();
        assert_eq!(store.load(oid).unwrap(), (b"bc".to_vec(), tid));
        assert!(!spills(&vfs));
    }

    #[test]
    fn dropping_a_transaction_removes_its_spill_file() {
        let (vfs, store) = spilling_store(0);
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.store(Oid(1), Tid::ZERO, b"a").unwrap();
        txn.vote().unwrap();
        assert!(spills(&vfs));
        drop(txn);
        assert!(!spills(&vfs));
        assert!(store.locks.is_empty());
    }

    /// A transaction that holds the last share of its store still removes
    /// its spill file when it drops.
    #[test]
    fn the_last_share_of_the_store_removes_the_spill_file() {
        let (vfs, store) = spilling_store(0);
        let store = Arc::new(store);
        let mut txn = Txn::begin(Arc::clone(&store), b"", b"", b"");
        drop(store);
        txn.store(Oid(1), Tid::ZERO, b"a").unwrap();
        assert!(spills(&vfs));
        drop(txn);
        assert!(!spills(&vfs));
    }

    /// A finish writes only the bytes that the transaction staged. A
    /// spill file that changed fails the finish before the commit.
    #[test]
    fn a_damaged_spill_file_fails_the_finish() {
        let (vfs, store) = spilling_store(0);
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.store(Oid(1), Tid::ZERO, b"abc").unwrap();
        txn.vote().unwrap();
        let name = &Vfs::list_files(&vfs, "staging").unwrap()[0];
        vfs.file(&format!("staging/{name}")).write(1, b"x").unwrap();

        let error = txn.finish();
        assert!(matches!(error, Err(ZoeError::Io(_))), "{error:?}");
        assert!(!spills(&vfs));
        assert!(store.locks.is_empty());
        assert!(matches!(store.load(Oid(1)), Err(ZoeError::NotFound { .. })));
        assert_eq!(store.last_transaction(), Tid::ZERO);

        // The error came after the finish took the sequencer, so the store
        // halts until it is reopened.
        let mut next = store.tpc_begin(b"", b"", b"").unwrap();
        next.store(Oid(2), Tid::ZERO, b"b").unwrap();
        next.vote().unwrap();
        assert!(matches!(next.finish(), Err(ZoeError::Halted)));
    }

    /// A spill file that changed while the caller resolves a conflict fails
    /// `staged` with the error of the read, never with `None`. The
    /// transaction keeps its locks until the caller aborts it.
    #[test]
    fn a_damaged_spill_file_fails_staged_during_resolution() {
        let (vfs, store) = spilling_store(0);
        let oid = store.new_oid().unwrap();
        let first = commit(&store, oid, Tid::ZERO, b"a");
        commit(&store, oid, first, b"b");

        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.store(oid, first, b"c").unwrap();
        assert_eq!(txn.vote_resolving().unwrap().len(), 1);
        let name = &Vfs::list_files(&vfs, "staging").unwrap()[0];
        vfs.file(&format!("staging/{name}")).write(0, b"x").unwrap();

        let error = txn.staged(oid).unwrap_err();
        assert!(
            matches!(&error, ZoeError::Io(e) if e.kind() == std::io::ErrorKind::InvalidData),
            "{error:?}"
        );
        assert!(!store.lock_table_is_empty());
        txn.abort();
        assert!(store.lock_table_is_empty());
        assert!(!spills(&vfs));
    }

    /// Transactions on many threads spill at once, and some of them drop on
    /// another thread. Each commit keeps its pickles, and no spill file
    /// stays. Under ThreadSanitizer, this covers the spill paths.
    #[test]
    fn transactions_on_many_threads_spill_at_once() {
        const THREADS: usize = 8;
        const ROUNDS: usize = 6;
        const OIDS: usize = 3;
        fn pickle(oid: Oid) -> Vec<u8> {
            format!("pickle {}", oid.0).into_bytes()
        }

        // About one pickle of each transaction fits in memory.
        let (vfs, store) = spilling_store(12);
        let store = Arc::new(store);
        let (sender, receiver) = std::sync::mpsc::channel::<Txn<Arc<Store>>>();
        let dropper = thread::spawn(move || receiver.into_iter().for_each(drop));
        let workers: Vec<_> = (0..THREADS)
            .map(|_| {
                let (store, sender) = (Arc::clone(&store), sender.clone());
                thread::spawn(move || {
                    let mut committed = Vec::new();
                    for round in 0..ROUNDS {
                        let mut txn = Txn::begin(Arc::clone(&store), b"", b"", b"");
                        let oids: Vec<Oid> = (0..OIDS).map(|_| store.new_oid().unwrap()).collect();
                        for &oid in &oids {
                            txn.store(oid, Tid::ZERO, &pickle(oid)).unwrap();
                        }
                        assert!(txn.staged_in_memory() <= 12);
                        if round % 2 == 1 {
                            sender.send(txn).unwrap();
                            continue;
                        }
                        txn.vote().unwrap();
                        let tid = txn.finish().unwrap();
                        committed.extend(oids.into_iter().map(|oid| (oid, tid)));
                    }
                    committed
                })
            })
            .collect();
        drop(sender);
        let committed: Vec<_> = workers
            .into_iter()
            .flat_map(|worker| worker.join().unwrap())
            .collect();
        dropper.join().unwrap();

        assert_eq!(committed.len(), THREADS * ROUNDS / 2 * OIDS);
        for (oid, tid) in committed {
            assert_eq!(store.load(oid).unwrap(), (pickle(oid), tid));
        }
        assert!(!spills(&vfs));
        assert!(store.lock_table_is_empty());
    }
}
