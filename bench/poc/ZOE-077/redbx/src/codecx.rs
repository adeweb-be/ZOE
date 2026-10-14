//! Cost per record of the codec terms, on real pickles. Not for merge.

use std::time::Instant;

use xxhash_rust::xxh3::Xxh3;
use zstd::bulk::{Compressor, Decompressor};

fn main() {
    let raw = std::fs::read(std::env::args().nth(1).unwrap()).unwrap();
    let mut samples = Vec::new();
    let mut i = 0;
    while i < raw.len() {
        let n = u32::from_le_bytes(raw[i..i + 4].try_into().unwrap()) as usize;
        samples.push(raw[i + 4..i + 4 + n].to_vec());
        i += 4 + n;
    }
    let total: usize = samples.iter().map(Vec::len).sum();
    let reps = 5;
    let n = (samples.len() * reps) as f64;
    println!("{} samples, mean {} B", samples.len(), total / samples.len());

    let time = |name: &str, f: &mut dyn FnMut(&[u8]) -> usize| {
        let t = Instant::now();
        let mut out = 0usize;
        for _ in 0..reps {
            for s in &samples {
                out += f(s);
            }
        }
        let us = t.elapsed().as_secs_f64() * 1e6 / n;
        println!("{name:<44} {us:>7.2} µs/record  ratio {:.2}", out as f64 / (total * reps) as f64);
    };

    for level in [3, 1, -1, -5] {
        time(&format!("zstd {level} fresh context (zoe today at 3)"), &mut |s| {
            let mut packed = Vec::with_capacity(s.len());
            let mut c = Compressor::new(level).unwrap();
            c.include_contentsize(true).unwrap();
            c.compress_to_buffer(s, &mut packed).unwrap();
            packed.len()
        });
        let mut c = Compressor::new(level).unwrap();
        c.include_contentsize(true).unwrap();
        time(&format!("zstd {level} reused context"), &mut |s| {
            let mut packed = Vec::with_capacity(s.len());
            c.compress_to_buffer(s, &mut packed).unwrap();
            packed.len()
        });
    }
    let mut c = Compressor::new(3).unwrap();
    c.include_contentsize(true).unwrap();
    let packed: Vec<Vec<u8>> = samples
        .iter()
        .map(|s| {
            let mut p = Vec::with_capacity(s.len());
            c.compress_to_buffer(s, &mut p).unwrap();
            p
        })
        .collect();
    let mut k = 0;
    time("unzstd fresh context (zoe today)", &mut |s| {
        let p = &packed[k % packed.len()];
        k += 1;
        let mut d = Vec::with_capacity(s.len());
        Decompressor::new().unwrap().decompress_to_buffer(p, &mut d).unwrap()
    });
    let mut dctx = Decompressor::new().unwrap();
    let mut k = 0;
    time("unzstd reused context", &mut |s| {
        let p = &packed[k % packed.len()];
        k += 1;
        let mut d = Vec::with_capacity(s.len());
        dctx.decompress_to_buffer(p, &mut d).unwrap()
    });
    time("xxh3 streaming (frame checksum)", &mut |s| {
        let mut h = Xxh3::new();
        h.update(&[1]);
        h.update(&[0; 16]);
        h.update(&[0; 24]);
        h.update(s);
        h.digest() as usize & 1
    });
    time("xxh3 one-shot", &mut |s| xxhash_rust::xxh3::xxh3_64(s) as usize & 1);
}
