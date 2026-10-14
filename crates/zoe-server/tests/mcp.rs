//! Acceptance test of `zoe mcp`: the MCP server with the read-only ops
//! tools, over stdio (F20).
//!
//! `zoe mcp --path STORE` opens a store folder read-only and speaks MCP on
//! stdin and stdout, with the `rmcp` SDK. It runs as the ops principal of
//! the local operator. It offers these tools, Loupe's functions with an
//! agent face, and no tool that writes:
//!
//! - `object_get` with `oid` or `path`, and `as_of` (a tid): the object as
//!   decoded JSON, as Loupe's `Object` gives it;
//! - `history` with `oid`: the revisions, newest first;
//! - `diff` with `oid`, `tid_a` and `tid_b`: the changes of the decoded
//!   state;
//! - `references` with `oid`: the outbound references.
//!
//! Oids and tids are ZODB-form strings, as Loupe writes them (`0x00`). An
//! error of a tool is a tool result with `isError`, never a broken
//! session.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Mutex, MutexGuard, PoisonError};

use serde_json::{Value, json};
use zoe_core::{Oid, Store, Tid};

const ZOE: &str = env!("CARGO_BIN_EXE_zoe");

static SPAWN: Mutex<()> = Mutex::new(());

fn spawn_lock() -> MutexGuard<'static, ()> {
    SPAWN.lock().unwrap_or_else(PoisonError::into_inner)
}

fn fresh_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("zoe-mcp-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

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

/// A store with a root that refers to one child, then a second revision
/// of the child. Return the tids.
fn store(dir: &Path) -> (Tid, Tid) {
    let store = {
        let _spawn = spawn_lock();
        Store::open(dir).unwrap()
    };
    let mut txn = store.tpc_begin(b"admin", b"create", b"").unwrap();
    txn.store(Oid(0), Tid(0), &mapping("root", &[1])).unwrap();
    txn.store(Oid(1), Tid(0), &mapping("first", &[])).unwrap();
    txn.vote().unwrap();
    let first = txn.finish().unwrap();
    let mut txn = store.tpc_begin(b"editor", b"rename", b"").unwrap();
    txn.store(Oid(1), first, &mapping("second", &[])).unwrap();
    txn.vote().unwrap();
    let second = txn.finish().unwrap();
    (first, second)
}

/// A client of `zoe mcp` over its stdio.
struct Session {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Session {
    fn start(dir: &Path) -> Session {
        let mut child = {
            let _spawn = spawn_lock();
            Command::new(ZOE)
                .args(["mcp", "--path", dir.to_str().unwrap()])
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

#[test]
fn the_tools_are_the_read_only_ops_tools() {
    let dir = fresh_dir("list");
    store(&dir);
    let mut session = Session::start(&dir);
    let tools = session.request("tools/list", json!({}));
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    for name in ["object_get", "history", "diff", "references"] {
        assert!(names.contains(&name), "no tool {name} in {names:?}");
    }
    for name in ["pack", "backup"] {
        assert!(!names.contains(&name), "a read-only server offers {name}");
    }
}

#[test]
fn object_get_history_diff_and_references() {
    let dir = fresh_dir("tools");
    let (first, second) = store(&dir);
    let tid = |t: Tid| format!("0x{:016x}", t.0);
    let mut session = Session::start(&dir);

    let (failed, root) = session.call("object_get", json!({"oid": "0x00"}));
    assert!(!failed);
    assert_eq!(root["class"], "persistent.mapping.PersistentMapping");
    assert_eq!(root["oid"], "0x00");

    let (_, now) = session.call("object_get", json!({"oid": "0x01"}));
    assert!(now.to_string().contains("second"));
    let (_, then) = session.call("object_get", json!({"oid": "0x01", "as_of": tid(first)}));
    assert!(then.to_string().contains("first"));

    let (_, history) = session.call("history", json!({"oid": "0x01"}));
    let revisions = history
        .as_array()
        .cloned()
        .unwrap_or_else(|| history["revisions"].as_array().cloned().unwrap());
    assert_eq!(revisions.len(), 2);
    assert_eq!(revisions[0]["description"], "rename");
    assert_eq!(revisions[1]["user"], "admin");

    let (_, diff) = session.call(
        "diff",
        json!({"oid": "0x01", "tid_a": tid(first), "tid_b": tid(second)}),
    );
    let text = diff.to_string();
    assert!(text.contains("first") && text.contains("second"), "{text}");

    let (_, references) = session.call("references", json!({"oid": "0x00"}));
    assert!(references.to_string().contains("0x01"), "{references}");
}

#[test]
fn an_unknown_oid_is_a_tool_error_and_the_session_goes_on() {
    let dir = fresh_dir("errors");
    store(&dir);
    let mut session = Session::start(&dir);
    let (failed, _) = session.call("object_get", json!({"oid": "0x99"}));
    assert!(failed);
    let (failed, _) = session.call("object_get", json!({"oid": "0x00"}));
    assert!(!failed);
}
