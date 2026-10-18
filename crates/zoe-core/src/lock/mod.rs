//! The per-OID lock manager.
//!
//! A vote takes an exclusive lock on each oid that its transaction writes
//! and a shared lock on each other oid that it reads as current. A shared
//! lock is compatible only with shared locks. A vote takes its locks in
//! ascending oid order, so two transactions cannot deadlock on each other.
//!
//! Each oid has a wait queue in arrival order. A lock request waits behind
//! each earlier request on the oid: it never passes a request that waits.
//! A vote that must wait keeps the locks that it holds and waits in one
//! queue at a time. Its waits together last at most the lock timeout. At
//! the timeout, the vote leaves the queue, releases the locks that it took,
//! and fails with a conflict on the oid that it waits for. The error gives
//! back the released locks, so the caller can log them.
//!
//! One mutex protects the lock table. A thread that releases a lock grants
//! it to the waiters at the head of the queue, under the mutex. Then it
//! releases the mutex and wakes them. So a waiter holds its lock before it
//! wakes up, and no new request can take the lock first.
//!
//! Each lock that a call takes keeps the time when the call took it, on
//! the store clock. `lock-warn` measures the hold time from it. The
//! timeout is measured on the monotonic time of the store clock, and a
//! call waits on a waiter of the store clock. So a simulator runs the
//! waits on simulated time. The lock manager never calls the store clock
//! or a waiter under the mutex, so a slow clock stops no other request, and
//! a clock that panics leaks no lock.
//!
//! A `LockCancel` ends a lock wait from another thread. The vote then
//! leaves the queue, releases the locks that it took, and fails with a
//! conflict on the oid that it waits for, as at the timeout. A cancel
//! stays: each later wait on the same `LockCancel` fails at once. A
//! cancel takes no mutex of the lock manager, and wakes the waiter
//! without it.
//!
//! Each wait for the lock of an oid runs in a `tracing` span `lock_wait`,
//! with the field `oid`. The call opens and closes it without the mutex.
//! The span ends at the grant, the timeout or the cancel.
//!
//! When the finish of a holder starts, the holder can hand the lock of an
//! oid that it writes to the exclusive request at the head of the queue of
//! the oid (`handoff`, ADR 0044). The entry keeps the hand-off until the
//! store holds the last commit of the oid (`settle`). Until then, only an
//! exclusive request takes the lock, and the call that takes it gets the
//! hand-off.

mod handoff;

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime};

use crate::clock::{Clock, Waiter};
use crate::error::ZoeError;
use crate::types::Oid;
pub(crate) use handoff::{Handoff, InFlight, Waited};

/// A handle that ends the lock waits of votes from another thread.
///
/// The clones share one state. Give a clone to a transaction with
/// `Txn::set_lock_cancel`, and keep one to call `cancel`. After `cancel`,
/// a vote that waits for a lock, or starts to wait later, leaves the
/// queue, releases the locks that it took and fails with `Conflict` on
/// the oid that it waits for. A cancel does nothing to a vote that does
/// not wait, or that has its locks.
#[derive(Clone, Default)]
pub struct LockCancel(Arc<CancelState>);

#[derive(Default)]
struct CancelState {
    cancelled: AtomicBool,
    /// The waiters of the lock waits that run.
    waiters: Mutex<Vec<Arc<dyn Waiter>>>,
}

impl std::fmt::Debug for LockCancel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LockCancel")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

impl LockCancel {
    /// Return a new handle that is not cancelled.
    pub fn new() -> Self {
        Self::default()
    }

    /// End the lock wait that runs, and each later lock wait. Call it from
    /// any thread. The call does not block on the lock manager.
    pub fn cancel(&self) {
        self.0.cancelled.store(true, Ordering::SeqCst);
        let waiters = self.waiters().clone();
        wake_all(waiters);
    }

    /// Tell if `cancel` was called.
    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::SeqCst)
    }

    fn waiters(&self) -> MutexGuard<'_, Vec<Arc<dyn Waiter>>> {
        self.0
            .waiters
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Make `cancel` wake `waiter` until the guard drops. Call it before
    /// the first check of `is_cancelled`, so a cancel between the check and
    /// the wait wakes the wait.
    fn watch(&self, waiter: &Arc<dyn Waiter>) -> Watch<'_> {
        self.waiters().push(Arc::clone(waiter));
        Watch {
            cancel: self,
            waiter: Arc::clone(waiter),
        }
    }
}

/// The waiter of a lock wait in its `LockCancel`. The drop removes it.
struct Watch<'a> {
    cancel: &'a LockCancel,
    waiter: Arc<dyn Waiter>,
}

impl Drop for Watch<'_> {
    fn drop(&mut self) {
        let mut waiters = self.cancel.waiters();
        if let Some(index) = waiters
            .iter()
            .position(|waiter| Arc::ptr_eq(waiter, &self.waiter))
        {
            waiters.swap_remove(index);
        }
    }
}

/// The mode of a lock request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    /// For an oid that the transaction reads as current. Other shared
    /// locks can hold the oid at the same time.
    Shared,
    /// For an oid that the transaction writes. No other lock can hold the
    /// oid at the same time.
    Exclusive,
    /// An early write-intent lock (ADR 0045): exclusive, taken before the
    /// vote, before the transaction reads the oid. It never takes a
    /// hand-off: it waits until the store holds the last commit of the oid,
    /// as a shared request does.
    Intent,
}

/// A request that waits in the queue of an oid.
struct Queued {
    mode: Mode,
    /// The thread of the request waits on this waiter. The pointer also
    /// identifies the request in the queue.
    wake: Arc<dyn Waiter>,
}

impl std::fmt::Debug for Queued {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Queued").field("mode", &self.mode).finish()
    }
}

/// The waiters that got a lock under the mutex. The caller wakes them
/// after it releases the mutex.
type Woken = Vec<Arc<dyn Waiter>>;

/// Wake each waiter of `woken`. Call it without the mutex.
fn wake_all(woken: Woken) {
    for waiter in woken {
        waiter.wake();
    }
}

/// The lock state of one oid.
#[derive(Default)]
struct Entry {
    /// The number of requests that hold the lock.
    holders: usize,
    /// True when the holder holds the lock in exclusive mode.
    exclusive: bool,
    /// The hand-off of the commit in flight that writes the last state of
    /// the oid, until the store holds the last commit of the oid.
    committing: Option<Arc<Handoff>>,
    /// The requests that wait, in arrival order.
    queue: VecDeque<Queued>,
}

impl std::fmt::Debug for Entry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Entry")
            .field("holders", &self.holders)
            .field("exclusive", &self.exclusive)
            .field("committing", &self.committing.is_some())
            .field("queue", &self.queue)
            .finish()
    }
}

impl Entry {
    /// Tell if the holders let a request in `mode` hold the lock with
    /// them. While a hand-off is in the entry, a shared request and an
    /// intent request wait: they read the oid from the store, and the store
    /// does not hold the last commit of the oid yet.
    fn admits(&self, mode: Mode) -> bool {
        match mode {
            Mode::Exclusive => self.holders == 0,
            Mode::Intent | Mode::Shared if self.holders == 0 => self.committing.is_none(),
            Mode::Intent => false,
            Mode::Shared => !self.exclusive,
        }
    }

    /// Return the hand-off that an exclusive request gets with the lock.
    fn handoff(&self, mode: Mode) -> Option<Arc<Handoff>> {
        (mode == Mode::Exclusive)
            .then(|| self.committing.clone())
            .flatten()
    }

    fn take(&mut self, mode: Mode) {
        self.holders += 1;
        self.exclusive = mode != Mode::Shared;
    }

    /// Grant the lock to the waiters at the head of the queue, in arrival
    /// order, while the holders admit them. Add each waiter that gets the
    /// lock to `woken`.
    fn grant(&mut self, woken: &mut Woken) {
        while self
            .queue
            .front()
            .is_some_and(|head| self.admits(head.mode))
        {
            if let Some(head) = self.queue.pop_front() {
                self.take(head.mode);
                woken.push(head.wake);
            }
        }
    }

    fn waits(&self, wake: &Arc<dyn Waiter>) -> bool {
        self.queue
            .iter()
            .any(|waiter| Arc::ptr_eq(&waiter.wake, wake))
    }

    fn is_unused(&self) -> bool {
        self.holders == 0 && self.queue.is_empty() && self.committing.is_none()
    }
}

/// The lock table. It has an entry only for an oid that a request holds or
/// waits for.
#[derive(Debug, Default)]
struct Table {
    entries: HashMap<Oid, Entry>,
}

impl Table {
    /// Tell if the request that waits on `wake` waits for `oid`.
    fn waits(&self, oid: Oid, wake: &Arc<dyn Waiter>) -> bool {
        self.entries
            .get(&oid)
            .is_some_and(|entry| entry.waits(wake))
    }

    /// Remove the request that waits on `wake` from the queue of `oid`.
    /// The waiters behind it can then get the lock: add them to `woken`.
    fn withdraw(&mut self, oid: Oid, wake: &Arc<dyn Waiter>, woken: &mut Woken) {
        if let Some(entry) = self.entries.get_mut(&oid) {
            entry
                .queue
                .retain(|waiter| !Arc::ptr_eq(&waiter.wake, wake));
            entry.grant(woken);
            if entry.is_unused() {
                self.entries.remove(&oid);
            }
        }
    }

    /// Release one hold on each lock of `held`, and grant each lock to
    /// the waiters that it admits. Add these waiters to `woken`.
    fn release(&mut self, held: &[Held], woken: &mut Woken) {
        for &Held { oid, .. } in held {
            if let Some(entry) = self.entries.get_mut(&oid) {
                entry.holders -= 1;
                entry.grant(woken);
                if entry.is_unused() {
                    self.entries.remove(&oid);
                }
            }
        }
    }
}

/// The per-OID locks of a store.
#[derive(Debug, Default)]
pub(crate) struct LockManager {
    table: Mutex<Table>,
}

/// A lock that a call took: its oid, and the time on the store clock when
/// the call took it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Held {
    pub(crate) oid: Oid,
    pub(crate) since: SystemTime,
}

/// The locks that one `acquire` call took. Dropping it releases them.
pub(crate) struct Locks<'m> {
    manager: &'m LockManager,
    held: Vec<Held>,
    /// The locks that came with a hand-off.
    handoffs: Vec<(Oid, Arc<Handoff>)>,
    /// The end of the lock timeout of the call, on the monotonic time of
    /// its clock.
    deadline: Duration,
}

impl std::fmt::Debug for Locks<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Locks")
            .field("held", &self.held)
            .field("handoffs", &self.handoffs.len())
            .finish()
    }
}

impl Drop for Locks<'_> {
    fn drop(&mut self) {
        self.manager.release(&self.held);
    }
}

impl Locks<'_> {
    /// Keep the locks after this value is dropped, and return them. The
    /// caller must give them to `LockManager::release` later.
    pub(crate) fn keep(mut self) -> Vec<Held> {
        std::mem::take(&mut self.held)
    }

    /// Take the locks that came with a hand-off, with their hand-offs.
    pub(crate) fn handoffs(&mut self) -> Vec<(Oid, Arc<Handoff>)> {
        std::mem::take(&mut self.handoffs)
    }

    /// Return the end of the lock timeout of the call, on the monotonic
    /// time of its clock. A later wait of the vote ends there too.
    pub(crate) fn deadline(&self) -> Duration {
        self.deadline
    }
}

/// An `acquire` call that failed.
#[derive(Debug)]
pub(crate) struct Refused {
    pub(crate) error: ZoeError,
    /// The locks that the call took and released at a lock-wait timeout
    /// or a cancel. The list is empty for other errors.
    pub(crate) released: Vec<Held>,
}

/// A request in the queue of an oid while its call waits without the
/// mutex. If the call unwinds, the drop takes the request out of the
/// queue, or releases the lock that a grant gave to it.
struct InQueue<'a> {
    manager: &'a LockManager,
    oid: Oid,
    wake: &'a Arc<dyn Waiter>,
    armed: bool,
}

impl Drop for InQueue<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut woken = Woken::new();
        let mut table = self.manager.table();
        if table.waits(self.oid, self.wake) {
            table.withdraw(self.oid, self.wake, &mut woken);
        } else {
            let held = Held {
                oid: self.oid,
                since: SystemTime::UNIX_EPOCH,
            };
            table.release(&[held], &mut woken);
        }
        drop(table);
        wake_all(woken);
    }
}

impl LockManager {
    fn table(&self) -> MutexGuard<'_, Table> {
        self.table.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Lock each oid of `requests` in its mode.
    ///
    /// The oids must be in strictly ascending order. Otherwise the call
    /// takes no lock and fails with `LockOrder`. The call takes the locks in
    /// that order and waits when a lock is not free. It measures `timeout`
    /// from its start on the monotonic time of `clock`, and waits on a
    /// waiter of `clock`. If it still waits at that time, it leaves the
    /// queue, releases the locks that it took and fails with `Conflict` on
    /// the oid that it waits for. `Refused` then gives the released locks.
    ///
    /// Each lock keeps the time of `clock` when the call took it. The call
    /// reads `clock` at its start and after each wait, without the mutex.
    /// It gets a waiter from `clock` only when it must wait.
    #[cfg(test)]
    pub(crate) fn acquire(
        &self,
        requests: &[(Oid, Mode)],
        timeout: Duration,
        clock: &dyn Clock,
    ) -> Result<Locks<'_>, Refused> {
        self.acquire_cancellable(requests, timeout, clock, None)
    }

    /// Lock each oid of `requests` as `acquire` does. A `cancel` ends the
    /// wait as the timeout does: the call leaves the queue, releases the
    /// locks that it took and fails with `Conflict` on the oid that it
    /// waits for. A call that must wait after the cancel fails at once. A
    /// call that does not wait takes its locks also after a cancel.
    pub(crate) fn acquire_cancellable(
        &self,
        requests: &[(Oid, Mode)],
        timeout: Duration,
        clock: &dyn Clock,
        cancel: Option<&LockCancel>,
    ) -> Result<Locks<'_>, Refused> {
        if let Some(pair) = requests.windows(2).find(|pair| pair[0].0 >= pair[1].0) {
            return Err(Refused {
                error: ZoeError::LockOrder { oid: pair[1].0 },
                released: Vec::new(),
            });
        }
        let deadline = clock.monotonic().saturating_add(timeout);
        let mut since = clock.now();
        let mut locks = Locks {
            manager: self,
            held: Vec::with_capacity(requests.len()),
            handoffs: Vec::new(),
            deadline,
        };
        // The call waits for one oid at a time, so one waiter is
        // sufficient.
        let mut wake: Option<Arc<dyn Waiter>> = None;
        // The span of the wait for the oid that the call waits for.
        let mut waited: Option<tracing::span::EnteredSpan> = None;
        // A cancel wakes the waiter while this guard lives.
        let mut _watch: Option<Watch<'_>> = None;
        let cancelled = || cancel.is_some_and(LockCancel::is_cancelled);
        let mut table = self.table();
        let mut next = 0;
        while let Some(&(oid, mode)) = requests.get(next) {
            let entry = table.entries.entry(oid).or_default();
            let free = entry.queue.is_empty() && entry.admits(mode);
            if free {
                entry.take(mode);
                if let Some(handoff) = entry.handoff(mode) {
                    locks.handoffs.push((oid, handoff));
                }
            } else {
                let Some(wake) = wake.clone() else {
                    // The waiter comes from the clock, without the mutex.
                    // Then the call looks at the oid again.
                    drop(table);
                    let waiter = clock.waiter();
                    _watch = cancel.map(|cancel| cancel.watch(&waiter));
                    wake = Some(waiter);
                    table = self.table();
                    continue;
                };
                // A call that must wait after a cancel fails at once.
                if cancelled() {
                    let mut woken = Woken::new();
                    table.release(&locks.held, &mut woken);
                    drop(table);
                    wake_all(woken);
                    return Err(Refused {
                        error: ZoeError::Conflict { oid },
                        released: std::mem::take(&mut locks.held),
                    });
                }
                entry.queue.push_back(Queued {
                    mode,
                    wake: Arc::clone(&wake),
                });
                // If the clock panics, the guard takes the request out of
                // the queue, or gives back the lock that it got.
                let mut queued = InQueue {
                    manager: self,
                    oid,
                    wake: &wake,
                    armed: true,
                };
                loop {
                    drop(table);
                    if waited.is_none() {
                        waited = Some(tracing::info_span!("lock_wait", oid = oid.0).entered());
                    }
                    let left = deadline.saturating_sub(clock.monotonic());
                    if !left.is_zero() {
                        wake.wait(left);
                    }
                    table = self.table();
                    // A grant removes the request from the queue.
                    if !table.waits(oid, &wake) {
                        if let Some(handoff) = table
                            .entries
                            .get(&oid)
                            .and_then(|entry| entry.handoff(mode))
                        {
                            locks.handoffs.push((oid, handoff));
                        }
                        break;
                    }
                    // A request that is still in the queue at its deadline
                    // has used all its time. A cancel ends it too.
                    if left.is_zero() || cancelled() {
                        queued.armed = false;
                        let mut woken = Woken::new();
                        table.withdraw(oid, &wake, &mut woken);
                        table.release(&locks.held, &mut woken);
                        drop(table);
                        wake_all(woken);
                        return Err(Refused {
                            error: ZoeError::Conflict { oid },
                            released: std::mem::take(&mut locks.held),
                        });
                    }
                }
                queued.armed = false;
            }
            next += 1;
            locks.held.push(Held { oid, since });
            if !free {
                // The call holds the lock now. It reads the clock without
                // the mutex, so a slow clock stops no other call. If the
                // clock panics, `locks` drops and releases this lock too.
                drop(table);
                since = clock.now();
                drop(waited.take());
                table = self.table();
                if let Some(last) = locks.held.last_mut() {
                    last.since = since;
                }
            }
        }
        Ok(locks)
    }

    /// Release one hold on each lock of `held`. `held` must come from
    /// `Locks::keep`.
    pub(crate) fn release(&self, held: &[Held]) {
        if !held.is_empty() {
            let mut woken = Woken::new();
            self.table().release(held, &mut woken);
            wake_all(woken);
        }
    }

    /// Take the exclusive lock of `oid` only when it is free: no request
    /// holds it, no request waits for it, and its entry carries no
    /// hand-off. Else take nothing and return `None`.
    ///
    /// The call never waits, so a vote can take it after locks of greater
    /// oids (ADR 0049): it adds no wait out of oid order, and it never
    /// passes a request that waits. A free lock without a hand-off means
    /// that the store holds the last commit of `oid`. The lock keeps the
    /// time of `clock`, which the call reads without the mutex.
    pub(crate) fn try_exclusive(&self, oid: Oid, clock: &dyn Clock) -> Option<Held> {
        let since = clock.now();
        let mut table = self.table();
        if table
            .entries
            .get(&oid)
            .is_some_and(|entry| !entry.is_unused())
        {
            return None;
        }
        table.entries.entry(oid).or_default().take(Mode::Exclusive);
        Some(Held { oid, since })
    }

    /// Return the oids of `oids` whose queue has an exclusive request at
    /// its head. The caller holds the exclusive lock of each oid of `oids`.
    pub(crate) fn contended(&self, oids: impl Iterator<Item = Oid>) -> Vec<Oid> {
        let table = self.table();
        oids.filter(|oid| {
            table.entries.get(oid).is_some_and(|entry| {
                entry
                    .queue
                    .front()
                    .is_some_and(|head| head.mode == Mode::Exclusive)
            })
        })
        .collect()
    }

    /// Hand the lock of each oid of `handoffs` to the exclusive request at
    /// the head of its queue, with its hand-off. `held` holds the locks of
    /// the holder, whose finish started. An oid whose head is no longer an
    /// exclusive request stays with the holder.
    ///
    /// The call removes each lock that it hands off from `held`, and
    /// returns these locks with their hand-offs. The entry of each one
    /// keeps the hand-off until `settle`. The call grants under the mutex,
    /// then wakes the new owners without it, as a release does.
    pub(crate) fn hand_off(
        &self,
        handoffs: Vec<(Oid, Arc<Handoff>)>,
        held: &mut Vec<Held>,
    ) -> Vec<(Held, Arc<Handoff>)> {
        let mut handed = Vec::new();
        let mut woken = Woken::new();
        let mut table = self.table();
        for (oid, handoff) in handoffs {
            let Some(entry) = table.entries.get_mut(&oid) else {
                continue;
            };
            let exclusive_head = entry
                .queue
                .front()
                .is_some_and(|head| head.mode == Mode::Exclusive);
            let Some(index) = held.iter().position(|lock| lock.oid == oid) else {
                continue;
            };
            if !(exclusive_head && entry.exclusive && entry.holders == 1) {
                continue;
            }
            // The hold passes to the head of the queue: the count of the
            // holders stays 1.
            let Some(head) = entry.queue.pop_front() else {
                continue;
            };
            entry.committing = Some(Arc::clone(&handoff));
            woken.push(head.wake);
            handed.push((held.swap_remove(index), handoff));
        }
        drop(table);
        wake_all(woken);
        // `held` stays in oid order for the release and the log.
        held.sort_by_key(|lock| lock.oid);
        handed
    }

    /// End the hand-offs of `handed`: the store holds the last commit of
    /// each oid, or the commit of the hand-off failed. The entry of each oid
    /// drops the hand-off when it is still the one of `handed`, and grants
    /// its lock to the waiters that it admits.
    ///
    /// Two finishes call it: the finish of the holder, when its commit is
    /// durable or failed, and the finish of a new owner, when its own commit
    /// is durable. That commit comes after the commit of the hand-off, so
    /// the store holds the last commit of the oid, even when the finish of
    /// the holder did not settle yet.
    pub(crate) fn settle<'h>(&self, handed: impl IntoIterator<Item = (Oid, &'h Arc<Handoff>)>) {
        let mut woken = Woken::new();
        let mut table = self.table();
        for (oid, handoff) in handed {
            let Some(entry) = table.entries.get_mut(&oid) else {
                continue;
            };
            if entry
                .committing
                .as_ref()
                .is_some_and(|committing| Arc::ptr_eq(committing, handoff))
            {
                entry.committing = None;
                entry.grant(&mut woken);
            }
            if entry.is_unused() {
                table.entries.remove(&oid);
            }
        }
        drop(table);
        wake_all(woken);
    }

    /// Tell if no request holds or waits for a lock.
    pub(crate) fn is_empty(&self) -> bool {
        self.table().entries.is_empty()
    }

    /// Return the number of requests that wait for `oid`.
    #[cfg(test)]
    pub(crate) fn waiting(&self, oid: Oid) -> usize {
        self.table()
            .entries
            .get(&oid)
            .map_or(0, |entry| entry.queue.len())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicIsize, Ordering};
    use std::sync::{Barrier, mpsc};
    use std::thread;
    use std::time::Instant;

    use super::Mode::{Exclusive, Shared};
    use super::*;
    use crate::clock::{ManualClock, SystemClock};

    /// A timeout that no test reaches when the code is correct.
    const LONG: Duration = Duration::from_secs(10);

    /// Wait until `count` requests wait for `oid`.
    fn until_waiting(locks: &LockManager, oid: Oid, count: usize) {
        let start = Instant::now();
        while locks.waiting(oid) != count {
            assert!(
                start.elapsed() < LONG,
                "{count} waiters expected on {oid:?}"
            );
            thread::sleep(Duration::from_millis(1));
        }
    }

    /// A test clock. With `simulated`, its monotonic time moves only when
    /// a request waits: each wait moves it by the whole timeout and returns
    /// at once. Otherwise its waits run on the operating system. Each wake
    /// notes whether the mutex of the lock table was free.
    struct TestClock {
        locks: Arc<LockManager>,
        simulated: bool,
        time: Arc<Mutex<Duration>>,
        /// One entry for each wake: true when the table was free.
        wakes: Arc<Mutex<Vec<bool>>>,
    }

    struct TestWaiter {
        locks: Arc<LockManager>,
        simulated: bool,
        time: Arc<Mutex<Duration>>,
        wakes: Arc<Mutex<Vec<bool>>>,
        inner: Arc<dyn Waiter>,
    }

    impl TestClock {
        fn new(locks: &Arc<LockManager>, simulated: bool) -> Self {
            Self {
                locks: Arc::clone(locks),
                simulated,
                time: Arc::new(Mutex::new(Duration::ZERO)),
                wakes: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    impl Clock for TestClock {
        fn now(&self) -> SystemTime {
            SystemTime::UNIX_EPOCH + *self.time.lock().unwrap()
        }

        fn monotonic(&self) -> Duration {
            if self.simulated {
                *self.time.lock().unwrap()
            } else {
                SystemClock.monotonic()
            }
        }

        fn waiter(&self) -> Arc<dyn Waiter> {
            Arc::new(TestWaiter {
                locks: Arc::clone(&self.locks),
                simulated: self.simulated,
                time: Arc::clone(&self.time),
                wakes: Arc::clone(&self.wakes),
                inner: SystemClock.waiter(),
            })
        }
    }

    impl Waiter for TestWaiter {
        fn wait(&self, timeout: Duration) {
            if self.simulated {
                *self.time.lock().unwrap() += timeout;
            } else {
                self.inner.wait(timeout);
            }
        }

        fn wake(&self) {
            let free = self.locks.table.try_lock().is_ok();
            self.wakes.lock().unwrap().push(free);
            self.inner.wake();
        }
    }

    #[test]
    fn a_lock_wait_runs_on_the_time_of_the_clock() {
        let locks = Arc::new(LockManager::default());
        let clock = TestClock::new(&locks, true);
        let held = locks
            .acquire(&[(Oid(1), Exclusive)], LONG, &SystemClock)
            .unwrap();
        // A timeout of an hour passes on the clock at the first wait.
        let start = Instant::now();
        let refused = locks
            .acquire(&[(Oid(1), Exclusive)], Duration::from_secs(3600), &clock)
            .unwrap_err();
        assert!(matches!(refused.error, ZoeError::Conflict { oid: Oid(1) }));
        assert!(start.elapsed() < LONG);
        assert_eq!(clock.monotonic(), Duration::from_secs(3600));
        drop(held);
        assert!(locks.is_empty());
    }

    #[test]
    fn a_grant_wakes_its_waiter_after_it_releases_the_mutex() {
        let locks = Arc::new(LockManager::default());
        let held = locks
            .acquire(&[(Oid(1), Exclusive)], LONG, &SystemClock)
            .unwrap();
        let clock = TestClock::new(&locks, false);
        let wakes = Arc::clone(&clock.wakes);
        let waiting = {
            let locks = Arc::clone(&locks);
            thread::spawn(move || {
                locks
                    .acquire(&[(Oid(1), Exclusive)], LONG, &clock)
                    .map(Locks::keep)
                    .map_err(|refused| refused.error)
            })
        };
        until_waiting(&locks, Oid(1), 1);
        drop(held);
        let kept = waiting.join().unwrap().unwrap();
        assert_eq!(*wakes.lock().unwrap(), [true]);
        locks.release(&kept);
        assert!(locks.is_empty());
    }

    #[test]
    fn a_free_lock_takes_no_waiter_from_the_clock() {
        struct NoWaiter;
        impl Clock for NoWaiter {
            fn now(&self) -> SystemTime {
                SystemTime::UNIX_EPOCH
            }

            fn waiter(&self) -> Arc<dyn Waiter> {
                panic!("a free lock needs no waiter")
            }
        }
        let locks = LockManager::default();
        let held = locks
            .acquire(&[(Oid(1), Exclusive), (Oid(2), Shared)], LONG, &NoWaiter)
            .unwrap();
        drop(held);
        assert!(locks.is_empty());
    }

    #[test]
    fn a_free_lock_is_taken_and_released() {
        let locks = LockManager::default();
        let held = locks
            .acquire(
                &[(Oid(1), Exclusive), (Oid(2), Shared)],
                Duration::ZERO,
                &SystemClock,
            )
            .unwrap();
        assert!(!locks.is_empty());
        drop(held);
        assert!(locks.is_empty());
    }

    #[test]
    fn kept_locks_stay_until_their_release() {
        let locks = LockManager::default();
        let held = locks
            .acquire(
                &[(Oid(1), Exclusive), (Oid(2), Shared)],
                Duration::ZERO,
                &SystemClock,
            )
            .unwrap()
            .keep();
        let oids: Vec<_> = held.iter().map(|lock| lock.oid).collect();
        assert_eq!(oids, [Oid(1), Oid(2)]);
        assert!(!locks.is_empty());
        locks.release(&held);
        assert!(locks.is_empty());
    }

    #[test]
    fn each_lock_keeps_the_clock_time_when_the_call_took_it() {
        let locks = LockManager::default();
        let clock = ManualClock::new();
        let start = clock.now();
        let holder = locks
            .acquire(&[(Oid(2), Exclusive)], Duration::ZERO, &clock)
            .unwrap();
        thread::scope(|scope| {
            let waiter = scope.spawn(|| {
                locks
                    .acquire(&[(Oid(1), Shared), (Oid(2), Exclusive)], LONG, &clock)
                    .map(Locks::keep)
            });
            until_waiting(&locks, Oid(2), 1);
            clock.advance(Duration::from_secs(5));
            drop(holder);
            let held = waiter.join().unwrap().unwrap();
            let since = start + Duration::from_secs(5);
            assert_eq!(
                held,
                [
                    Held {
                        oid: Oid(1),
                        since: start
                    },
                    Held { oid: Oid(2), since }
                ]
            );
            locks.release(&held);
        });
        assert!(locks.is_empty());
    }

    /// A clock that panics on each read after the first one.
    struct FailingClock(AtomicIsize);

    impl Clock for FailingClock {
        fn now(&self) -> SystemTime {
            assert!(
                self.0.fetch_add(1, Ordering::SeqCst) == 0,
                "the clock fails"
            );
            SystemTime::UNIX_EPOCH
        }
    }

    #[test]
    fn a_clock_that_panics_after_a_wait_leaks_no_lock() {
        let locks = LockManager::default();
        let holder = locks
            .acquire(&[(Oid(2), Exclusive)], Duration::ZERO, &SystemClock)
            .unwrap();
        let clock = FailingClock(AtomicIsize::new(0));
        thread::scope(|scope| {
            // The call takes `Oid(1)`, then waits for `Oid(2)`. The clock
            // panics when the call reads it after the wait.
            let waiter = scope
                .spawn(|| locks.acquire(&[(Oid(1), Exclusive), (Oid(2), Exclusive)], LONG, &clock));
            until_waiting(&locks, Oid(2), 1);
            drop(holder);
            assert!(waiter.join().is_err());
        });
        assert!(locks.is_empty());
    }

    #[test]
    fn a_request_out_of_order_is_refused() {
        let locks = LockManager::default();
        for requests in [
            [(Oid(2), Exclusive), (Oid(1), Exclusive)],
            [(Oid(1), Shared), (Oid(1), Exclusive)],
        ] {
            let error = locks
                .acquire(&requests, Duration::ZERO, &SystemClock)
                .map_err(|refused| refused.error);
            assert!(
                matches!(error, Err(ZoeError::LockOrder { oid: Oid(1) })),
                "{requests:?}"
            );
            assert!(locks.is_empty());
        }
    }

    #[test]
    fn a_shared_lock_is_compatible_only_with_shared_locks() {
        let locks = LockManager::default();
        let oid = Oid(1);
        let compatible = |held: Mode, asked: Mode| {
            let _held = locks
                .acquire(&[(oid, held)], Duration::ZERO, &SystemClock)
                .unwrap();
            let result = locks
                .acquire(&[(oid, asked)], Duration::ZERO, &SystemClock)
                .map_err(|refused| refused.error);
            match result {
                Ok(_) => true,
                Err(ZoeError::Conflict { oid: Oid(1) }) => false,
                Err(error) => panic!("{error:?}"),
            }
        };
        assert!(compatible(Shared, Shared));
        assert!(!compatible(Shared, Exclusive));
        assert!(!compatible(Exclusive, Shared));
        assert!(!compatible(Exclusive, Exclusive));
        assert!(locks.is_empty());
    }

    #[test]
    fn waiters_get_the_lock_in_arrival_order() {
        let locks = LockManager::default();
        let oid = Oid(1);
        let holder = locks
            .acquire(&[(oid, Exclusive)], Duration::ZERO, &SystemClock)
            .unwrap();
        let (granted, order) = mpsc::channel();
        thread::scope(|scope| {
            for name in 0..5 {
                let granted = granted.clone();
                let locks = &locks;
                scope.spawn(move || {
                    let held = locks
                        .acquire(&[(oid, Exclusive)], LONG, &SystemClock)
                        .unwrap();
                    granted.send(name).unwrap();
                    drop(held);
                });
                until_waiting(locks, oid, name + 1);
            }
            drop(holder);
        });
        let names: Vec<_> = order.try_iter().collect();
        assert_eq!(names, [0, 1, 2, 3, 4]);
        assert!(locks.is_empty());
    }

    #[test]
    fn a_shared_request_does_not_pass_a_waiting_exclusive_request() {
        let locks = LockManager::default();
        let oid = Oid(1);
        let reader = locks
            .acquire(&[(oid, Shared)], Duration::ZERO, &SystemClock)
            .unwrap();
        let (granted, order) = mpsc::channel();
        thread::scope(|scope| {
            let writer = granted.clone();
            let locks = &locks;
            scope.spawn(move || {
                let held = locks
                    .acquire(&[(oid, Exclusive)], LONG, &SystemClock)
                    .unwrap();
                writer.send("writer").unwrap();
                drop(held);
            });
            until_waiting(locks, oid, 1);

            // The shared lock is free for a reader, but a writer waits.
            let refused = locks
                .acquire(&[(oid, Shared)], Duration::ZERO, &SystemClock)
                .map_err(|refused| refused.error);
            assert!(matches!(refused, Err(ZoeError::Conflict { .. })));

            scope.spawn(move || {
                let held = locks.acquire(&[(oid, Shared)], LONG, &SystemClock).unwrap();
                granted.send("second reader").unwrap();
                drop(held);
            });
            until_waiting(locks, oid, 2);
            drop(reader);
        });
        let names: Vec<_> = order.try_iter().collect();
        assert_eq!(names, ["writer", "second reader"]);
        assert!(locks.is_empty());
    }

    #[test]
    fn a_release_grants_every_shared_waiter_at_the_head() {
        let locks = LockManager::default();
        let oid = Oid(1);
        let writer = locks
            .acquire(&[(oid, Exclusive)], Duration::ZERO, &SystemClock)
            .unwrap();
        let readers = Barrier::new(3);
        let (granted, order) = mpsc::channel();
        thread::scope(|scope| {
            for (waiting, name) in ["first reader", "second reader"].into_iter().enumerate() {
                let granted = granted.clone();
                let (locks, readers) = (&locks, &readers);
                scope.spawn(move || {
                    let held = locks.acquire(&[(oid, Shared)], LONG, &SystemClock).unwrap();
                    granted.send(name).unwrap();
                    // Both readers hold the lock at the same time.
                    readers.wait();
                    drop(held);
                });
                until_waiting(locks, oid, waiting + 1);
            }
            let locks = &locks;
            scope.spawn(move || {
                let held = locks
                    .acquire(&[(oid, Exclusive)], LONG, &SystemClock)
                    .unwrap();
                granted.send("second writer").unwrap();
                drop(held);
            });
            until_waiting(locks, oid, 3);
            drop(writer);
            readers.wait();
        });
        let names: Vec<_> = order.try_iter().collect();
        assert_eq!(names.len(), 3);
        assert_eq!(names[2], "second writer");
        assert!(locks.is_empty());
    }

    #[test]
    fn a_wait_past_the_timeout_releases_what_the_call_took() {
        let locks = LockManager::default();
        let holder = locks
            .acquire(&[(Oid(2), Exclusive)], Duration::ZERO, &SystemClock)
            .unwrap();
        let timeout = Duration::from_millis(50);
        let start = Instant::now();
        let refused = locks
            .acquire(
                &[(Oid(1), Exclusive), (Oid(2), Shared)],
                timeout,
                &SystemClock,
            )
            .unwrap_err();
        assert!(start.elapsed() >= timeout);
        assert!(matches!(refused.error, ZoeError::Conflict { oid: Oid(2) }));
        // The error gives back the lock that the call released.
        let released: Vec<_> = refused.released.iter().map(|lock| lock.oid).collect();
        assert_eq!(released, [Oid(1)]);
        assert_eq!(locks.waiting(Oid(2)), 0);
        locks
            .acquire(&[(Oid(1), Exclusive)], Duration::ZERO, &SystemClock)
            .unwrap();
        drop(holder);
        assert!(locks.is_empty());
    }

    #[test]
    fn the_timeout_covers_all_the_waits_of_a_call() {
        let locks = LockManager::default();
        let first = locks
            .acquire(&[(Oid(1), Exclusive)], Duration::ZERO, &SystemClock)
            .unwrap();
        let second = locks
            .acquire(&[(Oid(2), Exclusive)], Duration::ZERO, &SystemClock)
            .unwrap();
        // The margins of about half the timeout absorb slow scheduling, for
        // example under ThreadSanitizer.
        let timeout = Duration::from_secs(2);
        thread::scope(|scope| {
            let waiter = scope.spawn(|| {
                let start = Instant::now();
                let result = locks.acquire(
                    &[(Oid(1), Exclusive), (Oid(2), Exclusive)],
                    timeout,
                    &SystemClock,
                );
                let result = result.map(drop).map_err(|refused| refused.error);
                (result, start, Instant::now())
            });
            until_waiting(&locks, Oid(1), 1);
            thread::sleep(timeout / 2);
            let released = Instant::now();
            drop(first);
            let (result, start, end) = waiter.join().unwrap();
            assert!(matches!(result, Err(ZoeError::Conflict { oid: Oid(2) })));
            assert!(end - start >= timeout, "{:?}", end - start);
            // The wait for the second oid gets only the time that is left.
            assert!(end - released < timeout, "{:?}", end - released);
        });
        drop(second);
        assert!(locks.is_empty());
    }

    #[test]
    fn a_waiter_that_times_out_lets_the_waiters_behind_it_in() {
        let locks = LockManager::default();
        let oid = Oid(1);
        let reader = locks
            .acquire(&[(oid, Shared)], Duration::ZERO, &SystemClock)
            .unwrap();
        thread::scope(|scope| {
            // The second reader must queue before the writer times out. The
            // timeout is long so that slow scheduling cannot break the test.
            let writer = scope.spawn(|| {
                locks
                    .acquire(&[(oid, Exclusive)], Duration::from_secs(1), &SystemClock)
                    .map(drop)
                    .map_err(|refused| refused.error)
            });
            until_waiting(&locks, oid, 1);
            let second = scope.spawn(|| {
                locks
                    .acquire(&[(oid, Shared)], LONG, &SystemClock)
                    .map(drop)
            });
            until_waiting(&locks, oid, 2);
            assert!(matches!(
                writer.join().unwrap(),
                Err(ZoeError::Conflict { .. })
            ));
            // The second reader gets the lock while the first one holds it.
            second.join().unwrap().unwrap();
        });
        drop(reader);
        assert!(locks.is_empty());
    }

    #[test]
    fn a_zero_timeout_never_waits() {
        let locks = LockManager::default();
        let holder = locks
            .acquire(&[(Oid(1), Exclusive)], Duration::ZERO, &SystemClock)
            .unwrap();
        let error = locks
            .acquire(&[(Oid(1), Exclusive)], Duration::ZERO, &SystemClock)
            .map_err(|refused| refused.error);
        assert!(matches!(error, Err(ZoeError::Conflict { oid: Oid(1) })));
        assert_eq!(locks.waiting(Oid(1)), 0);
        drop(holder);
        assert!(locks.is_empty());
    }

    #[test]
    fn a_cancel_ends_a_wait_and_releases_what_the_call_took() {
        let locks = LockManager::default();
        let holder = locks
            .acquire(&[(Oid(2), Exclusive)], Duration::ZERO, &SystemClock)
            .unwrap();
        let cancel = LockCancel::new();
        thread::scope(|scope| {
            let waiter = scope.spawn(|| {
                let start = Instant::now();
                let refused = locks
                    .acquire_cancellable(
                        &[(Oid(1), Exclusive), (Oid(2), Exclusive)],
                        LONG,
                        &SystemClock,
                        Some(&cancel),
                    )
                    .unwrap_err();
                (refused, start.elapsed())
            });
            until_waiting(&locks, Oid(2), 1);
            cancel.cancel();
            let (refused, waited) = waiter.join().unwrap();
            assert!(waited < LONG, "{waited:?}");
            assert!(matches!(refused.error, ZoeError::Conflict { oid: Oid(2) }));
            let released: Vec<_> = refused.released.iter().map(|lock| lock.oid).collect();
            assert_eq!(released, [Oid(1)]);
        });
        assert_eq!(locks.waiting(Oid(2)), 0);
        locks
            .acquire(&[(Oid(1), Exclusive)], Duration::ZERO, &SystemClock)
            .unwrap();
        drop(holder);
        assert!(locks.is_empty());
    }

    #[test]
    fn a_wait_that_starts_after_a_cancel_fails_at_once() {
        let locks = LockManager::default();
        let holder = locks
            .acquire(&[(Oid(2), Exclusive)], Duration::ZERO, &SystemClock)
            .unwrap();
        let cancel = LockCancel::new();
        cancel.cancel();
        let start = Instant::now();
        let refused = locks
            .acquire_cancellable(
                &[(Oid(1), Exclusive), (Oid(2), Exclusive)],
                LONG,
                &SystemClock,
                Some(&cancel),
            )
            .unwrap_err();
        assert!(start.elapsed() < LONG);
        assert!(matches!(refused.error, ZoeError::Conflict { oid: Oid(2) }));
        let released: Vec<_> = refused.released.iter().map(|lock| lock.oid).collect();
        assert_eq!(released, [Oid(1)]);
        assert_eq!(locks.waiting(Oid(2)), 0);
        drop(holder);
        assert!(locks.is_empty());
    }

    #[test]
    fn a_cancel_does_nothing_to_a_call_that_does_not_wait() {
        let locks = LockManager::default();
        let cancel = LockCancel::new();
        cancel.cancel();
        let held = locks
            .acquire_cancellable(&[(Oid(1), Exclusive)], LONG, &SystemClock, Some(&cancel))
            .unwrap();
        // A call that has its locks keeps them after a cancel.
        let other = LockCancel::new();
        let kept = locks
            .acquire_cancellable(&[(Oid(2), Shared)], LONG, &SystemClock, Some(&other))
            .unwrap()
            .keep();
        other.cancel();
        assert_eq!(locks.waiting(Oid(1)), 0);
        drop(held);
        assert!(!locks.is_empty());
        locks.release(&kept);
        assert!(locks.is_empty());
    }

    #[test]
    fn a_cancel_wakes_each_wait_that_shares_it() {
        let locks = LockManager::default();
        let holder = locks
            .acquire(&[(Oid(1), Exclusive)], Duration::ZERO, &SystemClock)
            .unwrap();
        let cancel = LockCancel::new();
        thread::scope(|scope| {
            let waiters: Vec<_> = (0..2)
                .map(|_| {
                    scope.spawn(|| {
                        locks
                            .acquire_cancellable(
                                &[(Oid(1), Shared)],
                                LONG,
                                &SystemClock,
                                Some(&cancel),
                            )
                            .map(drop)
                            .map_err(|refused| refused.error)
                    })
                })
                .collect();
            until_waiting(&locks, Oid(1), 2);
            cancel.cancel();
            for waiter in waiters {
                assert!(matches!(
                    waiter.join().unwrap(),
                    Err(ZoeError::Conflict { oid: Oid(1) })
                ));
            }
        });
        drop(holder);
        assert!(locks.is_empty());
    }

    /// Return a hand-off of a new commit in flight.
    fn handoff() -> Arc<Handoff> {
        Arc::new(Handoff {
            commit: Arc::new(InFlight::new(b"", b"")),
            record: b"record".to_vec(),
        })
    }

    #[test]
    fn a_hand_off_passes_the_lock_to_the_exclusive_request_at_the_head() {
        let locks = LockManager::default();
        let mut held = locks
            .acquire(
                &[
                    (Oid(1), Exclusive),
                    (Oid(2), Exclusive),
                    (Oid(3), Exclusive),
                ],
                Duration::ZERO,
                &SystemClock,
            )
            .unwrap()
            .keep();
        let (first, second) = (handoff(), handoff());
        thread::scope(|scope| {
            let owner = scope.spawn(|| {
                let mut taken = locks
                    .acquire(&[(Oid(1), Exclusive)], LONG, &SystemClock)
                    .unwrap();
                let handoffs = taken.handoffs();
                (taken.keep(), handoffs)
            });
            let reader = scope.spawn(|| {
                locks
                    .acquire(&[(Oid(2), Shared)], LONG, &SystemClock)
                    .map(Locks::keep)
                    .map_err(|refused| refused.error)
            });
            until_waiting(&locks, Oid(1), 1);
            until_waiting(&locks, Oid(2), 1);
            // Oid 2 has a shared request at its head, and oid 3 no request.
            let oids = [Oid(1), Oid(2), Oid(3)].into_iter();
            assert_eq!(locks.contended(oids), [Oid(1)]);
            let offered = vec![(Oid(1), Arc::clone(&first)), (Oid(2), Arc::clone(&second))];
            let handed = locks.hand_off(offered, &mut held);
            assert_eq!(handed.len(), 1);
            assert_eq!(handed[0].0.oid, Oid(1));
            let oids: Vec<_> = held.iter().map(|lock| lock.oid).collect();
            assert_eq!(oids, [Oid(2), Oid(3)]);
            let (owned, handoffs) = owner.join().unwrap();
            assert!(Arc::ptr_eq(&handoffs[0].1, &first));
            locks.release(&owned);
            // The commit is in flight: a shared request waits, and the next
            // exclusive request takes the lock with the hand-off.
            let refused = locks
                .acquire(&[(Oid(1), Shared)], Duration::ZERO, &SystemClock)
                .map_err(|refused| refused.error);
            assert!(matches!(refused, Err(ZoeError::Conflict { oid: Oid(1) })));
            let mut next = locks
                .acquire(&[(Oid(1), Exclusive)], Duration::ZERO, &SystemClock)
                .unwrap();
            assert!(Arc::ptr_eq(&next.handoffs()[0].1, &first));
            drop(next);
            first.commit.end(true);
            locks.settle(handed.iter().map(|(lock, handoff)| (lock.oid, handoff)));
            let free = locks
                .acquire(&[(Oid(1), Shared)], Duration::ZERO, &SystemClock)
                .unwrap();
            assert!(free.handoffs.is_empty());
            drop(free);
            locks.release(&held);
            reader
                .join()
                .unwrap()
                .map(|kept| locks.release(&kept))
                .unwrap();
        });
        assert!(locks.is_empty());
    }

    #[test]
    fn a_settle_grants_the_shared_waiters_and_leaves_a_later_hand_off() {
        let locks = LockManager::default();
        let mut held = locks
            .acquire(&[(Oid(1), Exclusive)], Duration::ZERO, &SystemClock)
            .unwrap()
            .keep();
        let (first, later) = (handoff(), handoff());
        thread::scope(|scope| {
            let owner = scope.spawn(|| {
                locks
                    .acquire(&[(Oid(1), Exclusive)], LONG, &SystemClock)
                    .unwrap()
                    .keep()
            });
            until_waiting(&locks, Oid(1), 1);
            let handed = locks.hand_off(vec![(Oid(1), Arc::clone(&first))], &mut held);
            let mut owned = owner.join().unwrap();
            // The new owner hands the lock on before the first commit ends.
            let next = scope.spawn(|| {
                let mut taken = locks
                    .acquire(&[(Oid(1), Exclusive)], LONG, &SystemClock)
                    .unwrap();
                let handoffs = taken.handoffs();
                (taken.keep(), handoffs)
            });
            until_waiting(&locks, Oid(1), 1);
            let later_handed = locks.hand_off(vec![(Oid(1), Arc::clone(&later))], &mut owned);
            assert!(owned.is_empty());
            let (taken, handoffs) = next.join().unwrap();
            assert!(Arc::ptr_eq(&handoffs[0].1, &later));
            // The end of the first commit leaves the later hand-off.
            locks.settle(handed.iter().map(|(lock, handoff)| (lock.oid, handoff)));
            locks.release(&taken);
            let refused = locks
                .acquire(&[(Oid(1), Shared)], Duration::ZERO, &SystemClock)
                .map_err(|refused| refused.error);
            assert!(matches!(refused, Err(ZoeError::Conflict { oid: Oid(1) })));
            let reader = scope.spawn(|| {
                locks
                    .acquire(&[(Oid(1), Shared)], LONG, &SystemClock)
                    .map(Locks::keep)
                    .map_err(|refused| refused.error)
            });
            until_waiting(&locks, Oid(1), 1);
            locks.settle(
                later_handed
                    .iter()
                    .map(|(lock, handoff)| (lock.oid, handoff)),
            );
            let kept = reader.join().unwrap().unwrap();
            locks.release(&kept);
        });
        assert!(locks.is_empty());
    }

    /// Many threads lock overlapping oid sets with a long timeout. A lost
    /// wake-up shows as a timeout. An exclusive lock that another lock
    /// shares shows as a wrong count.
    #[test]
    fn no_wake_up_is_lost_under_contention() {
        const THREADS: usize = 8;
        const ROUNDS: usize = 500;
        const OIDS: u64 = 4;
        let locks = LockManager::default();
        // For each oid: the number of shared holders, or -1 for an
        // exclusive holder.
        let counts: Vec<AtomicIsize> = (0..OIDS).map(|_| AtomicIsize::new(0)).collect();
        thread::scope(|scope| {
            for thread in 0..THREADS {
                let (locks, counts) = (&locks, &counts);
                scope.spawn(move || {
                    for round in 0..ROUNDS {
                        // A sequence that differs per thread and per round.
                        let seed = (thread * ROUNDS + round) as u64;
                        let requests: Vec<_> = (0..OIDS)
                            .filter(|oid| (seed >> oid) & 1 == 1 || oid % 2 == seed % 2)
                            .map(|oid| {
                                let mode = if (seed / 3 + oid).is_multiple_of(3) {
                                    Exclusive
                                } else {
                                    Shared
                                };
                                (Oid(oid), mode)
                            })
                            .collect();
                        let held = locks.acquire(&requests, LONG, &SystemClock).unwrap();
                        for &(oid, mode) in &requests {
                            let count = &counts[oid.0 as usize];
                            if mode == Exclusive {
                                assert_eq!(count.swap(-1, Ordering::SeqCst), 0);
                            } else {
                                assert!(count.fetch_add(1, Ordering::SeqCst) >= 0);
                            }
                        }
                        thread::yield_now();
                        for &(oid, mode) in &requests {
                            let count = &counts[oid.0 as usize];
                            if mode == Exclusive {
                                assert_eq!(count.swap(0, Ordering::SeqCst), -1);
                            } else {
                                assert!(count.fetch_sub(1, Ordering::SeqCst) > 0);
                            }
                        }
                        drop(held);
                    }
                });
            }
        });
        assert!(locks.is_empty());
    }

    #[test]
    fn try_exclusive_takes_a_free_lock_only() {
        let locks = LockManager::default();
        let clock = SystemClock;
        let held = locks.try_exclusive(Oid(5), &clock).expect("a free lock");
        assert_eq!(held.oid, Oid(5));
        // A held lock is not free, in either mode.
        assert!(locks.try_exclusive(Oid(5), &clock).is_none());
        let shared = locks
            .acquire(&[(Oid(6), Shared)], LONG, &clock)
            .unwrap()
            .keep();
        assert!(locks.try_exclusive(Oid(6), &clock).is_none());
        locks.release(&[held]);
        locks.release(&shared);
        assert!(locks.is_empty());
    }

    #[test]
    fn try_exclusive_refuses_a_lock_with_a_waiter() {
        let locks = Arc::new(LockManager::default());
        let clock = SystemClock;
        let first = locks
            .acquire(&[(Oid(7), Exclusive)], LONG, &clock)
            .unwrap()
            .keep();
        let waiter = {
            let locks = Arc::clone(&locks);
            thread::spawn(move || {
                let held = locks
                    .acquire(&[(Oid(7), Exclusive)], LONG, &SystemClock)
                    .unwrap()
                    .keep();
                locks.release(&held);
            })
        };
        until_waiting(&locks, Oid(7), 1);
        assert!(locks.try_exclusive(Oid(7), &clock).is_none());
        locks.release(&first);
        waiter.join().unwrap();
        assert!(locks.is_empty());
    }

    #[test]
    fn try_exclusive_refuses_an_entry_with_a_hand_off() {
        let locks = Arc::new(LockManager::default());
        let clock = SystemClock;
        let mut holder = locks
            .acquire(&[(Oid(8), Exclusive)], LONG, &clock)
            .unwrap()
            .keep();
        let (sender, receiver) = mpsc::channel();
        let owner = {
            let locks = Arc::clone(&locks);
            thread::spawn(move || {
                let mut got = locks
                    .acquire(&[(Oid(8), Exclusive)], LONG, &SystemClock)
                    .unwrap();
                let handoffs = got.handoffs();
                let held = got.keep();
                // The new owner releases before the holder is durable: the
                // entry keeps the hand-off.
                locks.release(&held);
                sender.send(handoffs).unwrap();
            })
        };
        until_waiting(&locks, Oid(8), 1);
        let handoff = Arc::new(Handoff {
            commit: Arc::new(InFlight::new(b"", b"")),
            record: Vec::new(),
        });
        let handed = locks.hand_off(vec![(Oid(8), Arc::clone(&handoff))], &mut holder);
        assert_eq!(handed.len(), 1);
        owner.join().unwrap();
        assert_eq!(receiver.recv().unwrap().len(), 1);
        // No request holds the lock, but the store may not hold the last
        // commit of the oid yet.
        assert!(locks.try_exclusive(Oid(8), &clock).is_none());
        locks.settle([(Oid(8), &handoff)]);
        let held = locks.try_exclusive(Oid(8), &clock).expect("a settled lock");
        locks.release(&[held]);
        assert!(locks.is_empty());
    }
}
