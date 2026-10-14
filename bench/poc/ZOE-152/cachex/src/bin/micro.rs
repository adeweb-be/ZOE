//! Micro-benchmarks of the prototype. Not for merge.
//!
//! `micro codec FILE`: ratio and time of zstd on real pickles.
//! `micro lookup FILE`: the time of a hit in the cache, without Python.
//! `micro lock DIR`: the time of the writer lock and of the apply of a
//! transaction on a shared file.
//!
//! FILE holds pickles from `dump_pickles.py`. Output: one JSON line each.

use std::hint::black_box;
use std::path::Path;
use std::time::Instant;

use cachex::cache::{Cache, Config, Mode, Session};

fn pickles(path: &Path) -> Vec<Vec<u8>> {
    let b = std::fs::read(path).expect("pickles");
    let mut out = Vec::new();
    let mut i = 0;
    while i + 4 <= b.len() {
        let n = u32::from_le_bytes(b[i..i + 4].try_into().unwrap()) as usize;
        out.push(b[i + 4..i + 4 + n].to_vec());
        i += 4 + n;
    }
    out
}

fn median(mut v: Vec<f64>) -> (f64, f64, f64) {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    (v[v.len() / 2], v[0], v[v.len() - 1])
}

fn codec(file: &Path, rounds: usize) {
    let p = pickles(file);
    let mean = p.iter().map(Vec::len).sum::<usize>() as f64 / p.len() as f64;
    let xx: Vec<f64> = (0..rounds)
        .map(|_| {
            let t = Instant::now();
            for d in &p {
                black_box(xxhash_rust::xxh3::xxh3_64(d));
            }
            t.elapsed().as_secs_f64() * 1e6 / p.len() as f64
        })
        .collect();
    let copy: Vec<f64> = (0..rounds)
        .map(|_| {
            let t = Instant::now();
            for d in &p {
                black_box(d.clone());
            }
            t.elapsed().as_secs_f64() * 1e6 / p.len() as f64
        })
        .collect();
    let (x, xl, xh) = median(xx);
    let (c, cl, ch) = median(copy);
    println!(
        "{{\"bench\":\"codec\",\"file\":{:?},\"records\":{},\"mean_bytes\":{:.0},\"op\":\"xxh3\",\"us\":{x:.3},\"us_min\":{xl:.3},\"us_max\":{xh:.3}}}",
        file.display().to_string(),
        p.len(),
        mean
    );
    println!(
        "{{\"bench\":\"codec\",\"file\":{:?},\"records\":{},\"mean_bytes\":{:.0},\"op\":\"copy\",\"us\":{c:.3},\"us_min\":{cl:.3},\"us_max\":{ch:.3}}}",
        file.display().to_string(),
        p.len(),
        mean
    );
    for level in [-5, -1, 1, 3] {
        let mut cctx = zstd::bulk::Compressor::new(level).unwrap();
        let z: Vec<Vec<u8>> = p.iter().map(|d| cctx.compress(d).unwrap()).collect();
        let ratio = z.iter().map(Vec::len).sum::<usize>() as f64
            / p.iter().map(Vec::len).sum::<usize>() as f64;
        let comp: Vec<f64> = (0..rounds.min(5))
            .map(|_| {
                let t = Instant::now();
                for d in &p {
                    black_box(cctx.compress(d).unwrap());
                }
                t.elapsed().as_secs_f64() * 1e6 / p.len() as f64
            })
            .collect();
        let mut dctx = zstd::bulk::Decompressor::new().unwrap();
        let mut buf = vec![0u8; p.iter().map(Vec::len).max().unwrap()];
        let dec: Vec<f64> = (0..rounds)
            .map(|_| {
                let t = Instant::now();
                for (zz, d) in z.iter().zip(&p) {
                    let n = dctx.decompress_to_buffer(zz, &mut buf[..d.len()]).unwrap();
                    black_box(n);
                }
                t.elapsed().as_secs_f64() * 1e6 / p.len() as f64
            })
            .collect();
        let (cm, _, _) = median(comp);
        let (dm, dl, dh) = median(dec);
        println!(
            "{{\"bench\":\"codec\",\"file\":{:?},\"records\":{},\"mean_bytes\":{:.0},\"op\":\"zstd\",\"level\":{level},\"ratio\":{ratio:.3},\"compress_us\":{cm:.3},\"decompress_us\":{dm:.3},\"decompress_us_min\":{dl:.3},\"decompress_us_max\":{dh:.3}}}",
            file.display().to_string(),
            p.len(),
            mean
        );
    }
}

fn lookup(file: &Path, rounds: usize) {
    let p = pickles(file);
    for (name, compress_min) in [("raw", None), ("zstd1", Some(0))] {
        let cfg = Config {
            ring: 256 << 20,
            mode: Mode::Memory,
            compress_min,
            level: 1,
        };
        let s = Session::new(Cache::open(None, &cfg).unwrap());
        s.set_last_tid(10);
        for (i, d) in p.iter().enumerate() {
            s.store(i as u64, 5, 0, d);
        }
        let n = p.len() as u64;
        let mut buf = vec![0u8; p.iter().map(Vec::len).max().unwrap()];
        let times: Vec<f64> = (0..rounds)
            .map(|_| {
                let t = Instant::now();
                for oid in 0..n {
                    let r = s.cache.load_before_with(oid, 11, |len, fill| {
                        if fill(&mut buf[..len]) { Some(len) } else { None }
                    });
                    assert!(r.is_some());
                }
                t.elapsed().as_secs_f64() * 1e9 / n as f64
            })
            .collect();
        let (m, l, h) = median(times);
        println!(
            "{{\"bench\":\"lookup\",\"file\":{:?},\"codec\":\"{name}\",\"entries\":{n},\"ns_per_hit\":{m:.0},\"ns_min\":{l:.0},\"ns_max\":{h:.0}}}",
            file.display().to_string()
        );
    }
}

fn lock(dir: &Path, rounds: usize) {
    let path = dir.join("lock.zoec");
    let _ = std::fs::remove_file(&path);
    let cfg = Config {
        ring: 64 << 20,
        mode: Mode::Shared,
        compress_min: None,
        level: 1,
    };
    let s = Session::new(Cache::open(Some(&path), &cfg).unwrap());
    let n = 100_000;
    let t = Instant::now();
    for _ in 0..n {
        black_box(s.cache.lock());
    }
    let shared_lock = t.elapsed().as_secs_f64() * 1e9 / n as f64;
    let m = std::sync::Mutex::new(());
    let t = Instant::now();
    for _ in 0..n {
        black_box(m.lock().unwrap());
    }
    let mutex = t.elapsed().as_secs_f64() * 1e9 / n as f64;
    // The apply of a transaction of 100 oids that the cache holds.
    let data = vec![7u8; 2800];
    let mut tid = 10;
    s.set_last_tid(tid);
    for oid in 0..10_000u64 {
        s.store(oid, 5, 0, &data);
    }
    let mut times = Vec::new();
    for r in 0..rounds {
        tid += 1;
        let t = Instant::now();
        for k in 0..100u64 {
            s.invalidate((r as u64 * 100 + k) % 10_000, Some(tid));
        }
        s.set_last_tid(tid);
        times.push(t.elapsed().as_secs_f64() * 1e6);
    }
    let (am, al, ah) = median(times);
    println!(
        "{{\"bench\":\"lock\",\"ns_writer_lock_shared_file\":{shared_lock:.0},\"ns_mutex\":{mutex:.0},\"us_apply_100_oids\":{am:.1},\"us_apply_min\":{al:.1},\"us_apply_max\":{ah:.1}}}"
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let rounds = 21;
    match args.get(1).map(String::as_str) {
        Some("codec") => codec(Path::new(&args[2]), rounds),
        Some("lookup") => lookup(Path::new(&args[2]), rounds),
        Some("lock") => lock(Path::new(&args[2]), rounds),
        _ => eprintln!("usage: micro codec|lookup FILE | micro lock DIR"),
    }
}
