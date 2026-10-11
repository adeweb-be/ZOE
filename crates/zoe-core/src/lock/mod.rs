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
//! it to the waiters at the head of the queue, under the mutex, and then
//! wakes them. So a waiter holds its lock before it wakes up, and no new
//! request can take the lock first.
//!
//! Each lock that a call takes keeps the time when the call took it, on
//! the store clock. `lock-warn` measures the hold time from it. The
//! timeout uses the monotonic clock instead, because a `Condvar` wait
//! cannot run on a simulated clock.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use crate::clock::Clock;
use crate::error::ZoeError;
use crate::types::Oid;

/// The mode of a lock request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    /// For an oid that the transaction reads as current. Other shared
    /// locks can hold the oid at the same time.
    Shared,
    /// For an oid that the transaction writes. No other lock can hold the
    /// oid at the same time.
    Exclusive,
}

/// A request that waits in the queue of an oid.
#[derive(Debug)]
struct Waiter {
    mode: Mode,
    /// The thread of the request sleeps on this condition variable. The
    /// pointer also identifies the request in the queue.
    wake: Arc<Condvar>,
}

/// The lock state of one oid.
#[derive(Debug, Default)]
struct Entry {
    /// The number of requests that hold the lock.
    holders: usize,
    /// True when the holder holds the lock in exclusive mode.
    exclusive: bool,
    /// The requests that wait, in arrival order.
    queue: VecDeque<Waiter>,
}

impl Entry {
    /// Tell if the holders let a request in `mode` hold the lock with
    /// them.
    fn admits(&self, mode: Mode) -> bool {
        self.holders == 0 || (!self.exclusive && mode == Mode::Shared)
    }

    fn take(&mut self, mode: Mode) {
        self.holders += 1;
        self.exclusive = mode == Mode::Exclusive;
    }

    /// Grant the lock to the waiters at the head of the queue, in arrival
    /// order, while the holders admit them. Wake each waiter that gets the
    /// lock.
    fn grant(&mut self) {
        while self
            .queue
            .front()
            .is_some_and(|head| self.admits(head.mode))
        {
            if let Some(head) = self.queue.pop_front() {
                self.take(head.mode);
                head.wake.notify_one();
            }
        }
    }

    fn waits(&self, wake: &Arc<Condvar>) -> bool {
        self.queue
            .iter()
            .any(|waiter| Arc::ptr_eq(&waiter.wake, wake))
    }

    fn is_unused(&self) -> bool {
        self.holders == 0 && self.queue.is_empty()
    }
}

/// The lock table. It has an entry only for an oid that a request holds or
/// waits for.
#[derive(Debug, Default)]
struct Table {
    entries: HashMap<Oid, Entry>,
}

impl Table {
    /// Tell if the request that sleeps on `wake` waits for `oid`.
    fn waits(&self, oid: Oid, wake: &Arc<Condvar>) -> bool {
        self.entries
            .get(&oid)
            .is_some_and(|entry| entry.waits(wake))
    }

    /// Remove the request that sleeps on `wake` from the queue of `oid`.
    /// The waiters behind it can then get the lock.
    fn withdraw(&mut self, oid: Oid, wake: &Arc<Condvar>) {
        if let Some(entry) = self.entries.get_mut(&oid) {
            entry
                .queue
                .retain(|waiter| !Arc::ptr_eq(&waiter.wake, wake));
            entry.grant();
            if entry.is_unused() {
                self.entries.remove(&oid);
            }
        }
    }

    /// Release one hold on each lock of `held`, and grant each lock to
    /// the waiters that it admits.
    fn release(&mut self, held: &[Held]) {
        for &Held { oid, .. } in held {
            if let Some(entry) = self.entries.get_mut(&oid) {
                entry.holders -= 1;
                entry.grant();
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
#[derive(Debug)]
pub(crate) struct Locks<'m> {
    manager: &'m LockManager,
    held: Vec<Held>,
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
}

/// An `acquire` call that failed.
#[derive(Debug)]
pub(crate) struct Refused {
    pub(crate) error: ZoeError,
    /// The locks that the call took and released at a lock-wait timeout.
    /// The list is empty for other errors.
    pub(crate) released: Vec<Held>,
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
    /// from its start on the monotonic clock. If it still waits at that
    /// time, it leaves the queue, releases the locks that it took and
    /// fails with `Conflict` on the oid that it waits for. `Refused` then
    /// gives the released locks.
    ///
    /// Each lock keeps the time of `clock` when the call took it. The call
    /// reads `clock` at its start and after each wait.
    pub(crate) fn acquire(
        &self,
        requests: &[(Oid, Mode)],
        timeout: Duration,
        clock: &dyn Clock,
    ) -> Result<Locks<'_>, Refused> {
        if let Some(pair) = requests.windows(2).find(|pair| pair[0].0 >= pair[1].0) {
            return Err(Refused {
                error: ZoeError::LockOrder { oid: pair[1].0 },
                released: Vec::new(),
            });
        }
        let start = Instant::now();
        let mut since = clock.now();
        let mut locks = Locks {
            manager: self,
            held: Vec::with_capacity(requests.len()),
        };
        let mut table = self.table();
        // The call waits for one oid at a time, so one condition variable
        // is sufficient.
        let mut wake: Option<Arc<Condvar>> = None;
        for &(oid, mode) in requests {
            let entry = table.entries.entry(oid).or_default();
            if entry.queue.is_empty() && entry.admits(mode) {
                entry.take(mode);
            } else {
                let wake: &Arc<Condvar> = wake.get_or_insert_default();
                entry.queue.push_back(Waiter {
                    mode,
                    wake: Arc::clone(wake),
                });
                let left = timeout.saturating_sub(start.elapsed());
                table = wake
                    .wait_timeout_while(table, left, |table| table.waits(oid, wake))
                    .unwrap_or_else(PoisonError::into_inner)
                    .0;
                // A grant removes the request from the queue. A request
                // that is still in the queue has used all its time.
                if table.waits(oid, wake) {
                    table.withdraw(oid, wake);
                    table.release(&locks.held);
                    return Err(Refused {
                        error: ZoeError::Conflict { oid },
                        released: std::mem::take(&mut locks.held),
                    });
                }
                since = clock.now();
            }
            locks.held.push(Held { oid, since });
        }
        Ok(locks)
    }

    /// Release one hold on each lock of `held`. `held` must come from
    /// `Locks::keep`.
    pub(crate) fn release(&self, held: &[Held]) {
        if !held.is_empty() {
            self.table().release(held);
        }
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
}
