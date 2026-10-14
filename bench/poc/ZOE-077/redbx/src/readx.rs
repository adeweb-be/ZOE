//! Cost of one load: a read transaction per load against a shared one,
//! with 1 and 16 threads. Not for merge.

use std::sync::Arc;
use std::time::Instant;

use redb::{Database, Durability, ReadableDatabase, ReadableTable, TableDefinition};

const RECORDS: TableDefinition<&[u8; 16], &[u8]> = TableDefinition::new("records");

fn key(a: u64, b: u64) -> [u8; 16] {
    let mut k = [0; 16];
    k[..8].copy_from_slice(&a.to_be_bytes());
    k[8..].copy_from_slice(&b.to_be_bytes());
    k
}

fn load(t: &redb::ReadOnlyTable<&'static [u8; 16], &'static [u8]>, oid: u64) -> usize {
    let start = key(oid, 0);
    let end = key(oid, u64::MAX);
    let mut range = t.range::<&[u8; 16]>(&start..&end).unwrap();
    let (_, v) = range.next_back().unwrap().unwrap();
    let v = v.value();
    // A copy, as Record::data is owned.
    v.to_vec().len()
}

fn main() {
    let path = std::path::PathBuf::from(std::env::args().nth(1).unwrap()).join("r.redb");
    let _ = std::fs::remove_file(&path);
    let db = Arc::new(Database::create(&path).unwrap());
    let value = vec![7u8; 1530];
    let oids = 1600u64;
    let mut tid = 1;
    for _ in 0..60 {
        let mut txn = db.begin_write().unwrap();
        txn.set_durability(Durability::None).unwrap();
        {
            let mut t = txn.open_table(RECORDS).unwrap();
            for oid in 1..=oids {
                t.insert(&key(oid, tid), value.as_slice()).unwrap();
            }
        }
        txn.commit().unwrap();
        tid += 1;
    }
    let txn = db.begin_write().unwrap();
    txn.commit().unwrap();
    let per = 200_000u64;
    for threads in [1u64, 16] {
        for shared in [false, true] {
            let t0 = Instant::now();
            let hs: Vec<_> = (0..threads)
                .map(|i| {
                    let db = db.clone();
                    std::thread::spawn(move || {
                        let mut n = 0;
                        let shared_txn = db.begin_read().unwrap();
                        let shared_t = shared_txn.open_table(RECORDS).unwrap();
                        for j in 0..per / threads.max(1) * threads.min(16) / threads {
                            let oid = (i * 7919 + j * 104729) % oids + 1;
                            if shared {
                                n += load(&shared_t, oid);
                            } else {
                                let txn = db.begin_read().unwrap();
                                let t = txn.open_table(RECORDS).unwrap();
                                n += load(&t, oid);
                            }
                        }
                        n
                    })
                })
                .collect();
            let mut total = 0;
            for h in hs {
                total += h.join().unwrap();
            }
            let dt = t0.elapsed();
            let loads = per as f64;
            println!(
                "threads={threads:<2} {:<34} {:.2} µs wall per load per thread, {:.0} loads/s ({total})",
                if shared { "one read txn reused" } else { "one read txn per load (zoe today)" },
                dt.as_secs_f64() * 1e6 / (loads / threads as f64),
                loads / dt.as_secs_f64()
            );
        }
    }
    let _ = std::fs::remove_file(&path);
}
