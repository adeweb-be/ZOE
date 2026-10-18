//! Acceptance test of the rest of the read-only ops family of `zoe mcp`
//! in folder mode: `space`, `verify`, `explain_conflict` and the prompts
//! (F20).
//!
//! - `space` gives what Loupe's space screen gives: the classes with their
//!   counts and bytes, and the largest objects.
//! - `verify` gives the findings of `zoe verify`, in its JSON form.
//! - `explain_conflict` with `tid`: each oid that the transaction wrote,
//!   with its class, and for each oid the transactions just before and just
//!   after it that wrote the same oid, with their user and description:
//!   what another writer of the same objects did around it. Phase 2 has no
//!   record of the conflicts themselves; the tool reads the history.
//! - The prompts `explain_transaction` (argument `tid`), `why_conflict`
//!   (argument `oid`) and `summarise_changes` (argument `since`, a tid)
//!   give messages that hold the data that the tools give for them.

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
        // The reply to `initialize` comes after `exec`. So the mutex of the
        // spawns stays held until the end of this call.
        let _spawn = spawn_lock();
        let mut child = Command::new(ZOE)
            .args(["mcp", "--path", dir.to_str().unwrap()])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
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
fn the_rest_of_the_ops_tools_are_offered() {
    let dir = fresh_dir("more-list");
    store(&dir);
    let mut session = Session::start(&dir);
    let tools = session.request("tools/list", json!({}));
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    for name in ["space", "verify", "explain_conflict"] {
        assert!(names.contains(&name), "no tool {name} in {names:?}");
    }
}

#[test]
fn space_and_verify_give_what_loupe_and_zoe_verify_give() {
    let dir = fresh_dir("more-space");
    store(&dir);
    let mut session = Session::start(&dir);
    let (failed, space) = session.call("space", json!({}));
    assert!(!failed);
    assert_eq!(
        space["by_class"]["persistent.mapping.PersistentMapping"]["count"],
        2
    );

    let (failed, verify) = session.call("verify", json!({}));
    assert!(!failed);
    assert_eq!(verify["references"], json!([]));
    assert_eq!(verify["checksums"], json!([]));
}

#[test]
fn explain_conflict_shows_the_neighbours_of_each_oid() {
    let dir = fresh_dir("more-explain");
    let (first, second) = store(&dir);
    let tid = |t: Tid| format!("0x{:016x}", t.0);
    let mut session = Session::start(&dir);
    let (failed, explained) = session.call("explain_conflict", json!({"tid": tid(second)}));
    assert!(!failed);
    let text = explained.to_string();
    // The transaction wrote 0x01, which `create` wrote before it.
    assert!(text.contains("0x01"), "{text}");
    assert!(text.contains(&tid(first)), "{text}");
    assert!(text.contains("create"), "{text}");
    assert!(
        text.contains("persistent.mapping.PersistentMapping"),
        "{text}"
    );
}

#[test]
fn the_prompts_carry_the_data_of_their_tools() {
    let dir = fresh_dir("more-prompts");
    let (first, second) = store(&dir);
    let tid = |t: Tid| format!("0x{:016x}", t.0);
    let mut session = Session::start(&dir);
    let prompts = session.request("prompts/list", json!({}));
    let names: Vec<&str> = prompts["result"]["prompts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    for name in ["explain_transaction", "why_conflict", "summarise_changes"] {
        assert!(names.contains(&name), "no prompt {name} in {names:?}");
    }

    let got = session.request(
        "prompts/get",
        json!({"name": "explain_transaction", "arguments": {"tid": tid(second)}}),
    );
    let text = got["result"]["messages"].to_string();
    assert!(text.contains("rename") && text.contains("editor"), "{text}");

    let got = session.request(
        "prompts/get",
        json!({"name": "summarise_changes", "arguments": {"since": tid(first)}}),
    );
    assert!(got["result"]["messages"].to_string().contains("rename"));

    let got = session.request(
        "prompts/get",
        json!({"name": "why_conflict", "arguments": {"oid": "0x01"}}),
    );
    assert!(got["result"]["messages"].to_string().contains("0x01"));
}
