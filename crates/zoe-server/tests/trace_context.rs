//! Acceptance test of the `trace_context` extension at the wire (N17,
//! F13).
//!
//! A client that declared `trace_context` in `register` may add a fifth
//! element to a call: a map with the key `traceparent`, a W3C traceparent
//! (`00-<trace id>-<parent id>-<flags>`). The span of that call on the
//! server joins the trace: same trace id, and the parent id as its parent.
//! A call without the fifth element starts a trace of its own. A client
//! that did not declare the extension never sends five elements; a
//! five-element call from it is a protocol error that closes the
//! connection, as a stock server does with any message it cannot read.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

mod wire;

use wire::{Conn, MAX, Server, Value, fresh_dir, p64, s};

const TRACE: &str = "4bf92f3577b34da6a3ce929d0e0e4736";
const PARENT: &str = "00f067aa0ba902b7";

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

/// Send a call with the fifth element.
fn send_traced(conn: &mut Conn, id: i64, method: &str, args: Vec<Value>) {
    let message = Value::Array(vec![
        Value::from(id),
        Value::Boolean(false),
        Value::from(method),
        Value::Array(args),
        Value::Map(vec![(
            Value::from("traceparent"),
            Value::from(format!("00-{TRACE}-{PARENT}-01")),
        )]),
    ]);
    let mut bytes = Vec::new();
    rmpv::encode::write_value(&mut bytes, &message).unwrap();
    conn.write_frame(&bytes).unwrap();
}

fn register_traced(conn: &mut Conn) {
    conn.call(
        "register",
        vec![
            Value::from("1"),
            Value::Boolean(false),
            Value::Array(vec![Value::from("trace_context")]),
        ],
    )
    .unwrap();
    let info = conn.call("get_info", vec![]).unwrap();
    let extensions = info
        .as_map()
        .unwrap()
        .iter()
        .find(|(k, _)| k.as_str() == Some("extensions"))
        .map(|(_, v)| v.clone())
        .unwrap();
    assert_eq!(extensions, Value::Array(vec![Value::from("trace_context")]));
}

#[test]
fn a_call_with_a_traceparent_joins_the_trace_of_the_client() {
    let collector = Collector::start();
    let endpoint = format!("http://{}", collector.addr);
    let dir = fresh_dir("joins");
    let mut server = Server::start(
        &["--path", s(&dir.join("store"))],
        &[
            ("ZOE_OTLP_ENDPOINT", &endpoint),
            ("OTEL_EXPORTER_OTLP_PROTOCOL", "http/json"),
        ],
    );
    let mut conn = server.connect();
    register_traced(&mut conn);
    send_traced(
        &mut conn,
        100,
        "loadBefore",
        vec![p64(0), Value::Binary(MAX.to_vec())],
    );
    // The object does not exist: the reply is an error, and the span is
    // there all the same.
    let _ = conn.reply_to(&Value::from(100));
    // A call without the fifth element starts its own trace.
    let _ = conn.call("loadBefore", vec![p64(1), Value::Binary(MAX.to_vec())]);
    drop(conn);
    server.terminate(Duration::from_secs(30));

    let spans = collector.spans();
    let loads = spans.get("loadBefore").expect("no loadBefore span");
    let joined: Vec<_> = loads
        .iter()
        .filter(|span| span["traceId"].as_str().map(str::to_lowercase).as_deref() == Some(TRACE))
        .collect();
    assert_eq!(joined.len(), 1, "{loads:?}");
    assert_eq!(
        joined[0]["parentSpanId"]
            .as_str()
            .map(str::to_lowercase)
            .as_deref(),
        Some(PARENT)
    );
    assert_eq!(loads.len(), 2);
}

#[test]
fn a_client_that_did_not_declare_it_cannot_send_it() {
    let dir = fresh_dir("undeclared");
    let server = Server::path(&dir.join("store"));
    let mut conn = server.connect();
    conn.register("1", false).unwrap();
    send_traced(
        &mut conn,
        7,
        "loadBefore",
        vec![p64(0), Value::Binary(MAX.to_vec())],
    );
    assert!(conn.closed_within(Duration::from_secs(10)));
    // The server goes on serving the others.
    let mut other = server.connect();
    other.register("1", false).unwrap();
}
