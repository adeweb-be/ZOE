//! The record-store spike: write amplification and fsync cost of redb and
//! of an append-only log, on the commit path of the walking skeleton.
//!
//! Every backend kind runs through the same `Store` code: `tpc_begin`,
//! `store`, `vote`, `finish`. A cell runs `writers` threads. Each thread
//! commits transactions of `objects_per_txn` objects of `object_size`
//! bytes, on oids that only it writes. The setup commits each object
//! once. The measured commits then update them, so each commit writes a
//! new version of every object of its writer.
//!
//! The store lives in memory (`RamVfs`). A `MeteredVfs` counts the bytes
//! that the backend writes and its syncs. Each sync sleeps for the fsync
//! time, which emulates a device with that fsync latency. Writes cost
//! only a memory copy.
//!
//! `measure_batched` estimates a group commit. It skips the `Store` and
//! writes the transactions of all the writers in one backend write
//! transaction, with one commit. That is the best case of a group commit:
//! every batch is full, and nothing waits for a lock.
//!
//! `measure_scattered` fills a store with background records first, then
//! updates objects spread over it, so that a B-tree commit touches one
//! path per object.

pub mod crash;
mod meter;
mod ram;
pub mod summary;

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;
use zoe_core::log_backend::LogBackend;
use zoe_core::vfs::{OsVfs, Vfs};
use zoe_core::{Backend, Oid, Result, Store, SystemClock, Tid, TxnMeta, TxnStatus};

pub use meter::{Counts, Meter, MeteredFile, MeteredVfs};
pub use ram::{RamFile, RamVfs};

/// A record store under measurement.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum BackendKind {
    /// The redb backend with the default commit: one phase, one fsync.
    Redb,
    /// The redb backend with quick repair: two-phase commit, and the
    /// allocator state saved at each commit.
    RedbQuickRepair,
    /// The append-only log with an in-memory index.
    AppendOnlyLog,
}

impl BackendKind {
    /// Every kind, in the order of the reports.
    pub const ALL: [BackendKind; 3] = [
        BackendKind::Redb,
        BackendKind::RedbQuickRepair,
        BackendKind::AppendOnlyLog,
    ];

    /// Return the name of the kind in reports and on the command line.
    pub fn name(self) -> &'static str {
        match self {
            BackendKind::Redb => "redb",
            BackendKind::RedbQuickRepair => "redb-quick-repair",
            BackendKind::AppendOnlyLog => "log",
        }
    }

    /// Return the kind that `name` names.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.name() == name)
    }

    /// Open a store of this kind in `vfs`, and create it if necessary.
    pub fn open<V: Vfs>(self, vfs: V) -> Result<Store> {
        match self {
            BackendKind::Redb => Store::open_with_quick_repair(vfs, SystemClock, false),
            BackendKind::RedbQuickRepair => Store::open_with_quick_repair(vfs, SystemClock, true),
            BackendKind::AppendOnlyLog => {
                let log = LogBackend::open(vfs)?;
                let next_oid = log.next_oid();
                Ok(Store::from_backend(Box::new(log), next_oid, SystemClock))
            }
        }
    }
}

/// One workload cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Cell {
    /// The size of each pickle in bytes.
    pub object_size: usize,
    /// The number of objects that each transaction writes.
    pub objects_per_txn: usize,
    /// The number of threads that commit at the same time.
    pub writers: usize,
}

impl Cell {
    /// Return the pickle bytes of one transaction.
    pub fn txn_bytes(&self) -> u64 {
        (self.object_size * self.objects_per_txn) as u64
    }
}

/// The numbers of one cell for one backend kind.
#[derive(Clone, Debug, Serialize)]
pub struct Row {
    /// The backend kind.
    pub kind: BackendKind,
    /// The workload cell.
    pub cell: Cell,
    /// The transactions in one backend commit: 1 on the commit path of
    /// the store, the number of writers in a batched estimate.
    pub batch: usize,
    /// The measured commits.
    pub commits: u64,
    /// The pickle bytes of the measured commits.
    pub pickle_bytes: u64,
    /// The bytes that the backend wrote to its files.
    pub bytes_written: u64,
    /// The calls to `write` on the files.
    pub write_calls: u64,
    /// The syncs of the files and of the directory.
    pub fsyncs: u64,
    /// The calls to `set_len` on the files: the backend grew or shrank a
    /// file.
    pub file_resizes: u64,
    /// The wall-clock time of the measured commits, in seconds.
    pub seconds: f64,
    /// Bytes written to the storage ÷ pickle bytes committed.
    pub write_amplification: f64,
    /// Syncs ÷ commits.
    pub fsyncs_per_commit: f64,
    /// Commits ÷ wall-clock seconds.
    pub commits_per_second: f64,
    /// Bytes written ÷ commits.
    pub bytes_written_per_commit: f64,
    /// The median time from `tpc_begin` to the return of `finish`, in
    /// milliseconds.
    pub p50_commit_ms: f64,
    /// The 99th percentile of the same time, in milliseconds.
    pub p99_commit_ms: f64,
}

/// The rows of every cell.
#[derive(Clone, Debug, Serialize)]
pub struct Report {
    /// One row per cell, in the order of the cells.
    pub rows: Vec<Row>,
}

/// How long each cell runs, and the emulated fsync.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct Plan {
    /// The time that each sync sleeps, in seconds.
    pub fsync_seconds: f64,
    /// The fewest measured commits in a cell.
    pub min_commits: u64,
    /// The measured commits for each writer, when this gives more than
    /// `min_commits`.
    pub commits_per_writer: u64,
    /// The most pickle bytes that a cell commits. A cell always commits
    /// at least once per writer.
    pub byte_budget: u64,
    /// The most batches of a batched estimate.
    pub max_batches: u64,
}

impl Plan {
    /// The plan of the acceptance test: a few commits per cell.
    pub fn quick(fsync_seconds: f64) -> Self {
        Self {
            fsync_seconds,
            min_commits: 24,
            commits_per_writer: 1,
            byte_budget: 256 << 20,
            max_batches: 5,
        }
    }

    /// The plan of the finding: more commits per cell.
    pub fn full(fsync_seconds: f64) -> Self {
        Self {
            fsync_seconds,
            min_commits: 100,
            commits_per_writer: 3,
            byte_budget: 1 << 30,
            max_batches: 25,
        }
    }

    /// Return the number of measured commits of `cell`.
    pub fn commits(&self, cell: &Cell) -> u64 {
        let writers = cell.writers as u64;
        let target = self.min_commits.max(self.commits_per_writer * writers);
        let cap = writers.max(self.byte_budget / cell.txn_bytes().max(1));
        target.min(cap)
    }

    /// Return the number of batches of a batched estimate of `cell`. A
    /// batch holds one transaction per writer.
    pub fn batches(&self, cell: &Cell) -> u64 {
        let batch_bytes = cell.txn_bytes() * cell.writers as u64;
        (self.byte_budget / batch_bytes.max(1)).clamp(1, self.max_batches)
    }
}

/// Measure every cell for `kind`, with the quick plan and syncs that cost
/// `fsync_seconds`.
pub fn measure(kind: BackendKind, cells: &[Cell], fsync_seconds: f64) -> Result<Report> {
    measure_with(kind, cells, &Plan::quick(fsync_seconds))
}

/// Measure every cell for `kind` with `plan`.
pub fn measure_with(kind: BackendKind, cells: &[Cell], plan: &Plan) -> Result<Report> {
    let rows = cells
        .iter()
        .map(|cell| measure_cell(kind, cell, plan))
        .collect::<Result<_>>()?;
    Ok(Report { rows })
}

/// Measure one cell for `kind` with `plan`, in a new store.
pub fn measure_cell(kind: BackendKind, cell: &Cell, plan: &Plan) -> Result<Row> {
    let meter = Arc::new(Meter::new(Duration::from_secs_f64(plan.fsync_seconds)));
    let store = kind.open(MeteredVfs::new(RamVfs::new(), Arc::clone(&meter)))?;
    let payload = payload(cell.object_size);

    // The setup creates the objects of each writer. The meter is not
    // armed, so it costs no fsync time and counts nothing.
    let mut sets = Vec::with_capacity(cell.writers);
    for _ in 0..cell.writers {
        let oids = (0..cell.objects_per_txn)
            .map(|_| store.new_oid())
            .collect::<Result<Vec<Oid>>>()?;
        let created: Vec<(Oid, Tid)> = oids.iter().map(|&oid| (oid, Tid::ZERO)).collect();
        let tid = commit(&store, &created, &payload)?;
        sets.push(oids.into_iter().map(|oid| (oid, tid)).collect::<Vec<_>>());
    }

    let commits = plan.commits(cell);
    let tickets = AtomicU64::new(commits);
    let start = Barrier::new(cell.writers + 1);
    meter.arm();
    let (latencies, elapsed) = thread::scope(|scope| -> Result<(Vec<Duration>, Duration)> {
        let workers: Vec<_> = sets
            .into_iter()
            .map(|mut set| {
                let (store, payload, tickets, start) = (&store, &payload, &tickets, &start);
                scope.spawn(move || -> Result<Vec<Duration>> {
                    start.wait();
                    let mut latencies = Vec::new();
                    while take(tickets) {
                        let begun = Instant::now();
                        let tid = commit(store, &set, payload)?;
                        latencies.push(begun.elapsed());
                        for entry in &mut set {
                            entry.1 = tid;
                        }
                    }
                    Ok(latencies)
                })
            })
            .collect();
        start.wait();
        let begun = Instant::now();
        let mut latencies = Vec::new();
        for worker in workers {
            latencies.extend(worker.join().expect("a writer panicked")?);
        }
        Ok((latencies, begun.elapsed()))
    })?;
    let counts = meter.disarm();
    drop(store);
    let commits = latencies.len() as u64;
    Ok(row(kind, cell, 1, commits, &counts, elapsed, &latencies))
}

/// Estimate a group commit of `cell` for `kind`. One thread writes
/// batches of `cell.writers` transactions, one per writer, each batch in
/// one backend write transaction. The latency of a commit is the time of
/// its batch.
pub fn measure_batched(kind: BackendKind, cell: &Cell, plan: &Plan) -> Result<Row> {
    let meter = Arc::new(Meter::new(Duration::from_secs_f64(plan.fsync_seconds)));
    let store = kind.open(MeteredVfs::new(RamVfs::new(), Arc::clone(&meter)))?;
    let payload = payload(cell.object_size);
    let backend = store.backend();
    let mut sets = Vec::with_capacity(cell.writers);
    for _ in 0..cell.writers {
        let oids = (0..cell.objects_per_txn)
            .map(|_| store.new_oid().map(|oid| (oid, Tid::ZERO)))
            .collect::<Result<Vec<(Oid, Tid)>>>()?;
        sets.push(oids);
    }
    let mut tid = backend.last_tid();
    // The setup batch creates the objects, with the meter disarmed.
    write_batch(backend, &mut sets, &mut tid, &payload)?;

    let batches = plan.batches(cell);
    let mut latencies = Vec::new();
    meter.arm();
    let begun = Instant::now();
    for _ in 0..batches {
        let started = Instant::now();
        write_batch(backend, &mut sets, &mut tid, &payload)?;
        latencies.extend(std::iter::repeat_n(started.elapsed(), cell.writers));
    }
    let elapsed = begun.elapsed();
    let counts = meter.disarm();
    drop(store);
    let commits = batches * cell.writers as u64;
    Ok(row(
        kind,
        cell,
        cell.writers,
        commits,
        &counts,
        elapsed,
        &latencies,
    ))
}

/// The size of the pickles that fill a big store on disk.
const BIG_OBJECT: usize = 1 << 20;

/// Measure one-object commits of 1 KB on a redb store of about `bytes`
/// bytes in `dir`, with each redb setting.
///
/// The default redb commit first fills the store with 1 MiB pickles, 100
/// per transaction, and the store closes cleanly. Then for each kind of
/// `kinds`, the store reopens with that setting, and one writer commits
/// `commits` transactions of one new 1 KB object. The meter counts only
/// these commits. The fsyncs are real, so the commit rate is the rate of
/// the disk under `dir`. Return the size of the store file and one row
/// per kind.
pub fn measure_big_store(
    kinds: &[BackendKind],
    dir: &Path,
    bytes: u64,
    commits: u64,
) -> Result<(u64, Vec<Row>)> {
    let meter = Arc::new(Meter::new(Duration::ZERO));
    {
        let store =
            BackendKind::Redb.open(MeteredVfs::new(OsVfs::new(dir)?, Arc::clone(&meter)))?;
        let big = payload(BIG_OBJECT);
        let mut filled = 0;
        while filled < bytes {
            let set = (0..100)
                .map(|_| store.new_oid().map(|oid| (oid, Tid::ZERO)))
                .collect::<Result<Vec<_>>>()?;
            commit(&store, &set, &big)?;
            filled += 100 * BIG_OBJECT as u64;
        }
    }
    let small = payload(1024);
    let cell = Cell {
        object_size: 1024,
        objects_per_txn: 1,
        writers: 1,
    };
    let mut rows = Vec::new();
    for &kind in kinds {
        let store = kind.open(MeteredVfs::new(OsVfs::new(dir)?, Arc::clone(&meter)))?;
        let mut latencies = Vec::new();
        meter.arm();
        let begun = Instant::now();
        for _ in 0..commits {
            let started = Instant::now();
            commit(&store, &[(store.new_oid()?, Tid::ZERO)], &small)?;
            latencies.push(started.elapsed());
        }
        let elapsed = begun.elapsed();
        let counts = meter.disarm();
        drop(store);
        rows.push(row(kind, &cell, 1, commits, &counts, elapsed, &latencies));
    }
    let file = std::fs::metadata(dir.join("zoe.redb"))?.len();
    Ok((file, rows))
}

/// The objects in each transaction that fills a store with background
/// records.
const FILL_BATCH: u64 = 1_000;

/// Measure one writer that updates objects spread over a big store.
///
/// The store first gets `background` records of `cell.object_size` bytes,
/// committed through the store, with the meter disarmed. Each measured
/// transaction then updates `cell.objects_per_txn` of these records, at
/// equal distances in the oid order. So in a B-tree, the objects of a
/// transaction rarely share a leaf. `cell.writers` must be 1.
pub fn measure_scattered(
    kind: BackendKind,
    cell: &Cell,
    background: u64,
    plan: &Plan,
) -> Result<Row> {
    assert_eq!(cell.writers, 1, "a scattered measurement has one writer");
    let meter = Arc::new(Meter::new(Duration::from_secs_f64(plan.fsync_seconds)));
    let store = kind.open(MeteredVfs::new(RamVfs::new(), Arc::clone(&meter)))?;
    let payload = payload(cell.object_size);

    let mut filled: Vec<(Oid, Tid)> = Vec::with_capacity(background as usize);
    while (filled.len() as u64) < background {
        let n = FILL_BATCH.min(background - filled.len() as u64);
        let created = (0..n)
            .map(|_| store.new_oid().map(|oid| (oid, Tid::ZERO)))
            .collect::<Result<Vec<_>>>()?;
        let tid = commit(&store, &created, &payload)?;
        filled.extend(created.into_iter().map(|(oid, _)| (oid, tid)));
    }
    let stride = (filled.len() / cell.objects_per_txn).max(1);
    let mut set: Vec<(Oid, Tid)> = filled
        .iter()
        .step_by(stride)
        .take(cell.objects_per_txn)
        .copied()
        .collect();
    drop(filled);

    let commits = plan.min_commits;
    let mut latencies = Vec::new();
    meter.arm();
    let begun = Instant::now();
    for _ in 0..commits {
        let started = Instant::now();
        let tid = commit(&store, &set, &payload)?;
        latencies.push(started.elapsed());
        for entry in &mut set {
            entry.1 = tid;
        }
    }
    let elapsed = begun.elapsed();
    let counts = meter.disarm();
    drop(store);
    let mut cell = *cell;
    cell.objects_per_txn = set.len();
    Ok(row(kind, &cell, 1, commits, &counts, elapsed, &latencies))
}

/// Write one transaction per set in one backend write transaction. Each
/// transaction takes the next tid after `tid` and writes `payload` to
/// every oid of its set.
fn write_batch(
    backend: &dyn Backend,
    sets: &mut [Vec<(Oid, Tid)>],
    tid: &mut Tid,
    payload: &[u8],
) -> Result<()> {
    let mut write = backend.begin_write()?;
    for set in sets.iter_mut() {
        *tid = Tid(tid.0 + 1);
        for (oid, serial) in set.iter_mut() {
            write.put_record(*oid, *tid, *serial, payload)?;
            *serial = *tid;
        }
        write.put_txn(&TxnMeta {
            tid: *tid,
            user: b"bench".to_vec(),
            description: b"record-store spike".to_vec(),
            extension: Vec::new(),
            oid_count: set.len() as u64,
            status: TxnStatus::Committed,
        })?;
    }
    write.commit()
}

/// Return the row of a measurement.
fn row(
    kind: BackendKind,
    cell: &Cell,
    batch: usize,
    commits: u64,
    counts: &Counts,
    elapsed: Duration,
    latencies: &[Duration],
) -> Row {
    let pickle_bytes = commits * cell.txn_bytes();
    let seconds = elapsed.as_secs_f64();
    let mut millis: Vec<f64> = latencies.iter().map(|d| d.as_secs_f64() * 1e3).collect();
    millis.sort_by(f64::total_cmp);
    Row {
        kind,
        cell: *cell,
        batch,
        commits,
        pickle_bytes,
        bytes_written: counts.bytes,
        write_calls: counts.writes,
        fsyncs: counts.syncs,
        file_resizes: counts.set_lens,
        seconds,
        write_amplification: counts.bytes as f64 / pickle_bytes as f64,
        fsyncs_per_commit: counts.syncs as f64 / commits as f64,
        commits_per_second: commits as f64 / seconds,
        bytes_written_per_commit: counts.bytes as f64 / commits as f64,
        p50_commit_ms: percentile(&millis, 0.50),
        p99_commit_ms: percentile(&millis, 0.99),
    }
}

/// Take one commit ticket. Return false when none is left.
fn take(tickets: &AtomicU64) -> bool {
    tickets
        .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
        .is_ok()
}

/// Commit one transaction that writes `payload` to every oid of `set`.
/// Each entry gives the oid and the serial that the writer read.
pub fn commit(store: &Store, set: &[(Oid, Tid)], payload: &[u8]) -> Result<Tid> {
    let mut txn = store.tpc_begin(b"bench", b"record-store spike", b"")?;
    for &(oid, serial) in set {
        txn.store(oid, serial, payload)?;
    }
    txn.vote()?;
    txn.finish()
}

/// Return `size` bytes that do not repeat in a short period.
pub fn payload(size: usize) -> Vec<u8> {
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    (0..size)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect()
}

/// Return the nearest-rank percentile `p` of the sorted values.
pub fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let rank = (p * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(object_size: usize, objects_per_txn: usize, writers: usize) -> Cell {
        Cell {
            object_size,
            objects_per_txn,
            writers,
        }
    }

    #[test]
    fn the_plan_bounds_the_commits_of_a_cell() {
        let plan = Plan::quick(0.0);
        assert_eq!(plan.commits(&cell(1024, 1, 1)), 24);
        assert_eq!(plan.commits(&cell(1024, 1, 64)), 64);
        // 10 MB transactions: the budget allows 25, one per writer is 64.
        assert_eq!(plan.commits(&cell(100 * 1024, 100, 1)), 24);
        assert_eq!(plan.commits(&cell(100 * 1024, 100, 64)), 64);
        let full = Plan::full(0.0);
        assert_eq!(full.commits(&cell(100 * 1024, 100, 64)), 104);
        assert_eq!(full.batches(&cell(1024, 1, 64)), 25);
        assert_eq!(full.batches(&cell(100 * 1024, 100, 64)), 1);
    }

    #[test]
    fn percentiles_use_the_nearest_rank() {
        let values: Vec<f64> = (1..=100).map(f64::from).collect();
        assert_eq!(percentile(&values, 0.5), 50.0);
        assert_eq!(percentile(&values, 0.99), 99.0);
        assert_eq!(percentile(&[7.0], 0.99), 7.0);
    }

    /// Every kind commits through the same path, and the log writes about
    /// the pickle bytes plus the frames.
    #[test]
    fn every_kind_counts_its_writes_and_syncs() {
        let plan = Plan {
            min_commits: 10,
            ..Plan::quick(0.0)
        };
        for kind in BackendKind::ALL {
            let row = measure_cell(kind, &cell(1024, 10, 2), &plan).unwrap();
            assert_eq!(row.commits, 10, "{row:?}");
            assert_eq!(row.pickle_bytes, 10 * 10 * 1024);
            assert!(row.write_amplification >= 1.0, "{row:?}");
            let syncs = if kind == BackendKind::RedbQuickRepair {
                2.0
            } else {
                1.0
            };
            assert_eq!(row.fsyncs_per_commit, syncs, "{row:?}");
        }

        let log = measure_cell(BackendKind::AppendOnlyLog, &cell(1024, 10, 1), &plan).unwrap();
        // Per record: a kind byte, a 16-byte key and a 32-byte header.
        let records = 10 * (1 + 16 + 32 + 1024);
        // Per transaction row: a kind byte, an 8-byte key, a 32-byte
        // header, 21 bytes of fixed fields, then the user and the
        // description.
        let row = 1 + 8 + 32 + 21 + 5 + 18;
        assert_eq!(log.bytes_written_per_commit, f64::from(records + row));
        assert_eq!(log.write_calls, log.commits);
    }

    /// Updates of spread objects in a filled store write their pickles.
    #[test]
    fn a_scattered_update_writes_its_objects() {
        let plan = Plan {
            min_commits: 3,
            ..Plan::quick(0.0)
        };
        for kind in BackendKind::ALL {
            let row = measure_scattered(kind, &cell(1024, 10, 1), 2_500, &plan).unwrap();
            assert_eq!((row.commits, row.cell.objects_per_txn), (3, 10), "{row:?}");
            assert!(row.write_amplification >= 1.0, "{row:?}");
        }
    }

    /// A batch of 4 transactions costs the syncs of one commit.
    #[test]
    fn a_batch_shares_its_syncs() {
        let plan = Plan {
            max_batches: 3,
            ..Plan::quick(0.0)
        };
        for kind in BackendKind::ALL {
            let row = measure_batched(kind, &cell(1024, 10, 4), &plan).unwrap();
            assert_eq!((row.batch, row.commits), (4, 12), "{row:?}");
            let syncs = if kind == BackendKind::RedbQuickRepair {
                2.0
            } else {
                1.0
            };
            assert_eq!(row.fsyncs_per_commit, syncs / 4.0, "{row:?}");
        }
    }
}
