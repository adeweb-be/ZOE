//! The helpers that the integration tests of zoe-core share. Each test
//! binary uses some of them. `zoe_core::testing` holds the helpers that
//! the unit tests use too.

#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use zoe_core::{Clock, SystemClock, Waiter};

/// Return a new empty directory for the test `name` of this test binary.
pub fn fresh_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "zoe-{}-{name}-{}",
        env!("CARGO_CRATE_NAME"),
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A clock of the system that counts the waiters that it gives and the
/// waits on them. Each wait of the core gets its waiter from the clock of
/// the store, so the counts tell when a vote waits.
#[derive(Clone, Default)]
pub struct CountingClock {
    /// The number of waiters that the clock gave.
    pub waiters: Arc<AtomicUsize>,
    /// The number of waits on these waiters.
    pub waits: Arc<AtomicUsize>,
}

impl Clock for CountingClock {
    fn now(&self) -> SystemTime {
        SystemClock.now()
    }

    fn waiter(&self) -> Arc<dyn Waiter> {
        self.waiters.fetch_add(1, Ordering::SeqCst);
        Arc::new(Counted {
            inner: SystemClock.waiter(),
            waits: Arc::clone(&self.waits),
        })
    }
}

/// A waiter of `CountingClock`.
struct Counted {
    inner: Arc<dyn Waiter>,
    waits: Arc<AtomicUsize>,
}

impl Waiter for Counted {
    fn wait(&self, timeout: Duration) {
        self.waits.fetch_add(1, Ordering::SeqCst);
        self.inner.wait(timeout);
    }

    fn wake(&self) {
        self.inner.wake();
    }
}

/// A seeded xorshift generator, so that a failure repeats.
pub struct Rng(u64);

impl Rng {
    /// Return a generator that starts at `seed`. A xorshift generator stays
    /// at 0 forever, so the seed 0 starts at 1.
    pub fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }

    /// Return the next number.
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// Return a number below `n`.
    pub fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// A record that zstd shrinks, so that a store with compression keeps it
/// with codec 1.
pub fn compressible() -> Vec<u8> {
    let mut data = b"\x80\x03capp.content\nDocument\nq\x00.\x80\x03}q\x01(".to_vec();
    for i in 0..500u32 {
        data.extend_from_slice(b"X\x0b\x00\x00\x00descriptionq\x02X\x05\x00\x00\x00title");
        data.extend_from_slice(&i.to_le_bytes());
    }
    data.extend_from_slice(b"u.");
    data
}
