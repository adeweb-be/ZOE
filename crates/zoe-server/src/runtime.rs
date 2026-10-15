//! The tasks, the threads and the time of the server.
//!
//! The code of a connection reaches its tasks, its blocking threads and
//! the time only through the trait `Runtime`. `Tokio` is the runtime of
//! production. The simulator gives its own runtime, so the same connection
//! code runs on simulated tasks, threads and time.
//!
//! A thread of `spawn_blocking` waits for a channel only through
//! [`block_on`]. It waits on the waiter of its thread: a waiter of the
//! operating system by default, or the waiter that the runtime gives to
//! the thread.

use std::cell::RefCell;
use std::future::Future;
use std::pin::{Pin, pin};
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll, Wake, Waker};
use std::time::{Duration, Instant};

use zoe_core::{Clock, SystemClock, Waiter};

/// A future that a runtime gives or runs.
pub(crate) type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// The tasks, the blocking threads and the time of the server.
pub(crate) trait Runtime: Send + Sync + 'static {
    /// Run `task` until it ends. The caller does not wait for the task.
    fn spawn(&self, task: BoxFuture<()>);

    /// Run `work` on a thread that can block, as a core call does. The
    /// caller does not wait for the thread.
    fn spawn_blocking(&self, work: Box<dyn FnOnce() + Send>);

    /// Return a future that ends after `duration`.
    fn sleep(&self, duration: Duration) -> BoxFuture<()>;

    /// Return the time since a fixed origin, on the clock of `sleep`. It
    /// never goes back.
    fn now(&self) -> Duration;
}

/// The runtime of production: the tasks, the blocking pool and the timers
/// of the current tokio runtime.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Tokio;

impl Runtime for Tokio {
    fn spawn(&self, task: BoxFuture<()>) {
        tokio::spawn(task);
    }

    fn spawn_blocking(&self, work: Box<dyn FnOnce() + Send>) {
        tokio::task::spawn_blocking(work);
    }

    fn sleep(&self, duration: Duration) -> BoxFuture<()> {
        Box::pin(tokio::time::sleep(duration))
    }

    fn now(&self) -> Duration {
        static ORIGIN: OnceLock<Instant> = OnceLock::new();
        ORIGIN.get_or_init(Instant::now).elapsed()
    }
}

/// A runtime that counts the handoffs of the connection code: each task
/// that starts, each blocking thread that starts, and each wake of a task
/// that it started. The tasks and the threads run on `Tokio`.
#[cfg(test)]
#[derive(Clone, Default)]
pub(crate) struct Counting(Arc<Handoffs>);

/// The handoffs that a `Counting` runtime saw.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct Handoffs {
    tasks: std::sync::atomic::AtomicUsize,
    threads: std::sync::atomic::AtomicUsize,
    wakes: std::sync::atomic::AtomicUsize,
}

#[cfg(test)]
impl Counting {
    /// Return the handoffs so far: the tasks and the threads that started,
    /// and the wakes of the tasks.
    pub(crate) fn handoffs(&self) -> usize {
        use std::sync::atomic::Ordering::SeqCst;
        self.0.tasks.load(SeqCst) + self.0.threads.load(SeqCst) + self.0.wakes.load(SeqCst)
    }
}

#[cfg(test)]
impl Runtime for Counting {
    fn spawn(&self, task: BoxFuture<()>) {
        use std::sync::atomic::Ordering::SeqCst;
        self.0.tasks.fetch_add(1, SeqCst);
        let handoffs = Arc::clone(&self.0);
        let mut task = task;
        let mut first = true;
        Tokio.spawn(Box::pin(std::future::poll_fn(move |cx| {
            // The first poll is the start of the task, and each later poll
            // follows a wake.
            if !std::mem::take(&mut first) {
                handoffs.wakes.fetch_add(1, SeqCst);
            }
            task.as_mut().poll(cx)
        })));
    }

    fn spawn_blocking(&self, work: Box<dyn FnOnce() + Send>) {
        self.0
            .threads
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Tokio.spawn_blocking(work);
    }

    fn sleep(&self, duration: Duration) -> BoxFuture<()> {
        Tokio.sleep(duration)
    }

    fn now(&self) -> Duration {
        Tokio.now()
    }
}

thread_local! {
    /// The waiter of this thread for `block_on`. It is set at the first
    /// use, or by the runtime that started the thread.
    static WAITER: RefCell<Option<Arc<dyn Waiter>>> = const { RefCell::new(None) };
}

/// Make `block_on` on this thread wait on `waiter`. A runtime calls this at
/// the start of a thread that it starts.
#[cfg(feature = "sim")]
pub(crate) fn set_waiter(waiter: Arc<dyn Waiter>) {
    WAITER.with_borrow_mut(|slot| *slot = Some(waiter));
}

/// The waker of a thread in `block_on`: it wakes the waiter of the thread.
struct WakeWaiter(Arc<dyn Waiter>);

impl Wake for WakeWaiter {
    fn wake(self: Arc<Self>) {
        self.0.wake();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.wake();
    }
}

/// Run `future` on this thread until it ends, and return its output. The
/// thread waits on its waiter while the future cannot go on.
///
/// Call this only on a thread that is not a thread of an async runtime.
pub(crate) fn block_on<F: Future>(future: F) -> F::Output {
    let waiter =
        WAITER.with_borrow_mut(|slot| Arc::clone(slot.get_or_insert_with(|| SystemClock.waiter())));
    let waker = Waker::from(Arc::new(WakeWaiter(Arc::clone(&waiter))));
    let mut cx = Context::from_waker(&waker);
    let mut future = pin!(future);
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut cx) {
            return output;
        }
        waiter.wait(Duration::MAX);
    }
}

#[cfg(test)]
mod tests {
    use tokio::sync::mpsc;

    use super::*;

    #[test]
    fn block_on_waits_until_another_thread_sends() {
        let (sender, mut receiver) = mpsc::channel(1);
        let sending = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            sender.blocking_send(7).unwrap();
        });
        let start = Instant::now();
        assert_eq!(block_on(receiver.recv()), Some(7));
        assert!(start.elapsed() >= Duration::from_millis(40));
        sending.join().unwrap();
        // The sender is gone, so the channel ends.
        assert_eq!(block_on(receiver.recv()), None);
    }

    #[test]
    fn block_on_gives_a_ready_output_at_once() {
        assert_eq!(block_on(async { 3 }), 3);
    }
}
