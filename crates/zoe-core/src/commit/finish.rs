//! The end of a transaction. `finish` commits it in a commit batch.
//! `abort` and a drop release its locks.

use std::ops::Deref;
use std::sync::Arc;
use std::time::SystemTime;

use super::group::Member;
use super::{Finished, Txn};
use crate::error::{Result, ZoeError};
use crate::guard::Ended;
use crate::lock::{Handoff, Held, InFlight};
use crate::merge;
use crate::merge::policy::Rules;
use crate::store::Store;
use crate::types::{Oid, Tid};

impl<S: Deref<Target = Store>> Txn<S> {
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
        // The transaction takes its staged records and its blobs back, so
        // `Drop` removes its spill file and its unplaced blob files.
        if let Some(member) = member {
            self.staged = member.staged;
            self.blobs = member.blobs;
        }
        // The commit is durable, and it comes after the commit of each
        // hand-off that the vote got. So the store holds the last commit of
        // these oids, also when their holder did not settle yet.
        if finished.is_ok() && !self.handed.is_empty() {
            store
                .locks
                .settle(self.handed.iter().map(|(oid, handoff)| (*oid, handoff)));
        }
        // The commit is durable or failed here. The finish releases the
        // locks that it did not hand off before the end of the commit wakes
        // the votes that wait for it, so a woken vote finds them free. The
        // release reads no clock.
        let mut own = self.locks.take().unwrap_or_default();
        own.append(&mut self.intents);
        store.locks.release(&own);
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
        if !own.is_empty() {
            store.guardrails.locks_released(&own, store.clock.now());
        }
        let finished = finished?;
        self.committed = Some(finished.tid);
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
    pub(super) fn release(&mut self) {
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

/// Return the tid of a normal commit after the tid `last`: `now`, or the
/// tid after `last` if `now` is not after it. `Tid::MAX` is never given,
/// because `load` reads the versions before it. When no tid is left, fail
/// with `Protocol`.
pub(super) fn next_tid(now: Tid, last: Tid) -> Result<Tid> {
    match last.0.checked_add(1) {
        Some(after) if after < Tid::MAX.0 => Ok(now.max(Tid(after)).min(Tid(Tid::MAX.0 - 1))),
        _ => Err(ZoeError::Protocol("no tid is left after the last tid")),
    }
}

/// The hand-offs that a finish can make: each oid with its hand-off.
type Handoffs = Vec<(Oid, Arc<Handoff>)>;
