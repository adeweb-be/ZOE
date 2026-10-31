//! Acceptance test of the write-ahead log under the crash simulation
//! (ADR 0056; N1, N2).
//!
//! Each test records what the store writes to its `MemVfs`, then opens
//! every state that a crash can leave: each prefix of the operations, each
//! subset of the writes that no sync covers, and the torn halves of each
//! write. Every image must open, keep every commit that a finish
//! acknowledged before the crash, and hold the commits as a prefix of the
//! commit order: a commit is whole or absent, and no commit is there
//! without the commits before it. The tests cover the finishes across
//! segments and into a spare, a fold, the open that replays a log and a
//! clean close.

use std::io::Write;

use zoe_core::testing::{commit, new_oid};
use zoe_core::vfs::{CrashImage, MemVfs};
use zoe_core::{Flavour, Oid, Options, Store, SystemClock, Tid, WriteLog, ZoeError};

/// Settings under which no fold starts by itself.
fn held() -> WriteLog {
    WriteLog {
        fold_bytes: 1 << 40,
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

fn open(vfs: &MemVfs, write_log: WriteLog) -> Store {
    Store::open_full(vfs.clone(), SystemClock, options(write_log)).unwrap()
}

/// Return a `MemVfs` that holds a new, closed store, and whose record of
/// operations starts after the creation.
fn created(write_log: &WriteLog) -> MemVfs {
    let vfs = MemVfs::new();
    drop(open(&vfs, write_log.clone()));
    vfs.durable_copy()
}

/// Open `image` and return the store, or fail the test with the image.
fn reopen(image: &CrashImage, write_log: &WriteLog) -> Store {
    Store::open_full(image.vfs(), SystemClock, options(write_log.clone()))
        .unwrap_or_else(|e| panic!("{image:?} does not open: {e}"))
}

/// Tell which of `commits` the store holds, each with its own tid. Fail
/// when the store gives another tid or another error.
fn present(store: &Store, commits: &[(Oid, Tid)], image: &CrashImage) -> Vec<bool> {
    commits
        .iter()
        .map(|&(oid, tid)| match store.load(oid) {
            Ok((_, serial)) => {
                assert_eq!(serial, tid, "{image:?}: {oid:?}");
                true
            }
            Err(ZoeError::NotFound { .. }) => false,
            Err(e) => panic!("{image:?}: {oid:?}: {e}"),
        })
        .collect()
}

/// Check that the commits that an image holds are a prefix of `commits`,
/// and that it holds each commit acknowledged before the crash: the mark
/// `acknowledged <n>` follows the finish of commit n.
fn check_prefix(image: &CrashImage, held: &[bool]) {
    let prefix = held.iter().take_while(|&&p| p).count();
    assert!(
        held[prefix..].iter().all(|&p| !p),
        "{image:?}: a commit without the commits before it: {held:?}"
    );
    for (n, &p) in held.iter().enumerate() {
        if image.is_after(&format!("acknowledged {n}")) {
            assert!(p, "{image:?}: acknowledged commit {n} is lost");
        }
    }
}

#[test]
fn every_crash_image_of_finishes_across_segments_keeps_each_acknowledged_commit() {
    // Two entries fill a segment, so the finishes rotate, and after the
    // fold they take the segments that it freed.
    let write_log = WriteLog {
        segment_bytes: 4096,
        ..held()
    };
    let vfs = created(&write_log);
    let store = open(&vfs, write_log.clone());
    let mut commits = Vec::new();
    for n in 0..6 {
        if n == 3 {
            store.fold().unwrap();
            vfs.mark("folded");
        }
        let oid = new_oid(&store);
        commits.push((oid, commit(&store, oid, Tid::ZERO, &vec![n as u8; 1500])));
        vfs.mark(&format!("acknowledged {n}"));
    }
    // A crash does not close the store, so the images end here.
    let images = vfs.crash_images();
    drop(store);

    assert!(images.iter().any(|image| image.is_after("acknowledged 5")));
    for image in &images {
        let store = reopen(image, &write_log);
        check_prefix(image, &present(&store, &commits, image));
    }
}

#[test]
fn every_crash_image_of_a_fold_keeps_every_acknowledged_commit_whole() {
    let vfs = created(&held());
    let store = open(&vfs, held());
    let (a, b, c) = (new_oid(&store), new_oid(&store), new_oid(&store));
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(a, Tid::ZERO, b"a1").unwrap();
    txn.store(b, Tid::ZERO, b"b1").unwrap();
    txn.vote().unwrap();
    let t1 = txn.finish().unwrap();
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(a, t1, b"a2").unwrap();
    txn.kv_set(Flavour::Durable, "registry", b"key", b"value", None)
        .unwrap();
    txn.vote().unwrap();
    let t2 = txn.finish().unwrap();
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.delete(b, t1).unwrap();
    txn.vote().unwrap();
    let t3 = txn.finish().unwrap();
    let mut file = store.new_blob_file().unwrap();
    file.write_all(b"blob bytes").unwrap();
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store_blob(c, Tid::ZERO, b"c1", file).unwrap();
    txn.vote().unwrap();
    let t4 = txn.finish().unwrap();
    vfs.mark("acknowledged");
    store.fold().unwrap();
    vfs.mark("folded");
    let images = vfs.crash_images();
    drop(store);

    assert!(images.iter().any(|image| image.is_after("folded")));
    for image in images.iter().filter(|image| image.is_after("acknowledged")) {
        let store = reopen(image, &held());
        assert_eq!(store.load(a).unwrap(), (b"a2".to_vec(), t2), "{image:?}");
        assert!(
            matches!(store.load(b), Err(ZoeError::NotFound { .. })),
            "{image:?}"
        );
        assert_eq!(
            store.load_serial(b, t1).unwrap(),
            b"b1".to_vec(),
            "{image:?}"
        );
        assert_eq!(store.load(c).unwrap(), (b"c1".to_vec(), t4), "{image:?}");
        store
            .verify_blob(c, t4)
            .unwrap_or_else(|e| panic!("{image:?}: {e}"));
        let value = store.kv_get(Flavour::Durable, "registry", b"key").unwrap();
        assert_eq!(value.as_deref(), Some(&b"value"[..]), "{image:?}");
        let tids: Vec<(Tid, usize)> = store
            .iterator(None, None)
            .unwrap()
            .map(|txn| {
                let txn = txn.unwrap_or_else(|e| panic!("{image:?}: {e}"));
                (txn.tid, txn.records.len())
            })
            .collect();
        assert_eq!(tids, [(t1, 2), (t2, 1), (t3, 1), (t4, 1)], "{image:?}");
        assert_eq!(store.object_count(), Some(3), "{image:?}");
        store.fold().unwrap();
        assert_eq!(store.object_count(), Some(3), "{image:?}");
    }
}

#[test]
fn every_crash_image_of_a_recovery_keeps_every_acknowledged_commit() {
    // A store that a kill left with four commits in its log.
    let killed = MemVfs::new();
    let store = open(&killed, held());
    let mut commits = Vec::new();
    for n in 0..4 {
        let oid = new_oid(&store);
        commits.push((oid, commit(&store, oid, Tid::ZERO, &[n; 100])));
    }
    let vfs = killed.crash_now(|pending| pending);
    drop(store);

    // The record of operations starts with the open that replays the log.
    let store = open(&vfs, held());
    for n in 0..4 {
        vfs.mark(&format!("acknowledged {n}"));
    }
    store.fold().unwrap();
    vfs.mark("folded");
    let oid = new_oid(&store);
    commits.push((oid, commit(&store, oid, Tid::ZERO, b"after the recovery")));
    vfs.mark("acknowledged 4");
    let images = vfs.crash_images();
    drop(store);

    for image in &images {
        let store = reopen(image, &held());
        let held = present(&store, &commits, image);
        assert!(held[..4].iter().all(|&p| p), "{image:?}: {held:?}");
        check_prefix(image, &held);
    }
}

#[test]
fn every_crash_image_of_a_clean_close_keeps_every_acknowledged_commit() {
    let vfs = created(&held());
    let store = open(&vfs, held());
    let mut commits = Vec::new();
    for n in 0..3 {
        let oid = new_oid(&store);
        commits.push((oid, commit(&store, oid, Tid::ZERO, &[n; 100])));
        vfs.mark(&format!("acknowledged {n}"));
    }
    // The close folds the log and marks its segments spare.
    drop(store);
    vfs.mark("closed");
    let images = vfs.crash_images();

    assert!(images.iter().any(|image| image.is_after("closed")));
    for image in &images {
        let store = reopen(image, &held());
        check_prefix(image, &present(&store, &commits, image));
        if image.is_after("closed") {
            assert_eq!(
                store.backlog_bytes(),
                0,
                "{image:?}: a clean close left a backlog"
            );
        }
    }
}
