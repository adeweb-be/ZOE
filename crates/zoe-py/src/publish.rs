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
//! before it is published. No lock is held while a callback runs. A
//! callback cannot commit in its own storage: that commit would wait for
//! the callback that runs it.

use std::cell::RefCell;
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};

use zoe_core::Tid;

/// The last published tid of a store.
pub(crate) struct Published {
    last: Mutex<Tid>,
    changed: Condvar,
}

thread_local! {
    /// The addresses of the `Published` values whose callbacks run on this
    /// thread.
    static CALLBACKS: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
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

    fn key(&self) -> usize {
        std::ptr::from_ref(self) as usize
    }

    /// Tell if a callback of this store runs on this thread.
    pub(crate) fn in_callback(&self) -> bool {
        CALLBACKS.with_borrow(|running| running.contains(&self.key()))
    }

    /// Wait until `tid` is published, and return the last published tid.
    ///
    /// A callback that asks for the last tid does not wait, because the
    /// commit of its own callback is not published yet. It gets the last
    /// published tid.
    pub(crate) fn wait_for(&self, tid: Tid) -> Tid {
        if self.in_callback() {
            return *self.lock();
        }
        let last = self.lock();
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
    /// the callback does not wait, and `in_callback` is true in it.
    pub(crate) fn run<T>(&self, callback: impl FnOnce() -> T) -> T {
        struct Running(usize);
        impl Drop for Running {
            fn drop(&mut self) {
                CALLBACKS.with_borrow_mut(|running| {
                    if let Some(at) = running.iter().rposition(|&key| key == self.0) {
                        running.remove(at);
                    }
                });
            }
        }
        let key = self.published.key();
        CALLBACKS.with_borrow_mut(|running| running.push(key));
        let _running = Running(key);
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

    #[test]
    fn a_callback_is_known_only_to_its_store_and_thread() {
        let (mine, other) = (Published::new(Tid(1)), Published::new(Tid(1)));
        let publication = Publication::after(&mine, Tid(1), Tid(2));
        let seen = publication.run(|| {
            let elsewhere = thread::scope(|scope| scope.spawn(|| mine.in_callback()).join());
            (mine.in_callback(), other.in_callback(), elsewhere.unwrap())
        });
        assert_eq!(seen, (true, false, false));
        assert!(!mine.in_callback());
    }
}
