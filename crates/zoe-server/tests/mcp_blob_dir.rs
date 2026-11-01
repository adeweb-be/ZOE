//! Acceptance test of `--blob-dir` in `zoe mcp` (F20).
//!
//! The store does not record its blob directory, so each open must give
//! it (ADR 0027). `zoe mcp --blob-dir DIR` (`ZOE_BLOB_DIR`) gives the
//! blob directory of the store, `<path>/blobs` by default, as in
//! `zoe verify` and `zoe convert`. The `verify` and `space` tools then
//! read the blob files there. Without the option, a store whose blobs
//! live elsewhere stays a tool error for `verify`, as for `zoe verify`.
//! The read-only open creates nothing in the blob directory.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::json;
use zoe_core::{Oid, Options, Store, Tid};

mod common;

use common::mcp::Session;
use common::{ZOE, fresh_dir, s, spawn_lock};

/// The class pickle of `ZODB.blob.Blob`, then the state `None`.
const BLOB: &[u8] = b"\x80\x03cZODB.blob\nBlob\nq\x00.\x80\x03N.";

/// A `PersistentMapping` with an empty state.
const MAPPING: &[u8] = b"\x80\x03cpersistent.mapping\nPersistentMapping\nq\x00.\x80\x03}q\x01.";

/// A store in `dir` whose blob directory is `blobs`, with a root and one
/// blob of 4 bytes.
fn store_with_blobs_elsewhere(dir: &Path, blobs: &Path) {
    let options = Options {
        blob_dir: Some(blobs.to_owned()),
        ..Options::default()
    };
    let _spawn = spawn_lock();
    let store = Store::open_with(dir, options).unwrap();
    let mut txn = store.tpc_begin(b"admin", b"blob", b"").unwrap();
    txn.store(Oid(0), Tid(0), MAPPING).unwrap();
    let mut file = store.new_blob_file().unwrap();
    file.write_all(b"blob").unwrap();
    txn.store_blob(Oid(1), Tid(0), BLOB, file).unwrap();
    txn.vote().unwrap();
    txn.finish().unwrap();
}

/// Every path under `dir`, `dir` included.
fn tree(dir: &Path) -> BTreeSet<PathBuf> {
    let mut found = BTreeSet::from([dir.to_owned()]);
    if dir.is_dir() {
        for entry in std::fs::read_dir(dir).unwrap() {
            found.extend(tree(&entry.unwrap().path()));
        }
    }
    found
}

/// Check that `verify` and `space` read the blob of the store.
fn assert_reads_the_blob(session: &mut Session) {
    let (failed, report) = session.call("verify", json!({}));
    assert!(!failed, "{report}");
    assert_eq!(report["findings"], 0, "{report}");
    assert_eq!(report["walked"]["blobs"], 1, "{report}");

    let (failed, space) = session.call("space", json!({}));
    assert!(!failed, "{space}");
    assert_eq!(space["blobs"]["count"], 1, "{space}");
    assert_eq!(space["blobs"]["bytes"], 4, "{space}");
}

#[test]
fn blob_dir_gives_the_blob_directory_of_the_store() {
    let root = fresh_dir("option");
    let (dir, blobs) = (root.join("store"), root.join("elsewhere"));
    store_with_blobs_elsewhere(&dir, &blobs);
    let before = tree(&root);

    let mut session = Session::start(&["--path", s(&dir), "--blob-dir", s(&blobs)], &[]);
    assert_reads_the_blob(&mut session);
    drop(session);
    // The read-only open creates nothing, in the store or in the blob
    // directory.
    assert_eq!(tree(&root), before);
}

#[test]
fn the_twin_gives_the_blob_directory_and_the_option_wins() {
    let root = fresh_dir("twin");
    let (dir, blobs) = (root.join("store"), root.join("elsewhere"));
    store_with_blobs_elsewhere(&dir, &blobs);

    let mut session = Session::start(&["--path", s(&dir)], &[("ZOE_BLOB_DIR", s(&blobs))]);
    assert_reads_the_blob(&mut session);
    drop(session);

    let wrong = root.join("wrong");
    let mut session = Session::start(
        &["--path", s(&dir), "--blob-dir", s(&blobs)],
        &[("ZOE_BLOB_DIR", s(&wrong))],
    );
    assert_reads_the_blob(&mut session);
    drop(session);
    assert!(
        !wrong.exists(),
        "the open created the blob directory of the twin"
    );
}

#[test]
fn without_the_option_verify_is_a_tool_error_for_blobs_elsewhere() {
    let root = fresh_dir("default");
    let (dir, blobs) = (root.join("store"), root.join("elsewhere"));
    store_with_blobs_elsewhere(&dir, &blobs);

    let mut session = Session::start(&["--path", s(&dir)], &[]);
    let (failed, message) = session.call("verify", json!({}));
    assert!(failed, "{message}");
    let text = message.as_str().unwrap_or_default().to_owned();
    assert!(text.contains("has no layout file"), "{text}");
}

#[test]
fn the_help_names_the_option_and_its_twin() {
    let output = {
        let _spawn = spawn_lock();
        Command::new(ZOE).args(["mcp", "--help"]).output().unwrap()
    };
    assert_eq!(output.status.code(), Some(0));
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(help.contains("--blob-dir"), "{help}");
    assert!(help.contains("ZOE_BLOB_DIR"), "{help}");
}
