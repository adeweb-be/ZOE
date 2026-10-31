//! Acceptance test of the bytes that a commit writes with the write-ahead
//! log (ADR 0056; N4, N5).
//!
//! The bounds come from the spike of the log (`docs/spikes/write-log.md`),
//! which counted the bytes at the write calls, as the metered `Vfs` here
//! does, for commits of 100 records of 1 KB, half text that zstd shrinks
//! and half pseudo-random bytes, with zstd at level 3:
//!
//! - an `update` commit wrote 695,964 bytes without the log, all of them
//!   synced by the commit;
//! - with the log it synced 62,893 bytes, its log entry, and wrote 168,161
//!   bytes in all, its entry and its share of the fold.
//!
//! So an `update` commit here syncs its entry alone, at most 70,000 bytes;
//! writes at most 200,000 bytes in all; and writes at least three times
//! fewer bytes than the same commits without the log. A log entry holds
//! the record frames and a fixed part of the format, and nothing else. A
//! commit of large records writes the bytes that it writes without the
//! log, because it goes into the tables directly.
//!
//! The times of `finish` are not in this test: `finish_time.rs` of
//! zoe-bench measures them on a disk.

mod support;

use support::{MeteredVfs, Rng};
use zoe_core::testing::{commit_all, new_oid};
use zoe_core::{Compression, Oid, Options, Store, SystemClock, Tid, WriteLog};

/// The records of a commit.
const RECORDS: usize = 100;
/// The update commits of a run before the measure.
const WARMUP: usize = 20;
/// The update commits that a run measures. Their entries fit one slice of
/// the fold.
const COMMITS: usize = 60;
/// The most bytes of an entry beyond its record frames: the header of the
/// entry, the transaction row with its op, 13 bytes of op for each record
/// and the zeros of the alignment, with room to spare.
fn entry_overhead(records: usize) -> u64 {
    16 * records as u64 + 256
}

/// Return a record of 1 KiB as the spike wrote them: 512 bytes of text
/// that zstd shrinks, then 512 pseudo-random bytes.
fn record(rng: &mut Rng) -> Vec<u8> {
    let text = b"The quick brown fox jumps over the lazy dog. ";
    let mut data: Vec<u8> = text.iter().copied().cycle().take(512).collect();
    data.extend((0..512).map(|_| rng.next() as u8));
    data
}

/// What a run wrote.
struct Run {
    /// For each measured commit: the bytes that it wrote to the log, and
    /// the bytes of its record frames.
    commits: Vec<(u64, u64)>,
    /// The bytes written to the database file while the measured commits
    /// ran.
    during: u64,
    /// The bytes written to the log and to the database file by the
    /// measured commits and by the fold of their entries.
    log: u64,
    database: u64,
}

/// Return the sum of the frames of the records that `tid` wrote for
/// `oids`: a header of 32 bytes and the payload as stored.
fn frames(store: &Store, oids: &[Oid], tid: Tid) -> u64 {
    oids.iter()
        .map(|&oid| 32 + store.record_info(oid, tid).unwrap().stored_len)
        .sum()
}

fn open(metered: &MeteredVfs, write_log: WriteLog) -> Store {
    let options = Options {
        compression: Compression::Zstd { level: 3 },
        write_log,
        ..Options::default()
    };
    Store::open_full(metered.clone(), SystemClock, options).unwrap()
}

/// Commit 100 new records, then update them in each commit: `WARMUP`
/// commits, a fold, then `COMMITS` measured commits and their fold.
fn update_run(write_log: WriteLog) -> Run {
    let metered = MeteredVfs::default();
    let store = open(&metered, write_log);
    let mut rng = Rng::new(184);
    let oids: Vec<Oid> = (0..RECORDS).map(|_| new_oid(&store)).collect();
    let update = |serial: Tid, rng: &mut Rng| {
        let writes: Vec<_> = oids.iter().map(|&oid| (oid, serial, record(rng))).collect();
        commit_all(&store, &writes)
    };
    let mut serial = update(Tid::ZERO, &mut rng);
    for _ in 0..WARMUP {
        serial = update(serial, &mut rng);
    }
    store.fold().unwrap();

    metered.meter.reset();
    let mut commits = Vec::new();
    for _ in 0..COMMITS {
        let before = metered.meter.log_bytes();
        serial = update(serial, &mut rng);
        commits.push((
            metered.meter.log_bytes() - before,
            frames(&store, &oids, serial),
        ));
    }
    let during = metered.meter.database_bytes();
    store.fold().unwrap();
    Run {
        commits,
        during,
        log: metered.meter.log_bytes(),
        database: metered.meter.database_bytes(),
    }
}

#[test]
fn an_update_of_100_records_of_1_kb_syncs_its_entry_and_writes_far_fewer_bytes() {
    let logged = update_run(WriteLog::default());
    assert_eq!(logged.during, 0, "the finishes wrote the tables");
    for &(log, frames) in &logged.commits {
        assert!(
            log <= frames + entry_overhead(RECORDS),
            "an entry of {log} bytes for {frames} bytes of frames"
        );
        assert!(log <= 70_000, "a commit synced {log} bytes");
    }
    let per_commit = (logged.log + logged.database) / COMMITS as u64;
    assert!(per_commit <= 200_000, "{per_commit} bytes for each commit");

    let direct = update_run(WriteLog {
        enabled: false,
        ..WriteLog::default()
    });
    assert_eq!(direct.log, 0);
    let direct_per_commit = direct.database / COMMITS as u64;
    assert!(
        direct_per_commit >= 3 * per_commit,
        "{direct_per_commit} bytes for each commit without the log, {per_commit} with it"
    );
}

#[test]
fn an_add_of_100_records_of_1_kb_syncs_its_entry_alone() {
    let metered = MeteredVfs::default();
    let store = open(&metered, WriteLog::default());
    let mut rng = Rng::new(172);
    metered.meter.reset();
    for _ in 0..20 {
        let before = metered.meter.log_bytes();
        let writes: Vec<_> = (0..RECORDS)
            .map(|_| (new_oid(&store), Tid::ZERO, record(&mut rng)))
            .collect();
        let tid = commit_all(&store, &writes);
        let oids: Vec<Oid> = writes.iter().map(|&(oid, _, _)| oid).collect();
        let log = metered.meter.log_bytes() - before;
        let frames = frames(&store, &oids, tid);
        assert!(
            log <= frames + entry_overhead(RECORDS),
            "{log} for {frames}"
        );
        assert!(log <= 70_000, "a commit synced {log} bytes");
    }
    assert_eq!(
        metered.meter.database_bytes(),
        0,
        "the finishes wrote the tables"
    );
}

/// Commit 10 new records of 100 KiB in a new store and return the bytes
/// written to the log and to the database file.
fn large_run(write_log: WriteLog) -> (u64, u64) {
    let metered = MeteredVfs::default();
    let options = Options {
        write_log,
        ..Options::default()
    };
    let store = Store::open_full(metered.clone(), SystemClock, options).unwrap();
    let oids: Vec<Oid> = (0..10).map(|_| new_oid(&store)).collect();
    metered.meter.reset();
    let writes: Vec<_> = oids
        .iter()
        .map(|&oid| (oid, Tid::ZERO, vec![9; 100 << 10]))
        .collect();
    commit_all(&store, &writes);
    (metered.meter.log_bytes(), metered.meter.database_bytes())
}

#[test]
fn a_commit_of_10_records_of_100_kb_writes_what_it_writes_without_the_log() {
    let (log, database) = large_run(WriteLog::default());
    let (direct_log, direct_database) = large_run(WriteLog {
        enabled: false,
        ..WriteLog::default()
    });
    assert_eq!(log, 0, "the large records went to the log");
    assert_eq!(direct_log, 0);
    assert!(
        database.abs_diff(direct_database) * 100 <= direct_database,
        "{database} bytes with the log, {direct_database} without it"
    );
}
