//! Acceptance test of `zoe mcp` against a running server, of the `tail`
//! resource, and of the streamable HTTP transport (F20).
//!
//! `zoe mcp --server HOST:PORT --storage NAME` serves the same read-only
//! tools as on a folder, through `zoe-client`, registered read-only.
//! There it has the resource `zoe://NAME/tail`: the last commits of the
//! store, each with its tid, user, description and oids, as JSON. A client
//! that subscribes to it gets `notifications/resources/updated` after each
//! commit, from the invalidations of the server.
//!
//! `--http HOST:PORT` serves the streamable HTTP transport of MCP at
//! `/mcp` in place of stdio. The command writes `listening <address>` to
//! stdout when it accepts connections.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use zoe_core::{Oid, Tid};

mod wire;

use wire::{Server, ZOE, commit, fresh_dir, new_oids, s};

/// A `PersistentMapping` with a title and references to `refs`.
fn mapping(title: &str, refs: &[u64]) -> Vec<u8> {
    let mut pickle = b"\x80\x03cpersistent.mapping\nPersistentMapping\nq\x00.".to_vec();
    pickle.extend_from_slice(b"\x80\x03}(X\x05\x00\x00\x00titleX");
    pickle.extend_from_slice(&(title.len() as u32).to_le_bytes());
    pickle.extend_from_slice(title.as_bytes());
    pickle.extend_from_slice(b"X\x04\x00\x00\x00refs]");
    for oid in refs {
        pickle.extend_from_slice(b"(C\x08");
        pickle.extend_from_slice(&oid.to_be_bytes());
        pickle.extend_from_slice(b"cpersistent.mapping\nPersistentMapping\ntQa");
    }
    pickle.extend_from_slice(b"u.");
    pickle
}

/// A store with a root that refers to one child.
fn store(dir: &std::path::Path) {
    let store = wire::open_store(dir);
    let mut txn = store.tpc_begin(b"admin", b"create", b"").unwrap();
    txn.store(Oid(0), Tid(0), &mapping("root", &[1])).unwrap();
    txn.store(Oid(1), Tid(0), &mapping("first", &[])).unwrap();
    txn.vote().unwrap();
    txn.finish().unwrap();
}

/// A client of `zoe mcp` over its stdio.
struct Session {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Session {
    fn start(args: &[&str]) -> Session {
        let mut child = {
            let _spawn = wire::spawn_lock();
            Command::new(ZOE)
                .arg("mcp")
                .args(args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap()
        };
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut session = Session {
            child,
            stdin,
            stdout,
            next_id: 0,
        };
        let init = session.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "acceptance", "version": "0"}
            }),
        );
        assert!(
            init["result"]["capabilities"]["tools"].is_object(),
            "{init}"
        );
        session.notify("notifications/initialized", json!({}));
        session
    }

    fn send(&mut self, message: Value) {
        writeln!(self.stdin, "{message}").unwrap();
        self.stdin.flush().unwrap();
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.send(json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        loop {
            let mut line = String::new();
            assert!(
                self.stdout.read_line(&mut line).unwrap() > 0,
                "zoe mcp stopped"
            );
            let message: Value = serde_json::from_str(&line).unwrap();
            if message["id"] == id {
                return message;
            }
        }
    }

    /// Call a tool and return its result as JSON: the structured content,
    /// or the JSON of its first text.
    fn call(&mut self, tool: &str, arguments: Value) -> (bool, Value) {
        let reply = self.request("tools/call", json!({"name": tool, "arguments": arguments}));
        let result = &reply["result"];
        assert!(result.is_object(), "{reply}");
        let failed = result["isError"].as_bool().unwrap_or(false);
        let value = if result["structuredContent"].is_object() {
            result["structuredContent"].clone()
        } else {
            let text = result["content"][0]["text"].as_str().unwrap_or_default();
            serde_json::from_str(text).unwrap_or(Value::String(text.to_string()))
        };
        (failed, value)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Session {
    /// Read messages until a notification named `method` comes, or until
    /// `timeout`.
    fn wait_notification(&mut self, method: &str, timeout: Duration) -> Option<Value> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let mut line = String::new();
            if self.stdout.read_line(&mut line).unwrap() == 0 {
                return None;
            }
            let message: Value = serde_json::from_str(&line).unwrap();
            if message["method"] == method {
                return Some(message);
            }
        }
        None
    }
}

#[test]
fn the_tools_work_against_a_server() {
    let dir = fresh_dir("mcp-server");
    store(&dir);
    let server = Server::path(&dir);
    let addr = server.addr.to_string();
    let mut session = Session::start(&["--server", &addr, "--storage", "1"]);
    let (failed, root) = session.call("object_get", json!({"oid": "0x00"}));
    assert!(!failed, "{root}");
    assert_eq!(root["class"], "persistent.mapping.PersistentMapping");
}

#[test]
fn the_tail_resource_notifies_each_commit() {
    let dir = fresh_dir("mcp-tail");
    store(&dir);
    let server = Server::path(&dir);
    let addr = server.addr.to_string();
    let mut session = Session::start(&["--server", &addr, "--storage", "1"]);

    let resources = session.request("resources/list", json!({}));
    let uris: Vec<String> = resources["result"]["resources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["uri"].as_str().unwrap().to_string())
        .collect();
    assert!(uris.contains(&"zoe://1/tail".to_string()), "{uris:?}");
    session.request("resources/subscribe", json!({"uri": "zoe://1/tail"}));

    let mut writer = server.client("1");
    let oid = new_oids(&mut writer, 1)[0];
    let tid = commit(&mut writer, 1, &[(oid, 0, wire::record(b"new"))]).unwrap();

    let updated = session
        .wait_notification("notifications/resources/updated", Duration::from_secs(30))
        .expect("no notification after a commit");
    assert_eq!(updated["params"]["uri"], "zoe://1/tail");
    let read = session.request("resources/read", json!({"uri": "zoe://1/tail"}));
    let text = read["result"]["contents"][0]["text"].as_str().unwrap();
    assert!(text.contains(&format!("0x{tid:016x}")), "{text}");
    assert!(text.contains("description"), "{text}");
}

/// POST one JSON-RPC message to `/mcp`. Return the reply as JSON and the
/// session id of the server.
fn post(addr: SocketAddr, session: Option<&str>, message: &Value) -> (Value, Option<String>) {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    let body = message.to_string();
    let mut request = format!(
        "POST /mcp HTTP/1.1\r\nHost: zoe\r\nContent-Type: application/json\r\n\
         Accept: application/json, text/event-stream\r\nContent-Length: {}\r\n\
         Connection: close\r\n",
        body.len()
    );
    if let Some(id) = session {
        request.push_str(&format!("Mcp-Session-Id: {id}\r\n"));
    }
    request.push_str("\r\n");
    request.push_str(&body);
    stream.write_all(request.as_bytes()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    let (head, rest) = response.split_once("\r\n\r\n").unwrap();
    let id = head.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("mcp-session-id")
            .then(|| value.trim().to_string())
    });
    // The reply is JSON, or a stream of events whose data is JSON.
    let json = serde_json::from_str(rest.trim()).unwrap_or_else(|_| {
        let data = rest
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .find(|data| data.contains("\"id\""))
            .unwrap_or_else(|| panic!("no reply in {rest}"));
        serde_json::from_str(data.trim()).unwrap()
    });
    (json, id)
}

#[test]
fn the_streamable_http_transport_serves_the_tools() {
    let dir = fresh_dir("mcp-http");
    store(&dir);
    let mut child: Child = {
        let _spawn = wire::spawn_lock();
        Command::new(ZOE)
            .args(["mcp", "--path", s(&dir), "--http", "127.0.0.1:0"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap()
    };
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let addr: SocketAddr = loop {
        let mut line = String::new();
        assert!(stdout.read_line(&mut line).unwrap() > 0, "zoe mcp stopped");
        if let Some(addr) = line.trim().strip_prefix("listening ") {
            break addr.parse().unwrap();
        }
    };

    let (init, session) = post(
        addr,
        None,
        &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-06-18", "capabilities": {},
            "clientInfo": {"name": "acceptance", "version": "0"}}}),
    );
    assert!(
        init["result"]["capabilities"]["tools"].is_object(),
        "{init}"
    );
    post(
        addr,
        session.as_deref(),
        &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    );
    let (reply, _) = post(
        addr,
        session.as_deref(),
        &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                "params": {"name": "object_get", "arguments": {"oid": "0x00"}}}),
    );
    assert!(
        reply["result"].to_string().contains("PersistentMapping"),
        "{reply}"
    );
    let _ = child.kill();
    let _ = child.wait();
}
