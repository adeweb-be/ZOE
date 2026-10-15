//! A store of `ZOE format 3` keys its records by `(tid, oid)`. A store of
//! an earlier format does not open, for writes or for reads, and the
//! error names the format of the store and `ZOE format 3`. No tool
//! converts such a store.

use std::path::{Path, PathBuf};

use zoe_core::testing;
use zoe_core::vfs::MemVfs;
use zoe_core::{Options, Store, ZoeError};

/// Return an empty directory for one test, on the disk.
fn fresh_dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("format-3-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Check that `error` is the refusal of a store of `old`.
fn assert_refused(error: Option<ZoeError>, old: &str) {
    match &error {
        Some(ZoeError::Format(text)) => assert!(
            text.contains(old) && text.contains("ZOE format 3"),
            "the error does not name {old:?} and \"ZOE format 3\": {text:?}"
        ),
        other => panic!("a store of {old} opened or failed otherwise: {other:?}"),
    }
}

#[test]
fn a_new_store_is_of_format_3() {
    let dir = fresh_dir("new");
    drop(Store::open(&dir).unwrap());
    let database = std::fs::read(dir.join("zoe.redb")).unwrap();
    let name = b"ZOE format 3";
    assert!(
        database.windows(name.len()).any(|window| window == name),
        "the database file does not hold the name of format 3"
    );
}

#[test]
fn a_store_of_an_earlier_format_does_not_open() {
    for old in ["ZOE format 1", "ZOE format 2"] {
        let vfs = MemVfs::new();
        let store = Store::open_vfs(vfs.clone()).unwrap();
        testing::write_meta(&store, "format", old.as_bytes()).unwrap();
        drop(store);
        assert_refused(Store::open_vfs(vfs).err(), old);
    }
}

#[test]
fn a_store_of_an_earlier_format_does_not_open_for_reads() {
    for (name, old) in [("one", "ZOE format 1"), ("two", "ZOE format 2")] {
        let dir = fresh_dir(name);
        let store = Store::open(&dir).unwrap();
        testing::write_meta(&store, "format", old.as_bytes()).unwrap();
        drop(store);
        assert_refused(Store::open_read_only(&dir).err(), old);
        assert_refused(
            Store::open_read_only_repairing(&dir, Options::default()).err(),
            old,
        );
        assert_refused(Store::open(&dir).err(), old);
    }
}

#[test]
fn a_store_of_a_later_format_does_not_open() {
    let vfs = MemVfs::new();
    let store = Store::open_vfs(vfs.clone()).unwrap();
    testing::write_meta(&store, "format", b"ZOE format 4").unwrap();
    drop(store);
    assert_refused(Store::open_vfs(vfs).err(), "ZOE format 4");
}
