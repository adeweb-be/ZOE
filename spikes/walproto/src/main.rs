//! The prototype of ZOE-184: the layout `main` (the core as it is) against
//! the layout `log` (a write-ahead log with a background checkpoint).
mod baseline;
mod frame;
mod io;
mod logstore;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use baseline::Baseline;
use logstore::{LogStore, Member};

const RECORDS: usize = 100;

fn member(records: Vec<(u64, u64, Vec<u8>)>, precompress: bool, compress: bool) -> Member {
    let m = Member { records, encoded: None };
    if precompress { m.precompress(compress) } else { m }
}

fn micros(d: Duration) -> f64 {
    d.as_secs_f64() * 1e6
}

fn pct(samples: &mut [Duration], p: f64) -> f64 {
    samples.sort();
    let i = ((samples.len() as f64 - 1.0) * p).round() as usize;
    micros(samples[i.min(samples.len() - 1)])
}

struct Args(HashMap<String, String>);

impl Args {
    fn get(&self, key: &str, default: &str) -> String {
        self.0.get(key).cloned().unwrap_or_else(|| default.to_owned())
    }
    fn num(&self, key: &str, default: u64) -> u64 {
        self.0.get(key).map(|v| v.parse().unwrap()).unwrap_or(default)
    }
}

fn shuffled(n: usize, seed: u64) -> Vec<usize> {
    let mut order: Vec<usize> = (0..n).collect();
    let mut state = seed | 1;
    for i in (1..n).rev() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        order.swap(i, (state % (i as u64 + 1)) as usize);
    }
    order
}

/// A clean directory for a store.
fn fresh(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).unwrap();
}

/// The commit measurement: `txns` transactions of 100 records of `size`
/// bytes on each of `c` threads, `add` or `update`.
fn run_commit(args: &Args) {
    let layout = args.get("layout", "main");
    let scenario = args.get("scenario", "add");
    let size = args.num("size", 1024) as usize;
    let c = args.num("c", 1) as usize;
    let txns = args.num("txns", 100) as usize;
    let compress = args.num("compress", 1) == 1;
    let threshold = args.num("threshold", 64) << 20;
    let max_pending = args.num("max_pending", 1024) << 20;
    let precompress = args.num("precompress", 0) == 1;
    let dir = PathBuf::from(args.get("dir", "/home/user/ZOE/target/walproto"));
    fresh(&dir);
    let update = scenario == "update";
    let finishes: Mutex<Vec<Duration>> = Mutex::new(Vec::new());
    let started = Instant::now();
    let mut out = format!(
        "{{\"layout\":\"{layout}\",\"scenario\":\"{scenario}\",\"size\":{size},\"c\":{c},\"txns\":{txns},\"compress\":{compress},\"precompress\":{precompress}"
    );
    let wall;
    if layout == "main" {
        let b = Baseline::open(&dir, compress);
        // Populate: each thread's objects, for update.
        let owned: Vec<Vec<(u64, u64)>> = (0..c)
            .map(|_| {
                let oids = b.new_oids(RECORDS);
                let pairs: Vec<(u64, u64)> = oids.iter().map(|&o| (o, 0)).collect();
                if update {
                    let (_, tid) = b.commit(&pairs, 0, size);
                    oids.iter().map(|&o| (o, tid)).collect()
                } else {
                    pairs
                }
            })
            .collect();
        b.io.take();
        let begin = Instant::now();
        std::thread::scope(|scope| {
            for mut mine in owned {
                let b = &b;
                let finishes = &finishes;
                scope.spawn(move || {
                    let mut local = Vec::with_capacity(txns);
                    for i in 0..txns {
                        if update {
                            let (d, tid) = b.commit(&mine, i as u64 + 1, size);
                            for pair in &mut mine {
                                pair.1 = tid;
                            }
                            local.push(d);
                        } else {
                            let oids = b.new_oids(RECORDS);
                            let pairs: Vec<(u64, u64)> = oids.iter().map(|&o| (o, 0)).collect();
                            local.push(b.commit(&pairs, 0, size).0);
                        }
                    }
                    finishes.lock().unwrap().extend(local);
                });
            }
        });
        wall = begin.elapsed();
        let taken = b.io.take();
        let commits = (c * txns) as u64;
        out += &format!(
            ",\"bytes_per_commit\":{},\"syncs_per_commit\":{:.3},\"sync_ms_total\":{:.1},\"write_ms_total\":{:.1},\"writes_per_commit\":{:.1}",
            taken.written / commits,
            taken.syncs as f64 / commits as f64,
            taken.sync.as_secs_f64() * 1e3,
            taken.write.as_secs_f64() * 1e3,
            taken.writes as f64 / commits as f64
        );
    } else {
        let (s, _) = LogStore::open(&dir, compress, threshold, max_pending);
        let owned: Vec<Vec<(u64, u64)>> = (0..c)
            .map(|_| {
                let oids = s.new_oids(RECORDS);
                if update {
                    let records = oids.iter().map(|&o| (o, 0, frame::record(o, 0, size))).collect();
                    let (tid, _) = s.finish(member(records, precompress, compress));
                    oids.iter().map(|&o| (o, tid)).collect()
                } else {
                    oids.iter().map(|&o| (o, 0)).collect()
                }
            })
            .collect();
        s.force_checkpoint();
        s.tree_io.take();
        *s.stats.lock().unwrap() = Default::default();
        s.log_bytes.store(0, Ordering::SeqCst);
        s.syncs.store(0, Ordering::SeqCst);
        s.sync_nanos.store(0, Ordering::SeqCst);
        s.batches.store(0, Ordering::SeqCst);
        let begin = Instant::now();
        std::thread::scope(|scope| {
            for mut mine in owned {
                let s = &s;
                let finishes = &finishes;
                scope.spawn(move || {
                    let mut local = Vec::with_capacity(txns);
                    for i in 0..txns {
                        if update {
                            let records = mine
                                .iter()
                                .map(|&(o, prev)| (o, prev, frame::record(o, i as u64 + 1, size)))
                                .collect();
                            let (tid, d) = s.finish(member(records, precompress, compress));
                            for pair in &mut mine {
                                pair.1 = tid;
                            }
                            local.push(d);
                        } else {
                            let oids = s.new_oids(RECORDS);
                            let records = oids.iter().map(|&o| (o, 0, frame::record(o, 0, size))).collect();
                            local.push(s.finish(member(records, precompress, compress)).1);
                        }
                    }
                    finishes.lock().unwrap().extend(local);
                });
            }
        });
        wall = begin.elapsed();
        let commits = (c * txns) as u64;
        let log_bytes = s.log_bytes.load(Ordering::SeqCst);
        let syncs = s.syncs.load(Ordering::SeqCst);
        let sync_nanos = s.sync_nanos.load(Ordering::SeqCst);
        let stalls = s.stalls.load(Ordering::SeqCst);
        let stall_ms = s.stall_nanos.load(Ordering::SeqCst) as f64 / 1e6;
        let folds_during = s.stats.lock().unwrap().folds;
        let fold_start = Instant::now();
        s.force_checkpoint();
        let final_fold = fold_start.elapsed();
        let stats = s.stats.lock().unwrap().clone();
        let tree_bytes = stats.tree_bytes;
        out += &format!(
            ",\"log_bytes_per_commit\":{},\"tree_bytes_per_commit\":{},\"log_syncs_per_commit\":{:.3},\"log_sync_mean_us\":{:.0},\"commits_per_batch\":{:.2},\"stalls\":{stalls},\"stall_ms\":{stall_ms:.1},\"folds_during\":{folds_during},\"final_fold_ms\":{:.1},\"cp\":{{\"folds\":{},\"commits\":{},\"records\":{},\"record_bytes\":{},\"tree_bytes\":{},\"fold_ms\":{:.1},\"longest_fold_ms\":{:.1},\"records_per_s\":{:.0},\"amplification\":{:.2}}}",
            log_bytes / commits,
            tree_bytes / commits,
            syncs as f64 / commits as f64,
            if syncs > 0 { sync_nanos as f64 / syncs as f64 / 1e3 } else { 0.0 },
            commits as f64 / syncs.max(1) as f64,
            final_fold.as_secs_f64() * 1e3,
            stats.folds,
            stats.commits,
            stats.records,
            stats.record_bytes,
            stats.tree_bytes,
            stats.fold_nanos as f64 / 1e6,
            stats.longest_fold_nanos as f64 / 1e6,
            if stats.fold_nanos > 0 { stats.records as f64 / (stats.fold_nanos as f64 / 1e9) } else { 0.0 },
            if stats.record_bytes > 0 { stats.tree_bytes as f64 / stats.record_bytes as f64 } else { 0.0 },
        );
        s.close(true);
    }
    let mut all = finishes.into_inner().unwrap();
    let commits = all.len();
    out += &format!(
        ",\"commits\":{commits},\"finish_p50_us\":{:.0},\"finish_p90_us\":{:.0},\"finish_p99_us\":{:.0},\"finish_max_us\":{:.0},\"commits_per_s\":{:.1},\"wall_ms\":{:.0},\"total_ms\":{:.0}}}",
        pct(&mut all, 0.5),
        pct(&mut all, 0.9),
        pct(&mut all, 0.99),
        pct(&mut all, 1.0),
        commits as f64 / wall.as_secs_f64(),
        wall.as_secs_f64() * 1e3,
        started.elapsed().as_secs_f64() * 1e3,
    );
    println!("{out}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The read measurement: loads of tree records with a table of `table`
/// records in memory, and loads of table records; with a writer that
/// keeps the checkpoint busy when `under_fold` is 1.
fn run_reads(args: &Args) {
    let size = args.num("size", 1024) as usize;
    let table = args.num("table", 0) as usize;
    let under_fold = args.num("under_fold", 0) == 1;
    let compress = args.num("compress", 1) == 1;
    let loads = args.num("loads", 20000) as usize;
    let precompress = false;
    let dir = PathBuf::from(args.get("dir", "/home/user/ZOE/target/walproto"));
    fresh(&dir);
    let tree_objects = 10_000usize;
    // The baseline load, for reference.
    let main_p50 = {
        let b = Baseline::open(&dir.join("main"), compress);
        let mut oids = Vec::new();
        for _ in 0..tree_objects / RECORDS {
            let new = b.new_oids(RECORDS);
            let pairs: Vec<(u64, u64)> = new.iter().map(|&o| (o, 0)).collect();
            b.commit(&pairs, 0, size);
            oids.extend(new);
        }
        let order = shuffled(oids.len(), 3);
        for &i in &order[..1000] {
            b.load(oids[i], u64::MAX);
        }
        let mut samples = Vec::with_capacity(loads);
        for k in 0..loads {
            let oid = oids[order[k % order.len()]];
            let start = Instant::now();
            b.load(oid, u64::MAX);
            samples.push(start.elapsed());
        }
        (pct(&mut samples, 0.5), pct(&mut samples, 0.99))
    };
    let threshold = if under_fold { 4 << 20 } else { u64::MAX / 2 };
    let (s, _) = LogStore::open(&dir.join("log"), compress, threshold, u64::MAX / 2);
    let mut tree_oids = Vec::new();
    for _ in 0..tree_objects / RECORDS {
        let oids = s.new_oids(RECORDS);
        let records = oids.iter().map(|&o| (o, 0, frame::record(o, 0, size))).collect();
        s.finish(member(records, precompress, compress));
        tree_oids.extend(oids);
    }
    s.force_checkpoint();
    assert_eq!(s.memtable_len(), 0);
    // The table: records that are not folded. With `under_fold`, the
    // writer below fills it instead.
    let mut table_oids = Vec::new();
    if !under_fold {
        for _ in 0..table.div_ceil(RECORDS) {
            let oids = s.new_oids(RECORDS);
            let records = oids.iter().map(|&o| (o, 0, frame::record(o, 0, size))).collect();
            s.finish(member(records, precompress, compress));
            table_oids.extend(oids);
        }
        assert_eq!(s.memtable_len(), table_oids.len());
    }
    let stop = Arc::new(AtomicBool::new(false));
    let writer = under_fold.then(|| {
        let s = s.clone();
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut n = 0u64;
            while !stop.load(Ordering::SeqCst) {
                let oids = s.new_oids(RECORDS);
                let records = oids.iter().map(|&o| (o, 0, frame::record(o, 0, size))).collect();
                s.finish(member(records, precompress, compress));
                n += 1;
            }
            n
        })
    });
    let measure = |oids: &[u64], loads: usize| -> (f64, f64) {
        let order = shuffled(oids.len(), 5);
        for &i in &order[..order.len().min(1000)] {
            s.load_before(oids[i], u64::MAX).unwrap();
        }
        let mut samples = Vec::with_capacity(loads);
        for k in 0..loads {
            let oid = oids[order[k % order.len()]];
            let start = Instant::now();
            s.load_before(oid, u64::MAX).unwrap();
            samples.push(start.elapsed());
        }
        (pct(&mut samples, 0.5), pct(&mut samples, 0.99))
    };
    let tree = measure(&tree_oids, loads);
    let hits = if table_oids.is_empty() { (0.0, 0.0) } else { measure(&table_oids, loads) };
    stop.store(true, Ordering::SeqCst);
    let written = writer.map(|w| w.join().unwrap()).unwrap_or(0);
    let folds = s.stats.lock().unwrap().folds;
    println!(
        "{{\"table\":{},\"size\":{size},\"under_fold\":{under_fold},\"main_load_p50_us\":{:.1},\"main_load_p99_us\":{:.1},\"tree_load_p50_us\":{:.1},\"tree_load_p99_us\":{:.1},\"table_load_p50_us\":{:.1},\"table_load_p99_us\":{:.1},\"writer_commits\":{written},\"folds\":{folds},\"mem_bytes\":{}}}",
        s.memtable_len(),
        main_p50.0,
        main_p50.1,
        tree.0,
        tree.1,
        hits.0,
        hits.1,
        s.mem_bytes.load(Ordering::SeqCst)
    );
    s.close(true);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The recovery measurement: `mb` MB of log that no checkpoint folded,
/// then an open that replays it and a fold of the replayed records.
fn run_recovery(args: &Args) {
    let size = args.num("size", 1024) as usize;
    let mb = args.num("mb", 64);
    let compress = args.num("compress", 1) == 1;
    let precompress = false;
    let dir = PathBuf::from(args.get("dir", "/home/user/ZOE/target/walproto"));
    fresh(&dir);
    let (s, _) = LogStore::open(&dir, compress, u64::MAX / 2, u64::MAX / 2);
    let mut commits = 0u64;
    while s.log_bytes.load(Ordering::SeqCst) < mb << 20 {
        let oids = s.new_oids(RECORDS);
        let records = oids.iter().map(|&o| (o, 0, frame::record(o, 0, size))).collect();
        s.finish(member(records, precompress, compress));
        commits += 1;
    }
    let log_bytes = s.log_bytes.load(Ordering::SeqCst);
    let records = s.memtable_len();
    s.close(false);
    drop(s);
    let start = Instant::now();
    let (s, (replayed, replayed_records, replay)) = LogStore::open(&dir, compress, u64::MAX / 2, u64::MAX / 2);
    let open = start.elapsed();
    assert_eq!(replayed_records as usize, records);
    let fold_start = Instant::now();
    s.force_checkpoint();
    let fold = fold_start.elapsed();
    println!(
        "{{\"log_mb\":{:.1},\"commits\":{commits},\"records\":{records},\"replayed_bytes\":{replayed},\"replay_ms\":{:.1},\"open_ms\":{:.1},\"replay_ms_per_mb\":{:.2},\"fold_ms\":{:.1}}}",
        log_bytes as f64 / 1048576.0,
        replay.as_secs_f64() * 1e3,
        open.as_secs_f64() * 1e3,
        replay.as_secs_f64() * 1e3 / (log_bytes as f64 / 1048576.0),
        fold.as_secs_f64() * 1e3
    );
    s.close(true);
    let _ = std::fs::remove_dir_all(&dir);
}

fn main() {
    let mut argv = std::env::args().skip(1);
    let cmd = argv.next().unwrap_or_else(|| "commit".into());
    let map: HashMap<String, String> = argv
        .filter_map(|a| a.split_once('=').map(|(k, v)| (k.to_owned(), v.to_owned())))
        .collect();
    let args = Args(map);
    match cmd.as_str() {
        "commit" => run_commit(&args),
        "reads" => run_reads(&args),
        "recovery" => run_recovery(&args),
        other => panic!("unknown command {other}"),
    }
}
