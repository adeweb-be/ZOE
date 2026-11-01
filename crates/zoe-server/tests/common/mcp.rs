//! A client of `zoe mcp` over its stdio, and a small store for its tests.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{Value, json};
use zoe_core::{Oid, Tid};

use super::{ZOE, clear_zoe_env, open_store, s, spawn_lock};

/// A `PersistentMapping` with a title and references to `refs`.
pub fn mapping(title: &str, refs: &[u64]) -> Vec<u8> {
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
pub fn store(dir: &Path) -> (Tid, Tid) {
    let store = open_store(dir);
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
pub struct Session {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Session {
    /// Start `zoe mcp --path DIR`. No `ZOE_*` variable reaches it.
    pub fn path(dir: &Path) -> Session {
        Session::start(&["--path", s(dir)], &[])
    }

    /// Start `zoe mcp` with `args` and the variables `env`. No other
    /// `ZOE_*` variable reaches it.
    pub fn start(args: &[&str], env: &[(&str, &str)]) -> Session {
        let mut command = Command::new(ZOE);
        clear_zoe_env(&mut command)
            .arg("mcp")
            .args(args)
            .envs(env.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        // The reply to `initialize` comes after `exec`. So the mutex of the
        // spawns stays held until the end of this call.
        let _spawn = spawn_lock();
        let mut child = command.spawn().unwrap();
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

    /// Write one message, as one line.
    pub fn send(&mut self, message: Value) {
        writeln!(self.stdin, "{message}").unwrap();
        self.stdin.flush().unwrap();
    }

    /// Send a notification. It has no reply.
    pub fn notify(&mut self, method: &str, params: Value) {
        self.send(json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    /// Send a request and return its reply. Skip the other messages.
    pub fn request(&mut self, method: &str, params: Value) -> Value {
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

    /// Call a tool. Return whether it failed, and its result as JSON: the
    /// structured content, or the JSON of its first text.
    pub fn call(&mut self, tool: &str, arguments: Value) -> (bool, Value) {
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
