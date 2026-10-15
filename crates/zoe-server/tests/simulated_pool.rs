//! Acceptance test: the order of the invalidations of a pool of
//! connections, under the deterministic simulator (F28, N2, N18).
//!
//! The native client runs several transactions at once through a pool of
//! connections. One connection of the pool, the listener, reads the
//! invalidations and does not commit. Its stream is the order of the
//! client. The client delivers each tid of that stream in turn: a tid of
//! another client as an invalidation, a tid of the pool through the
//! callback of its finish. This test runs that rule on the code of the
//! client and of the server, under the faults of the simulator.
//!
//! - `Simulation::pool(n)`: each client runs a pool of at most `n`
//!   connections that commit, and a listener, and runs `n` transactions at
//!   once. Each client records what its pool delivers, in order.
//! - `Faults::listener_losses`: the network closes the listener of a pool
//!   this many times in a run. Each time, a finish of that pool has the
//!   reply of the server and waits for its tid on the listener.
//! - `Report::order_faults` lists each break of the rule, as text:
//!   - a pool delivers a tid twice, or after a higher tid;
//!   - a pool skips a tid that the store committed after the `register`
//!     of its listener, up to the last tid that it delivered;
//!   - a commit of the pool reaches its consumer as an invalidation;
//!   - a finish of tid T returns before the pool delivered every tid below
//!     T, or returns T without its callback;
//!   - the last transaction of the pool is above the last tid that it
//!     delivered;
//!   - after a lost listener, a finish that had its reply fails, or runs
//!     its callback before the pool told its consumer that its state is
//!     stale;
//!   - at the end of the run, a pool that keeps its listener did not
//!     deliver every tid of the store. A finish that failed or lost its
//!     connection, and did not leave the hold, stops the delivery.
//! - `Report::foreign_deliveries` counts the tids of other clients that
//!   the pools delivered, and `Report::own_deliveries` the commits of a
//!   pool that it delivered through the callback of their finish.
//! - `Report::held` counts the tids that a listener held while a finish
//!   that was sent before the tid waited for its reply.
//! - `Report::waited_finishes` counts the finishes whose reply came before
//!   their pool delivered every tid below theirs.
//! - `Report::lost_listener_finishes` counts the finishes that had their
//!   reply when their pool lost its listener, and returned their tid.
//!
//! `ZOE_SIM_SEEDS` sets the number of seeds (100 by default). `ZOE_SIM_SEED`
//! runs one seed. A failing run prints `ZOE_SIM_SEED=<seed>`.

use zoe_server::sim::{Faults, Report, Simulation};

/// The sizes of the pool that each seed runs. With 1, one connection
/// commits and the listener delivers.
const POOLS: [usize; 2] = [1, 4];

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

fn check(report: &Report, seed: u64, pool: usize) {
    assert!(
        report.order_faults.is_empty(),
        "ZOE_SIM_SEED={seed}, pool {pool}: {:?}",
        report.order_faults
    );
    assert_eq!(report.lost, 0, "ZOE_SIM_SEED={seed}, pool {pool}");
    assert_eq!(report.stale_reads, 0, "ZOE_SIM_SEED={seed}, pool {pool}");
    assert!(report.acknowledged > 0, "ZOE_SIM_SEED={seed}, pool {pool}");
}

#[test]
fn a_seed_gives_the_same_run_twice_with_pools() {
    for seed in seeds().into_iter().take(10) {
        let run = || {
            Simulation::new(seed)
                .clients(3)
                .pool(4)
                .commits(60)
                .faults(Faults {
                    closes: 2,
                    listener_losses: 1,
                    ..Faults::default()
                })
                .run()
        };
        let (first, second) = (run(), run());
        assert_eq!(first.trace, second.trace, "ZOE_SIM_SEED={seed}");
    }
}

#[test]
fn pools_deliver_each_tid_once_in_tid_order() {
    let (mut foreign, mut own, mut held, mut waited) = (0, 0, 0, 0);
    for seed in seeds() {
        for pool in POOLS {
            let report = Simulation::new(seed)
                .clients(3)
                .pool(pool)
                .commits(150)
                .faults(Faults {
                    closes: 2,
                    ..Faults::default()
                })
                .run();
            check(&report, seed, pool);
            foreign += report.foreign_deliveries;
            own += report.own_deliveries;
            held += report.held;
            waited += report.waited_finishes;
        }
    }
    // The runs reach each path of the rule: tids of other clients, commits
    // of the pool, a tid that the listener held, and a finish that waited
    // for an earlier tid.
    assert!(foreign > 0, "no tid of another client reached a pool");
    assert!(own > 0, "no commit of a pool reached its callback");
    assert!(held > 0, "no listener held a tid");
    assert!(waited > 0, "no finish waited for an earlier tid");
}

#[test]
fn a_finish_with_its_reply_returns_its_tid_when_the_listener_is_lost() {
    let mut finishes = 0;
    for seed in seeds() {
        let report = Simulation::new(seed)
            .clients(3)
            .pool(4)
            .commits(100)
            .faults(Faults {
                loss: 0.0,
                listener_losses: 2,
                ..Faults::default()
            })
            .run();
        check(&report, seed, 4);
        finishes += report.lost_listener_finishes;
    }
    assert!(
        finishes > 0,
        "no finish had its reply when its listener was lost"
    );
}
