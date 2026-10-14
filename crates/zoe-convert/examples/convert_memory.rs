//! Convert a `Data.fs` into a store as `zoe convert --from-data-fs` does,
//! with a page cache of a given size, and log the resident memory.
//!
//! Usage:
//!
//! ```text
//! convert_memory DATA_FS STORE CACHE_MIB [BLOB_DIR]
//! ```
//!
//! A thread reads `VmRSS` and `VmHWM` from `/proc/self/status` each
//! second and writes them to stderr with the size of the store file. So
//! the log tells the copy pass (the store grows) from the verification
//! pass (the store keeps its size). The last line gives the peak.

use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use zoe_core::Store;

/// Return the value in KiB of the field `name` of `/proc/self/status`.
fn status_kib(name: &str) -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    status
        .lines()
        .find_map(|line| line.strip_prefix(name))
        .and_then(|rest| rest.trim().trim_end_matches("kB").trim().parse().ok())
        .unwrap_or(0)
}

fn sample(start: Instant, database: &Path) {
    let store_kib = std::fs::metadata(database).map_or(0, |m| m.len() / 1024);
    eprintln!(
        "t={:>6}s rss_kib={} hwm_kib={} store_kib={store_kib}",
        start.elapsed().as_secs(),
        status_kib("VmRSS:"),
        status_kib("VmHWM:"),
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!("usage: {} DATA_FS STORE CACHE_MIB [BLOB_DIR]", args[0]);
        std::process::exit(2);
    }
    let data_fs = PathBuf::from(&args[1]);
    let dir = PathBuf::from(&args[2]);
    let cache_mib: usize = args[3].parse().expect("a number of MiB");
    let blob_dir = args.get(4).map(PathBuf::from);

    let start = Instant::now();
    let database = dir.join("zoe.redb");
    {
        let database = database.clone();
        thread::spawn(move || {
            loop {
                sample(start, &database);
                thread::sleep(Duration::from_secs(1));
            }
        });
    }

    let store = Store::open_with_cache_size(&dir, cache_mib << 20).expect("open the store");
    let report = zoe_convert::from_datafs(&data_fs, blob_dir.as_deref(), &store).expect("convert");
    sample(start, &database);
    println!(
        "cache_mib={cache_mib} transactions={} copied={} records={} blobs={} problems={} \
         hwm_kib={}",
        report.transactions,
        report.copied,
        report.records,
        report.blobs,
        report.problems.len(),
        status_kib("VmHWM:"),
    );
}
