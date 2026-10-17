//! Early write-intent locks (ADR 0045).
//!
//! An intent owner takes exclusive locks before the vote of its
//! transaction, before the transaction reads the oids. A client takes them
//! when it loads an object of a hot class, so a second writer waits before
//! it reads, and reads the commit of the first one.
//!
//! The locks of one owner come in oid order: a request for an oid at or
//! below an oid that the owner holds fails with `LockOrder` at once, and
//! takes nothing. A request waits in the queue of each oid, as a vote
//! waits, within the lock timeout of the store. A timeout or a cancel fails
//! it with `Conflict`, and the request keeps no lock that it took. An
//! intent request never takes a lock hand-off: it waits until the store
//! holds the last commit of the oid, so the current serial that it gives
//! back is the serial of that commit.
//!
//! `Txn::adopt_intents` gives the locks of an owner to a transaction before
//! its vote. The vote counts them as its own. The drop of the owner, and
//! `release`, release the locks that it still holds.
//!
//! The locks are advisory: the vote still checks every serial. So a holder
//! that drops its locks makes nothing wrong.

use std::ops::Deref;
use std::time::{Duration, SystemTime};

use crate::error::{Result, ZoeError};
use crate::lock::{Held, LockCancel, Mode, Refused};
use crate::store::Store;
use crate::types::{Oid, Tid};

/// The early write-intent locks of one owner of a store.
///
/// `S` is how the owner reaches its store, as for `Txn`. Dropping the
/// owner releases its locks.
pub struct IntentLocks<S: Deref<Target = Store>> {
    store: S,
    /// The locks that the owner holds, in ascending oid order.
    held: Vec<Held>,
    /// The handle that ends a wait from another thread.
    lock_cancel: Option<LockCancel>,
}

impl<S: Deref<Target = Store>> IntentLocks<S> {
    /// Return an owner of `store` that holds no lock.
    pub fn new(store: S) -> Self {
        Self {
            store,
            held: Vec::new(),
            lock_cancel: None,
        }
    }

    /// Let `cancel` end a wait of `lock` from another thread, as
    /// `Txn::set_lock_cancel` does for a vote.
    pub fn set_lock_cancel(&mut self, cancel: LockCancel) {
        self.lock_cancel = Some(cancel);
    }

    /// Take the intent lock of each oid of `oids`, and return the current
    /// serial of each one, in the order of `oids`: `Tid::ZERO` for an oid
    /// that has no record.
    ///
    /// The oids must be in strictly ascending order, and above every oid
    /// that the owner holds. Otherwise the call fails with `LockOrder` and
    /// takes nothing. The call waits for a lock that another request holds,
    /// at most the lock timeout of the store. A longer wait, or a cancel,
    /// fails with `Conflict` on the oid that it waits for, and the call
    /// keeps no lock that it took; so does a failed read of the serials. A
    /// read-only store fails with `ReadOnly`.
    pub fn lock(&mut self, oids: &[Oid]) -> Result<Vec<Tid>> {
        self.store.writable()?;
        let Some(&first) = oids.first() else {
            return Ok(Vec::new());
        };
        if self.held.last().is_some_and(|last| last.oid >= first) {
            return Err(ZoeError::LockOrder { oid: first });
        }
        let store: &Store = &self.store;
        let requests: Vec<(Oid, Mode)> = oids.iter().map(|&oid| (oid, Mode::Intent)).collect();
        let started = store.observer().map(|_| store.clock.monotonic());
        let acquired = store.locks.acquire_cancellable(
            &requests,
            store.lock_timeout,
            &*store.clock,
            self.lock_cancel.as_ref(),
        );
        let wait = started.map(|started| store.clock.monotonic().saturating_sub(started));
        let locks = match acquired {
            Ok(locks) => locks.keep(),
            Err(Refused { error, released }) => {
                store
                    .guardrails
                    .locks_released(&released, store.clock.now());
                if let (Some(observer), Some(wait)) = (store.observer(), wait) {
                    observer.intent_locks_acquired(0, wait);
                }
                return Err(error);
            }
        };
        // The intent requests waited until the store held the last commit
        // of each oid, so the store gives its serial. A failed read keeps
        // no lock of the call, and counts none.
        let serials = match store.backend.current_serials(&mut oids.iter().copied()) {
            Ok(serials) => serials,
            Err(error) => {
                store.locks.release(&locks);
                store.guardrails.locks_released(&locks, store.clock.now());
                if let (Some(observer), Some(wait)) = (store.observer(), wait) {
                    observer.intent_locks_acquired(0, wait);
                }
                return Err(error);
            }
        };
        if let (Some(observer), Some(wait)) = (store.observer(), wait) {
            observer.intent_locks_acquired(locks.len(), wait);
        }
        self.held.extend(locks);
        Ok(serials
            .into_iter()
            .map(|serial| serial.unwrap_or(Tid::ZERO))
            .collect())
    }

    /// Release every lock of the owner. The owner can take locks again.
    pub fn release(&mut self) {
        if self.held.is_empty() {
            return;
        }
        let held = std::mem::take(&mut self.held);
        let now = self.store.clock.now();
        self.store.locks.release(&held);
        self.store.guardrails.locks_released(&held, now);
    }

    /// Return the oids that the owner holds, in ascending order.
    pub fn oids(&self) -> Vec<Oid> {
        self.held.iter().map(|lock| lock.oid).collect()
    }

    /// Tell if the owner holds no lock.
    pub fn is_empty(&self) -> bool {
        self.held.is_empty()
    }

    /// Return how long the owner holds its oldest lock on the store clock,
    /// or `None` when it holds none.
    pub fn held_for(&self) -> Option<Duration> {
        let since = self.held.iter().map(|lock| lock.since).min()?;
        let now: SystemTime = self.store.clock.now();
        Some(now.duration_since(since).unwrap_or_default())
    }

    /// Return the store of the owner.
    pub(super) fn store(&self) -> &Store {
        &self.store
    }

    /// Take the locks out of the owner, for a transaction that adopts them.
    pub(super) fn take(&mut self) -> Vec<Held> {
        std::mem::take(&mut self.held)
    }
}

impl<S: Deref<Target = Store>> Drop for IntentLocks<S> {
    fn drop(&mut self) {
        self.release();
    }
}
