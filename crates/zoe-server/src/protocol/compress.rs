//! The compression pool: the threads that encode the staged pickles of the
//! transactions, off the path of the transaction.
//!
//! All the connections of the server share one pool: one queue of jobs,
//! and at most its number of threads, the number of cores by default,
//! that run them in the order of the queue, on blocking threads of the
//! runtime. A thread of the pool runs the jobs of the queue one after the
//! other, with the zstd context of its thread, and ends when the queue is
//! empty. A new job starts a thread only when it waits behind the jobs
//! that the threads run. So a stream of small jobs wakes few threads.
//!
//! `storea` and `restorea` stage the pickle in the core, queue a job and
//! reply at once. The job encodes the pickle with the compression of the
//! store (`zoe_core::Encoded`). The vote waits for each job of its
//! transaction before it takes a lock, and attaches each result to its
//! staged record. Then the finish writes these payloads and encodes no
//! pickle again. A job that fails fails the vote.
//!
//! A job sits in a slot that a thread of the pool and the vote can both
//! take, under the lock of the slot. The vote takes each job of its
//! transaction that no thread took yet, and encodes it itself. It waits
//! only for a job that a thread encodes. So a vote never waits for a
//! thread that does not start: when the blocking pool of tokio is full,
//! or when the runtime stops and drops a thread before it runs. The queue
//! only moves the work off the path of the transaction when a thread is
//! free.
//!
//! The transaction holds the receiver and the slot of each job. An abort,
//! a failed vote and a lost connection drop them. A job whose receiver is
//! gone does not encode when a thread takes it. A job that encodes already
//! ends, and nothing reads its result.

use std::collections::VecDeque;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tokio::sync::oneshot;
use zoe_core::{Compression, Encoded, Oid};

use crate::runtime::{Runtime, block_on};

/// What a test does in a job before it encodes: it can hold the job, and
/// it fails the job when it returns false.
#[cfg(test)]
pub(crate) type Hook = Arc<dyn Fn(Oid) -> bool + Send + Sync>;

/// The compression pool of all the connections.
#[derive(Clone)]
pub(crate) struct Compressor(Arc<Shared>);

struct Shared {
    /// The most threads that run jobs at once.
    threads: usize,
    state: Mutex<State>,
    #[cfg(test)]
    hook: Option<Hook>,
    /// What a test does in a thread after it left the pool, before it
    /// ends.
    #[cfg(test)]
    leaving: Option<Arc<dyn Fn() + Send + Sync>>,
}

#[derive(Default)]
struct State {
    /// The jobs that wait for a thread, in the order of their queue. A
    /// slot that the vote emptied stays here until a thread takes it.
    queue: VecDeque<Slot>,
    /// The threads of the pool that run.
    threads: usize,
    /// The threads that encode a job. The others take the next job.
    encoding: usize,
}

/// The job of one pickle, until a thread of the pool or the vote takes
/// it.
type Slot = Arc<Mutex<Option<Work>>>;

/// A job in the queue.
struct Work {
    #[cfg(test)]
    oid: Oid,
    data: Vec<u8>,
    compression: Compression,
    sender: oneshot::Sender<Encoded>,
}

/// The job of one staged pickle. The drop of the job cancels it.
pub(crate) struct Job {
    oid: Oid,
    slot: Slot,
    encoded: oneshot::Receiver<Encoded>,
    #[cfg(test)]
    hook: Option<Hook>,
}

/// A job that gave no result: its thread panicked, or the runtime dropped
/// it.
#[derive(Debug)]
pub(crate) struct Failed(pub(crate) Oid);

impl Compressor {
    /// A pool of `threads` threads.
    pub(crate) fn new(threads: NonZeroUsize) -> Self {
        Self(Arc::new(Shared {
            threads: threads.get(),
            state: Mutex::new(State::default()),
            #[cfg(test)]
            hook: None,
            #[cfg(test)]
            leaving: None,
        }))
    }

    /// Run `hook` in each job before it encodes. Call it before the pool
    /// has a clone.
    #[cfg(test)]
    pub(crate) fn with_hook(mut self, hook: Hook) -> Self {
        Arc::get_mut(&mut self.0)
            .expect("a pool without clones")
            .hook = Some(hook);
        self
    }

    /// Run `leaving` in each thread after it left the pool, before it
    /// ends. Call it before the pool has a clone.
    #[cfg(test)]
    pub(crate) fn with_leaving(mut self, leaving: Arc<dyn Fn() + Send + Sync>) -> Self {
        Arc::get_mut(&mut self.0)
            .expect("a pool without clones")
            .leaving = Some(leaving);
        self
    }

    /// Queue the encode of `data`, the staged pickle of `oid`, with
    /// `compression`. Start a thread of the pool on `runtime` when the job
    /// waits behind the jobs that the threads run, and the pool has fewer
    /// threads than its limit. The call does not wait.
    pub(crate) fn queue(
        &self,
        runtime: &Arc<dyn Runtime>,
        oid: Oid,
        data: Vec<u8>,
        compression: Compression,
    ) -> Job {
        let (sender, encoded) = oneshot::channel();
        let slot = Arc::new(Mutex::new(Some(Work {
            #[cfg(test)]
            oid,
            data,
            compression,
            sender,
        })));
        let start = {
            let mut state = self.0.lock();
            state.queue.push_back(Arc::clone(&slot));
            let free = state.threads - state.encoding;
            let start = state.queue.len() > free && state.threads < self.0.threads;
            if start {
                state.threads += 1;
            }
            start
        };
        if start {
            self.0.start(runtime);
        }
        Job {
            oid,
            slot,
            encoded,
            #[cfg(test)]
            hook: self.0.hook.clone(),
        }
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        // The state stays valid after a panic: `Running` counts the end
        // of its thread when it unwinds.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Start a thread of the pool, which `queue` counted already.
    fn start(self: &Arc<Self>, runtime: &Arc<dyn Runtime>) {
        let running = Running {
            pool: Arc::clone(self),
            runtime: Arc::clone(runtime),
            ran: false,
        };
        runtime.spawn_blocking(Box::new(move || running.run()));
    }
}

/// Take the job out of `slot`. Return `None` when a thread of the pool or
/// the vote took it already.
fn take(slot: &Slot) -> Option<Work> {
    slot.lock().unwrap_or_else(PoisonError::into_inner).take()
}

/// Encode `work`. Return `None` when the test hook fails the job.
fn encode(work: &Work, #[cfg(test)] hook: Option<&Hook>) -> Option<Encoded> {
    #[cfg(test)]
    if hook.is_some_and(|hook| !hook(work.oid)) {
        return None;
    }
    Some(Encoded::encode(&work.data, work.compression))
}

/// A thread of the pool. A drop of a thread that never ran, as at the
/// stop of the runtime, gives back its count. After a panic, its drop
/// starts another thread when jobs wait.
struct Running {
    pool: Arc<Shared>,
    runtime: Arc<dyn Runtime>,
    /// The thread started to run the jobs.
    ran: bool,
}

impl Running {
    /// Run the jobs of the queue until it is empty. The thread leaves the
    /// pool under the same lock as the check of the empty queue. So a job
    /// that `queue` adds after that check sees one thread less, and starts
    /// a thread.
    fn run(mut self) {
        self.ran = true;
        loop {
            let slot = {
                let mut state = self.pool.lock();
                match state.queue.pop_front() {
                    Some(slot) => {
                        state.encoding += 1;
                        slot
                    }
                    None => {
                        state.threads -= 1;
                        drop(state);
                        #[cfg(test)]
                        if let Some(leaving) = &self.pool.leaving {
                            leaving();
                        }
                        return;
                    }
                }
            };
            // The vote took the job, or the transaction ended.
            if let Some(work) = take(&slot).filter(|work| !work.sender.is_closed()) {
                let encoded = encode(
                    &work,
                    #[cfg(test)]
                    self.pool.hook.as_ref(),
                );
                if let Some(encoded) = encoded {
                    let _ = work.sender.send(encoded);
                }
            }
            self.pool.lock().encoding -= 1;
        }
    }
}

impl Drop for Running {
    /// A thread that never ran leaves the pool here. The jobs that wait
    /// stay in their slots, and their votes encode them, or a later job
    /// starts a thread. A thread that panics while it encodes leaves the
    /// pool here too. When jobs wait, another thread takes its place.
    fn drop(&mut self) {
        if !self.ran {
            self.pool.lock().threads -= 1;
            return;
        }
        if !std::thread::panicking() {
            return;
        }
        let mut state = self.pool.lock();
        state.encoding -= 1;
        if state.queue.is_empty() {
            state.threads -= 1;
        } else {
            drop(state);
            self.pool.start(&self.runtime);
        }
    }
}

impl Job {
    /// Return the oid and its payload. When no thread of the pool took the
    /// job yet, take it and encode it on this thread. Else wait until the
    /// thread ends the job.
    pub(crate) fn wait(self) -> Result<(Oid, Encoded), Failed> {
        let oid = self.oid;
        if let Some(work) = take(&self.slot) {
            return encode(
                &work,
                #[cfg(test)]
                self.hook.as_ref(),
            )
            .map(|encoded| (oid, encoded))
            .ok_or(Failed(oid));
        }
        block_on(self.encoded)
            .map(|encoded| (oid, encoded))
            .map_err(|_| Failed(oid))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use super::*;
    use crate::runtime::Tokio;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
    }

    fn data(n: usize) -> Vec<u8> {
        b"persistent mapping ".repeat(n)
    }

    #[test]
    fn a_job_gives_the_payload_of_an_inline_encode() {
        let tokio = runtime();
        let _entered = tokio.enter();
        let runtime: Arc<dyn Runtime> = Arc::new(Tokio);
        let pool = Compressor::new(NonZeroUsize::new(2).unwrap());
        let compression = Compression::default();
        let jobs: Vec<Job> = (1..=8)
            .map(|oid| pool.queue(&runtime, Oid(oid), data(oid as usize * 100), compression))
            .collect();
        for job in jobs {
            let (oid, encoded) = job.wait().unwrap();
            let inline = Encoded::encode(&data(oid.0 as usize * 100), compression);
            assert_eq!(format!("{encoded:?}"), format!("{inline:?}"));
        }
    }

    #[test]
    fn the_pool_runs_at_most_its_threads_at_once() {
        let tokio = runtime();
        let _entered = tokio.enter();
        let runtime: Arc<dyn Runtime> = Arc::new(Tokio);
        let running = Arc::new(AtomicUsize::new(0));
        let most = Arc::new(AtomicUsize::new(0));
        let hook: Hook = {
            let (running, most) = (Arc::clone(&running), Arc::clone(&most));
            Arc::new(move |_| {
                let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                most.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(20));
                running.fetch_sub(1, Ordering::SeqCst);
                true
            })
        };
        let pool = Compressor::new(NonZeroUsize::new(2).unwrap()).with_hook(hook);
        let jobs: Vec<Job> = (0..8)
            .map(|oid| pool.queue(&runtime, Oid(oid), data(10), Compression::default()))
            .collect();
        // The threads of the pool run every job before a wait, which would
        // take a job that waits.
        let deadline = Instant::now() + Duration::from_secs(30);
        while jobs.iter().any(|job| job.slot.lock().unwrap().is_some())
            || running.load(Ordering::SeqCst) > 0
        {
            assert!(Instant::now() < deadline, "the pool runs the jobs");
            std::thread::sleep(Duration::from_millis(5));
        }
        for job in jobs {
            job.wait().unwrap();
        }
        assert_eq!(most.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_dropped_job_that_waits_never_encodes() {
        let tokio = runtime();
        let _entered = tokio.enter();
        let runtime: Arc<dyn Runtime> = Arc::new(Tokio);
        let (release, held) = std::sync::mpsc::channel::<()>();
        let held = Mutex::new(held);
        let started = Arc::new(Mutex::new(Vec::new()));
        let hook: Hook = {
            let started = Arc::clone(&started);
            Arc::new(move |oid| {
                started.lock().unwrap().push(oid);
                if oid == Oid(1) {
                    let _ = held.lock().unwrap().recv();
                }
                true
            })
        };
        let pool = Compressor::new(NonZeroUsize::MIN).with_hook(hook);
        let first = pool.queue(&runtime, Oid(1), data(10), Compression::default());
        let deadline = Instant::now() + Duration::from_secs(30);
        while started.lock().unwrap().is_empty() {
            assert!(Instant::now() < deadline, "the first job starts");
            std::thread::sleep(Duration::from_millis(5));
        }
        // The second job waits for the one thread of the pool.
        let second = pool.queue(&runtime, Oid(2), data(10), Compression::default());
        drop(second);
        release.send(()).unwrap();
        first.wait().unwrap();
        // A third job runs after the thread is free, so the second had its
        // turn before it.
        let third = pool.queue(&runtime, Oid(3), data(10), Compression::default());
        third.wait().unwrap();
        assert_eq!(*started.lock().unwrap(), vec![Oid(1), Oid(3)]);
    }

    #[test]
    fn a_job_queued_while_a_thread_leaves_still_runs() {
        let tokio = runtime();
        let _entered = tokio.enter();
        let runtime: Arc<dyn Runtime> = Arc::new(Tokio);
        // The first thread that leaves the pool stops there until the test
        // lets it go.
        let (left, leaves) = std::sync::mpsc::channel::<()>();
        let (go, wait) = std::sync::mpsc::channel::<()>();
        let wait = Mutex::new(Some(wait));
        let leaving: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            if let Some(wait) = wait.lock().unwrap().take() {
                left.send(()).unwrap();
                let _ = wait.recv();
            }
        });
        let pool = Compressor::new(NonZeroUsize::MIN).with_leaving(leaving);
        let first = pool.queue(&runtime, Oid(1), data(10), Compression::default());
        first.wait().unwrap();
        leaves.recv_timeout(Duration::from_secs(30)).unwrap();
        // The thread has left but still runs. A new job starts a thread,
        // which takes it: the wait finds the slot empty.
        let second = pool.queue(&runtime, Oid(2), data(10), Compression::default());
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut taken = false;
        while !taken && Instant::now() < deadline {
            taken = second.slot.lock().unwrap().is_none();
            std::thread::sleep(Duration::from_millis(5));
        }
        go.send(()).unwrap();
        assert!(taken, "a thread of the pool takes the job");
        assert_eq!(second.wait().unwrap().0, Oid(2));
    }

    #[test]
    fn a_job_ends_when_the_blocking_pool_of_tokio_is_full() {
        // The runtime has one blocking thread. That thread queues a job and
        // waits for it, as a vote does, so the thread of the pool cannot
        // start.
        let tokio = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let pool = Compressor::new(NonZeroUsize::new(2).unwrap());
        let (done, ended) = std::sync::mpsc::channel();
        tokio.spawn_blocking(move || {
            let runtime: Arc<dyn Runtime> = Arc::new(Tokio);
            let job = pool.queue(&runtime, Oid(1), data(100), Compression::default());
            done.send(job.wait().map(|(oid, _)| oid)).unwrap();
        });
        let result = ended.recv_timeout(Duration::from_secs(30));
        assert_eq!(result.expect("the job ends").unwrap(), Oid(1));
    }

    /// A runtime that drops each blocking thread before it runs, as tokio
    /// does at its stop.
    struct Stopped;

    impl Runtime for Stopped {
        fn spawn(&self, _: crate::runtime::BoxFuture<()>) {}

        fn spawn_blocking(&self, work: Box<dyn FnOnce() + Send>) {
            drop(work);
        }

        fn sleep(&self, _: Duration) -> crate::runtime::BoxFuture<()> {
            Box::pin(std::future::pending())
        }

        fn now(&self) -> Duration {
            Duration::ZERO
        }
    }

    #[test]
    fn a_thread_dropped_before_it_runs_leaves_no_waiter_and_no_count() {
        let stopped: Arc<dyn Runtime> = Arc::new(Stopped);
        let pool = Compressor::new(NonZeroUsize::MIN);
        let first = pool.queue(&stopped, Oid(1), data(100), Compression::default());
        let second = pool.queue(&stopped, Oid(2), data(100), Compression::default());
        // The thread that the first job started never ran, and gave back
        // its count.
        assert_eq!(pool.0.lock().threads, 0);
        let (done, ended) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let oids = [first.wait(), second.wait()].map(|result| result.map(|(oid, _)| oid));
            done.send(oids).unwrap();
        });
        let oids = ended
            .recv_timeout(Duration::from_secs(30))
            .expect("the waits end");
        assert_eq!(oids.map(Result::unwrap), [Oid(1), Oid(2)]);
        // A later job on a runtime that runs its threads gets a thread.
        let tokio = runtime();
        let _entered = tokio.enter();
        let runtime: Arc<dyn Runtime> = Arc::new(Tokio);
        let third = pool.queue(&runtime, Oid(3), data(100), Compression::default());
        let deadline = Instant::now() + Duration::from_secs(30);
        while third.slot.lock().unwrap().is_some() {
            assert!(
                Instant::now() < deadline,
                "a thread of the pool takes the job"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(third.wait().unwrap().0, Oid(3));
    }

    #[test]
    fn a_job_that_fails_gives_its_oid() {
        let tokio = runtime();
        let _entered = tokio.enter();
        let runtime: Arc<dyn Runtime> = Arc::new(Tokio);
        let pool = Compressor::new(NonZeroUsize::MIN).with_hook(Arc::new(|oid| oid != Oid(7)));
        let failed = pool.queue(&runtime, Oid(7), data(10), Compression::default());
        let passed = pool.queue(&runtime, Oid(8), data(10), Compression::default());
        assert_eq!(failed.wait().unwrap_err().0, Oid(7));
        assert_eq!(passed.wait().unwrap().0, Oid(8));
    }
}
