//! Acceptance test of the text deltas in `zoe backup`, `zoe restore` and
//! `zoe verify --backup` (ADR 0060, section 6; N15).
//!
//! A dump of version 3 carries the deltas of its transactions after their
//! records: `X` a text delta, `Y` an unindex, `C` a clear delta. A restore
//! writes each delta under its tid, through a full dump and incremental
//! dumps, also for a transaction of deltas only. After a pack of the
//! source, the restore keeps more history than the source, and `zoe verify
//! --backup` finds no difference. It finds a delta that differs, and names
//! it without its text. `zoe backup` refuses a directory with a dump of
//! version 2, and `zoe restore` refuses such a dump. A damaged dump fails
//! the restore before anything is restored.

use std::path::{Path, PathBuf};

use zoe_core::testing;
use zoe_core::{Oid, Store, TextChange, TextDelta, Tid};

mod common;

use common::{assert_exit, fresh_dir, open_store, s, zoe};

const ROOT: Oid = Oid(0);
const CATALOG: Oid = Oid(1);
const INDEX: &str = "SearchableText";
const TITLE: &str = "Title";

/// The words of each text. No output may hold them.
const SECRET: &str = "confidential-words";

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

fn snapshot(store: &Store) -> Tid {
    Tid(store.last_transaction().0 + 1)
}

/// Commit the root and the catalog, with the texts of rids 1 and 2.
fn create(store: &Store) -> Tid {
    let before = snapshot(store);
    let mut txn = store.tpc_begin(b"user", b"create", b"").unwrap();
    txn.store(ROOT, Tid::ZERO, &record(&[CATALOG])).unwrap();
    txn.store(CATALOG, Tid::ZERO, &record(&[])).unwrap();
    for rid in [1, 2] {
        let text = format!("{SECRET} {rid}");
        txn.text_set(CATALOG, INDEX, rid, "en", &text, before)
            .unwrap();
    }
    txn.text_set(CATALOG, TITLE, 1, "", "a title", before)
        .unwrap();
    txn.vote().unwrap();
    txn.finish().unwrap()
}

/// Commit a transaction of deltas only: a new text of rid 1 and an
/// unindex of rid 2.
fn deltas_only(store: &Store, n: u32) -> Tid {
    let before = snapshot(store);
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    let text = format!("{SECRET} again {n}");
    txn.text_set(CATALOG, INDEX, 1, "en", &text, before)
        .unwrap();
    txn.text_unindex(CATALOG, INDEX, 2, before).unwrap();
    txn.vote().unwrap();
    txn.finish().unwrap()
}

/// Commit a clear delta of the title index with a text after it, and an
/// edit of the catalog.
fn clear(store: &Store) -> Tid {
    let before = snapshot(store);
    let serial = store.load(CATALOG).unwrap().1;
    let mut txn = store.tpc_begin(b"", b"clear", b"").unwrap();
    txn.store(CATALOG, serial, &record(&[])).unwrap();
    txn.text_clear(CATALOG, TITLE).unwrap();
    txn.text_set(CATALOG, TITLE, 3, "", "a new title", before)
        .unwrap();
    txn.vote().unwrap();
    txn.finish().unwrap()
}

/// Every delta of `store`, with its tid.
fn deltas(store: &Store) -> Vec<(Tid, TextDelta)> {
    let mut all = Vec::new();
    for txn in store.iterator(None, None).unwrap() {
        let tid = txn.unwrap().tid;
        for delta in store.text_deltas_at(tid).unwrap() {
            all.push((tid, delta));
        }
    }
    all
}

/// The key of every delta of `store`.
fn keys(store: &Store) -> Vec<TextChange> {
    store.text_keys(None, usize::MAX).unwrap()
}

fn backup(source: &Path, dumps: &Path, what: &str) {
    assert_exit(
        &zoe(&["backup", "--path", s(source), "--to", s(dumps)], &[]),
        0,
        what,
    );
}

fn restore(dumps: &Path, target: &Path) -> std::process::Output {
    zoe(&["restore", "--from", s(dumps), "--path", s(target)], &[])
}

fn verify(source: &Path, dumps: &Path) -> std::process::Output {
    zoe(&["verify", "--path", s(source), "--backup", s(dumps)], &[])
}

/// The text of both streams of `output`.
fn text(output: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// The dump files of `dir`, in order.
fn dump_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "zoedump"))
        .collect();
    files.sort();
    files
}

/// Give the dump `path` the version byte 2. The header comes first: the
/// commands read it before the hash of the file.
fn make_version_2(path: &Path) {
    let mut bytes = std::fs::read(path).unwrap();
    bytes[7] = 2;
    std::fs::write(path, bytes).unwrap();
}

#[test]
fn a_restore_gives_the_same_deltas_under_the_same_tids() {
    let dir = fresh_dir("deltas");
    let (source, dumps, target) = (dir.join("source"), dir.join("dumps"), dir.join("target"));
    {
        let store = open_store(&source);
        create(&store);
        deltas_only(&store, 1);
    }
    backup(&source, &dumps, "full");
    {
        let store = open_store(&source);
        clear(&store);
        deltas_only(&store, 2);
    }
    backup(&source, &dumps, "incremental");
    assert_eq!(dump_files(&dumps).len(), 2);

    let output = restore(&dumps, &target);
    assert_exit(&output, 0, "restore");
    assert!(!text(&output).contains(SECRET), "{}", text(&output));
    let source = open_store(&source);
    let target = open_store(&target);
    let ours = deltas(&source);
    assert!(
        ours.iter().any(|(_, delta)| delta.rid.is_none()),
        "{ours:?}"
    );
    assert!(
        ours.iter()
            .any(|(_, delta)| delta.text.is_none() && delta.rid.is_some())
    );
    assert_eq!(ours, deltas(&target));
    assert_eq!(keys(&source), keys(&target));
    let tids = |store: &Store| -> Vec<Tid> {
        store
            .iterator(None, None)
            .unwrap()
            .map(|txn| txn.unwrap().tid)
            .collect()
    };
    assert_eq!(tids(&source), tids(&target));
    for rid in [1, 2, 3] {
        for index in [INDEX, TITLE] {
            assert_eq!(
                source.text_before(CATALOG, index, rid, Tid::MAX).unwrap(),
                target.text_before(CATALOG, index, rid, Tid::MAX).unwrap()
            );
        }
    }
    let checked = target.verify_text().unwrap();
    assert!(checked.findings.is_empty(), "{checked:?}");
}

#[test]
fn after_a_pack_the_restore_keeps_more_history_and_verify_finds_no_difference() {
    let dir = fresh_dir("packed");
    let (source, dumps, target) = (dir.join("source"), dir.join("dumps"), dir.join("target"));
    let last = {
        let store = open_store(&source);
        create(&store);
        deltas_only(&store, 1);
        clear(&store);
        deltas_only(&store, 2)
    };
    backup(&source, &dumps, "full");
    {
        let store = open_store(&source);
        let report = store.pack(last).unwrap();
        assert!(
            report.text_deltas > 0 && report.clear_deltas > 0,
            "{report:?}"
        );
    }
    let output = verify(&source, &dumps);
    assert_exit(&output, 0, "verify after the pack");
    assert!(!text(&output).contains(SECRET), "{}", text(&output));

    // A later dump continues the chain from the tid of the pack.
    {
        let store = open_store(&source);
        deltas_only(&store, 3);
    }
    backup(&source, &dumps, "incremental after the pack");
    assert_exit(&verify(&source, &dumps), 0, "verify of both dumps");

    assert_exit(&restore(&dumps, &target), 0, "restore");
    let source = open_store(&source);
    let target = open_store(&target);
    assert!(keys(&target).len() > keys(&source).len());
    for rid in [1, 2, 3] {
        for index in [INDEX, TITLE] {
            assert_eq!(
                source.text_before(CATALOG, index, rid, Tid::MAX).unwrap(),
                target.text_before(CATALOG, index, rid, Tid::MAX).unwrap(),
                "{index} {rid}"
            );
        }
    }
}

#[test]
fn verify_finds_a_delta_that_differs_and_names_it_without_its_text() {
    let dir = fresh_dir("differs");
    let (source, dumps) = (dir.join("source"), dir.join("dumps"));
    let tid = {
        let store = open_store(&source);
        create(&store);
        deltas_only(&store, 1)
    };
    backup(&source, &dumps, "backup");
    assert_exit(&verify(&source, &dumps), 0, "fresh dumps");
    {
        let store = open_store(&source);
        let key = testing::text_delta_key(CATALOG, INDEX, Some(1), tid);
        let frame = testing::read_text_frame(&store, &key).unwrap();
        let prev = Tid(u64::from_le_bytes(frame[8..16].try_into().unwrap()));
        let other = testing::text_payload("en", &format!("{SECRET} changed"));
        let frame = testing::text_frame(&key, prev, &other);
        testing::write_text_frame(&store, &key, Some(&frame)).unwrap();
    }
    let output = verify(&source, &dumps);
    assert_exit(&output, 1, "a delta that differs");
    let all = text(&output);
    assert!(all.contains("the delta differs"), "{all}");
    assert!(all.contains("rid 1"), "{all}");
    assert!(!all.contains(SECRET), "the output holds a text:\n{all}");
}

#[test]
fn a_dump_of_version_2_is_refused_by_backup_and_by_restore() {
    let dir = fresh_dir("version-2");
    let (source, dumps, target) = (dir.join("source"), dir.join("dumps"), dir.join("target"));
    {
        let store = open_store(&source);
        create(&store);
    }
    backup(&source, &dumps, "full");
    let files = dump_files(&dumps);
    make_version_2(&files[0]);
    {
        let store = open_store(&source);
        deltas_only(&store, 1);
    }
    let output = zoe(&["backup", "--path", s(&source), "--to", s(&dumps)], &[]);
    assert_exit(&output, 1, "a backup into a directory of version 2");
    let all = text(&output);
    assert!(
        all.contains("version 2") && all.contains("new full dump"),
        "{all}"
    );
    assert_eq!(dump_files(&dumps), files, "the backup wrote a dump");

    let output = restore(&dumps, &target);
    assert_exit(&output, 1, "a restore of a dump of version 2");
    assert!(text(&output).contains("version 2"), "{}", text(&output));
    assert!(!target.exists());
}

#[test]
fn a_damaged_dump_fails_before_anything_is_restored() {
    let dir = fresh_dir("damaged");
    let (source, dumps, target) = (dir.join("source"), dir.join("dumps"), dir.join("target"));
    {
        let store = open_store(&source);
        create(&store);
    }
    backup(&source, &dumps, "full");
    {
        let store = open_store(&source);
        deltas_only(&store, 1);
        clear(&store);
    }
    backup(&source, &dumps, "incremental");
    // A damaged delta in the last dump: the full dump is not restored
    // either.
    let files = dump_files(&dumps);
    let last = files.last().unwrap();
    let mut bytes = std::fs::read(last).unwrap();
    let at = bytes
        .windows(SECRET.len())
        .position(|window| window == SECRET.as_bytes())
        .unwrap();
    bytes[at] ^= 0x01;
    std::fs::write(last, bytes).unwrap();
    assert_exit(&restore(&dumps, &target), 1, "a damaged dump");
    assert!(!target.exists());
    assert!(!dir.join("target.restore").exists());
}

/// A catalog that the root does not reach.
const ORPHAN: Oid = Oid(2);

/// Commit an edit of the catalog record. Return the tid.
fn edit(store: &Store) -> Tid {
    let serial = store.load(CATALOG).unwrap().1;
    let mut txn = store.tpc_begin(b"", b"edit", b"").unwrap();
    txn.store(CATALOG, serial, &record(&[])).unwrap();
    txn.vote().unwrap();
    txn.finish().unwrap()
}

#[test]
fn the_texts_of_a_catalog_that_a_pack_removed_are_no_difference() {
    let dir = fresh_dir("orphan");
    let (source, dumps) = (dir.join("source"), dir.join("dumps"));
    let last = {
        let store = open_store(&source);
        create(&store);
        let before = snapshot(&store);
        let mut txn = store.tpc_begin(b"", b"orphan", b"").unwrap();
        txn.store(ORPHAN, Tid::ZERO, &record(&[])).unwrap();
        txn.text_set(ORPHAN, INDEX, 1, "en", &format!("{SECRET} orphan"), before)
            .unwrap();
        txn.vote().unwrap();
        txn.finish().unwrap();
        edit(&store)
    };
    backup(&source, &dumps, "full");
    {
        // The garbage collection removes the orphan catalog and its texts.
        let store = open_store(&source);
        store.pack(last).unwrap();
        assert!(store.load(ORPHAN).is_err());
        assert_eq!(store.text_before(ORPHAN, INDEX, 1, Tid::MAX).unwrap(), None);
    }
    let output = verify(&source, &dumps);
    assert_exit(&output, 0, "verify after the pack of an orphan catalog");
    assert!(!text(&output).contains(SECRET), "{}", text(&output));
}

#[test]
fn a_lost_live_text_of_a_catalog_that_is_still_there_is_a_difference() {
    let dir = fresh_dir("lost");
    let (source, dumps) = (dir.join("source"), dir.join("dumps"));
    let (first, last) = {
        let store = open_store(&source);
        let first = create(&store);
        (first, edit(&store))
    };
    backup(&source, &dumps, "full");
    {
        // The pack keeps the text of rid 1: the test then removes it, with
        // its key in text_txn, as a pack that lost it would.
        let store = open_store(&source);
        store.pack(last).unwrap();
        let key = testing::text_delta_key(CATALOG, INDEX, Some(1), first);
        testing::read_text_frame(&store, &key).unwrap();
        testing::write_text_frame(&store, &key, None).unwrap();
        testing::write_text_txn_key(&store, &testing::text_txn_key(&key), false).unwrap();
        assert!(store.load(CATALOG).is_ok());
    }
    let output = verify(&source, &dumps);
    assert_exit(&output, 1, "a lost live text");
    let all = text(&output);
    assert!(
        all.contains("the live text") && all.contains("rid 1"),
        "{all}"
    );
    assert!(!all.contains(SECRET), "the output holds a text:\n{all}");
}
