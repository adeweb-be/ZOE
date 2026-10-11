//! Acceptance test of the server mode of Loupe (F15).
//!
//! `Remote::connect(address, storage)` opens a view of a store that a
//! server serves, through `zoe-client` over the ZEO protocol. It registers
//! read-only, so it works against a read-only server and never writes. It
//! shows the same screens as a store folder: `Remote` has the methods of
//! `Folder` (`last_tid`, `root`, `object`, `resolve`, `history`, `diff`
//! and `as_of`), and each gives what the folder gives on the same store.
//!
//! The test writes the fixture store, reads it through `Folder`, closes
//! it, serves it with `zoe_server::serve::Server`, and reads it again
//! through `Remote`.

mod common;

use std::collections::BTreeMap;

use serde_json::Value;
use zoe_core::Tid;
use zoe_loupe::{Folder, Remote};
use zoe_server::serve::{ServeOptions, Server};

use common::{fresh_dir, write_store};

/// What a view shows of every path of the fixture as of each of its tids,
/// as JSON, and the history of each object that a path leads to.
#[derive(Debug, PartialEq)]
struct Shown {
    root: Value,
    paths: BTreeMap<(Tid, String), Value>,
    histories: BTreeMap<String, Value>,
    diffs: BTreeMap<String, Value>,
}

macro_rules! show {
    ($source:expr, $fixture:expr) => {{
        let source = $source;
        let fixture = $fixture;
        let mut shown = Shown {
            root: serde_json::to_value(source.root().unwrap()).unwrap(),
            paths: BTreeMap::new(),
            histories: BTreeMap::new(),
            diffs: BTreeMap::new(),
        };
        for txn in &fixture.txns {
            let view = source.as_of(txn.tid);
            for (path, target) in &txn.paths {
                let found = view.resolve(path);
                let json = match (found, target) {
                    (Ok(object), Some(_)) => serde_json::to_value(&object).unwrap(),
                    (Err(error), None) => Value::String(error.to_string()),
                    (found, target) => {
                        panic!("{path} as of {:?}: {found:?} for {target:?}", txn.tid)
                    }
                };
                shown.paths.insert((txn.tid, path.clone()), json);
            }
        }
        let last = fixture.last_tid();
        for txn in &fixture.txns {
            for (path, target) in &txn.paths {
                let Some((oid, _)) = target else { continue };
                let history = source.history(*oid).unwrap();
                shown
                    .histories
                    .insert(path.clone(), serde_json::to_value(&history).unwrap());
                if let [newer, older, ..] = history.as_slice() {
                    let diff = source.diff(*oid, older.tid, newer.tid).unwrap();
                    shown.diffs.insert(
                        path.clone(),
                        Value::Array(
                            diff.into_iter()
                                .map(|c| serde_json::json!([c.path, c.before, c.after]))
                                .collect(),
                        ),
                    );
                }
            }
        }
        assert_eq!(source.last_tid(), last);
        shown
    }};
}

#[test]
fn a_server_shows_what_the_folder_shows() {
    let dir = fresh_dir("server-mode");
    let fixture = write_store(&dir);
    let by_folder = {
        let folder = Folder::open(&dir).unwrap();
        show!(&folder, &fixture)
    };

    let server = Server::start(ServeOptions::new(&dir)).unwrap();
    let remote = Remote::connect(server.addr(), "1").unwrap();
    let by_server = show!(&remote, &fixture);
    drop(remote);
    server.stop();

    assert_eq!(by_server, by_folder);
}

#[test]
fn a_read_only_server_is_enough() {
    let dir = fresh_dir("read-only");
    let fixture = write_store(&dir);
    let server = Server::start(ServeOptions::new(&dir).read_only(true)).unwrap();
    let remote = Remote::connect(server.addr(), "1").unwrap();
    assert_eq!(remote.last_tid(), fixture.last_tid());
    let root = remote.root().unwrap();
    assert_eq!(
        root.class.as_deref(),
        Some("persistent.mapping.PersistentMapping")
    );
    drop(remote);
    server.stop();
}

#[test]
fn an_unknown_storage_or_a_dead_server_is_an_error() {
    let dir = fresh_dir("errors");
    write_store(&dir);
    let server = Server::start(ServeOptions::new(&dir)).unwrap();
    assert!(Remote::connect(server.addr(), "nothing-here").is_err());
    let addr = server.addr();
    server.stop();
    assert!(Remote::connect(addr, "1").is_err());
}
