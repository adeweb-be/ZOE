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
//! The transaction holds the receiver of each job. An abort, a failed
//! vote and a lost connection drop the receivers. A job whose receiver is
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
}

#[derive(Default)]
struct State {
    /// The jobs that wait for a thread, in the order of their queue.
    queue: VecDeque<Work>,
    /// The threads of the pool that run.
    threads: usize,
    /// The threads that encode a job. The others take the next job.
    encoding: usize,
}

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
    encoded: oneshot::Receiver<Encoded>,
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
        let start = {
            let mut state = self.0.lock();
            state.queue.push_back(Work {
                #[cfg(test)]
                oid,
                data,
                compression,
                sender,
            });
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
        Job { oid, encoded }
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
        };
        runtime.spawn_blocking(Box::new(move || running.run()));
    }
}

/// A thread of the pool. Its drop counts its end. After a panic, the drop
/// starts another thread when jobs wait, so no job waits for ever.
struct Running {
    pool: Arc<Shared>,
    runtime: Arc<dyn Runtime>,
}

impl Running {
    /// Run the jobs of the queue until it is empty.
    fn run(&self) {
        loop {
            let work = {
                let mut state = self.pool.lock();
                match state.queue.pop_front() {
                    Some(work) => {
                        state.encoding += 1;
                        work
                    }
                    None => return,
                }
            };
            let skip = work.sender.is_closed();
            #[cfg(test)]
            let skip = skip || self.pool.hook.as_ref().is_some_and(|hook| !hook(work.oid));
            if !skip {
                let encoded = Encoded::encode(&work.data, work.compression);
                let _ = work.sender.send(encoded);
            }
            self.pool.lock().encoding -= 1;
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let mut state = self.pool.lock();
        if std::thread::panicking() {
            state.encoding -= 1;
            if !state.queue.is_empty() {
                drop(state);
                self.pool.start(&self.runtime);
                return;
            }
        }
        state.threads -= 1;
    }
}

impl Job {
    /// Wait until the job ends, and return the oid and its payload.
    pub(crate) fn wait(self) -> Result<(Oid, Encoded), Failed> {
        let oid = self.oid;
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
