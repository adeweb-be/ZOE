//! Acceptance test of the durability of an open: every open syncs the
//! store directory, also when `zoe.redb` exists. So a commit that a later
//! process acknowledges survives a power loss, also when a kill stopped an
//! earlier process before its sync. An open of an existing store writes no
//! other file.

use zoe_core::vfs::MemVfs;
use zoe_core::{Oid, Store, Tid, ZoeError};

/// A protocol-3 ZODB record: the class pickle, then the state pickle.
const PICKLE: &[u8] =
    b"\x80\x03cpersistent.mapping\nPersistentMapping\nq\x00.\x80\x03}q\x01X\x04\x00\x00\x00datag\x01s.";

/// Commit one new object and return its oid and the tid of the commit.
fn commit_one(store: &Store) -> (Oid, Tid) {
    let oid = store.new_oid().unwrap();
    let mut txn = store.tpc_begin(b"user", b"open durability", b"").unwrap();
    txn.store(oid, Tid::ZERO, PICKLE).unwrap();
    txn.vote().unwrap();
    let tid = txn.finish().unwrap();
    (oid, tid)
}

/// Open the store in `vfs` and kill the process at the next sync of the
/// store directory, as `kill -9` does. Then start a new process on the
/// same files. The operations that the killed process did not sync stay
/// pending.
fn kill_an_open(vfs: &MemVfs) {
    vfs.kill_at_dir_sync("");
    // An open that syncs the store directory fails at the sync. An open
    // that does not sync it returns, and the process dies after it.
    if let Ok(store) = Store::open_vfs(vfs.clone()) {
        // A killed process runs no destructor.
        std::mem::forget(store);
    }
    vfs.restart();
}

/// Open the store in `vfs`, commit one object, mark the acknowledgement
/// and close the store. Return the oid and the tid of the commit.
fn commit_and_close(vfs: &MemVfs) -> (Oid, Tid) {
    let store = Store::open_vfs(vfs.clone()).unwrap();
    let committed = commit_one(&store);
    vfs.mark("acknowledged");
    drop(store);
    committed
}

/// Open each crash image of `vfs`. Each image opens without repair. Each
/// image after the mark `acknowledged` loads the commit `tid` of `oid`.
fn assert_every_image_keeps(vfs: &MemVfs, oid: Oid, tid: Tid) {
    let images = vfs.crash_images();
    assert!(
        images.iter().any(|image| image.is_after("acknowledged")),
        "the simulation produced crash images after the acknowledgement"
    );
    for image in &images {
        let store = Store::open_vfs(image.vfs())
            .unwrap_or_else(|e| panic!("{image:?} does not open without repair: {e}"));
        match store.load(oid) {
            Ok(found) => assert_eq!(found, (PICKLE.to_vec(), tid), "{image:?}"),
            Err(ZoeError::NotFound { .. }) => assert!(
                !image.is_after("acknowledged"),
                "{image:?} lost an acknowledged commit"
            ),
            Err(e) => panic!("{image:?}: {e}"),
        }
    }
}

/// The first open creates the store and is killed at the sync of the
/// store directory, after the rename of the new database. The next open
/// finds `zoe.redb` and must make it durable before it acknowledges a
/// commit.
#[test]
fn a_commit_after_a_killed_creation_survives_a_power_loss() {
    let vfs = MemVfs::new();
    vfs.kill_at_dir_sync("");
    assert!(
        Store::open_vfs(vfs.clone()).is_err(),
        "the creation syncs the store directory, so the kill stops it"
    );
    vfs.restart();

    let (oid, tid) = commit_and_close(&vfs);
    assert_every_image_keeps(&vfs, oid, tid);
}

/// The creation is killed, then the next open of the existing store is
/// killed at the sync of the store directory too. The open after it must
/// still make the store durable before it acknowledges a commit.
#[test]
fn a_commit_after_a_killed_second_open_survives_a_power_loss() {
    let vfs = MemVfs::new();
    kill_an_open(&vfs);
    kill_an_open(&vfs);

    let (oid, tid) = commit_and_close(&vfs);
    assert_every_image_keeps(&vfs, oid, tid);
}

/// An open of an existing store creates, removes and renames no file, and
/// a crash at any point of that open keeps every acknowledged commit.
#[test]
fn an_open_of_an_existing_store_writes_no_other_file() {
    let vfs = MemVfs::new();
    let (oid, tid) = commit_and_close(&vfs);
    let names = vfs.names();

    let store = Store::open_vfs(vfs.clone()).unwrap();
    assert_eq!(store.load(oid).unwrap(), (PICKLE.to_vec(), tid));
    drop(store);

    assert_eq!(
        vfs.names(),
        names,
        "the open changed the files of the store"
    );
    assert_every_image_keeps(&vfs, oid, tid);
}
