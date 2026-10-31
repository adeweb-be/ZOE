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
//! segments and into a spare, a fold, an entry that the folded tid cuts in
//! two, one oid through the log and through direct batches, a pack that
//! waits for the fold while finishes run, the open that replays a log and
//! a clean close.

mod support;

use std::io::Write;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, SystemTime};

use support::{CountingClock, MeteredVfs, wal};
use zoe_core::testing::{commit, commit_all, new_oid, wait_until};
use zoe_core::vfs::{CrashImage, MemVfs};
use zoe_core::{Flavour, Oid, Options, Store, SystemClock, Tid, WriteLog, ZoeError, timestamp};

/// Settings under which no fold starts by itself, by size or by age.
fn held() -> WriteLog {
    WriteLog {
        fold_bytes: 1 << 40,
        fold_age: Duration::from_secs(3600),
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

#[test]
fn every_crash_image_of_an_entry_that_the_folded_tid_cuts_counts_its_objects() {
    // Each slice takes one commit, and the fold stops below 15 KiB: after
    // two commits of 10 KiB, in the middle of the entry of the second batch.
    let write_log = WriteLog {
        fold_bytes: 15 << 10,
        slice_bytes: 1,
        fold_age: Duration::from_secs(3600),
        backlog_bytes: 1 << 30,
        ..WriteLog::default()
    };
    let metered = MeteredVfs::over(created(&write_log));
    let clock = CountingClock::default();
    let options = options(write_log.clone());
    let store = Store::open_full(metered.clone(), clock.clone(), options).unwrap();
    let mut voted = Vec::new();
    for n in 0..3u8 {
        let oid = new_oid(&store);
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.store(oid, Tid::ZERO, &vec![n; 10 << 10]).unwrap();
        txn.vote().unwrap();
        voted.push((oid, txn));
    }
    let oids: Vec<Oid> = voted.iter().map(|(oid, _)| *oid).collect();

    let tids: Vec<Tid> = thread::scope(|scope| {
        // `Store::sync` holds the commit sequencer at the gate of the
        // database sync. So the first finish leads a batch of its own, and
        // the two others wait for the next batch: one entry, two commits.
        let held = metered.meter.hold();
        let sync = scope.spawn(|| store.sync().unwrap());
        wait_until(|| metered.meter.database_gate.waiting() == 1);
        let waiters = clock.waiters.load(Ordering::SeqCst);
        let mut finishes = Vec::new();
        for (n, (_, txn)) in voted.into_iter().enumerate() {
            finishes.push(scope.spawn(move || txn.finish().unwrap()));
            wait_until(|| clock.waiters.load(Ordering::SeqCst) > waiters + n);
        }
        thread::sleep(Duration::from_millis(100));
        drop(held);
        sync.join().unwrap();
        finishes.into_iter().map(|f| f.join().unwrap()).collect()
    });
    let commits: Vec<(Oid, Tid)> = oids.iter().copied().zip(tids.iter().copied()).collect();
    for n in 0..3 {
        metered.vfs.mark(&format!("acknowledged {n}"));
    }
    wait_until(|| store.backlog_bytes() < 15 << 10);
    metered.vfs.mark("folded");
    let two_in_one = wal::find(&metered.vfs, tids[2]).is_some_and(|(_, e)| e.previous == tids[0])
        || wal::find(&metered.vfs, tids[1]).is_some_and(|(_, e)| e.previous == Tid::ZERO);
    assert!(two_in_one, "no entry holds two commits");
    let images = metered.vfs.crash_images();
    drop(store);

    assert!(images.iter().any(|image| image.is_after("folded")));
    for image in &images {
        let store = reopen(image, &write_log);
        let held = present(&store, &commits, image);
        check_prefix(image, &held);
        let count = held.iter().filter(|&&p| p).count() as u64;
        assert_eq!(store.object_count(), Some(count), "{image:?}");
        store.fold().unwrap();
        assert_eq!(store.object_count(), Some(count), "{image:?}");
    }
}

/// Check that `store` holds a prefix of `versions` of `oid`, oldest first,
/// with each version that `image` acknowledged, through `history`, `load`
/// and `load_before`. A version is acknowledged after the mark
/// `acknowledged <n>`.
fn check_version_prefix(store: &Store, oid: Oid, versions: &[(Tid, Vec<u8>)], image: &CrashImage) {
    let history: Vec<Tid> = match store.history(oid, 100) {
        Ok(entries) => entries.into_iter().rev().map(|entry| entry.tid).collect(),
        Err(ZoeError::NotFound { .. }) => Vec::new(),
        Err(e) => panic!("{image:?}: {e}"),
    };
    let held = history.len();
    let expected: Vec<Tid> = versions[..held].iter().map(|(tid, _)| *tid).collect();
    assert_eq!(history, expected, "{image:?}: not a prefix");
    for n in 0..versions.len() {
        if image.is_after(&format!("acknowledged {n}")) {
            assert!(held > n, "{image:?}: acknowledged version {n} is lost");
        }
    }
    if let Some((tid, data)) = versions[..held].last() {
        assert_eq!(store.load(oid).unwrap(), (data.clone(), *tid), "{image:?}");
    }
    for (k, (tid, data)) in versions[..held].iter().enumerate() {
        let read = store.load_before(oid, Tid(tid.0 + 1)).unwrap().unwrap();
        assert_eq!((&read.data, read.serial), (data, *tid), "{image:?}: {k}");
        let next = versions[..held].get(k + 1).map(|(t, _)| *t);
        assert_eq!(read.next, next, "{image:?}: {k}");
    }
}

/// Commit `data` for `oid` over its last version in `versions`, add the
/// new version, and mark `<mark> <n>`, n the index of the version.
fn update(
    vfs: &MemVfs,
    store: &Store,
    oid: Oid,
    versions: &mut Vec<(Tid, Vec<u8>)>,
    data: Vec<u8>,
    mark: &str,
) {
    let serial = versions.last().map_or(Tid::ZERO, |(tid, _)| *tid);
    let tid = commit(store, oid, serial, &data);
    versions.push((tid, data));
    vfs.mark(&format!("{mark} {}", versions.len() - 1));
}

#[test]
fn every_crash_image_of_one_oid_through_the_log_and_direct_batches_keeps_its_versions() {
    const ACK: &str = "acknowledged";
    let vfs = created(&held());
    let store = open(&vfs, held());
    let x = new_oid(&store);
    let mut versions: Vec<(Tid, Vec<u8>)> = Vec::new();
    // Through the log.
    update(&vfs, &store, x, &mut versions, b"log 1".to_vec(), ACK);
    // A deferred restore: a direct batch with no sync, so a crash can lose
    // it until the next durable commit.
    let restored = timestamp(SystemTime::now() + Duration::from_secs(3600));
    let mut txn = store
        .tpc_begin_restore_deferred(restored, b"", b"", b"")
        .unwrap();
    txn.restore(x, Some(b"deferred")).unwrap();
    txn.vote().unwrap();
    txn.finish().unwrap();
    versions.push((restored, b"deferred".to_vec()));
    vfs.mark("returned 1");
    // A direct batch with a sync, which makes the restore durable.
    update(
        &vfs,
        &store,
        x,
        &mut versions,
        b"after the restore".to_vec(),
        ACK,
    );
    // Through the log, then a large record that finds that backlog and
    // goes to the log.
    update(&vfs, &store, x, &mut versions, b"log 2".to_vec(), ACK);
    update(&vfs, &store, x, &mut versions, vec![2; 100 << 10], ACK);
    store.fold().unwrap();
    vfs.mark("folded");
    // A large record on an empty backlog: a direct batch. Then the log.
    update(&vfs, &store, x, &mut versions, vec![3; 100 << 10], ACK);
    update(&vfs, &store, x, &mut versions, b"log 3".to_vec(), ACK);
    let images = vfs.crash_images();
    drop(store);

    assert!(images.iter().any(|image| image.is_after("acknowledged 6")));
    for image in &images {
        let store = reopen(image, &held());
        check_version_prefix(&store, x, &versions, image);
    }
}

/// Write `s` as a pickle text.
fn text(p: &mut Vec<u8>, s: &str) {
    p.push(b'X');
    p.extend_from_slice(&(s.len() as u32).to_le_bytes());
    p.extend_from_slice(s.as_bytes());
}

/// Return a record of the class `app.content.Item` with a title and a
/// persistent reference to each oid of `refs`.
fn item(title: &str, refs: &[Oid]) -> Vec<u8> {
    let mut p = b"\x80\x03capp.content\nItem\n.\x80\x03}(".to_vec();
    text(&mut p, "title");
    text(&mut p, title);
    for (n, oid) in refs.iter().enumerate() {
        text(&mut p, &format!("ref{n}"));
        p.extend_from_slice(b"C\x08");
        p.extend_from_slice(&oid.0.to_be_bytes());
        p.extend_from_slice(b"capp.content\nItem\n\x86Q");
    }
    p.extend_from_slice(b"u.");
    p
}

#[test]
fn every_crash_image_of_a_pack_that_waits_for_the_fold_while_finishes_run() {
    let vfs = created(&held());
    let store = open(&vfs, held());
    let root = Oid(0);
    let (x, g1, g2) = (new_oid(&store), new_oid(&store), new_oid(&store));
    // g1 and g2 are garbage: no record refers to them.
    let t1 = commit_all(
        &store,
        &[
            (root, Tid::ZERO, item("root", &[x])),
            (x, Tid::ZERO, item("x0", &[])),
            (g1, Tid::ZERO, item("g1", &[])),
            (g2, Tid::ZERO, item("g2", &[])),
        ],
    );
    let mut versions = vec![(t1, item("x0", &[]))];
    let t2 = commit(&store, x, t1, &item("x1", &[]));
    versions.push((t2, item("x1", &[])));
    vfs.mark("acknowledged 0");
    vfs.mark("acknowledged 1");

    thread::scope(|scope| {
        let pack = scope.spawn(|| store.pack(store.last_transaction()).unwrap());
        for n in 2..5 {
            let serial = versions.last().unwrap().0;
            let data = item(&format!("x{n}"), &[]);
            let tid = commit(&store, x, serial, &data);
            versions.push((tid, data));
            vfs.mark(&format!("acknowledged {n}"));
        }
        pack.join().unwrap();
    });
    vfs.mark("packed");
    let images = vfs.crash_images();
    drop(store);

    assert!(images.iter().any(|image| image.is_after("packed")));
    for image in &images {
        let store = reopen(image, &held());
        assert_eq!(store.load(root).unwrap().0, item("root", &[x]), "{image:?}");
        let (data, serial) = store.load(x).unwrap();
        let k = versions
            .iter()
            .position(|(tid, _)| *tid == serial)
            .unwrap_or_else(|| panic!("{image:?}: an unknown serial of x"));
        assert_eq!(data, versions[k].1, "{image:?}");
        for n in 0..versions.len() {
            if image.is_after(&format!("acknowledged {n}")) {
                assert!(k >= n, "{image:?}: acknowledged version {n} of x is lost");
            }
        }
        for g in [g1, g2] {
            match store.load(g) {
                Ok(_) | Err(ZoeError::NotFound { .. }) => {}
                Err(e) => panic!("{image:?}: {e}"),
            }
        }
    }
}
