//! Acceptance test of `zoe verify` on the text deltas (ADR 0060, section
//! 7).
//!
//! `zoe verify` reads every row of `text_deltas`. It checks the frame, the
//! parse of the key, the previous serial, the transaction row at the tid,
//! and that each delta has its key in `text_txn` and each key of
//! `text_txn` its delta. A damaged delta, a delta without its key in
//! `text_txn`, a key of `text_txn` without its delta, and a delta without
//! its transaction row are findings, each named by the catalog, the index,
//! the rid and the tid. The output holds no text. A missing previous delta
//! at or before `pack_watermark` passes: a pack removed it.

use std::path::Path;

use zoe_core::testing;
use zoe_core::{Oid, Options, Store, Tid};

mod common;

use common::{fresh_dir, open_store, record, s, spawn_lock, zoe};

const CATALOG: Oid = Oid(0x2a);
const INDEX: &str = "SearchableText";

/// The words of each text. No output may hold them.
const SECRET: &str = "confidential-words";

/// Commit the text of `rid` with `SECRET` in it. Return the tid.
fn set(store: &Store, rid: i32) -> Tid {
    let before = Tid(store.last_transaction().0 + 1);
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.text_set(
        CATALOG,
        INDEX,
        rid,
        "en",
        &format!("{SECRET} {rid}"),
        before,
    )
    .unwrap();
    txn.vote().unwrap();
    txn.finish().unwrap()
}

/// Run `zoe verify` on `dir` in both formats. Return the exit code, the
/// output of the text format and the checksum findings of the JSON format.
fn verify(dir: &Path) -> (i32, String, Vec<serde_json::Value>) {
    let text = zoe(&["verify", "--path", s(dir)], &[]);
    let json = zoe(&["verify", "--path", s(dir), "--format", "json"], &[]);
    let code = text.status.code().unwrap();
    assert_eq!(json.status.code(), Some(code));
    let mut all = String::from_utf8_lossy(&text.stdout).into_owned();
    all.push_str(&String::from_utf8_lossy(&text.stderr));
    let both = format!("{all}{}", String::from_utf8_lossy(&json.stdout));
    assert!(!both.contains(SECRET), "the output holds a text:\n{both}");
    let report: serde_json::Value =
        serde_json::from_slice(&json.stdout).unwrap_or_else(|_| panic!("no JSON report:\n{all}"));
    let checksums = report["checksums"].as_array().unwrap().clone();
    (code, all, checksums)
}

/// Tell if one checksum finding names the delta of `rid` at `tid` and holds
/// `problem`.
fn finds(checksums: &[serde_json::Value], rid: i32, tid: Tid, problem: &str) -> bool {
    checksums.iter().any(|finding| {
        let error = finding["error"].as_str().unwrap_or_default();
        finding["tid"] == tid.0
            && error.contains(&format!("rid {rid}"))
            && error.contains(INDEX)
            && error.contains(&CATALOG.repr())
            && error.contains(problem)
    })
}

#[test]
fn each_damage_of_the_deltas_is_a_finding_that_names_the_delta_and_no_text() {
    let dir = fresh_dir("damaged");
    let (damaged, unindexed, orphan, rowless) = {
        let store = open_store(&dir);
        let damaged = set(&store, 1);
        let unindexed = set(&store, 2);
        let orphan = set(&store, 3);
        // A damaged frame of the delta of rid 1.
        let key = testing::text_delta_key(CATALOG, INDEX, Some(1), damaged);
        let mut frame = testing::read_text_frame(&store, &key).unwrap();
        let last = frame.len() - 1;
        frame[last] ^= 0x04;
        testing::write_text_frame(&store, &key, Some(&frame)).unwrap();
        // The delta of rid 2 loses its key in text_txn.
        let key = testing::text_delta_key(CATALOG, INDEX, Some(2), unindexed);
        testing::write_text_txn_key(&store, &testing::text_txn_key(&key), false).unwrap();
        // A key of text_txn for rid 3 at a tid without its delta.
        let missing = Tid(orphan.0 + 7);
        let key = testing::text_delta_key(CATALOG, INDEX, Some(3), missing);
        testing::write_text_txn_key(&store, &testing::text_txn_key(&key), true).unwrap();
        // A delta of rid 4 at a tid without a transaction row, before the
        // first commit.
        let rowless = Tid(damaged.0 - 1000);
        let key = testing::text_delta_key(CATALOG, INDEX, Some(4), rowless);
        let payload = testing::text_payload("en", SECRET);
        testing::write_text_frame(
            &store,
            &key,
            Some(&testing::text_frame(&key, Tid::ZERO, &payload)),
        )
        .unwrap();
        testing::write_text_txn_key(&store, &testing::text_txn_key(&key), true).unwrap();
        (damaged, unindexed, missing, rowless)
    };
    let (code, text, checksums) = verify(&dir);
    assert_eq!(code, 1, "{text}");
    assert!(finds(&checksums, 1, damaged, "checksum"), "{checksums:#?}");
    assert!(
        finds(&checksums, 2, unindexed, "without its key in text_txn"),
        "{checksums:#?}"
    );
    assert!(
        finds(&checksums, 3, orphan, "without its delta"),
        "{checksums:#?}"
    );
    assert!(
        finds(&checksums, 4, rowless, "without its transaction row"),
        "{checksums:#?}"
    );
    assert_eq!(checksums.len(), 4, "{checksums:#?}");
    // The text format names each delta too.
    for tid in [damaged, unindexed, orphan, rowless] {
        assert!(text.contains(&tid.hex()), "{tid:?}:\n{text}");
    }
}

#[test]
fn a_store_with_deltas_verifies_and_a_missing_previous_delta_before_the_watermark_passes() {
    let dir = fresh_dir("watermark");
    let tids = {
        let _spawn = spawn_lock();
        // Without garbage collection, a pack keeps the newest record of
        // each oid, so the transaction rows of these commits stay.
        let options = Options {
            pack_gc: false,
            ..Options::default()
        };
        let store = Store::open_with(&dir, options).unwrap();
        let mut tids = Vec::new();
        for (oid, value) in [(Oid(1), b"a"), (Oid(2), b"b"), (Oid(3), b"c")] {
            let before = Tid(store.last_transaction().0 + 1);
            let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
            txn.store(oid, Tid::ZERO, &record(value)).unwrap();
            let text = format!("{SECRET} {}", tids.len());
            txn.text_set(CATALOG, INDEX, 1, "en", &text, before)
                .unwrap();
            txn.vote().unwrap();
            tids.push(txn.finish().unwrap());
        }
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.text_clear(CATALOG, INDEX).unwrap();
        txn.vote().unwrap();
        txn.finish().unwrap();
        store.pack(tids[1]).unwrap();
        assert_eq!(store.pack_watermark().unwrap(), tids[1]);
        tids
    };
    let (code, text, checksums) = verify(&dir);
    assert_eq!(code, 0, "{text}");
    assert!(checksums.is_empty(), "{checksums:#?}");

    // The delta at the first tid goes, as a pack of the deltas would remove
    // it: the delta after it names a previous delta at or before the
    // watermark, which passes.
    {
        let store = open_store(&dir);
        let key = testing::text_delta_key(CATALOG, INDEX, Some(1), tids[0]);
        testing::write_text_frame(&store, &key, None).unwrap();
        testing::write_text_txn_key(&store, &testing::text_txn_key(&key), false).unwrap();
    }
    let (code, text, checksums) = verify(&dir);
    assert_eq!(code, 0, "{text}");
    assert!(checksums.is_empty(), "{checksums:#?}");
}
