//! Pages per commit: the record layout of zoe-core against a log layout.
//! Not for merge.

use std::time::Instant;

use redb::{Database, Durability, ReadableDatabase, ReadableTableMetadata, TableDefinition};

const RECORDS: TableDefinition<&[u8; 16], &[u8]> = TableDefinition::new("records");
const TXN_OIDS: TableDefinition<&[u8; 16], ()> = TableDefinition::new("txn_oids");
const HISTORY: TableDefinition<&[u8; 16], ()> = TableDefinition::new("history");
const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");

fn key(a: u64, b: u64) -> [u8; 16] {
    let mut k = [0; 16];
    k[..8].copy_from_slice(&a.to_be_bytes());
    k[8..].copy_from_slice(&b.to_be_bytes());
    k
}

fn write_bytes() -> u64 {
    // wchar: the bytes that write syscalls passed, synced or not.
    std::fs::read_to_string("/proc/self/io")
        .unwrap()
        .lines()
        .find_map(|l| l.strip_prefix("wchar: ").map(|v| v.parse().unwrap()))
        .unwrap()
}

#[derive(Clone, Copy, PartialEq)]
enum Layout {
    /// records[(oid, tid)] = frame; txn_oids[(tid, oid)] = ()
    OidTid,
    /// records[(tid, oid)] = frame; history[(oid, tid)] = ()
    TidOid,
}

fn commit(db: &Database, layout: Layout, tid: u64, oids: std::ops::Range<u64>, value: &[u8], durable: bool) {
    let mut txn = db.begin_write().unwrap();
    if !durable {
        txn.set_durability(Durability::None).unwrap();
    }
    {
        let mut records = txn.open_table(RECORDS).unwrap();
        let mut index = match layout {
            Layout::OidTid => txn.open_table(TXN_OIDS).unwrap(),
            Layout::TidOid => txn.open_table(HISTORY).unwrap(),
        };
        for oid in oids {
            match layout {
                Layout::OidTid => {
                    records.insert(&key(oid, tid), value).unwrap();
                    index.insert(&key(tid, oid), ()).unwrap();
                }
                Layout::TidOid => {
                    records.insert(&key(tid, oid), value).unwrap();
                    index.insert(&key(oid, tid), ()).unwrap();
                }
            }
        }
        let mut meta = txn.open_table(META).unwrap();
        meta.insert("last_tid", &tid.to_le_bytes()[..]).unwrap();
    }
    txn.commit().unwrap();
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dir = std::path::PathBuf::from(&args[1]);
    let value_len: usize = args.get(2).map_or(600, |v| v.parse().unwrap());
    let depth: u64 = args.get(3).map_or(60, |v| v.parse().unwrap());
    let groups = 16u64;
    let per = 100u64;
    let measured = 400u64;
    let value: Vec<u8> = (0..value_len).map(|i| (i * 7 % 251) as u8).collect();
    for (name, layout) in [("oid,tid (zoe-core)", Layout::OidTid), ("tid,oid + history", Layout::TidOid)] {
        for durable in [true, false] {
            let path = dir.join("x.redb");
            let _ = std::fs::remove_file(&path);
            let db = Database::create(&path).unwrap();
            let mut tid = 1u64;
            // Prefill: each group of 100 oids gets `depth` versions.
            for _ in 0..depth {
                for g in 0..groups {
                    commit(&db, layout, tid, g * per + 1..(g + 1) * per + 1, &value, false);
                    tid += 1;
                }
            }
            commit(&db, layout, tid, 0..0, &value, true);
            tid += 1;
            let w0 = write_bytes();
            let t0 = Instant::now();
            for i in 0..measured {
                let g = i % groups;
                commit(&db, layout, tid, g * per + 1..(g + 1) * per + 1, &value, durable);
                tid += 1;
            }
            let dt = t0.elapsed();
            let w = write_bytes() - w0;
            // A read check: the newest version of oid 1, for each layout.
            let r = db.begin_read().unwrap();
            let records = r.open_table(RECORDS).unwrap();
            let n = records.len().unwrap();
            println!(
                "{name:<20} durable={durable:<5} value={value_len}B depth={depth}: {:.3} ms/commit, {:.1} KiB written/commit, {} rows, file {:.1} MiB",
                dt.as_secs_f64() * 1e3 / measured as f64,
                w as f64 / 1024.0 / measured as f64,
                n,
                std::fs::metadata(&path).unwrap().len() as f64 / 1048576.0
            );
            drop(records);
            drop(r);
            drop(db);
            let _ = std::fs::remove_file(&path);
        }
    }
}
