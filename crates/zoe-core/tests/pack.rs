//! Acceptance test of pack with garbage collection. A pack at tid T keeps
//! every revision that a reader at T or later can reach, and removes the
//! others with their blob files. It finds references by reading the
//! persistent ids of each pickle. It runs in batches, while other threads
//! commit and read, and a crash at any point leaves a store that opens and
//! keeps every reachable revision.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use zoe_core::vfs::MemVfs;
use zoe_core::{Oid, Store, Tid, Txn, ZoeError};

const ROOT: Oid = Oid(0);
const MAPPING: (&str, &str) = ("persistent.mapping", "PersistentMapping");
const ITEM: (&str, &str) = ("app.content", "Item");
const BLOB: (&str, &str) = ("ZODB.blob", "Blob");

/// A persistent reference, in one of the forms that ZODB writes.
#[derive(Clone, Copy)]
enum Ref {
    /// The oid alone, as ZODB writes it for a class with `__getnewargs__`.
    Oid(Oid),
    /// `(oid, class)`, the usual form.
    Typed(Oid),
    /// `['w', (oid,)]`: a weak reference. It keeps nothing alive.
    Weak(Oid),
    /// `['m', (database, oid, class)]`: a reference into another database.
    /// It keeps nothing alive in this one.
    Foreign(Oid),
}

/// One value in the state of a record.
#[derive(Clone, Copy)]
enum Value {
    /// A text.
    Text(&'static str),
    /// A reference in the state dict.
    Ref(Ref),
    /// A reference inside a tuple.
    InTuple(Ref),
    /// A reference in the arguments of a REDUCE of a function that the
    /// pickle tree does not model.
    InReduce(Ref),
    /// A reference in the state of an inline object that NEWOBJ and BUILD
    /// make.
    InObject(Ref),
}

fn global(p: &mut Vec<u8>, (module, name): (&str, &str)) {
    p.push(b'c');
    p.extend_from_slice(module.as_bytes());
    p.push(b'\n');
    p.extend_from_slice(name.as_bytes());
    p.push(b'\n');
}

fn text(p: &mut Vec<u8>, s: &str) {
    p.push(b'X');
    p.extend_from_slice(&(s.len() as u32).to_le_bytes());
    p.extend_from_slice(s.as_bytes());
}

fn oid_bytes(p: &mut Vec<u8>, oid: Oid) {
    p.extend_from_slice(b"C\x08");
    p.extend_from_slice(&oid.0.to_be_bytes());
}

fn reference(p: &mut Vec<u8>, r: Ref) {
    match r {
        Ref::Oid(oid) => oid_bytes(p, oid),
        Ref::Typed(oid) => {
            oid_bytes(p, oid);
            global(p, ITEM);
            p.push(0x86);
        }
        Ref::Weak(oid) => {
            p.extend_from_slice(b"](");
            text(p, "w");
            oid_bytes(p, oid);
            p.extend_from_slice(b"\x85e");
        }
        Ref::Foreign(oid) => {
            p.extend_from_slice(b"](");
            text(p, "m");
            p.push(b'(');
            text(p, "other");
            oid_bytes(p, oid);
            global(p, ITEM);
            p.extend_from_slice(b"te");
        }
    }
    p.push(b'Q');
}

/// Return a protocol-3 record of `class`: the class pickle, then a dict
/// state that maps each key to its value.
fn record(class: (&str, &str), state: &[(&str, Value)]) -> Vec<u8> {
    let mut p = b"\x80\x03".to_vec();
    global(&mut p, class);
    p.extend_from_slice(b".\x80\x03}");
    if !state.is_empty() {
        p.push(b'(');
        for &(key, value) in state {
            text(&mut p, key);
            match value {
                Value::Text(s) => text(&mut p, s),
                Value::Ref(r) => reference(&mut p, r),
                Value::InTuple(r) => {
                    p.push(b'(');
                    text(&mut p, "nested");
                    reference(&mut p, r);
                    p.push(b't');
                }
                Value::InReduce(r) => {
                    global(&mut p, ("app.inline", "factory"));
                    p.push(b'(');
                    reference(&mut p, r);
                    p.extend_from_slice(b"tR");
                }
                Value::InObject(r) => {
                    global(&mut p, ("app.inline", "Item"));
                    p.extend_from_slice(b")\x81}(");
                    text(&mut p, "target");
                    reference(&mut p, r);
                    p.extend_from_slice(b"ub");
                }
            }
        }
        p.push(b'u');
    }
    p.push(b'.');
    p
}

const FIRST: &[u8] = b"first revision";
const SECOND: &[u8] = b"second revision";
const THIRD: &[u8] = b"third revision";
const DROPPED: &[u8] = b"dropped blob";

/// A record of class `ITEM` that holds only a title.
fn item(title: &'static str) -> Vec<u8> {
    record(ITEM, &[("title", Value::Text(title))])
}

/// The pickle of a blob record.
fn blob_record() -> Vec<u8> {
    record(BLOB, &[])
}

fn fresh_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("zoe-pack-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Every record that a test committed.
#[derive(Default)]
struct Model {
    records: BTreeMap<(Oid, Tid), Vec<u8>>,
}

impl Model {
    /// The serial of the newest record of `oid`, or `Tid::ZERO`.
    fn serial(&self, oid: Oid) -> Tid {
        self.records
            .keys()
            .filter(|(o, _)| *o == oid)
            .map(|&(_, tid)| tid)
            .max()
            .unwrap_or(Tid::ZERO)
    }

    /// Commit one transaction that writes `writes` and blobs `blobs`.
    fn commit(&mut self, store: &Store, writes: &[(Oid, Vec<u8>)], blobs: &[(Oid, &[u8])]) -> Tid {
        let mut txn = store.tpc_begin(b"someone", b"pack test", b"").unwrap();
        for (oid, data) in writes {
            txn.store(*oid, self.serial(*oid), data).unwrap();
        }
        for &(oid, bytes) in blobs {
            store_blob(store, &mut txn, oid, self.serial(oid), bytes);
        }
        txn.vote().unwrap();
        let tid = txn.finish().unwrap();
        for (oid, data) in writes {
            self.records.insert((*oid, tid), data.clone());
        }
        for &(oid, _) in blobs {
            self.records.insert((oid, tid), blob_record());
        }
        tid
    }
}

fn store_blob(store: &Store, txn: &mut Txn<'_>, oid: Oid, serial: Tid, bytes: &[u8]) {
    let mut file = store.new_blob_file().unwrap();
    file.write_all(bytes).unwrap();
    txn.store_blob(oid, serial, &blob_record(), file).unwrap();
}

/// The contents of the blob that `tid` wrote for `oid`.
fn read_blob(store: &Store, oid: Oid, tid: Tid) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    let mut reader = store.open_blob(oid, tid).map_err(|e| e.to_string())?;
    reader.read_to_end(&mut bytes).map_err(|e| e.to_string())?;
    Ok(bytes)
}

/// What a reader at a snapshot gets for one oid: the pickle, its serial
/// and the serial of the next record.
type View = Option<(Vec<u8>, Tid, Option<Tid>)>;

/// The tid that `load_before` takes to read the snapshot `at`.
fn after(at: Tid) -> Tid {
    if at == Tid::MAX { at } else { Tid(at.0 + 1) }
}

/// What readers at each snapshot of `snapshots` get for each oid of
/// `oids`.
fn views(store: &Store, oids: &[Oid], snapshots: &[Tid]) -> Vec<(Oid, Tid, View)> {
    let mut views = Vec::new();
    for &at in snapshots {
        for &oid in oids {
            let view = store
                .load_before(oid, after(at))
                .unwrap_or_else(|e| panic!("{oid:?} at {at:?}: {e}"))
                .map(|r| (r.data, r.serial, r.next));
            views.push((oid, at, view));
        }
    }
    views
}

fn assert_removed(store: &Store, oid: Oid, tid: Tid) {
    let loaded = store.load_serial(oid, tid);
    assert!(
        matches!(loaded, Err(ZoeError::NotFound { .. })),
        "{oid:?} at {tid:?} was not removed: {loaded:?}"
    );
}

/// The store of the main scenario.
struct Scenario {
    model: Model,
    /// The tids of the six transactions. The pack tid is `tids[3]`.
    tids: Vec<Tid>,
    a: Oid,
    b: Oid,
    c: Oid,
    d: Oid,
    r: Oid,
    w: Oid,
    m: Oid,
    h: Oid,
    x1: Oid,
    x2: Oid,
    e: Oid,
}

impl Scenario {
    fn pack_tid(&self) -> Tid {
        self.tids[3]
    }

    /// The oids that a reader at the pack tid or later can reach.
    fn kept(&self) -> Vec<Oid> {
        vec![
            ROOT, self.a, self.b, self.c, self.r, self.h, self.x1, self.x2, self.e,
        ]
    }

    /// The snapshots of readers at the pack tid or later.
    fn snapshots(&self) -> Vec<Tid> {
        let mut snapshots = self.tids[3..].to_vec();
        snapshots.push(Tid::MAX);
        snapshots
    }

    /// The records that the pack removes.
    fn removed(&self) -> Vec<(Oid, Tid)> {
        let t = &self.tids;
        vec![
            (ROOT, t[0]),
            (self.a, t[0]),
            (self.b, t[0]),
            (self.b, t[1]),
            (self.b, t[2]),
            (self.d, t[0]),
            (self.d, t[1]),
            (self.w, t[0]),
            (self.m, t[0]),
        ]
    }
}

/// Write the history of the main scenario:
///
/// 1. The root holds a, b, d, r, w (weakly), m (in another database) and
///    h. a holds c in a tuple. h holds x1 in a REDUCE and x2 in an inline
///    object.
/// 2. a, b and d change.
/// 3. The root drops d and r. b changes.
/// 4. b changes. This is the pack tid.
/// 5. The root holds r again, and a new object e. b changes.
/// 6. b changes.
fn scenario(store: &Store) -> Scenario {
    let [a, b, c, d, r, w, m, h, x1, x2, e] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11].map(Oid);
    let mut model = Model::default();
    let root = |extra: &[(&'static str, Value)]| {
        let mut state = vec![
            ("a", Value::Ref(Ref::Typed(a))),
            ("b", Value::Ref(Ref::Typed(b))),
            ("h", Value::Ref(Ref::Typed(h))),
            ("m", Value::Ref(Ref::Foreign(m))),
            ("w", Value::Ref(Ref::Weak(w))),
        ];
        state.extend_from_slice(extra);
        record(MAPPING, &state)
    };
    let holder = |title| {
        record(
            ITEM,
            &[
                ("c", Value::InTuple(Ref::Typed(c))),
                ("title", Value::Text(title)),
            ],
        )
    };

    let t1 = model.commit(
        store,
        &[
            (
                ROOT,
                root(&[
                    ("d", Value::Ref(Ref::Typed(d))),
                    ("r", Value::Ref(Ref::Oid(r))),
                ]),
            ),
            (a, holder("a1")),
            (b, item("b1")),
            (c, item("c1")),
            (d, item("d1")),
            (r, item("r1")),
            (w, item("w1")),
            (m, item("m1")),
            (
                h,
                record(
                    ITEM,
                    &[
                        ("x", Value::InReduce(Ref::Oid(x1))),
                        ("y", Value::InObject(Ref::Typed(x2))),
                    ],
                ),
            ),
            (x1, item("x1")),
            (x2, item("x2")),
        ],
        &[],
    );
    let t2 = model.commit(
        store,
        &[(a, holder("a2")), (b, item("b2")), (d, item("d2"))],
        &[],
    );
    let t3 = model.commit(store, &[(ROOT, root(&[])), (b, item("b3"))], &[]);
    let t4 = model.commit(store, &[(b, item("b4"))], &[]);
    let t5 = model.commit(
        store,
        &[
            (
                ROOT,
                root(&[
                    ("e", Value::Ref(Ref::Typed(e))),
                    ("r", Value::Ref(Ref::Oid(r))),
                ]),
            ),
            (e, item("e1")),
            (b, item("b5")),
        ],
        &[],
    );
    let t6 = model.commit(store, &[(b, item("b6"))], &[]);
    Scenario {
        model,
        tids: vec![t1, t2, t3, t4, t5, t6],
        a,
        b,
        c,
        d,
        r,
        w,
        m,
        h,
        x1,
        x2,
        e,
    }
}

/// Check the store after a pack of the main scenario.
fn assert_packed(store: &Store, s: &Scenario) {
    let removed = s.removed();
    for (&(oid, tid), data) in &s.model.records {
        if removed.contains(&(oid, tid)) {
            assert_removed(store, oid, tid);
        } else {
            let loaded = store.load_serial(oid, tid);
            assert_eq!(loaded.ok().as_ref(), Some(data), "{oid:?} at {tid:?}");
        }
    }
    for oid in [s.d, s.w, s.m] {
        let loaded = store.load(oid);
        assert!(
            matches!(loaded, Err(ZoeError::NotFound { .. })),
            "{oid:?}: {loaded:?}"
        );
    }
}

#[test]
fn a_pack_keeps_what_readers_at_or_after_the_pack_tid_see() {
    let store = Store::open(fresh_dir("reachability")).unwrap();
    let s = scenario(&store);
    let before = views(&store, &s.kept(), &s.snapshots());

    let report = store.pack(s.pack_tid()).unwrap();

    assert_eq!(views(&store, &s.kept(), &s.snapshots()), before);
    assert_packed(&store, &s);
    let history: Vec<Tid> = store
        .history(s.b, 10)
        .unwrap()
        .iter()
        .map(|h| h.tid)
        .collect();
    assert_eq!(
        history,
        s.tids[3..].iter().rev().copied().collect::<Vec<_>>()
    );

    let removed = s.removed();
    let bytes: usize = removed.iter().map(|key| s.model.records[key].len()).sum();
    assert_eq!(report.records, removed.len() as u64, "{report:?}");
    assert_eq!(report.bytes, bytes as u64, "{report:?}");
    assert_eq!(report.objects, 3, "{report:?}");
    assert_eq!((report.blobs, report.blob_bytes), (0, 0), "{report:?}");

    // A second pack at the same tid finds nothing to remove.
    let again = store.pack(s.pack_tid()).unwrap();
    assert_eq!((again.records, again.bytes, again.objects), (0, 0, 0));
    assert_eq!(views(&store, &s.kept(), &s.snapshots()), before);
}

#[test]
fn a_pack_removes_the_blob_files_of_the_records_it_removes() {
    let dir = fresh_dir("blobs");
    let store = Store::open(&dir).unwrap();
    let (f, g) = (Oid(1), Oid(2));
    let mut model = Model::default();
    let root = |names: &[(&'static str, Oid)]| {
        let state: Vec<_> = names
            .iter()
            .map(|&(name, oid)| (name, Value::Ref(Ref::Typed(oid))))
            .collect();
        record(MAPPING, &state)
    };

    let t1 = model.commit(
        &store,
        &[(ROOT, root(&[("f", f), ("g", g)]))],
        &[(f, FIRST), (g, DROPPED)],
    );
    let t2 = model.commit(&store, &[(ROOT, root(&[("f", f)]))], &[(f, SECOND)]);
    let t3 = model.commit(&store, &[], &[(f, THIRD)]);
    let paths =
        [(f, t1), (f, t2), (f, t3), (g, t1)].map(|(oid, tid)| store.blob_path(oid, tid).unwrap());
    assert!(paths.iter().all(|path| path.exists()));

    let report = store.pack(t2).unwrap();

    assert!(!paths[0].exists(), "the old revision of f kept its file");
    assert!(!paths[3].exists(), "the unreachable blob kept its file");
    assert_eq!(read_blob(&store, f, t2), Ok(SECOND.to_vec()));
    assert_eq!(read_blob(&store, f, t3), Ok(THIRD.to_vec()));
    store.verify_blob(f, t2).unwrap();
    store.verify_blob(f, t3).unwrap();
    assert_removed(&store, f, t1);
    assert_removed(&store, g, t1);
    assert_removed(&store, ROOT, t1);

    assert_eq!((report.records, report.objects), (3, 1), "{report:?}");
    assert_eq!(report.blobs, 2, "{report:?}");
    let blob_bytes = FIRST.len() + DROPPED.len();
    assert_eq!(report.blob_bytes, blob_bytes as u64, "{report:?}");
}

#[test]
fn a_pack_runs_while_other_threads_commit_and_read() {
    let store = Arc::new(Store::open(fresh_dir("online")).unwrap());
    let s = scenario(&store);
    let kept = s.kept();
    let at_pack: Vec<_> = views(&store, &kept, &[s.pack_tid()]);
    let b = s.b;

    let mut batches = 0;
    let mut written = Vec::new();
    let report = store
        .pack_in_batches(s.pack_tid(), 2, &mut || {
            batches += 1;
            let worker = Arc::clone(&store);
            let (kept, at_pack, pack_tid) = (kept.clone(), at_pack.clone(), s.pack_tid());
            let new = Oid(100 + batches);
            let (done, wait) = mpsc::channel();
            // The worker reads and commits from another thread. The pack
            // thread waits for it here, between two batches.
            thread::spawn(move || {
                let result = (|| {
                    if views(&worker, &kept, &[pack_tid]) != at_pack {
                        return Err("a reader at the pack tid saw another state".to_owned());
                    }
                    let (_, serial) = worker.load(b).map_err(|e| e.to_string())?;
                    let mut txn = worker
                        .tpc_begin(b"", b"during the pack", b"")
                        .map_err(|e| e.to_string())?;
                    txn.store(b, serial, &item("during"))
                        .map_err(|e| e.to_string())?;
                    txn.store(new, Tid::ZERO, &item("new"))
                        .map_err(|e| e.to_string())?;
                    txn.vote().map_err(|e| e.to_string())?;
                    txn.finish().map_err(|e| e.to_string())
                })();
                let _ = done.send(result);
            });
            let tid = wait
                .recv_timeout(Duration::from_secs(10))
                .expect("a commit during the pack did not finish")
                .unwrap();
            written.push((new, tid));
        })
        .unwrap();

    assert!(batches >= 2, "the pack ran {batches} batches");
    assert_eq!(report.records, s.removed().len() as u64, "{report:?}");
    assert_eq!(views(&store, &kept, &[s.pack_tid()]), at_pack);
    for &(oid, tid) in &written {
        assert_eq!(store.load_serial(oid, tid).unwrap(), item("new"));
        assert_eq!(store.load_serial(b, tid).unwrap(), item("during"));
    }
    let last = written.last().unwrap().1;
    assert_eq!(store.load(b).unwrap(), (item("during"), last));
}

#[test]
fn a_reference_that_cannot_be_read_keeps_its_target() {
    let store = Store::open(fresh_dir("unreadable")).unwrap();
    let (u, x) = (Oid(1), Oid(2));
    let mut model = Model::default();

    // The state of u holds a reference to x, then an opcode that Python
    // refuses.
    let mut unreadable = record(ITEM, &[]);
    unreadable.truncate(unreadable.len() - 1);
    unreadable.push(b'(');
    text(&mut unreadable, "x");
    reference(&mut unreadable, Ref::Typed(x));
    unreadable.extend_from_slice(b"\xff.");

    let t1 = model.commit(
        &store,
        &[
            (ROOT, record(MAPPING, &[("u", Value::Ref(Ref::Typed(u)))])),
            (u, item("u1")),
            (x, item("x1")),
        ],
        &[],
    );
    let t2 = model.commit(&store, &[(u, unreadable.clone())], &[]);

    // A refused pack removes nothing.
    if store.pack(t2).is_err() {
        for (&(oid, tid), data) in &model.records {
            assert_eq!(&store.load_serial(oid, tid).unwrap(), data);
        }
    }
    assert_eq!(store.load(x).unwrap(), (item("x1"), t1));
    assert_eq!(store.load(u).unwrap(), (unreadable, t2));
}

#[test]
fn a_crash_at_any_point_of_a_pack_keeps_every_reachable_revision() {
    let (a, d, f) = (Oid(1), Oid(2), Oid(3));
    let mut model = Model::default();
    let built = MemVfs::new();
    let tids = {
        let store = Store::open_vfs(built.clone()).unwrap();
        let root = |names: &[(&'static str, Oid)]| {
            let state: Vec<_> = names
                .iter()
                .map(|&(name, oid)| (name, Value::Ref(Ref::Typed(oid))))
                .collect();
            record(MAPPING, &state)
        };
        let t1 = model.commit(
            &store,
            &[
                (ROOT, root(&[("a", a), ("d", d), ("f", f)])),
                (a, item("a1")),
                (d, item("d1")),
            ],
            &[(f, FIRST)],
        );
        let t2 = model.commit(
            &store,
            &[(ROOT, root(&[("a", a), ("f", f)])), (a, item("a2"))],
            &[(f, SECOND)],
        );
        let t3 = model.commit(&store, &[(a, item("a3"))], &[]);
        [t1, t2, t3]
    };
    let [t1, t2, t3] = tids;
    let removed = [(ROOT, t1), (a, t1), (d, t1), (f, t1)];

    // The crash images cover the pack, not the commits before it.
    let vfs = built.durable_copy();
    {
        let store = Store::open_vfs(vfs.clone()).unwrap();
        let report = store.pack_in_batches(t2, 2, &mut || {}).unwrap();
        assert_eq!(report.records, removed.len() as u64, "{report:?}");
        vfs.mark("packed");
    }

    let images = vfs.crash_images();
    assert!(images.len() > 1, "the simulation produced crash images");
    for image in &images {
        let store = Store::open_vfs(image.vfs())
            .unwrap_or_else(|e| panic!("{image:?} does not open without repair: {e}"));
        // Every revision that a reader at t2 or later sees is there.
        for (oid, tid) in [(ROOT, t2), (a, t2), (a, t3), (f, t2)] {
            let loaded = store.load_serial(oid, tid);
            assert_eq!(
                loaded.as_ref().ok(),
                Some(&model.records[&(oid, tid)]),
                "{image:?} lost {oid:?} at {tid:?}: {loaded:?}"
            );
        }
        assert_eq!(read_blob(&store, f, t2), Ok(SECOND.to_vec()), "{image:?}");
        // A blob record that is there still has its file.
        match store.load_serial(f, t1) {
            Ok(_) => assert_eq!(read_blob(&store, f, t1), Ok(FIRST.to_vec()), "{image:?}"),
            Err(ZoeError::NotFound { .. }) => {}
            Err(e) => panic!("{image:?}: {e}"),
        }
        if image.is_after("packed") {
            for (oid, tid) in removed {
                assert_removed(&store, oid, tid);
            }
        }
    }
}
