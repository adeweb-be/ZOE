//! Acceptance test of the write-ahead log and its fold (ADR 0056; N1, N2,
//! N6, N7).
//!
//! A finish appends its commit batch to a segment of `wal/` and syncs that
//! segment alone. The records stay in the table of unfolded records, and
//! every read path reads that table before the tables of the backend, so
//! a load needs no second lookup. The fold writes the commits into the
//! tables later, in slices. The backlog is bounded, and a finish that
//! meets the bound waits for the fold. An open replays the log after the
//! folded tid, refuses a log that lost an acknowledged commit, reads a use
//! whose header is damaged from its first entry and never reuses its file,
//! reports an entry that it cuts, and fails on an I/O error. A reader sees
//! a prefix of the commits, also while the fold runs. A batch of large
//! records, a batch larger than half of the backlog and a batch of deferred
//! restores go into the tables directly. An idle store folds by age. The
//! format of such a store is `ZOE format 3`.
//!
//! The crash images are in `write_log_crash.rs`, the bytes of a commit in
//! `write_log_bytes.rs`, and the read-only opens in
//! `write_log_read_only.rs`.

mod support;

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime};

use support::{CountingClock, MeteredVfs, Rng, wal};
use zoe_core::testing::{self, ManualClock, commit, commit_all, commit_as, new_oid, try_commit};
use zoe_core::vfs::{MemVfs, Vfs};
use zoe_core::{
    DurableRow, Flavour, Observer, Oid, Options, PackReport, Store, SystemClock, Tid, WriteLog,
    ZoeError, timestamp,
};
use zoe_pickle::python::{Heap, Value};

/// Settings under which no fold starts by itself, by size or by age: only
/// `Store::fold`, a pack and a close fold.
fn held() -> WriteLog {
    WriteLog {
        fold_bytes: 1 << 40,
        fold_age: HOUR,
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

/// Open the store in `vfs` and return the error of the open.
fn refused(vfs: MemVfs, write_log: WriteLog) -> ZoeError {
    match Store::open_full(vfs, SystemClock, options(write_log)) {
        Ok(_) => panic!("the open passed"),
        Err(error) => error,
    }
}

/// Commit `count` new objects, one commit each, with `size` bytes of
/// pickle. Return the oid and the tid of each commit.
fn commit_new(store: &Store, count: usize, size: usize) -> Vec<(Oid, Tid)> {
    (0..count)
        .map(|n| {
            let oid = new_oid(store);
            (oid, commit(store, oid, Tid::ZERO, &vec![n as u8; size]))
        })
        .collect()
}

/// Commit `count` new objects in a store of `vfs` that no fold writes, then
/// return the state that a kill of the process leaves: every write landed,
/// and the store not closed.
fn killed_after(
    vfs: &MemVfs,
    write_log: WriteLog,
    count: usize,
    size: usize,
) -> (MemVfs, Vec<(Oid, Tid)>) {
    let store = open(vfs, write_log);
    let commits = commit_new(&store, count, size);
    let image = vfs.crash_now(|pending| pending);
    drop(store);
    (image, commits)
}

/// An observer that counts the slices of the fold and the waits of the
/// finishes for it.
#[derive(Default)]
struct Seen {
    slices: AtomicUsize,
    folded: AtomicUsize,
    waits: AtomicUsize,
    synced: AtomicUsize,
}

impl Observer for Seen {
    fn batch_synced(&self, commits: usize) {
        self.synced.fetch_add(commits, Ordering::SeqCst);
    }

    fn slice_folded(&self, commits: usize, _: u64, _: Duration) {
        self.slices.fetch_add(1, Ordering::SeqCst);
        self.folded.fetch_add(commits, Ordering::SeqCst);
    }

    fn backlog_waited(&self, _: Duration) {
        self.waits.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn a_store_of_format_3_opens_and_a_store_of_format_2_is_refused() {
    let vfs = MemVfs::new();
    let store = open(&vfs, WriteLog::default());
    let oid = new_oid(&store);
    let tid = commit(&store, oid, Tid::ZERO, b"pickle");
    testing::write_meta(&store, "format", b"ZOE format 3").unwrap();
    drop(store);

    let store = open(&vfs, WriteLog::default());
    assert_eq!(store.load(oid).unwrap(), (b"pickle".to_vec(), tid));
    testing::write_meta(&store, "format", b"ZOE format 2").unwrap();
    drop(store);

    // No migration: `zoe convert` through `Data.fs` moves the data.
    match refused(vfs, WriteLog::default()) {
        ZoeError::Format(text) => assert!(text.contains("ZOE format 2"), "{text}"),
        other => panic!("{other}"),
    }
}

#[test]
fn a_finish_syncs_its_log_entry_and_writes_no_byte_of_the_tables() {
    let metered = MeteredVfs::default();
    let store = Store::open_full(metered.clone(), SystemClock, options(held())).unwrap();
    let oids: Vec<Oid> = (0..100).map(|_| new_oid(&store)).collect();
    let created: Vec<_> = oids
        .iter()
        .map(|&o| (o, Tid::ZERO, vec![7; 1024]))
        .collect();
    let first = commit_all(&store, &created);
    store.fold().unwrap();
    assert_eq!(store.backlog_bytes(), 0);

    metered.meter.reset();
    let updates: Vec<_> = oids.iter().map(|&o| (o, first, vec![8; 1024])).collect();
    let second = commit_all(&store, &updates);
    assert_eq!(
        metered.meter.database_bytes(),
        0,
        "the finish wrote the tables"
    );
    assert_eq!(metered.meter.database_syncs(), 0);
    // The fold thread can also sync the segment that it grows.
    assert!(metered.meter.log_syncs() >= 1);
    assert!(metered.meter.log_bytes() >= 100 * 1024);
    assert!(store.backlog_bytes() >= 100 * 1024);
    for &oid in &oids {
        assert_eq!(store.load(oid).unwrap(), (vec![8; 1024], second));
    }

    store.fold().unwrap();
    assert_eq!(store.backlog_bytes(), 0);
    assert!(metered.meter.database_bytes() > 0);
    assert!(metered.meter.database_syncs() >= 1);
    for &oid in &oids {
        assert_eq!(store.load(oid).unwrap(), (vec![8; 1024], second));
    }
}

/// Read the integer of `len` bytes at `at` of `bytes`, little-endian.
fn le(bytes: &[u8], at: usize, len: usize) -> u64 {
    let mut value = [0; 8];
    value[..len].copy_from_slice(&bytes[at..at + len]);
    u64::from_le_bytes(value)
}

#[test]
fn the_log_entry_of_a_finish_has_the_layout_of_the_format() {
    let vfs = MemVfs::new();
    let store = open(&vfs, held());
    let (a, b) = (new_oid(&store), new_oid(&store));
    let first = commit_all(
        &store,
        &[
            (a, Tid::ZERO, b"a1".to_vec()),
            (b, Tid::ZERO, b"b1".to_vec()),
        ],
    );
    let second = commit(&store, a, first, b"a2");

    let (name, entry) = wal::find(&vfs, second).expect("the entry of the second commit");
    let segment = wal::segment(&vfs, &name).unwrap();
    assert_eq!(segment.version, 1);
    assert_eq!(segment.state, wal::IN_USE);
    assert_eq!(entry.epoch, segment.epoch);
    let (_, entry_one) = wal::find(&vfs, first).unwrap();
    assert_eq!(entry_one.previous, Tid::ZERO);
    assert_eq!(entry.previous, first);
    // One transaction row and one record.
    assert_eq!(entry.ops, 2);
    assert_eq!(entry_one.ops, 3);

    // The body: the transaction row, then each record, each a frame as the
    // tables store it.
    let body = wal::body(&vfs, &name, &entry);
    assert_eq!(body[0], 1, "the first op is the transaction row");
    assert_eq!(Tid(le(&body, 1, 8)), second);
    let row = le(&body, 9, 4) as usize;
    let at = 13 + row;
    assert_eq!(body[at], 2, "then a record");
    assert_eq!(Oid(le(&body, at + 1, 8)), a);
    let frame_len = le(&body, at + 9, 4) as usize;
    let frame = &body[at + 13..at + 13 + frame_len];
    assert_eq!(
        at + 13 + frame_len,
        body.len(),
        "the body holds nothing else"
    );
    // Bytes 8 to 15 of a frame are the previous serial, 16 to 19 the
    // length of the payload.
    assert_eq!(Tid(le(frame, 8, 8)), first);
    let stored = store.record_info(a, second).unwrap().stored_len;
    assert_eq!(le(frame, 16, 4), stored);
}

const REGISTRY: &str = "registry";
const SESSIONS: &str = "sessions";
const HOUR: Duration = Duration::from_secs(3600);

/// The oids and the tids of `write_history`, and an oid without a record.
struct History {
    oids: Vec<Oid>,
    tids: Vec<Tid>,
}

/// Commit a history that uses every kind of write: records, a deletion,
/// two blobs, namespace writes of both flavours, a delete of a namespace
/// key and a commit of namespace writes only. The first two commits go
/// into the tables, the others stay in the log.
fn write_history(store: &Store) -> History {
    let (a, b, c, d) = (
        new_oid(store),
        new_oid(store),
        new_oid(store),
        new_oid(store),
    );
    let t1 = commit_as(
        store,
        (b"user one", b"first", b""),
        &[
            (a, Tid::ZERO, b"a1".to_vec()),
            (b, Tid::ZERO, b"b1".to_vec()),
        ],
    );

    let mut file = store.new_blob_file().unwrap();
    file.write_all(b"the bytes of the first blob").unwrap();
    let mut txn = store.tpc_begin(b"user two", b"second", b"").unwrap();
    txn.store(a, t1, b"a2").unwrap();
    txn.store_blob(c, Tid::ZERO, b"c1", file).unwrap();
    txn.vote().unwrap();
    let t2 = txn.finish().unwrap();
    store.fold().unwrap();

    let mut txn = store.tpc_begin(b"", b"third", b"").unwrap();
    txn.delete(b, t1).unwrap();
    txn.kv_set(Flavour::Durable, REGISTRY, b"a", b"1", None)
        .unwrap();
    txn.kv_set(Flavour::Durable, REGISTRY, b"b", b"2", None)
        .unwrap();
    txn.kv_set(Flavour::Ephemeral, SESSIONS, b"a", b"s", Some(HOUR))
        .unwrap();
    txn.vote().unwrap();
    let t3 = txn.finish().unwrap();

    let mut file = store.new_blob_file().unwrap();
    file.write_all(b"the bytes of the second blob").unwrap();
    let mut txn = store.tpc_begin(b"", b"fourth", b"").unwrap();
    txn.store(a, t2, b"a3").unwrap();
    txn.store_blob(d, Tid::ZERO, b"d1", file).unwrap();
    txn.kv_delete(Flavour::Durable, REGISTRY, b"a").unwrap();
    txn.vote().unwrap();
    let t4 = txn.finish().unwrap();

    let mut txn = store.tpc_begin(b"", b"fifth", b"").unwrap();
    txn.kv_set(Flavour::Durable, REGISTRY, b"c", b"3", None)
        .unwrap();
    txn.vote().unwrap();
    let t5 = txn.finish().unwrap();

    History {
        oids: vec![a, b, c, d, Oid(1 << 40)],
        tids: vec![t1, t2, t3, t4, t5],
    }
}

/// Return what every read path of `store` gives for the history, as text,
/// in a fixed order.
fn view(store: &Store, history: &History) -> Vec<String> {
    let mut out = Vec::new();
    let mut at: Vec<Tid> = history.tids.iter().map(|t| Tid(t.0 + 1)).collect();
    at.extend(history.tids.iter().copied());
    at.push(Tid::MAX);
    for &oid in &history.oids {
        out.push(format!("load {oid:?}: {:?}", store.load(oid)));
        out.push(format!("history {oid:?}: {:?}", store.history(oid, 10)));
        for &tid in &at {
            let loaded = store.load_before(oid, tid);
            out.push(format!("load_before {oid:?} {tid:?}: {loaded:?}"));
        }
        for &tid in &history.tids {
            out.push(format!(
                "load_serial {oid:?} {tid:?}: {:?}",
                store.load_serial(oid, tid)
            ));
            out.push(format!(
                "record_info {oid:?} {tid:?}: {:?}",
                store.record_info(oid, tid)
            ));
            let blob = store.load_blob(oid, tid);
            out.push(format!("load_blob {oid:?} {tid:?}: {blob:?}"));
            if let Ok(Some(_)) = blob {
                let mut bytes = Vec::new();
                let read = store
                    .open_blob(oid, tid)
                    .and_then(|mut reader| Ok(reader.read_to_end(&mut bytes)?));
                out.push(format!("open_blob {oid:?} {tid:?}: {read:?} {bytes:?}"));
            }
        }
    }
    let transactions: Vec<_> = store.iterator(None, None).unwrap().collect();
    out.push(format!("iterator: {transactions:?}"));
    for (n, &start) in history.tids.iter().enumerate() {
        let part: Vec<_> = store.iterator(Some(start), Some(start)).unwrap().collect();
        out.push(format!("iterator of {n}: {part:?}"));
    }
    out.push(format!(
        "written_oids: {:?}",
        store.written_oids(Tid::ZERO, Tid::MAX, 100)
    ));
    let mut next = None;
    for _ in 0..10 {
        match store.record_iternext(next) {
            Ok(Some(current)) => {
                out.push(format!("record_iternext: {current:?}"));
                match current.next {
                    Some(oid) => next = Some(oid),
                    None => break,
                }
            }
            other => {
                out.push(format!("record_iternext: {other:?}"));
                break;
            }
        }
    }
    out.push(format!(
        "record_keys: {:?}",
        store.record_keys(Oid(0), 1000)
    ));
    out.push(format!(
        "txn_rows: {:?}",
        store.txn_rows().collect::<Vec<_>>()
    ));
    out.push(format!("undo_log: {:?}", store.undo_log(Tid::MAX, 100)));
    out.push(format!("object_count: {:?}", store.object_count()));
    out.push(format!("last_transaction: {:?}", store.last_transaction()));
    for (flavour, name) in [(Flavour::Durable, REGISTRY), (Flavour::Ephemeral, SESSIONS)] {
        let scan = store.kv_scan(flavour, name, None, None, 100);
        out.push(format!("kv_scan {flavour:?}: {scan:?}"));
        for key in [&b"a"[..], b"b", b"c"] {
            let value = store.kv_get(flavour, name, key);
            out.push(format!("kv_get {flavour:?} {key:?}: {value:?}"));
        }
    }
    out.push(format!(
        "kv_generation: {:?}",
        store.kv_generation(REGISTRY)
    ));
    out.push(format!("ephemeral: {:?}", store.has_ephemeral_namespaces()));
    out
}

#[test]
fn every_read_path_gives_the_same_answers_from_the_log_and_from_the_tables() {
    let clock = ManualClock::new();
    let vfs = MemVfs::new();
    let open = |vfs: MemVfs| Store::open_full(vfs, clock.clone(), options(held())).unwrap();
    let store = open(vfs.clone());
    let history = write_history(&store);
    // The last three commits are in the log only.
    assert!(store.backlog_bytes() > 0);
    let unfolded = view(&store, &history);
    // A kill now leaves the log for the next open to replay.
    let image = vfs.crash_now(|pending| pending);

    store.fold().unwrap();
    assert_eq!(store.backlog_bytes(), 0);
    assert_eq!(view(&store, &history), unfolded, "after the fold");
    drop(store);

    let reopened = open(vfs.clone());
    assert_eq!(reopened.backlog_bytes(), 0, "the close left a backlog");
    assert_eq!(view(&reopened, &history), unfolded, "after a clean close");
    drop(reopened);

    let replayed = open(image);
    assert_eq!(view(&replayed, &history), unfolded, "after a replay");
}

#[test]
fn a_vote_and_an_undo_read_the_records_of_the_log() {
    let vfs = MemVfs::new();
    let store = open(&vfs, held());
    let oid = new_oid(&store);
    let t1 = commit(&store, oid, Tid::ZERO, b"one");
    let t2 = commit(&store, oid, t1, b"two");
    assert!(store.backlog_bytes() > 0);

    // The current serial of the oid is in the log only.
    let mut stale = store.tpc_begin(b"", b"", b"").unwrap();
    stale.store(oid, t1, b"stale").unwrap();
    assert!(matches!(stale.vote(), Err(ZoeError::Conflict { .. })));
    assert_eq!(stale.conflict_serials(), Some((t2, t1)));
    drop(stale);

    // The undo reads the records of t2 and the record before them.
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.undo(t2).unwrap();
    txn.vote().unwrap();
    let t3 = txn.finish().unwrap();
    assert_eq!(store.load(oid).unwrap(), (b"one".to_vec(), t3));
    let read = store.load_before(oid, t3).unwrap().unwrap();
    assert_eq!(
        (read.data, read.serial, read.next),
        (b"two".to_vec(), t2, Some(t3))
    );
}

/// Write `s` as a pickle text.
fn text(p: &mut Vec<u8>, s: &str) {
    p.push(b'X');
    p.extend_from_slice(&(s.len() as u32).to_le_bytes());
    p.extend_from_slice(s.as_bytes());
}

/// Return a record of the class `app.content.Item` with a title and a
/// persistent reference, `(oid, class)`, to each oid of `refs`.
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

/// Commit a history in which a pack removes old revisions and an object
/// that no reader can reach. Fold it first when `fold` is true. Return the
/// pack report and the keys of the records that stay, by oid.
fn packed(fold: bool) -> (PackReport, Vec<(Oid, usize)>, usize) {
    let vfs = MemVfs::new();
    let store = open(&vfs, held());
    let root = Oid(0);
    let (x, y) = (new_oid(&store), new_oid(&store));
    let t1 = commit_all(
        &store,
        &[
            (root, Tid::ZERO, item("root", &[x, y])),
            (x, Tid::ZERO, item("x1", &[])),
            (y, Tid::ZERO, item("y1", &[])),
        ],
    );
    let t2 = commit_all(
        &store,
        &[(root, t1, item("root", &[x])), (x, t1, item("x2", &[]))],
    );
    commit(&store, x, t2, &item("x3", &[]));
    if fold {
        store.fold().unwrap();
    }
    let report = store.pack(store.last_transaction()).unwrap();
    // A pack folds the commits before it first: it removes records from
    // the tables alone.
    let backlog = store.backlog_bytes() as usize;
    let mut kept: Vec<(Oid, usize)> = Vec::new();
    for (oid, _) in store.record_keys(Oid(0), 1000).unwrap() {
        match kept.last_mut() {
            Some((last, count)) if *last == oid => *count += 1,
            _ => kept.push((oid, 1)),
        }
    }
    assert_eq!(store.load(x).unwrap().0, item("x3", &[]));
    assert_eq!(store.load(root).unwrap().0, item("root", &[x]));
    assert!(matches!(store.load(y), Err(ZoeError::NotFound { .. })));
    (report, kept, backlog)
}

#[test]
fn a_pack_of_commits_in_the_log_removes_what_a_pack_of_folded_commits_removes() {
    let (unfolded, unfolded_kept, backlog) = packed(false);
    let (folded, folded_kept, _) = packed(true);
    assert_eq!(backlog, 0, "the pack left a backlog");
    assert_eq!(unfolded, folded);
    assert_eq!(unfolded_kept, folded_kept);
    // The root and x keep their newest record; y goes.
    assert_eq!(unfolded_kept.len(), 2, "{unfolded_kept:?}");
    assert!(unfolded_kept.iter().all(|&(_, count)| count == 1));
}

#[test]
fn a_finish_waits_for_the_fold_when_the_backlog_is_full() {
    const RECORD: usize = 32 << 10;
    const BOUND: u64 = 256 << 10;
    let metered = MeteredVfs::default();
    let write_log = WriteLog {
        fold_bytes: 64 << 10,
        slice_bytes: 64 << 10,
        backlog_bytes: BOUND,
        ..WriteLog::default()
    };
    let store = Store::open_full(metered.clone(), SystemClock, options(write_log)).unwrap();
    let seen = Arc::new(Seen::default());
    assert!(store.set_observer(seen.clone()));
    let oids: Vec<Oid> = (0..20).map(|_| new_oid(&store)).collect();
    let done = AtomicUsize::new(0);

    let (finished, backlog) = thread::scope(|scope| {
        // The fold cannot end a slice: its sync waits at the gate. So the
        // backlog grows until a finish waits for the fold.
        let held = metered.meter.hold();
        let writer = scope.spawn(|| {
            for &oid in &oids {
                commit(&store, oid, Tid::ZERO, &vec![1; RECORD]);
                done.fetch_add(1, Ordering::SeqCst);
            }
        });
        testing::wait_until(|| store.backlog_bytes() + RECORD as u64 > BOUND);
        thread::sleep(Duration::from_millis(200));
        let seen = (done.load(Ordering::SeqCst), store.backlog_bytes());
        drop(held);
        writer.join().unwrap();
        seen
    });
    assert!(
        finished < oids.len(),
        "every finish passed while the fold waited"
    );
    assert!(backlog <= BOUND, "the backlog holds {backlog} bytes");
    assert!(seen.waits.load(Ordering::SeqCst) >= 1);
    for &oid in &oids {
        store.load(oid).unwrap();
    }
    store.fold().unwrap();
    assert_eq!(store.backlog_bytes(), 0);
    assert!(seen.slices.load(Ordering::SeqCst) >= 2);
    assert_eq!(seen.folded.load(Ordering::SeqCst), oids.len());
}

#[test]
fn a_failed_fold_halts_the_store_and_the_next_open_replays_the_log() {
    let metered = MeteredVfs::default();
    let store = Store::open_full(metered.clone(), SystemClock, options(held())).unwrap();
    let mut acknowledged = commit_new(&store, 3, 100);
    metered
        .meter
        .fail_database_writes
        .store(true, Ordering::SeqCst);
    // A finish writes no byte of the tables, so it passes.
    acknowledged.extend(commit_new(&store, 2, 100));

    assert!(matches!(store.fold(), Err(ZoeError::Halted)));
    let next = new_oid(&store);
    assert!(matches!(
        try_commit(&store, next, Tid::ZERO, b"refused"),
        Err(ZoeError::Halted)
    ));
    // The table of unfolded records still serves every commit.
    for &(oid, tid) in &acknowledged {
        assert_eq!(store.load(oid).unwrap().1, tid);
    }
    drop(store);

    metered
        .meter
        .fail_database_writes
        .store(false, Ordering::SeqCst);
    let reopened = open(&metered.vfs, held());
    for &(oid, tid) in &acknowledged {
        assert_eq!(reopened.load(oid).unwrap().1, tid);
    }
    assert!(matches!(
        reopened.load(next),
        Err(ZoeError::NotFound { .. })
    ));
    reopened.fold().unwrap();
    assert_eq!(reopened.backlog_bytes(), 0);
}

#[test]
fn a_batch_of_large_records_goes_into_the_tables_when_the_backlog_is_empty() {
    const LARGE: usize = 100 << 10;
    let metered = MeteredVfs::default();
    let store = Store::open_full(metered.clone(), SystemClock, options(held())).unwrap();
    let oids: Vec<Oid> = (0..10).map(|_| new_oid(&store)).collect();

    metered.meter.reset();
    let created: Vec<_> = oids
        .iter()
        .map(|&o| (o, Tid::ZERO, vec![1; LARGE]))
        .collect();
    let t1 = commit_all(&store, &created);
    assert_eq!(
        metered.meter.log_bytes(),
        0,
        "the large batch went to the log"
    );
    assert!(metered.meter.database_syncs() >= 1);
    assert_eq!(store.backlog_bytes(), 0);

    // A small commit fills the backlog, so the next large batch goes to
    // the log, and asks for a fold.
    commit_new(&store, 1, 10);
    let before = metered.meter.log_bytes();
    let updates: Vec<_> = oids.iter().map(|&o| (o, t1, vec![2; LARGE])).collect();
    let t2 = commit_all(&store, &updates);
    assert!(metered.meter.log_bytes() >= before + 10 * LARGE as u64);
    store.fold().unwrap();

    let before = metered.meter.log_bytes();
    let updates: Vec<_> = oids.iter().map(|&o| (o, t2, vec![3; LARGE])).collect();
    let t3 = commit_all(&store, &updates);
    assert_eq!(metered.meter.log_bytes(), before);
    for &oid in &oids {
        assert_eq!(store.load(oid).unwrap(), (vec![3; LARGE], t3));
    }
}

#[test]
fn a_batch_larger_than_half_of_the_backlog_waits_for_an_empty_backlog_and_skips_the_log() {
    let metered = MeteredVfs::default();
    let write_log = WriteLog {
        backlog_bytes: 1 << 20,
        fold_bytes: 1 << 20,
        slice_bytes: 256 << 10,
        ..WriteLog::default()
    };
    let store = Store::open_full(metered.clone(), SystemClock, options(write_log)).unwrap();
    let seen = Arc::new(Seen::default());
    store.set_observer(seen.clone());
    let small = commit_new(&store, 1, 100);
    assert!(store.backlog_bytes() > 0);

    // 600 records of 1 KiB: no large record, but more than half of the
    // bound of the backlog.
    let writes: Vec<_> = (0..600)
        .map(|_| (new_oid(&store), Tid::ZERO, vec![5; 1024]))
        .collect();
    let before = metered.meter.log_bytes();
    let tid = commit_all(&store, &writes);
    assert_eq!(
        metered.meter.log_bytes(),
        before,
        "the batch went to the log"
    );
    assert_eq!(store.backlog_bytes(), 0);
    assert!(seen.waits.load(Ordering::SeqCst) >= 1);
    assert_eq!(store.load(writes[599].0).unwrap(), (vec![5; 1024], tid));
    assert_eq!(store.load(small[0].0).unwrap().1, small[0].1);
}

#[test]
fn deferred_restores_go_into_the_tables_without_the_log() {
    let metered = MeteredVfs::default();
    let store = Store::open_full(metered.clone(), SystemClock, options(held())).unwrap();
    metered.meter.reset();
    for n in 1..=3 {
        let mut txn = store
            .tpc_begin_restore_deferred(Tid(n), b"", b"", b"")
            .unwrap();
        txn.restore(Oid(n), Some(b"restored")).unwrap();
        txn.vote().unwrap();
        txn.finish().unwrap();
    }
    assert_eq!(metered.meter.log_bytes(), 0);
    assert_eq!(metered.meter.log_syncs(), 0);
    assert_eq!(metered.meter.database_syncs(), 0);

    // The next normal finish goes into the tables with a sync, which makes
    // the deferred restores durable, as without the log.
    let oid = new_oid(&store);
    let tid = commit(&store, oid, Tid::ZERO, b"normal");
    assert_eq!(metered.meter.log_bytes(), 0);
    assert!(metered.meter.database_syncs() >= 1);

    // The finish after it goes to the log.
    commit(&store, oid, tid, b"logged");
    assert!(metered.meter.log_bytes() > 0);
}

#[test]
fn a_scan_of_a_namespace_skips_the_rows_that_the_log_deletes() {
    let vfs = MemVfs::new();
    let store = open(&vfs, held());
    let keys: Vec<Vec<u8>> = (0..600)
        .map(|n| format!("key{n:04}").into_bytes())
        .collect();
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    for key in &keys {
        txn.kv_set(Flavour::Durable, REGISTRY, key, b"v", None)
            .unwrap();
    }
    txn.vote().unwrap();
    txn.finish().unwrap();
    store.fold().unwrap();

    // The log deletes the first 400 keys, more than one chunk of the scan,
    // and adds one key.
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    for key in &keys[..400] {
        txn.kv_delete(Flavour::Durable, REGISTRY, key).unwrap();
    }
    txn.kv_set(Flavour::Durable, REGISTRY, b"new", b"n", None)
        .unwrap();
    txn.vote().unwrap();
    txn.finish().unwrap();
    assert!(store.backlog_bytes() > 0);

    let mut found = Vec::new();
    let mut start: Option<Vec<u8>> = None;
    loop {
        let page = store
            .kv_scan(Flavour::Durable, REGISTRY, start.as_deref(), None, 1000)
            .unwrap();
        let Some((last, _)) = page.last() else {
            break;
        };
        let mut after = last.clone();
        after.push(0);
        start = Some(after);
        found.extend(page.into_iter().map(|(key, _)| key));
    }
    let mut expected: Vec<Vec<u8>> = keys[400..].to_vec();
    expected.push(b"new".to_vec());
    assert_eq!(found, expected);
    assert_eq!(
        store.kv_get(Flavour::Durable, REGISTRY, &keys[0]).unwrap(),
        None
    );
}

#[test]
fn segments_rotate_and_their_number_stays_bounded() {
    let vfs = MemVfs::new();
    let write_log = WriteLog {
        segment_bytes: 16 << 10,
        fold_bytes: 64 << 10,
        slice_bytes: 32 << 10,
        backlog_bytes: 256 << 10,
        ..WriteLog::default()
    };
    let store = open(&vfs, write_log.clone());
    let commits = commit_new(&store, 300, 2 << 10);
    // The backlog fills at most 16 segments of 16 KiB, with one more in
    // use and at most two spares.
    let files = wal::files(&vfs).len();
    assert!(files >= 2, "the log did not rotate: {files} file");
    assert!(files <= 16 + 4, "{files} files in wal/");
    for (n, &(oid, tid)) in commits.iter().enumerate() {
        assert_eq!(store.load(oid).unwrap(), (vec![n as u8; 2 << 10], tid));
    }
    drop(store);

    let reopened = open(&vfs, write_log);
    assert_eq!(reopened.backlog_bytes(), 0);
    for (n, &(oid, tid)) in commits.iter().enumerate() {
        assert_eq!(reopened.load(oid).unwrap(), (vec![n as u8; 2 << 10], tid));
    }
}

#[test]
fn a_clean_close_folds_the_log_and_marks_each_segment_spare() {
    let vfs = MemVfs::new();
    let store = open(&vfs, held());
    let seen = Arc::new(Seen::default());
    store.set_observer(seen.clone());
    let commits = commit_new(&store, 10, 100);
    assert_eq!(seen.synced.load(Ordering::SeqCst), 10);
    store.fold().unwrap();
    assert_eq!(seen.folded.load(Ordering::SeqCst), 10);
    assert!(seen.slices.load(Ordering::SeqCst) >= 1);
    let later = commit_new(&store, 3, 100);
    drop(store);

    let files = wal::files(&vfs);
    assert!(!files.is_empty());
    for file in &files {
        let segment = wal::segment(&vfs, file).unwrap_or_else(|| panic!("{file}: no header"));
        assert_eq!(segment.state, wal::SPARE, "{file}");
    }
    let reopened = open(&vfs, held());
    assert_eq!(reopened.backlog_bytes(), 0);
    for &(oid, tid) in commits.iter().chain(&later) {
        assert_eq!(reopened.load(oid).unwrap().1, tid);
    }
}

#[test]
fn the_first_entry_after_an_open_starts_a_new_use_of_a_segment() {
    let vfs = MemVfs::new();
    let (image, commits) = killed_after(&vfs, held(), 2, 100);
    let store = open(&image, held());
    let after = commit_new(&store, 1, 100);
    let (name, entry) = wal::find(&image, after[0].1).unwrap();
    assert_eq!(entry.offset, wal::SEGMENT_HEADER);
    assert_eq!(entry.previous, commits[1].1);
    let epoch = wal::segment(&image, &name).unwrap().epoch;
    for other in wal::files(&image) {
        if other != name
            && let Some(segment) = wal::segment(&image, &other)
        {
            assert!(segment.epoch < epoch, "{other}: epoch {}", segment.epoch);
        }
    }
}

#[test]
fn a_damaged_entry_before_a_later_entry_fails_the_open_with_corrupt_txn() {
    let vfs = MemVfs::new();
    let (image, commits) = killed_after(&vfs, held(), 2, 100);
    let (first, second) = (commits[0].1, commits[1].1);
    let (name, entry) = wal::find(&image, first).unwrap();
    assert_eq!(wal::find(&image, second).unwrap().1.previous, first);
    wal::flip(&image, &name, entry.body() + 3);
    match refused(image, held()) {
        ZoeError::CorruptTxn { tid } => assert_eq!(tid, first),
        other => panic!("{other}"),
    }
}

#[test]
fn a_damaged_last_entry_is_cut_as_a_torn_tail() {
    let vfs = MemVfs::new();
    let (image, commits) = killed_after(&vfs, held(), 2, 100);
    let (name, entry) = wal::find(&image, commits[1].1).unwrap();
    wal::flip(&image, &name, entry.body() + 3);
    let store = open(&image, held());
    assert_eq!(store.load(commits[0].0).unwrap().1, commits[0].1);
    assert!(matches!(
        store.load(commits[1].0),
        Err(ZoeError::NotFound { .. })
    ));
    assert_eq!(store.last_transaction(), commits[0].1);
    // The header of the cut entry passed: the open reports its tids.
    assert_eq!(store.cut_at_open(), Some((commits[0].1, commits[1].1)));
    let after = commit_new(&store, 1, 100);
    assert!(after[0].1 > commits[0].1);
}

#[test]
fn a_damaged_entry_that_the_fold_wrote_does_not_stop_the_open() {
    let vfs = MemVfs::new();
    let store = open(&vfs, held());
    let first = commit_new(&store, 1, 100);
    store.fold().unwrap();
    let second = commit_new(&store, 1, 100);
    let image = vfs.crash_now(|pending| pending);
    drop(store);

    let (name, entry) = wal::find(&image, first[0].1).unwrap();
    assert_eq!(wal::find(&image, second[0].1).unwrap().0, name);
    wal::flip(&image, &name, entry.body() + 3);
    let store = open(&image, held());
    for &(oid, tid) in first.iter().chain(&second) {
        assert_eq!(store.load(oid).unwrap().1, tid);
    }
}

#[test]
fn a_missing_segment_of_unfolded_commits_fails_the_open() {
    let vfs = MemVfs::new();
    let write_log = WriteLog {
        segment_bytes: 4096,
        ..held()
    };
    let (image, commits) = killed_after(&vfs, write_log.clone(), 2, 3000);
    let (segment_one, _) = wal::find(&image, commits[0].1).unwrap();
    let (segment_two, _) = wal::find(&image, commits[1].1).unwrap();
    assert_ne!(segment_one, segment_two, "the log did not rotate");
    image.remove(&segment_one).unwrap();
    match refused(image, write_log) {
        ZoeError::CorruptTxn { tid } => assert_eq!(tid, commits[0].1),
        other => panic!("{other}"),
    }
}

#[test]
fn readers_see_each_acknowledged_commit_while_the_fold_moves_it() {
    const OIDS: usize = 16;
    const COMMITS: usize = 400;
    let vfs = MemVfs::new();
    let write_log = WriteLog {
        fold_bytes: 32 << 10,
        slice_bytes: 16 << 10,
        backlog_bytes: 1 << 20,
        ..WriteLog::default()
    };
    let store = open(&vfs, write_log);
    let oids: Vec<Oid> = (0..OIDS).map(|_| new_oid(&store)).collect();
    // The acknowledged tids of each oid, in commit order.
    let acknowledged: Vec<Mutex<Vec<Tid>>> = (0..OIDS).map(|_| Mutex::new(Vec::new())).collect();
    let done = AtomicBool::new(false);

    thread::scope(|scope| {
        scope.spawn(|| {
            for n in 0..COMMITS {
                let i = n % OIDS;
                let serial = acknowledged[i].lock().unwrap().last().copied();
                let data = format!("{n:08}").repeat(128);
                let tid = commit(
                    &store,
                    oids[i],
                    serial.unwrap_or(Tid::ZERO),
                    data.as_bytes(),
                );
                acknowledged[i].lock().unwrap().push(tid);
            }
            done.store(true, Ordering::SeqCst);
        });
        for reader in 0..4 {
            let (store, oids, acknowledged, done) = (&store, &oids, &acknowledged, &done);
            scope.spawn(move || {
                let mut rng = Rng::new(reader + 1);
                loop {
                    let finished = done.load(Ordering::SeqCst);
                    let i = rng.below(OIDS);
                    let tids = acknowledged[i].lock().unwrap().clone();
                    if let Some(&last) = tids.last() {
                        let (_, serial) = store.load(oids[i]).unwrap();
                        assert!(serial >= last, "an older serial than an acknowledged one");
                        let k = rng.below(tids.len());
                        let read = store
                            .load_before(oids[i], Tid(tids[k].0 + 1))
                            .unwrap()
                            .unwrap();
                        assert_eq!(read.serial, tids[k]);
                        assert_eq!(read.next, tids.get(k + 1).copied().or(read.next));
                        store.load_serial(oids[i], tids[k]).unwrap();
                    }
                    if finished {
                        break;
                    }
                }
            });
        }
    });
    store.fold().unwrap();
    for (i, &oid) in oids.iter().enumerate() {
        let tids = acknowledged[i].lock().unwrap();
        assert_eq!(store.history(oid, COMMITS).unwrap().len(), tids.len());
    }
}

#[test]
fn a_damaged_header_of_the_current_use_loses_no_commit_and_the_file_is_not_reused() {
    let vfs = MemVfs::new();
    let (image, commits) = killed_after(&vfs, held(), 3, 100);
    let (name, _) = wal::find(&image, commits[2].1).unwrap();
    // A byte of the epoch: the header no longer passes its checksum.
    wal::flip(&image, &name, 20);
    assert_eq!(wal::segment(&image, &name), None);

    let store = open(&image, held());
    for &(oid, tid) in &commits {
        assert_eq!(store.load(oid).unwrap().1, tid);
    }
    let after = commit_new(&store, 2, 100);
    for &(_, tid) in &after {
        let (segment, _) = wal::find(&image, tid).unwrap();
        assert_ne!(segment, name, "a new use of a file whose header failed");
    }
    store.fold().unwrap();
    assert!(
        !wal::files(&image).contains(&name),
        "the fold kept a file whose header failed"
    );
    drop(store);

    let reopened = open(&image, held());
    for &(oid, tid) in commits.iter().chain(&after) {
        assert_eq!(reopened.load(oid).unwrap().1, tid);
    }
}

#[test]
fn a_damaged_header_and_first_entry_fail_the_open_when_later_entries_hold_commits() {
    let vfs = MemVfs::new();
    let (image, commits) = killed_after(&vfs, held(), 3, 100);
    let (name, first) = wal::find(&image, commits[0].1).unwrap();
    assert_eq!(first.offset, wal::SEGMENT_HEADER);
    wal::flip(&image, &name, 20);
    // A byte of the previous tid of the first entry.
    wal::flip(&image, &name, first.offset + 26);
    // The two later entries are evidence of synced commits: no silent loss.
    match refused(image, held()) {
        ZoeError::CorruptTxn { tid } => assert_eq!(tid, commits[0].1),
        other => panic!("{other}"),
    }
}

#[test]
fn a_damaged_entry_header_in_the_middle_of_a_segment_fails_the_open() {
    let vfs = MemVfs::new();
    let (image, commits) = killed_after(&vfs, held(), 3, 100);
    let (name, middle) = wal::find(&image, commits[1].1).unwrap();
    // A byte of the previous tid: the header of the entry fails.
    wal::flip(&image, &name, middle.offset + 26);
    match refused(image, held()) {
        ZoeError::CorruptTxn { tid } => assert_eq!(tid, commits[1].1),
        other => panic!("{other}"),
    }
}

#[test]
fn a_damaged_last_entry_of_a_segment_that_is_not_the_last_fails_the_open() {
    let vfs = MemVfs::new();
    let write_log = WriteLog {
        segment_bytes: 4096,
        ..held()
    };
    let (image, commits) = killed_after(&vfs, write_log.clone(), 2, 3000);
    let (segment_one, entry) = wal::find(&image, commits[0].1).unwrap();
    assert_ne!(wal::find(&image, commits[1].1).unwrap().0, segment_one);
    wal::flip(&image, &segment_one, entry.body() + 3);
    // The next segment names the cut entry as the one before it.
    match refused(image, write_log) {
        ZoeError::CorruptTxn { tid } => assert_eq!(tid, commits[0].1),
        other => panic!("{other}"),
    }
}

#[test]
fn an_io_error_on_a_read_of_the_log_fails_the_open() {
    let vfs = MemVfs::new();
    let (image, _) = killed_after(&vfs, held(), 2, 100);
    let metered = MeteredVfs::over(image);
    metered.meter.fail_log_reads.store(true, Ordering::SeqCst);
    match Store::open_full(metered.clone(), SystemClock, options(held())) {
        Err(ZoeError::Io(_)) => {}
        Err(other) => panic!("{other}"),
        Ok(_) => panic!("the open read a failed read as a torn tail"),
    }
}

/// Load each version of `oid` that `versions` gives, oldest first, through
/// `load`, `load_before` and `history`.
fn check_versions(store: &Store, oid: Oid, versions: &[(Tid, Vec<u8>)]) {
    let (last_tid, last_data) = versions.last().unwrap();
    assert_eq!(store.load(oid).unwrap(), (last_data.clone(), *last_tid));
    for (k, (tid, data)) in versions.iter().enumerate() {
        let read = store.load_before(oid, Tid(tid.0 + 1)).unwrap().unwrap();
        assert_eq!((&read.data, read.serial), (data, *tid), "version {k}");
        assert_eq!(
            read.next,
            versions.get(k + 1).map(|(t, _)| *t),
            "version {k}"
        );
    }
    let history: Vec<Tid> = store
        .history(oid, 100)
        .unwrap()
        .into_iter()
        .map(|entry| entry.tid)
        .collect();
    let expected: Vec<Tid> = versions.iter().rev().map(|(tid, _)| *tid).collect();
    assert_eq!(history, expected);
}

/// Commit `data` for `oid` over its last version in `versions`, and add the
/// new version.
fn update(store: &Store, oid: Oid, versions: &mut Vec<(Tid, Vec<u8>)>, data: Vec<u8>) {
    let serial = versions.last().map_or(Tid::ZERO, |(tid, _)| *tid);
    let tid = commit(store, oid, serial, &data);
    versions.push((tid, data));
}

#[test]
fn one_oid_written_through_the_log_and_through_direct_batches_reads_in_order() {
    let vfs = MemVfs::new();
    let write_log = WriteLog {
        backlog_bytes: 1 << 20,
        fold_bytes: 1 << 20,
        fold_age: HOUR,
        ..WriteLog::default()
    };
    let store = open(&vfs, write_log.clone());
    let x = new_oid(&store);
    let mut versions = Vec::new();

    // Through the log.
    update(&store, x, &mut versions, b"log 1".to_vec());
    // A deferred restore: a direct batch, after the fold of the backlog.
    let restored = timestamp(SystemTime::now() + HOUR);
    let mut txn = store
        .tpc_begin_restore_deferred(restored, b"", b"", b"")
        .unwrap();
    txn.restore(x, Some(b"deferred restore")).unwrap();
    txn.vote().unwrap();
    txn.finish().unwrap();
    versions.push((restored, b"deferred restore".to_vec()));
    assert_eq!(store.backlog_bytes(), 0);
    // After it, a direct batch with a sync.
    update(&store, x, &mut versions, b"after the restore".to_vec());
    // y fills the backlog, then a batch larger than half of the bound,
    // with x, waits for an empty backlog.
    commit_new(&store, 1, 10);
    assert!(store.backlog_bytes() > 0);
    let serial = versions.last().unwrap().0;
    let mut writes = vec![(x, serial, b"oversized".to_vec())];
    writes.extend((0..600).map(|_| (new_oid(&store), Tid::ZERO, vec![1; 1024])));
    versions.push((commit_all(&store, &writes), b"oversized".to_vec()));
    assert_eq!(store.backlog_bytes(), 0);
    // Through the log, then a large record that finds that backlog.
    update(&store, x, &mut versions, b"log 2".to_vec());
    update(&store, x, &mut versions, vec![2; 100 << 10]);
    store.fold().unwrap();
    // A large record on an empty backlog: a direct batch.
    update(&store, x, &mut versions, vec![3; 100 << 10]);
    update(&store, x, &mut versions, b"log 3".to_vec());
    check_versions(&store, x, &versions);

    // A kill, then an open with the log off: the open replays the log, and
    // the next batch waits for its fold.
    let image = vfs.crash_now(|pending| pending);
    drop(store);
    let off = WriteLog {
        enabled: false,
        ..write_log
    };
    let store = open(&image, off);
    check_versions(&store, x, &versions);
    update(&store, x, &mut versions, b"with the log off".to_vec());
    assert_eq!(store.backlog_bytes(), 0);
    check_versions(&store, x, &versions);
}

/// Return the number that the first 8 bytes of `data` give.
fn number(data: &[u8]) -> u64 {
    u64::from_be_bytes(data[..8].try_into().unwrap())
}

#[test]
fn readers_never_see_half_of_a_commit() {
    const COMMITS: u64 = 300;
    let vfs = MemVfs::new();
    let write_log = WriteLog {
        fold_bytes: 32 << 10,
        slice_bytes: 16 << 10,
        fold_age: Duration::from_millis(50),
        ..WriteLog::default()
    };
    let store = open(&vfs, write_log);
    let (a, b) = (new_oid(&store), new_oid(&store));
    let value = |n: u64| n.to_be_bytes().repeat(64);
    let mut serial = commit_all(
        &store,
        &[(a, Tid::ZERO, value(0)), (b, Tid::ZERO, value(0))],
    );
    let done = AtomicBool::new(false);

    thread::scope(|scope| {
        scope.spawn(|| {
            for n in 1..=COMMITS {
                let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
                txn.store(a, serial, &value(n)).unwrap();
                txn.store(b, serial, &value(n)).unwrap();
                for key in [b"k1", b"k2"] {
                    txn.kv_set(Flavour::Durable, REGISTRY, key, &n.to_be_bytes(), None)
                        .unwrap();
                }
                txn.vote().unwrap();
                serial = txn.finish().unwrap();
            }
            done.store(true, Ordering::SeqCst);
        });
        for _ in 0..4 {
            scope.spawn(|| {
                loop {
                    let finished = done.load(Ordering::SeqCst);
                    // a is read first: once a shows commit n, b shows n or later.
                    let first = number(&store.load(a).unwrap().0);
                    let second = number(&store.load(b).unwrap().0);
                    assert!(second >= first, "a at {first}, b at {second}");
                    let get = |key: &[u8]| {
                        store
                            .kv_get(Flavour::Durable, REGISTRY, key)
                            .unwrap()
                            .map_or(0, |value| number(&value))
                    };
                    let (k1, k2) = (get(b"k1"), get(b"k2"));
                    assert!(k2 >= k1, "k1 at {k1}, k2 at {k2}");
                    let pairs = store
                        .kv_scan(Flavour::Durable, REGISTRY, None, None, 10)
                        .unwrap();
                    if pairs.len() == 2 {
                        assert_eq!(pairs[0].1, pairs[1].1, "a scan saw half of a commit");
                    }
                    let tid = store.last_transaction();
                    let at = Tid(tid.0 + 1);
                    let (ra, rb) = (
                        store.load_before(a, at).unwrap().unwrap(),
                        store.load_before(b, at).unwrap().unwrap(),
                    );
                    assert_eq!((ra.serial, &ra.data), (rb.serial, &rb.data));
                    if finished {
                        break;
                    }
                }
            });
        }
    });
}

/// The acknowledged commits of a writer: their tids, their oids and their
/// namespace keys.
type Acknowledged = (Vec<Tid>, Vec<Oid>, Vec<Vec<u8>>);

#[test]
fn scans_during_folds_give_no_duplicate_no_gap_and_no_error() {
    const COMMITS: usize = 150;
    const RECORDS: usize = 3;
    let vfs = MemVfs::new();
    let write_log = WriteLog {
        fold_bytes: 16 << 10,
        slice_bytes: 8 << 10,
        fold_age: Duration::from_millis(20),
        ..WriteLog::default()
    };
    let store = open(&vfs, write_log);
    // The acknowledged commits, their oids and their namespace keys.
    let acknowledged: Mutex<Acknowledged> = Mutex::default();
    let done = AtomicBool::new(false);

    thread::scope(|scope| {
        scope.spawn(|| {
            for n in 0..COMMITS {
                let oids: Vec<Oid> = (0..RECORDS).map(|_| new_oid(&store)).collect();
                let key = format!("key{n:05}").into_bytes();
                let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
                for &oid in &oids {
                    txn.store(oid, Tid::ZERO, &[n as u8; 512]).unwrap();
                }
                txn.kv_set(Flavour::Durable, REGISTRY, &key, b"v", None)
                    .unwrap();
                txn.vote().unwrap();
                let tid = txn.finish().unwrap();
                let mut seen = acknowledged.lock().unwrap();
                seen.0.push(tid);
                seen.1.extend(oids);
                seen.2.push(key);
            }
            done.store(true, Ordering::SeqCst);
        });
        for seed in 1..=2 {
            let (store, acknowledged, done) = (&store, &acknowledged, &done);
            scope.spawn(move || {
                let mut rng = Rng::new(seed);
                loop {
                    let finished = done.load(Ordering::SeqCst);
                    let (tids, oids, keys) = acknowledged.lock().unwrap().clone();

                    let found: Vec<Tid> = store
                        .iterator(None, None)
                        .unwrap()
                        .map(|txn| {
                            let txn = txn.unwrap();
                            assert_eq!(txn.records.len(), RECORDS, "{:?}", txn.tid);
                            txn.tid
                        })
                        .collect();
                    assert!(found.windows(2).all(|w| w[0] < w[1]), "the iterator");
                    assert!(tids.iter().all(|tid| found.contains(tid)), "a gap");
                    if !tids.is_empty() {
                        let tid = tids[rng.below(tids.len())];
                        let one: Vec<_> = store.iterator(Some(tid), Some(tid)).unwrap().collect();
                        assert_eq!(one.len(), 1);
                        assert_eq!(one[0].as_ref().unwrap().records.len(), RECORDS);
                    }

                    let mut walked = Vec::new();
                    let mut next = None;
                    while let Some(current) = store.record_iternext(next).unwrap() {
                        walked.push(current.oid);
                        match current.next {
                            Some(oid) => next = Some(oid),
                            None => break,
                        }
                    }
                    assert!(walked.windows(2).all(|w| w[0] < w[1]), "record_iternext");
                    assert!(oids.iter().all(|oid| walked.contains(oid)), "a gap");

                    let keys_found = store.record_keys(Oid(0), 1 << 20).unwrap();
                    assert!(keys_found.windows(2).all(|w| w[0] < w[1]), "record_keys");
                    assert!(
                        oids.iter()
                            .all(|oid| keys_found.iter().any(|(o, _)| o == oid))
                    );

                    let log: Vec<Tid> = store
                        .undo_log(Tid::MAX, 10_000)
                        .unwrap()
                        .into_iter()
                        .map(|row| row.tid)
                        .collect();
                    assert!(log.windows(2).all(|w| w[0] > w[1]), "undo_log");
                    assert!(tids.iter().all(|tid| log.contains(tid)), "a gap");

                    let pairs = store
                        .kv_scan(Flavour::Durable, REGISTRY, None, None, 1000)
                        .unwrap();
                    assert!(pairs.windows(2).all(|w| w[0].0 < w[1].0), "kv_scan");
                    assert!(keys.iter().all(|key| pairs.iter().any(|(k, _)| k == key)));
                    if finished {
                        break;
                    }
                }
            });
        }
    });
}

/// Return the record of a `BTrees.Length.Length` of `value`.
fn length(value: i128) -> Vec<u8> {
    let mut heap = Heap::new();
    let class = heap.new_class("BTrees.Length", "Length");
    testing::record(&heap, class, Value::Int(value))
}

#[test]
fn the_object_count_does_not_count_a_record_merged_onto_a_commit_in_flight() {
    let clock = CountingClock::default();
    let metered = MeteredVfs::default();
    let store = Store::open_full(metered.clone(), clock.clone(), options(held())).unwrap();
    let oid = new_oid(&store);
    let read = commit(&store, oid, Tid::ZERO, &length(0));
    commit_new(&store, 2, 10);
    let count = store.object_count();
    assert_eq!(count, Some(3));
    let mut first = store.tpc_begin(b"", b"", b"").unwrap();
    first.store(oid, read, &length(1)).unwrap();
    first.vote().unwrap();

    let (one, two) = thread::scope(|scope| {
        let waits = clock.waits.load(Ordering::SeqCst);
        let second = scope.spawn(|| {
            let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
            txn.store(oid, read, &length(1)).unwrap();
            txn.vote().map(|()| txn)
        });
        // The second vote waits for the lock of the first transaction.
        testing::wait_until(|| clock.waits.load(Ordering::SeqCst) > waits);
        thread::sleep(Duration::from_millis(200));
        // `Store::sync` holds the commit sequencer at the gate of the
        // database sync, so the first commit stays in flight.
        let held = metered.meter.hold();
        let sync = scope.spawn(|| store.sync().unwrap());
        testing::wait_until(|| metered.meter.database_gate.waiting() == 1);
        let one = scope.spawn(move || first.finish().unwrap());
        // The finish hands the lock off, and the vote merges onto the
        // record of the commit in flight: its serial is a placeholder.
        let second = second.join().unwrap().unwrap();
        assert_eq!(second.natively_resolved(), [oid]);
        let two = scope.spawn(move || second.finish().unwrap());
        thread::sleep(Duration::from_millis(50));
        drop(held);
        sync.join().unwrap();
        (one.join().unwrap(), two.join().unwrap())
    });
    assert!(two > one);
    assert_eq!(store.load(oid).unwrap(), (length(2), two));
    assert_eq!(store.load_before(oid, two).unwrap().unwrap().serial, one);
    assert_eq!(
        store.object_count(),
        count,
        "a merged record counted as new"
    );
    store.fold().unwrap();
    assert_eq!(store.object_count(), count);
}

#[test]
fn a_pack_that_removes_an_oid_that_a_log_commit_writes_counts_it_after_the_fold() {
    let vfs = MemVfs::new();
    let store = open(&vfs, held());
    let root = Oid(0);
    let z = new_oid(&store);
    // No record refers to z, so a pack removes it.
    let t1 = commit_all(
        &store,
        &[
            (root, Tid::ZERO, item("root", &[])),
            (z, Tid::ZERO, item("z1", &[])),
        ],
    );
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(z, t1, &item("z2", &[])).unwrap();
    txn.vote().unwrap();
    store.pack(store.last_transaction()).unwrap();
    assert!(matches!(
        store.load_serial(z, t1),
        Err(ZoeError::NotFound { .. })
    ));
    let t2 = txn.finish().unwrap();
    assert_eq!(store.load(z).unwrap(), (item("z2", &[]), t2));

    let mut oids: Vec<Oid> = store
        .record_keys(Oid(0), 1000)
        .unwrap()
        .into_iter()
        .map(|(oid, _)| oid)
        .collect();
    oids.dedup();
    let exact = oids.len() as u64;
    assert_eq!(exact, 2);
    // Until the slice that folds the commit, the count can be one lower
    // (ADR 0056, section 19).
    let count = store.object_count().unwrap();
    assert!(count == exact || count + 1 == exact, "{count} for {exact}");
    store.fold().unwrap();
    assert_eq!(store.object_count(), Some(exact));
}

#[test]
fn a_batch_of_namespace_writes_only_goes_to_the_log_on_an_empty_backlog() {
    let metered = MeteredVfs::default();
    let store = Store::open_full(metered.clone(), SystemClock, options(held())).unwrap();
    assert_eq!(store.backlog_bytes(), 0);
    metered.meter.reset();
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.kv_set(Flavour::Durable, REGISTRY, b"key", b"value", None)
        .unwrap();
    txn.vote().unwrap();
    txn.finish().unwrap();
    // No large record: no direct batch.
    assert!(metered.meter.log_bytes() > 0);
    assert_eq!(metered.meter.database_syncs(), 0);
    assert!(store.backlog_bytes() > 0);
    let value = store.kv_get(Flavour::Durable, REGISTRY, b"key").unwrap();
    assert_eq!(value.as_deref(), Some(&b"value"[..]));
}

#[test]
fn a_restore_of_durable_rows_lands_over_the_log_and_beside_a_writer() {
    let vfs = MemVfs::new();
    let store = open(&vfs, held());
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.kv_set(Flavour::Durable, REGISTRY, b"k", b"logged", None)
        .unwrap();
    txn.vote().unwrap();
    txn.finish().unwrap();
    assert!(store.backlog_bytes() > 0);

    thread::scope(|scope| {
        let writer = scope.spawn(|| {
            for n in 0..50 {
                let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
                let key = format!("w{n:02}").into_bytes();
                txn.kv_set(Flavour::Durable, REGISTRY, &key, b"w", None)
                    .unwrap();
                txn.vote().unwrap();
                txn.finish().unwrap();
            }
        });
        let row = DurableRow {
            name: REGISTRY.to_owned(),
            key: b"k".to_vec(),
            payload: b"restored".to_vec(),
        };
        store.kv_restore_durable(&[row]).unwrap();
        writer.join().unwrap();
    });
    let check = |store: &Store| {
        let value = store.kv_get(Flavour::Durable, REGISTRY, b"k").unwrap();
        assert_eq!(value.as_deref(), Some(&b"restored"[..]));
        for n in 0..50 {
            let key = format!("w{n:02}").into_bytes();
            assert!(
                store
                    .kv_get(Flavour::Durable, REGISTRY, &key)
                    .unwrap()
                    .is_some()
            );
        }
    };
    check(&store);
    store.fold().unwrap();
    check(&store);
    drop(store);
    check(&open(&vfs, held()));
}

#[test]
fn an_ephemeral_row_in_the_log_alone_reads_absent_after_its_expiry() {
    let clock = ManualClock::new();
    let vfs = MemVfs::new();
    let store = Store::open_full(vfs.clone(), clock.clone(), options(held())).unwrap();
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.kv_set(
        Flavour::Ephemeral,
        SESSIONS,
        b"s1",
        b"data",
        Some(Duration::from_secs(60)),
    )
    .unwrap();
    txn.vote().unwrap();
    txn.finish().unwrap();
    assert!(store.backlog_bytes() > 0);
    let value = store.kv_get(Flavour::Ephemeral, SESSIONS, b"s1").unwrap();
    assert_eq!(value.as_deref(), Some(&b"data"[..]));

    clock.advance(Duration::from_secs(120));
    assert_eq!(
        store.kv_get(Flavour::Ephemeral, SESSIONS, b"s1").unwrap(),
        None
    );
    let pairs = store
        .kv_scan(Flavour::Ephemeral, SESSIONS, None, None, 10)
        .unwrap();
    assert!(pairs.is_empty(), "{pairs:?}");
    assert!(store.backlog_bytes() > 0, "the row left the log");
}

#[test]
fn an_idle_store_folds_by_age() {
    let vfs = MemVfs::new();
    let write_log = WriteLog {
        fold_age: Duration::from_millis(100),
        ..WriteLog::default()
    };
    let store = open(&vfs, write_log);
    let commits = commit_new(&store, 3, 100);
    // Far below `fold_bytes`: only the age starts the fold.
    testing::wait_until(|| store.backlog_bytes() == 0);
    for &(oid, tid) in &commits {
        assert_eq!(store.load(oid).unwrap().1, tid);
    }
}
