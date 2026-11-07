//! Acceptance test of pack with the text deltas (ADR 0060, section 4; F8,
//! F18, F26).
//!
//! A pack at T keeps every delta after T, so `text_before` at each tid
//! after T gives the live text of before the pack, for each catalog that a
//! reader can reach. A text delta at or
//! before T that a clear delta at or before T hides goes, with that clear
//! delta and every clear delta of the index before it. Of the other text
//! deltas at or before T of one key, the newest stays and the others go,
//! and that newest one goes too when it is an unindex. With `pack-gc` true,
//! the deltas at or before T of a catalog that the mark does not reach go
//! too. A transaction row stays while it keeps a record or a delta. A dry
//! run counts the deltas that the pack removes. A crash between two
//! batches never brings a dead text delta back. A delta that fails its
//! checksum stops the pack with `CorruptText`. A cap holds the pack at its
//! tid.

use zoe_core::testing;
use zoe_core::vfs::MemVfs;
use zoe_core::{LiveText, Oid, Options, PackReport, Store, Tid, ZoeError};

const ROOT: Oid = Oid(0);
/// The catalog that the root reaches.
const CATALOG: Oid = Oid(1);
/// A catalog that no record reaches.
const ORPHAN: Oid = Oid(9);
const TEXT: &str = "SearchableText";
const TITLE: &str = "Title";

/// Return a record of class `m.C` whose state is a dict that holds a
/// reference to each oid of `refs`.
fn record(refs: &[Oid]) -> Vec<u8> {
    let mut p = b"\x80\x03cm\nC\nq\x00.\x80\x03}".to_vec();
    if !refs.is_empty() {
        p.push(b'(');
        for (i, oid) in refs.iter().enumerate() {
            p.extend_from_slice(b"K");
            p.push(u8::try_from(i).unwrap());
            p.extend_from_slice(b"C\x08");
            p.extend_from_slice(&oid.0.to_be_bytes());
            p.extend_from_slice(b"h\x00\x86Q");
        }
        p.push(b'u');
    }
    p.push(b'.');
    p
}

/// One write of a transaction of the scenario.
enum Write {
    /// A record of the oid, with references to the oids.
    Record(Oid, &'static [Oid]),
    /// A text delta: the catalog, the index, the rid and the text.
    Text(Oid, &'static str, i32, &'static str),
    /// An unindex: the catalog, the index and the rid.
    Unindex(Oid, &'static str, i32),
    /// A clear delta: the catalog and the index.
    Clear(Oid, &'static str),
}

/// Commit `writes` in one transaction. Return the tid.
fn commit(store: &Store, writes: &[Write]) -> Tid {
    let before = Tid(store.last_transaction().0 + 1);
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    for write in writes {
        match *write {
            Write::Record(oid, refs) => {
                let serial = store.load(oid).map_or(Tid::ZERO, |(_, serial)| serial);
                txn.store(oid, serial, &record(refs)).unwrap();
            }
            Write::Text(catalog, index, rid, text) => txn
                .text_set(catalog, index, rid, "en", text, before)
                .unwrap(),
            Write::Unindex(catalog, index, rid) => {
                txn.text_unindex(catalog, index, rid, before).unwrap()
            }
            Write::Clear(catalog, index) => txn.text_clear(catalog, index).unwrap(),
        }
    }
    txn.vote().unwrap();
    txn.finish().unwrap()
}

/// The tids of the scenario.
#[derive(Clone, Copy, Debug)]
struct Tids {
    /// The root, the catalog, and the first texts of both catalogs.
    t1: Tid,
    /// Only a delta: a new text of rid 1.
    t2: Tid,
    /// Only a delta: an unindex of rid 2.
    t3: Tid,
    /// A clear of `TITLE`, then a text of rid 2 of `TITLE`.
    t4: Tid,
    /// An edit of the catalog with a text of rid 3. A later edit replaces
    /// its record.
    t5: Tid,
    /// The later edit of the catalog, and a second text of the orphan.
    t6: Tid,
    /// The pack tid T: a text of rid 4.
    t7: Tid,
    /// After T: a new text of rid 1.
    t8: Tid,
    /// After T: an unindex of rid 3.
    t9: Tid,
}

/// Commit the scenario into `store`.
fn scenario(store: &Store) -> Tids {
    use Write::*;
    let t1 = commit(
        store,
        &[
            Record(ROOT, &[CATALOG]),
            Record(CATALOG, &[]),
            Text(CATALOG, TEXT, 1, "one a"),
            Text(CATALOG, TEXT, 2, "two a"),
            Text(CATALOG, TITLE, 1, "title a"),
            Text(ORPHAN, TEXT, 1, "orphan a"),
        ],
    );
    let t2 = commit(store, &[Text(CATALOG, TEXT, 1, "one b")]);
    let t3 = commit(store, &[Unindex(CATALOG, TEXT, 2)]);
    let t4 = commit(
        store,
        &[Clear(CATALOG, TITLE), Text(CATALOG, TITLE, 2, "title two")],
    );
    let t5 = commit(
        store,
        &[Record(CATALOG, &[]), Text(CATALOG, TEXT, 3, "three a")],
    );
    let t6 = commit(
        store,
        &[Record(CATALOG, &[]), Text(ORPHAN, TEXT, 2, "orphan b")],
    );
    let t7 = commit(store, &[Text(CATALOG, TEXT, 4, "four a")]);
    let t8 = commit(store, &[Text(CATALOG, TEXT, 1, "one c")]);
    let t9 = commit(store, &[Unindex(CATALOG, TEXT, 3)]);
    Tids {
        t1,
        t2,
        t3,
        t4,
        t5,
        t6,
        t7,
        t8,
        t9,
    }
}

/// Every key of the scenario: the catalog, the index and the rid.
const KEYS: [(Oid, &str, i32); 8] = [
    (CATALOG, TEXT, 1),
    (CATALOG, TEXT, 2),
    (CATALOG, TEXT, 3),
    (CATALOG, TEXT, 4),
    (CATALOG, TITLE, 1),
    (CATALOG, TITLE, 2),
    (ORPHAN, TEXT, 1),
    (ORPHAN, TEXT, 2),
];

/// The snapshots after T of the scenario.
fn after_t(tids: Tids) -> Vec<Tid> {
    let next = |tid: Tid| Tid(tid.0 + 1);
    vec![
        next(tids.t7),
        tids.t8,
        next(tids.t8),
        tids.t9,
        next(tids.t9),
        Tid::MAX,
    ]
}

/// The live text of each key at each snapshot of `snapshots`. With garbage
/// collection, only the keys of the catalog that the mark reaches count:
/// rule 5 removes the deltas of the other one.
fn live_texts(store: &Store, gc: bool, snapshots: &[Tid]) -> Vec<Option<LiveText>> {
    let mut texts = Vec::new();
    let keys = KEYS
        .iter()
        .filter(|(catalog, ..)| !gc || *catalog != ORPHAN);
    for &(catalog, index, rid) in keys {
        for &tid in snapshots {
            texts.push(store.text_before(catalog, index, rid, tid).unwrap());
        }
    }
    texts
}

/// Tell if `store` holds the delta of `rid` (or the clear delta, for
/// `None`) of `index` of `catalog` at `tid`.
fn has(store: &Store, catalog: Oid, index: &str, rid: Option<i32>, tid: Tid) -> bool {
    let key = testing::text_delta_key(catalog, index, rid, tid);
    testing::read_text_frame(store, &key).is_ok()
}

/// Return the tids of the transaction rows of `store`.
fn rows(store: &Store) -> Vec<Tid> {
    store
        .iterator(None, None)
        .unwrap()
        .map(|txn| txn.unwrap().tid)
        .collect()
}

fn open(gc: bool) -> Store {
    let options = Options {
        pack_gc: gc,
        ..Options::default()
    };
    Store::open_full(MemVfs::new(), zoe_core::SystemClock, options).unwrap()
}

#[test]
fn a_pack_keeps_the_live_text_of_every_snapshot_after_its_tid() {
    for gc in [true, false] {
        let store = open(gc);
        let tids = scenario(&store);
        let snapshots = after_t(tids);
        let expected = live_texts(&store, gc, &snapshots);
        store.pack(tids.t7).unwrap();
        assert_eq!(live_texts(&store, gc, &snapshots), expected, "gc {gc}");
        let check = store.verify_text().unwrap();
        assert!(check.findings.is_empty(), "gc {gc}: {:?}", check.findings);
    }
}

#[test]
fn the_rules_of_the_deltas_remove_what_they_name() {
    for gc in [true, false] {
        let store = open(gc);
        let t = scenario(&store);
        let report = store.pack(t.t7).unwrap();
        let what = format!("gc {gc}");

        // Rule 1: every delta after T stays.
        assert!(has(&store, CATALOG, TEXT, Some(1), t.t8), "{what}");
        assert!(has(&store, CATALOG, TEXT, Some(3), t.t9), "{what}");
        // Rule 2: the clear delta hides the text of rid 1 of the title:
        // both go. The text of rid 2 at the tid of the clear is live, and
        // stays.
        assert!(!has(&store, CATALOG, TITLE, Some(1), t.t1), "{what}");
        assert!(!has(&store, CATALOG, TITLE, None, t.t4), "{what}");
        assert!(has(&store, CATALOG, TITLE, Some(2), t.t4), "{what}");
        // Rule 3: the newest text at or before T of a key stays, the older
        // ones go.
        assert!(!has(&store, CATALOG, TEXT, Some(1), t.t1), "{what}");
        assert!(has(&store, CATALOG, TEXT, Some(1), t.t2), "{what}");
        assert!(has(&store, CATALOG, TEXT, Some(3), t.t5), "{what}");
        assert!(has(&store, CATALOG, TEXT, Some(4), t.t7), "{what}");
        // Rule 4: a newest unindex goes too.
        assert!(!has(&store, CATALOG, TEXT, Some(2), t.t1), "{what}");
        assert!(!has(&store, CATALOG, TEXT, Some(2), t.t3), "{what}");
        // Rule 5: with garbage collection, the deltas of a catalog that the
        // mark does not reach go. Without it, rules 1 to 4 apply.
        assert_eq!(has(&store, ORPHAN, TEXT, Some(1), t.t1), !gc, "{what}");
        assert_eq!(has(&store, ORPHAN, TEXT, Some(2), t.t6), !gc, "{what}");

        let removed_texts = if gc { 6 } else { 4 };
        assert_eq!(report.text_deltas, removed_texts, "{what}: {report:?}");
        assert_eq!(report.clear_deltas, 1, "{what}: {report:?}");
    }
}

#[test]
fn a_row_stays_while_it_keeps_a_record_or_a_delta() {
    for gc in [true, false] {
        let store = open(gc);
        let t = scenario(&store);
        store.pack(t.t7).unwrap();
        let rows = rows(&store);
        let what = format!("gc {gc}: {rows:?}");
        // A transaction of deltas only keeps its row while a delta of it
        // stays.
        assert!(rows.contains(&t.t2), "{what}");
        // Its unindex went, and it kept nothing: the row goes.
        assert!(!rows.contains(&t.t3), "{what}");
        // The clear delta went, and the text of the same tid stays.
        assert!(rows.contains(&t.t4), "{what}");
        // The pack removed the record of the edit, and its delta stays.
        assert!(matches!(
            store.load_serial(CATALOG, t.t5),
            Err(ZoeError::NotFound { .. })
        ));
        assert!(rows.contains(&t.t5), "{what}");
        let check = store.verify_text().unwrap();
        assert!(check.findings.is_empty(), "{what}: {:?}", check.findings);
    }
}

#[test]
fn the_dry_run_counts_the_deltas_that_the_pack_removes() {
    for gc in [true, false] {
        let store = open(gc);
        let t = scenario(&store);
        let dry = store.pack_dry_run(t.t7).unwrap();
        // The dry run removes nothing.
        assert!(has(&store, CATALOG, TEXT, Some(1), t.t1));
        assert!(has(&store, CATALOG, TITLE, None, t.t4));
        let packed = store.pack(t.t7).unwrap();
        assert_eq!(dry, packed, "gc {gc}");
        assert!(dry.text_deltas > 0 && dry.clear_deltas == 1, "{dry:?}");
    }
}

#[test]
fn a_crash_at_each_batch_brings_no_dead_text_back_and_changes_no_live_text() {
    for gc in [true, false] {
        let built = MemVfs::new();
        let (tids, snapshots, expected) = {
            let store = Store::open_full(
                built.clone(),
                zoe_core::SystemClock,
                Options {
                    pack_gc: gc,
                    ..Options::default()
                },
            )
            .unwrap();
            let tids = scenario(&store);
            let snapshots = after_t(tids);
            let expected = live_texts(&store, gc, &snapshots);
            (tids, snapshots, expected)
        };
        let vfs = built.durable_copy();
        let options = || Options {
            pack_gc: gc,
            ..Options::default()
        };
        {
            let store = Store::open_full(vfs.clone(), zoe_core::SystemClock, options()).unwrap();
            store.pack_in_batches(tids.t7, 1, || {}).unwrap();
            vfs.mark("packed");
        }
        let images = vfs.crash_images();
        assert!(images.len() > 1, "the simulation produced crash images");
        for image in &images {
            let store = Store::open_full(image.vfs(), zoe_core::SystemClock, options())
                .unwrap_or_else(|e| panic!("{image:?} does not open: {e}"));
            assert_eq!(
                live_texts(&store, gc, &snapshots),
                expected,
                "gc {gc}: {image:?} changed a live text"
            );
            // The clear delta goes only after the dead text that it hides.
            if !has(&store, CATALOG, TITLE, None, tids.t4) {
                assert!(
                    !has(&store, CATALOG, TITLE, Some(1), tids.t1),
                    "gc {gc}: {image:?} brought a dead text back"
                );
            }
            if image.is_after("packed") {
                assert!(!has(&store, CATALOG, TEXT, Some(1), tids.t1), "{image:?}");
                assert!(!has(&store, CATALOG, TITLE, None, tids.t4), "{image:?}");
            }
        }
    }
}

#[test]
fn a_delta_that_fails_its_checksum_stops_the_pack() {
    for gc in [true, false] {
        let store = open(gc);
        let t = scenario(&store);
        let key = testing::text_delta_key(CATALOG, TEXT, Some(4), t.t7);
        let mut frame = testing::read_text_frame(&store, &key).unwrap();
        let last = frame.len() - 1;
        frame[last] ^= 0x01;
        testing::write_text_frame(&store, &key, Some(&frame)).unwrap();
        match store.pack(t.t7) {
            Err(ZoeError::CorruptText {
                catalog, rid, tid, ..
            }) => {
                assert_eq!((catalog, rid, tid), (CATALOG, Some(4), t.t7), "gc {gc}");
            }
            other => panic!("gc {gc}: {other:?}"),
        }
        // The pack removed nothing: the evidence stays.
        assert_eq!(testing::read_text_frame(&store, &key).unwrap(), frame);
        assert!(has(&store, CATALOG, TEXT, Some(1), t.t1), "gc {gc}");
        assert!(has(&store, CATALOG, TITLE, None, t.t4), "gc {gc}");
    }
}

#[test]
fn a_cap_holds_the_pack_at_its_tid() {
    let store = open(true);
    let t = scenario(&store);
    // The holder applied the deltas up to t2, not the unindex of t3.
    let cap = store.pack_cap(t.t2);
    assert_eq!(cap.tid(), t.t2);
    let report = store.pack(t.t7).unwrap();
    assert_eq!(store.pack_watermark().unwrap(), t.t2);
    assert!(has(&store, CATALOG, TEXT, Some(2), t.t3));
    assert!(!has(&store, CATALOG, TEXT, Some(1), t.t1), "{report:?}");
    // A dry run packs at the cap too.
    assert_eq!(store.pack_dry_run(t.t7).unwrap(), PackReport::default());

    // The holder moves the cap forward. A tid before it changes nothing.
    cap.advance(t.t4);
    cap.advance(t.t1);
    assert_eq!(cap.tid(), t.t4);
    store.pack(t.t7).unwrap();
    assert_eq!(store.pack_watermark().unwrap(), t.t4);
    assert!(!has(&store, CATALOG, TEXT, Some(2), t.t3));
    assert!(has(&store, CATALOG, TEXT, Some(3), t.t5));

    // The drop releases it.
    drop(cap);
    store.pack(t.t7).unwrap();
    assert_eq!(store.pack_watermark().unwrap(), t.t7);
}
