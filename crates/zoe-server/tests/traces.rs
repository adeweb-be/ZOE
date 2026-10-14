//! Acceptance test of the OpenTelemetry traces of `zoe serve` (N17).
//!
//! With `ZOE_OTLP_ENDPOINT` set, the server exports its spans over OTLP to
//! that endpoint, with the protocol that `OTEL_EXPORTER_OTLP_PROTOCOL`
//! names (`grpc`, `http/protobuf` or `http/json`). Without it, the server
//! exports nothing and starts no exporter. Each request is a span named
//! after its method. A wait for a per-OID lock is a span `lock_wait`, the
//! sync of a commit batch a span `fsync`, and each batch of a pack a span
//! `pack_batch`, each inside the span of the request that caused it. Every
//! span has the attribute `zoe.storage`. The server flushes its spans
//! when it stops.
//!
//! The test runs a small OTLP/HTTP collector that takes the JSON encoding.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

mod wire;

use wire::{Server, Value, begin, commit, fresh_dir, load, new_oids, p64, record, s};

/// A collector that keeps the body of each `POST /v1/traces`.
struct Collector {
    addr: SocketAddr,
    bodies: Arc<Mutex<Vec<serde_json::Value>>>,
}

impl Collector {
    fn start() -> Collector {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let kept = bodies.clone();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let kept = kept.clone();
                thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut length = 0usize;
                    let mut path = String::new();
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).unwrap_or(0) == 0 {
                            return;
                        }
                        let line = line.trim_end();
                        if path.is_empty() {
                            path = line.to_string();
                        }
                        if line.is_empty() {
                            break;
                        }
                        if let Some((name, value)) = line.split_once(':')
                            && name.eq_ignore_ascii_case("content-length")
                        {
                            length = value.trim().parse().unwrap();
                        }
                    }
                    let mut body = vec![0u8; length];
                    reader.read_exact(&mut body).unwrap();
                    if path.contains("/v1/traces")
                        && let Ok(json) = serde_json::from_slice(&body)
                    {
                        kept.lock().unwrap().push(json);
                    }
                    let mut stream = stream;
                    let _ = stream.write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}",
                    );
                });
            }
        });
        Collector { addr, bodies }
    }

    /// Every span that the collector received, by name.
    fn spans(&self) -> BTreeMap<String, Vec<serde_json::Value>> {
        let mut spans: BTreeMap<String, Vec<serde_json::Value>> = BTreeMap::new();
        for body in self.bodies.lock().unwrap().iter() {
            for resource in body["resourceSpans"].as_array().into_iter().flatten() {
                for scope in resource["scopeSpans"].as_array().into_iter().flatten() {
                    for span in scope["spans"].as_array().into_iter().flatten() {
                        let name = span["name"].as_str().unwrap_or_default().to_string();
                        spans.entry(name).or_default().push(span.clone());
                    }
                }
            }
        }
        spans
    }
}

fn attribute(span: &serde_json::Value, key: &str) -> Option<String> {
    span["attributes"]
        .as_array()?
        .iter()
        .find(|a| a["key"] == key)
        .and_then(|a| a["value"]["stringValue"].as_str().map(str::to_string))
}

/// The time of the clock as a float, as `pack` takes it.
fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}

#[test]
fn requests_lock_waits_syncs_and_pack_batches_are_spans() {
    let collector = Collector::start();
    let endpoint = format!("http://{}", collector.addr);
    let dir = fresh_dir("spans");
    let mut server = Server::start(
        &["--path", s(&dir.join("store"))],
        &[
            ("ZOE_OTLP_ENDPOINT", &endpoint),
            ("OTEL_EXPORTER_OTLP_PROTOCOL", "http/json"),
        ],
    );
    let mut holder = server.client("1");
    let mut waiter = server.client("1");
    let oid = new_oids(&mut holder, 1)[0];
    let first = commit(&mut holder, 1, &[(oid, 0, record(b"a"))]).unwrap();
    load(&mut holder, oid).unwrap();

    // A vote that waits for a lock.
    begin(&mut holder, 2);
    holder.send_async(
        "storea",
        vec![
            p64(oid),
            p64(first),
            Value::Binary(record(b"b")),
            Value::from(2),
        ],
    );
    holder.call("vote", vec![Value::from(2)]).unwrap();
    begin(&mut waiter, 3);
    waiter.send_async(
        "storea",
        vec![
            p64(oid),
            p64(first),
            Value::Binary(record(b"c")),
            Value::from(3),
        ],
    );
    let vote = waiter.send_call("vote", vec![Value::from(3)]);
    thread::sleep(Duration::from_millis(200));
    holder.call("tpc_finish", vec![Value::from(2)]).unwrap();
    let _ = waiter.reply_to(&vote);
    let _ = waiter.call("tpc_abort", vec![Value::from(3)]);

    // A pack.
    thread::sleep(Duration::from_millis(20));
    holder
        .call("pack", vec![Value::from(now()), Value::from(1)])
        .unwrap();

    drop(holder);
    drop(waiter);
    let (status, _) = server.terminate(Duration::from_secs(30));
    assert_eq!(status.code(), Some(0));

    let spans = collector.spans();
    for name in [
        "loadBefore",
        "vote",
        "tpc_finish",
        "lock_wait",
        "fsync",
        "pack_batch",
    ] {
        let found = spans
            .get(name)
            .unwrap_or_else(|| panic!("no span {name} in {:?}", spans.keys().collect::<Vec<_>>()));
        assert!(
            found
                .iter()
                .all(|span| attribute(span, "zoe.storage").as_deref() == Some("1")),
            "a span {name} has no zoe.storage"
        );
    }
    // The lock wait is inside the span of the vote that waited.
    let votes = &spans["vote"];
    let inside = spans["lock_wait"].iter().any(|wait| {
        votes.iter().any(|vote| {
            vote["traceId"] == wait["traceId"] && vote["spanId"] == wait["parentSpanId"]
        })
    });
    assert!(inside, "no lock_wait span is a child of a vote span");
}

#[test]
fn without_an_endpoint_nothing_is_exported() {
    let collector = Collector::start();
    let dir = fresh_dir("quiet");
    let mut server = Server::start(
        &["--path", s(&dir.join("store"))],
        &[(
            "OTEL_EXPORTER_OTLP_ENDPOINT",
            &format!("http://{}", collector.addr),
        )],
    );
    let mut conn = server.client("1");
    let oid = new_oids(&mut conn, 1)[0];
    commit(&mut conn, 1, &[(oid, 0, record(b"a"))]).unwrap();
    drop(conn);
    server.terminate(Duration::from_secs(30));
    assert!(collector.spans().is_empty());
}
