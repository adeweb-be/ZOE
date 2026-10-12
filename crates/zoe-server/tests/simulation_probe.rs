//! Measures of the simulator prototype. They print what the runs did, so
//! the finding can quote them. Run with `--nocapture`.

use std::time::{Duration, Instant};

use zoe_server::sim::{Faults, Simulation};

fn count(trace: &[String], needle: &str) -> usize {
    trace.iter().filter(|line| line.contains(needle)).count()
}

fn summary(label: &str, reports: &[zoe_server::sim::Report]) {
    let sum = |f: &dyn Fn(&zoe_server::sim::Report) -> usize| reports.iter().map(f).sum::<usize>();
    println!(
        "{label}: runs {} acked {} lost {} crashes {} trace lines {} lock waits {} conflicts {} lost connections {} late syncs {} sim time {:?}",
        reports.len(),
        sum(&|r| r.acknowledged),
        sum(&|r| r.lost),
        sum(&|r| r.crashes),
        sum(&|r| r.trace.len()),
        sum(&|r| count(&r.trace, "waits for a lock")),
        sum(&|r| count(&r.trace, "ConflictError")),
        sum(&|r| count(&r.trace, "breaks")),
        sum(&|r| count(&r.trace, "syncs late")),
        reports.iter().map(|r| r.elapsed).sum::<Duration>(),
    );
}

#[test]
fn measure_the_runs_of_the_acceptance_test() {
    let started = Instant::now();
    let clean: Vec<_> = (0..20)
        .map(|seed| {
            Simulation::new(seed)
                .clients(3)
                .commits(50)
                .crashes(2)
                .run()
        })
        .collect();
    println!("clean 3x50 crashes 2: wall {:?}", started.elapsed());
    summary("clean 3x50 crashes 2", &clean);

    let started = Instant::now();
    let crashy: Vec<_> = (0..20)
        .map(|seed| {
            Simulation::new(seed)
                .clients(4)
                .commits(100)
                .crashes(5)
                .run()
        })
        .collect();
    println!("4x100 crashes 5: wall {:?}", started.elapsed());
    summary("4x100 crashes 5", &crashy);
    let phases = ["crash in a sync", "crash after a sync"];
    for phase in phases {
        println!(
            "  {phase}: {}",
            crashy.iter().map(|r| count(&r.trace, phase)).sum::<usize>()
        );
    }

    let started = Instant::now();
    let stalled = Simulation::new(1)
        .clients(2)
        .commits(10)
        .faults(Faults {
            stall_client_for: Some(Duration::from_secs(60)),
            ..Faults::default()
        })
        .run();
    println!("stall: wall {:?}", started.elapsed());
    summary("stall 60 s", std::slice::from_ref(&stalled));
    for line in stalled
        .trace
        .iter()
        .filter(|l| {
            l.contains("stalls") || l.contains("waits for a lock") || l.contains("ConflictError")
        })
        .take(8)
    {
        println!("  {line}");
    }

    let started = Instant::now();
    let faults = Faults {
        ack_before_durable: true,
        ..Faults::default()
    };
    let mut caught = Vec::new();
    for seed in 0..200 {
        let report = Simulation::new(seed)
            .clients(3)
            .commits(50)
            .crashes(3)
            .faults(faults.clone())
            .run();
        if report.lost > 0 {
            caught.push((seed, report.lost, report.acknowledged));
        }
    }
    println!(
        "bug: wall {:?} for 200 seeds; caught by {} seeds; first {:?}",
        started.elapsed(),
        caught.len(),
        caught.first()
    );
    let first = Simulation::new(caught[0].0)
        .clients(3)
        .commits(50)
        .crashes(3)
        .faults(faults)
        .run();
    for line in first
        .trace
        .iter()
        .filter(|l| l.contains("server") || l.contains("verify") || l.contains("syncs late"))
        .take(12)
    {
        println!("  {line}");
    }
}

#[test]
fn print_the_stall_trace() {
    let stalled = Simulation::new(1)
        .clients(2)
        .commits(10)
        .faults(Faults {
            stall_client_for: Some(Duration::from_secs(60)),
            ..Faults::default()
        })
        .run();
    for line in &stalled.trace {
        println!("{line}");
    }
}

#[test]
fn print_trace_hashes() {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let mut all = DefaultHasher::new();
    for seed in 0..20 {
        let report = Simulation::new(seed)
            .clients(4)
            .commits(100)
            .crashes(5)
            .run();
        let mut h = DefaultHasher::new();
        report.trace.hash(&mut h);
        report.trace.hash(&mut all);
        println!(
            "seed {seed}: {:016x} lines {}",
            h.finish(),
            report.trace.len()
        );
    }
    println!("all: {:016x}", all.finish());
}

#[test]
fn many_seeds() {
    let n: u64 = std::env::var("ZOE_SIM_SEEDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);
    let started = Instant::now();
    let mut reports = Vec::new();
    let mut lost_seeds = Vec::new();
    for seed in 0..n {
        let report = Simulation::new(seed)
            .clients(4)
            .commits(100)
            .crashes(5)
            .run();
        if report.lost > 0 {
            lost_seeds.push(seed);
        }
        reports.push(report);
    }
    let wall = started.elapsed();
    summary("many seeds 4x100 crashes 5", &reports);
    println!(
        "wall {wall:?} for {n} seeds: {:.1} ms per seed; seeds that lost: {lost_seeds:?}",
        wall.as_secs_f64() * 1000.0 / n as f64
    );
}
