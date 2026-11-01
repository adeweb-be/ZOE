//! The unit tests of the backups, the restores and the checks of a
//! backup.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use zoe_core::{DurableRow, Oid, Store, Tid, TxnInfo, ZoeError};

use super::backup::{TEMP_SUFFIX, lock_dir};
use super::chain::{dump_name, parse_name};
use super::check::CHECK_PREFIX;
use super::dump::{HASH, HEADER};
use super::restore::{DATABASE, WORK_SUFFIX};
use super::*;
use crate::filestorage::tests::test_dir;
use crate::fsutil::sibling;

/// Compare the dumps in `dumps` with `store`, through a copy in
/// `scratch`.
fn check_dumps(store: &Store, dumps: &Path, scratch: &Path) -> Result<BackupCheck, Error> {
    verify_backup(dumps, scratch, || Ok::<_, Error>(store))
}

/// The record of a blob.
const BLOB_RECORD: &[u8] = b"\x80\x03cZODB.blob\nBlob\nq\x00.\x80\x03N.";

/// A record of a `PersistentMapping` that holds `value`, with no
/// reference.
fn pickle(value: &[u8]) -> Vec<u8> {
    let mut pickle = b"\x80\x03cpersistent.mapping\nPersistentMapping\nq\x00.".to_vec();
    pickle.extend_from_slice(b"\x80\x03}q\x00X\x01\x00\x00\x00vq\x01C");
    pickle.push(u8::try_from(value.len()).unwrap());
    pickle.extend_from_slice(value);
    pickle.extend_from_slice(b"q\x02s.");
    pickle
}

/// A write of a restore transaction: a pickle, a deletion or a blob.
enum W<'a> {
    Data(u64, &'a [u8]),
    Delete(u64),
    Blob(u64, &'a [u8]),
}

/// Commit a restore transaction at `tid` with `writes`.
fn commit(store: &Store, tid: u64, writes: &[W<'_>]) {
    let description = format!("at {tid}");
    let mut txn = store
        .tpc_begin_restore(Tid(tid), b"user", description.as_bytes(), b"ext")
        .unwrap();
    for write in writes {
        match write {
            W::Data(oid, data) => txn.restore(Oid(*oid), Some(data)).unwrap(),
            W::Delete(oid) => txn.restore(Oid(*oid), None).unwrap(),
            W::Blob(oid, bytes) => {
                let mut file = store.new_blob_file().unwrap();
                file.write_all(bytes).unwrap();
                txn.restore_blob(Oid(*oid), BLOB_RECORD, file).unwrap();
            }
        }
    }
    txn.vote().unwrap();
    assert_eq!(txn.finish().unwrap(), Tid(tid));
}

/// A store in `dir` with two transactions, a deletion, a blob and a
/// sequence.
fn sample(dir: &Path) -> Store {
    let store = Store::open(dir).unwrap();
    commit(&store, 100, &[W::Data(1, b"one"), W::Data(2, b"two")]);
    commit(&store, 200, &[W::Delete(2), W::Blob(3, &[7; 70_000])]);
    for _ in 0..3 {
        store.sequence("intids").next().unwrap();
    }
    store
}

fn transactions(store: &Store) -> Vec<TxnInfo> {
    store
        .iterator(None, None)
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn blob(store: &Store, oid: u64, tid: u64) -> Vec<u8> {
    let mut bytes = Vec::new();
    store
        .open_blob(Oid(oid), Tid(tid))
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    bytes
}

/// The dump files of `dir`, in name order.
fn dump_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    files.sort();
    files
}

/// A write of a namespace: a flavour, a name, a key and a value or
/// `None` for a delete.
type KvWrite<'a> = (zoe_core::Flavour, &'a str, &'a [u8], Option<&'a [u8]>);

/// Commit `writes` of the namespaces in a normal transaction. Return
/// the tid.
fn commit_kv(store: &Store, writes: &[KvWrite<'_>]) -> Tid {
    use zoe_core::Flavour;
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    for &(flavour, name, key, value) in writes {
        match value {
            Some(value) => {
                let ttl =
                    (flavour == Flavour::Ephemeral).then(|| std::time::Duration::from_secs(3600));
                txn.kv_set(flavour, name, key, value, ttl).unwrap();
            }
            None => txn.kv_delete(flavour, name, key).unwrap(),
        }
    }
    txn.vote().unwrap();
    txn.finish().unwrap()
}

#[test]
fn a_dump_holds_every_durable_row_and_the_restore_writes_those_of_the_last_dump() {
    use zoe_core::Flavour::{Durable, Ephemeral};
    let dir = test_dir("backup-durable");
    let (source, dumps, target) = (dir.join("store"), dir.join("dumps"), dir.join("target"));
    let store = sample(&source);
    let first = commit_kv(
        &store,
        &[
            (Durable, "registry", b"a", Some(b"1")),
            (Durable, "registry", b"b", Some(b"2")),
            (Ephemeral, "sessions", b"s", Some(b"secret")),
        ],
    );
    let full = backup(&store, &dumps).unwrap();
    // Two keys and the generation row; the ephemeral row stays out.
    assert_eq!(full.durable_rows, 3);
    let second = commit_kv(
        &store,
        &[
            (Durable, "registry", b"a", None),
            (Durable, "registry", b"c", Some(b"3")),
            (Durable, "other", b"x", Some(b"4")),
        ],
    );
    let incremental = backup(&store, &dumps).unwrap();
    assert_eq!(incremental.durable_rows, 5);
    // A backup with nothing new writes nothing.
    assert_eq!(backup(&store, &dumps).unwrap().dump, None);
    assert!(
        check_dumps(&store, &dumps, &dir)
            .unwrap()
            .differences
            .is_empty()
    );
    drop(store);

    let restored = restore(&dumps, &target).unwrap();
    assert_eq!(restored.durable_rows, 5);
    let copy = Store::open(&target).unwrap();
    let get = |name, key: &[u8]| copy.kv_get(Durable, name, key).unwrap();
    assert_eq!(get("registry", b"a"), None);
    assert_eq!(get("registry", b"b").as_deref(), Some(&b"2"[..]));
    assert_eq!(get("registry", b"c").as_deref(), Some(&b"3"[..]));
    assert_eq!(get("other", b"x").as_deref(), Some(&b"4"[..]));
    assert_eq!(copy.kv_generation("registry").unwrap(), second);
    assert!(first < second);
    assert_eq!(copy.kv_get(Ephemeral, "sessions", b"s").unwrap(), None);
    // The rows of the restored store verify: no generation after its
    // last tid. The pickles of the sample are not real pickles.
    let report = crate::verify(&copy, None);
    assert!(report.checksums.is_empty(), "{report:?}");
    assert_eq!(report.durable_keys, 3);
    drop(copy);
    let _ = fs::remove_dir_all(&dir);
}

/// A store in `dir` whose last commit writes a namespace only, then
/// packed at that commit, as `zoe pack --days 0` packs. The root holds a
/// real pickle, so the pack can read its references.
fn packed_with_an_empty_tail(dir: &Path) -> (Store, Tid) {
    use zoe_core::Flavour::Durable;
    let store = Store::open(dir).unwrap();
    let root = b"\x80\x03cpersistent.mapping\nPersistentMapping\nq\x00.\x80\x03}q\x01.";
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(Oid(0), Tid::ZERO, root).unwrap();
    txn.vote().unwrap();
    txn.finish().unwrap();
    let last = commit_kv(&store, &[(Durable, "registry", b"title", Some(b"site"))]);
    store.pack(last).unwrap();
    (store, last)
}

#[test]
fn a_backup_after_a_pack_at_an_empty_last_commit_continues() {
    use zoe_core::Flavour::Durable;
    let dir = test_dir("backup-pack-empty-tail");
    let (source, dumps) = (dir.join("store"), dir.join("dumps"));
    let store = Store::open(&source).unwrap();
    let root = b"\x80\x03cpersistent.mapping\nPersistentMapping\nq\x00.\x80\x03}q\x01.";
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(Oid(0), Tid::ZERO, root).unwrap();
    txn.vote().unwrap();
    txn.finish().unwrap();
    let last = commit_kv(&store, &[(Durable, "registry", b"a", Some(b"1"))]);
    backup(&store, &dumps).unwrap();
    // The pack at the last tid keeps the row of the last commit, which
    // the dumps end at.
    store.pack(last).unwrap();
    commit_kv(&store, &[(Durable, "registry", b"b", Some(b"2"))]);
    let next = backup(&store, &dumps).unwrap();
    assert_eq!(next.after, last);
    assert!(next.dump.is_some());
    drop(store);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_backup_after_a_gc_pack_that_empties_the_last_commit_continues() {
    use zoe_core::Flavour::Durable;
    let dir = test_dir("backup-gc-pack-tail");
    let (source, dumps) = (dir.join("store"), dir.join("dumps"));
    let store = Store::open(&source).unwrap();
    let mapping = b"\x80\x03cpersistent.mapping\nPersistentMapping\nq\x00.\x80\x03}q\x01.";
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(Oid(0), Tid::ZERO, mapping).unwrap();
    txn.vote().unwrap();
    txn.finish().unwrap();
    // The last commit writes only an object that nothing refers to.
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(Oid(1), Tid::ZERO, mapping).unwrap();
    txn.vote().unwrap();
    let last = txn.finish().unwrap();
    backup(&store, &dumps).unwrap();
    // The pack removes the only record of the last commit, and keeps
    // its row, which the dumps end at.
    store.pack(last).unwrap();
    assert!(store.load(Oid(1)).is_err());
    assert_eq!(store.last_transaction(), last);
    commit_kv(&store, &[(Durable, "registry", b"b", Some(b"2"))]);
    let next = backup(&store, &dumps).unwrap();
    assert_eq!(next.after, last);
    assert!(next.dump.is_some());
    drop(store);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_restore_after_a_pack_at_an_empty_last_commit_has_the_same_last_tid() {
    let dir = test_dir("restore-pack-empty-tail");
    let (source, dumps, target) = (dir.join("store"), dir.join("dumps"), dir.join("target"));
    let (store, last) = packed_with_an_empty_tail(&source);
    backup(&store, &dumps).unwrap();
    drop(store);
    restore(&dumps, &target).unwrap();
    let copy = Store::open(&target).unwrap();
    assert_eq!(copy.last_transaction(), last);
    assert_eq!(copy.kv_generation("registry").unwrap(), last);
    let report = crate::verify(&copy, None);
    assert_eq!(report.problems(), 0, "{report:?}");
    drop(copy);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn the_check_of_a_backup_finds_a_durable_row_that_differs() {
    use zoe_core::Flavour::Durable;
    let dir = test_dir("backup-durable-check");
    let (source, dumps) = (dir.join("store"), dir.join("dumps"));
    let store = sample(&source);
    commit_kv(&store, &[(Durable, "registry", b"title", Some(b"site"))]);
    backup(&store, &dumps).unwrap();
    // A change of the row that no commit made: the dumps differ.
    let write_row = |payload: &[u8]| {
        let row = DurableRow {
            name: "registry".to_owned(),
            key: b"title".to_vec(),
            payload: payload.to_vec(),
        };
        store.kv_restore_durable(&[row]).unwrap();
    };
    write_row(b"other");
    let check = check_dumps(&store, &dumps, &dir).unwrap();
    assert_eq!(check.differences.len(), 1, "{:?}", check.differences);
    let text = &check.differences[0];
    assert!(
        text.contains("\"registry\"") && text.contains("another value"),
        "{text}"
    );
    assert!(!text.contains("title"), "a difference names no key: {text}");
    drop(store);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_restore_refuses_a_dump_of_version_1() {
    let dir = test_dir("backup-version-1");
    let (source, dumps, target) = (dir.join("store"), dir.join("dumps"), dir.join("target"));
    let store = sample(&source);
    let full = backup(&store, &dumps).unwrap();
    drop(store);
    // A dump of version 1 gets the refusal of an unknown version, 3.
    let path = full.dump.unwrap();
    let good = fs::read(&path).unwrap();
    let mut refusals = Vec::new();
    for version in [1, 3] {
        let mut bytes = good.clone();
        bytes[7] = version;
        let end = bytes.len() - HASH;
        let hash = blake3::hash(&bytes[..end]);
        bytes[end..].copy_from_slice(hash.as_bytes());
        fs::write(&path, bytes).unwrap();
        let error = restore(&dumps, &target).unwrap_err();
        assert!(
            matches!(&error, Error::Dump { reason, .. } if reason == "not a dump file of ZOE"),
            "version {version}: {error}"
        );
        assert!(!target.exists(), "version {version} left a target");
        refusals.push(error.to_string());
    }
    assert_eq!(refusals[0], refusals[1]);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_name_gives_back_its_tids() {
    let name = dump_name(Tid(0), Tid(0x03f6_db4f_2222_2222));
    assert_eq!(name, "0000000000000000-03f6db4f22222222.zoedump");
    assert_eq!(
        parse_name(&name),
        Some((Tid(0), Tid(0x03f6_db4f_2222_2222)))
    );
    for other in [
        "0000000000000000-03F6DB4F22222222.zoedump",
        "0000000000000000-03f6db4f2222222.zoedump",
        "0000000000000000-03f6db4f22222222.zoedump.tmp",
        "0000000000000000_03f6db4f22222222.zoedump",
        "+000000000000000-03f6db4f22222222.zoedump",
    ] {
        assert_eq!(parse_name(other), None, "{other}");
    }
}

#[test]
fn a_restore_gives_back_the_store() {
    let dir = test_dir("backup-round-trip");
    let (source, dumps, target) = (dir.join("source"), dir.join("dumps"), dir.join("target"));
    let store = sample(&source);
    let full = backup(&store, &dumps).unwrap();
    assert_eq!((full.after, full.last), (Tid::ZERO, Tid(200)));
    assert_eq!((full.transactions, full.records, full.blobs), (2, 4, 1));
    assert_eq!((full.blob_bytes, full.sequences), (70_000, 1));

    commit(
        &store,
        300,
        &[W::Data(1, b"one again"), W::Data(4, b"four")],
    );
    let mut empty = store.tpc_begin(b"", b"empty", b"").unwrap();
    empty.vote().unwrap();
    empty.finish().unwrap();
    // A second block of the sequence: its end is in the next dump.
    let sequence = store.sequence("intids");
    let last_value = (0..120).map(|_| sequence.next().unwrap()).max().unwrap();
    let incremental = backup(&store, &dumps).unwrap();
    assert_eq!(incremental.after, Tid(200));
    assert_eq!(incremental.transactions, 2);
    drop(store);

    let restored = restore(&dumps, &target).unwrap();
    assert_eq!((restored.dumps, restored.transactions), (2, 4));
    assert_eq!(
        (restored.records, restored.blobs, restored.sequences),
        (6, 1, 1)
    );
    assert!(!sibling(&target, WORK_SUFFIX).unwrap().exists());

    let source = Store::open(&source).unwrap();
    let target = Store::open(&target).unwrap();
    assert_eq!(transactions(&source), transactions(&target));
    assert_eq!(source.last_transaction(), target.last_transaction());
    assert_eq!(blob(&target, 3, 200), vec![7; 70_000]);
    assert_eq!(
        source.load_blob(Oid(3), Tid(200)).unwrap(),
        target.load_blob(Oid(3), Tid(200)).unwrap()
    );
    assert!(target.sequence("intids").next().unwrap() > last_value);
    assert!(target.new_oid().unwrap() > Oid(4));
    drop((source, target));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_backup_writes_a_new_file_and_nothing_when_nothing_is_new() {
    let dir = test_dir("backup-files");
    let (source, dumps) = (dir.join("source"), dir.join("dumps"));
    let store = sample(&source);
    backup(&store, &dumps).unwrap();
    let first = dump_files(&dumps);
    assert_eq!(first.len(), 1);
    let bytes = fs::read(&first[0]).unwrap();

    // Nothing new: nothing written.
    let nothing = backup(&store, &dumps).unwrap();
    assert_eq!(nothing.dump, None);
    assert_eq!(dump_files(&dumps), first);

    // A stopped backup leaves a temporary file. The next one removes it.
    let leftover = dumps.join(format!("{}{TEMP_SUFFIX}", dump_name(Tid(200), Tid(250))));
    fs::write(&leftover, b"partial").unwrap();
    let foreign = dumps.join("notes.txt");
    fs::write(&foreign, b"kept").unwrap();
    commit(&store, 300, &[W::Data(1, b"one again")]);
    let written = backup(&store, &dumps).unwrap();
    let name = dump_name(Tid(200), Tid(300));
    assert_eq!(written.dump, Some(dumps.join(&name)));
    assert!(!leftover.exists());
    assert!(foreign.exists());
    assert_eq!(fs::read(&first[0]).unwrap(), bytes);
    drop(store);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn an_empty_store_gives_a_full_dump_that_a_later_dump_follows() {
    let dir = test_dir("backup-empty");
    let (source, dumps, target) = (dir.join("source"), dir.join("dumps"), dir.join("target"));
    let store = Store::open(&source).unwrap();
    let empty = backup(&store, &dumps).unwrap();
    assert_eq!(empty.dump, Some(dumps.join(dump_name(Tid(0), Tid(0)))));
    assert_eq!(backup(&store, &dumps).unwrap().dump, None);
    commit(&store, 100, &[W::Data(1, b"one")]);
    backup(&store, &dumps).unwrap();
    drop(store);
    let restored = restore(&dumps, &target).unwrap();
    assert_eq!((restored.dumps, restored.transactions), (2, 1));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_damaged_dump_fails_the_restore_before_it_writes_anything() {
    let dir = test_dir("backup-damage");
    let (source, dumps) = (dir.join("source"), dir.join("dumps"));
    let store = sample(&source);
    backup(&store, &dumps).unwrap();
    drop(store);
    let file = dump_files(&dumps).remove(0);
    let good = fs::read(&file).unwrap();
    let len = good.len();
    let target = dir.join("target");

    let flips = [
        0,
        9,
        20,
        HEADER,
        HEADER + 5,
        len / 2,
        len - HASH - 1,
        len - 1,
    ];
    for at in flips {
        let mut bytes = good.clone();
        bytes[at] ^= 0x01;
        fs::write(&file, &bytes).unwrap();
        let error = restore(&dumps, &target).unwrap_err();
        assert!(
            matches!(error, Error::Dump { .. }),
            "a flip at {at}: {error}"
        );
        // After the header, the hash pass finds the damage before a
        // parse reads a length.
        if at >= HEADER {
            assert!(error.to_string().contains("checksum"), "{at}: {error}");
        }
        assert!(!target.exists(), "a flip at {at} left a target");
    }
    for bytes in [
        &good[..len - 1],
        &good[..len / 2],
        &[good.clone(), vec![0]].concat(),
    ] {
        fs::write(&file, bytes).unwrap();
        let error = restore(&dumps, &target).unwrap_err();
        assert!(matches!(error, Error::Dump { .. }), "{error}");
        assert!(!target.exists());
    }
    fs::write(&file, &good).unwrap();
    restore(&dumps, &target).unwrap();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_missing_dump_breaks_the_chain() {
    let dir = test_dir("backup-chain");
    let (source, dumps) = (dir.join("source"), dir.join("dumps"));
    let store = sample(&source);
    backup(&store, &dumps).unwrap();
    for tid in [300, 400] {
        commit(&store, tid, &[W::Data(1, format!("{tid}").as_bytes())]);
        backup(&store, &dumps).unwrap();
    }
    let files = dump_files(&dumps);
    assert_eq!(files.len(), 3);
    let target = dir.join("target");

    let middle = fs::read(&files[1]).unwrap();
    fs::remove_file(&files[1]).unwrap();
    let error = restore(&dumps, &target).unwrap_err();
    assert!(error.to_string().contains("a dump is missing"), "{error}");
    // A backup does not add a dump after a gap.
    commit(&store, 500, &[W::Data(1, b"500")]);
    assert!(matches!(backup(&store, &dumps), Err(Error::Dump { .. })));
    fs::write(&files[1], &middle).unwrap();

    fs::rename(&files[0], dir.join("away")).unwrap();
    let error = restore(&dumps, &target).unwrap_err();
    assert!(
        error.to_string().contains("full dump is missing"),
        "{error}"
    );
    fs::rename(dir.join("away"), &files[0]).unwrap();

    // Two dumps that start after the same tid.
    let overlap = dumps.join(dump_name(Tid(200), Tid(350)));
    fs::copy(&files[1], &overlap).unwrap();
    let error = restore(&dumps, &target).unwrap_err();
    assert!(error.to_string().contains("the dumps overlap"), "{error}");
    fs::remove_file(&overlap).unwrap();

    // A dump under the name of another range.
    let renamed = dumps.join(dump_name(Tid(400), Tid(450)));
    fs::copy(&files[2], &renamed).unwrap();
    let error = restore(&dumps, &target).unwrap_err();
    assert!(error.to_string().contains("header"), "{error}");
    fs::remove_file(&renamed).unwrap();

    assert!(!target.exists());
    let empty = dir.join("empty");
    fs::create_dir(&empty).unwrap();
    assert!(matches!(
        restore(&empty, &target),
        Err(Error::NoDump { .. })
    ));
    drop(store);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_backup_refuses_the_dumps_of_another_store() {
    let dir = test_dir("backup-diverged");
    let (one, two, dumps) = (dir.join("one"), dir.join("two"), dir.join("dumps"));
    let first = sample(&one);
    backup(&first, &dumps).unwrap();
    let other = Store::open(&two).unwrap();
    commit(&other, 150, &[W::Data(1, b"other")]);
    commit(&other, 250, &[W::Data(1, b"other again")]);
    let error = backup(&other, &dumps).unwrap_err();
    assert!(matches!(error, Error::BackupDiverged { .. }), "{error}");
    // A store behind the dumps does not continue them.
    let behind = Store::open(dir.join("three")).unwrap();
    commit(&behind, 100, &[W::Data(1, b"one")]);
    let error = backup(&behind, &dumps).unwrap_err();
    assert!(matches!(error, Error::BackupDiverged { .. }), "{error}");
    // A store made after the dumps and packed after their last tid
    // does not continue them either.
    let later = Store::open(dir.join("four")).unwrap();
    commit(&later, 300, &[W::Data(0, &pickle(b"root"))]);
    commit(&later, 400, &[W::Data(1, &pickle(b"later"))]);
    later.pack(Tid(400)).unwrap();
    let error = backup(&later, &dumps).unwrap_err();
    assert!(matches!(error, Error::BackupDiverged { .. }), "{error}");
    assert!(
        error
            .to_string()
            .contains("new full dump into a new directory")
    );
    assert_eq!(dump_files(&dumps).len(), 1);
    drop((first, other, behind, later));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_backup_takes_the_lock_of_its_directory() {
    let dir = test_dir("backup-lock");
    let (source, dumps) = (dir.join("source"), dir.join("dumps"));
    let store = sample(&source);
    fs::create_dir(&dumps).unwrap();
    let held = lock_dir(&dumps).unwrap();
    assert!(matches!(backup(&store, &dumps), Err(Error::Locked { .. })));
    assert!(dump_files(&dumps).is_empty());
    drop(held);
    backup(&store, &dumps).unwrap();
    drop(store);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_target_whose_database_entry_is_a_directory_or_a_dangling_link_holds_a_store() {
    let dir = test_dir("backup-target-entry");
    let (source, dumps) = (dir.join("source"), dir.join("dumps"));
    let store = sample(&source);
    backup(&store, &dumps).unwrap();
    drop(store);

    // Any entry with the name of the database file counts.
    let folder = dir.join("folder");
    fs::create_dir_all(folder.join(DATABASE)).unwrap();
    let link = dir.join("link");
    fs::create_dir(&link).unwrap();
    std::os::unix::fs::symlink(dir.join("nowhere"), link.join(DATABASE)).unwrap();
    for target in [&folder, &link] {
        match restore(&dumps, target) {
            Err(Error::Target { reason, .. }) => {
                assert!(reason.starts_with("holds a store"), "{reason}");
            }
            other => panic!("{other:?}"),
        }
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_restore_writes_only_into_a_new_or_an_empty_directory() {
    let dir = test_dir("backup-target");
    let (source, dumps) = (dir.join("source"), dir.join("dumps"));
    let store = sample(&source);
    backup(&store, &dumps).unwrap();
    drop(store);

    let refused = |target: &Path| match restore(&dumps, target) {
        Err(Error::Target { reason, .. }) => reason,
        other => panic!("{other:?}"),
    };
    assert!(refused(&source).starts_with("holds a store"));
    let busy = dir.join("busy");
    fs::create_dir(&busy).unwrap();
    fs::write(busy.join("file"), b"kept").unwrap();
    assert!(refused(&busy).starts_with("not empty"));
    assert!(refused(&busy.join("file")).starts_with("not a directory"));

    // A work directory that exists can belong to another restore.
    let target = dir.join("target");
    let work = sibling(&target, WORK_SUFFIX).unwrap();
    fs::create_dir(&work).unwrap();
    assert!(refused(&target).starts_with("exists"));
    assert!(work.exists() && !target.exists());
    fs::remove_dir(&work).unwrap();

    // An empty directory takes the store, also given with a slash.
    fs::create_dir(&target).unwrap();
    let with_slash = PathBuf::from(format!("{}/", target.display()));
    restore(&dumps, &with_slash).unwrap();
    assert!(target.join(DATABASE).exists());
    assert!(!work.exists());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn the_check_of_a_backup_finds_each_difference() {
    let dir = test_dir("backup-check");
    let (source, dumps, scratch) = (dir.join("source"), dir.join("dumps"), dir.join("scratch"));
    let store = sample(&source);
    backup(&store, &dumps).unwrap();

    // A commit after the dumps is no difference.
    commit(&store, 300, &[W::Data(1, b"after")]);
    let check = check_dumps(&store, &dumps, &scratch).unwrap();
    assert_eq!(check.differences, Vec::<String>::new());
    assert_eq!(
        (check.restored.transactions, check.restored.last_tid),
        (2, Tid(200))
    );
    assert_eq!(fs::read_dir(&scratch).unwrap().count(), 0);
    drop(store);

    // Another store with the same tids differs in a record, lacks the
    // blob and a transaction, and has an older sequence.
    let other = Store::open(dir.join("other")).unwrap();
    commit(&other, 100, &[W::Data(1, b"ONE"), W::Data(2, b"two")]);
    commit(&other, 150, &[W::Data(5, b"five")]);
    let check = check_dumps(&other, &dumps, &scratch).unwrap();
    let text = check.differences.join("\n");
    assert!(
        text.contains("oid 0x0000000000000001 at 0x0000000000000064: the record differs"),
        "{text}"
    );
    assert!(
        text.contains("transaction 0x0000000000000096, which the dumps do not have"),
        "{text}"
    );
    assert!(
        text.contains("lacks transaction 0x00000000000000c8"),
        "{text}"
    );
    assert!(
        text.contains("sequence \"intids\": the store has no row"),
        "{text}"
    );
    // At the last tid of the dumps, the current records of oids 1, 2,
    // 3 and 5 differ: another pickle, a deletion, a blob and an object
    // that the dumps do not have.
    for oid in [1, 2, 3, 5] {
        let line = format!(
            "oid {} at 0x00000000000000c8: the current record differs",
            Oid(oid).hex()
        );
        assert!(text.contains(&line), "{text}");
    }
    assert_eq!(check.differences.len(), 8, "{text}");
    drop(other);

    // A damaged dump fails the check.
    let file = dump_files(&dumps).remove(0);
    let mut bytes = fs::read(&file).unwrap();
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0x01;
    fs::write(&file, &bytes).unwrap();
    // The check restores the dumps before it opens the store, so it
    // fails before the open.
    let error = verify_backup(&dumps, &scratch, || -> Result<Store, Error> {
        panic!("the check opened the store")
    })
    .unwrap_err();
    assert!(matches!(error, Error::Dump { .. }), "{error}");
    assert_eq!(fs::read_dir(&scratch).unwrap().count(), 0);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn records_that_a_pack_removed_are_no_difference() {
    let dir = test_dir("backup-pack");
    let (source, dumps, scratch) = (dir.join("source"), dir.join("dumps"), dir.join("scratch"));
    let store = Store::open(&source).unwrap();
    // Oids 1 and 2 are not reachable from the root, so a pack removes
    // every record of them at or before the pack tid.
    commit(
        &store,
        100,
        &[W::Data(0, &pickle(b"root")), W::Data(1, &pickle(b"a"))],
    );
    commit(&store, 200, &[W::Data(1, &pickle(b"b"))]);
    commit(&store, 300, &[W::Data(1, &pickle(b"c"))]);
    commit(&store, 400, &[W::Data(2, &pickle(b"d"))]);
    backup(&store, &dumps).unwrap();
    store.pack(Tid(250)).unwrap();
    assert!(store.load_serial(Oid(1), Tid(200)).is_err());
    let check = check_dumps(&store, &dumps, &scratch).unwrap();
    assert_eq!(check.differences, Vec::<String>::new());

    // A pack before the last tid of the dumps lets the backups go on.
    commit(&store, 500, &[W::Data(0, &pickle(b"root again"))]);
    let written = backup(&store, &dumps).unwrap();
    assert_eq!((written.after, written.transactions), (Tid(400), 1));
    let check = check_dumps(&store, &dumps, &scratch).unwrap();
    assert_eq!(check.differences, Vec::<String>::new());
    assert_eq!((check.restored.dumps, check.restored.transactions), (2, 5));

    // A pack at the last tid of the dumps removes only history that the
    // dumps hold, so the backups go on.
    store.pack(Tid(500)).unwrap();
    assert!(store.load_serial(Oid(0), Tid(100)).is_err());
    assert!(store.load_serial(Oid(2), Tid(400)).is_err());
    let check = check_dumps(&store, &dumps, &scratch).unwrap();
    assert_eq!(check.differences, Vec::<String>::new());
    commit(&store, 600, &[W::Data(0, &pickle(b"root once more"))]);
    let written = backup(&store, &dumps).unwrap();
    assert_eq!((written.after, written.transactions), (Tid(500), 1));
    let check = check_dumps(&store, &dumps, &scratch).unwrap();
    assert_eq!(check.differences, Vec::<String>::new());
    assert_eq!((check.restored.dumps, check.restored.transactions), (3, 6));

    // A pack after the last tid of the dumps stops the backups, and it
    // is a difference. The pack keeps the root at 600, so the last
    // transaction of the dumps stays in the store.
    commit(&store, 700, &[W::Data(3, &pickle(b"f"))]);
    store.pack(Tid(700)).unwrap();
    match backup(&store, &dumps) {
        Err(Error::BackupPacked {
            last, watermark, ..
        }) => assert_eq!((last, watermark), (Tid(600), Tid(700))),
        other => panic!("{other:?}"),
    }
    assert_eq!(dump_files(&dumps).len(), 3);
    let check = check_dumps(&store, &dumps, &scratch).unwrap();
    assert_eq!(check.differences.len(), 1, "{:?}", check.differences);
    assert!(check.differences[0].starts_with(
        "the store was packed at tid 0x00000000000002bc, after the last tid 0x0000000000000258"
    ));
    drop(store);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_store_restored_from_a_part_of_the_dumps_does_not_continue_them() {
    let dir = test_dir("backup-rollback");
    let (source, dumps, scratch) = (dir.join("source"), dir.join("dumps"), dir.join("scratch"));
    let store = Store::open(&source).unwrap();
    commit(
        &store,
        100,
        &[W::Data(0, &pickle(b"root")), W::Data(1, &pickle(b"4"))],
    );
    backup(&store, &dumps).unwrap();
    commit(&store, 200, &[W::Data(1, &pickle(b"5"))]);
    backup(&store, &dumps).unwrap();
    drop(store);

    // A restore of the full dump alone, then other commits.
    let part = dir.join("part");
    fs::create_dir(&part).unwrap();
    let full = dump_files(&dumps).remove(0);
    fs::copy(&full, part.join(full.file_name().unwrap())).unwrap();
    let rolled = dir.join("rolled");
    restore(&part, &rolled).unwrap();
    let rolled = Store::open(&rolled).unwrap();
    for tid in [300, 400, 500] {
        commit(&rolled, tid, &[W::Data(2, &pickle(b"other"))]);
    }
    let error = backup(&rolled, &dumps).unwrap_err();
    assert!(matches!(error, Error::BackupDiverged { .. }), "{error}");
    let check = check_dumps(&rolled, &dumps, &scratch).unwrap();
    let text = check.differences.join("\n");
    assert!(
        text.contains("the store lacks transaction 0x00000000000000c8"),
        "{text}"
    );
    let differs = "oid 0x0000000000000001 at 0x00000000000000c8: the current record differs";
    assert!(text.contains(differs), "{text}");

    // A pack does not hide it.
    rolled.pack(Tid(500)).unwrap();
    let error = backup(&rolled, &dumps).unwrap_err();
    assert!(matches!(error, Error::BackupDiverged { .. }), "{error}");
    let check = check_dumps(&rolled, &dumps, &scratch).unwrap();
    let text = check.differences.join("\n");
    assert!(text.starts_with("the store was packed at tid"), "{text}");
    assert_eq!(dump_files(&dumps).len(), 2);
    drop(rolled);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_check_removes_the_directories_of_a_killed_check() {
    let dir = test_dir("backup-stale");
    let (source, dumps, scratch) = (dir.join("source"), dir.join("dumps"), dir.join("scratch"));
    let store = sample(&source);
    backup(&store, &dumps).unwrap();
    // No process has the id u32::MAX, and this process keeps its own.
    let gone = scratch.join(format!("{CHECK_PREFIX}{}-0{WORK_SUFFIX}", u32::MAX));
    let mine = scratch.join(format!("{CHECK_PREFIX}{}-999", std::process::id()));
    let other = scratch.join("zoe-backup-check-notes");
    for path in [&gone, &mine, &other] {
        fs::create_dir_all(path.join("inside")).unwrap();
    }
    check_dumps(&store, &dumps, &scratch).unwrap();
    assert!(!gone.exists());
    assert!(mine.exists() && other.exists());
    drop(store);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn an_open_that_fails_fails_the_check_and_removes_the_copy() {
    let dir = test_dir("backup-open");
    let (source, dumps, scratch) = (dir.join("source"), dir.join("dumps"), dir.join("scratch"));
    let store = sample(&source);
    backup(&store, &dumps).unwrap();
    // The copy is already built when the open fails.
    let error = verify_backup(&dumps, &scratch, || {
        Err::<Store, _>(Error::Store(ZoeError::Locked))
    })
    .unwrap_err();
    assert!(matches!(error, Error::Store(_)), "{error}");
    assert_eq!(fs::read_dir(&scratch).unwrap().count(), 0);
    drop(store);
    let _ = fs::remove_dir_all(&dir);
}
