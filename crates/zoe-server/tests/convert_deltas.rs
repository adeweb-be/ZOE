//! Acceptance test of the text deltas in `zoe convert` and in a copy
//! through the iterator (ADR 0060, section 6; F10, F14).
//!
//! A `Data.fs` holds no delta. `zoe convert --to-data-fs` of a store with
//! deltas writes every record and every transaction that the same store
//! without its deltas gives, byte for byte, and never fails for a delta.
//! Its report counts the deltas and holds no text. A copy through the
//! iterator, as `copyTransactionsFrom` makes it, gives every record of
//! such a store, no delta and no error.

use std::path::{Path, PathBuf};

use zoe_core::{Oid, Store, Tid, TxnInfo};

mod common;

use common::{assert_exit, fresh_dir, open_store, record, s, zoe};

const CATALOG: Oid = Oid(1);
const INDEX: &str = "SearchableText";
const TITLE: &str = "Title";

/// The words of each text. No output may hold them.
const SECRET: &str = "confidential-words";

/// The first tid of the stores.
const BASE: u64 = 0x03f6_db4f_2222_2222;

/// The number of deltas that `build` writes.
const DELTAS: u64 = 7;

/// Build in `dir` a store of three transactions at fixed tids: records,
/// a transaction with nothing but deltas, and an edit with a clear delta.
/// With `deltas` false, the same store without its deltas.
fn build(dir: &Path, deltas: bool) {
    let store = open_store(dir);
    let mut txn = store
        .tpc_begin_restore(Tid(BASE + 1), b"user", b"create", b"")
        .unwrap();
    txn.restore(Oid(0), Some(&record(b"root"))).unwrap();
    txn.restore(CATALOG, Some(&record(b"catalog"))).unwrap();
    if deltas {
        for rid in [1, 2] {
            let text = format!("{SECRET} {rid}");
            txn.restore_text(CATALOG, INDEX, rid, "en", Some(&text))
                .unwrap();
        }
        txn.restore_text(CATALOG, TITLE, 1, "", Some("a title"))
            .unwrap();
    }
    txn.vote().unwrap();
    txn.finish().unwrap();

    let mut txn = store
        .tpc_begin_restore(Tid(BASE + 2), b"", b"deltas", b"")
        .unwrap();
    if deltas {
        let text = format!("{SECRET} again");
        txn.restore_text(CATALOG, INDEX, 1, "en", Some(&text))
            .unwrap();
        txn.restore_text(CATALOG, INDEX, 2, "", None).unwrap();
    }
    txn.vote().unwrap();
    txn.finish().unwrap();

    let mut txn = store
        .tpc_begin_restore(Tid(BASE + 3), b"", b"clear", b"")
        .unwrap();
    txn.restore(CATALOG, Some(&record(b"catalog 2"))).unwrap();
    if deltas {
        txn.restore_clear(CATALOG, TITLE).unwrap();
        txn.restore_text(CATALOG, TITLE, 2, "", Some("another title"))
            .unwrap();
    }
    txn.vote().unwrap();
    txn.finish().unwrap();
    let written = store.text_changed(Tid::ZERO, Tid::MAX).unwrap().len() as u64;
    assert_eq!(written, if deltas { DELTAS } else { 0 });
}

/// Convert the store in `dir` into `dir/Data.fs`. Return the output and
/// the path of the file.
fn convert(dir: &Path) -> (std::process::Output, PathBuf) {
    let data_fs = dir.join("Data.fs");
    let blobs = dir.join("blobs.fs");
    let output = zoe(
        &[
            "convert",
            "--path",
            s(&dir.join("store")),
            "--to-data-fs",
            s(&data_fs),
            "--data-fs-blob-dir",
            s(&blobs),
        ],
        &[],
    );
    (output, data_fs)
}

/// Every transaction of a store, with its records.
fn transactions(store: &Store) -> Vec<TxnInfo> {
    store
        .iterator(None, None)
        .unwrap()
        .map(|txn| txn.unwrap())
        .collect()
}

#[test]
fn a_conversion_to_a_data_fs_drops_the_deltas_and_counts_them() {
    let dir = fresh_dir("to-data-fs");
    let (with, without) = (dir.join("with"), dir.join("without"));
    build(&with.join("store"), true);
    build(&without.join("store"), false);

    let (output, with_file) = convert(&with);
    assert_exit(&output, 0, "the store with deltas");
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        all.contains(&format!("left out      {DELTAS} deltas")),
        "{all}"
    );
    assert!(!all.contains(SECRET), "the report holds a text:\n{all}");
    assert!(!all.contains("a title"), "the report holds a text:\n{all}");

    let (output, without_file) = convert(&without);
    assert_exit(&output, 0, "the store without deltas");
    let plain = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(!plain.contains("left out"), "{plain}");

    // The same transactions and the same records, byte for byte.
    let with_bytes = std::fs::read(&with_file).unwrap();
    assert!(with_bytes.len() > 4);
    assert_eq!(with_bytes, std::fs::read(&without_file).unwrap());
}

#[test]
fn a_copy_through_the_iterator_gives_every_record_and_no_delta() {
    let dir = fresh_dir("iterator-copy");
    build(&dir.join("source"), true);
    let source = open_store(&dir.join("source"));
    let target = open_store(&dir.join("target"));
    // What `copyTransactionsFrom` does: each transaction of the iterator
    // goes through a restore transaction under its tid.
    for txn in source.iterator(None, None).unwrap() {
        let txn = txn.unwrap();
        let mut copy = target
            .tpc_begin_restore(txn.tid, &txn.user, &txn.description, &txn.extension)
            .unwrap();
        for record in &txn.records {
            copy.restore(record.oid, record.data.as_deref()).unwrap();
        }
        copy.vote().unwrap();
        copy.finish().unwrap();
    }
    assert_eq!(transactions(&source), transactions(&target));
    assert_eq!(transactions(&target).len(), 3);
    assert!(target.text_changed(Tid::ZERO, Tid::MAX).unwrap().is_empty());
}
