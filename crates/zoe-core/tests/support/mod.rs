//! The helpers that the integration tests of zoe-core share. Each test
//! binary uses some of them. `zoe_core::testing` holds the helpers that
//! the unit tests use too.

#![allow(dead_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use zoe_core::testing::TempDir;
use zoe_core::{Clock, SystemClock, Waiter};

/// Return a new empty directory for the test `name` of this test binary.
/// The drop of the directory removes it.
pub fn fresh_dir(name: &str) -> TempDir {
    zoe_core::testing::fresh_dir(&format!("{}-{name}", env!("CARGO_CRATE_NAME")))
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

/// A `MemVfs` that counts the bytes and the syncs of the store files. It
/// can hold the syncs of the database file or of the log at a gate, fail
/// each write of the database file, and fail each read of the log. The
/// clones share the files, the counts and the switches.
///
/// A file is the database file (`zoe.redb`), a file of the write-ahead log
/// (a name under `wal/`), or another file. Only the first two count.
#[derive(Clone, Default)]
pub struct MeteredVfs {
    pub vfs: zoe_core::vfs::MemVfs,
    pub meter: Arc<Meter>,
}

/// A gate that holds the syncs of one kind of file.
#[derive(Default)]
pub struct Gate {
    held: std::sync::Mutex<bool>,
    released: std::sync::Condvar,
    /// The syncs that wait at the gate now.
    waiting: AtomicUsize,
}

impl Gate {
    fn set(&self, held: bool) {
        *self.held.lock().unwrap() = held;
        self.released.notify_all();
    }

    fn pass(&self) {
        let mut held = self.held.lock().unwrap();
        if *held {
            self.waiting.fetch_add(1, Ordering::SeqCst);
            while *held {
                held = self.released.wait(held).unwrap();
            }
            self.waiting.fetch_sub(1, Ordering::SeqCst);
        }
    }

    /// Return the syncs that wait at the gate now.
    pub fn waiting(&self) -> usize {
        self.waiting.load(Ordering::SeqCst)
    }
}

/// The counts of a `MeteredVfs`, its gates and its switches.
#[derive(Default)]
pub struct Meter {
    pub database_bytes: std::sync::atomic::AtomicU64,
    pub database_syncs: std::sync::atomic::AtomicU64,
    pub log_bytes: std::sync::atomic::AtomicU64,
    pub log_syncs: std::sync::atomic::AtomicU64,
    /// The gate of the syncs of the database file.
    pub database_gate: Gate,
    /// The gate of the syncs of the files of the log.
    pub log_gate: Gate,
    /// True while each write of the database file fails.
    pub fail_database_writes: std::sync::atomic::AtomicBool,
    /// True while each read of a file of the log fails.
    pub fail_log_reads: std::sync::atomic::AtomicBool,
}

impl std::fmt::Debug for Meter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Meter")
    }
}

impl Meter {
    /// Set every count to 0.
    pub fn reset(&self) {
        for count in [
            &self.database_bytes,
            &self.database_syncs,
            &self.log_bytes,
            &self.log_syncs,
        ] {
            count.store(0, Ordering::SeqCst);
        }
    }

    pub fn database_bytes(&self) -> u64 {
        self.database_bytes.load(Ordering::SeqCst)
    }

    pub fn database_syncs(&self) -> u64 {
        self.database_syncs.load(Ordering::SeqCst)
    }

    pub fn log_bytes(&self) -> u64 {
        self.log_bytes.load(Ordering::SeqCst)
    }

    pub fn log_syncs(&self) -> u64 {
        self.log_syncs.load(Ordering::SeqCst)
    }

    /// Hold the syncs of the database file until the returned guard drops.
    /// A test that fails while a thread waits at the gate then still ends.
    pub fn hold(&self) -> Held<'_> {
        self.database_gate.set(true);
        Held(&self.database_gate)
    }

    /// Hold the syncs of the files of the log until the returned guard
    /// drops.
    pub fn hold_log(&self) -> Held<'_> {
        self.log_gate.set(true);
        Held(&self.log_gate)
    }
}

/// A held gate. Its drop lets the syncs go on.
pub struct Held<'m>(&'m Gate);

impl Drop for Held<'_> {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

/// The kind of a file of a `MeteredVfs`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FileKind {
    Database,
    Log,
    Other,
}

fn kind_of(name: &str) -> FileKind {
    if name == "zoe.redb" {
        FileKind::Database
    } else if name.starts_with("wal/") {
        FileKind::Log
    } else {
        FileKind::Other
    }
}

/// A file of a `MeteredVfs`.
#[derive(Debug)]
pub struct MeteredFile {
    file: zoe_core::vfs::MemFile,
    kind: FileKind,
    meter: Arc<Meter>,
}

impl redb::StorageBackend for MeteredFile {
    fn len(&self) -> std::io::Result<u64> {
        self.file.len()
    }

    fn read(&self, offset: u64, out: &mut [u8]) -> std::io::Result<()> {
        if self.kind == FileKind::Log && self.meter.fail_log_reads.load(Ordering::SeqCst) {
            return Err(std::io::Error::other("an injected read failure"));
        }
        self.file.read(offset, out)
    }

    fn set_len(&self, len: u64) -> std::io::Result<()> {
        self.file.set_len(len)
    }

    fn sync_data(&self) -> std::io::Result<()> {
        match self.kind {
            FileKind::Database => {
                self.meter.database_gate.pass();
                self.meter.database_syncs.fetch_add(1, Ordering::SeqCst);
            }
            FileKind::Log => {
                self.meter.log_gate.pass();
                self.meter.log_syncs.fetch_add(1, Ordering::SeqCst);
            }
            FileKind::Other => {}
        }
        self.file.sync_data()
    }

    fn write(&self, offset: u64, data: &[u8]) -> std::io::Result<()> {
        match self.kind {
            FileKind::Database => {
                if self.meter.fail_database_writes.load(Ordering::SeqCst) {
                    return Err(std::io::Error::other("an injected write failure"));
                }
                let bytes = data.len() as u64;
                self.meter.database_bytes.fetch_add(bytes, Ordering::SeqCst);
            }
            FileKind::Log => {
                let bytes = data.len() as u64;
                self.meter.log_bytes.fetch_add(bytes, Ordering::SeqCst);
            }
            FileKind::Other => {}
        }
        self.file.write(offset, data)
    }

    fn close(&self) -> std::io::Result<()> {
        self.file.close()
    }
}

/// A sequential writer of a `MeteredVfs` that counts the bytes and the
/// syncs of a file of the log.
struct MeteredWriter {
    inner: Box<dyn zoe_core::vfs::WriteFile>,
    kind: FileKind,
    meter: Arc<Meter>,
}

impl std::io::Write for MeteredWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        if self.kind == FileKind::Log {
            self.meter.log_bytes.fetch_add(n as u64, Ordering::SeqCst);
        }
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

impl zoe_core::vfs::WriteFile for MeteredWriter {
    fn sync(&mut self) -> std::io::Result<()> {
        if self.kind == FileKind::Log {
            self.meter.log_gate.pass();
            self.meter.log_syncs.fetch_add(1, Ordering::SeqCst);
        }
        self.inner.sync()
    }
}

/// A sequential reader of a `MeteredVfs` that fails while the reads of the
/// log fail.
struct MeteredReader {
    inner: Box<dyn std::io::Read + Send>,
    kind: FileKind,
    meter: Arc<Meter>,
}

impl std::io::Read for MeteredReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.kind == FileKind::Log && self.meter.fail_log_reads.load(Ordering::SeqCst) {
            return Err(std::io::Error::other("an injected read failure"));
        }
        self.inner.read(buf)
    }
}

impl MeteredVfs {
    /// Return a metered view of `vfs`, with a meter of its own.
    pub fn over(vfs: zoe_core::vfs::MemVfs) -> Self {
        Self {
            vfs,
            meter: Arc::default(),
        }
    }

    fn metered(&self, name: &str, file: zoe_core::vfs::MemFile) -> MeteredFile {
        MeteredFile {
            file,
            kind: kind_of(name),
            meter: Arc::clone(&self.meter),
        }
    }
}

impl zoe_core::vfs::Vfs for MeteredVfs {
    type File = MeteredFile;

    fn exists(&self, name: &str) -> std::io::Result<bool> {
        self.vfs.exists(name)
    }

    fn open(&self, name: &str) -> std::io::Result<MeteredFile> {
        Ok(self.metered(name, self.vfs.open(name)?))
    }

    fn create_new(&self, name: &str) -> std::io::Result<MeteredFile> {
        Ok(self.metered(name, self.vfs.create_new(name)?))
    }

    fn rename(&self, from: &str, to: &str) -> std::io::Result<()> {
        self.vfs.rename(from, to)
    }

    fn hard_link(&self, from: &str, to: &str) -> std::io::Result<()> {
        self.vfs.hard_link(from, to)
    }

    fn remove(&self, name: &str) -> std::io::Result<()> {
        self.vfs.remove(name)
    }

    fn sync_dir(&self) -> std::io::Result<()> {
        self.vfs.sync_dir()
    }

    fn sync_dir_at(&self, dir: &str) -> std::io::Result<()> {
        self.vfs.sync_dir_at(dir)
    }

    fn create_dir(&self, name: &str) -> std::io::Result<()> {
        self.vfs.create_dir(name)
    }

    fn list_files(&self, dir: &str) -> std::io::Result<Vec<String>> {
        self.vfs.list_files(dir)
    }

    fn list_entries(&self, dir: &str) -> std::io::Result<Vec<String>> {
        self.vfs.list_entries(dir)
    }

    fn create_file(&self, name: &str) -> std::io::Result<Box<dyn zoe_core::vfs::WriteFile>> {
        Ok(Box::new(MeteredWriter {
            inner: self.vfs.create_file(name)?,
            kind: kind_of(name),
            meter: Arc::clone(&self.meter),
        }))
    }

    fn read_file(&self, name: &str) -> std::io::Result<Box<dyn std::io::Read + Send>> {
        Ok(Box::new(MeteredReader {
            inner: self.vfs.read_file(name)?,
            kind: kind_of(name),
            meter: Arc::clone(&self.meter),
        }))
    }

    fn sync_file(&self, name: &str) -> std::io::Result<()> {
        self.vfs.sync_file(name)
    }

    fn set_read_only(&self, name: &str) -> std::io::Result<()> {
        self.vfs.set_read_only(name)
    }

    fn lock_dir(&self, name: &str) -> std::io::Result<zoe_core::vfs::DirLock> {
        self.vfs.lock_dir(name)
    }

    fn path(&self, name: &str) -> std::path::PathBuf {
        self.vfs.path(name)
    }
}

/// A reader of the files of the write-ahead log, as ADR 0056 lays them
/// out. The tests read and damage the log with it.
pub mod wal {
    use redb::StorageBackend;
    use xxhash_rust::xxh3::xxh3_64;
    use zoe_core::Tid;
    use zoe_core::vfs::{MemVfs, Vfs};

    /// The length of the header of a segment, and the offset of the first
    /// entry of a use.
    pub const SEGMENT_HEADER: u64 = 32;
    /// The length of the header of an entry.
    pub const ENTRY_HEADER: u64 = 56;
    /// The magic number of a segment.
    pub const SEGMENT_MAGIC: &[u8; 8] = b"ZOE-WAL\0";
    /// The magic number of an entry.
    pub const ENTRY_MAGIC: &[u8; 4] = b"ZWAL";
    /// The state of a segment that holds the entries of its use.
    pub const IN_USE: u32 = 1;
    /// The state of a segment that holds no entry that counts.
    pub const SPARE: u32 = 2;

    /// The header of a segment.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Segment {
        pub version: u32,
        pub state: u32,
        pub epoch: u64,
    }

    /// The place and the header of one entry.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Entry {
        /// The offset of the entry in its segment.
        pub offset: u64,
        pub ops: u32,
        pub body_len: u64,
        pub epoch: u64,
        pub previous: Tid,
        pub last: Tid,
    }

    impl Entry {
        /// The offset of the first byte of the body.
        pub fn body(&self) -> u64 {
            self.offset + ENTRY_HEADER
        }

        /// The offset of the next entry: after the body and the zeros that
        /// align it to 8 bytes.
        pub fn end(&self) -> u64 {
            (self.offset + ENTRY_HEADER + self.body_len).next_multiple_of(8)
        }
    }

    /// Return the names of the segment files of `wal/` (`.wal`), with the
    /// directory part, in order.
    pub fn files(vfs: &MemVfs) -> Vec<String> {
        match vfs.list_files("wal") {
            Ok(names) => names
                .into_iter()
                .filter(|name| name.ends_with(".wal"))
                .map(|name| format!("wal/{name}"))
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    fn read(vfs: &MemVfs, name: &str, offset: u64, len: u64) -> Option<Vec<u8>> {
        let file = vfs.file(name);
        if file.len().ok()? < offset + len {
            return None;
        }
        let mut bytes = vec![0; len as usize];
        file.read(offset, &mut bytes).ok()?;
        Some(bytes)
    }

    fn u32_at(bytes: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
    }

    fn u64_at(bytes: &[u8], at: usize) -> u64 {
        u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
    }

    /// Return the header of the segment `name`, or `None` when it does
    /// not pass its checksum.
    pub fn segment(vfs: &MemVfs, name: &str) -> Option<Segment> {
        let header = read(vfs, name, 0, SEGMENT_HEADER)?;
        if &header[..8] != SEGMENT_MAGIC || xxh3_64(&header[..24]) != u64_at(&header, 24) {
            return None;
        }
        Some(Segment {
            version: u32_at(&header, 8),
            state: u32_at(&header, 12),
            epoch: u64_at(&header, 16),
        })
    }

    /// Return the entry at `offset` of the segment `name` when its header
    /// and its body pass their checksums.
    pub fn entry_at(vfs: &MemVfs, name: &str, offset: u64) -> Option<Entry> {
        let header = read(vfs, name, offset, ENTRY_HEADER)?;
        if &header[..4] != ENTRY_MAGIC || xxh3_64(&header[..48]) != u64_at(&header, 48) {
            return None;
        }
        let entry = Entry {
            offset,
            ops: u32_at(&header, 4),
            body_len: u64_at(&header, 8),
            epoch: u64_at(&header, 16),
            previous: Tid(u64_at(&header, 24)),
            last: Tid(u64_at(&header, 32)),
        };
        let body = read(vfs, name, entry.body(), entry.body_len)?;
        let checked = [&header[..40], &body[..]].concat();
        (xxh3_64(&checked) == u64_at(&header, 40)).then_some(entry)
    }

    /// Return the entries of the use of the segment `name`, from the first
    /// one to the first that does not pass. The epoch of the use comes from
    /// the header, or from the first entry when the header does not pass.
    pub fn entries(vfs: &MemVfs, name: &str) -> Vec<Entry> {
        let epoch = match segment(vfs, name) {
            Some(segment) if segment.state == IN_USE => segment.epoch,
            Some(_) => return Vec::new(),
            None => match entry_at(vfs, name, SEGMENT_HEADER) {
                Some(first) => first.epoch,
                None => return Vec::new(),
            },
        };
        let mut entries = Vec::new();
        let mut offset = SEGMENT_HEADER;
        while let Some(entry) = entry_at(vfs, name, offset) {
            if entry.epoch != epoch {
                break;
            }
            entries.push(entry);
            offset = entry.end();
        }
        entries
    }

    /// Return the segment that holds the entry whose last tid is `tid`, and
    /// that entry.
    pub fn find(vfs: &MemVfs, tid: Tid) -> Option<(String, Entry)> {
        files(vfs).into_iter().find_map(|name| {
            let entry = entries(vfs, &name).into_iter().find(|e| e.last == tid)?;
            Some((name, entry))
        })
    }

    /// Return the body of `entry` in the segment `name`.
    pub fn body(vfs: &MemVfs, name: &str, entry: &Entry) -> Vec<u8> {
        read(vfs, name, entry.body(), entry.body_len).expect("the body of the entry")
    }

    /// Flip the bits of one byte of the file `name` at `offset`.
    pub fn flip(vfs: &MemVfs, name: &str, offset: u64) {
        let file = vfs.file(name);
        let mut byte = [0];
        file.read(offset, &mut byte).unwrap();
        file.write(offset, &[!byte[0]]).unwrap();
    }
}
