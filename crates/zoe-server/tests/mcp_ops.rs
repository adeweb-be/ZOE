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

use serde_json::json;
use zoe_core::Tid;

mod common;

use common::fresh_dir;
use common::mcp::{Session, store};

#[test]
fn the_rest_of_the_ops_tools_are_offered() {
    let dir = fresh_dir("more-list");
    store(&dir);
    let mut session = Session::path(&dir);
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
    let mut session = Session::path(&dir);
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
    let mut session = Session::path(&dir);
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
    let mut session = Session::path(&dir);
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
