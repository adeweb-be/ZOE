//! Acceptance test of the repair of a store that a writer left open, at a
//! read-only open (N1, N7, F15, F20, F25).
//!
//! A writer that dies by a signal leaves the database file of its store
//! with the recovery flag of redb, and without the allocator state that a
//! clean close writes. `Store::open_read_only` refuses such a store with
//! `NotClosedCleanly` and changes no byte: the tests of a clean close rely
//! on it. `Store::open_read_only_repairing` is the open of the tools that
//! read a store. It takes the lock of the store, lets redb repair the
//! database file through a writer open, closes the database cleanly, then
//! opens the store read-only, with the lock held from start to end. It
//! writes only `zoe.redb`, and only when the store needs the repair.
//!
//! The crash image is a copy of the database file of an open store after
//! a commit, as a kill of the writer leaves it.

mod support;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use support::fresh_dir;
use zoe_core::testing::{Meta, commit_as};
use zoe_core::{Oid, Options, Store, Tid, ZoeError};

/// The user, the description and the extension of each commit.
const META: Meta = (b"user", b"description", b"");

/// The database file of a store.
const DATABASE: &str = "zoe.redb";
/// The lock file of a store.
const LOCK: &str = "lock";

/// Return every entry under `dir`, by path: the bytes of a file, `None`
/// for a directory.
fn tree(dir: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    let mut entries = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path.clone());
                entries.insert(path, None);
            } else {
                let bytes = fs::read(&path).unwrap();
                entries.insert(path, Some(bytes));
            }
        }
    }
    entries
}

/// Write a store in `dir` and close it cleanly. Return the tid of its
/// commit.
fn clean_store(dir: &Path) -> Tid {
    let store = Store::open(dir).unwrap();
    commit_as(&store, META, &[(Oid(0), Tid::ZERO, b"first")])
}

/// Make a store in `root/image` that a writer left open after its last
/// commit. Return the store directory and the tid of that commit.
fn crash_image(root: &Path) -> (PathBuf, Tid) {
    let (live, image) = (root.join("live"), root.join("image"));
    let first = clean_store(&live);
    // The open sets the recovery flag of redb. The commit is durable, and
    // only a clean close writes the allocator state after it.
    let store = Store::open(&live).unwrap();
    let last = commit_as(&store, META, &[(Oid(0), first, b"second")]);
    fs::create_dir_all(&image).unwrap();
    fs::copy(live.join(DATABASE), image.join(DATABASE)).unwrap();
    fs::write(image.join(LOCK), b"").unwrap();
    drop(store);
    (image, last)
}

/// Check that only `zoe.redb` differs between `before` and `after`: the
/// same entries, and the same bytes in every other file.
fn assert_only_the_database_changed(
    dir: &Path,
    before: &BTreeMap<PathBuf, Option<Vec<u8>>>,
    after: &BTreeMap<PathBuf, Option<Vec<u8>>>,
) {
    let database = dir.join(DATABASE);
    assert_eq!(
        before.keys().collect::<Vec<_>>(),
        after.keys().collect::<Vec<_>>(),
        "the open added or removed an entry"
    );
    for (path, bytes) in before {
        if *path != database {
            assert!(after[path] == *bytes, "the open changed {}", path.display());
        }
    }
}

#[test]
fn the_strict_read_only_open_still_refuses_a_store_left_open_and_writes_nothing() {
    let root = fresh_dir("strict");
    let (image, _) = crash_image(&root);
    let before = tree(&image);

    let result = Store::open_read_only(&image);

    assert!(
        matches!(result, Err(ZoeError::NotClosedCleanly)),
        "{:?}",
        result.err()
    );
    assert!(tree(&image) == before, "the strict open changed a file");
}

#[test]
fn the_repairing_open_repairs_a_store_left_open_and_reads_its_last_commit() {
    let root = fresh_dir("repair");
    let (image, last) = crash_image(&root);
    let before = tree(&image);

    let store = Store::open_read_only_repairing(&image, Options::default()).unwrap();

    assert_eq!(store.load(Oid(0)).unwrap(), (b"second".to_vec(), last));
    assert_eq!(store.last_transaction(), last);
    assert!(matches!(store.new_oid(), Err(ZoeError::ReadOnly)));
    assert!(matches!(
        store.tpc_begin(b"", b"", b""),
        Err(ZoeError::ReadOnly)
    ));
    drop(store);
    let after = tree(&image);
    assert!(
        after[&image.join(DATABASE)] != before[&image.join(DATABASE)],
        "the open did not repair the database file"
    );
    assert_only_the_database_changed(&image, &before, &after);

    // The repair closed the database cleanly: the strict open accepts it.
    let store = Store::open_read_only(&image).unwrap();
    assert_eq!(store.load(Oid(0)).unwrap(), (b"second".to_vec(), last));
    drop(store);
    assert!(tree(&image) == after, "the strict open changed a file");
}

#[test]
fn the_repairing_open_of_a_clean_store_writes_nothing() {
    let root = fresh_dir("clean");
    let dir = root.join("store");
    let tid = clean_store(&dir);
    let before = tree(&dir);

    let store = Store::open_read_only_repairing(&dir, Options::default()).unwrap();
    assert_eq!(store.load(Oid(0)).unwrap(), (b"first".to_vec(), tid));
    drop(store);

    assert!(tree(&dir) == before, "the open changed a file");
}

#[test]
fn a_second_repairing_open_writes_nothing() {
    let root = fresh_dir("twice");
    let (image, last) = crash_image(&root);
    drop(Store::open_read_only_repairing(&image, Options::default()).unwrap());
    let repaired = tree(&image);

    let store = Store::open_read_only_repairing(&image, Options::default()).unwrap();
    assert_eq!(store.load(Oid(0)).unwrap(), (b"second".to_vec(), last));
    drop(store);

    assert!(tree(&image) == repaired, "the second open changed a file");
}

#[test]
fn the_repair_leaves_the_leftovers_of_the_writer_to_the_next_writer_open() {
    let root = fresh_dir("leftovers");
    let (image, last) = crash_image(&root);
    // Files that a writer open removes: a blob file of an unfinished
    // commit and a spill file. The repair writes only the database file.
    fs::create_dir_all(image.join("blobs/tmp")).unwrap();
    fs::write(image.join("blobs/.layout"), b"bushy").unwrap();
    fs::write(image.join("blobs/tmp/7.tmp"), b"a blob").unwrap();
    fs::create_dir_all(image.join("staging")).unwrap();
    fs::write(image.join("staging/3.spill"), b"a pickle").unwrap();
    let before = tree(&image);

    let store = Store::open_read_only_repairing(&image, Options::default()).unwrap();
    assert_eq!(store.load(Oid(0)).unwrap(), (b"second".to_vec(), last));
    drop(store);

    assert_only_the_database_changed(&image, &before, &tree(&image));
}

#[test]
fn the_repairing_open_creates_nothing_in_a_blob_directory_elsewhere() {
    let root = fresh_dir("blob-dir");
    let (image, last) = crash_image(&root);
    let blobs = root.join("blobs-elsewhere");
    let options = Options {
        blob_dir: Some(blobs.clone()),
        ..Options::default()
    };

    let store = Store::open_read_only_repairing(&image, options).unwrap();
    assert_eq!(store.load(Oid(0)).unwrap(), (b"second".to_vec(), last));
    drop(store);

    assert!(!blobs.exists(), "the open created the blob directory");
    assert!(!image.join("blobs").exists(), "the open created blobs/");
}

#[test]
fn the_repairing_open_writes_nothing_while_another_open_holds_the_lock() {
    let root = fresh_dir("locked");
    let (image, _) = crash_image(&root);
    let before = tree(&image);
    let holder = fs::File::open(image.join(LOCK)).unwrap();
    holder.try_lock().unwrap();

    let result = Store::open_read_only_repairing(&image, Options::default());

    assert!(
        matches!(result, Err(ZoeError::Locked)),
        "{:?}",
        result.err()
    );
    assert!(tree(&image) == before, "the open changed a file");
    drop(holder);
    let store = Store::open_read_only_repairing(image, Options::default()).unwrap();
    drop(store);
}

#[test]
fn the_repaired_store_holds_its_lock_against_a_writer() {
    let root = fresh_dir("exclusive");
    let (image, _) = crash_image(&root);

    let reader = Store::open_read_only_repairing(&image, Options::default()).unwrap();

    assert!(matches!(Store::open(&image), Err(ZoeError::Locked)));
    assert!(matches!(
        Store::open_read_only_repairing(&image, Options::default()),
        Err(ZoeError::Locked)
    ));
    drop(reader);
    drop(Store::open(image).unwrap());
}

#[test]
fn the_repairing_open_creates_nothing_without_a_store() {
    let dir = fresh_dir("none");
    let missing = dir.join("missing");

    assert!(Store::open_read_only_repairing(&missing, Options::default()).is_err());
    assert!(!missing.exists());
    assert!(Store::open_read_only_repairing(&dir, Options::default()).is_err());
    assert!(tree(&dir).is_empty());
}
