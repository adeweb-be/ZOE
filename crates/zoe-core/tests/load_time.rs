//! A measurement of `load_before` in the core, as a server loads: a
//! record of 1 KB or 100 KB, with compression off and with zstd, from
//! a store on the disk whose pages are in the cache.
//!
//! The test is ignored, because its numbers mean something only in a
//! release build. Run it with:
//!
//! ```text
//! cargo test -p zoe-core --release --test load_time -- --ignored --nocapture
//! ```
//!
//! For each size and compression it prints the time of one load of the
//! current record of an object that has one version (`current`), of the
//! current record of an object that has two versions (`newest of two`),
//! and of the older record of such an object, before the newer one (`older
//! of two`), as a reader at an earlier tid loads. Each number is the
//! median, the 90th percentile and the mean of the loads, in µs.
//!
//! `ZOE_LOAD_OBJECTS` sets the number of objects of each store, 2000 by
//! default at 1 KB; at 100 KB it is one tenth of that. `ZOE_LOAD_DIR` sets
//! the directory of the store, the temporary directory of cargo by default.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use zoe_core::vfs::OsVfs;
use zoe_core::{Compression, Oid, Options, Store, SystemClock, Tid};

/// The records of a transaction.
const RECORDS: usize = 100;

/// The record of `oid` in its `version`, of `size` bytes. The first half
/// is text that zstd compresses well, the second half is pseudo-random
/// bytes that it does not, as the records of zodbshootout.
fn record(oid: Oid, version: u64, size: usize) -> Vec<u8> {
    let mut data = Vec::with_capacity(size);
    data.extend_from_slice(b"\x80\x03czodbshootout.speedtest\nPObject\nq\x00.\x80\x03}q\x01(");
    while data.len() < size / 2 {
        data.extend_from_slice(b"X\x04\x00\x00\x00attrq\x02K\x01X\x04\x00\x00\x00dataq\x03");
    }
    let mut state = oid.0 ^ (version << 32) ^ 0x9e37_79b9_7f4a_7c15;
    while data.len() < size {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        data.extend_from_slice(&state.to_le_bytes());
    }
    data.truncate(size);
    data
}

/// Commit `oids` in `version`. Return the tid.
fn commit(store: &Store, oids: &[(Oid, Tid)], version: u64, size: usize) -> Tid {
    let mut txn = store.tpc_begin(b"user", b"load time", b"").unwrap();
    for &(oid, serial) in oids {
        txn.store(oid, serial, &record(oid, version, size)).unwrap();
    }
    txn.vote().unwrap();
    txn.finish().unwrap()
}

/// Return the median, the 90th percentile and the mean of `samples`, in
/// µs.
fn summary(samples: &mut [Duration]) -> String {
    samples.sort();
    let micros = |d: Duration| d.as_secs_f64() * 1e6;
    let median = micros(samples[samples.len() / 2]);
    let p90 = micros(samples[samples.len() * 9 / 10]);
    let mean = micros(samples.iter().sum::<Duration>()) / samples.len() as f64;
    format!("median {median:7.1}  p90 {p90:7.1}  mean {mean:7.1}")
}

/// A pseudo-random order of `n` indexes.
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

/// Time one load of each of `oids` at `tid`, in `order`.
fn loads(store: &Store, oids: &[Oid], tid: Tid, order: &[usize], expect: usize) -> Vec<Duration> {
    let mut samples = Vec::with_capacity(order.len());
    for &i in order {
        let start = Instant::now();
        let loaded = store.load_before(oids[i], tid).unwrap().unwrap();
        samples.push(start.elapsed());
        assert_eq!(loaded.data.len(), expect);
    }
    samples
}

fn run(dir: &Path, compression: Compression, size: usize, objects: usize) {
    let _ = std::fs::remove_dir_all(dir);
    let options = Options {
        compression,
        ..Options::default()
    };
    let store = Store::open_full(OsVfs::new(dir).unwrap(), SystemClock, options).unwrap();
    // Half of the objects get one version, the other half two.
    let oids: Vec<Oid> = (0..objects).map(|_| store.new_oid().unwrap()).collect();
    let mut first = Vec::new();
    for chunk in oids.chunks(RECORDS) {
        let new: Vec<(Oid, Tid)> = chunk.iter().map(|&oid| (oid, Tid::ZERO)).collect();
        first.push(commit(&store, &new, 0, size));
    }
    let between = Tid(first.last().unwrap().0 + 1);
    let (single, double) = oids.split_at(objects / 2);
    let serial_of = |oid: Oid| -> Tid {
        let i = oids.iter().position(|&o| o == oid).unwrap();
        first[i / RECORDS]
    };
    for chunk in double.chunks(RECORDS) {
        let again: Vec<(Oid, Tid)> = chunk.iter().map(|&oid| (oid, serial_of(oid))).collect();
        commit(&store, &again, 1, size);
    }
    let name = match compression {
        Compression::None => "none",
        _ => "zstd",
    };
    let rounds = if size > 10_000 { 3 } else { 10 };
    let order: Vec<usize> = (0..rounds)
        .flat_map(|r| shuffled(single.len(), r as u64 + 7))
        .collect();
    // Warm the page cache.
    loads(&store, single, Tid::MAX, &order[..single.len()], size);
    loads(&store, double, Tid::MAX, &order[..single.len()], size);
    println!(
        "{size} bytes, compression {name}, {} loads of each",
        order.len()
    );
    let mut current = loads(&store, single, Tid::MAX, &order, size);
    println!("  current           {}", summary(&mut current));
    let mut newest = loads(&store, double, Tid::MAX, &order, size);
    println!("  newest of two     {}", summary(&mut newest));
    let mut older = loads(&store, double, between, &order, size);
    println!("  older of two      {}", summary(&mut older));
    drop(store);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
#[ignore = "a measurement: run it in release"]
fn load_time_of_a_record() {
    let objects: usize = std::env::var("ZOE_LOAD_OBJECTS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(2000);
    let dir = std::env::var_os("ZOE_LOAD_DIR")
        .map_or_else(|| PathBuf::from(env!("CARGO_TARGET_TMPDIR")), PathBuf::from)
        .join("load-time");
    for size in [1024, 100 * 1024] {
        let objects = if size > 10_000 { objects / 10 } else { objects };
        for compression in [Compression::None, Compression::default()] {
            run(&dir, compression, size, objects);
        }
    }
}
