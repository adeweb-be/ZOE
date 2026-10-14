//! Acceptance test: the kill loop runs inside the deterministic simulator
//! (N18, N1, the phase 2 gate "kill loop 0 losses").
//!
//! Each seed runs clients that commit objects and blobs against the server
//! while the server crashes at points that the seed picks, inside
//! `tpc_finish` included. A crash is a power loss: the simulator reopens
//! the store from a crash image of the `MemVfs` that keeps only what was
//! synced, with the unsynced writes reordered or cut, as the crash
//! simulation of the core does. The clients reconnect, verify their cache
//! with `getInvalidations`, and go on.
//!
//! After each run: no acknowledged commit is lost, no blob record lacks
//! its file and no blob file lacks its record, the store passes the checks
//! of `zoe verify`, and no client ever read a record older than one that
//! it knew committed.
//!
//! `ZOE_SIM_SEEDS` sets the number of seeds (200 by default; the nightly
//! job runs 10,000). `ZOE_SIM_SEED` runs one seed. A failing seed prints
//! `ZOE_SIM_SEED=<seed>` and reproduces.

use zoe_server::sim::{Faults, Simulation};

fn seeds() -> Vec<u64> {
    if let Ok(seed) = std::env::var("ZOE_SIM_SEED") {
        return vec![seed.parse().unwrap()];
    }
    let n: u64 = std::env::var("ZOE_SIM_SEEDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(200);
    (0..n).collect()
}

#[test]
fn the_simulated_kill_loop_loses_nothing() {
    let mut acknowledged = 0;
    for seed in seeds() {
        let report = Simulation::new(seed)
            .clients(4)
            .commits(200)
            .blobs(true)
            .crashes(5)
            .faults(Faults {
                power_loss: true,
                ..Faults::default()
            })
            .run();
        acknowledged += report.acknowledged;
        assert_eq!(report.lost, 0, "ZOE_SIM_SEED={seed}");
        assert_eq!(report.stale_reads, 0, "ZOE_SIM_SEED={seed}");
        assert!(
            report.verify.is_empty(),
            "ZOE_SIM_SEED={seed}: {:?}",
            report.verify
        );
        assert!(report.crashes > 0, "ZOE_SIM_SEED={seed} crashed nothing");
    }
    assert!(acknowledged > 0);
}

#[test]
fn crashes_hit_every_phase_of_the_commit() {
    // Over the seeds, the crashes fall before the vote, between the vote
    // and the finish, during the sync of the finish, and after the
    // acknowledgement.
    let mut phases = std::collections::BTreeSet::new();
    for seed in seeds().into_iter().take(200) {
        let report = Simulation::new(seed)
            .clients(4)
            .commits(200)
            .crashes(5)
            .faults(Faults {
                power_loss: true,
                ..Faults::default()
            })
            .run();
        phases.extend(report.crash_phases);
    }
    for phase in ["staged", "locked", "finishing", "acknowledged"] {
        assert!(phases.contains(phase), "no crash in the phase {phase}");
    }
}
