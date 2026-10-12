//! Probes of madsim: the blocking pool, std time and threads under a seed.
#![cfg(madsim)]
use std::sync::{Arc, Mutex};
use std::time::Duration;

use madsim::runtime::Runtime;

fn blocking_race(seed: u64) -> Vec<u32> {
    let rt = Runtime::with_seed_and_config(seed, madsim::Config::default());
    rt.block_on(async move {
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut handles = Vec::new();
        for i in 0..4u32 {
            let o = order.clone();
            handles.push(tokio::task::spawn_blocking(move || {
                let mut x = 0u64;
                for k in 0..(200_000 + (i as u64 * 7_919) % 50_000) {
                    x = x.wrapping_mul(31).wrapping_add(k);
                }
                std::hint::black_box(x);
                o.lock().unwrap().push(i);
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        let v = order.lock().unwrap().clone();
        v
    })
}

#[test]
fn blocking_pool_order_under_one_seed() {
    let mut seen = std::collections::BTreeSet::new();
    for _ in 0..50 {
        seen.insert(blocking_race(7));
    }
    println!("distinct orders of spawn_blocking under one seed: {} {:?}", seen.len(), seen);
}

#[test]
fn std_time_in_the_simulation() {
    let rt = Runtime::with_seed_and_config(1, madsim::Config::default());
    rt.block_on(async {
        let w0 = std::time::Instant::now();
        let s0 = std::time::SystemTime::now();
        tokio::time::sleep(Duration::from_secs(60)).await;
        println!("after a simulated sleep of 60 s: std Instant moved {:?}, SystemTime moved {:?}", w0.elapsed(), s0.elapsed().unwrap());
        let real = std::time::Instant::now();
        // A real condvar wait with a timeout, as the lock wait of the core.
        let h = tokio::task::spawn_blocking(|| {
            let pair = (std::sync::Mutex::new(false), std::sync::Condvar::new());
            let g = pair.0.lock().unwrap();
            let t = std::time::Instant::now();
            let _ = pair.1.wait_timeout(g, Duration::from_millis(300)).unwrap();
            t.elapsed()
        });
        let inside = h.await.unwrap();
        println!("condvar wait_timeout of 300 ms in spawn_blocking: std Instant inside says {:?}, outside {:?}", inside, real.elapsed());
    });
}

#[test]
fn std_thread_spawn_in_the_simulation() {
    let r = std::panic::catch_unwind(|| {
        let rt = Runtime::with_seed_and_config(1, madsim::Config::default());
        rt.block_on(async {
            let t = std::thread::spawn(|| 1 + 1);
            t.join().unwrap()
        })
    });
    println!("std::thread::spawn inside the simulation: {:?}", r.map_err(|e| e.downcast_ref::<String>().cloned().or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))));
}

#[test]
fn check_determinism_with_select() {
    Runtime::check_determinism(5, madsim::Config::default(), || async {
        for _ in 0..64 {
            let a = std::future::ready(0u8);
            let b = std::future::ready(1u8);
            let _ = tokio::select! { v = a => v, v = b => v };
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    });
    println!("check_determinism with select!: passed");
}
