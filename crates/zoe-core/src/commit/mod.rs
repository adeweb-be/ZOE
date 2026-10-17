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
//! `resolutions` tells what the tiers did. The vote after
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
mod group;
mod intent;
mod observer;
mod staging;
mod undo;

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Deref;
use std::path::Path;
use std::sync::Arc;
use std::thread::{self, ThreadId};
use std::time::{Duration, SystemTime};

use crate::backend::Record;
use crate::blob::{BlobFile, PendingBlob};
use crate::error::{Result, ZoeError};
use crate::format;
use crate::guard::{Ended, SizeLimit};
use crate::kv::{self, Flavour};
use crate::lock::{Handoff, Held, InFlight, LockCancel, Mode, Refused, Waited};
use crate::merge::policy::{Policy, Rules};
use crate::merge::{self, Merge, Outcome, Resolution, Resolved, Verification};
use crate::store::Store;
use crate::types::{Oid, Tid};
pub use encoded::Encoded;
pub(crate) use group::GroupCommit;
use group::Member;
pub use intent::IntentLocks;
pub use observer::Observer;
use staging::Staging;
pub(crate) use staging::StagingArea;

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

/// What the serial check of a vote found.
enum Check {
    /// No serial fails the vote. Each stale written oid can be resolved:
    /// the check gives it with its committed record, or tier 1 merged it.
    Passed {
        /// The stale written oids that the caller resolves.
        conflicts: Vec<(Oid, Record)>,
        /// The records that tier 1 merged: the oid, the serial of the
        /// committed record and the merged pickle.
        merged: Vec<(Oid, Tid, Vec<u8>)>,
        /// The merged oids whose committed record is the record of a
        /// commit in flight, with that commit.
        onto: Vec<(Oid, Arc<InFlight>)>,
        /// The transaction size with the merged pickles.
        stored_bytes: u64,
    },
    /// A stale serial fails the vote.
    Failed(Stale),
}

/// An oid whose current serial is not the serial that the transaction
/// read.
struct Stale {
    oid: Oid,
    /// The current serial. It is `None` when the current record is the
    /// record of a commit in flight that has no tid yet.
    current: Option<Tid>,
    read: Tid,
    /// True when the transaction read the oid as current and does not
    /// write it.
    read_only: bool,
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
    ///
    /// A pickle longer than `max-record-size`, or a pickle that takes the
    /// transaction past `max-transaction-size`, fails with `TooLarge`. A
    /// restore transaction checks no size limit.
    pub fn store(&mut self, oid: Oid, serial: Tid, data: &[u8]) -> Result<()> {
        self.check_store(oid, data)?;
        let stored_bytes = self.check_size(oid, data)?;
        let area = &self.store.backend.staging;
        self.staged.put(area, oid, serial, data, false)?;
        self.stored_bytes = stored_bytes;
        Ok(())
    }

    /// Stage a deletion of `oid`: a record with an empty pickle. `serial`
    /// is the serial that the client read. This is `deleteObject` of ZODB.
    ///
    /// The vote checks `serial` as it does for `store`: a stale serial
    /// fails the vote with `Conflict`. `vote_resolving` never resolves a
    /// deletion. A restore transaction takes `delete` too, with the same
    /// check.
    ///
    /// The deletion replaces a pickle that the transaction staged for
    /// `oid`, and drops the staged blob of `oid`: a deleted object has no
    /// blob. After a vote, `delete` fails with `Protocol`.
    pub fn delete(&mut self, oid: Oid, serial: Tid) -> Result<()> {
        self.check_thread()?;
        if self.locked() {
            return Err(ZoeError::Protocol("delete after vote"));
        }
        check_oid(oid)?;
        let stored_bytes = self.check_size(oid, &[])?;
        let area = &self.store.backend.staging;
        self.staged.put(area, oid, serial, &[], false)?;
        self.stored_bytes = stored_bytes;
        self.blobs.remove(&oid);
        Ok(())
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

    /// Check `data` for `oid` against `max-record-size` and
    /// `max-transaction-size`. Return the new value of `stored_bytes`,
    /// with `data` in it. A refused call changes nothing.
    ///
    /// The callers check the order of the call first, so a call out of
    /// order is `Protocol`, whatever its size.
    fn check_size(&self, oid: Oid, data: &[u8]) -> Result<u64> {
        if self.is_restore() {
            return Ok(self.stored_bytes);
        }
        let guardrails = &self.store.guardrails;
        let size = data.len() as u64;
        guardrails.check(SizeLimit::Record, oid, size)?;
        // A pickle that replaces a staged pickle of the same oid counts
        // once. The check reads the length of the staged pickle, never its
        // bytes, so a spilled pickle costs no read.
        let staged = self
            .staged
            .record(oid)
            .map_or(0, |staged| staged.pickle_len() as u64);
        let stored_bytes = self
            .stored_bytes
            .saturating_sub(staged)
            .saturating_add(size);
        guardrails.check(SizeLimit::Transaction, oid, stored_bytes)?;
        Ok(stored_bytes)
    }

    /// Tell if this is a restore transaction. A restore transaction copies
    /// records as they are, so it checks no size limit: a copy never
    /// refuses what its source took.
    fn is_restore(&self) -> bool {
        self.tid.is_some()
    }

    /// Stage a write of `value` for `key` in the namespace `name` of
    /// `flavour`. An ephemeral key needs `ttl`, more than 0 and at most
    /// `MAX_TTL_SECONDS`: the finish sets its expiry to the time of the
    /// write of its commit batch plus `ttl`. A durable key takes no TTL.
    ///
    /// The write takes no lock, and the vote checks nothing for it: when
    /// two transactions write one key, the commit with the later tid wins.
    /// A later write of the same key in this transaction replaces this
    /// one. An abort drops it.
    ///
    /// The value counts toward `max-record-size` and
    /// `max-transaction-size` as a pickle does: a refusal is `KvTooLarge`.
    /// A bad name, key or TTL is `InvalidKv`. A call from another thread is
    /// `WrongThread`. A call after the vote, or in a restore transaction,
    /// is `Protocol`. A refused call stages nothing.
    pub fn kv_set(
        &mut self,
        flavour: Flavour,
        name: &str,
        key: &[u8],
        value: &[u8],
        ttl: Option<Duration>,
    ) -> Result<()> {
        self.check_kv(name, key)?;
        let ttl_ms = kv::ttl_millis(flavour, ttl)?;
        let row = format::kv_key(name.as_bytes(), key);
        let stored_bytes = self.check_kv_size(flavour, &row, value.len())?;
        let area = &self.store.backend.staging;
        self.staged
            .put_kv(area, flavour, row, Some(value), ttl_ms)?;
        self.stored_bytes = stored_bytes;
        Ok(())
    }

    /// Stage a delete of `key` in the namespace `name` of `flavour`, with
    /// the rules of `kv_set`. The finish removes the row. A delete of a key
    /// that has no row does nothing.
    pub fn kv_delete(&mut self, flavour: Flavour, name: &str, key: &[u8]) -> Result<()> {
        self.check_kv(name, key)?;
        let row = format::kv_key(name.as_bytes(), key);
        let stored_bytes = self.check_kv_size(flavour, &row, 0)?;
        let area = &self.store.backend.staging;
        self.staged.put_kv(area, flavour, row, None, 0)?;
        self.stored_bytes = stored_bytes;
        Ok(())
    }

    /// Check that the transaction can stage a namespace write of `key` in
    /// the namespace `name`. A refused call changes nothing.
    fn check_kv(&self, name: &str, key: &[u8]) -> Result<()> {
        self.check_thread()?;
        if self.is_restore() {
            return Err(ZoeError::Protocol(
                "a namespace write in a restore transaction",
            ));
        }
        if self.locked() {
            return Err(ZoeError::Protocol("a namespace write after vote"));
        }
        kv::check_name(name)?;
        kv::check_key(key)
    }

    /// Check a value of `size` bytes for the row `row` of `flavour` against
    /// `max-record-size` and `max-transaction-size`, as `check_size` does
    /// for a pickle. The key of the row counts toward the transaction size
    /// with the value, and a write that replaces a staged write of the same
    /// row counts once. Return the new value of `stored_bytes`.
    fn check_kv_size(&self, flavour: Flavour, row: &[u8], size: usize) -> Result<u64> {
        let guardrails = &self.store.guardrails;
        guardrails.check_kv(SizeLimit::Record, size as u64)?;
        let size = (row.len() + size) as u64;
        let staged = self
            .staged
            .kv_record(flavour, row)
            .map_or(0, |staged| (row.len() + staged.value_len()) as u64);
        let stored_bytes = self
            .stored_bytes
            .saturating_sub(staged)
            .saturating_add(size);
        guardrails.check_kv(SizeLimit::Transaction, stored_bytes)?;
        Ok(stored_bytes)
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

    /// Return each oid that the transaction staged, in ascending oid order.
    /// This reads no pickle.
    pub fn staged_oids(&self) -> impl Iterator<Item = Oid> + '_ {
        self.staged.iter().map(|(oid, _)| oid)
    }

    /// Return the class of the pickle that the transaction staged for
    /// `oid`, as `module.name`. `zoe_pickle` reads it from the class
    /// pickle and never unpickles. This is the `class_name` of a ZODB
    /// `ConflictError`.
    ///
    /// Return `None` when the transaction did not stage `oid` or staged a
    /// deletion, when the class pickle does not decode or names no class,
    /// and when a spilled pickle does not read.
    pub fn staged_class_name(&self, oid: Oid) -> Option<String> {
        let (_, pickle) = self.staged(oid).ok()??;
        if pickle.is_empty() {
            return None;
        }
        let (module, name) = zoe_pickle::decode_class_name(&pickle).ok()??;
        Some(format!("{module}.{name}"))
    }

    /// Attach `encoded` to the staged record of `oid`, so the finish writes
    /// it and does not encode the pickle. Return true when it is attached.
    ///
    /// The call attaches `encoded` only when it encodes the staged pickle
    /// of `oid` with the compression of the store: the length, the xxh3-64
    /// hash and the compression must be the same. Else it returns false,
    /// and the finish encodes the pickle inline. So the frame is always the
    /// frame of an inline encode. A later `store`, `delete` or `restore` of
    /// `oid` drops the payload.
    ///
    /// The call computes the hash of a pickle in memory. A call from
    /// another thread fails with `WrongThread`.
    pub fn set_encoded(&mut self, oid: Oid, encoded: Encoded) -> Result<bool> {
        self.check_thread()?;
        if encoded.compression != self.store.compression() {
            return Ok(false);
        }
        Ok(self.staged.attach(oid, encoded))
    }

    /// Tell if the transaction keeps the staged pickle of `oid` in memory.
    /// It is false for an oid that is not staged, and for a pickle in the
    /// spill file.
    pub fn stages_in_memory(&self, oid: Oid) -> bool {
        self.staged.in_memory_of(oid)
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
    /// store with `Closed`. Then a blob longer than `max-blob-size` fails
    /// with `TooLarge`, before the sync, as a pickle over a size limit
    /// does. A restore transaction checks no size limit. A refused call
    /// stages nothing and drops `file`, which removes it.
    pub fn store_blob(&mut self, oid: Oid, serial: Tid, data: &[u8], file: BlobFile) -> Result<()> {
        self.check_store(oid, data)?;
        file.check_owner(&self.store.backend)?;
        let stored_bytes = self.check_size(oid, data)?;
        if !self.is_restore() {
            self.store
                .guardrails
                .check(SizeLimit::Blob, oid, file.size())?;
        }
        let blob = file.close(&self.store.backend)?;
        self.stage_blob(oid, serial, data, false, stored_bytes, blob)
    }

    /// Stage a new version of `oid` with the blob in the file at `path`, as
    /// `store_blob` does, with the same checks. The file must be in the
    /// blob temporary directory of this store, `Store::blob_temp_dir`, or
    /// in one of its subdirectories.
    ///
    /// The call takes the file by rename, never by copy: it syncs the file,
    /// renames it to a temporary name of the store and computes its BLAKE3
    /// hash. `finish` renames it into the blob store. A second blob of
    /// `oid` replaces the first.
    ///
    /// A refused call leaves the file in its place: a call from another
    /// thread, after the vote, with an empty pickle or a reserved oid, or
    /// with a path outside the blob temporary directory. A file that the
    /// transaction took belongs to it: an error after the rename, an abort
    /// or a drop of the transaction removes the file. So a blob longer
    /// than `max-blob-size`, which the call can measure only after it took
    /// the file, fails with `TooLarge` and removes the file.
    pub fn store_blob_at(&mut self, oid: Oid, serial: Tid, data: &[u8], path: &Path) -> Result<()> {
        self.check_store(oid, data)?;
        let stored_bytes = self.check_size(oid, data)?;
        let blob = self.store.take_blob_file(path)?;
        if !self.is_restore() {
            self.store
                .guardrails
                .check(SizeLimit::Blob, oid, blob.record().size)?;
        }
        self.stage_blob(oid, serial, data, false, stored_bytes, blob)
    }

    /// Stage a new version of `oid` with the blob in the file `name` of the
    /// directory of `oid` in the blob directory, as `store_blob_at` does,
    /// with the same checks. This is the `storeBlobShared` of ZEO: a client
    /// that shares the blob directory writes the file there, and gives its
    /// name.
    ///
    /// `name` is a name, not a path: it has no `/`, and it ends in `.tmp` or
    /// `.tmp-`. Another name is refused with `Protocol`. The call syncs the
    /// levels of the oid directory and the file, then takes the file by
    /// rename, as `store_blob_at` does. A refused call leaves the file in
    /// its place, and a file that the transaction took belongs to it.
    pub fn store_blob_shared(
        &mut self,
        oid: Oid,
        serial: Tid,
        data: &[u8],
        name: &str,
    ) -> Result<()> {
        self.check_store(oid, data)?;
        let stored_bytes = self.check_size(oid, data)?;
        self.store.writable()?;
        let blob =
            self.store
                .blobs()
                .take_shared(Arc::downgrade(&self.store.backend), oid, name)?;
        if !self.is_restore() {
            self.store
                .guardrails
                .check(SizeLimit::Blob, oid, blob.record().size)?;
        }
        self.stage_blob(oid, serial, data, false, stored_bytes, blob)
    }

    /// Stage `data` for `oid` with `serial`, and the blob `blob`.
    /// `stored_bytes` is the size of the staged pickles with `data`, as
    /// `check_size` gives it. A stage that fails drops `blob`, which
    /// removes its file.
    fn stage_blob(
        &mut self,
        oid: Oid,
        serial: Tid,
        data: &[u8],
        restored: bool,
        stored_bytes: u64,
        blob: PendingBlob,
    ) -> Result<()> {
        let area = &self.store.backend.staging;
        self.staged.put(area, oid, serial, data, restored)?;
        self.stored_bytes = stored_bytes;
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
    ///
    /// A restore copies a record as it is: it checks no size limit.
    pub fn restore(&mut self, oid: Oid, data: Option<&[u8]>) -> Result<()> {
        let current = self.check_restore(oid)?;
        let area = &self.store.backend.staging;
        self.staged
            .put(area, oid, current, data.unwrap_or_default(), true)?;
        if data.is_none() {
            self.blobs.remove(&oid);
        }
        Ok(())
    }

    /// Stage a copy of a version of `oid` with the blob in `file`, in a
    /// restore transaction. This is `restore` with a blob, the
    /// `restoreBlob` of ZODB. Then sync the file. `finish` renames it into
    /// the blob store.
    ///
    /// As for `restore`, the version gets the tid of the transaction, and
    /// its previous serial is the current serial of `oid` in this store. A
    /// change of `oid` by another transaction before the vote fails the
    /// vote with `Conflict`, and `vote_resolving` never resolves it. So
    /// `restore_blob` differs from `store_blob` in a restore transaction:
    /// `store_blob` takes the serial of the client and checks it at the
    /// vote.
    ///
    /// `data` cannot be empty, because a deletion has no blob: an empty
    /// pickle is refused with `Protocol`. `file` must come from
    /// `new_blob_file` of this store, as for `store_blob`. A refused call
    /// stages nothing and drops `file`, which removes it.
    pub fn restore_blob(&mut self, oid: Oid, data: &[u8], file: BlobFile) -> Result<()> {
        let current = self.check_restore_blob(oid, data)?;
        let blob = file.close(&self.store.backend)?;
        self.stage_blob(oid, current, data, true, self.stored_bytes, blob)
    }

    /// Stage a copy of a version of `oid` with the blob in the file at
    /// `path`, as `restore_blob` does, with the same checks. The call takes
    /// the file by rename, as `store_blob_at` does, and a refused call
    /// leaves the file in its place.
    pub fn restore_blob_at(&mut self, oid: Oid, data: &[u8], path: &Path) -> Result<()> {
        let current = self.check_restore_blob(oid, data)?;
        let blob = self.store.take_blob_file(path)?;
        self.stage_blob(oid, current, data, true, self.stored_bytes, blob)
    }

    /// Check that `restore` can stage a copy of `oid`, and return the
    /// current serial of `oid`. A refused call changes nothing.
    fn check_restore(&self, oid: Oid) -> Result<Tid> {
        self.check_thread()?;
        if self.tid.is_none() {
            return Err(ZoeError::Protocol("restore outside a restore transaction"));
        }
        if self.locked() {
            return Err(ZoeError::Protocol("restore after vote"));
        }
        check_oid(oid)?;
        self.current_serial(oid)
    }

    /// Check that `restore_blob` can stage a copy of `oid` with `data`, and
    /// return the current serial of `oid`.
    fn check_restore_blob(&self, oid: Oid, data: &[u8]) -> Result<Tid> {
        let current = self.check_restore(oid)?;
        if data.is_empty() {
            return Err(ZoeError::Protocol("restore of a blob with an empty pickle"));
        }
        Ok(current)
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
    /// the lock timeout of the store. A longer wait fails with `Conflict`,
    /// and so does a wait that the cancel of `set_lock_cancel` ends.
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
        // Only the vote that takes the locks runs tier 1. The vote after
        // `vote_resolving` checks the serials again.
        let native = !self.locked();
        if native {
            self.lock()?;
        } else {
            self.verify_shadows();
        }
        self.resolving.clear();
        let checked = self.check(false, native);
        if native {
            self.observe_lock_wait();
        }
        checked.map(drop)
    }

    /// Give the lock wait of the vote that took the locks to the observer.
    fn observe_lock_wait(&mut self) {
        if let (Some(observer), Some(waited)) = (self.store.observer(), self.lock_waited.take()) {
            observer.locks_acquired(waited);
        }
    }

    /// Compare each merge of shadow mode of `vote_resolving` with the
    /// record that the caller stored for its oid over the committed serial.
    /// An oid that the caller did not resolve gives nothing.
    fn verify_shadows(&mut self) {
        self.verifications.clear();
        for (oid, shadowed) in std::mem::take(&mut self.shadows) {
            if !self.resolving.contains(&oid) {
                continue;
            }
            let Ok(Some((serial, pickle))) = self.staged.get(oid) else {
                continue;
            };
            if serial != shadowed.committed {
                continue;
            }
            self.verifications.push(Verification {
                oid,
                tier: shadowed.tier,
                class: shadowed.class,
                policy: shadowed.policy,
                same: blake3::hash(&pickle) == shadowed.digest,
                committed: shadowed.committed,
                read: shadowed.read,
            });
        }
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
        let checked = self.check(true, true);
        self.observe_lock_wait();
        let conflicts = checked?;
        self.resolving = conflicts.iter().map(|&(oid, _)| oid).collect();
        Ok(conflicts)
    }

    /// Return the user of the transaction, as `tpc_begin` gave it.
    pub fn user(&self) -> &[u8] {
        &self.user
    }

    /// Return the description of the transaction, as `tpc_begin` gave it.
    pub fn description(&self) -> &[u8] {
        &self.description
    }

    /// Return what tiers 1 and 2 did in the undos of the transaction, then
    /// in the last vote that took the locks: one entry for each three-way
    /// merge of an undo, and one for each stale written oid of the vote, of
    /// a class of a tier whose mode is not `Off`, in oid order. The vote that comes
    /// after `vote_resolving` keeps the entries of `vote_resolving`.
    pub fn resolutions(&self) -> &[Resolution] {
        &self.resolutions
    }

    /// Return the lines of the resolution log (ADR 0042) of what the tiers
    /// did in the undos and in the last vote that took the locks: one JSON
    /// object for each entry of `resolutions`, at `time`, for the storage `storage` of a server.
    /// The user and the description of the committed transaction come from
    /// the history of the oid; when the store no longer has that record,
    /// the line gives null for them.
    pub fn resolution_log_lines(&self, time: SystemTime, storage: Option<&str>) -> Vec<String> {
        self.resolution_log_lines_of(0..self.resolutions.len(), time, storage)
    }

    /// Return the number of entries of `resolutions` that the undos of the
    /// transaction gave. They come first, before the entries of the vote.
    pub fn undo_resolution_count(&self) -> usize {
        self.undo_resolutions
    }

    /// Return the lines of the resolution log of the entries `range` of
    /// `resolutions`, as `resolution_log_lines` does. A skin that writes
    /// the lines of each undo when it ends, and those of the vote after it,
    /// writes each line once.
    pub fn resolution_log_lines_of(
        &self,
        range: std::ops::Range<usize>,
        time: SystemTime,
        storage: Option<&str>,
    ) -> Vec<String> {
        self.resolutions[range]
            .iter()
            .map(|resolution| {
                // A conflict with a commit in flight names that commit. Its
                // tid can be unknown at the vote: the line then gives 0.
                if resolution.committed == Tid::ZERO
                    && let Some(handoff) = self.handed.get(&resolution.oid)
                {
                    let commit = &handoff.commit;
                    let mut resolution = resolution.clone();
                    resolution.committed = commit.tid().unwrap_or(Tid::ZERO);
                    let context = merge::LogContext {
                        time,
                        storage,
                        user: &self.user,
                        description: &self.description,
                        committed_user: Some(&commit.user),
                        committed_description: Some(&commit.description),
                    };
                    return merge::log_line(&resolution, &context);
                }
                let committed = self.committed_entry(resolution.oid, resolution.committed);
                let context = merge::LogContext {
                    time,
                    storage,
                    user: &self.user,
                    description: &self.description,
                    committed_user: committed.as_ref().map(|entry| &entry.user[..]),
                    committed_description: committed.as_ref().map(|entry| &entry.description[..]),
                };
                merge::log_line(resolution, &context)
            })
            .collect()
    }

    /// Return what the vote after `vote_resolving` found for each merge of
    /// shadow mode that the caller resolved: whether the record of the
    /// caller is the merged record (ADR 0046).
    pub fn verifications(&self) -> &[Verification] {
        &self.verifications
    }

    /// Return the lines of the resolution log of `verifications`, at
    /// `time`, for the storage `storage` of a server.
    pub fn verification_log_lines(&self, time: SystemTime, storage: Option<&str>) -> Vec<String> {
        self.verifications
            .iter()
            .map(|verification| {
                let committed = self.committed_entry(verification.oid, verification.committed);
                let context = merge::LogContext {
                    time,
                    storage,
                    user: &self.user,
                    description: &self.description,
                    committed_user: committed.as_ref().map(|entry| &entry.user[..]),
                    committed_description: committed.as_ref().map(|entry| &entry.description[..]),
                };
                merge::verification_line(verification, &context)
            })
            .collect()
    }

    /// Return the oids that tier 1 merged in the last vote that took the
    /// locks and passed. Their staged records hold the merged records, over
    /// the serial of the committed records.
    pub fn natively_resolved(&self) -> Vec<Oid> {
        self.resolutions[self.undo_resolutions..]
            .iter()
            .filter(|resolution| resolution.applied && resolution.outcome == Resolved::Merged)
            .map(|resolution| resolution.oid)
            .collect()
    }

    /// Return the history entry of `oid` at `tid`, when the store still has
    /// it.
    fn committed_entry(&self, oid: Oid, tid: Tid) -> Option<crate::read::HistoryEntry> {
        self.store
            .history(oid, HISTORY_SEARCH)
            .ok()
            .and_then(|entries| entries.into_iter().find(|entry| entry.tid == tid))
    }

    /// Take the locks of the vote, and keep them until `finish`, `abort` or
    /// a failed vote. A lock timeout releases the locks that the call took,
    /// and logs each one held past `lock-warn`. A read-only store fails
    /// with `ReadOnly` and gives no lock.
    fn lock(&mut self) -> Result<()> {
        self.store.writable()?;
        let store: &Store = &self.store;
        let mut requests = self.lock_requests();
        // A store without an observer does not read the clock. The wait is
        // measured on the monotonic time of the store clock, as the lock
        // timeout is, so a simulator gives it on simulated time.
        let started = store.observer().map(|_| store.clock.monotonic());
        let cancel = self.lock_cancel.as_ref();
        let mut acquired = None;
        // A vote that takes its locks without a wait waits for a commit in
        // flight until the lock timeout after its start.
        let mut deadline = None;
        if !self.intents.is_empty()
            && let Some(&(lowest, _)) = requests.first()
        {
            // The intent locks are above some oid that the vote needs. When
            // every such oid is free, the vote takes them at once. Else it
            // releases the intent locks above the lowest of them, so that it
            // waits in oid order, and takes them again with the others. The
            // serial check catches a commit that came in between.
            let begun = store.clock.monotonic();
            let tried =
                store
                    .locks
                    .acquire_cancellable(&requests, Duration::ZERO, &*store.clock, cancel);
            match tried {
                Ok(locks) => {
                    deadline = Some(begun.saturating_add(store.lock_timeout));
                    acquired = Some(Ok(locks));
                }
                Err(Refused { released, .. }) => {
                    store
                        .guardrails
                        .locks_released(&released, store.clock.now());
                    let above = self.intents.partition_point(|lock| lock.oid < lowest);
                    let given: Vec<Held> = self.intents.split_off(above);
                    store.locks.release(&given);
                    store.guardrails.locks_released(&given, store.clock.now());
                    requests.extend(given.iter().map(|lock| (lock.oid, Mode::Exclusive)));
                    requests.sort_by_key(|&(oid, _)| oid);
                }
            }
        }
        let acquired = acquired.unwrap_or_else(|| {
            store
                .locks
                .acquire_cancellable(&requests, store.lock_timeout, &*store.clock, cancel)
        });
        let waited = started.map(|started| store.clock.monotonic().saturating_sub(started));
        match acquired {
            Ok(mut locks) => {
                // The vote adds its waits for commits in flight, and gives
                // the sum to the observer at its end.
                self.lock_waited = waited;
                self.handed = locks.handoffs().into_iter().collect();
                self.onto.clear();
                self.lock_deadline = deadline.unwrap_or_else(|| locks.deadline());
                let mut held = locks.keep();
                held.append(&mut self.intents);
                held.sort_by_key(|lock| lock.oid);
                self.locks = Some(held);
                Ok(())
            }
            Err(Refused { error, released }) => {
                if let (Some(observer), Some(waited)) = (store.observer(), waited) {
                    observer.locks_acquired(waited);
                }
                let now = store.clock.now();
                store.guardrails.locks_released(&released, now);
                Err(error)
            }
        }
    }

    /// Check the serials with the locks held. Return the stale written oids
    /// that the caller can resolve, with their committed records. When
    /// `resolve` is false, the caller can resolve none. When `native` is
    /// true, tier 1 runs first on each stale written oid of its classes, and
    /// the check stages the records that it merged.
    ///
    /// A stale serial that the caller cannot resolve, a refusal of tier 1,
    /// or an error, fails the check and releases the locks.
    fn check(&mut self, resolve: bool, native: bool) -> Result<Vec<(Oid, Record)>> {
        if native {
            // The entries of the undos come first, and they stay.
            self.resolutions.truncate(self.undo_resolutions);
            self.shadows.clear();
            self.verifications.clear();
        }
        let checked = self
            .check_undone()
            .and_then(|()| self.check_serials(resolve, native))
            .and_then(|check| match check {
                Check::Passed {
                    conflicts,
                    merged,
                    onto,
                    stored_bytes,
                } => {
                    self.stage_merged(merged, stored_bytes)?;
                    self.onto.extend(onto);
                    Ok(Check::Passed {
                        conflicts,
                        merged: Vec::new(),
                        onto: Vec::new(),
                        stored_bytes,
                    })
                }
                failed => Ok(failed),
            });
        let error = match checked {
            Ok(Check::Passed { conflicts, .. }) => return Ok(conflicts),
            Ok(Check::Failed(stale)) => {
                self.conflict = stale.current.map(|current| (current, stale.read));
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

    /// Fail with `Undo` when the transaction undoes a transaction at or
    /// before the pack watermark: a pack ran after the undo read it. The
    /// vote checks this with its locks held, so the common case fails
    /// before the finish. The finish checks it again under the commit
    /// sequencer.
    fn check_undone(&self) -> Result<()> {
        group::check_undone(&self.store, self.undone)
    }

    /// Return the serials of the stale oid that failed the last vote: the
    /// current serial of the oid, then the serial that this transaction
    /// read. These are the `serials` of a ZODB `ConflictError`.
    ///
    /// Return `None` when the last vote did not fail on a stale serial, for
    /// example when it waited past the lock timeout or a cancel ended its
    /// wait.
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
        // An adopted intent lock is exclusive already.
        for lock in &self.intents {
            requests.remove(&lock.oid);
        }
        requests.into_iter().collect()
    }

    /// Stage the records that tier 1 merged over the serials of the
    /// committed records, and mark their entries as applied.
    fn stage_merged(&mut self, merged: Vec<(Oid, Tid, Vec<u8>)>, stored_bytes: u64) -> Result<()> {
        if merged.is_empty() {
            return Ok(());
        }
        let area = &self.store.backend.staging;
        for (oid, serial, pickle) in &merged {
            self.staged.put(area, *oid, *serial, pickle, false)?;
        }
        self.stored_bytes = stored_bytes;
        for resolution in &mut self.resolutions[self.undo_resolutions..] {
            if resolution.outcome == Resolved::Merged
                && merged.iter().any(|(oid, _, _)| *oid == resolution.oid)
            {
                resolution.applied = true;
            }
        }
        Ok(())
    }

    /// Compare the serial that the client read of each oid with its current
    /// serial. The written oids come first, then the read-current oids.
    ///
    /// The first stale oid fails the check, unless tier 1 merges it, or
    /// `resolve` is true and the oid is a written oid that the caller can
    /// resolve. The check gives each such oid with its committed record.
    /// When `native` is true, tier 1 runs on each stale written oid that
    /// the caller could resolve, whether `resolve` is true or not, and its
    /// entries go to `self.resolutions`.
    fn check_serials(&mut self, resolve: bool, native: bool) -> Result<Check> {
        let mut conflicts = Vec::new();
        let mut merged = Vec::new();
        let mut onto = Vec::new();
        let mut stored_bytes = self.stored_bytes;
        // The first conflict reads the merge policy. A vote without a
        // conflict reads nothing.
        let mut policy: Option<Option<Arc<Policy>>> = None;
        // One backend read serves a chunk of oids. The chunk bounds the
        // committed records in memory.
        let staged: Vec<_> = self.staged.iter().collect();
        for chunk in staged.chunks(READ_CHUNK) {
            let serials = self.current_serials(chunk.iter().map(|(oid, _)| *oid))?;
            for (&(oid, staged), serial) in chunk.iter().zip(serials) {
                let mut current = serial.unwrap_or(Tid::ZERO);
                // The store does not hold the commit in flight of a lock
                // that came with a hand-off: its staged record is the
                // committed record.
                if let Some(handoff) = self.handed.get(&oid).cloned() {
                    let checked = self.check_handed(
                        oid,
                        staged,
                        &handoff,
                        native,
                        stored_bytes,
                        &mut policy,
                    )?;
                    match checked {
                        Handed::Passed => continue,
                        Handed::Merged {
                            pickle,
                            size,
                            resolution,
                        } => {
                            stored_bytes = size;
                            // The serial is a placeholder: the finish writes
                            // the tid of the commit in flight. Another vote
                            // of the oid without the hand-off fails on it.
                            merged.push((oid, Tid::ZERO, pickle));
                            onto.push((oid, Arc::clone(&handoff.commit)));
                            self.resolutions.push(resolution);
                            continue;
                        }
                        Handed::Failed { stale, resolution } => {
                            self.resolutions.extend(resolution);
                            return Ok(Check::Failed(stale));
                        }
                        Handed::Store { waited } => {
                            // The wait for the commit in flight counts in the
                            // lock wait of the vote.
                            if let Some(total) = &mut self.lock_waited {
                                *total += waited;
                            }
                            current = self.current_serial(oid)?;
                        }
                        Handed::Late { waited } => {
                            if let Some(total) = &mut self.lock_waited {
                                *total += waited;
                            }
                            return Err(ZoeError::Conflict { oid });
                        }
                    }
                }
                if current == staged.serial {
                    continue;
                }
                let stale = Stale {
                    oid,
                    current: Some(current),
                    read: staged.serial,
                    read_only: false,
                };
                if !(resolve || native)
                    || staged.restored
                    || staged.serial == Tid::ZERO
                    || staged.pickle_len() == 0
                {
                    return Ok(Check::Failed(stale));
                }
                // Only a conflict reads its committed record. A deletion
                // has an empty pickle and is never resolved. A record that
                // is gone since the serials were read is a pack of it:
                // the vote then fails as it does for a stale serial.
                let record = match self.store.backend.load_before(oid, Tid::MAX)? {
                    Some(record) if record.tid == current && !record.data.is_empty() => record,
                    _ => return Ok(Check::Failed(stale)),
                };
                let resolved = if native {
                    if policy.is_none() {
                        policy = Some(self.store.merge_policy()?);
                    }
                    let rules = Rules {
                        config: &self.store.structural_merges,
                        policy: policy.as_ref().and_then(Option::as_deref),
                    };
                    self.native(oid, staged.serial, &record.data, &rules)?
                        .map(|(mode, merge)| (mode, rules.version(), merge))
                } else {
                    None
                };
                if let Some((mode, version, merge)) = resolved {
                    let mut resolution =
                        Resolution::of(oid, mode, version, &merge, current, staged.serial);
                    if let (merge::Mode::Shadow, Outcome::Merged(pickle)) = (mode, &merge.outcome) {
                        self.shadows.insert(
                            oid,
                            Shadowed {
                                tier: merge.tier,
                                class: merge.class.clone(),
                                policy: version,
                                digest: blake3::hash(pickle),
                                committed: current,
                                read: staged.serial,
                            },
                        );
                    }
                    match (mode, merge.outcome) {
                        (merge::Mode::On, Outcome::Merged(pickle)) => {
                            match self.merged_size(oid, &pickle, stored_bytes) {
                                Some(size) => {
                                    stored_bytes = size;
                                    merged.push((oid, current, pickle));
                                    self.resolutions.push(resolution);
                                    continue;
                                }
                                None => {
                                    resolution.outcome =
                                        Resolved::Undecided("a merged record over a size limit");
                                }
                            }
                        }
                        (merge::Mode::On, Outcome::Refused(_)) => {
                            resolution.applied = true;
                            self.resolutions.push(resolution);
                            return Ok(Check::Failed(stale));
                        }
                        _ => {}
                    }
                    self.resolutions.push(resolution);
                }
                if !resolve {
                    return Ok(Check::Failed(stale));
                }
                conflicts.push((oid, record));
            }
        }
        let read_current: Vec<_> = self.read_current.iter().collect();
        for chunk in read_current.chunks(READ_CHUNK) {
            let serials = self.current_serials(chunk.iter().map(|&(&oid, _)| oid))?;
            for (&(&oid, &read), serial) in chunk.iter().zip(serials) {
                // The last commit of an oid held with a hand-off can be in
                // flight: the store does not hold it yet.
                if let Some(handoff) = self.handed.get(&oid)
                    && handoff.commit.ended().is_none()
                    && handoff.commit.tid() != Some(read)
                {
                    return Ok(Check::Failed(Stale {
                        oid,
                        current: handoff.commit.tid(),
                        read,
                        read_only: true,
                    }));
                }
                let current = serial.unwrap_or(Tid::ZERO);
                if current != read {
                    return Ok(Check::Failed(Stale {
                        oid,
                        current: Some(current),
                        read,
                        read_only: true,
                    }));
                }
            }
        }
        Ok(Check::Passed {
            conflicts,
            merged,
            onto,
            stored_bytes,
        })
    }

    /// Check a written oid whose lock came with `handoff`.
    ///
    /// When the commit in flight of the hand-off is durable or failed, the
    /// check goes on with the store. Otherwise its staged record is the
    /// committed record: the tier of its class in mode `on` merges onto it,
    /// or refuses. When no tier decides, the call waits until the commit is
    /// durable or failed, at most until the lock deadline of the vote, and
    /// the check then goes on with the store. A wait past the deadline, or
    /// a cancel, ends as `Handed::Late`: the vote fails with `Conflict` on
    /// the oid. A transaction that read the commit in flight checks the oid
    /// against the store, which holds that commit.
    fn check_handed(
        &self,
        oid: Oid,
        staged: &staging::Staged,
        handoff: &Handoff,
        native: bool,
        stored_bytes: u64,
        policy: &mut Option<Option<Arc<Policy>>>,
    ) -> Result<Handed> {
        // The merge of the vote before, onto the same commit, stands.
        if self.onto.contains_key(&oid) {
            return Ok(Handed::Passed);
        }
        let commit = &handoff.commit;
        // The transaction read the commit in flight: its batch is visible,
        // and the store holds it.
        if commit.ended().is_some() || commit.tid() == Some(staged.serial) {
            return Ok(Handed::Store {
                waited: Duration::ZERO,
            });
        }
        let stale = Stale {
            oid,
            current: commit.tid(),
            read: staged.serial,
            read_only: false,
        };
        // The holder changed the oid, so the serial that the transaction
        // read is stale. A restore, a new object or a deletion cannot be
        // resolved.
        if staged.restored
            || staged.serial == Tid::ZERO
            || staged.pickle_len() == 0
            || handoff.record.is_empty()
        {
            return Ok(Handed::Failed {
                stale,
                resolution: None,
            });
        }
        if native {
            // The first conflict of the vote reads the merge policy.
            if policy.is_none() {
                *policy = Some(self.store.merge_policy()?);
            }
        }
        let rules = Rules {
            config: &self.store.structural_merges,
            policy: policy.as_ref().and_then(Option::as_deref),
        };
        if native
            && let Some((mode @ merge::Mode::On, merge)) =
                self.native(oid, staged.serial, &handoff.record, &rules)?
        {
            let committed = commit.tid().unwrap_or(Tid::ZERO);
            let mut resolution =
                Resolution::of(oid, mode, rules.version(), &merge, committed, staged.serial);
            match merge.outcome {
                Outcome::Merged(pickle) => {
                    if let Some(size) = self.merged_size(oid, &pickle, stored_bytes) {
                        return Ok(Handed::Merged {
                            pickle,
                            size,
                            resolution,
                        });
                    }
                }
                Outcome::Refused(_) => {
                    resolution.applied = true;
                    return Ok(Handed::Failed {
                        stale,
                        resolution: Some(resolution),
                    });
                }
                Outcome::Undecided(_) => {}
            }
        }
        let store: &Store = &self.store;
        let started = store.observer().map(|_| store.clock.monotonic());
        let outcome = commit.wait(
            oid,
            self.lock_deadline,
            &*store.clock,
            self.lock_cancel.as_ref(),
        );
        let waited = started.map_or(Duration::ZERO, |started| {
            store.clock.monotonic().saturating_sub(started)
        });
        match outcome {
            Waited::Durable | Waited::Failed => Ok(Handed::Store { waited }),
            Waited::TimedOut | Waited::Cancelled => Ok(Handed::Late { waited }),
        }
    }

    /// Run tier 1 or tier 2 on the conflict of `oid`: the transaction read
    /// `read`, and `committed` is the pickle of the current record, or of
    /// the commit in flight of a hand-off. Return `None` when the
    /// class of the staged record is neither a class of tier 1 whose mode
    /// is not `Off`, nor a class whose entry in the merge policy has tier 2
    /// on or in shadow mode.
    ///
    /// The class comes from the class pickle of the staged record alone.
    /// Only a class of one of the tiers reads the record at `read`.
    fn native(
        &self,
        oid: Oid,
        read: Tid,
        committed: &[u8],
        rules: &Rules<'_>,
    ) -> Result<Option<(merge::Mode, Merge)>> {
        // A staged pickle that does not read back gives no answer: the
        // conflict goes on, and the caller that reads the pickle again
        // reports the error.
        let Ok(Some((_, new))) = self.staged.get(oid) else {
            return Ok(None);
        };
        let Some(tier) = Tier::of(&new, rules) else {
            return Ok(None);
        };
        let Some(old) = self.store.backend.load_serial(oid, read)? else {
            let merge = Merge {
                tier: tier.tier,
                class: tier.class,
                outcome: Outcome::Undecided("the record that the transaction read is gone"),
                keys: Vec::new(),
                changed: 0,
                shadow: None,
                fields: Vec::new(),
            };
            return Ok(Some((tier.mode, merge)));
        };
        Ok(Some(tier.merge(&old, committed, &new, rules)))
    }

    /// Return the transaction size after a merged pickle of `oid` replaces
    /// its staged pickle in a transaction of `stored_bytes`, or `None` when
    /// the pickle breaks `max-record-size` or `max-transaction-size`.
    fn merged_size(&self, oid: Oid, pickle: &[u8], stored_bytes: u64) -> Option<u64> {
        if self.is_restore() {
            return Some(stored_bytes);
        }
        let guardrails = &self.store.guardrails;
        let size = pickle.len() as u64;
        guardrails.check(SizeLimit::Record, oid, size).ok()?;
        let staged = self
            .staged
            .record(oid)
            .map_or(0, |staged| staged.pickle_len() as u64);
        let total = stored_bytes.saturating_sub(staged).saturating_add(size);
        guardrails.check(SizeLimit::Transaction, oid, total).ok()?;
        Some(total)
    }

    /// Return the current serial of each oid of `oids`, in order, from one
    /// backend read. A backend that gives another number of serials fails
    /// with a backend error, so the vote never skips an oid.
    fn current_serials(
        &self,
        oids: impl ExactSizeIterator<Item = Oid>,
    ) -> Result<Vec<Option<Tid>>> {
        let expected = oids.len();
        let mut oids = oids;
        let serials = self.store.backend.current_serials(&mut oids)?;
        if serials.len() != expected {
            let error = format!(
                "the backend gave {} current serials for {expected} oids",
                serials.len()
            );
            return Err(ZoeError::Backend(error.into()));
        }
        Ok(serials)
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
    /// when this returns, except for a deferred restore
    /// (`begin_restore_deferred`): that commit is only visible, and the
    /// next durable commit makes it durable.
    ///
    /// A restore transaction commits under its own tid. If that tid is not
    /// after the last tid, `finish` fails with `Protocol`. A normal
    /// transaction gets a tid after the last tid, below `Tid::MAX`. When no
    /// such tid is left, `finish` fails with `Protocol`. A transaction that
    /// undoes a transaction at or before the pack watermark fails with
    /// `Undo`: a pack ran after the undo read its records. A read of the
    /// watermark for that check that fails gives its error. Each of these
    /// errors fails only this commit, not the others of its batch, and the
    /// store does not halt.
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
        let (commit, handoffs) = self.handoffs();
        let member = Member {
            tid: self.tid,
            deferred: self.deferred,
            staged: std::mem::take(&mut self.staged),
            blobs: std::mem::take(&mut self.blobs),
            user: std::mem::take(&mut self.user),
            description: std::mem::take(&mut self.description),
            extension: std::mem::take(&mut self.extension),
            commit: commit.clone(),
            onto: std::mem::take(&mut self.onto),
            undone: self.undone,
        };
        let store: &Store = &self.store;
        let held = &mut self.locks;
        let mut handed = Vec::new();
        let empty = handoffs.is_empty();
        // The member is in the queue while `queued` runs, so it calls no
        // code that can panic but the lock manager: the time of the
        // hand-offs comes before the finish joins the queue.
        let released = if empty {
            SystemTime::UNIX_EPOCH
        } else {
            store.clock.now()
        };
        // The hand-offs come once the member is in the queue of group
        // commit, so the commit of a new owner comes after this one.
        let queued = (!empty).then_some(|| {
            if let Some(held) = held.as_mut() {
                handed = store.locks.hand_off(handoffs, held);
            }
        });
        let (member, finished) = store.group.finish(store, member, queued);
        if let Some(commit) = &commit {
            commit.end(finished.is_ok());
        }
        if !handed.is_empty() {
            store
                .locks
                .settle(handed.iter().map(|(lock, handoff)| (lock.oid, handoff)));
            let locks: Vec<Held> = handed.iter().map(|(lock, _)| *lock).collect();
            store.guardrails.locks_released(&locks, released);
            if let Some(observer) = store.observer() {
                observer.locks_handed_off(locks.len());
            }
        }
        // The transaction takes its staged records and its blobs back, so
        // `Drop` removes its spill file and its unplaced blob files.
        if let Some(member) = member {
            self.staged = member.staged;
            self.blobs = member.blobs;
        }
        let finished = finished?;
        self.committed = Some(finished.tid);
        // The commit is durable, and it comes after the commit of each
        // hand-off that the vote got. So the store holds the last commit of
        // these oids, also when their holder did not settle yet.
        if !self.handed.is_empty() {
            store
                .locks
                .settle(self.handed.iter().map(|(oid, handoff)| (*oid, handoff)));
        }
        self.release();
        Ok(finished)
    }

    /// Return the commit in flight of a finish that starts and the
    /// hand-offs that it can make: one for each written oid of a class that
    /// tier 1 merges in mode `on`, whose queue has an exclusive request at
    /// its head, with a copy of its staged pickle. The mode comes from the
    /// configuration and the merge policy, as in the vote. A restore hands
    /// off nothing, because its finish can fail alone, and nor does a
    /// finish that cannot read the merge policy.
    fn handoffs(&self) -> (Option<Arc<InFlight>>, Handoffs) {
        if self.is_restore() {
            return (None, Vec::new());
        }
        let contended = self
            .store
            .locks
            .contended(self.staged.iter().map(|(oid, _)| oid));
        if contended.is_empty() {
            return (None, Vec::new());
        }
        let Ok(policy) = self.store.merge_policy() else {
            return (None, Vec::new());
        };
        let rules = Rules {
            config: &self.store.structural_merges,
            policy: policy.as_deref(),
        };
        let commit = Arc::new(InFlight::new(&self.user, &self.description));
        let handoffs: Vec<_> = contended
            .into_iter()
            .filter_map(|oid| {
                let Ok(Some((_, pickle))) = self.staged.get(oid) else {
                    return None;
                };
                let class = merge::tier1_class(&pickle)?;
                (rules.tier1_mode(&class) == merge::Mode::On).then(|| {
                    let handoff = Handoff {
                        commit: Arc::clone(&commit),
                        record: pickle.into_owned(),
                    };
                    (oid, Arc::new(handoff))
                })
            })
            .collect();
        if handoffs.is_empty() {
            return (None, handoffs);
        }
        (Some(commit), handoffs)
    }

    /// Abort the transaction: drop the staged records, remove the spill
    /// file and release the locks.
    pub fn abort(mut self) {
        self.release();
    }

    /// Release the locks, then log each lock held past `lock-warn`.
    fn release(&mut self) {
        let mut held = self.locks.take().unwrap_or_default();
        held.append(&mut self.intents);
        if !held.is_empty() {
            let now = self.store.clock.now();
            self.store.locks.release(&held);
            self.store.guardrails.locks_released(&held, now);
        }
    }
}

impl<S: Deref<Target = Store>> Drop for Txn<S> {
    fn drop(&mut self) {
        self.release();
        self.staged.remove_spill(&self.store.backend.staging);
        // `finish` and `abort` also end here. So each transaction is
        // checked against `transaction-warn` once, when it ends.
        let now = self.store.clock.now();
        let ended = Ended {
            oids: self.staged.len() as u64,
            oid: self.staged.iter().next().map(|(oid, _)| oid),
            tid: self.committed,
        };
        self.store
            .guardrails
            .transaction_ended(self.began, now, ended);
    }
}

/// The tier that merges the records of a class, and its mode: tier 1 for
/// a class of BTrees or `Length`, tier 2 for a class that the merge policy
/// names.
struct Tier<'r> {
    tier: u8,
    class: String,
    mode: merge::Mode,
    /// The entry of the merge policy of a class of tier 2.
    entry: Option<&'r merge::policy::ClassPolicy>,
}

impl<'r> Tier<'r> {
    /// Return the tier of the record `new`, from its class pickle alone, or
    /// `None` when no tier merges its class or the mode of the tier is
    /// `Off`.
    fn of(new: &[u8], rules: &'r Rules<'_>) -> Option<Self> {
        let tier = if let Some(class) = merge::tier1_class(new) {
            let mode = rules.tier1_mode(&class);
            Tier {
                tier: 1,
                class,
                mode,
                entry: None,
            }
        } else {
            let class = merge::class_name(new)?;
            let entry = rules.tier2(&class)?;
            Tier {
                tier: 2,
                class,
                mode: entry.tier2,
                entry: Some(entry),
            }
        };
        (tier.mode != merge::Mode::Off).then_some(tier)
    }

    /// Merge the three records of a conflict in this tier: `old` is the
    /// base, `committed` the committed record and `new` the record that
    /// goes over it.
    fn merge(
        &self,
        old: &[u8],
        committed: &[u8],
        new: &[u8],
        rules: &Rules<'_>,
    ) -> (merge::Mode, Merge) {
        let merge = match self.entry {
            None => {
                merge::tier1(old, committed, new, rules.posting_dicts()).expect("a class of tier 1")
            }
            Some(entry) => merge::tier2(old, committed, new, &self.class, entry),
        };
        (self.mode, merge)
    }
}

/// Merge the three records of an undo through the tiers: `old` is the
/// record of the undone transaction, `committed` the current record and
/// `new` the record before the undone transaction. Return `None` when no
/// tier merges the class of `new`, or the mode of its tier is `Off`.
///
/// The undo calls it. The vote picks its tier with the same `Tier::of`
/// and merges with the same `Tier::merge`, apart, so that it reads the
/// record that its transaction read only for a class of a tier.
///
/// In an undo, a field of strategy `lww` that both sides changed refuses,
/// as `strict` does (ADR 0046): the undo would drop the later change
/// without a word.
fn undo_tiers(
    old: &[u8],
    committed: &[u8],
    new: &[u8],
    rules: &Rules<'_>,
) -> Option<(merge::Mode, Merge)> {
    let tier = Tier::of(new, rules)?;
    let strict = tier.entry.map(without_lww);
    let tier = Tier {
        entry: strict.as_ref().or(tier.entry),
        ..tier
    };
    Some(tier.merge(old, committed, new, rules))
}

/// Return `entry` with the strategy `strict` in place of each `lww`.
fn without_lww(entry: &merge::policy::ClassPolicy) -> merge::policy::ClassPolicy {
    use merge::policy::Strategy;
    let strict = |strategy: Strategy| match strategy {
        Strategy::Lww => Strategy::Strict,
        other => other,
    };
    let mut entry = entry.clone();
    entry.default = strict(entry.default);
    for strategy in entry.fields.values_mut() {
        *strategy = strict(*strategy);
    }
    entry
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

/// The hand-offs that a finish can make: each oid with its hand-off.
type Handoffs = Vec<(Oid, Arc<Handoff>)>;

/// What the check of an oid that came with a hand-off found.
enum Handed {
    /// The oid passes.
    Passed,
    /// A tier merged onto the record of the commit in flight.
    Merged {
        pickle: Vec<u8>,
        /// The transaction size with the merged pickle.
        size: u64,
        resolution: Resolution,
    },
    /// The conflict fails the vote.
    Failed {
        stale: Stale,
        resolution: Option<Resolution>,
    },
    /// The commit in flight is durable or failed, or the transaction read
    /// it: the check goes on with the store. `waited` is the wait for its
    /// end.
    Store { waited: Duration },
    /// The wait for the end of the commit in flight passed the lock
    /// deadline of the vote, or a cancel ended it: the vote fails.
    Late { waited: Duration },
}

/// The number of oids whose current serials the vote reads in one backend
/// read.
const READ_CHUNK: usize = 256;

/// The most records of the history of an oid that a line of the resolution
/// log reads to find the committed record. Other commits can come after
/// the vote, once it released its locks.
const HISTORY_SEARCH: usize = 8;

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
    use crate::clock::{ManualClock, SystemClock};
    use crate::guard::capture::{Captured, events_of};
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
    fn a_cancel_ends_the_lock_wait_of_a_vote_from_another_thread() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let low = store.new_oid().unwrap();
        let high = store.new_oid().unwrap();
        let mut holder = store.tpc_begin(b"", b"", b"").unwrap();
        holder.store(high, Tid::ZERO, b"held").unwrap();
        holder.vote().unwrap();
        let cancel = LockCancel::new();
        thread::scope(|scope| {
            let voter = scope.spawn(|| {
                let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
                txn.set_lock_cancel(cancel.clone());
                txn.store(low, Tid::ZERO, b"gone").unwrap();
                txn.store(high, Tid::ZERO, b"gone").unwrap();
                let voted = txn.vote();
                (voted, txn.conflict_serials())
            });
            // The voter holds the lock of `low` and waits for `high`.
            while store.locks.waiting(high) == 0 {
                thread::sleep(Duration::from_millis(1));
            }
            cancel.cancel();
            let (voted, serials) = voter.join().unwrap();
            assert!(matches!(voted, Err(ZoeError::Conflict { oid }) if oid == high));
            assert_eq!(serials, None);
        });
        // The vote released the lock of `low`.
        commit(&store, low, Tid::ZERO, b"after");
        holder.finish().unwrap();
        assert!(store.lock_table_is_empty());
    }

    #[test]
    fn a_cancel_does_nothing_to_a_vote_that_passed() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let oid = store.new_oid().unwrap();
        let cancel = LockCancel::new();
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.set_lock_cancel(cancel.clone());
        txn.store(oid, Tid::ZERO, b"a").unwrap();
        txn.vote().unwrap();
        cancel.cancel();
        assert!(!store.lock_table_is_empty());
        let tid = txn.finish().unwrap();
        assert_eq!(store.load(oid).unwrap(), (b"a".to_vec(), tid));
        assert!(store.lock_table_is_empty());
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

    /// Open a store in memory with `options`.
    fn open_with(options: Options) -> Store {
        Store::open_full(MemVfs::new(), SystemClock, options).unwrap()
    }

    #[test]
    fn a_refused_store_stages_nothing() {
        let store = open_with(Options {
            max_record_size: Some(3),
            ..Options::default()
        });
        let (kept, refused) = (Oid(1), Oid(2));
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.store(kept, Tid::ZERO, b"abc").unwrap();
        for oid in [kept, refused] {
            let error = txn.store(oid, Tid::ZERO, b"abcd").unwrap_err();
            assert!(
                matches!(
                    error,
                    ZoeError::TooLarge {
                        limit: SizeLimit::Record,
                        size: 4,
                        max: 3,
                        ..
                    }
                ),
                "{error:?}"
            );
        }
        let (serial, data) = txn.staged(kept).unwrap().unwrap();
        assert_eq!((serial, &*data), (Tid::ZERO, &b"abc"[..]));
        assert!(txn.staged(refused).unwrap().is_none());
        txn.vote().unwrap();
        let tid = txn.finish().unwrap();
        assert_eq!(store.load(kept).unwrap(), (b"abc".to_vec(), tid));
        assert!(matches!(
            store.load(refused),
            Err(ZoeError::NotFound { .. })
        ));
    }

    #[test]
    fn a_call_out_of_order_is_protocol_whatever_its_size() {
        let store = open_with(Options {
            max_record_size: Some(1),
            ..Options::default()
        });
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.store(Oid(1), Tid::ZERO, b"a").unwrap();
        txn.vote().unwrap();
        let error = txn.store(Oid(1), Tid::ZERO, b"too large");
        assert!(matches!(error, Err(ZoeError::Protocol(_))), "{error:?}");
    }

    #[test]
    fn a_replaced_pickle_counts_once_toward_the_transaction_size() {
        let store = open_with(Options {
            max_transaction_size: Some(6),
            ..Options::default()
        });
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.store(Oid(1), Tid::ZERO, b"abcd").unwrap();
        txn.store(Oid(1), Tid::ZERO, b"abcdef").unwrap();
        let error = txn.store(Oid(2), Tid::ZERO, b"g").unwrap_err();
        assert!(
            matches!(
                error,
                ZoeError::TooLarge {
                    oid: Oid(2),
                    limit: SizeLimit::Transaction,
                    size: 7,
                    max: 6
                }
            ),
            "{error:?}"
        );
        // A shorter pickle of the same oid makes room.
        txn.store(Oid(1), Tid::ZERO, b"abcde").unwrap();
        txn.store(Oid(2), Tid::ZERO, b"g").unwrap();
        txn.vote().unwrap();
        txn.finish().unwrap();
    }

    #[test]
    fn a_restore_transaction_checks_no_size_limit() {
        let store = open_with(Options {
            max_record_size: Some(3),
            max_transaction_size: Some(3),
            ..Options::default()
        });
        let mut txn = store.tpc_begin_restore(Tid(100), b"", b"", b"").unwrap();
        txn.restore(Oid(1), Some(b"a copy over the limits"))
            .unwrap();
        txn.store(Oid(2), Tid::ZERO, b"a store over the limits")
            .unwrap();
        txn.vote().unwrap();
        assert_eq!(txn.finish().unwrap(), Tid(100));
        assert_eq!(store.load(Oid(1)).unwrap().0, b"a copy over the limits");
        assert_eq!(store.load(Oid(2)).unwrap().0, b"a store over the limits");

        // A normal transaction keeps the limits.
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        let error = txn.store(Oid(3), Tid::ZERO, b"abcd");
        assert!(matches!(error, Err(ZoeError::TooLarge { .. })), "{error:?}");
    }

    /// Return the duration, the lowest oid and the tid of each
    /// `transaction-warn` event of `events`.
    fn slow_transactions(events: &[Captured]) -> Vec<[Option<u64>; 3]> {
        events
            .iter()
            .filter(|e| e.message().contains("transaction-warn"))
            .map(|e| [e.number("duration_ms"), e.number("oid"), e.number("tid")])
            .collect()
    }

    /// Open a store in memory with `options` and a clock that the test
    /// moves.
    fn open_with_clock(options: Options) -> (Store, ManualClock) {
        let clock = ManualClock::new();
        let store = Store::open_full(MemVfs::new(), clock.clone(), options).unwrap();
        (store, clock)
    }

    #[test]
    fn each_end_of_a_transaction_is_logged_once() {
        let (store, clock) = open_with_clock(Options {
            transaction_warn: Some(Duration::from_secs(1)),
            ..Options::default()
        });
        let (tid, events) = events_of(|| {
            let mut finished = store.tpc_begin(b"", b"", b"").unwrap();
            finished.store(Oid(9), Tid::ZERO, b"a").unwrap();
            finished.store(Oid(1), Tid::ZERO, b"a").unwrap();
            finished.vote().unwrap();
            clock.advance(Duration::from_millis(1_100));
            let tid = finished.finish().unwrap();

            let mut aborted = store.tpc_begin(b"", b"", b"").unwrap();
            aborted.store(Oid(2), Tid::ZERO, b"b").unwrap();
            aborted.vote().unwrap();
            clock.advance(Duration::from_millis(1_200));
            aborted.abort();

            let dropped = store.tpc_begin(b"", b"", b"").unwrap();
            clock.advance(Duration::from_millis(1_300));
            drop(dropped);

            // A finish that fails ends the transaction too.
            let mut failed = store.tpc_begin(b"", b"", b"").unwrap();
            failed.store(Oid(3), Tid::ZERO, b"c").unwrap();
            clock.advance(Duration::from_millis(1_400));
            assert!(failed.finish().is_err());
            tid
        });
        // Only a commit has a tid, and only a transaction that writes has
        // an oid: the lowest one.
        assert_eq!(
            slow_transactions(&events),
            [
                [Some(1_100), Some(1), Some(tid.0)],
                [Some(1_200), Some(2), None],
                [Some(1_300), None, None],
                [Some(1_400), Some(3), None],
            ],
            "{events:?}"
        );
    }

    /// A clock that moves on by `step` at each read.
    struct StepClock {
        now: std::sync::Mutex<std::time::SystemTime>,
        step: Duration,
    }

    impl crate::clock::Clock for StepClock {
        fn now(&self) -> std::time::SystemTime {
            let mut now = self.now.lock().unwrap();
            *now += self.step;
            *now
        }
    }

    #[test]
    fn the_locks_that_a_lock_wait_timeout_releases_are_logged() {
        let step = Duration::from_secs(2);
        let clock = StepClock {
            now: std::sync::Mutex::new(std::time::UNIX_EPOCH),
            step,
        };
        // A zero lock timeout turns every wait into a timeout.
        let options = Options {
            lock_timeout: Duration::ZERO,
            lock_warn: Some(Duration::from_secs(1)),
            ..Options::default()
        };
        let store = Store::open_full(MemVfs::new(), clock, options).unwrap();
        let (low, high) = (Oid(1), Oid(2));
        let mut holder = store.tpc_begin(b"", b"", b"").unwrap();
        holder.store(high, Tid::ZERO, b"a").unwrap();
        holder.vote().unwrap();

        // The vote takes `low`, then times out on `high`. It reads the
        // clock when it takes `low` and after it releases `low`, so the
        // hold lasts one step.
        let ((), events) = events_of(|| {
            let mut waiter = store.tpc_begin(b"", b"", b"").unwrap();
            waiter.check_current_serial(low, Tid::ZERO).unwrap();
            waiter.store(high, Tid::ZERO, b"b").unwrap();
            let error = waiter.vote();
            assert!(
                matches!(error, Err(ZoeError::Conflict { oid }) if oid == high),
                "{error:?}"
            );
        });
        let logged: Vec<_> = events
            .iter()
            .filter(|e| e.message().contains("lock-warn"))
            .map(|e| (e.number("oid"), e.number("duration_ms")))
            .collect();
        let step = Some(step.as_millis() as u64);
        assert_eq!(logged, [(Some(low.0), step), (None, None)], "{events:?}");
        holder.abort();
        assert!(store.lock_table_is_empty());
    }

    #[test]
    fn a_lock_taken_after_a_wait_is_held_from_its_grant() {
        let (store, clock) = open_with_clock(Options {
            lock_warn: Some(Duration::from_secs(1)),
            ..Options::default()
        });
        let (low, high) = (Oid(1), Oid(2));
        let mut holder = store.tpc_begin(b"", b"", b"").unwrap();
        holder.store(high, Tid::ZERO, b"a").unwrap();
        holder.vote().unwrap();

        let ((), events) = events_of(|| {
            let waiter = thread::scope(|scope| {
                // The waiter takes `low`, then waits for `high`.
                let waiter = scope.spawn(|| {
                    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
                    txn.check_current_serial(low, Tid::ZERO).unwrap();
                    txn.store(high, Tid::ZERO, b"b").unwrap();
                    txn.vote().unwrap();
                    txn
                });
                while store.locks.waiting(high) == 0 {
                    thread::sleep(Duration::from_millis(1));
                }
                clock.advance(Duration::from_millis(3_000));
                holder.abort();
                waiter.join().unwrap()
            });
            clock.advance(Duration::from_millis(500));
            waiter.finish().unwrap();
        });
        let logged: Vec<_> = events
            .iter()
            .filter(|e| e.message().contains("lock-warn") && e.number("oid").is_some())
            .map(|e| (e.number("oid"), e.number("duration_ms")))
            .collect();
        // The waiter held `low` while it waited, and `high` only from the
        // abort of the holder.
        assert_eq!(
            logged,
            [(Some(high.0), Some(3_000)), (Some(low.0), Some(3_500))],
            "{events:?}"
        );
        assert!(store.lock_table_is_empty());
    }

    /// The size checks read the length of a spilled pickle, never its
    /// bytes. So a damaged spill file does not change them.
    #[test]
    fn the_size_checks_read_no_spilled_pickle() {
        let vfs = MemVfs::new();
        let options = Options {
            staging_memory: 0,
            max_transaction_size: Some(6),
            ..Options::default()
        };
        let store = Store::open_full(vfs.clone(), SystemClock, options).unwrap();
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.store(Oid(1), Tid::ZERO, b"abcd").unwrap();
        assert!(spills(&vfs));
        let name = &Vfs::list_files(&vfs, "staging").unwrap()[0];
        vfs.file(&format!("staging/{name}"))
            .write(0, b"xxxx")
            .unwrap();

        // The spilled pickles of `Oid(1)` count once, with their length.
        txn.store(Oid(1), Tid::ZERO, b"abcdef").unwrap();
        let error = txn.store(Oid(2), Tid::ZERO, b"g");
        assert!(
            matches!(
                error,
                Err(ZoeError::TooLarge {
                    size: 7,
                    max: 6,
                    ..
                })
            ),
            "{error:?}"
        );
        txn.store(Oid(1), Tid::ZERO, b"abcde").unwrap();
        txn.store(Oid(2), Tid::ZERO, b"g").unwrap();
        txn.abort();
        assert!(!spills(&vfs));
    }

    /// A pickle of a `persistent.mapping.PersistentMapping` record, as ZODB
    /// writes it: the class pickle, then the state pickle.
    const MAPPING: &[u8] = b"\x80\x03X\x12\x00\x00\x00persistent.mappingq\x00\
        X\x11\x00\x00\x00PersistentMappingq\x01\x86q\x02N\x86q\x03.\x80\x03}q\x00.";

    #[test]
    fn delete_writes_a_deletion_with_the_serial_check_of_store() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let (kept, gone) = (Oid(1), Oid(2));
        let first = commit_all(&store, &[(kept, Tid::ZERO, b"k"), (gone, Tid::ZERO, b"g")]);

        // A stale serial fails the vote with the serials of a conflict.
        let mut stale = store.tpc_begin(b"", b"", b"").unwrap();
        stale.delete(gone, Tid::ZERO).unwrap();
        assert!(matches!(stale.vote(), Err(ZoeError::Conflict { oid }) if oid == gone));
        assert_eq!(stale.conflict_serials(), Some((first, Tid::ZERO)));
        assert!(store.lock_table_is_empty());

        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.delete(gone, first).unwrap();
        assert_eq!(
            txn.staged(gone).unwrap(),
            Some((first, Cow::Borrowed(&b""[..])))
        );
        txn.vote().unwrap();
        let deleted = txn.finish().unwrap();

        // The deletion is a record with an empty payload after `first`.
        assert!(matches!(store.load(gone), Err(ZoeError::NotFound { .. })));
        let record = store.backend.load_before(gone, Tid::MAX).unwrap().unwrap();
        assert_eq!((record.tid, record.prev), (deleted, first));
        assert!(record.data.is_empty());
        let before = store.load_before(gone, deleted).unwrap().unwrap();
        assert_eq!((before.data, before.serial), (b"g".to_vec(), first));
        let sizes: Vec<_> = store
            .history(gone, 10)
            .unwrap()
            .iter()
            .map(|entry| (entry.tid, entry.size))
            .collect();
        assert_eq!(sizes, vec![(deleted, 0), (first, 1)]);
        let current = store.record_iternext(None).unwrap().unwrap();
        assert_eq!((current.oid, current.next), (kept, None));
        assert_eq!(store.load(kept).unwrap(), (b"k".to_vec(), first));

        // A store after the deletion reads the serial of the deletion.
        let again = commit(&store, gone, deleted, b"h");
        assert_eq!(store.load(gone).unwrap(), (b"h".to_vec(), again));
    }

    #[test]
    fn delete_in_a_restore_transaction_checks_the_serial() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let first = commit(&store, Oid(1), Tid::ZERO, b"a");
        let mut stale = store
            .tpc_begin_restore(Tid(first.0 + 1), b"", b"", b"")
            .unwrap();
        stale.delete(Oid(1), Tid::ZERO).unwrap();
        assert!(matches!(stale.vote(), Err(ZoeError::Conflict { .. })));
        drop(stale);

        let tid = Tid(first.0 + 2);
        let mut txn = store.tpc_begin_restore(tid, b"", b"", b"").unwrap();
        txn.delete(Oid(1), first).unwrap();
        txn.vote().unwrap();
        assert_eq!(txn.finish().unwrap(), tid);
        assert!(matches!(store.load(Oid(1)), Err(ZoeError::NotFound { .. })));
    }

    #[test]
    fn a_deletion_replaces_a_staged_pickle_and_drops_its_blob() {
        let store = open_with(Options {
            max_transaction_size: Some(4),
            ..Options::default()
        });
        let first = commit(&store, Oid(1), Tid::ZERO, b"a");
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        let mut file = store.new_blob_file().unwrap();
        file.write_all(b"blob").unwrap();
        txn.store_blob(Oid(1), first, b"abcd", file).unwrap();
        txn.delete(Oid(1), first).unwrap();
        // The deleted pickle no longer counts toward the transaction size.
        txn.store(Oid(2), Tid::ZERO, b"efgh").unwrap();
        txn.vote().unwrap();
        let tid = txn.finish().unwrap();

        assert!(matches!(store.load(Oid(1)), Err(ZoeError::NotFound { .. })));
        assert!(store.open_blob(Oid(1), tid).is_err());
        assert_eq!(store.load(Oid(2)).unwrap(), (b"efgh".to_vec(), tid));
    }

    #[test]
    fn a_deletion_is_never_resolved() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let first = commit(&store, Oid(1), Tid::ZERO, b"a");
        let second = commit(&store, Oid(1), first, b"b");
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.delete(Oid(1), first).unwrap();
        let error = txn.vote_resolving();
        assert!(
            matches!(error, Err(ZoeError::Conflict { oid: Oid(1) })),
            "{error:?}"
        );
        assert_eq!(txn.conflict_serials(), Some((second, first)));
        assert!(store.lock_table_is_empty());
    }

    #[test]
    fn delete_is_refused_out_of_order_from_another_thread_and_on_the_reserved_oid() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let first = commit(&store, Oid(1), Tid::ZERO, b"a");
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        let reserved = txn.delete(Oid(u64::MAX), Tid::ZERO);
        assert!(
            matches!(reserved, Err(ZoeError::Protocol(_))),
            "{reserved:?}"
        );
        thread::scope(|scope| {
            scope.spawn(|| {
                let refused = txn.delete(Oid(1), first);
                assert!(matches!(refused, Err(ZoeError::WrongThread)), "{refused:?}");
            });
        });
        // The refused calls staged nothing.
        assert_eq!(txn.staged(Oid(1)).unwrap(), None);
        txn.store(Oid(2), Tid::ZERO, b"b").unwrap();
        txn.vote().unwrap();
        let after = txn.delete(Oid(1), first);
        assert!(matches!(after, Err(ZoeError::Protocol(_))), "{after:?}");
        txn.finish().unwrap();
        assert_eq!(store.load(Oid(1)).unwrap(), (b"a".to_vec(), first));
    }

    #[test]
    fn staged_class_name_reads_the_class_pickle() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.store(Oid(1), Tid::ZERO, MAPPING).unwrap();
        txn.store(Oid(2), Tid::ZERO, b"not a pickle").unwrap();
        txn.delete(Oid(3), Tid::ZERO).unwrap();
        assert_eq!(
            txn.staged_class_name(Oid(1)).as_deref(),
            Some("persistent.mapping.PersistentMapping")
        );
        for oid in [Oid(2), Oid(3), Oid(4)] {
            assert_eq!(txn.staged_class_name(oid), None, "{oid:?}");
        }
    }
}
