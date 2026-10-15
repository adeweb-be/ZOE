//! A measurement of `finish` at concurrency 1, as zodbshootout commits:
//! 100 records of 1 KB in each transaction, in a store on the disk.
//!
//! The test is ignored, because its numbers mean something only in a
//! release build on a real disk. Run it with:
//!
//! ```text
//! cargo test -p zoe-core --release --test finish_time -- --ignored --nocapture
//! ```
//!
//! It runs four scenarios: `add` writes 100 new objects in each
//! transaction, and `update` writes 100 existing objects again, each with
//! compression off and with zstd, the default. For each scenario it prints
//! the time of `finish`, the time of the span `fsync` (N17: the write and
//! the sync of the commit batch), the time of the syncs and of the writes
//! of the database file, and `finish` less its syncs. Each number is the
//! median, the 90th percentile and the mean of the transactions, in µs.
//! It also prints the syncs, the writes and the bytes written of each
//! finish.
//!
//! `ZOE_FINISH_TXNS` sets the number of transactions of each scenario,
//! 500 by default. `ZOE_FINISH_DIR` sets the directory of the store, the
//! temporary directory of cargo by default.

use std::fmt;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use redb::StorageBackend;
use zoe_core::vfs::{DirLock, OsVfs, Vfs, WriteFile};
use zoe_core::{Compression, Oid, Options, Store, SystemClock, Tid};

/// The records of a transaction.
const RECORDS: usize = 100;

/// The size of a record.
const SIZE: usize = 1024;

/// The transactions that run before each scenario and are not measured.
const WARMUP: usize = 20;

/// The name of the database file in the store directory.
const DATABASE: &str = "zoe.redb";

/// The syncs and the writes of the database file: their time, their
/// count and the bytes written.
#[derive(Default)]
struct Io {
    sync_nanos: AtomicU64,
    syncs: AtomicU64,
    write_nanos: AtomicU64,
    writes: AtomicU64,
    written: AtomicU64,
}

/// What `Io::take` gives: the numbers since the last call.
struct Taken {
    sync: Duration,
    syncs: u64,
    write: Duration,
    writes: u64,
    written: u64,
}

impl Io {
    /// Return the numbers since the last call, and start from zero.
    fn take(&self) -> Taken {
        let nanos = |counter: &AtomicU64| Duration::from_nanos(counter.swap(0, Ordering::SeqCst));
        Taken {
            sync: nanos(&self.sync_nanos),
            syncs: self.syncs.swap(0, Ordering::SeqCst),
            write: nanos(&self.write_nanos),
            writes: self.writes.swap(0, Ordering::SeqCst),
            written: self.written.swap(0, Ordering::SeqCst),
        }
    }
}

impl fmt::Debug for Io {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Io")
    }
}

/// A file of the store. The database file counts its syncs and writes in
/// `io`. Another file has no `io`.
#[derive(Debug)]
struct TimedFile {
    file: redb::backends::FileBackend,
    io: Option<Arc<Io>>,
}

/// Add the time since `start` to `nanos`, and 1 to `count`.
fn count(nanos: &AtomicU64, count: &AtomicU64, start: Instant) {
    nanos.fetch_add(start.elapsed().as_nanos() as u64, Ordering::SeqCst);
    count.fetch_add(1, Ordering::SeqCst);
}

impl StorageBackend for TimedFile {
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

/// A directory of the operating system whose database file counts its
/// syncs and writes.
struct TimedVfs {
    vfs: OsVfs,
    io: Arc<Io>,
}

impl TimedVfs {
    fn timed(&self, name: &str, file: redb::backends::FileBackend) -> TimedFile {
        let io = (name == DATABASE).then(|| self.io.clone());
        TimedFile { file, io }
    }
}

impl Vfs for TimedVfs {
    type File = TimedFile;

    fn exists(&self, name: &str) -> io::Result<bool> {
        self.vfs.exists(name)
    }

    fn open(&self, name: &str) -> io::Result<TimedFile> {
        Ok(self.timed(name, self.vfs.open(name)?))
    }

    fn create_new(&self, name: &str) -> io::Result<TimedFile> {
        Ok(self.timed(name, self.vfs.create_new(name)?))
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        self.vfs.rename(from, to)
    }

    fn remove(&self, name: &str) -> io::Result<()> {
        self.vfs.remove(name)
    }

    fn sync_dir(&self) -> io::Result<()> {
        self.vfs.sync_dir()
    }

    fn sync_dir_at(&self, dir: &str) -> io::Result<()> {
        self.vfs.sync_dir_at(dir)
    }

    fn create_dir(&self, name: &str) -> io::Result<()> {
        self.vfs.create_dir(name)
    }

    fn list_files(&self, dir: &str) -> io::Result<Vec<String>> {
        self.vfs.list_files(dir)
    }

    fn list_entries(&self, dir: &str) -> io::Result<Vec<String>> {
        self.vfs.list_entries(dir)
    }

    fn create_file(&self, name: &str) -> io::Result<Box<dyn WriteFile>> {
        self.vfs.create_file(name)
    }

    fn read_file(&self, name: &str) -> io::Result<Box<dyn Read + Send>> {
        self.vfs.read_file(name)
    }

    fn sync_file(&self, name: &str) -> io::Result<()> {
        self.vfs.sync_file(name)
    }

    fn set_read_only(&self, name: &str) -> io::Result<()> {
        self.vfs.set_read_only(name)
    }

    fn lock_dir(&self, name: &str) -> io::Result<DirLock> {
        self.vfs.lock_dir(name)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.vfs.path(name)
    }
}

/// A subscriber of `tracing` that adds up the time of each span `fsync`.
struct FsyncSpans {
    next: AtomicU64,
    /// The id of each open `fsync` span and the time it was entered.
    open: Mutex<Vec<(u64, Option<Instant>)>>,
    nanos: AtomicU64,
}

impl FsyncSpans {
    fn new() -> Self {
        Self {
            next: AtomicU64::new(1),
            open: Mutex::new(Vec::new()),
            nanos: AtomicU64::new(0),
        }
    }

    fn take(&self) -> Duration {
        Duration::from_nanos(self.nanos.swap(0, Ordering::SeqCst))
    }
}

impl tracing::Subscriber for FsyncSpans {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        metadata.is_span() && metadata.name() == "fsync"
    }

    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        self.open.lock().unwrap().push((id, None));
        tracing::span::Id::from_u64(id)
    }

    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

    fn event(&self, _: &tracing::Event<'_>) {}

    fn enter(&self, span: &tracing::span::Id) {
        let mut open = self.open.lock().unwrap();
        if let Some(entry) = open.iter_mut().find(|(id, _)| *id == span.into_u64()) {
            entry.1 = Some(Instant::now());
        }
    }

    fn exit(&self, span: &tracing::span::Id) {
        let mut open = self.open.lock().unwrap();
        if let Some(at) = open.iter().position(|(id, _)| *id == span.into_u64())
            && let (_, Some(entered)) = open.swap_remove(at)
        {
            let nanos = entered.elapsed().as_nanos() as u64;
            self.nanos.fetch_add(nanos, Ordering::SeqCst);
        }
    }
}

/// The record of `oid` in its `version`: 1 KB, the size of a record of
/// zodbshootout. The first half is text that zstd compresses well, the
/// second half is pseudo-random bytes that it does not.
fn record(oid: Oid, version: u64) -> Vec<u8> {
    let mut data = Vec::with_capacity(SIZE);
    data.extend_from_slice(b"\x80\x03czodbshootout.speedtest\nPObject\nq\x00.\x80\x03}q\x01(");
    while data.len() < SIZE / 2 {
        data.extend_from_slice(b"X\x04\x00\x00\x00attrq\x02K\x01X\x04\x00\x00\x00dataq\x03");
    }
    let mut state = oid.0 ^ (version << 32) ^ 0x9e37_79b9_7f4a_7c15;
    while data.len() < SIZE {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        data.extend_from_slice(&state.to_le_bytes());
    }
    data.truncate(SIZE);
    data
}

/// The numbers of one scenario, one entry for each transaction.
#[derive(Default)]
struct Samples {
    finish: Vec<Duration>,
    fsync: Vec<Duration>,
    sync: Vec<Duration>,
    write: Vec<Duration>,
    syncs: u64,
    writes: u64,
    written: u64,
}

/// Return the median, the 90th percentile and the mean of `samples`, in
/// µs.
fn summary(samples: &[Duration]) -> String {
    let mut sorted = samples.to_vec();
    sorted.sort();
    let micros = |d: Duration| d.as_secs_f64() * 1e6;
    let median = micros(sorted[sorted.len() / 2]);
    let p90 = micros(sorted[sorted.len() * 9 / 10]);
    let mean = micros(sorted.iter().sum::<Duration>()) / sorted.len() as f64;
    format!("median {median:6.0}  p90 {p90:6.0}  mean {mean:6.0}")
}

impl Samples {
    fn print(&self, name: &str) {
        let txns = self.finish.len() as u64;
        println!(
            "{name}: {txns} transactions; per finish {:.2} syncs, {} writes, {} bytes written",
            self.syncs as f64 / txns as f64,
            self.writes / txns,
            self.written / txns
        );
        println!("  finish            {}", summary(&self.finish));
        println!("  span fsync        {}", summary(&self.fsync));
        println!("  sync of the file  {}", summary(&self.sync));
        println!("  writes            {}", summary(&self.write));
        let rest: Vec<Duration> = (self.finish.iter().zip(&self.sync))
            .map(|(finish, sync)| finish.saturating_sub(*sync))
            .collect();
        println!("  finish less sync  {}", summary(&rest));
    }
}

/// Commit one transaction that writes `oids` in `version`. Return the
/// time of `finish` and the oids with their new serial. `io` counts only
/// the syncs and the writes of `finish`.
fn commit(
    store: &Store,
    io: &Io,
    oids: &[(Oid, Tid)],
    version: u64,
) -> (Duration, Vec<(Oid, Tid)>) {
    let mut txn = store.tpc_begin(b"user", b"finish time", b"").unwrap();
    for &(oid, serial) in oids {
        txn.store(oid, serial, &record(oid, version)).unwrap();
    }
    txn.vote().unwrap();
    io.take();
    let start = Instant::now();
    let tid = txn.finish().unwrap();
    let elapsed = start.elapsed();
    (elapsed, oids.iter().map(|&(oid, _)| (oid, tid)).collect())
}

/// Run `txns` transactions of `scenario` in a new store in `dir` with
/// `compression`, and return their numbers. `add` writes new oids, and
/// `update` writes again the oids of the warmup, in turn.
fn run(dir: &Path, compression: Compression, scenario: &str, txns: usize) -> Samples {
    let _ = std::fs::remove_dir_all(dir);
    let io = Arc::new(Io::default());
    let vfs = TimedVfs {
        vfs: OsVfs::new(dir).unwrap(),
        io: io.clone(),
    };
    let options = Options {
        compression,
        ..Options::default()
    };
    let store = Store::open_full(vfs, SystemClock, options).unwrap();
    let spans = Arc::new(FsyncSpans::new());
    let dispatch = tracing::Dispatch::from(spans.clone());
    let new_oids = |store: &Store| -> Vec<(Oid, Tid)> {
        (0..RECORDS)
            .map(|_| (store.new_oid().unwrap(), Tid::ZERO))
            .collect()
    };

    let mut samples = Samples::default();
    // The subscriber is the default from the first span on, so the span
    // `fsync` is never cached as a span that no subscriber wants.
    tracing::dispatcher::with_default(&dispatch, || {
        let mut written = Vec::new();
        for _ in 0..WARMUP {
            let (_, oids) = commit(&store, &io, &new_oids(&store), 0);
            written.push(oids);
        }
        spans.take();
        for i in 0..txns {
            let finish = if scenario == "add" {
                commit(&store, &io, &new_oids(&store), 0).0
            } else {
                let slot = i % written.len();
                let (finish, oids) = commit(&store, &io, &written[slot], i as u64 + 1);
                written[slot] = oids;
                finish
            };
            let taken = io.take();
            assert!(taken.syncs >= 1, "the finish syncs the database file");
            samples.finish.push(finish);
            samples.fsync.push(spans.take());
            samples.sync.push(taken.sync);
            samples.write.push(taken.write);
            samples.syncs += taken.syncs;
            samples.writes += taken.writes;
            samples.written += taken.written;
        }
    });
    drop(store);
    let _ = std::fs::remove_dir_all(dir);
    samples
}

#[test]
#[ignore = "a measurement: run it in release on a real disk"]
fn finish_time_at_concurrency_1_with_1_kb_records() {
    let txns = std::env::var("ZOE_FINISH_TXNS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(500);
    // The temporary directory of cargo is in the target directory, on
    // the disk. `/tmp` can be a memory filesystem, whose sync costs
    // nothing.
    let dir = std::env::var_os("ZOE_FINISH_DIR")
        .map_or_else(|| PathBuf::from(env!("CARGO_TARGET_TMPDIR")), PathBuf::from)
        .join("finish-time");
    for (name, compression) in [
        ("none", Compression::None),
        ("zstd", Compression::default()),
    ] {
        for scenario in ["add", "update"] {
            let samples = run(&dir, compression, scenario, txns);
            samples.print(&format!("{scenario}, compression {name}"));
        }
    }
}
