//! The publication of commits to ZODB, in commit order.
//!
//! The core makes a commit visible before `tpc_finish` returns. Then
//! `tpc_finish` calls the callback of ZODB, which sends the invalidations
//! of the commit to the other connections. A commit is published when its
//! callback has run. `lastTransaction` gives only a published tid. So a
//! connection never starts a snapshot after a commit whose invalidations it
//! did not get.
//!
//! The callbacks run in commit order: a commit waits until the commit
//! before it is published. No lock is held while a callback runs.

use std::cell::Cell;
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};

use zoe_core::Tid;

/// The last published tid of a store.
pub(crate) struct Published {
    last: Mutex<Tid>,
    changed: Condvar,
}

thread_local! {
    /// The number of callbacks that run on this thread.
    static CALLBACKS: Cell<usize> = const { Cell::new(0) };
}

impl Published {
    /// Start with `tid` published: the last tid of the store when it
    /// opens.
    pub(crate) fn new(tid: Tid) -> Self {
        Self {
            last: Mutex::new(tid),
            changed: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Tid> {
        self.last.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Wait until `tid` is published, and return the last published tid.
    ///
    /// A callback that asks for the last tid does not wait, because the
    /// commit of its own callback is not published yet. It gets the last
    /// published tid.
    pub(crate) fn wait_for(&self, tid: Tid) -> Tid {
        let last = self.lock();
        if CALLBACKS.get() > 0 {
            return *last;
        }
        *self
            .changed
            .wait_while(last, |last| *last < tid)
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Publish `tid`, the commit after the last published one.
    fn publish(&self, tid: Tid) {
        let mut last = self.lock();
        *last = (*last).max(tid);
        drop(last);
        self.changed.notify_all();
    }
}

/// The publication of one commit. Dropping it publishes the commit, also
/// when the callback fails or panics.
pub(crate) struct Publication<'p> {
    published: &'p Published,
    tid: Tid,
}

impl<'p> Publication<'p> {
    /// Wait until `previous`, the commit before `tid`, is published.
    /// Then return the publication of `tid`.
    pub(crate) fn after(published: &'p Published, previous: Tid, tid: Tid) -> Self {
        published.wait_for(previous);
        Self { published, tid }
    }

    /// Return the tid of the commit.
    pub(crate) fn tid(&self) -> Tid {
        self.tid
    }

    /// Run `callback`, the callback of the commit. A call to `wait_for` in
    /// the callback does not wait.
    pub(crate) fn run<T>(&self, callback: impl FnOnce() -> T) -> T {
        struct Running;
        impl Drop for Running {
            fn drop(&mut self) {
                CALLBACKS.set(CALLBACKS.get() - 1);
            }
        }
        CALLBACKS.set(CALLBACKS.get() + 1);
        let _running = Running;
        callback()
    }
}

impl Drop for Publication<'_> {
    fn drop(&mut self) {
        self.published.publish(self.tid);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use super::*;

    #[test]
    fn a_commit_waits_for_the_commit_before_it() {
        let published = Published::new(Tid(1));
        let (done, order) = mpsc::channel();
        thread::scope(|scope| {
            let (published, later) = (&published, done.clone());
            scope.spawn(move || {
                let publication = Publication::after(published, Tid(2), Tid(3));
                later.send(3).unwrap();
                drop(publication);
            });
            thread::sleep(Duration::from_millis(50));
            let publication = Publication::after(published, Tid(1), Tid(2));
            done.send(2).unwrap();
            drop(publication);
        });
        assert_eq!(order.try_iter().collect::<Vec<_>>(), [2, 3]);
        assert_eq!(published.wait_for(Tid(3)), Tid(3));
    }

    #[test]
    fn a_reader_waits_until_the_tid_is_published() {
        let published = Published::new(Tid(1));
        let publication = Publication::after(&published, Tid(1), Tid(2));
        thread::scope(|scope| {
            let reader = scope.spawn(|| published.wait_for(Tid(2)));
            thread::sleep(Duration::from_millis(50));
            assert!(!reader.is_finished());
            drop(publication);
            assert_eq!(reader.join().unwrap(), Tid(2));
        });
    }

    #[test]
    fn a_callback_does_not_wait_for_its_own_commit() {
        let published = Published::new(Tid(1));
        let publication = Publication::after(&published, Tid(1), Tid(2));
        assert_eq!(publication.run(|| published.wait_for(Tid(2))), Tid(1));
        drop(publication);
        assert_eq!(published.wait_for(Tid(2)), Tid(2));
    }
}
