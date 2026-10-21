//! The vote. It takes the per-oid locks in ascending oid order, then
//! checks the serials with the locks held (`VoteCheck`). The vote that
//! takes the locks runs the tiers and the plans of the split merge and of
//! the order merge.

use std::collections::BTreeMap;
use std::ops::Deref;
use std::sync::Arc;
use std::time::Duration;

use super::group;
use super::observer::Stopwatch;
use super::split::adopt;
use super::tier::Tier;
use super::{READ_CHUNK, Shadowed, Txn, order, split, staging};
use crate::backend::Record;
use crate::error::{Result, ZoeError};
use crate::guard::SizeLimit;
use crate::lock::{Handoff, Held, InFlight, Mode, Refused, Waited};
use crate::merge::policy::{Policy, Rules};
use crate::merge::{self, Merge, Outcome, Resolution, Resolved, Verification};
use crate::store::Store;
use crate::types::{Oid, Tid};

/// What the serial check of a vote found.
enum Check {
    /// No serial fails the vote. Each stale written oid can be resolved:
    /// the check gives it with its committed record, or a tier merged it.
    /// The check staged the merged records.
    Passed(Vec<(Oid, Record)>),
    /// A stale serial fails the vote.
    Failed(Stale),
}

/// An oid whose current serial is not the serial that the transaction
/// read.
pub(super) struct Stale {
    oid: Oid,
    /// The current serial. It is `None` when the current record is the
    /// record of a commit in flight that has no tid yet.
    current: Option<Tid>,
    read: Tid,
    /// True when the transaction read the oid as current and does not
    /// write it.
    read_only: bool,
}

impl Stale {
    /// Return a stale written oid.
    pub(super) fn written(oid: Oid, current: Tid, read: Tid) -> Self {
        Stale {
            oid,
            current: Some(current),
            read,
            read_only: false,
        }
    }

    /// Return a stale read-current oid.
    fn read_current(oid: Oid, current: Option<Tid>, read: Tid) -> Self {
        Stale {
            oid,
            current,
            read,
            read_only: true,
        }
    }
}

/// A staged record as the vote reads it. It holds no pickle.
#[derive(Clone, Copy)]
struct Write {
    /// The serial that the client read.
    serial: Tid,
    /// True when `restore` staged the record.
    restored: bool,
    /// The length of the pickle: 0 for a deletion.
    len: usize,
}

impl Write {
    fn of(staged: &staging::Staged) -> Self {
        Write {
            serial: staged.serial,
            restored: staged.restored,
            len: staged.pickle_len(),
        }
    }
}

impl<S: Deref<Target = Store>> Txn<S> {
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
    /// `ReadConflict`, unless rule 1 of the split merge lets it through: a
    /// read-current oid is never resolved. A failed vote releases every
    /// lock that it took.
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

    /// Take the locks of the vote, and keep them until `finish`, `abort` or
    /// a failed vote. A lock timeout releases the locks that the call took,
    /// and logs each one held past `lock-warn`. A read-only store fails
    /// with `ReadOnly` and gives no lock.
    fn lock(&mut self) -> Result<()> {
        self.store.writable()?;
        let store: &Store = &self.store;
        let mut requests = self.lock_requests();
        let watch = Stopwatch::start(store);
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
                    store.log_released(&released);
                    let above = self.intents.partition_point(|lock| lock.oid < lowest);
                    let given: Vec<Held> = self.intents.split_off(above);
                    store.release_locks(&given);
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
        let waited = watch.elapsed();
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
                store.log_released(&released);
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
            self.dropping.clear();
            self.unstored.clear();
            // The entries of the undos come first, and they stay.
            self.resolutions.truncate(self.undo_resolutions);
            self.shadows.clear();
            self.verifications.clear();
            self.passed.clear();
        }
        let checked = self
            .check_undone()
            .and_then(|()| VoteCheck::new(resolve, native, self.stored_bytes).run(self));
        let error = match checked {
            Ok(Check::Passed(conflicts)) => return Ok(conflicts),
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
        mark_applied(
            &mut self.resolutions[self.undo_resolutions..],
            |resolution| {
                resolution.outcome == Resolved::Merged
                    && merged.iter().any(|(oid, _, _)| *oid == resolution.oid)
            },
        );
        Ok(())
    }

    /// Take each stored oid that the split merge resolves without a record
    /// out of the staging, with its blob, and mark its entry as applied.
    /// No batch writes it, the transaction index does not list it, and the
    /// finish never hands its lock off. The vote keeps its lock until the
    /// end of the transaction.
    fn drop_resolved(&mut self) {
        let dropping = std::mem::take(&mut self.dropping);
        if dropping.is_empty() {
            return;
        }
        for &oid in &dropping {
            self.staged.remove(oid);
            self.blobs.remove(&oid);
        }
        mark_applied(
            &mut self.resolutions[self.undo_resolutions..],
            |resolution| {
                resolution.outcome == Resolved::Merged && dropping.contains(&resolution.oid)
            },
        );
    }

    /// Return the committed record of `oid` when it is the record at
    /// `current` and holds a pickle. A deletion has an empty pickle and is
    /// never resolved. A record that is gone since the serials were read is
    /// a pack of it. Both give `None`: the vote then fails as it does for a
    /// stale serial.
    fn committed_record(&self, oid: Oid, current: Tid) -> Result<Option<Record>> {
        Ok(self
            .store
            .backend
            .load_before(oid, Tid::MAX)?
            .filter(|record| record.tid == current && !record.data.is_empty()))
    }

    /// Add `waited` to the lock wait of the vote, when the observer
    /// measures it.
    pub(super) fn add_lock_wait(&mut self, waited: Duration) {
        if let Some(total) = &mut self.lock_waited {
            *total += waited;
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
    pub(super) fn merged_size(&self, oid: Oid, pickle: &[u8], stored_bytes: u64) -> Option<u64> {
        self.merged_size_replacing(oid, pickle, stored_bytes, None)
    }

    /// Return the transaction size after `pickle` replaces a pickle of
    /// `oid` of `replaced` bytes in a transaction of `stored_bytes`, as
    /// `merged_size` does. `None` for `replaced` is the staged pickle of
    /// `oid`; a merge that a later merge replaces gives its own length.
    pub(super) fn merged_size_replacing(
        &self,
        oid: Oid,
        pickle: &[u8],
        stored_bytes: u64,
        replaced: Option<u64>,
    ) -> Option<u64> {
        if self.is_restore() {
            return Some(stored_bytes);
        }
        let guardrails = &self.store.guardrails;
        let size = pickle.len() as u64;
        guardrails.check(SizeLimit::Record, oid, size).ok()?;
        let replaced = replaced.unwrap_or_else(|| {
            self.staged
                .record(oid)
                .map_or(0, |staged| staged.pickle_len() as u64)
        });
        let total = stored_bytes.saturating_sub(replaced).saturating_add(size);
        guardrails.check(SizeLimit::Transaction, oid, total).ok()?;
        Some(total)
    }

    /// Return the current serial of each oid of `oids`, in order, from one
    /// backend read. A backend that gives another number of serials fails
    /// with a backend error, so the vote never skips an oid.
    pub(super) fn current_serials(
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
    pub(super) fn current_serial(&self, oid: Oid) -> Result<Tid> {
        Ok(self
            .store
            .backend
            .load_before(oid, Tid::MAX)?
            .map_or(Tid::ZERO, |record| record.tid))
    }
}

/// Mark as applied each entry of `entries` that `applies` selects.
fn mark_applied(entries: &mut [Resolution], applies: impl Fn(&Resolution) -> bool) {
    for resolution in entries {
        if applies(resolution) {
            resolution.applied = true;
        }
    }
}

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

/// The serial check of one vote, with the locks held. It compares the
/// serial that the client read of each oid with its current serial: the
/// written oids first, then the read-current oids. Then it renumbers the
/// orders that the order merge merges.
///
/// The first stale oid fails the check, unless a tier merges it, or
/// `resolve` is true and the oid is a written oid that the caller can
/// resolve. The check gives each such oid with its committed record. When
/// `native` is true, the tiers run on each stale written oid that the
/// caller could resolve, whether `resolve` is true or not, and their
/// entries go to the resolutions of the transaction.
///
/// The first conflict reads the merge policy, once: a vote without a
/// conflict reads nothing. The first conflict of a vote that takes the
/// locks makes the plan of the split merge (ADR 0048). The first conflict
/// of a written oid also makes the plan of the order merge (ADR 0052).
struct VoteCheck {
    /// True when the caller resolves each stale written oid that it can.
    resolve: bool,
    /// True for the vote that takes the locks: the tiers and the plans run.
    native: bool,
    /// The stale written oids that the caller resolves, with their
    /// committed records.
    conflicts: Vec<(Oid, Record)>,
    /// The merged records: the oid, the serial of the committed record and
    /// the merged pickle.
    merged: Vec<(Oid, Tid, Vec<u8>)>,
    /// The merged oids whose committed record is the record of a commit in
    /// flight, with that commit.
    onto: Vec<(Oid, Arc<InFlight>)>,
    /// The transaction size with the merged pickles.
    stored_bytes: u64,
    /// The merge policy, once the first conflict read it.
    policy: Option<Option<Arc<Policy>>>,
    /// The plan of the split merge, once the first conflict made it.
    plan: Option<split::Plan>,
    /// The plan of the order merge, once the first conflict of a written
    /// oid made it.
    orders: Option<order::Plan>,
}

/// What tier 1 or tier 2 did for a stale written oid of a vote.
enum Tiered {
    /// A tier merged the oid in mode `on`: the vote stages the merged
    /// record.
    Merged,
    /// A tier refused the merge in mode `on`: the vote fails on the oid.
    Refused,
    /// No tier decided: the conflict stays.
    Undecided,
}

impl VoteCheck {
    fn new(resolve: bool, native: bool, stored_bytes: u64) -> Self {
        VoteCheck {
            resolve,
            native,
            conflicts: Vec::new(),
            merged: Vec::new(),
            onto: Vec::new(),
            stored_bytes,
            policy: None,
            plan: None,
            orders: None,
        }
    }

    /// Check the serials of `txn`. When no serial fails the vote, stage the
    /// merged records.
    fn run<S: Deref<Target = Store>>(mut self, txn: &mut Txn<S>) -> Result<Check> {
        if let Some(stale) = self.check_written(txn)? {
            return Ok(Check::Failed(stale));
        }
        if let Some(stale) = self.check_read_current(txn)? {
            return Ok(Check::Failed(stale));
        }
        if let Some(stale) = self.check_orders(txn)? {
            return Ok(Check::Failed(stale));
        }
        txn.stage_merged(self.merged, self.stored_bytes)?;
        txn.drop_resolved();
        txn.onto.extend(self.onto);
        // A node that rule 1 let through counts once the vote passes.
        mark_applied(&mut txn.resolutions[txn.undo_resolutions..], |resolution| {
            resolution.split == Some(1)
                && resolution.outcome == Resolved::Merged
                && txn.passed.contains(&resolution.oid)
        });
        Ok(Check::Passed(self.conflicts))
    }

    /// Return the merge policy of `store`. The first call reads it, and
    /// the next calls give the same.
    fn policy(&mut self, store: &Store) -> Result<Option<&Policy>> {
        if self.policy.is_none() {
            self.policy = Some(store.merge_policy()?);
        }
        Ok(self.policy.as_ref().and_then(Option::as_deref))
    }

    /// Return the settings of the vote: the configuration of `store`, and
    /// the merge policy once a conflict read it.
    fn rules<'r>(&'r self, store: &'r Store) -> Rules<'r> {
        Rules {
            config: &store.structural_merges,
            policy: self.policy.as_ref().and_then(Option::as_deref),
        }
    }

    /// Make the plan of the split merge at the first conflict of the vote,
    /// after a read of the merge policy, and take it into the check. Return
    /// the stale node of a refusal, which fails the vote.
    fn split_plan<S: Deref<Target = Store>>(&mut self, txn: &mut Txn<S>) -> Result<Option<Stale>> {
        self.policy(&txn.store)?;
        if self.plan.is_some() {
            return Ok(None);
        }
        let mut plan = txn.split_plan(&self.rules(&txn.store), self.stored_bytes)?;
        let refused = adopt(&mut plan, txn, &mut self.merged, &mut self.stored_bytes);
        if refused.is_none() {
            self.plan = Some(plan);
        }
        Ok(refused)
    }

    /// Make the plan of the order merge at the first conflict of a written
    /// oid. Return the order of a refusal, which fails the vote.
    fn order_plan<S: Deref<Target = Store>>(&mut self, txn: &mut Txn<S>) -> Result<Option<Stale>> {
        if self.orders.is_some() {
            return Ok(None);
        }
        let plan = txn.order_plan(&self.rules(&txn.store))?;
        if let Some((oid, current, read, entry)) = plan.refused {
            txn.resolutions.push(entry);
            return Ok(Some(Stale::written(oid, current, read)));
        }
        self.orders = Some(plan);
        Ok(None)
    }

    /// Check the serial of each written oid. Return the stale oid that
    /// fails the vote.
    fn check_written<S: Deref<Target = Store>>(
        &mut self,
        txn: &mut Txn<S>,
    ) -> Result<Option<Stale>> {
        // One backend read serves a chunk of oids. The chunk bounds the
        // committed records in memory.
        let staged: Vec<(Oid, Write)> = txn
            .staged
            .iter()
            .map(|(oid, staged)| (oid, Write::of(staged)))
            .collect();
        for chunk in staged.chunks(READ_CHUNK) {
            let serials = txn.current_serials(chunk.iter().map(|(oid, _)| *oid))?;
            for (&(oid, staged), serial) in chunk.iter().zip(serials) {
                let current = serial.unwrap_or(Tid::ZERO);
                if let Some(stale) = self.check_written_oid(txn, oid, staged, current)? {
                    return Ok(Some(stale));
                }
            }
        }
        Ok(None)
    }

    /// Check the written oid `oid`, of current serial `current` in the
    /// store. Return the oid when it is stale and fails the vote.
    fn check_written_oid<S: Deref<Target = Store>>(
        &mut self,
        txn: &mut Txn<S>,
        oid: Oid,
        staged: Write,
        current: Tid,
    ) -> Result<Option<Stale>> {
        let mut current = current;
        // The store does not hold the commit in flight of a lock that came
        // with a hand-off: its staged record is the committed record.
        if let Some(handoff) = txn.handed.get(&oid).cloned() {
            match self.check_handed(txn, oid, staged, &handoff)? {
                Handed::Passed => return Ok(None),
                Handed::Merged {
                    pickle,
                    size,
                    resolution,
                } => {
                    self.stored_bytes = size;
                    // The serial is a placeholder: the finish writes the
                    // tid of the commit in flight. Another vote of the oid
                    // without the hand-off fails on it.
                    self.merged.push((oid, Tid::ZERO, pickle));
                    self.onto.push((oid, Arc::clone(&handoff.commit)));
                    txn.resolutions.push(resolution);
                    return Ok(None);
                }
                Handed::Failed { stale, resolution } => {
                    txn.resolutions.extend(resolution);
                    return Ok(Some(stale));
                }
                Handed::Store { waited } => {
                    // The wait for the commit in flight counts in the lock
                    // wait of the vote.
                    txn.add_lock_wait(waited);
                    current = txn.current_serial(oid)?;
                }
                Handed::Late { waited } => {
                    txn.add_lock_wait(waited);
                    return Err(ZoeError::Conflict { oid });
                }
            }
        }
        if current == staged.serial {
            return Ok(None);
        }
        let stale = Stale::written(oid, current, staged.serial);
        if !(self.resolve || self.native)
            || staged.restored
            || staged.serial == Tid::ZERO
            || staged.len == 0
        {
            return Ok(Some(stale));
        }
        if self.native {
            if let Some(refused) = self.split_plan(txn)? {
                return Ok(Some(refused));
            }
            // The split merge staged the merged record of the oid.
            if self
                .plan
                .as_ref()
                .is_some_and(|plan| plan.covered.contains(&oid))
            {
                return Ok(None);
            }
            if let Some(refused) = self.order_plan(txn)? {
                return Ok(Some(refused));
            }
            if let Some(orders) = &self.orders {
                // The vote stages the merged order after the check of
                // every serial.
                if orders.covers(oid) {
                    return Ok(None);
                }
                if let Some(entry) = orders.lines.get(&oid) {
                    txn.resolutions.push(entry.clone());
                }
            }
        }
        // Only a conflict reads its committed record.
        let Some(record) = txn.committed_record(oid, current)? else {
            return Ok(Some(stale));
        };
        // Tier 2 never merges the order of a folder.
        let order = self
            .orders
            .as_ref()
            .is_some_and(|orders| orders.known.contains(&oid));
        if self.native && !order {
            match self.merge_tiers(txn, oid, staged.serial, current, &record.data)? {
                Tiered::Merged => return Ok(None),
                Tiered::Refused => return Ok(Some(stale)),
                Tiered::Undecided => {}
            }
        }
        if !self.resolve {
            return Ok(Some(stale));
        }
        self.conflicts.push((oid, record));
        Ok(None)
    }

    /// Run tier 1 or tier 2 on the stale written oid `oid`: the transaction
    /// read `read`, and `committed` is the pickle of the current record at
    /// `current`. A merge in mode `on` joins the merged records, and each
    /// outcome gives an entry.
    fn merge_tiers<S: Deref<Target = Store>>(
        &mut self,
        txn: &mut Txn<S>,
        oid: Oid,
        read: Tid,
        current: Tid,
        committed: &[u8],
    ) -> Result<Tiered> {
        let rules = self.rules(&txn.store);
        let Some((mode, merge)) = txn.native(oid, read, committed, &rules)? else {
            return Ok(Tiered::Undecided);
        };
        let version = rules.version();
        let mut resolution = Resolution::of(oid, mode, version, &merge, current, read);
        // In shadow mode, the entry tells what the split merge gives for
        // the oid.
        if let Some(shadow) = self.plan.as_ref().and_then(|plan| plan.shadows.get(&oid))
            && resolution.shadow.is_none()
        {
            resolution.shadow = Some(shadow.outcome);
            resolution.split = Some(shadow.rule);
        }
        if let (merge::Mode::Shadow, Outcome::Merged(pickle)) = (mode, &merge.outcome) {
            txn.shadows.insert(
                oid,
                Shadowed {
                    tier: merge.tier,
                    class: merge.class.clone(),
                    policy: version,
                    digest: blake3::hash(pickle),
                    committed: current,
                    read,
                },
            );
        }
        match (mode, merge.outcome) {
            (merge::Mode::On, Outcome::Merged(pickle)) => {
                match txn.merged_size(oid, &pickle, self.stored_bytes) {
                    Some(size) => {
                        self.stored_bytes = size;
                        self.merged.push((oid, current, pickle));
                        txn.resolutions.push(resolution);
                        return Ok(Tiered::Merged);
                    }
                    None => {
                        resolution.outcome =
                            Resolved::Undecided("a merged record over a size limit");
                    }
                }
            }
            (merge::Mode::On, Outcome::Refused(_)) => {
                resolution.applied = true;
                txn.resolutions.push(resolution);
                return Ok(Tiered::Refused);
            }
            _ => {}
        }
        txn.resolutions.push(resolution);
        Ok(Tiered::Undecided)
    }

    /// Check the serial of each read-current oid. Return the stale oid that
    /// fails the vote.
    fn check_read_current<S: Deref<Target = Store>>(
        &mut self,
        txn: &mut Txn<S>,
    ) -> Result<Option<Stale>> {
        let read_current: Vec<(Oid, Tid)> = txn
            .read_current
            .iter()
            .map(|(&oid, &read)| (oid, read))
            .collect();
        for chunk in read_current.chunks(READ_CHUNK) {
            let serials = txn.current_serials(chunk.iter().map(|&(oid, _)| oid))?;
            for (&(oid, read), serial) in chunk.iter().zip(serials) {
                // The last commit of an oid held with a hand-off can be in
                // flight: the store does not hold it yet.
                if let Some(handoff) = txn.handed.get(&oid)
                    && handoff.commit.ended().is_none()
                    && handoff.commit.tid() != Some(read)
                {
                    return Ok(Some(Stale::read_current(oid, handoff.commit.tid(), read)));
                }
                let current = serial.unwrap_or(Tid::ZERO);
                if current == read || txn.passed.contains(&oid) {
                    continue;
                }
                if self.native {
                    if let Some(refused) = self.split_plan(txn)? {
                        return Ok(Some(refused));
                    }
                    // Rule 1 of the split merge lets the node through.
                    if txn.passed.contains(&oid) {
                        continue;
                    }
                    // The entry tells why rule 1 did not let the node
                    // through, or what it gives in mode `shadow`.
                    if let Some(plan) = &self.plan {
                        let rules = self.rules(&txn.store);
                        if let Some(shadow) = plan.shadows.get(&oid) {
                            let entry = split::read_shadow(oid, shadow, read, current, &rules);
                            txn.resolutions.push(entry);
                        } else if let Some(&(ref class, why)) = plan.unanswered.get(&oid) {
                            let entry = split::undecided(oid, class, 1, why, read, current, &rules);
                            txn.resolutions.push(entry);
                        }
                    }
                }
                return Ok(Some(Stale::read_current(oid, Some(current), read)));
            }
        }
        Ok(None)
    }

    /// Renumber the orders that the order merge merges, after the check of
    /// every serial: the records of the positions that the vote stages,
    /// then the merged lists. Return the order of a merge without an
    /// answer when it fails the vote.
    fn check_orders<S: Deref<Target = Store>>(
        &mut self,
        txn: &mut Txn<S>,
    ) -> Result<Option<Stale>> {
        let Some(orders) = &self.orders else {
            return Ok(None);
        };
        if orders.merges.is_empty() {
            return Ok(None);
        }
        let policy = self.policy.clone().flatten();
        let failed = txn.finish_orders(
            orders,
            policy.as_deref(),
            &mut self.merged,
            &self.onto,
            &mut self.stored_bytes,
        )?;
        let Some((oid, current, read)) = failed else {
            return Ok(None);
        };
        let stale = Stale::written(oid, current, read);
        if !self.resolve {
            return Ok(Some(stale));
        }
        // The caller gets the order with its committed record.
        match txn.committed_record(oid, current)? {
            Some(record) => {
                self.conflicts.push((oid, record));
                Ok(None)
            }
            None => Ok(Some(stale)),
        }
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
    fn check_handed<S: Deref<Target = Store>>(
        &mut self,
        txn: &Txn<S>,
        oid: Oid,
        staged: Write,
        handoff: &Handoff,
    ) -> Result<Handed> {
        // The merge of the vote before, onto the same commit, stands.
        if txn.onto.contains_key(&oid) {
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
            || staged.len == 0
            || handoff.record.is_empty()
        {
            return Ok(Handed::Failed {
                stale,
                resolution: None,
            });
        }
        if self.native {
            // The first conflict of the vote reads the merge policy.
            self.policy(&txn.store)?;
        }
        let rules = self.rules(&txn.store);
        if self.native
            && let Some((mode @ merge::Mode::On, merge)) =
                txn.native(oid, staged.serial, &handoff.record, &rules)?
        {
            let committed = commit.tid().unwrap_or(Tid::ZERO);
            let mut resolution =
                Resolution::of(oid, mode, rules.version(), &merge, committed, staged.serial);
            match merge.outcome {
                Outcome::Merged(pickle) => {
                    if let Some(size) = txn.merged_size(oid, &pickle, self.stored_bytes) {
                        return Ok(Handed::Merged {
                            pickle,
                            size,
                            resolution,
                        });
                    }
                }
                // The split merge can merge a split that tier 1 refuses, once
                // the store holds the commit in flight: wait for it.
                Outcome::Refused(merge::Reason::BTrees(0 | 11))
                    if rules.split_merges() != merge::Mode::Off => {}
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
        let store: &Store = &txn.store;
        let watch = Stopwatch::start(store);
        let outcome = commit.wait(
            oid,
            txn.lock_deadline,
            &*store.clock,
            txn.lock_cancel.as_ref(),
        );
        let waited = watch.elapsed().unwrap_or_default();
        match outcome {
            Waited::Durable | Waited::Failed => Ok(Handed::Store { waited }),
            Waited::TimedOut | Waited::Cancelled => Ok(Handed::Late { waited }),
        }
    }
}
