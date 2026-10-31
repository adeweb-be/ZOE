//! Acceptance test of the metrics and the logs of `zoe serve` (N9, N16).
//!
//! `GET /metrics` on the admin port gives the Prometheus text format. Each
//! metric has the `storage` label. Durations are histograms in seconds,
//! counters end in `_total`:
//!
//! - `zoe_loads_total`, `zoe_load_latency_seconds`
//! - `zoe_commits_total`, `zoe_vote_latency_seconds`,
//!   `zoe_finish_latency_seconds`
//! - `zoe_fsync_batch_size`, the number of commits that one sync makes
//!   durable
//! - `zoe_lock_wait_seconds`
//! - `zoe_conflicts_resolved_total{path="native"|"python"}`,
//!   `zoe_conflicts_raised_total`
//! - `zoe_clients_connected`, `zoe_invalidation_queue_depth`
//!
//! With `--log-format json` (`ZOE_LOG_FORMAT`), each log line on stderr is
//! one JSON object. A lock held longer than `--lock-warn` (`ZOE_LOCK_WARN`)
//! is logged with its `oid`, the `client` that held it (its label, from
//! `set_client_label`, or its address) and its `storage`. A log line holds
//! no pickle bytes.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::process::{Command, Stdio};
use std::time::Duration;

mod common;

use common::{
    Conn, MAX, Server, Value, ZOE, begin, commit, fresh_dir, load, new_oids, p64, record, s,
};

fn get(addr: SocketAddr, path: &str) -> String {
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(10)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: zoe\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    response.split_once("\r\n\r\n").unwrap().1.to_string()
}

/// The value of each sample of the text format, by its name and labels.
fn samples(text: &str) -> BTreeMap<String, f64> {
    text.lines()
        .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
        .map(|line| {
            let (name, value) = line.rsplit_once(' ').unwrap();
            (name.to_string(), value.parse().unwrap())
        })
        .collect()
}

fn sample(samples: &BTreeMap<String, f64>, name: &str, labels: &[(&str, &str)]) -> f64 {
    samples
        .iter()
        .filter(|(key, _)| {
            key.split('{').next() == Some(name)
                && labels
                    .iter()
                    .all(|(k, v)| key.contains(&format!("{k}=\"{v}\"")))
        })
        .map(|(_, value)| *value)
        .sum()
}

#[test]
fn every_metric_of_the_specification_is_exported_per_storage() {
    let dir = fresh_dir("names");
    let server = Server::start(
        &[
            "--path",
            s(&dir.join("store")),
            "--admin-listen",
            "127.0.0.1:0",
        ],
        &[],
    );
    let text = get(server.admin.unwrap(), "/metrics");
    for name in [
        "zoe_loads_total",
        "zoe_load_latency_seconds_bucket",
        "zoe_commits_total",
        "zoe_vote_latency_seconds_bucket",
        "zoe_finish_latency_seconds_bucket",
        "zoe_fsync_batch_size_bucket",
        "zoe_lock_wait_seconds_bucket",
        "zoe_conflicts_resolved_total",
        "zoe_conflicts_raised_total",
        "zoe_clients_connected",
        "zoe_invalidation_queue_depth",
    ] {
        let found = text
            .lines()
            .any(|line| line.starts_with(name) && line.contains("storage=\"1\""));
        assert!(found, "no {name} with the storage label in:\n{text}");
    }
}

#[test]
fn the_counters_follow_the_work() {
    let dir = fresh_dir("counters");
    let server = Server::start(
        &[
            "--path",
            s(&dir.join("store")),
            "--admin-listen",
            "127.0.0.1:0",
        ],
        &[],
    );
    let admin = server.admin.unwrap();
    let one = [("storage", "1")];
    let mut a = server.client("1");
    let _b = server.client("1");
    let before = samples(&get(admin, "/metrics"));
    assert_eq!(sample(&before, "zoe_clients_connected", &one), 2.0);

    let oid = new_oids(&mut a, 1)[0];
    let first = commit(&mut a, 1, &[(oid, 0, record(b"1"))]).unwrap();
    commit(&mut a, 2, &[(oid, first, record(b"2"))]).unwrap();
    for _ in 0..5 {
        load(&mut a, oid).unwrap();
    }
    // A stale serial is a conflict: the server raises it, or lists it for
    // the client to resolve.
    let _ = commit(&mut a, 3, &[(oid, first, record(b"3"))]);

    let after = samples(&get(admin, "/metrics"));
    let delta = |name: &str| sample(&after, name, &one) - sample(&before, name, &one);
    assert!(delta("zoe_loads_total") >= 5.0);
    assert!(delta("zoe_commits_total") >= 2.0);
    assert!(delta("zoe_finish_latency_seconds_count") >= 2.0);
    assert!(delta("zoe_vote_latency_seconds_count") >= 2.0);
    assert!(delta("zoe_fsync_batch_size_count") >= 1.0);
    assert!(
        delta("zoe_conflicts_raised_total") + delta("zoe_conflicts_resolved_total") >= 1.0,
        "the stale serial counted no conflict"
    );
}

/// A client that reads `/metrics` right after a reply reads counters that
/// already count its call: the server counts a call before its reply goes
/// out.
#[test]
fn a_reply_comes_after_the_count_of_its_call() {
    let dir = fresh_dir("count-before-reply");
    let server = Server::start(
        &[
            "--path",
            s(&dir.join("store")),
            "--admin-listen",
            "127.0.0.1:0",
        ],
        &[],
    );
    let admin = server.admin.unwrap();
    let one = [("storage", "1")];
    let mut a = server.client("1");
    let before = samples(&get(admin, "/metrics"));
    // The calls that each metric of `names` counted since the start of the
    // test, from one read.
    let counted = |names: [&str; 2]| {
        let metrics = samples(&get(admin, "/metrics"));
        names.map(|name| sample(&metrics, name, &one) - sample(&before, name, &one))
    };
    let oid = new_oids(&mut a, 1)[0];
    let mut serial = 0;
    for n in 1..=20 {
        serial = commit(&mut a, n, &[(oid, serial, record(b"v"))]).unwrap();
        let counts = counted(["zoe_commits_total", "zoe_finish_latency_seconds_count"]);
        assert!(
            counts.iter().all(|&c| c >= n as f64),
            "commit {n}: {counts:?}"
        );
    }
    for n in 1..=200 {
        load(&mut a, oid).unwrap();
        let counts = counted(["zoe_loads_total", "zoe_load_latency_seconds_count"]);
        assert!(
            counts.iter().all(|&c| c >= n as f64),
            "load {n}: {counts:?}"
        );
    }
}

/// Find each value of `key` anywhere in a JSON value.
fn values_of<'a>(value: &'a serde_json::Value, key: &str, out: &mut Vec<&'a serde_json::Value>) {
    match value {
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                if k == key {
                    out.push(v);
                }
                values_of(v, key, out);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                values_of(item, key, out);
            }
        }
        _ => {}
    }
}

#[test]
fn a_long_lock_is_logged_with_its_oid_and_its_client() {
    let dir = fresh_dir("lock-warn");
    let log = dir.join("stderr.log");
    let (mut child, mut stdout) = common::spawn_until_output(
        Command::new(ZOE)
            .args([
                "serve",
                "--path",
                s(&dir.join("store")),
                "--listen",
                "127.0.0.1:0",
                "--log-format",
                "json",
                "--lock-warn",
                "1",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(File::create(&log).unwrap()),
    );
    let addr: SocketAddr = loop {
        let mut line = String::new();
        assert!(
            stdout.read_line(&mut line).unwrap() > 0,
            "the server stopped"
        );
        if let Some(addr) = line.trim().strip_prefix("listening ") {
            break addr.parse().unwrap();
        }
    };

    let mut conn = Conn::open(addr);
    conn.register("1", false).unwrap();
    conn.call("set_client_label", vec![Value::from("slow-writer")])
        .unwrap();
    let oid = new_oids(&mut conn, 1)[0];
    begin(&mut conn, 1);
    conn.send_async(
        "storea",
        vec![
            p64(oid),
            p64(0),
            Value::Binary(record(b"secret-bytes")),
            Value::from(1),
        ],
    );
    conn.call("vote", vec![Value::from(1)]).unwrap();
    std::thread::sleep(Duration::from_millis(1300));
    conn.call("tpc_finish", vec![Value::from(1)]).unwrap();
    let _ = conn.call("loadBefore", vec![p64(oid), Value::Binary(MAX.to_vec())]);
    drop(conn);
    child.kill().unwrap();
    child.wait().unwrap();

    let text = std::fs::read_to_string(&log).unwrap();
    assert!(
        !text.contains("secret-bytes"),
        "a log line holds pickle bytes"
    );
    let events: Vec<serde_json::Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).expect("a log line is not JSON"))
        .collect();
    let logged = events.iter().any(|event| {
        let mut oids = Vec::new();
        let mut clients = Vec::new();
        let mut storages = Vec::new();
        values_of(event, "oid", &mut oids);
        values_of(event, "client", &mut clients);
        values_of(event, "storage", &mut storages);
        oids.iter()
            .any(|v| v.as_u64() == Some(oid) || v.as_str() == Some(&oid.to_string()))
            && clients
                .iter()
                .any(|v| v.as_str().is_some_and(|c| c.contains("slow-writer")))
            && storages.iter().any(|v| v.as_str() == Some("1"))
    });
    assert!(
        logged,
        "no lock-warn event with oid, client and storage:\n{text}"
    );
}
