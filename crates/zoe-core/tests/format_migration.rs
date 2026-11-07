//! Acceptance test of `ZOE format 4` and its migration tool (ADR 0060,
//! section 2; N19).
//!
//! A store of `ZOE format 3` fails to open with `Format`, and the message
//! names `zoe migrate`. `Store::migrate`, the work of `zoe migrate`, opens
//! it with the rules of format 3, folds its write-ahead log, then writes
//! the row `format` and creates the tables `text_deltas` and `text_txn` in
//! one backend write with a sync. The moved store opens with every record,
//! transaction row, blob record, sequence and namespace row of the
//! original, and with no delta. A second run changes nothing. Another
//! format is refused with `Format`. A crash during the tool leaves a store
//! of format 3 or of format 4, never a mix.
//!
//! The test makes a store of format 3 with the test hook
//! `zoe_core::testing::write_previous_format`, which removes the tables of
//! the deltas and writes the name of format 3.

use std::io::{Read, Write};

use zoe_core::testing::{self, commit, new_oid};
use zoe_core::vfs::MemVfs;
use zoe_core::{Flavour, Migrated, Oid, Options, Store, SystemClock, Tid, WriteLog, ZoeError};

const PICKLE: &[u8] =
    b"\x80\x03cpersistent.mapping\nPersistentMapping\nq\x00.\x80\x03}q\x01X\x04\x00\x00\x00datag\x01s.";

const OTHER: &[u8] =
    b"\x80\x03cpersistent.mapping\nPersistentMapping\nq\x00.\x80\x03}q\x01X\x05\x00\x00\x00otherg\x01s.";

/// What a store of format 3 holds before the move.
struct Original {
    records: Vec<(Oid, Tid, Vec<u8>)>,
    blob: (Oid, Tid),
    tids: Vec<Tid>,
    sequence: u64,
}

/// A write-ahead log that keeps its commits until the close.
fn held() -> Options {
    Options {
        write_log: WriteLog {
            fold_bytes: 1 << 40,
            fold_age: std::time::Duration::from_secs(3600),
            backlog_bytes: 1 << 41,
            ..WriteLog::default()
        },
        ..Options::default()
    }
}

/// Write records, a blob, a sequence and namespace rows into a new store in
/// `vfs`, make it a store of format 3 and close it.
fn format_3(vfs: &MemVfs) -> Original {
    let store = Store::open_full(vfs.clone(), SystemClock, held()).unwrap();
    let a = new_oid(&store);
    let b = new_oid(&store);
    let t1 = commit(&store, a, Tid::ZERO, PICKLE);
    let t2 = commit(&store, b, Tid::ZERO, OTHER);
    let mut file = store.new_blob_file().unwrap();
    file.write_all(b"the bytes of a blob").unwrap();
    let mut txn = store.tpc_begin(b"user", b"blob", b"").unwrap();
    txn.store_blob(b, t2, PICKLE, file).unwrap();
    txn.vote().unwrap();
    let t3 = txn.finish().unwrap();
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.kv_set(Flavour::Durable, "registry", b"title", b"site", None)
        .unwrap();
    txn.vote().unwrap();
    let t4 = txn.finish().unwrap();
    let sequence = store.sequence("intids").next().unwrap();
    testing::write_previous_format(&store).unwrap();
    // A commit after the change of the name goes through the log. The
    // close folds it.
    let t5 = commit(&store, a, t1, OTHER);
    assert!(store.backlog_bytes() > 0);
    drop(store);
    Original {
        records: vec![
            (a, t1, PICKLE.to_vec()),
            (b, t2, OTHER.to_vec()),
            (b, t3, PICKLE.to_vec()),
            (a, t5, OTHER.to_vec()),
        ],
        blob: (b, t3),
        tids: vec![t1, t2, t3, t4, t5],
        sequence,
    }
}

/// Check that `store` holds everything of `original` and no delta.
fn check(store: &Store, original: &Original) {
    for (oid, tid, data) in &original.records {
        let record = store.load_serial(*oid, *tid).unwrap();
        assert_eq!(&record, data);
    }
    let (oid, tid) = original.blob;
    let mut blob = Vec::new();
    store
        .open_blob(oid, tid)
        .unwrap()
        .read_to_end(&mut blob)
        .unwrap();
    assert_eq!(blob, b"the bytes of a blob");
    let tids: Vec<Tid> = store
        .iterator(None, None)
        .unwrap()
        .map(|txn| txn.unwrap().tid)
        .collect();
    assert_eq!(tids, original.tids);
    assert_eq!(
        store
            .kv_get(Flavour::Durable, "registry", b"title")
            .unwrap()
            .as_deref(),
        Some(&b"site"[..])
    );
    assert!(store.sequence("intids").next().unwrap() > original.sequence);
    assert_eq!(store.object_count(), Some(2));
    assert!(store.text_changed(Tid::ZERO, Tid::MAX).unwrap().is_empty());
    assert_eq!(
        store.text_before(Oid(1), "Title", 1, Tid::MAX).unwrap(),
        None
    );
    let checked = store.verify_text().unwrap();
    assert!(checked.findings.is_empty(), "{checked:?}");
    assert_eq!(checked.text_deltas + checked.clear_deltas, 0);
}

/// Tell if `error` is the refusal of a store of format 3.
fn refused_format_3(error: &ZoeError) -> bool {
    matches!(error, ZoeError::Format(text) if text.contains("ZOE format 3") && text.contains("zoe migrate"))
}

#[test]
fn a_store_of_format_3_is_refused_and_moves_with_the_tool() {
    let vfs = MemVfs::new();
    let original = format_3(&vfs);
    match Store::open_vfs(vfs.clone()) {
        Err(error) => assert!(refused_format_3(&error), "{error}"),
        Ok(_) => panic!("a store of format 3 opened"),
    }
    assert_eq!(
        Store::migrate_in(vfs.clone(), SystemClock).unwrap(),
        Migrated::Moved
    );
    let store = Store::open_vfs(vfs.clone()).unwrap();
    check(&store, &original);
    // The moved store takes deltas.
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.text_set(
        Oid(1),
        "Title",
        1,
        "en",
        "a title",
        Tid(store.last_transaction().0 + 1),
    )
    .unwrap();
    txn.vote().unwrap();
    txn.finish().unwrap();
    drop(store);

    // A second run changes nothing.
    assert_eq!(
        Store::migrate_in(vfs.clone(), SystemClock).unwrap(),
        Migrated::Current
    );
    let store = Store::open_vfs(vfs).unwrap();
    assert_eq!(
        store
            .text_before(Oid(1), "Title", 1, Tid::MAX)
            .unwrap()
            .map(|live| live.text),
        Some("a title".to_owned())
    );
}

#[test]
fn a_read_only_open_of_a_store_of_format_3_is_refused_and_names_the_tool() {
    let dir = testing::fresh_dir("format-3-read-only");
    {
        let store = Store::open(&dir).unwrap();
        let oid = new_oid(&store);
        commit(&store, oid, Tid::ZERO, PICKLE);
        testing::write_previous_format(&store).unwrap();
    }
    match Store::open_read_only_repairing(&dir, Options::default()) {
        Err(error) => assert!(refused_format_3(&error), "{error}"),
        Ok(_) => panic!("a store of format 3 opened"),
    }
    assert_eq!(Store::migrate(&dir).unwrap(), Migrated::Moved);
    assert_eq!(Store::migrate(&dir).unwrap(), Migrated::Current);
    let store = Store::open_read_only_repairing(&dir, Options::default()).unwrap();
    assert_eq!(store.object_count(), Some(1));
}

#[test]
fn another_format_is_refused_and_gets_no_write() {
    let vfs = MemVfs::new();
    {
        let store = Store::open_vfs(vfs.clone()).unwrap();
        let oid = new_oid(&store);
        commit(&store, oid, Tid::ZERO, PICKLE);
        testing::write_meta(&store, "format", b"ZOE format 2").unwrap();
    }
    match Store::migrate_in(vfs.clone(), SystemClock) {
        Err(ZoeError::Format(text)) => assert!(text.contains("ZOE format 2"), "{text}"),
        other => panic!("{other:?}"),
    }
    // The store keeps its name: a second run refuses it the same way.
    assert!(matches!(
        Store::migrate_in(vfs, SystemClock),
        Err(ZoeError::Format(_))
    ));
    // A directory without a store gets nothing.
    let dir = testing::fresh_dir("no-store");
    assert!(Store::migrate(&dir).is_err());
    assert!(!Store::exists(&dir));
}

#[test]
fn a_crash_during_the_tool_leaves_a_store_of_format_3_or_of_format_4() {
    let vfs = MemVfs::new();
    let original = format_3(&vfs);
    vfs.mark("migrating");
    assert_eq!(
        Store::migrate_in(vfs.clone(), SystemClock).unwrap(),
        Migrated::Moved
    );
    vfs.mark("moved");
    let images = vfs.crash_images();
    let (mut old, mut new) = (0, 0);
    for image in images.iter().filter(|image| image.is_after("migrating")) {
        match Store::open_vfs(image.vfs()) {
            Ok(store) => {
                new += 1;
                check(&store, &original);
            }
            Err(error) => {
                assert!(refused_format_3(&error), "{image:?}: {error}");
                assert!(!image.is_after("moved"), "{image:?}");
                old += 1;
                // The tool runs again on such a store.
                let moved = image.vfs();
                assert_eq!(
                    Store::migrate_in(moved.clone(), SystemClock).unwrap(),
                    Migrated::Moved,
                    "{image:?}"
                );
                check(&Store::open_vfs(moved).unwrap(), &original);
            }
        }
    }
    assert!(old > 0 && new > 0, "{old} {new}");
}

#[test]
fn a_store_of_format_3_whose_log_holds_commits_moves_with_the_tool() {
    let vfs = MemVfs::new();
    let store = Store::open_full(vfs.clone(), SystemClock, held()).unwrap();
    let a = new_oid(&store);
    let t1 = commit(&store, a, Tid::ZERO, PICKLE);
    testing::write_previous_format(&store).unwrap();
    let t2 = commit(&store, a, t1, OTHER);
    assert!(store.backlog_bytes() > 0);
    // A crash: the log holds the last commit, and no close folds it.
    let image = vfs.crash_now(|_| 0);
    drop(store);
    assert_eq!(
        Store::migrate_in(image.clone(), SystemClock).unwrap(),
        Migrated::Moved
    );
    let store = Store::open_vfs(image).unwrap();
    assert_eq!(store.load(a).unwrap(), (OTHER.to_vec(), t2));
    assert_eq!(store.load_serial(a, t1).unwrap(), PICKLE);
    let tids: Vec<Tid> = store
        .iterator(None, None)
        .unwrap()
        .map(|txn| txn.unwrap().tid)
        .collect();
    assert_eq!(tids, [t1, t2]);
    let checked = store.verify_text().unwrap();
    assert!(checked.findings.is_empty(), "{checked:?}");
}
