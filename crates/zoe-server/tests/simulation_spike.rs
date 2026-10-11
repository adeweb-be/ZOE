//! Acceptance test of the spike on deterministic simulation (N18).
//!
//! The spike runs the server and its clients in one process on a simulated
//! network and a simulated clock, with the store on the `MemVfs` of the
//! core. A seed decides the schedule: the order of the tasks, the delays
//! and losses of the network, and the points where the server crashes. A
//! crash drops the process state and reopens the store from a crash image
//! of the `MemVfs`. The same seed gives the same run, event for event, so a
//! failing seed reproduces.
//!
//! The spike code stays on the card branch. The finding,
//! `docs/spikes/deterministic-simulation.md`, compares the ways to do it
//! (for example turmoil, madsim, or an executor of our own), says what the
//! server, the client and the core must route through traits, how the
//! blocking pool runs under the simulation, and what it costs, and
//! recommends one way. This test fixes the shape of the prototype:
//!
//! - `Simulation::new(seed)` with `clients(n)`, `commits(n)`,
//!   `crashes(n)` and `faults(Faults)`, then `run()`, gives a `Report`.
//! - `Report::trace` lists the events of the run in order, as text.
//! - `Report::acknowledged` counts the commits that a client saw
//!   acknowledged, and `Report::lost` the acknowledged commits that a
//!   reopened store does not hold.
//! - `Faults::ack_before_durable` is a deliberate bug for the test: the
//!   server acknowledges `tpc_finish` before the commit is durable.

use std::time::{Duration, Instant};

use zoe_server::sim::{Faults, Simulation};

/// The seeds of the clean runs.
const SEEDS: std::ops::Range<u64> = 0..20;

#[test]
fn a_seed_gives_the_same_run_twice() {
    for seed in SEEDS {
        let first = Simulation::new(seed)
            .clients(3)
            .commits(50)
            .crashes(2)
            .run();
        let second = Simulation::new(seed)
            .clients(3)
            .commits(50)
            .crashes(2)
            .run();
        assert!(!first.trace.is_empty());
        assert_eq!(first.trace, second.trace, "seed {seed} does not reproduce");
        assert_eq!(first.acknowledged, second.acknowledged);
    }
}

#[test]
fn seeds_give_different_schedules() {
    let traces: std::collections::BTreeSet<Vec<String>> = SEEDS
        .map(|seed| Simulation::new(seed).clients(3).commits(50).run().trace)
        .collect();
    assert!(traces.len() > 1, "every seed gave the same schedule");
}

#[test]
fn no_acknowledged_commit_is_lost_through_crashes() {
    for seed in SEEDS {
        let report = Simulation::new(seed)
            .clients(4)
            .commits(100)
            .crashes(5)
            .run();
        assert!(report.acknowledged > 0, "seed {seed} acknowledged nothing");
        assert_eq!(report.lost, 0, "seed {seed} lost acknowledged commits");
    }
}

#[test]
fn simulated_time_does_not_cost_real_time() {
    // A client that holds a lock past the lock timeout makes the others
    // wait for the timeout on the simulated clock, not on the wall clock.
    let started = Instant::now();
    let report = Simulation::new(1)
        .clients(2)
        .commits(10)
        .faults(Faults {
            stall_client_for: Some(Duration::from_secs(60)),
            ..Faults::default()
        })
        .run();
    assert!(report.acknowledged > 0);
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "a run with a simulated stall of 60 s took {:?}",
        started.elapsed()
    );
}

#[test]
fn a_durability_bug_is_caught_and_its_seed_reproduces() {
    let faults = Faults {
        ack_before_durable: true,
        ..Faults::default()
    };
    let failing = (0..200)
        .map(|seed| {
            let report = Simulation::new(seed)
                .clients(3)
                .commits(50)
                .crashes(3)
                .faults(faults.clone())
                .run();
            (seed, report)
        })
        .find(|(_, report)| report.lost > 0);
    let (seed, report) = failing.expect("no seed caught the deliberate bug");
    let again = Simulation::new(seed)
        .clients(3)
        .commits(50)
        .crashes(3)
        .faults(faults)
        .run();
    assert_eq!(again.lost, report.lost);
    assert_eq!(again.trace, report.trace);
}
