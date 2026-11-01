//! Acceptance test of the pack dry run. A dry run at tid T reports what a
//! pack at T would remove, by class and by size, and writes nothing. Its
//! report equals the report of the pack that follows. Class names come
//! from the class pickle of each record.

use std::collections::BTreeMap;
use std::io::Write;

use redb::StorageBackend;
use zoe_core::vfs::MemVfs;
use zoe_core::{Oid, PackReport, Store, Tid, ZoeError};

const ROOT: Oid = Oid(0);

/// The class pickle of a record of `module.name`, in the form that ZODB
/// writes for most classes: the class alone.
fn class(module: &str, name: &str) -> Vec<u8> {
    let mut p = global(module, name);
    p.extend_from_slice(b"q\x00.");
    p
}

/// The class pickle of a record of `module.name`, in the form that ZODB
/// writes for a class with `__getnewargs__`: the class and its arguments.
fn class_with_args(module: &str, name: &str) -> Vec<u8> {
    let mut p = global(module, name);
    p.extend_from_slice(b"q\x00)\x86q\x01.");
    p
}

/// The start of a protocol-3 pickle that names the class `module.name`.
fn global(module: &str, name: &str) -> Vec<u8> {
    let mut p = b"\x80\x03c".to_vec();
    p.extend_from_slice(module.as_bytes());
    p.push(b'\n');
    p.extend_from_slice(name.as_bytes());
    p.push(b'\n');
    p
}

fn text(p: &mut Vec<u8>, s: &str) {
    p.push(b'X');
    p.extend_from_slice(&(s.len() as u32).to_le_bytes());
    p.extend_from_slice(s.as_bytes());
}

/// Return a record: the class pickle `class`, then a dict state with a
/// title and a `(oid, class)` reference for each of `refs`.
fn titled_record(class: Vec<u8>, title: &str, refs: &[(&str, Oid)]) -> Vec<u8> {
    let mut p = class;
    p.extend_from_slice(b"\x80\x03}(");
    text(&mut p, "title");
    text(&mut p, title);
    for &(key, oid) in refs {
        text(&mut p, key);
        p.extend_from_slice(b"C\x08");
        p.extend_from_slice(&oid.0.to_be_bytes());
        p.extend_from_slice(b"capp.content\nItem\n\x86Q");
    }
    p.extend_from_slice(b"u.");
    p
}

fn folder(title: &str, refs: &[(&str, Oid)]) -> Vec<u8> {
    titled_record(class("app.content", "Folder"), title, refs)
}

fn document(title: &str) -> Vec<u8> {
    titled_record(class("app.content", "Document"), title, &[])
}

/// The pickle of a blob record.
const BLOB_PICKLE: &[u8] = b"\x80\x03cZODB.blob\nBlob\nq\x00.\x80\x03N.";

/// Every committed record, with the class name that the report gives it.
#[derive(Default)]
struct Model {
    records: BTreeMap<(Oid, Tid), (Vec<u8>, &'static str)>,
    blobs: BTreeMap<(Oid, Tid), Vec<u8>>,
}

impl Model {
    fn serial(&self, oid: Oid) -> Tid {
        self.records
            .keys()
            .filter(|(o, _)| *o == oid)
            .map(|&(_, tid)| tid)
            .max()
            .unwrap_or(Tid::ZERO)
    }

    /// Commit the records `writes`, each with its class name, and the
    /// blobs `blobs`.
    fn commit(
        &mut self,
        store: &Store,
        writes: Vec<(Oid, Vec<u8>, &'static str)>,
        blobs: &[(Oid, &[u8])],
    ) -> Tid {
        let mut txn = store.tpc_begin(b"someone", b"dry run test", b"").unwrap();
        for (oid, data, _) in &writes {
            txn.store(*oid, self.serial(*oid), data).unwrap();
        }
        for &(oid, bytes) in blobs {
            let mut file = store.new_blob_file().unwrap();
            file.write_all(bytes).unwrap();
            txn.store_blob(oid, self.serial(oid), BLOB_PICKLE, file)
                .unwrap();
        }
        txn.vote().unwrap();
        let tid = txn.finish().unwrap();
        for (oid, data, class) in writes {
            self.records.insert((oid, tid), (data, class));
        }
        for &(oid, bytes) in blobs {
            self.records
                .insert((oid, tid), (BLOB_PICKLE.to_vec(), "ZODB.blob.Blob"));
            self.blobs.insert((oid, tid), bytes.to_vec());
        }
        tid
    }
}

const FOLDER: &str = "app.content.Folder";
const DOCUMENT: &str = "app.content.Document";
const BUCKET: &str = "BTrees.OOBTree.OOBucket";
const MAPPING: &str = "persistent.mapping.PersistentMapping";

/// Write a store with old revisions and unreachable objects of several
/// classes, and return the model and the tids. The pack tid is the third
/// tid.
///
/// 1. The root holds a folder, a bucket and a blob. The folder holds
///    three documents.
/// 2. The folder drops one document. Another document, the bucket and the
///    blob change.
/// 3. A document and the bucket change. This is the pack tid.
/// 4. The blob changes.
fn scenario(store: &Store) -> (Model, Vec<Tid>) {
    let [f, d1, d2, d3, k, blob] = [1, 2, 3, 4, 5, 6].map(Oid);
    let mut model = Model::default();
    let root = titled_record(
        class("persistent.mapping", "PersistentMapping"),
        "root",
        &[("blob", blob), ("bucket", k), ("folder", f)],
    );
    let bucket = |title| titled_record(class_with_args("BTrees.OOBTree", "OOBucket"), title, &[]);

    let t1 = model.commit(
        store,
        vec![
            (ROOT, root, MAPPING),
            (f, folder("f1", &[("a", d1), ("b", d2), ("c", d3)]), FOLDER),
            (d1, document("d1 first"), DOCUMENT),
            (d2, document("d2 first"), DOCUMENT),
            (d3, document("d3 first"), DOCUMENT),
            (k, bucket("k1"), BUCKET),
        ],
        &[(blob, b"first blob revision".as_slice())],
    );
    let t2 = model.commit(
        store,
        vec![
            (f, folder("f2", &[("a", d1), ("b", d2)]), FOLDER),
            (d1, document("d1 second"), DOCUMENT),
            // The same class in the class-only form.
            (
                k,
                titled_record(class("BTrees.OOBTree", "OOBucket"), "k2", &[]),
                BUCKET,
            ),
        ],
        &[(blob, b"second blob revision".as_slice())],
    );
    let t3 = model.commit(
        store,
        vec![
            (d2, document("d2 second"), DOCUMENT),
            (k, bucket("k3"), BUCKET),
        ],
        &[],
    );
    let t4 = model.commit(store, vec![], &[(blob, b"third blob revision".as_slice())]);
    (model, vec![t1, t2, t3, t4])
}

/// The bytes of every file of `vfs`.
fn files(vfs: &MemVfs) -> BTreeMap<String, Vec<u8>> {
    vfs.names()
        .into_iter()
        .map(|name| {
            let file = vfs.file(&name);
            let mut bytes = vec![0; file.len().unwrap() as usize];
            file.read(0, &mut bytes).unwrap();
            (name, bytes)
        })
        .collect()
}

/// The records of the model that the store no longer has.
fn removed(store: &Store, model: &Model) -> Vec<(Oid, Tid)> {
    model
        .records
        .keys()
        .copied()
        .filter(|&(oid, tid)| match store.load_serial(oid, tid) {
            Ok(_) => false,
            Err(ZoeError::NotFound { .. }) => true,
            Err(e) => panic!("{oid:?} at {tid:?}: {e}"),
        })
        .collect()
}

/// The report of a pack that removed `removed`, by class: the number of
/// records and their pickle bytes.
fn by_class(model: &Model, removed: &[(Oid, Tid)]) -> BTreeMap<String, (u64, u64)> {
    let mut classes = BTreeMap::<String, (u64, u64)>::new();
    for key in removed {
        let (data, class) = &model.records[key];
        let entry = classes.entry((*class).to_owned()).or_default();
        entry.0 += 1;
        entry.1 += data.len() as u64;
    }
    classes
}

fn report_by_class(report: &PackReport) -> BTreeMap<String, (u64, u64)> {
    report
        .by_class
        .iter()
        .map(|(class, totals)| (class.clone(), (totals.records, totals.bytes)))
        .collect()
}

#[test]
fn a_dry_run_reports_what_the_pack_removes_by_class_and_size() {
    let store = Store::open_vfs(MemVfs::new()).unwrap();
    let (model, tids) = scenario(&store);

    let dry = store.pack_dry_run(tids[2]).unwrap();
    let packed = store.pack(tids[2]).unwrap();
    assert_eq!(dry, packed);

    // What the pack removed, record by record.
    let removed = removed(&store, &model);
    let [f, d1, d2, d3, k, blob] = [1, 2, 3, 4, 5, 6].map(Oid);
    let mut expected = vec![
        (f, tids[0]),
        (d1, tids[0]),
        (d2, tids[0]),
        (d3, tids[0]),
        (k, tids[0]),
        (k, tids[1]),
        (blob, tids[0]),
    ];
    expected.sort();
    assert_eq!(removed, expected);

    assert_eq!(report_by_class(&dry), by_class(&model, &removed), "{dry:?}");
    let bytes: u64 = removed
        .iter()
        .map(|key| model.records[key].0.len() as u64)
        .sum();
    assert_eq!(dry.records, removed.len() as u64, "{dry:?}");
    assert_eq!(dry.bytes, bytes, "{dry:?}");
    assert_eq!(dry.objects, 1, "{dry:?}");
    assert_eq!(dry.blobs, 1, "{dry:?}");
    let blob_bytes = model.blobs[&(blob, tids[0])].len() as u64;
    assert_eq!(dry.blob_bytes, blob_bytes, "{dry:?}");
}

#[test]
fn a_dry_run_writes_nothing() {
    let vfs = MemVfs::new();
    let store = Store::open_vfs(vfs.clone()).unwrap();
    let (model, tids) = scenario(&store);
    let last = store.last_transaction();
    let before = files(&vfs);

    let report = store.pack_dry_run(tids[2]).unwrap();
    assert!(report.records > 0, "{report:?}");

    assert!(files(&vfs) == before, "the dry run changed a file");
    assert_eq!(store.last_transaction(), last);
    for (&(oid, tid), (data, _)) in &model.records {
        assert_eq!(&store.load_serial(oid, tid).unwrap(), data);
    }

    // A second dry run gives the same report.
    assert_eq!(store.pack_dry_run(tids[2]).unwrap(), report);
}

#[test]
fn a_dry_run_after_the_pack_reports_nothing() {
    let store = Store::open_vfs(MemVfs::new()).unwrap();
    let (_, tids) = scenario(&store);
    store.pack(tids[2]).unwrap();

    let report = store.pack_dry_run(tids[2]).unwrap();
    assert_eq!(
        (report.records, report.bytes, report.objects),
        (0, 0, 0),
        "{report:?}"
    );
    assert_eq!((report.blobs, report.blob_bytes), (0, 0), "{report:?}");
    assert!(report.by_class.is_empty(), "{report:?}");
}
