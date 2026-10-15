//! A redb storage backend that counts the bytes it writes and the time of
//! its syncs.
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use redb::StorageBackend;

#[derive(Default, Debug)]
pub struct Io {
    pub sync_nanos: AtomicU64,
    pub syncs: AtomicU64,
    pub write_nanos: AtomicU64,
    pub writes: AtomicU64,
    pub written: AtomicU64,
}

pub struct Taken {
    pub sync: Duration,
    pub syncs: u64,
    pub write: Duration,
    pub writes: u64,
    pub written: u64,
}

impl Io {
    pub fn take(&self) -> Taken {
        let nanos = |c: &AtomicU64| Duration::from_nanos(c.swap(0, Ordering::SeqCst));
        Taken {
            sync: nanos(&self.sync_nanos),
            syncs: self.syncs.swap(0, Ordering::SeqCst),
            write: nanos(&self.write_nanos),
            writes: self.writes.swap(0, Ordering::SeqCst),
            written: self.written.swap(0, Ordering::SeqCst),
        }
    }
}

fn count(nanos: &AtomicU64, counter: &AtomicU64, start: Instant) {
    nanos.fetch_add(start.elapsed().as_nanos() as u64, Ordering::SeqCst);
    counter.fetch_add(1, Ordering::SeqCst);
}

#[derive(Debug)]
pub struct CountedFile {
    pub file: redb::backends::FileBackend,
    pub io: Option<Arc<Io>>,
}

impl StorageBackend for CountedFile {
    fn len(&self) -> io::Result<u64> {
        self.file.len()
    }
    fn read(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
        self.file.read(offset, out)
    }
    fn set_len(&self, len: u64) -> io::Result<()> {
        self.file.set_len(len)
    }
    fn sync_data(&self) -> io::Result<()> {
        let start = Instant::now();
        let result = self.file.sync_data();
        if let Some(io) = &self.io {
            count(&io.sync_nanos, &io.syncs, start);
        }
        result
    }
    fn write(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        let start = Instant::now();
        let result = self.file.write(offset, data);
        if let Some(io) = &self.io {
            count(&io.write_nanos, &io.writes, start);
            io.written.fetch_add(data.len() as u64, Ordering::SeqCst);
        }
        result
    }
    fn close(&self) -> io::Result<()> {
        self.file.close()
    }
}
