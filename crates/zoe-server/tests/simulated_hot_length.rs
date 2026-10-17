//! The kill loop inside the deterministic simulator, with writers of a hot
//! `Length` and the lock hand-off (ADR 0044).
//!
//! Each seed runs clients whose hot oid holds a `BTrees.Length.Length`. A
//! transaction that writes it stores the value that it read plus 1. So the
//! writers of the hot oid conflict, tier 1 of the server merges them, and a
//! finish hands the lock of the oid to the vote at the head of its queue.
//! A sync takes simulated time, and other votes and finishes run during
//! it, so a vote merges onto a commit that is not durable yet. The server
//! crashes at points that the seed picks, inside `tpc_finish` included,
//! with a power loss.
//!
//! After each run: no acknowledged commit is lost, no client read a record
//! older than one that it knew committed, the store passes the checks of
//! `zoe verify`, and each record of the hot `Length` holds the value of the
//! record before it plus 1, so no merge lost or repeated a change. Over the
//! seeds, the finishes handed off locks.
//!
//! On a network whose round trip is shorter than a sync, the commits of
//! the hot `Length` share the syncs: the mean of `zoe_fsync_batch_size`
//! over 20 seeds is above 2. Without the hand-off, the same runs give about
//! 1.15, because each commit of the hot oid waits for the sync of the one
//! before it.
//!
//! `ZOE_SIM_SEEDS` sets the number of seeds (100 by default). `ZOE_SIM_SEED`
//! runs one seed. A failing seed prints `ZOE_SIM_SEED=<seed>` and
//! reproduces.

use std::time::Duration;

use zoe_server::sim::{Faults, Report, Simulation};

fn seeds() -> Vec<u64> {
    if let Ok(seed) = std::env::var("ZOE_SIM_SEED") {
        return vec![seed.parse().unwrap()];
    }
    let n: u64 = std::env::var("ZOE_SIM_SEEDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100);
    (0..n).collect()
}

/// Check what each run must keep, with or without crashes.
fn check(seed: u64, report: &Report) {
    assert_eq!(report.lost, 0, "ZOE_SIM_SEED={seed}");
    assert_eq!(report.stale_reads, 0, "ZOE_SIM_SEED={seed}");
    assert!(
        report.verify.is_empty(),
        "ZOE_SIM_SEED={seed}: {:?}",
        report.verify
    );
    assert!(
        report.length_faults.is_empty(),
        "ZOE_SIM_SEED={seed}: {} faults, the first: {:?}",
        report.length_faults.len(),
        &report.length_faults[..report.length_faults.len().min(5)]
    );
}

#[test]
fn writers_of_a_hot_length_lose_nothing_across_crashes() {
    let (mut acknowledged, mut handoffs) = (0, 0);
    for seed in seeds() {
        let report = Simulation::new(seed)
            .clients(8)
            .commits(200)
            .crashes(5)
            .hot_length(true)
            .slow_syncs(true)
            .faults(Faults {
                power_loss: true,
                ..Faults::default()
            })
            .run();
        check(seed, &report);
        assert!(report.crashes > 0, "ZOE_SIM_SEED={seed} crashed nothing");
        acknowledged += report.acknowledged;
        handoffs += report.handoffs;
    }
    assert!(acknowledged > 0);
    assert!(handoffs > 0, "no finish handed off a lock");
}

#[test]
fn writers_of_a_hot_length_hand_the_lock_on() {
    for seed in seeds().into_iter().take(20) {
        let report = Simulation::new(seed)
            .clients(8)
            .commits(200)
            .hot_length(true)
            .slow_syncs(true)
            .run();
        check(seed, &report);
        assert!(report.handoffs > 0, "ZOE_SIM_SEED={seed}: no hand-off");
    }
}

#[test]
fn writers_of_a_hot_length_share_the_syncs() {
    let (mut syncs, mut commits) = (0, 0);
    for seed in seeds().into_iter().take(20) {
        let report = Simulation::new(seed)
            .clients(16)
            .commits(400)
            .hot_length(true)
            .slow_syncs(true)
            .faults(Faults {
                // A sync takes 100 µs to 2 ms; a round trip takes less.
                delay: Some(Duration::from_micros(50)),
                loss: 0.0,
                ..Faults::default()
            })
            .run();
        check(seed, &report);
        syncs += report.syncs;
        commits += report.synced_commits;
    }
    let mean = commits as f64 / syncs as f64;
    assert!(mean > 2.0, "{commits} commits in {syncs} syncs");
}
