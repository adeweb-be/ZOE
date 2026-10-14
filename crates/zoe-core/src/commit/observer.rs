//! The observer of a store: a hook for the metrics of a skin.
//!
//! The core calls the observer of a store after a sync of a commit batch
//! and after the lock acquisition of each vote. The core keeps no metric
//! and has no metrics dependency: the skin counts what it gets. Each
//! method has a default that does nothing.

use std::sync::Arc;
use std::time::Duration;

use crate::store::Store;

/// What a store tells its observer. The calls come from the threads that
/// commit, so a method must be fast and must not block. A method must not
/// panic: the leader of a batch calls `batch_synced` before it gives the
/// members of the batch their outcomes and wakes the waiting finishes.
pub trait Observer: Send + Sync {
    /// One backend commit, with one sync, made `commits` commits durable.
    /// The leader of the batch calls it after it released the commit
    /// sequencer.
    fn batch_synced(&self, commits: usize) {
        let _ = commits;
    }

    /// A vote spent `wait` to take its locks, measured on the monotonic
    /// time of the store clock (`Clock::monotonic`). A vote that waited
    /// past the lock timeout calls it too.
    fn locks_acquired(&self, wait: Duration) {
        let _ = wait;
    }
}

impl Store {
    /// Give the store its observer. Only the first call sets it. Return
    /// false when the store has an observer already.
    pub fn set_observer(&self, observer: Arc<dyn Observer>) -> bool {
        self.group.observer.set(observer).is_ok()
    }

    /// Return the observer of the store, if it has one.
    pub(crate) fn observer(&self) -> Option<&dyn Observer> {
        self.group.observer.get().map(|observer| &**observer)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::types::Tid;
    use crate::vfs::MemVfs;

    /// An observer that keeps what it gets.
    #[derive(Default)]
    struct Kept {
        batches: Mutex<Vec<usize>>,
        waits: Mutex<Vec<Duration>>,
    }

    impl Observer for Kept {
        fn batch_synced(&self, commits: usize) {
            self.batches.lock().unwrap().push(commits);
        }

        fn locks_acquired(&self, wait: Duration) {
            self.waits.lock().unwrap().push(wait);
        }
    }

    #[test]
    fn the_observer_gets_each_batch_and_each_lock_acquisition() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let kept = Arc::new(Kept::default());
        assert!(store.set_observer(kept.clone()));
        assert!(!store.set_observer(Arc::new(Kept::default())));

        let oid = store.new_oid().unwrap();
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.store(oid, Tid::ZERO, b"a").unwrap();
        txn.vote().unwrap();
        txn.finish().unwrap();
        assert_eq!(*kept.batches.lock().unwrap(), [1]);
        assert_eq!(kept.waits.lock().unwrap().len(), 1);

        // A vote that fails on a stale serial took its locks. Its
        // transaction syncs nothing.
        let mut stale = store.tpc_begin(b"", b"", b"").unwrap();
        stale.store(oid, Tid::ZERO, b"b").unwrap();
        assert!(stale.vote().is_err());
        assert_eq!(kept.waits.lock().unwrap().len(), 2);
        assert_eq!(*kept.batches.lock().unwrap(), [1]);
    }

    #[test]
    fn a_wait_past_the_lock_timeout_is_observed_with_its_length() {
        let mut store = Store::open_vfs(MemVfs::new()).unwrap();
        store.lock_timeout = Duration::from_millis(50);
        let kept = Arc::new(Kept::default());
        store.set_observer(kept.clone());
        let oid = store.new_oid().unwrap();
        let mut holder = store.tpc_begin(b"", b"", b"").unwrap();
        holder.store(oid, Tid::ZERO, b"a").unwrap();
        holder.vote().unwrap();

        std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    let mut waiter = store.tpc_begin(b"", b"", b"").unwrap();
                    waiter.store(oid, Tid::ZERO, b"b").unwrap();
                    assert!(waiter.vote().is_err());
                })
                .join()
                .unwrap();
        });
        let waits = kept.waits.lock().unwrap();
        assert_eq!(waits.len(), 2);
        assert!(waits[1] >= Duration::from_millis(50), "{waits:?}");
    }
}
