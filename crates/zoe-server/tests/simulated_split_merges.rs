//! The kill loop inside the deterministic simulator, with split writers of
//! one BTree, the split merge of ADR 0049 and the lock hand-off (ADR 0044).
//!
//! The merge policy of the store sets `split_merges` to `on`. Each client
//! declares `server_writes` and inserts growing int keys into one
//! `IIBTree` whose buckets hold at most 4 keys. Two clients that read one
//! state of the last bucket both split it. The vote of the second merges
//! with rule 3: it writes the bucket and the new bucket Y of the other
//! side, and resolves the node N and its own new bucket X without a
//! record. A sync takes simulated time, so finishes hand their locks off
//! and votes of tier 1 merge onto commits that are not durable yet. A vote
//! of rule 3 never does: it takes the lock of Y only when no commit of Y
//! is in flight, and the plan waits for each commit in flight of its
//! hand-offs. The server
//! crashes at points that the seed picks, inside `tpc_finish` included,
//! with a power loss.
//!
//! After each recovery and after the run: the walk of the tree finds the
//! separators, the order of the keys and the chain of the next buckets
//! correct; the tree holds the key of each acknowledged commit; no bucket
//! with a record is outside the tree, so no record of X exists; and the
//! transaction index of each commit whose vote resolved oids lists the
//! records that the vote wrote, Y included, and neither N nor X when the
//! vote dropped them; each transaction of the store reads, a commit that
//! a crash interrupted and that landed included. The finish of each
//! acknowledged commit whose vote resolved oids handed off no more locks
//! than the records of its commit, and the finish of each vote that wrote
//! Y handed off no lock. After the run: no acknowledged commit is lost, no
//! client read a record older than one that it knew committed, and the
//! store passes the checks of `zoe verify`. Over the seeds, votes wrote Y,
//! dropped oids, finishes handed off locks, and crashes or lost
//! connections came before the reply of the vote of a split, and between
//! a vote that wrote Y and the reply of its finish.
//!
//! The tree has one node: the runs have no split of a node and no root
//! that holds its bucket inline, so the case of Y1 and Y2 is not covered.
//!
//! `ZOE_SIM_SEEDS` sets the number of seeds (50 by default; `just crash`
//! runs its `split_seeds`). `ZOE_SIM_SEED` runs one seed. A failing seed
//! prints `ZOE_SIM_SEED=<seed>` and reproduces.

use zoe_server::sim::{Faults, Report, Simulation};

fn seeds() -> Vec<u64> {
    if let Ok(seed) = std::env::var("ZOE_SIM_SEED") {
        return vec![seed.parse().unwrap()];
    }
    let n: u64 = std::env::var("ZOE_SIM_SEEDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(50);
    (0..n).collect()
}

/// Check what each run must keep.
fn check(seed: u64, report: &Report) {
    assert_eq!(report.lost, 0, "ZOE_SIM_SEED={seed}");
    assert_eq!(report.stale_reads, 0, "ZOE_SIM_SEED={seed}");
    assert!(
        report.verify.is_empty(),
        "ZOE_SIM_SEED={seed}: {:?}",
        report.verify
    );
    assert!(
        report.tree_faults.is_empty(),
        "ZOE_SIM_SEED={seed}: {} faults, the first: {:?}",
        report.tree_faults.len(),
        &report.tree_faults[..report.tree_faults.len().min(5)]
    );
    assert_eq!(
        report.excess_handoffs, 0,
        "ZOE_SIM_SEED={seed}: a finish handed off more locks than its commit wrote"
    );
    // In a tree of one node, the finish of a vote that wrote Y hands off
    // nothing: every other writer of the bucket or of Y takes the lock of
    // the node first, and the vote holds it until its commit is durable.
    // So a hand-off there is of the node or of X, or breaks the premise of
    // section 12 of ADR 0049.
    assert_eq!(
        report.split_handoffs, 0,
        "ZOE_SIM_SEED={seed}: the finish of a vote that wrote Y handed off a lock"
    );
}

#[test]
fn split_writers_lose_nothing_across_crashes() {
    let mut totals = [0usize; 7];
    let (mut handoffs, mut acked_handoffs) = (0, 0);
    for seed in seeds() {
        let report = Simulation::new(seed)
            .clients(8)
            .commits(200)
            .crashes(5)
            .split_merges(true)
            .slow_syncs(true)
            .faults(Faults {
                power_loss: true,
                ..Faults::default()
            })
            .run();
        check(seed, &report);
        assert!(report.crashes > 0, "ZOE_SIM_SEED={seed} crashed nothing");
        assert_eq!(
            report.tree_checks,
            report.crashes + 1,
            "ZOE_SIM_SEED={seed}: a recovery without a check of the tree"
        );
        for (total, count) in totals.iter_mut().zip([
            report.acknowledged,
            report.unstored_writes,
            report.dropped,
            report.interrupted_split_finishes,
            report.interrupted_split_votes,
            report.crashes,
            report.split_handoffs,
        ]) {
            *total += count;
        }
        assert!(
            report.acked_handoffs as u64 <= report.handoffs,
            "ZOE_SIM_SEED={seed}: more hand-offs of acknowledged commits than of the server"
        );
        handoffs += report.handoffs;
        acked_handoffs += report.acked_handoffs;
    }
    let [
        acknowledged,
        unstored,
        dropped,
        finishes,
        votes,
        crashes,
        split,
    ] = totals;
    println!(
        "{acknowledged} acknowledged, {unstored} votes that wrote Y, {dropped} dropped oids, \
         {finishes} finishes after such a vote without a reply, {votes} votes of a split \
         without a reply, {crashes} crashes, {handoffs} hand-offs, {acked_handoffs} of them by \
         acknowledged commits, {split} by the finishes of votes that wrote Y"
    );
    assert!(acknowledged > 0);
    assert!(unstored > 0, "no vote wrote the bucket of the other side");
    assert!(dropped > 0, "no vote dropped an oid");
    assert!(handoffs > 0, "no finish handed off a lock");
    // The hand-offs reach the commits that made them, so a count of 0 for
    // the votes that wrote Y means none.
    assert!(acked_handoffs > 0, "no hand-off reached its commit");
    if seeds().len() >= 20 {
        assert!(finishes > 0, "no crash came after a vote that wrote Y");
        assert!(
            votes > 0,
            "no crash came before the reply of a vote of a split"
        );
    }
}

#[test]
fn the_same_seed_gives_the_same_run_of_split_writers() {
    let run = || {
        Simulation::new(7)
            .clients(8)
            .commits(100)
            .crashes(3)
            .split_merges(true)
            .slow_syncs(true)
            .faults(Faults {
                power_loss: true,
                ..Faults::default()
            })
            .run()
    };
    let (first, second) = (run(), run());
    check(7, &first);
    assert_eq!(first.trace, second.trace);
}
