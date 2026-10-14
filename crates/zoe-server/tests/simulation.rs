//! Acceptance test of the deterministic simulator (N18).
//!
//! The server and `zoe-client` run their own code on a simulated network
//! and a simulated clock, with the store on the `MemVfs` of the core, in
//! the way that the spike on deterministic simulation recommends. A seed
//! decides the whole schedule. The simulator injects faults on the
//! network: lost and late messages, partitions between a client and the
//! server, and connections that close. Clients reconnect, as
//! `ZEO.ClientStorage` does.
//!
//! - `Simulation::new(seed)` with `clients(n)`, `commits(n)`, `crashes(n)`
//!   and `faults(Faults)`, then `run()`, gives a `Report`.
//! - `Report::trace` lists the events in order, as text: each message on
//!   the network with its method, each fault, each crash.
//! - `Report::acknowledged` and `Report::lost` count the commits that a
//!   client saw acknowledged and those that the store lost.
//! - `Report::stale_reads` counts loads that gave a record older than one
//!   that the client knew to be committed before the load.
//!
//! A failing run prints `ZOE_SIM_SEED=<seed>`, and `ZOE_SIM_SEED` runs that
//! seed alone. If the spike recommends another shape, the planner updates
//! this skeleton before the card moves to Ready.

use std::time::{Duration, Instant};

use zoe_server::sim::{Faults, Simulation};

fn seeds() -> Vec<u64> {
    match std::env::var("ZOE_SIM_SEED") {
        Ok(seed) => vec![seed.parse().unwrap()],
        Err(_) => (0..50).collect(),
    }
}

fn faulty() -> Faults {
    Faults {
        loss: 0.01,
        delay: Some(Duration::from_millis(50)),
        partitions: 2,
        closes: 2,
        ..Faults::default()
    }
}

#[test]
fn a_seed_gives_the_same_run_twice() {
    for seed in seeds() {
        let run = || {
            Simulation::new(seed)
                .clients(3)
                .commits(50)
                .faults(faulty())
                .run()
        };
        let (first, second) = (run(), run());
        assert_eq!(first.trace, second.trace, "ZOE_SIM_SEED={seed}");
    }
}

#[test]
fn the_runs_use_the_protocol_of_the_server_and_the_client() {
    let report = Simulation::new(1).clients(2).commits(20).run();
    let text = report.trace.join("\n");
    for method in [
        "register",
        "loadBefore",
        "tpc_begin",
        "vote",
        "tpc_finish",
        "invalidateTransaction",
    ] {
        assert!(text.contains(method), "no {method} in the trace");
    }
}

#[test]
fn network_faults_lose_nothing_and_show_nothing_stale() {
    for seed in seeds() {
        let report = Simulation::new(seed)
            .clients(4)
            .commits(100)
            .faults(faulty())
            .run();
        assert!(report.acknowledged > 0, "ZOE_SIM_SEED={seed}");
        assert_eq!(report.lost, 0, "ZOE_SIM_SEED={seed}");
        assert_eq!(report.stale_reads, 0, "ZOE_SIM_SEED={seed}");
    }
}

#[test]
fn simulated_time_does_not_cost_real_time() {
    let started = Instant::now();
    let report = Simulation::new(3)
        .clients(2)
        .commits(10)
        .faults(Faults {
            stall_client_for: Some(Duration::from_secs(120)),
            ..Faults::default()
        })
        .run();
    assert!(report.acknowledged > 0);
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[test]
fn a_durability_bug_is_caught_and_its_seed_reproduces() {
    let faults = Faults {
        ack_before_durable: true,
        ..Faults::default()
    };
    let run = |seed| {
        Simulation::new(seed)
            .clients(3)
            .commits(50)
            .crashes(3)
            .faults(faults.clone())
            .run()
    };
    let (seed, report) = (0..200)
        .map(|seed| (seed, run(seed)))
        .find(|(_, report)| report.lost > 0)
        .expect("no seed caught the deliberate bug");
    let again = run(seed);
    assert_eq!((again.lost, again.trace), (report.lost, report.trace));
}
