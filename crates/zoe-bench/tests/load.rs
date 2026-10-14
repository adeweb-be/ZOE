//! Acceptance test of `zoe-bench load` and `zoe-bench saturate`, the load
//! generator that speaks the ZEO protocol (harness B2, N3).
//!
//! `zoe-bench load --server HOST:PORT --storage NAME` runs `--clients N`
//! clients, each with `--pipeline D` calls in flight, on a store of
//! `--objects K` objects that it creates first. A client runs
//! `--transactions T` transactions: each reads `--txn-size S` objects and,
//! with the probability `--write-ratio W`, writes them back. The objects
//! come from a Zipf law of exponent `--zipf Z` over the objects, so a hot
//! set forms. A transaction that fails with a conflict is retried, and the
//! retry counts. The run writes one JSON document to stdout.
//!
//! `zoe-bench saturate` raises the number of clients step by step until the
//! p99 of `loadBefore` passes `--p99-ceiling SECONDS`, and reports the
//! loads per second of the last step under the ceiling.
//!
//! The results describe the machine, as the zodbshootout results do, and
//! hold no host name, user name or path. The corpus is synthetic.

use std::process::Command;

use zoe_server::serve::{ServeOptions, Server};

const BENCH: &str = env!("CARGO_BIN_EXE_zoe-bench");

fn fresh_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("zoe-bench-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn run(args: &[&str]) -> serde_json::Value {
    let output = Command::new(BENCH).args(args).output().unwrap();
    assert!(
        output.status.success(),
        "zoe-bench {args:?}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("the report is not JSON")
}

fn number(report: &serde_json::Value, key: &str) -> f64 {
    report[key]
        .as_f64()
        .unwrap_or_else(|| panic!("no number {key} in {report}"))
}

#[test]
fn a_load_run_reports_throughput_latency_and_contention() {
    let dir = fresh_dir("load");
    let server = Server::start(ServeOptions::new(dir.join("store"))).unwrap();
    let addr = server.addr().to_string();
    let report = run(&[
        "load",
        "--server",
        &addr,
        "--storage",
        "1",
        "--clients",
        "8",
        "--pipeline",
        "4",
        "--objects",
        "200",
        "--transactions",
        "50",
        "--txn-size",
        "5",
        "--write-ratio",
        "0.5",
        "--zipf",
        "1.2",
    ]);
    server.stop();

    // The settings of the run.
    assert_eq!(report["settings"]["clients"], 8);
    assert_eq!(report["settings"]["pipeline"], 4);
    // Every transaction of every client ends committed or read-only.
    assert_eq!(number(&report, "transactions"), 400.0);
    assert!(number(&report, "commits") > 0.0);
    assert!(number(&report, "loads_per_second") > 0.0);
    for key in [
        "load_p50_seconds",
        "load_p99_seconds",
        "lock_wait_p99_seconds",
    ] {
        assert!(number(&report, key) >= 0.0, "{key}");
    }
    assert!(number(&report, "load_p99_seconds") >= number(&report, "load_p50_seconds"));
    // A hot set with writes meets conflicts, and each one is retried.
    let conflicts = number(&report, "conflicts_per_1000_commits");
    assert!(conflicts >= 0.0);
    assert!(number(&report, "retries") >= 0.0);
    // The machine is described; nothing names the host or a path.
    for key in ["cpu", "logical_cpus", "memory_bytes", "kernel"] {
        assert!(!report["machine"][key].is_null(), "no machine.{key}");
    }
    let text = report.to_string();
    assert!(
        !text.contains(dir.to_str().unwrap()),
        "the report names a path"
    );
}

#[test]
fn a_saturation_run_stops_at_the_ceiling() {
    let dir = fresh_dir("saturate");
    let server = Server::start(ServeOptions::new(dir.join("store"))).unwrap();
    let addr = server.addr().to_string();
    let report = run(&[
        "saturate",
        "--server",
        &addr,
        "--storage",
        "1",
        "--objects",
        "200",
        "--transactions",
        "20",
        "--p99-ceiling",
        "0.05",
        "--max-clients",
        "16",
    ]);
    server.stop();
    let steps = report["steps"].as_array().expect("no steps");
    assert!(!steps.is_empty());
    for pair in steps.windows(2) {
        assert!(pair[1]["clients"].as_u64() > pair[0]["clients"].as_u64());
    }
    let best = number(&report, "loads_per_second_under_ceiling");
    assert!(best > 0.0);
    assert!(
        steps
            .iter()
            .any(|step| step["load_p99_seconds"].as_f64().unwrap() <= 0.05)
    );
}
