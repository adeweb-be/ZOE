//! The per-OID lock manager. This module is a red zone.
//!
//! A transaction takes exclusive locks on the oids it writes, in ascending
//! oid order, so that two transactions cannot deadlock on each other. A
//! transaction that must wait keeps the locks it holds and waits once, up
//! to the lock timeout.

use std::collections::HashSet;
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::error::{Result, ZoeError};
use crate::types::Oid;

/// The exclusive locks that transactions hold.
#[derive(Debug, Default)]
pub(crate) struct LockManager {
    held: Mutex<HashSet<Oid>>,
    released: Condvar,
}

impl LockManager {
    fn held(&self) -> MutexGuard<'_, HashSet<Oid>> {
        self.held.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Lock every oid of `oids`, which are in ascending order.
    ///
    /// A wait longer than `timeout` releases the locks this call took and
    /// returns a conflict on the oid it waited for.
    pub(crate) fn acquire(&self, oids: &[Oid], timeout: Duration) -> Result<()> {
        debug_assert!(oids.windows(2).all(|w| w[0] < w[1]));
        let deadline = Instant::now() + timeout;
        let mut held = self.held();
        for (i, &oid) in oids.iter().enumerate() {
            while held.contains(&oid) {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    for taken in &oids[..i] {
                        held.remove(taken);
                    }
                    self.released.notify_all();
                    return Err(ZoeError::Conflict { oid });
                }
                held = self
                    .released
                    .wait_timeout(held, left)
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
            }
            held.insert(oid);
        }
        Ok(())
    }

    /// Release the locks on `oids`.
    pub(crate) fn release(&self, oids: &[Oid]) {
        if oids.is_empty() {
            return;
        }
        let mut held = self.held();
        for oid in oids {
            held.remove(oid);
        }
        self.released.notify_all();
    }

    /// Tell if no lock is held.
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.held().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::thread;

    use super::*;

    #[test]
    fn a_free_lock_is_taken_and_released() {
        let locks = LockManager::default();
        locks.acquire(&[Oid(1), Oid(2)], Duration::ZERO).unwrap();
        assert!(locks.acquire(&[Oid(2)], Duration::ZERO).is_err());
        locks.release(&[Oid(1), Oid(2)]);
        assert!(locks.is_empty());
    }

    #[test]
    fn a_timeout_releases_what_the_call_took() {
        let locks = LockManager::default();
        locks.acquire(&[Oid(2)], Duration::ZERO).unwrap();
        let error = locks.acquire(&[Oid(1), Oid(2)], Duration::from_millis(10));
        assert!(matches!(error, Err(ZoeError::Conflict { oid: Oid(2) })));
        locks.acquire(&[Oid(1)], Duration::ZERO).unwrap();
    }

    #[test]
    fn a_waiter_gets_the_lock_after_the_release() {
        let locks = Arc::new(LockManager::default());
        locks.acquire(&[Oid(1)], Duration::ZERO).unwrap();
        let waiter = {
            let locks = Arc::clone(&locks);
            thread::spawn(move || locks.acquire(&[Oid(1)], Duration::from_secs(10)))
        };
        thread::sleep(Duration::from_millis(20));
        locks.release(&[Oid(1)]);
        waiter.join().unwrap().unwrap();
        assert!(!locks.is_empty());
    }
}
