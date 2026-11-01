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

use serde_json::json;
use zoe_core::Tid;

mod common;

use common::fresh_dir;
use common::mcp::{Session, store};

#[test]
fn the_tools_are_the_read_only_ops_tools() {
    let dir = fresh_dir("list");
    store(&dir);
    let mut session = Session::path(&dir);
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
    let mut session = Session::path(&dir);

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
    let mut session = Session::path(&dir);
    let (failed, _) = session.call("object_get", json!({"oid": "0x99"}));
    assert!(failed);
    let (failed, _) = session.call("object_get", json!({"oid": "0x00"}));
    assert!(!failed);
}
