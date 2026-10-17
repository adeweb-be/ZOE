//! The lock hand-off (ADR 0044).
//!
//! When the finish of a transaction starts, the transaction cannot abort.
//! Its staged records are the records that its commit batch writes. So the
//! lock of an oid that it writes can pass to the exclusive request at the
//! head of the queue of the oid, before the commit is durable. That request
//! gets a copy of the staged record of the holder as the committed state of
//! the oid. Its vote merges onto that record, and its finish starts after
//! the finish of the holder, so its commit comes after the commit of the
//! holder.
//!
//! The entry of the oid keeps the hand-off until the store holds the last
//! commit of the oid: the commit of the holder is durable or failed, or a
//! later commit of a new owner is durable. Until then, only an exclusive
//! request takes the lock of the oid, and it takes the hand-off with it. A
//! shared request waits. So no request reads the oid from the store while
//! the store does not hold the last commit of it.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use super::LockCancel;
use crate::clock::{Clock, Waiter};
use crate::types::{Oid, Tid};

/// A commit whose finish started. Its batch gives it a tid, then it
/// becomes durable or fails.
pub(crate) struct InFlight {
    state: Mutex<State>,
    /// The user of the transaction, as `tpc_begin` gave it.
    pub(crate) user: Vec<u8>,
    /// The description of the transaction, as `tpc_begin` gave it.
    pub(crate) description: Vec<u8>,
}

#[derive(Default)]
struct State {
    tid: Option<Tid>,
    /// `Some(true)` when the commit is durable, `Some(false)` when it
    /// failed.
    end: Option<bool>,
    /// The waits for the end of the commit.
    waiters: Vec<Arc<dyn Waiter>>,
}

/// How a wait for a commit in flight ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Waited {
    /// The commit is durable.
    Durable,
    /// The commit failed.
    Failed,
    /// The deadline came first.
    TimedOut,
    /// A `LockCancel` ended the wait.
    Cancelled,
}

impl InFlight {
    pub(crate) fn new(user: &[u8], description: &[u8]) -> Self {
        Self {
            state: Mutex::default(),
            user: user.to_vec(),
            description: description.to_vec(),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Give the commit its tid. The leader of its batch calls it under the
    /// commit sequencer.
    pub(crate) fn assign(&self, tid: Tid) {
        self.state().tid = Some(tid);
    }

    /// Return the tid of the commit, once its batch gave it one.
    pub(crate) fn tid(&self) -> Option<Tid> {
        self.state().tid
    }

    /// Tell if the commit is durable.
    #[cfg(test)]
    pub(crate) fn is_durable(&self) -> bool {
        self.state().end == Some(true)
    }

    /// Tell if the commit is durable (`Some(true)`) or failed
    /// (`Some(false)`). `None` while it is in flight.
    pub(crate) fn ended(&self) -> Option<bool> {
        self.state().end
    }

    /// End the commit: durable when `durable` is true, else failed. Only
    /// the first call counts. The call wakes each wait, without the mutex.
    pub(crate) fn end(&self, durable: bool) {
        let mut state = self.state();
        if state.end.is_some() {
            return;
        }
        state.end = Some(durable);
        let waiters = std::mem::take(&mut state.waiters);
        drop(state);
        for waiter in waiters {
            waiter.wake();
        }
    }

    /// Wait until the commit is durable or failed, at most until
    /// `deadline` on the monotonic time of `clock`. A `cancel` ends the
    /// wait as the deadline does. The wait runs in the span `lock_wait` of
    /// `oid`, because it counts in the lock timeout of the vote.
    pub(crate) fn wait(
        &self,
        oid: Oid,
        deadline: Duration,
        clock: &dyn Clock,
        cancel: Option<&LockCancel>,
    ) -> Waited {
        let ended = |end| {
            if end { Waited::Durable } else { Waited::Failed }
        };
        if let Some(end) = self.ended() {
            return ended(end);
        }
        // The waiter comes from the clock, without the mutex.
        let waiter = clock.waiter();
        let _watch = cancel.map(|cancel| cancel.watch(&waiter));
        {
            let mut state = self.state();
            if let Some(end) = state.end {
                return ended(end);
            }
            state.waiters.push(Arc::clone(&waiter));
        }
        let _span = tracing::info_span!("lock_wait", oid = oid.0).entered();
        let waited = loop {
            if let Some(end) = self.ended() {
                break ended(end);
            }
            if cancel.is_some_and(LockCancel::is_cancelled) {
                break Waited::Cancelled;
            }
            let left = deadline.saturating_sub(clock.monotonic());
            if left.is_zero() {
                break Waited::TimedOut;
            }
            waiter.wait(left);
        };
        let mut state = self.state();
        state.waiters.retain(|other| !Arc::ptr_eq(other, &waiter));
        waited
    }
}

/// The lock of one oid that a holder handed off, with the record that the
/// holder writes for the oid.
pub(crate) struct Handoff {
    /// The commit of the holder.
    pub(crate) commit: Arc<InFlight>,
    /// A copy of the staged pickle of the holder. The new owner never reads
    /// the spill file of the holder, which goes away when the transaction
    /// of the holder ends.
    pub(crate) record: Vec<u8>,
}

#[cfg(test)]
mod tests {
    use std::thread;

    use super::*;
    use crate::clock::{ManualClock, SystemClock};

    #[test]
    fn a_wait_ends_at_the_end_of_the_commit() {
        let commit = Arc::new(InFlight::new(b"u", b"d"));
        assert_eq!(commit.tid(), None);
        commit.assign(Tid(7));
        thread::scope(|scope| {
            let waiting = scope.spawn(|| {
                let deadline = SystemClock.monotonic() + Duration::from_secs(10);
                commit.wait(Oid(1), deadline, &SystemClock, None)
            });
            thread::sleep(Duration::from_millis(10));
            commit.end(true);
            assert_eq!(waiting.join().unwrap(), Waited::Durable);
        });
        assert!(commit.is_durable());
        assert_eq!(commit.tid(), Some(Tid(7)));
        // Only the first end counts.
        commit.end(false);
        assert_eq!(commit.ended(), Some(true));
    }

    #[test]
    fn a_wait_ends_at_its_deadline_or_at_a_cancel() {
        let commit = InFlight::new(b"", b"");
        let clock = ManualClock::new();
        // The deadline is past, so the wait ends at once.
        let deadline = clock.monotonic();
        assert_eq!(
            commit.wait(Oid(1), deadline, &clock, None),
            Waited::TimedOut
        );
        let cancel = LockCancel::new();
        cancel.cancel();
        let later = SystemClock.monotonic() + Duration::from_secs(10);
        assert_eq!(
            commit.wait(Oid(1), later, &SystemClock, Some(&cancel)),
            Waited::Cancelled
        );
        commit.end(false);
        assert_eq!(
            commit.wait(Oid(1), later, &SystemClock, None),
            Waited::Failed
        );
    }
}
