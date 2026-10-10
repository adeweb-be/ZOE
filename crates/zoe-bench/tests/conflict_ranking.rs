//! Acceptance test of the ConflictError ranking tool.
//!
//! Rows are sorted by count, highest first, then by class or oid in
//! ascending text order.

use std::process::Command;

fn data(name: &str) -> String {
    format!("{}/tests/data/{name}", env!("CARGO_MANIFEST_DIR"))
}

fn rank(paths: &[String]) -> serde_json::Value {
    let output = Command::new(env!("CARGO_BIN_EXE_zoe-bench"))
        .arg("conflicts")
        .args(paths)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn ranks_the_synthetic_sample() {
    let expected: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(data("conflicts-expected.json")).unwrap())
            .unwrap();
    assert_eq!(rank(&[data("conflicts-sample.log")]), expected);
}

#[test]
fn several_logs_add_up() {
    let sample = data("conflicts-sample.log");
    let ranked = rank(&[sample.clone(), sample]);
    assert_eq!(ranked["total"], 18);
    assert_eq!(ranked["by_class"][0]["count"], 8);
}

#[test]
fn a_missing_path_fails_with_a_message() {
    let output = Command::new(env!("CARGO_BIN_EXE_zoe-bench"))
        .args(["conflicts", "/nonexistent/zope.log"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!output.stderr.is_empty());
}
