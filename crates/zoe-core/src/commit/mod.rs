//! The transaction coordinator: the two-phase commit state machine.
//!
//! `store` stages records and makes nothing visible. `delete` stages a
//! deletion, a record with an empty pickle, with the serial check of
//! `store`. `check_current_serial` adds an oid to the read-current set.
//! `vote` takes the per-OID locks in ascending oid order over the write
//! and read-current sets together, then detects conflicts before any disk
//! write. `finish` allocates the tid and writes the records, the
//! transaction row and the meta update in one backend write transaction,
//! durable before it returns. `finish` and `abort` release the locks.
//! Dropping a transaction aborts it.
//!
//! The vote that takes the locks runs tier 1 of the conflict resolution
//! (`crate::merge`) on each stale written oid of a class of BTrees or
//! `Length`, and tier 2 on each stale written oid of a class that the
//! merge policy marks mergeable: it stores the merged record over the
//! committed one, or fails on a refusal, or leaves the conflict as it is.
//! `resolutions` tells what the tiers did. When the merge policy switches
//! on the split merge (ADR 0048, `split.rs`), the first conflict of the
//! vote also runs its rules on the nodes of BTrees: it stages the records
//! of the node, of the bucket and of the new bucket that the transaction
//! split, and lets a read-current node through. The vote after
//! `vote_resolving` compares each merge of shadow mode with the
//! resolution that the caller stored, and `verifications` tells what it
//! found.
//!
//! When the finish of a transaction starts, it hands the lock of each oid
//! that it writes, of a class that tier 1 merges, to the exclusive vote at
//! the head of the queue of the oid (ADR 0044). That vote merges onto the
//! staged record of the holder. Its commit comes after the commit of the
//! holder, in the same commit batch or a later one. When tier 1 does not
//! decide the conflict, the vote waits until the commit of the holder is
//! durable, within the lock timeout, and goes on as without the hand-off.
//!
//! `vote_resolving` lets the caller resolve conflicts with the locks held.
//! It returns the stale written oids with their committed records, and
//! keeps the locks. The caller stores a resolved pickle of each oid and
//! calls `vote` again. That vote takes no lock: it checks the serials
//! again with the locks that the first vote took.
//!
//! The thread that begins a transaction drives it: `store`, the blob
//! stores, `delete`, `restore`, the blob restores, `check_current_serial`,
//! `vote` and `vote_resolving` from another thread fail with
//! `WrongThread`. `finish` and `abort` work from any thread.
//!
//! `store_blob` stages a record with a synced blob file. `store_blob_at`
//! takes the blob file by its path in the blob temporary directory, by
//! rename. The blob stays with its oid until finish or abort: a later
//! `store` of the oid, the store of a resolved conflict too, replaces only
//! the pickle, as in ZODB. `restore_blob` and `restore_blob_at` are
//! `restore` with a blob. `finish` places the blob files before the backend
//! commit, and the same backend write transaction holds their blob records.
//! Dropping a transaction removes the blob files that it did not place.
//!
//! The staged records live in a staging buffer: in memory up to the
//! `staging_memory` option of the store, and in a spill file under
//! `staging/` beyond it. `staging` describes the buffer.
//!
//! `set_encoded` attaches to a staged record its payload, encoded on
//! another thread with `Encoded::encode`. The finish writes an attached
//! payload as it is, and encodes inline each record without one. The
//! frames are the same either way.
//!
//! `kv_set` and `kv_delete` stage a write of a KV namespace in the same
//! buffer. It takes no lock, and the vote checks nothing for it. The leader
//! of the commit batch writes it with the records of the transaction.
//!
//! `store` and `store_blob` check the size limits of the store before
//! they stage anything. A transaction logs its slow locks when it releases
//! them, and logs itself when it ends, if it was slow.

mod encoded;
mod finish;
mod group;
mod intent;
mod log;
mod observer;
mod order;
mod source;
mod split;
mod stage;
mod staging;
mod tier;
mod undo;
mod vote;

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Deref;
use std::sync::Arc;
use std::thread::{self, ThreadId};
use std::time::{Duration, SystemTime};

use crate::blob::PendingBlob;
use crate::error::{Result, ZoeError};
use crate::lock::{Handoff, Held, InFlight, LockCancel};
use crate::merge::{Resolution, Verification};
use crate::store::Store;
use crate::types::{Oid, Tid};
pub use encoded::Encoded;
use finish::next_tid;
pub(crate) use group::GroupCommit;
pub use intent::IntentLocks;
pub use observer::Observer;
use staging::Staging;
pub(crate) use staging::StagingArea;
use tier::undo_tiers;

/// A commit that `finish_after` made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Finished {
    /// The tid of the commit.
    pub tid: Tid,
    /// The tid of the commit just before this one in commit order. In a
    /// commit batch, it is the tid of the member before this one, or the
    /// last tid of the store before the batch.
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
    /// True for a restore transaction whose finish commits without a sync.
    deferred: bool,
    user: Vec<u8>,
    description: Vec<u8>,
    extension: Vec<u8>,
    /// The staged records. `Drop` removes their spill file while `store`
    /// is still there.
    staged: Staging,
    /// The oids that the transaction read as current, with the serial that
    /// it read.
    read_current: BTreeMap<Oid, Tid>,
    /// The locks that a vote took, with the time when it took each one.
    /// The transaction holds the locks until `finish`, `abort` or a failed
    /// vote.
    locks: Option<Vec<Held>>,
    /// The oids that `vote_resolving` gave as conflicts. The set is empty
    /// except between that call and the next vote.
    resolving: BTreeSet<Oid>,
    /// The stale read-current oids that rule 1 of the split merge let
    /// through in the last vote that took the locks. The vote after
    /// `vote_resolving` lets them through too: their shared locks are held.
    passed: BTreeSet<Oid>,
    /// The current serial and the read serial of the stale oid that failed
    /// the last vote.
    conflict: Option<(Tid, Tid)>,
    /// The sum of the lengths of the pickles that `store` and `store_blob`
    /// staged. `max-transaction-size` limits it.
    stored_bytes: u64,
    /// The time on the store clock when the transaction began.
    began: SystemTime,
    /// The tid of the commit, after a `finish` that succeeded.
    committed: Option<Tid>,
    /// The handle that ends the lock wait of a vote from another thread.
    lock_cancel: Option<LockCancel>,
    /// What tiers 1 and 2 did in the undos of the transaction, then in the
    /// last vote that took the locks.
    resolutions: Vec<Resolution>,
    /// The locks that the last vote that took the locks got with a
    /// hand-off, with their hand-offs.
    handed: BTreeMap<Oid, Arc<Handoff>>,
    /// The oids whose staged records tier 1 merged onto the record of a
    /// commit in flight, with that commit. The finish writes the tid of
    /// that commit as the previous serial of each.
    onto: BTreeMap<Oid, Arc<InFlight>>,
    /// The end of the lock timeout of the last vote that took the locks, on
    /// the monotonic time of the store clock.
    lock_deadline: Duration,
    /// The early write-intent locks that the transaction adopted before
    /// its vote, in ascending oid order. The vote counts them as its own.
    intents: Vec<Held>,
    /// The lock wait of the vote that takes the locks: the acquisition, and
    /// the waits for the durability of commits in flight. The vote gives
    /// it to the observer at its end. `None` without an observer.
    lock_waited: Option<Duration>,
    /// The number of entries of `resolutions` that the undos gave. They
    /// come first, and a vote keeps them.
    undo_resolutions: usize,
    /// The lowest tid that the transaction undoes. The vote checks it
    /// against the pack watermark with its locks held, and the finish
    /// again under the commit sequencer.
    undone: Option<Tid>,
    /// The merges of shadow mode of the last vote that took the locks: the
    /// digest of each merged record, by oid. The vote after
    /// `vote_resolving` compares them with the records of the caller.
    shadows: BTreeMap<Oid, Shadowed>,
    /// What the vote after `vote_resolving` found for each merge of
    /// shadow mode that the caller resolved.
    verifications: Vec<Verification>,
    /// True when the client takes records that it did not store (ADR
    /// 0049): rule 3 of the split merge can then write the new bucket of
    /// the other side.
    server_writes: bool,
    /// The stored oids that the split merge of the last vote that took the
    /// locks resolves without a record. A vote that passes takes them out
    /// of the staging.
    dropping: Vec<Oid>,
    /// The oids that the vote staged and that the client did not store.
    unstored: BTreeSet<Oid>,
}

/// A merge of shadow mode, kept for its check against the resolution of
/// the caller.
struct Shadowed {
    tier: u8,
    class: String,
    policy: Option<u64>,
    /// The BLAKE3 digest of the merged record.
    digest: blake3::Hash,
    /// The serial of the committed record.
    committed: Tid,
    /// The serial that the transaction read.
    read: Tid,
}

impl<S: Deref<Target = Store>> Txn<S> {
    /// Begin a transaction on `store`, driven by the calling thread.
    pub fn begin(store: S, user: &[u8], description: &[u8], extension: &[u8]) -> Self {
        let began = store.clock.now();
        Self {
            blobs: BTreeMap::new(),
            store,
            thread: thread::current().id(),
            tid: None,
            deferred: false,
            user: user.to_vec(),
            description: description.to_vec(),
            extension: extension.to_vec(),
            staged: Staging::default(),
            read_current: BTreeMap::new(),
            locks: None,
            resolving: BTreeSet::new(),
            passed: BTreeSet::new(),
            conflict: None,
            stored_bytes: 0,
            began,
            committed: None,
            lock_cancel: None,
            resolutions: Vec::new(),
            handed: BTreeMap::new(),
            onto: BTreeMap::new(),
            lock_deadline: Duration::ZERO,
            intents: Vec::new(),
            lock_waited: None,
            undo_resolutions: 0,
            undone: None,
            shadows: BTreeMap::new(),
            verifications: Vec::new(),
            server_writes: false,
            dropping: Vec::new(),
            unstored: BTreeSet::new(),
        }
    }

    /// Tell the vote whether the client of the transaction takes records
    /// that it did not store (ADR 0049): it ends their current entries in
    /// its cache, and gives every Connection of its process one
    /// invalidation with every oid of the commit before it publishes the
    /// tid. Only then can rule 3 of the split merge keep the new bucket of
    /// the other side when both sides split one bucket; without it the
    /// rule gives no answer there. The default is false.
    pub fn set_server_writes(&mut self, on: bool) {
        self.server_writes = on;
    }

    /// Tell if the last vote that took the locks and passed staged a record
    /// that the client did not store. The client then gives every
    /// Connection of its process every oid of the commit (ADR 0049).
    pub fn wrote_unstored(&self) -> bool {
        !self.unstored.is_empty()
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

    /// Begin a restore transaction on `store`, as `begin_restore` does,
    /// whose finish commits without a sync of the database file.
    ///
    /// The finish runs the same commit path: the tid check, the locks and
    /// the placement of the blobs with all their syncs. Only the backend
    /// commit has no sync. The commit is visible to readers and sets the
    /// last tid when `finish` returns. It becomes durable with the next
    /// durable commit: `Store::sync`, or a finish of another kind. A crash
    /// before it loses the commit, and the next writer open removes its
    /// blob files.
    pub fn begin_restore_deferred(
        store: S,
        tid: Tid,
        user: &[u8],
        description: &[u8],
        extension: &[u8],
    ) -> Result<Self> {
        let mut txn = Self::begin_restore(store, tid, user, description, extension)?;
        txn.deferred = true;
        Ok(txn)
    }

    /// Adopt the early write-intent locks of `intents` (ADR 0045): the vote
    /// counts them as its own, and the end of the transaction releases
    /// them. `intents` then holds no lock.
    ///
    /// The call must come before the vote, and `intents` must be an owner
    /// of the store of the transaction. Otherwise it fails with `Protocol`
    /// and changes nothing.
    pub fn adopt_intents<T: Deref<Target = Store>>(
        &mut self,
        intents: &mut IntentLocks<T>,
    ) -> Result<()> {
        if self.locked() {
            return Err(ZoeError::Protocol("adopt intent locks after the vote"));
        }
        if !std::ptr::eq(intents.store(), &*self.store) {
            return Err(ZoeError::Protocol("intent locks of another store"));
        }
        self.intents.extend(intents.take());
        self.intents.sort_by_key(|lock| lock.oid);
        Ok(())
    }

    /// Let `cancel` end the lock wait of the votes of this transaction from
    /// another thread. A vote that waits for a lock when `cancel` is
    /// cancelled, or that starts to wait after it, leaves the queue,
    /// releases each lock that it took and fails with `Conflict` on the oid
    /// that it waits for. `conflict_serials` then gives `None`.
    ///
    /// A cancel does nothing to a vote that takes its locks without a wait,
    /// or that has its locks: the transaction keeps them until `finish`,
    /// `abort` or a drop.
    pub fn set_lock_cancel(&mut self, cancel: LockCancel) {
        self.lock_cancel = Some(cancel);
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

    /// Return the user of the transaction, as `tpc_begin` gave it.
    pub fn user(&self) -> &[u8] {
        &self.user
    }

    /// Return the description of the transaction, as `tpc_begin` gave it.
    pub fn description(&self) -> &[u8] {
        &self.description
    }
}

impl Store {
    /// Release the locks `held`, then log each one held past `lock-warn`.
    /// The release reads no clock, so the locks are free before the log.
    pub(crate) fn release_locks(&self, held: &[Held]) {
        self.locks.release(held);
        self.log_released(held);
    }

    /// Log each lock of `held` held past `lock-warn`, once the lock manager
    /// released it.
    pub(crate) fn log_released(&self, held: &[Held]) {
        self.guardrails.locks_released(held, self.clock.now());
    }
}

/// The number of oids whose current serials the vote reads in one backend
/// read.
const READ_CHUNK: usize = 256;

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
mod tests;
