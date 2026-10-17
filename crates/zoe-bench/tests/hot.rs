//! `zoe-bench hot`: writers of one hot object, with vote-time locks and
//! with early write-intent locks (F7, N5, ADR 0044 and ADR 0045).

use std::process::Command;
use std::time::Duration;

use zoe_server::serve::{ServeOptions, Server};

const BENCH: &str = env!("CARGO_BIN_EXE_zoe-bench");

fn fresh_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("zoe-bench-hot-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Run `zoe-bench hot` with `args` against a new server, and return its
/// report.
fn hot(name: &str, args: &[&str]) -> serde_json::Value {
    let dir = fresh_dir(name);
    let options = ServeOptions::new(dir.join("store"))
        .admin_listen("127.0.0.1:0".parse().unwrap())
        .lock_timeout(Duration::from_secs(60));
    let server = Server::start(options).unwrap();
    let addr = server.addr().to_string();
    let admin = server.admin_addr().unwrap().to_string();
    let output = Command::new(BENCH)
        .args(["hot", "--server", &addr, "--admin", &admin])
        .args(["--writers", "8", "--transactions", "10"])
        .args(args)
        .output()
        .unwrap();
    server.stop();
    assert!(
        output.status.success(),
        "zoe-bench hot {args:?}\n{}",
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
fn writers_of_an_object_that_cannot_merge_conflict_without_early_locks() {
    let report = hot("mapping", &["--object", "mapping", "--attempts", "100"]);
    assert_eq!(
        number(&report, "commits") + number(&report, "gave_up"),
        80.0
    );
    // No update is lost: the object holds each commit.
    assert_eq!(number(&report, "value"), number(&report, "commits"));
    assert!(number(&report, "conflicts") > 0.0, "{report}");
    assert_eq!(number(&report, "intent_locks"), 0.0);
}

#[test]
fn writers_of_an_object_that_cannot_merge_wait_with_early_locks() {
    let report = hot("mapping-early", &["--object", "mapping", "--early-locks"]);
    assert_eq!(number(&report, "commits"), 80.0);
    assert_eq!(number(&report, "gave_up"), 0.0);
    assert_eq!(number(&report, "value"), 80.0);
    assert_eq!(number(&report, "conflicts"), 0.0, "{report}");
    assert_eq!(number(&report, "intent_locks"), 80.0);
}

#[test]
fn writers_of_a_length_merge_at_the_vote() {
    let report = hot("length", &["--object", "length"]);
    assert_eq!(number(&report, "commits"), 80.0);
    assert_eq!(number(&report, "value"), 80.0);
    assert_eq!(number(&report, "conflicts"), 0.0, "{report}");
    assert_eq!(report["settings"]["object"], "length");
    assert!(number(&report, "commits_per_sync") >= 1.0);
}
