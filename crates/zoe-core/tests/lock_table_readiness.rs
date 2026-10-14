//! Acceptance test of `Store::lock_table_is_empty` as a documented public
//! method (N9).
//!
//! `/readyz` of `zoe serve` reads it once after the open of each store:
//! the lock manager of a store that the process just opened holds no
//! stale entry. So the method is part of the public API of the core, not
//! a test helper: rustdoc shows it, its documentation gives its meaning,
//! and `crates/zoe-core/CLAUDE.md` states it as an invariant.
//!
//! The method tells if no transaction holds or waits for a lock. The test
//! checks that meaning at each state of a transaction, then the
//! documentation.

use std::path::{Path, PathBuf};

use zoe_core::{Oid, Store, Tid};

fn fresh_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("zoe-lock-table-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A `PersistentMapping` whose state holds `value` under the key `v`.
fn record(value: &[u8]) -> Vec<u8> {
    assert!(value.len() < 256);
    let mut pickle = b"\x80\x03cpersistent.mapping\nPersistentMapping\nq\x00.".to_vec();
    pickle.extend_from_slice(b"\x80\x03}q\x00X\x01\x00\x00\x00vq\x01C");
    pickle.push(value.len() as u8);
    pickle.extend_from_slice(value);
    pickle.extend_from_slice(b"q\x02s.");
    pickle
}

#[test]
fn the_lock_table_is_empty_unless_a_transaction_holds_a_lock() {
    let dir = fresh_dir("states");
    let store = Store::open(&dir).unwrap();
    // A store just opened.
    assert!(store.lock_table_is_empty());

    // A vote takes the locks, and finish releases them.
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(Oid(1), Tid(0), &record(b"a")).unwrap();
    assert!(store.lock_table_is_empty(), "a store takes no lock");
    txn.vote().unwrap();
    assert!(!store.lock_table_is_empty());
    let tid = txn.finish().unwrap();
    assert!(store.lock_table_is_empty());

    // An abort after the vote releases them.
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(Oid(1), tid, &record(b"b")).unwrap();
    txn.vote().unwrap();
    assert!(!store.lock_table_is_empty());
    txn.abort();
    assert!(store.lock_table_is_empty());

    // A drop after the vote releases them.
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(Oid(1), tid, &record(b"c")).unwrap();
    txn.vote().unwrap();
    drop(txn);
    assert!(store.lock_table_is_empty());

    // A store opened again holds no entry.
    drop(store);
    let store = Store::open(&dir).unwrap();
    assert!(store.lock_table_is_empty());
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Return the lines of `source` just above the line that declares
/// `signature`: its attributes and its doc comment.
fn lines_above<'a>(source: &'a str, signature: &str) -> Vec<&'a str> {
    let lines: Vec<&str> = source.lines().collect();
    let at = lines
        .iter()
        .position(|line| line.trim_start().starts_with(signature))
        .unwrap_or_else(|| panic!("no `{signature}` in the source"));
    lines[..at]
        .iter()
        .rev()
        .take_while(|line| {
            let line = line.trim_start();
            line.starts_with("///") || line.starts_with("#[")
        })
        .copied()
        .collect()
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

#[test]
fn the_method_is_documented_public_api() {
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let source = read(&crate_dir.join("src/store.rs"));
    let above = lines_above(&source, "pub fn lock_table_is_empty(");
    assert!(
        !above.iter().any(|line| line.contains("doc(hidden)")),
        "rustdoc hides `lock_table_is_empty`: {above:?}"
    );
    let doc = above
        .iter()
        .rev()
        .filter_map(|line| line.trim_start().strip_prefix("///"))
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    assert!(!doc.is_empty(), "`lock_table_is_empty` has no doc comment");
    assert!(
        !doc.contains("test"),
        "the doc comment calls the method a test helper: {doc}"
    );
    assert!(
        doc.contains("readiness") || doc.contains("stale"),
        "the doc comment does not give the use for readiness: {doc}"
    );

    let invariants = read(&crate_dir.join("CLAUDE.md"));
    assert!(
        invariants
            .lines()
            .any(|line| line.starts_with("- ") && line.contains("lock_table_is_empty")),
        "crates/zoe-core/CLAUDE.md states no invariant of `lock_table_is_empty`"
    );
}
