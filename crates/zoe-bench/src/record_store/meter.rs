//! A `Vfs` wrapper that counts what the store writes and emulates a slow
//! fsync.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use zoe_core::vfs::Vfs;

/// The counters of a `MeteredVfs`, and the cost of its fsync.
///
/// The meter counts and sleeps only while it is armed. So the creation
/// of a store and the setup of a workload cost nothing.
#[derive(Debug, Default)]
pub struct Meter {
    /// The time that each sync of a file or of the directory sleeps.
    delay: Duration,
    armed: AtomicBool,
    bytes: AtomicU64,
    writes: AtomicU64,
    syncs: AtomicU64,
    set_lens: AtomicU64,
}

/// What a meter counted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// The bytes passed to `write`.
    pub bytes: u64,
    /// The calls to `write`.
    pub writes: u64,
    /// The syncs of a file or of the directory.
    pub syncs: u64,
    /// The calls to `set_len`.
    pub set_lens: u64,
}

impl Meter {
    /// Return a disarmed meter whose syncs sleep `delay` when armed.
    pub fn new(delay: Duration) -> Self {
        Self {
            delay,
            ..Self::default()
        }
    }

    /// Clear the counters and start to count and to sleep.
    pub fn arm(&self) {
        for counter in [&self.bytes, &self.writes, &self.syncs, &self.set_lens] {
            counter.store(0, Ordering::SeqCst);
        }
        self.armed.store(true, Ordering::SeqCst);
    }

    /// Stop to count and to sleep, and return the counters.
    pub fn disarm(&self) -> Counts {
        self.armed.store(false, Ordering::SeqCst);
        self.counts()
    }

    /// Return the counters.
    pub fn counts(&self) -> Counts {
        Counts {
            bytes: self.bytes.load(Ordering::SeqCst),
            writes: self.writes.load(Ordering::SeqCst),
            syncs: self.syncs.load(Ordering::SeqCst),
            set_lens: self.set_lens.load(Ordering::SeqCst),
        }
    }

    fn armed(&self) -> bool {
        self.armed.load(Ordering::SeqCst)
    }

    fn sync(&self) {
        if self.armed() {
            self.syncs.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(self.delay);
        }
    }
}

/// A file whose writes and syncs a meter counts.
#[derive(Debug)]
pub struct MeteredFile<F> {
    inner: F,
    meter: Arc<Meter>,
}

impl<F: redb::StorageBackend> redb::StorageBackend for MeteredFile<F> {
    fn len(&self) -> io::Result<u64> {
        self.inner.len()
    }

    fn read(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
        self.inner.read(offset, out)
    }

    fn set_len(&self, len: u64) -> io::Result<()> {
        if self.meter.armed() {
            self.meter.set_lens.fetch_add(1, Ordering::SeqCst);
        }
        self.inner.set_len(len)
    }

    fn sync_data(&self) -> io::Result<()> {
        self.inner.sync_data()?;
        self.meter.sync();
        Ok(())
    }

    fn write(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        if self.meter.armed() {
            self.meter
                .bytes
                .fetch_add(data.len() as u64, Ordering::SeqCst);
            self.meter.writes.fetch_add(1, Ordering::SeqCst);
        }
        self.inner.write(offset, data)
    }

    fn close(&self) -> io::Result<()> {
        self.inner.close()
    }
}

/// A directory whose files a meter counts.
#[derive(Debug)]
pub struct MeteredVfs<V> {
    inner: V,
    meter: Arc<Meter>,
}

impl<V: Vfs> MeteredVfs<V> {
    /// Wrap `inner`, and count with `meter`.
    pub fn new(inner: V, meter: Arc<Meter>) -> Self {
        Self { inner, meter }
    }
}

impl<V: Vfs> Vfs for MeteredVfs<V> {
    type File = MeteredFile<V::File>;

    fn exists(&self, name: &str) -> io::Result<bool> {
        self.inner.exists(name)
    }

    fn open(&self, name: &str) -> io::Result<Self::File> {
        Ok(MeteredFile {
            inner: self.inner.open(name)?,
            meter: Arc::clone(&self.meter),
        })
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        self.inner.rename(from, to)
    }

    fn remove(&self, name: &str) -> io::Result<()> {
        self.inner.remove(name)
    }

    fn sync_dir(&self) -> io::Result<()> {
        self.inner.sync_dir()?;
        self.meter.sync();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use redb::StorageBackend;

    use super::*;
    use crate::record_store::RamVfs;

    #[test]
    fn only_an_armed_meter_counts() {
        let meter = Arc::new(Meter::new(Duration::ZERO));
        let vfs = MeteredVfs::new(RamVfs::new(), Arc::clone(&meter));
        let file = vfs.open("f").unwrap();
        file.write(0, b"not counted").unwrap();
        file.sync_data().unwrap();

        meter.arm();
        file.write(0, b"four").unwrap();
        file.write(4, b"more").unwrap();
        file.set_len(100).unwrap();
        file.sync_data().unwrap();
        vfs.sync_dir().unwrap();
        let counts = meter.disarm();
        file.write(0, b"not counted").unwrap();

        let expected = Counts {
            bytes: 8,
            writes: 2,
            syncs: 2,
            set_lens: 1,
        };
        assert_eq!(counts, expected);
        assert_eq!(meter.counts(), expected);
    }

    #[test]
    fn an_armed_sync_sleeps() {
        let meter = Arc::new(Meter::new(Duration::from_millis(20)));
        let vfs = MeteredVfs::new(RamVfs::new(), Arc::clone(&meter));
        let file = vfs.open("f").unwrap();
        meter.arm();
        let start = std::time::Instant::now();
        file.sync_data().unwrap();
        assert!(start.elapsed() >= Duration::from_millis(20));
    }
}
