//! POC ZOE-187 (not for merge): the cost of the terms of one hit.
//!
//! `cargo test --release -p zoe-client --lib poc_cost -- --ignored --nocapture`
//!
//! A cache of 20 MB holds 100 entries of a size. Each term runs over the
//! 100 entries, 200 times, and prints the median time per entry.

use std::hint::black_box;
use std::sync::atomic::Ordering;
use std::time::Instant;

use xxhash_rust::xxh3::{Xxh3Default, xxh3_64};

use super::*;

const ENTRIES: u64 = 100;
const PASSES: usize = 200;

fn median_ns(mut f: impl FnMut()) -> f64 {
    let mut times: Vec<f64> = (0..PASSES)
        .map(|_| {
            let begin = Instant::now();
            f();
            begin.elapsed().as_nanos() as f64 / ENTRIES as f64
        })
        .collect();
    times.sort_by(f64::total_cmp);
    times[PASSES / 2]
}

fn data(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            b'a' + (x % 26) as u8
        })
        .collect()
}

fn terms(len: usize) {
    let cache = Cache::new(20 << 20).unwrap();
    let datas: Vec<Vec<u8>> = (0..ENTRIES).map(|i| data(len, i)).collect();
    for (i, d) in datas.iter().enumerate() {
        assert!(cache.store(Oid(i as u64 + 1), Tid(5), None, d, Tid(0)));
    }
    let before = Tid(10);
    let oids: Vec<Oid> = (1..=ENTRIES).map(Oid).collect();

    let lookup = median_ns(|| {
        for &oid in &oids {
            black_box(cache.load_before_with(oid, before, |_, _| Some(())));
        }
    });
    let copy_hash = median_ns(|| {
        for &oid in &oids {
            black_box(cache.load_before_with(oid, before, |_, copy| {
                copy(&mut |block| {
                    black_box(block);
                })
                .then_some(())
            }));
        }
    });
    let mut out = vec![0u8; len];
    let into_buffer = median_ns(|| {
        for &oid in &oids {
            black_box(cache.load_before_with(oid, before, |_, copy| {
                let mut at = 0;
                copy(&mut |block| {
                    out[at..at + block.len()].copy_from_slice(block);
                    at += block.len();
                })
                .then_some(())
            }));
        }
    });
    let into_vec = median_ns(|| {
        for &oid in &oids {
            black_box(cache.load_before(oid, before));
        }
    });
    // The atomic loads of the words of each entry alone, as `copy` does them.
    let positions: Vec<Head> = oids
        .iter()
        .map(|oid| cache.chain(oid.0).next().unwrap())
        .collect();
    let words_only = median_ns(|| {
        for entry in &positions {
            let at = cache.at(entry.pos) + HEAD_WORDS;
            let words = &cache.map.words()[at..at + (entry.len as usize).div_ceil(8)];
            let mut block = [0u8; BLOCK_WORDS * 8];
            for chunk in words.chunks(BLOCK_WORDS) {
                for (bytes, word) in block.as_chunks_mut::<8>().0.iter_mut().zip(chunk) {
                    *bytes = word.load(Ordering::Relaxed).to_le_bytes();
                }
                black_box(&block);
            }
        }
    });
    let hash_only = median_ns(|| {
        for d in &datas {
            black_box(xxh3_64(d));
        }
    });
    let hash_stream = median_ns(|| {
        for d in &datas {
            let mut h = Xxh3Default::new();
            for block in d.chunks(BLOCK_WORDS * 8) {
                h.update(block);
            }
            black_box(h.digest());
        }
    });
    let memcpy = median_ns(|| {
        for d in &datas {
            out[..d.len()].copy_from_slice(d);
            black_box(&out);
        }
    });
    let alloc_copy = median_ns(|| {
        for d in &datas {
            black_box(d.clone());
        }
    });
    println!(
        "poc_cost {len:>6} B ns/entry: lookup {lookup:.0}  copy+hash {copy_hash:.0}  \
         copy+hash+memcpy(buffer) {into_buffer:.0}  copy+hash+Vec {into_vec:.0}  \
         atomic words only {words_only:.0}  xxh3 one-shot {hash_only:.0}  \
         xxh3 4 KiB stream {hash_stream:.0}  memcpy {memcpy:.0}  alloc+memcpy {alloc_copy:.0}"
    );
}

#[test]
#[ignore = "POC timing, run by hand"]
fn poc_cost() {
    for len in [1024, 16 << 10, 100 << 10] {
        terms(len);
    }
}

/// A memo of checked copies by oid and position, as the binding keeps one.
type Memo = std::sync::Mutex<std::collections::HashMap<u64, (u64, Vec<u8>)>>;

/// Look up `oid` before `before`, and give the copy of `memo` when it holds
/// the entry at the position of the hit. Else copy, check and keep the
/// copy. Return the record and whether the memo served it.
fn memo_load(cache: &Cache, memo: &Memo, oid: Oid, before: Tid) -> Option<(Record, bool)> {
    let mut served = false;
    let found = cache.load_before_at_with(oid, before, |pos, len, copy| {
        if let Some((held, data)) = memo.lock().unwrap().get(&oid.0)
            && *held == pos
        {
            served = true;
            return Some(data.clone());
        }
        let data = to_vec(len, copy)?;
        memo.lock().unwrap().insert(oid.0, (pos, data.clone()));
        Some(data)
    });
    found.map(|(data, tid, end)| (Record { data, tid, end }, served))
}

fn position(cache: &Cache, oid: u64, before: u64) -> Option<u64> {
    cache
        .load_before_at_with(Oid(oid), Tid(before), |pos, _, _| Some(pos))
        .map(|(pos, _, _)| pos)
}

/// A position names one entry: the same entry keeps it, and a new
/// revision, a drop, a clear or a wrap of the ring gives another position
/// or a miss.
#[test]
fn poc_positions_name_one_entry() {
    let cache = Cache::new(4096).unwrap();
    cache.apply(Tid(5), &[]);
    assert!(cache.store(Oid(1), Tid(5), None, &[1; 100], Tid(5)));
    let first = position(&cache, 1, 6).unwrap();
    assert_eq!(position(&cache, 1, 6), Some(first));
    // A commit ends the entry in place: the old snapshot keeps its
    // position and its data, the new snapshot gets a new entry.
    cache.commit(Tid(7), &[(Oid(1), Some(&[2; 100][..]))]);
    assert_eq!(position(&cache, 1, 7), Some(first));
    let second = position(&cache, 1, 8).unwrap();
    assert_ne!(second, first);
    assert_eq!(
        cache.load_before(Oid(1), Tid(7)).unwrap().data,
        vec![1; 100]
    );
    // A drop of the current entry: no hit.
    cache.drop_current(Oid(1));
    assert_eq!(position(&cache, 1, 8), None);
    // A clear: no hit, and a new store takes a new position.
    assert!(cache.store(Oid(2), Tid(7), None, &[3; 100], Tid(7)));
    let third = position(&cache, 2, 8).unwrap();
    cache.clear();
    assert_eq!(position(&cache, 2, 8), None);
    assert!(cache.store(Oid(2), Tid(7), None, &[3; 100], Tid(7)));
    let fourth = position(&cache, 2, 8).unwrap();
    assert!(fourth > third);
    // A wrap of the ring overwrites the entry: no hit at its position.
    for oid in 10..100 {
        cache.store(Oid(oid), Tid(7), None, &[4; 200], Tid(7));
    }
    assert_eq!(position(&cache, 2, 8), None);
}

/// The test `readers_never_get_a_stale_or_torn_hit`, with readers that
/// share a memo: a hit at the position of a kept copy gives the copy and
/// copies nothing. Each hit is checked against the history while a writer
/// applies commits, stores current replies and wraps the ring many times.
#[test]
fn poc_memo_readers_never_get_a_stale_or_torn_hit() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64};
    const OIDS: u64 = 64;
    const COMMITS: u64 = 3000;
    let cache = Arc::new(Cache::new(32 * 1024).unwrap());
    cache.apply(Tid(1), &[]);
    let memo: Arc<Memo> = Arc::default();
    let data = |tid: u64| -> Vec<u8> {
        let len = 8 + (tid % 200) as usize;
        (0..len).map(|i| (tid as usize + i) as u8).collect()
    };
    let revision = move |oid: u64, before: u64| -> u64 {
        (2..before).rev().find(|tid| tid % OIDS == oid).unwrap_or(0)
    };
    let done = Arc::new(AtomicBool::new(false));
    let hits = Arc::new(AtomicU64::new(0));
    let served = Arc::new(AtomicU64::new(0));
    let readers: Vec<_> = (0..4u64)
        .map(|seed| {
            let (cache, memo, done) = (cache.clone(), memo.clone(), done.clone());
            let (hits, served) = (hits.clone(), served.clone());
            std::thread::spawn(move || {
                let mut n = seed;
                while !done.load(Ordering::Relaxed) {
                    n = n.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                    // A few oids, so that the memo serves often.
                    let oid = (n >> 33) % 8;
                    let applied = cache.applied().0;
                    let before = applied + 1 - ((n >> 20) % 50).min(applied - 1);
                    if let Some((found, from_memo)) =
                        memo_load(&cache, &memo, Oid(oid), Tid(before))
                    {
                        let tid = revision(oid, before);
                        assert_eq!(found.tid, Tid(tid), "oid {oid} before {before}");
                        assert_eq!(found.data, data(tid), "oid {oid} memo {from_memo}");
                        if let Some(end) = found.end {
                            assert!(end.0 >= before);
                        }
                        hits.fetch_add(1, Ordering::Relaxed);
                        served.fetch_add(u64::from(from_memo), Ordering::Relaxed);
                    }
                }
            })
        })
        .collect();
    for tid in 2..COMMITS {
        let oid = tid % OIDS;
        let other = (tid * 7) % OIDS;
        let start = revision(other, tid);
        if start != 0 {
            cache.store(Oid(other), Tid(start), None, &data(start), Tid(tid - 1));
        }
        if tid % 3 == 0 {
            cache.apply(Tid(tid), &[Oid(oid)]);
        } else {
            cache.commit(Tid(tid), &[(Oid(oid), Some(data(tid).as_slice()))]);
        }
        if tid % 500 == 0 {
            cache.clear();
        }
    }
    done.store(true, Ordering::Relaxed);
    for reader in readers {
        reader.join().unwrap();
    }
    let (hits, served) = (hits.load(Ordering::Relaxed), served.load(Ordering::Relaxed));
    println!("poc memo: {hits} hits, {served} from the memo");
    assert!(served > 0 && served < hits);
}
