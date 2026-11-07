//! Acceptance test of the text deltas (ADR 0060, sections 1, 3 and 8; F18,
//! N1, N2).
//!
//! A transaction stages text deltas, unindexes and clear deltas with
//! `Txn::text_set`, `Txn::text_unindex` and `Txn::text_clear`, and its
//! commit writes them in the key spaces `text_deltas` and `text_txn`, with
//! a history per tid. `Store::text_before` gives the live text of a
//! document at each snapshot: a text delta before the newest clear delta of
//! its index is dead, and one at the tid of the clear is live.
//! `Store::text_changed` gives the keys of the deltas of a tid range, in
//! tid order, then in key order. The deltas read the same through the
//! write-ahead log, after its fold and in a direct batch. A transaction
//! with only deltas commits with a transaction row of 0 oids, and the
//! iterator gives no record for it. A restore transaction takes only
//! `restore_text` and `restore_clear`. Each acknowledged delta survives a
//! crash, and no delta of an unacknowledged commit appears.

use zoe_core::testing::{self, commit};
use zoe_core::vfs::MemVfs;
use zoe_core::{
    LiveText, Oid, Options, SizeLimit, Store, TextChange, Tid, Txn, WriteLog, ZoeError, check_rid,
};

const CATALOG: Oid = Oid(42);
const INDEX: &str = "SearchableText";

const PICKLE: &[u8] =
    b"\x80\x03cpersistent.mapping\nPersistentMapping\nq\x00.\x80\x03}q\x01X\x04\x00\x00\x00datag\x01s.";

/// Return the snapshot of a transaction that begins now: the tid after the
/// last tid.
fn snapshot(store: &Store) -> Tid {
    Tid(store.last_transaction().0 + 1)
}

fn live(language: &str, text: &str) -> Option<LiveText> {
    Some(LiveText {
        language: language.to_owned(),
        text: text.to_owned(),
    })
}

/// Return the live text of `rid` in `INDEX` before `tid`.
fn text(store: &Store, rid: i32, tid: Tid) -> Option<LiveText> {
    store.text_before(CATALOG, INDEX, rid, tid).unwrap()
}

/// Commit the text `value` of `rid` in `INDEX`. Return the tid.
fn set(store: &Store, rid: i32, value: &str) -> Tid {
    let before = snapshot(store);
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.text_set(CATALOG, INDEX, rid, "en", value, before)
        .unwrap();
    txn.vote().unwrap();
    txn.finish().unwrap()
}

/// Commit an unindex of `rid` in `INDEX`. Return the tid.
fn unindex(store: &Store, rid: i32) -> Tid {
    let before = snapshot(store);
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.text_unindex(CATALOG, INDEX, rid, before).unwrap();
    txn.vote().unwrap();
    txn.finish().unwrap()
}

/// Commit a clear delta of `INDEX`. Return the tid.
fn clear(store: &Store) -> Tid {
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.text_clear(CATALOG, INDEX).unwrap();
    txn.vote().unwrap();
    txn.finish().unwrap()
}

fn after(tid: Tid) -> Tid {
    Tid(tid.0 + 1)
}

#[test]
fn text_deltas_unindexes_and_clears_give_the_live_text_at_each_tid() {
    let store = Store::open_vfs(MemVfs::new()).unwrap();
    let t1 = set(&store, 1, "one");
    let t2 = set(&store, 2, "two");
    let t3 = set(&store, 1, "uno");
    let t4 = unindex(&store, 2);
    let t5 = clear(&store);
    // A transaction that clears the index and then sets a text: the text
    // is at the tid of the clear, and it is live.
    let before = snapshot(&store);
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.text_clear(CATALOG, INDEX).unwrap();
    txn.text_set(CATALOG, INDEX, 3, "fr", "trois", before)
        .unwrap();
    txn.vote().unwrap();
    let t6 = txn.finish().unwrap();

    assert_eq!(text(&store, 1, t1), None);
    assert_eq!(text(&store, 1, after(t1)), live("en", "one"));
    assert_eq!(text(&store, 2, after(t1)), None);
    assert_eq!(text(&store, 2, after(t2)), live("en", "two"));
    assert_eq!(text(&store, 1, after(t3)), live("en", "uno"));
    assert_eq!(text(&store, 1, t3), live("en", "one"));
    assert_eq!(text(&store, 2, after(t4)), None);
    assert_eq!(text(&store, 2, t4), live("en", "two"));
    // After the clear, every text delta before it is dead.
    assert_eq!(text(&store, 1, after(t5)), None);
    assert_eq!(text(&store, 1, t5), live("en", "uno"));
    assert_eq!(text(&store, 3, after(t6)), live("fr", "trois"));
    assert_eq!(text(&store, 3, t6), None);
    assert_eq!(text(&store, 1, Tid::MAX), None);
    assert_eq!(text(&store, 3, Tid::MAX), live("fr", "trois"));
    // Another index and another catalog are not touched.
    assert_eq!(store.text_before(CATALOG, "Title", 1, Tid::MAX).unwrap(), None);
    assert_eq!(store.text_before(Oid(43), INDEX, 1, Tid::MAX).unwrap(), None);
}

#[test]
fn an_abort_writes_no_delta() {
    let store = Store::open_vfs(MemVfs::new()).unwrap();
    let before = snapshot(&store);
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.text_set(CATALOG, INDEX, 1, "en", "gone", before)
        .unwrap();
    txn.text_clear(CATALOG, "Title").unwrap();
    txn.vote().unwrap();
    txn.abort();
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.text_set(CATALOG, INDEX, 2, "en", "dropped", before)
        .unwrap();
    drop(txn);
    assert_eq!(store.last_transaction(), Tid::ZERO);
    assert_eq!(text(&store, 1, Tid::MAX), None);
    assert_eq!(text(&store, 2, Tid::MAX), None);
    assert!(store.text_changed(Tid::ZERO, Tid::MAX).unwrap().is_empty());
}

#[test]
fn a_later_delta_of_a_key_replaces_the_staged_one_and_a_clear_drops_those_before_it() {
    let store = Store::open_vfs(MemVfs::new()).unwrap();
    let before = snapshot(&store);
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.text_set(CATALOG, INDEX, 1, "en", "first", before)
        .unwrap();
    txn.text_set(CATALOG, INDEX, 1, "en", "second", before)
        .unwrap();
    txn.text_set(CATALOG, INDEX, 2, "en", "before the clear", before)
        .unwrap();
    txn.text_set(CATALOG, "Title", 2, "en", "another index", before)
        .unwrap();
    txn.text_clear(CATALOG, INDEX).unwrap();
    txn.text_set(CATALOG, INDEX, 3, "en", "after the clear", before)
        .unwrap();
    txn.text_unindex(CATALOG, INDEX, 4, before).unwrap();
    txn.vote().unwrap();
    let tid = txn.finish().unwrap();

    assert_eq!(text(&store, 1, Tid::MAX), None);
    assert_eq!(text(&store, 2, Tid::MAX), None);
    assert_eq!(text(&store, 3, Tid::MAX), live("en", "after the clear"));
    assert_eq!(
        store.text_before(CATALOG, "Title", 2, Tid::MAX).unwrap(),
        live("en", "another index")
    );
    // The commit holds the clear, the deltas staged after it and the
    // delta of the other index, in key order: the clear deltas of the
    // catalog first, then the index ids by their length and bytes.
    let changes = store.text_changed(Tid::ZERO, tid).unwrap();
    let keys: Vec<(&str, Option<i32>)> = changes
        .iter()
        .map(|change| (change.index.as_str(), change.rid))
        .collect();
    assert_eq!(
        keys,
        [
            (INDEX, None),
            ("Title", Some(2)),
            (INDEX, Some(3)),
            (INDEX, Some(4))
        ]
    );
}

/// Write the same deltas into `store`, and return the tids.
fn history(store: &Store) -> Vec<Tid> {
    vec![
        set(store, -7, "minus seven"),
        set(store, 9, "nine"),
        unindex(store, -7),
        clear(store),
        set(store, 9, "nine again"),
    ]
}

/// Check the deltas of `history` in `store`.
fn check_history(store: &Store, tids: &[Tid]) {
    assert_eq!(text(store, -7, after(tids[0])), live("en", "minus seven"));
    assert_eq!(text(store, -7, after(tids[2])), None);
    assert_eq!(text(store, 9, after(tids[2])), live("en", "nine"));
    assert_eq!(text(store, 9, after(tids[3])), None);
    assert_eq!(text(store, 9, Tid::MAX), live("en", "nine again"));
    let changes = store.text_changed(Tid::ZERO, Tid::MAX).unwrap();
    let rids: Vec<(Tid, Option<i32>)> = changes
        .iter()
        .map(|change| (change.tid, change.rid))
        .collect();
    assert_eq!(
        rids,
        [
            (tids[0], Some(-7)),
            (tids[1], Some(9)),
            (tids[2], Some(-7)),
            (tids[3], None),
            (tids[4], Some(9)),
        ]
    );
}

fn held() -> WriteLog {
    WriteLog {
        fold_bytes: 1 << 40,
        fold_age: std::time::Duration::from_secs(3600),
        backlog_bytes: 1 << 41,
        ..WriteLog::default()
    }
}

fn options(write_log: WriteLog) -> Options {
    Options {
        write_log,
        ..Options::default()
    }
}

#[test]
fn a_delta_through_the_log_after_the_fold_and_in_a_direct_batch_reads_the_same() {
    // Through the log: the backlog holds every commit.
    let vfs = MemVfs::new();
    let store = Store::open_full(vfs.clone(), zoe_core::SystemClock, options(held())).unwrap();
    let tids = history(&store);
    assert!(store.backlog_bytes() > 0);
    check_history(&store, &tids);
    // After the fold, from the tables.
    store.fold().unwrap();
    assert_eq!(store.backlog_bytes(), 0);
    check_history(&store, &tids);
    drop(store);
    let store = Store::open_full(vfs, zoe_core::SystemClock, options(held())).unwrap();
    check_history(&store, &tids);

    // In direct batches: the log is off.
    let direct = WriteLog {
        enabled: false,
        ..WriteLog::default()
    };
    let store =
        Store::open_full(MemVfs::new(), zoe_core::SystemClock, options(direct)).unwrap();
    let tids = history(&store);
    assert_eq!(store.backlog_bytes(), 0);
    check_history(&store, &tids);
}

#[test]
fn a_backlog_and_the_tables_merge_in_a_scan_of_text_txn() {
    let vfs = MemVfs::new();
    let store = Store::open_full(vfs, zoe_core::SystemClock, options(held())).unwrap();
    let t1 = set(&store, 5, "five");
    let t2 = set(&store, 1, "one");
    store.fold().unwrap();
    let t3 = set(&store, 3, "three");
    let t4 = set(&store, 2, "two");
    assert!(store.backlog_bytes() > 0);
    let changes = store.text_changed(Tid::ZERO, Tid::MAX).unwrap();
    let got: Vec<(Tid, Option<i32>)> = changes.iter().map(|c| (c.tid, c.rid)).collect();
    assert_eq!(
        got,
        [(t1, Some(5)), (t2, Some(1)), (t3, Some(3)), (t4, Some(2))]
    );
    // A range: after the first tid, at or before the third.
    let changes = store.text_changed(t1, t3).unwrap();
    assert_eq!(
        changes,
        [
            TextChange {
                tid: t2,
                catalog: CATALOG,
                index: INDEX.to_owned(),
                rid: Some(1)
            },
            TextChange {
                tid: t3,
                catalog: CATALOG,
                index: INDEX.to_owned(),
                rid: Some(3)
            },
        ]
    );
    assert!(store.text_changed(t4, t4).unwrap().is_empty());
}

#[test]
fn a_transaction_with_only_deltas_commits_a_row_of_no_oid_and_the_iterator_gives_no_record() {
    let store = Store::open_vfs(MemVfs::new()).unwrap();
    let oid = testing::new_oid(&store);
    let first = commit(&store, oid, Tid::ZERO, PICKLE);
    let tid = set(&store, 1, "text");
    assert_eq!(store.last_transaction(), tid);
    let txns: Vec<_> = store
        .iterator(None, None)
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(txns.len(), 2);
    assert_eq!(txns[0].tid, first);
    assert_eq!(txns[1].tid, tid);
    assert!(txns[1].records.is_empty());
    // The object count does not count deltas, and no record has the tid.
    assert_eq!(store.object_count(), Some(1));
    assert_eq!(store.load(oid).unwrap(), (PICKLE.to_vec(), first));
    assert_eq!(store.history(oid, 10).unwrap().len(), 1);
}

#[test]
fn a_delta_over_a_limit_or_with_a_bad_argument_stages_nothing() {
    let options = Options {
        max_record_size: Some(64),
        max_transaction_size: Some(200),
        ..Options::default()
    };
    let store = Store::open_full(MemVfs::new(), zoe_core::SystemClock, options).unwrap();
    let before = snapshot(&store);
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    let long = "x".repeat(64);
    match txn.text_set(CATALOG, INDEX, 1, "en", &long, before) {
        Err(ZoeError::TextTooLarge {
            limit: SizeLimit::Record,
            ..
        }) => {}
        other => panic!("{other:?}"),
    }
    let fits = "y".repeat(60);
    txn.text_set(CATALOG, INDEX, 1, "en", &fits, before).unwrap();
    txn.text_set(CATALOG, INDEX, 2, "en", &fits, before).unwrap();
    match txn.text_set(CATALOG, INDEX, 3, "en", &fits, before) {
        Err(ZoeError::TextTooLarge {
            limit: SizeLimit::Transaction,
            ..
        }) => {}
        other => panic!("{other:?}"),
    }
    // A delta that replaces a staged one counts once.
    txn.text_set(CATALOG, INDEX, 2, "en", "short", before)
        .unwrap();
    for (index, language) in [("", "en"), (&*"i".repeat(256), "en"), (INDEX, &*"l".repeat(36)), (INDEX, "é")] {
        match txn.text_set(CATALOG, index, 9, language, "t", before) {
            Err(ZoeError::InvalidText(_)) => {}
            other => panic!("{other:?}"),
        }
    }
    assert!(matches!(
        txn.text_unindex(CATALOG, "", 9, before),
        Err(ZoeError::InvalidText(_))
    ));
    assert!(matches!(
        txn.text_clear(CATALOG, &"i".repeat(256)),
        Err(ZoeError::InvalidText(_))
    ));
    // A rid outside the signed 32-bit range.
    assert!(matches!(
        check_rid(i64::from(i32::MAX) + 1),
        Err(ZoeError::InvalidText(_))
    ));
    assert!(matches!(check_rid(i64::from(i32::MIN) - 1), Err(ZoeError::InvalidText(_))));
    assert_eq!(check_rid(-3).unwrap(), -3);
    txn.vote().unwrap();
    let tid = txn.finish().unwrap();
    let changes = store.text_changed(Tid::ZERO, tid).unwrap();
    let rids: Vec<Option<i32>> = changes.iter().map(|change| change.rid).collect();
    assert_eq!(rids, [Some(1), Some(2)]);
    assert_eq!(text(&store, 2, Tid::MAX), live("en", "short"));
    assert_eq!(text(&store, 3, Tid::MAX), None);
}

#[test]
fn a_restore_transaction_takes_only_the_restored_deltas() {
    let store = Store::open_vfs(MemVfs::new()).unwrap();
    let mut txn = Txn::begin_restore(&store, Tid(1000), b"", b"", b"").unwrap();
    assert!(matches!(
        txn.text_set(CATALOG, INDEX, 1, "en", "x", Tid(1)),
        Err(ZoeError::Protocol(_))
    ));
    assert!(matches!(
        txn.text_unindex(CATALOG, INDEX, 1, Tid(1)),
        Err(ZoeError::Protocol(_))
    ));
    assert!(matches!(
        txn.text_clear(CATALOG, INDEX),
        Err(ZoeError::Protocol(_))
    ));
    txn.restore_clear(CATALOG, INDEX).unwrap();
    txn.restore_text(CATALOG, INDEX, 1, "en", Some("restored"))
        .unwrap();
    txn.restore_text(CATALOG, INDEX, 2, "", None).unwrap();
    txn.vote().unwrap();
    assert_eq!(txn.finish().unwrap(), Tid(1000));
    assert_eq!(text(&store, 1, Tid::MAX), live("en", "restored"));
    assert_eq!(text(&store, 1, Tid(1000)), None);
    let changes = store.text_changed(Tid::ZERO, Tid::MAX).unwrap();
    let rids: Vec<Option<i32>> = changes.iter().map(|change| change.rid).collect();
    assert_eq!(rids, [None, Some(1), Some(2)]);

    // A normal transaction refuses the restored deltas.
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    assert!(matches!(
        txn.restore_text(CATALOG, INDEX, 1, "en", Some("x")),
        Err(ZoeError::Protocol(_))
    ));
    assert!(matches!(
        txn.restore_clear(CATALOG, INDEX),
        Err(ZoeError::Protocol(_))
    ));
}

#[test]
fn each_acknowledged_delta_survives_a_crash_and_no_delta_of_an_unacknowledged_commit_appears() {
    for write_log in [WriteLog::default(), held(), WriteLog { enabled: false, ..WriteLog::default() }] {
        let vfs = MemVfs::new();
        let (first, second) = {
            let store =
                Store::open_full(vfs.clone(), zoe_core::SystemClock, options(write_log.clone()))
                    .unwrap();
            let first = set(&store, 1, "acknowledged");
            vfs.mark("acknowledged");
            let before = snapshot(&store);
            let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
            txn.text_set(CATALOG, INDEX, 2, "en", "later", before)
                .unwrap();
            txn.text_clear(CATALOG, "Title").unwrap();
            txn.text_set(CATALOG, INDEX, 1, "en", "changed", before)
                .unwrap();
            txn.vote().unwrap();
            vfs.mark("voted");
            let second = txn.finish().unwrap();
            vfs.mark("finished");
            (first, second)
        };
        let images = vfs.crash_images();
        assert!(images.len() > 1);
        for image in &images {
            let store = Store::open_full(image.vfs(), zoe_core::SystemClock, options(write_log.clone()))
                .unwrap_or_else(|e| panic!("{image:?} does not open: {e}"));
            if image.is_after("acknowledged") {
                assert_eq!(
                    text(&store, 1, after(first)),
                    live("en", "acknowledged"),
                    "{image:?}"
                );
            }
            // The second commit is whole or absent.
            let later = text(&store, 2, Tid::MAX);
            let changed = text(&store, 1, Tid::MAX) == live("en", "changed");
            let committed = store.last_transaction() == second;
            assert_eq!(later.is_some(), committed, "{image:?}");
            assert_eq!(changed, committed, "{image:?}");
            let changes = store.text_changed(first, Tid::MAX).unwrap();
            assert_eq!(changes.len(), if committed { 3 } else { 0 }, "{image:?}");
            if image.is_after("finished") {
                assert!(committed, "{image:?}");
            }
        }
    }
}
