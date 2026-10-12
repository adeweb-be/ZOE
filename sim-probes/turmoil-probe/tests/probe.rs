//! Probes of turmoil: the blocking pool, time and select! under a seed.
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Two blocking tasks race; return the order in which they end.
fn blocking_race(seed: u64) -> Vec<u32> {
    let order = Arc::new(Mutex::new(Vec::new()));
    let mut sim = turmoil::Builder::new().rng_seed(seed).build();
    let o = order.clone();
    sim.client("c", async move {
        let mut handles = Vec::new();
        for i in 0..4u32 {
            let o = o.clone();
            handles.push(tokio::task::spawn_blocking(move || {
                // Real work of uneven length, as a core call does.
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
        Ok(())
    });
    sim.run().unwrap();
    let v = order.lock().unwrap().clone();
    v
}

#[test]
fn blocking_pool_order_under_one_seed() {
    let mut seen = std::collections::BTreeSet::new();
    for _ in 0..200 {
        seen.insert(blocking_race(7));
    }
    println!("distinct orders of spawn_blocking under one seed: {}", seen.len());
}

#[test]
fn blocking_task_and_simulated_time() {
    let mut sim = turmoil::Builder::new()
        .rng_seed(1)
        .simulation_duration(Duration::from_secs(3600))
        .build();
    sim.client("c", async move {
        let t0 = tokio::time::Instant::now();
        let w0 = std::time::Instant::now();
        let s0 = std::time::SystemTime::now();
        // A blocking call that waits on a real condvar with a timeout, as
        // the lock wait of the core does.
        let h = tokio::task::spawn_blocking(|| {
            let pair = (std::sync::Mutex::new(false), std::sync::Condvar::new());
            let g = pair.0.lock().unwrap();
            let _ = pair.1.wait_timeout(g, Duration::from_millis(300)).unwrap();
        });
        let sleeper = tokio::time::sleep(Duration::from_secs(60));
        tokio::pin!(sleeper);
        tokio::select! {
            _ = h => println!("blocking ended first: sim {:?} wall {:?}", t0.elapsed(), w0.elapsed()),
            _ = &mut sleeper => println!("sim timer of 60 s ended first: sim {:?} wall {:?}", t0.elapsed(), w0.elapsed()),
        }
        println!("std SystemTime moved {:?} (real time, not simulated)", s0.elapsed().unwrap());
        Ok(())
    });
    sim.run().unwrap();
}

/// select! between two ready branches, many times.
fn select_choices(seed: u64) -> Vec<u8> {
    let out = Arc::new(Mutex::new(Vec::new()));
    let mut sim = turmoil::Builder::new().rng_seed(seed).build();
    let o = out.clone();
    sim.client("c", async move {
        for _ in 0..64 {
            let a = std::future::ready(0u8);
            let b = std::future::ready(1u8);
            let v = tokio::select! { v = a => v, v = b => v };
            o.lock().unwrap().push(v);
        }
        Ok(())
    });
    sim.run().unwrap();
    let v = out.lock().unwrap().clone();
    v
}

#[test]
fn select_under_one_seed() {
    let first = select_choices(3);
    let mut same = 0;
    for _ in 0..20 {
        if select_choices(3) == first {
            same += 1;
        }
    }
    println!("select! runs equal to the first under one seed: {same}/20");
}
