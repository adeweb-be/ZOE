//! Undo in the core: `Txn::undo` and the undo log (`Store::undo_log`).
//!
//! The acceptance test `test_undo.py` compares the undo with FileStorage,
//! in-process and through `zoe serve`. These tests check the core alone.

use std::io::{self, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;

use zoe_core::testing::commit;
use zoe_core::vfs::{DirLock, MemFile, MemVfs, Vfs, WriteFile};
use zoe_core::{Oid, Options, Store, SystemClock, Tid, ZoeError, undo_id, undo_tid};
use zoe_pickle::python::{Heap, Pickler, Value};

const PICKLE: &[u8] = b"\x80\x03cZODB.blob\nBlob\nq\x00.\x80\x03N.";

fn store() -> Store {
    Store::open_vfs(MemVfs::new()).unwrap()
}

/// Returns the record of a `Length` of `value`.
fn length(value: i128) -> Vec<u8> {
    let mut heap = Heap::new();
    let class = heap.new_class("BTrees.Length", "Length");
    let mut pickler = Pickler::new(&heap);
    pickler.dump(class).unwrap();
    pickler.dump(Value::Int(value)).unwrap();
    pickler.into_bytes()
}

/// Undoes `tid` in a transaction of its own, and returns the tid of the
/// undo.
fn undo(store: &Store, tid: Tid) -> Result<Tid, ZoeError> {
    let mut txn = store.tpc_begin(b"", b"undo", b"").unwrap();
    let undone = txn.undo(tid)?;
    assert_eq!(undone.conflicts, []);
    txn.vote()?;
    txn.finish()
}

fn current(store: &Store, oid: Oid) -> Result<Vec<u8>, ZoeError> {
    store.load(oid).map(|(data, _)| data)
}

#[test]
fn an_undo_stores_the_state_before_the_transaction() {
    let store = store();
    let oid = store.new_oid().unwrap();
    let first = commit(&store, oid, Tid::ZERO, b"one");
    let second = commit(&store, oid, first, b"two");
    let tid = undo(&store, second).unwrap();
    assert_eq!(store.load(oid).unwrap(), (b"one".to_vec(), tid));
    // An undo is a transaction: its own undo gives the state back.
    undo(&store, tid).unwrap();
    assert_eq!(current(&store, oid).unwrap(), b"two");
    assert_eq!(store.history(oid, 10).unwrap().len(), 4);
}

#[test]
fn an_undo_of_a_creation_deletes_the_object() {
    let store = store();
    let oid = store.new_oid().unwrap();
    let created = commit(&store, oid, Tid::ZERO, b"one");
    let tid = undo(&store, created).unwrap();
    assert!(matches!(
        current(&store, oid),
        Err(ZoeError::NotFound { .. })
    ));
    // The undo of the undo creates the object again.
    undo(&store, tid).unwrap();
    assert_eq!(current(&store, oid).unwrap(), b"one");
}

#[test]
fn an_undo_after_a_later_change_merges_through_tier_1() {
    let store = store();
    let oid = store.new_oid().unwrap();
    let first = commit(&store, oid, Tid::ZERO, &length(10));
    let undone = commit(&store, oid, first, &length(15));
    commit(&store, oid, undone, &length(17));
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    let staged = txn.undo(undone).unwrap();
    assert_eq!(staged.oids, [oid]);
    assert_eq!(staged.conflicts, []);
    assert_eq!(txn.resolutions().len(), 1);
    assert!(txn.resolutions()[0].applied);
    txn.vote().unwrap();
    // The undo merge is not a merge of the vote.
    assert_eq!(txn.natively_resolved(), []);
    txn.finish().unwrap();
    assert_eq!(current(&store, oid).unwrap(), length(12));
}

#[test]
fn an_undo_that_no_tier_merges_goes_to_the_caller() {
    let store = store();
    let oid = store.new_oid().unwrap();
    let first = commit(&store, oid, Tid::ZERO, b"one");
    let undone = commit(&store, oid, first, b"two");
    let last = commit(&store, oid, undone, b"three");
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    let staged = txn.undo(undone).unwrap();
    assert_eq!(staged.oids, [oid]);
    let [conflict] = &staged.conflicts[..] else {
        panic!("{staged:?}");
    };
    assert_eq!(conflict.oid, oid);
    assert_eq!(conflict.serial, last);
    assert_eq!(conflict.current, b"three");
    assert_eq!(conflict.undone, undone);
    assert_eq!(conflict.before, b"one");
    // The call stages nothing for the oid: the caller stores the result.
    assert_eq!(txn.staged(oid).unwrap(), None);
    txn.store(oid, conflict.serial, b"resolved").unwrap();
    txn.vote().unwrap();
    txn.finish().unwrap();
    assert_eq!(current(&store, oid).unwrap(), b"resolved");
}

#[test]
fn a_refusal_of_tier_1_fails_the_undo() {
    let store = store();
    let oid = store.new_oid().unwrap();
    let mut heap = Heap::new();
    let bucket = |heap: &mut Heap, items: &[(i128, i128)]| {
        let class = heap.new_class("BTrees.IIBTree", "IIBucket");
        let flat = items
            .iter()
            .flat_map(|&(k, v)| [Value::Int(k), Value::Int(v)])
            .collect();
        let state = zoe_pickle::btrees::new_bucket_state(heap, flat, None);
        let mut pickler = Pickler::new(heap);
        pickler.dump(class).unwrap();
        pickler.dump(state).unwrap();
        pickler.into_bytes()
    };
    let first = commit(&store, oid, Tid::ZERO, &bucket(&mut heap, &[(1, 1)]));
    let undone = commit(&store, oid, first, &bucket(&mut heap, &[(1, 1), (2, 1)]));
    commit(&store, oid, undone, &bucket(&mut heap, &[(1, 1), (2, 2)]));
    // The undo deletes 2, which the later transaction changed.
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    assert!(matches!(
        txn.undo(undone),
        Err(ZoeError::Undo { oid: Some(found), .. }) if found == oid
    ));
}

#[test]
fn an_undo_of_a_creation_that_a_later_transaction_changed_fails() {
    let store = store();
    let oid = store.new_oid().unwrap();
    let created = commit(&store, oid, Tid::ZERO, b"one");
    commit(&store, oid, created, b"two");
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    assert!(matches!(txn.undo(created), Err(ZoeError::Undo { .. })));
}

#[test]
fn two_undos_of_one_object_in_one_transaction_undo_both() {
    let store = store();
    let oid = store.new_oid().unwrap();
    let first = commit(&store, oid, Tid::ZERO, b"one");
    let second = commit(&store, oid, first, b"two");
    let third = commit(&store, oid, second, b"three");
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.undo(third).unwrap();
    // The second undo sees the state that the first one staged.
    let staged = txn.undo(second).unwrap();
    assert_eq!(staged.conflicts, []);
    txn.vote().unwrap();
    txn.finish().unwrap();
    assert_eq!(current(&store, oid).unwrap(), b"one");
}

#[test]
fn a_change_after_the_undo_read_its_object_is_a_conflict_of_the_vote() {
    let store = store();
    let oid = store.new_oid().unwrap();
    let first = commit(&store, oid, Tid::ZERO, b"one");
    let second = commit(&store, oid, first, b"two");
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.undo(second).unwrap();
    commit(&store, oid, second, b"three");
    assert!(matches!(txn.vote(), Err(ZoeError::Conflict { .. })));
}

#[test]
fn nothing_is_undoable_at_or_before_the_last_pack() {
    let store = store();
    // The root stays after a pack. The pack reads the references of the
    // records, so they are pickles.
    let oid = Oid(0);
    let first = commit(&store, oid, Tid::ZERO, &length(1));
    let second = commit(&store, oid, first, &length(2));
    let third = commit(&store, oid, second, &length(3));
    store.pack(second).unwrap();
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    assert!(matches!(
        txn.undo(second),
        Err(ZoeError::Undo { oid: None, .. })
    ));
    drop(txn);
    undo(&store, third).unwrap();
    assert_eq!(current(&store, oid).unwrap(), length(2));
    // The undo log starts after the pack.
    let log: Vec<Tid> = store
        .undo_log(Tid::MAX, 10)
        .unwrap()
        .into_iter()
        .map(|row| row.tid)
        .collect();
    assert_eq!(log.len(), 2);
    assert_eq!(log[1], third);
}

#[test]
fn an_undo_whose_state_before_a_pack_removed_fails() {
    let store = store();
    let root = commit(&store, Oid(0), Tid::ZERO, &length(0));
    // No reader can reach this object, so the pack removes its old record.
    let oid = store.new_oid().unwrap();
    let first = commit(&store, oid, Tid::ZERO, &length(1));
    let second = commit(&store, oid, first, &length(2));
    store.pack(Tid(second.0 - 1)).unwrap();
    assert!(store.pack_watermark().unwrap() > root);
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    assert!(matches!(
        txn.undo(second),
        Err(ZoeError::Undo { oid: Some(found), .. }) if found == oid
    ));
}

#[test]
fn an_undo_of_a_transaction_that_the_store_does_not_have_fails() {
    let store = store();
    let oid = store.new_oid().unwrap();
    let tid = commit(&store, oid, Tid::ZERO, b"one");
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    assert!(matches!(
        txn.undo(Tid(tid.0 + 1)),
        Err(ZoeError::Undo { oid: None, .. })
    ));
}

#[test]
fn a_restore_transaction_refuses_an_undo() {
    let store = store();
    let oid = store.new_oid().unwrap();
    let tid = commit(&store, oid, Tid::ZERO, b"one");
    let mut txn = store
        .tpc_begin_restore(Tid(tid.0 + 1), b"", b"", b"")
        .unwrap();
    assert!(matches!(txn.undo(tid), Err(ZoeError::Protocol(_))));
}

#[test]
fn the_undo_log_is_newest_first_with_the_rows_of_the_transactions() {
    let store = store();
    let oid = store.new_oid().unwrap();
    let mut tids = Vec::new();
    let mut serial = Tid::ZERO;
    for n in 0..5u8 {
        let mut txn = store.tpc_begin(b"user", &[b'0' + n], b"").unwrap();
        txn.store(oid, serial, &[n + 1]).unwrap();
        txn.vote().unwrap();
        serial = txn.finish().unwrap();
        tids.push(serial);
    }
    let log = store.undo_log(Tid::MAX, 3).unwrap();
    let found: Vec<Tid> = log.iter().map(|row| row.tid).collect();
    assert_eq!(found, [tids[4], tids[3], tids[2]]);
    assert_eq!(log[0].description, b"4");
    assert_eq!(log[0].user, b"user");
    // The next part starts before the last row of the part before it.
    let rest = store.undo_log(tids[2], 10).unwrap();
    assert_eq!(rest.len(), 2);
    // An undo id is the base64 text of the tid.
    assert_eq!(undo_tid(&undo_id(tids[0])), Some(tids[0]));
}

fn commit_blob(store: &Store, oid: Oid, serial: Tid, bytes: &[u8]) -> Tid {
    let mut file = store.new_blob_file().unwrap();
    file.write_all(bytes).unwrap();
    let mut txn = store.tpc_begin(b"", b"blob", b"").unwrap();
    txn.store_blob(oid, serial, PICKLE, file).unwrap();
    txn.vote().unwrap();
    txn.finish().unwrap()
}

fn read_blob(store: &Store, oid: Oid, tid: Tid) -> Vec<u8> {
    let mut bytes = Vec::new();
    store
        .open_blob(oid, tid)
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    bytes
}

#[test]
fn a_blob_undo_links_the_earlier_blob_file() {
    let dir = std::env::temp_dir().join(format!("zoe-undo-blob-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store = Store::open(&dir).unwrap();
    let oid = store.new_oid().unwrap();
    let first = commit_blob(&store, oid, Tid::ZERO, b"first blob");
    let second = commit_blob(&store, oid, first, b"second blob");
    let before = std::fs::metadata(store.blob_path(oid, first).unwrap()).unwrap();
    let tid = undo(&store, second).unwrap();
    assert_eq!(read_blob(&store, oid, tid), b"first blob");
    assert_eq!(read_blob(&store, oid, second), b"second blob");
    // The blob of the undo is the earlier file under a second name.
    let linked = std::fs::metadata(store.blob_path(oid, tid).unwrap()).unwrap();
    assert_eq!(linked.ino(), before.ino());
    assert_eq!(linked.mtime_nsec(), before.mtime_nsec());
    assert_eq!(linked.nlink(), 2);
    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn every_crash_image_of_a_blob_undo_is_whole_or_absent() {
    let built = MemVfs::new();
    let (oid, first, second) = {
        let store = Store::open_vfs(built.clone()).unwrap();
        let oid = store.new_oid().unwrap();
        let first = commit_blob(&store, oid, Tid::ZERO, b"first blob");
        let second = commit_blob(&store, oid, first, b"second blob");
        (oid, first, second)
    };
    let vfs = built.durable_copy();
    let tid = {
        let store = Store::open_vfs(vfs.clone()).unwrap();
        let tid = undo(&store, second).unwrap();
        vfs.mark("acknowledged");
        tid
    };
    for image in vfs.crash_images() {
        let image_vfs = image.vfs();
        let store = Store::open_vfs(image_vfs.clone())
            .unwrap_or_else(|e| panic!("{image:?} does not open: {e}"));
        let (data, serial) = store.load(oid).unwrap();
        assert_eq!(data, PICKLE, "{image:?}");
        if serial == tid {
            assert_eq!(read_blob(&store, oid, tid), b"first blob", "{image:?}");
        } else {
            assert_eq!(serial, second, "{image:?}");
            assert!(!image.is_after("acknowledged"), "{image:?} lost the undo");
        }
        // The open removed a link that no blob record names.
        let files = image_vfs
            .names()
            .iter()
            .filter(|name| name.ends_with(".blob"))
            .count();
        assert_eq!(files, if serial == tid { 3 } else { 2 }, "{image:?}");
        // The earlier blob files stay.
        assert_eq!(read_blob(&store, oid, first), b"first blob", "{image:?}");
        assert_eq!(read_blob(&store, oid, second), b"second blob", "{image:?}");
    }
}

#[test]
fn an_undo_of_the_tid_0_names_no_transaction() {
    let store = store();
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    assert!(matches!(
        txn.undo(Tid::ZERO),
        Err(ZoeError::Undo { oid: None, reason }) if reason.contains("no such transaction")
    ));
}

#[test]
fn the_line_of_the_resolution_log_of_an_undo_merge_says_undo() {
    let store = store();
    let oid = store.new_oid().unwrap();
    let first = commit(&store, oid, Tid::ZERO, &length(10));
    let undone = commit(&store, oid, first, &length(15));
    commit(&store, oid, undone, &length(17));
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.undo(undone).unwrap();
    let [line] = &txn.resolution_log_lines(std::time::SystemTime::now(), None)[..] else {
        panic!("one line");
    };
    assert!(line.contains("\"undo\":true"), "{line}");
}

fn global(p: &mut Vec<u8>, module: &str, name: &str) {
    p.push(b'c');
    p.extend_from_slice(module.as_bytes());
    p.push(b'\n');
    p.extend_from_slice(name.as_bytes());
    p.push(b'\n');
}

/// Returns the record of a `PersistentMapping`. With `target`, its key `a`
/// refers to that oid, so a pack keeps it.
fn mapping(target: Option<Oid>) -> Vec<u8> {
    let mut p = b"\x80\x03".to_vec();
    global(&mut p, "persistent.mapping", "PersistentMapping");
    p.extend_from_slice(b".\x80\x03}");
    if let Some(oid) = target {
        p.push(b'(');
        p.push(b'X');
        p.extend_from_slice(&1u32.to_le_bytes());
        p.push(b'a');
        p.extend_from_slice(b"C\x08");
        p.extend_from_slice(&oid.0.to_be_bytes());
        global(&mut p, "app.content", "Item");
        p.push(0x86);
        p.push(b'Q');
        p.push(b'u');
    }
    p.push(b'.');
    p
}

#[test]
fn a_pack_between_the_undo_and_its_finish_fails_the_finish() {
    let store = store();
    let root = Oid(0);
    let x = store.new_oid().unwrap();
    let x = if x == root {
        store.new_oid().unwrap()
    } else {
        x
    };
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(root, Tid::ZERO, &mapping(Some(x))).unwrap();
    txn.store(x, Tid::ZERO, &mapping(None)).unwrap();
    txn.vote().unwrap();
    let t1 = txn.finish().unwrap();
    // The root lets go of x.
    let t2 = commit(&store, root, t1, &mapping(None));
    let mut undo = store.tpc_begin(b"", b"undo", b"").unwrap();
    undo.undo(t2).unwrap();
    undo.vote().unwrap();
    // A pack at t2 removes x after the vote.
    store.pack(t2).unwrap();
    assert!(store.load(x).is_err());
    assert!(matches!(
        undo.finish(),
        Err(ZoeError::Undo { oid: None, .. })
    ));
    // The store does not halt, and the root does not refer to x again.
    assert_eq!(current(&store, root).unwrap(), mapping(None));
    commit(&store, root, store.load(root).unwrap().1, &mapping(None));
}

#[test]
fn an_undo_of_a_blob_that_a_later_transaction_changed_fails() {
    let store = store();
    let oid = store.new_oid().unwrap();
    let first = commit_blob(&store, oid, Tid::ZERO, b"first blob");
    let second = commit_blob(&store, oid, first, b"second blob");
    let third = commit_blob(&store, oid, second, b"third blob");
    // The three revisions have one pickle: only the blob tells them apart.
    let mut txn = store.tpc_begin(b"", b"undo", b"").unwrap();
    assert!(matches!(
        txn.undo(second),
        Err(ZoeError::Undo { oid: Some(found), .. }) if found == oid
    ));
    drop(txn);
    assert_eq!(store.load(oid).unwrap().1, third);
    // Two undos in one transaction, the newest first, undo both.
    let mut txn = store.tpc_begin(b"", b"undo", b"").unwrap();
    txn.undo(third).unwrap();
    txn.undo(second).unwrap();
    txn.vote().unwrap();
    let tid = txn.finish().unwrap();
    assert_eq!(read_blob(&store, oid, tid), b"first blob");
}

#[test]
fn a_blob_undo_links_in_a_blob_directory_of_its_own() {
    let base = std::env::temp_dir().join(format!("zoe-undo-reloc-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let options = Options {
        blob_dir: Some(base.join("shared-blobs")),
        ..Options::default()
    };
    let store = Store::open_with(base.join("store"), options).unwrap();
    let oid = store.new_oid().unwrap();
    let first = commit_blob(&store, oid, Tid::ZERO, b"first blob");
    let second = commit_blob(&store, oid, first, b"second blob");
    let tid = undo(&store, second).unwrap();
    assert_eq!(read_blob(&store, oid, tid), b"first blob");
    let a = std::fs::metadata(store.blob_path(oid, first).unwrap()).unwrap();
    let b = std::fs::metadata(store.blob_path(oid, tid).unwrap()).unwrap();
    assert_eq!(a.ino(), b.ino());
    let redo = undo(&store, tid).unwrap();
    assert_eq!(read_blob(&store, oid, redo), b"second blob");
    drop(store);
    std::fs::remove_dir_all(&base).unwrap();
}

/// A directory that cannot make hard links, as a filesystem without them:
/// each link fails with the error that the function gives.
struct NoLinks(MemVfs, fn() -> io::Error);

impl Vfs for NoLinks {
    type File = MemFile;

    fn exists(&self, name: &str) -> io::Result<bool> {
        self.0.exists(name)
    }

    fn open(&self, name: &str) -> io::Result<MemFile> {
        self.0.open(name)
    }

    fn create_new(&self, name: &str) -> io::Result<MemFile> {
        self.0.create_new(name)
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        self.0.rename(from, to)
    }

    fn hard_link(&self, _from: &str, _to: &str) -> io::Result<()> {
        Err((self.1)())
    }

    fn remove(&self, name: &str) -> io::Result<()> {
        self.0.remove(name)
    }

    fn sync_dir(&self) -> io::Result<()> {
        self.0.sync_dir()
    }

    fn sync_dir_at(&self, dir: &str) -> io::Result<()> {
        self.0.sync_dir_at(dir)
    }

    fn create_dir(&self, name: &str) -> io::Result<()> {
        self.0.create_dir(name)
    }

    fn list_files(&self, dir: &str) -> io::Result<Vec<String>> {
        self.0.list_files(dir)
    }

    fn list_entries(&self, dir: &str) -> io::Result<Vec<String>> {
        self.0.list_entries(dir)
    }

    fn create_file(&self, name: &str) -> io::Result<Box<dyn WriteFile>> {
        self.0.create_file(name)
    }

    fn read_file(&self, name: &str) -> io::Result<Box<dyn Read + Send>> {
        self.0.read_file(name)
    }

    fn sync_file(&self, name: &str) -> io::Result<()> {
        self.0.sync_file(name)
    }

    fn set_read_only(&self, name: &str) -> io::Result<()> {
        self.0.set_read_only(name)
    }

    fn lock_dir(&self, name: &str) -> io::Result<DirLock> {
        self.0.lock_dir(name)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.path(name)
    }
}

#[test]
fn a_blob_undo_copies_into_a_new_file_where_the_directory_cannot_link() {
    // Two devices, and the `ENOTSUP` of FAT and exFAT on macOS, which Rust
    // does not map to `Unsupported`.
    let errors: [fn() -> io::Error; 2] = [
        || io::Error::from(io::ErrorKind::CrossesDevices),
        || io::Error::from_raw_os_error(libc::ENOTSUP),
    ];
    for error in errors {
        let vfs = MemVfs::new();
        let store = Store::open_in(NoLinks(vfs.clone(), error), SystemClock).unwrap();
        let oid = store.new_oid().unwrap();
        let first = commit_blob(&store, oid, Tid::ZERO, b"first blob");
        let second = commit_blob(&store, oid, first, b"second blob");
        let files = vfs.names().iter().filter(|n| n.ends_with(".blob")).count();
        let tid = undo(&store, second).unwrap();
        assert_eq!(read_blob(&store, oid, tid), b"first blob");
        assert_eq!(read_blob(&store, oid, first), b"first blob");
        let after = vfs.names().iter().filter(|n| n.ends_with(".blob")).count();
        assert_eq!(after, files + 1, "{:?}", error());
    }
}

#[test]
fn a_link_that_fails_for_another_reason_fails_the_undo() {
    let vfs = MemVfs::new();
    let error: fn() -> io::Error = || io::Error::other("a disk error");
    let store = Store::open_in(NoLinks(vfs, error), SystemClock).unwrap();
    let oid = store.new_oid().unwrap();
    let first = commit_blob(&store, oid, Tid::ZERO, b"first blob");
    let second = commit_blob(&store, oid, first, b"second blob");
    let mut txn = store.tpc_begin(b"", b"undo", b"").unwrap();
    assert!(matches!(txn.undo(second), Err(ZoeError::Io(_))));
}

#[test]
fn a_pack_between_the_undo_and_its_vote_fails_the_vote() {
    let store = store();
    let root = Oid(0);
    let x = store.new_oid().unwrap();
    let x = if x == root {
        store.new_oid().unwrap()
    } else {
        x
    };
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(root, Tid::ZERO, &mapping(Some(x))).unwrap();
    txn.store(x, Tid::ZERO, &mapping(None)).unwrap();
    txn.vote().unwrap();
    let t1 = txn.finish().unwrap();
    let t2 = commit(&store, root, t1, &mapping(None));
    let mut undo = store.tpc_begin(b"", b"undo", b"").unwrap();
    undo.undo(t2).unwrap();
    store.pack(t2).unwrap();
    // The vote fails with its locks held, before the finish.
    assert!(matches!(undo.vote(), Err(ZoeError::Undo { oid: None, .. })));
    assert!(store.lock_table_is_empty());
}

/// Returns the oid that a record of `mapping` refers to.
fn target_of(data: &[u8]) -> Option<Oid> {
    let at = data.windows(2).position(|w| w == b"C\x08")?;
    let bytes: [u8; 8] = data[at + 2..at + 10].try_into().ok()?;
    Some(Oid(u64::from_be_bytes(bytes)))
}

#[test]
fn an_undo_that_races_packs_never_names_a_corrupt_store() {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    let store = store();
    let root = Oid(0);
    let first = commit(&store, root, Tid::ZERO, &mapping(None));
    let serial = Mutex::new(first);
    let done = AtomicBool::new(false);
    let errors = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        scope.spawn(|| {
            while !done.load(Ordering::Relaxed) {
                store.pack(store.last_transaction()).unwrap();
            }
        });
        scope.spawn(|| {
            // The packer stops also when an assertion of this thread fails.
            let _done = SetOnDrop(&done);
            for _ in 0..200 {
                let x = store.new_oid().unwrap();
                let s = *serial.lock().unwrap();
                let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
                txn.store(root, s, &mapping(Some(x))).unwrap();
                txn.store(x, Tid::ZERO, &mapping(None)).unwrap();
                txn.vote().unwrap();
                let linked = txn.finish().unwrap();
                let dropped = commit(&store, root, linked, &mapping(None));
                *serial.lock().unwrap() = dropped;
                let mut undo = store.tpc_begin(b"", b"undo", b"").unwrap();
                let outcome = undo
                    .undo(dropped)
                    .and_then(|_| undo.vote())
                    .and_then(|()| undo.finish());
                match outcome {
                    Ok(tid) => {
                        *serial.lock().unwrap() = tid;
                        // The object that the undo links again loads.
                        let (data, _) = store.load(root).unwrap();
                        assert_eq!(target_of(&data), Some(x));
                        store.load(x).unwrap();
                    }
                    Err(error) => errors.lock().unwrap().push(error),
                }
            }
        });
    });
    for error in errors.into_inner().unwrap() {
        assert!(
            matches!(error, ZoeError::Undo { oid: None, reason } if reason.contains("pack")),
            "an undo that races a pack gives {error:?}"
        );
    }
    // The store does not halt.
    let (data, s) = store.load(root).unwrap();
    commit(&store, root, s, &data);
}

/// Sets its flag when it drops, so a loop that waits for the flag ends
/// also when the thread that holds it panics.
struct SetOnDrop<'a>(&'a std::sync::atomic::AtomicBool);

impl Drop for SetOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

#[test]
fn a_blob_undo_that_races_packs_never_names_a_corrupt_blob() {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    let store = store();
    let root = Oid(0);
    let blob = store.new_oid().unwrap();
    let blob = if blob == root {
        store.new_oid().unwrap()
    } else {
        blob
    };
    commit(&store, root, Tid::ZERO, &mapping(Some(blob)));
    let first = commit_blob(&store, blob, Tid::ZERO, b"revision 0");
    let serial = Mutex::new(first);
    let done = AtomicBool::new(false);
    let errors = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        scope.spawn(|| {
            while !done.load(Ordering::Relaxed) {
                store.pack(store.last_transaction()).unwrap();
            }
        });
        scope.spawn(|| {
            let _done = SetOnDrop(&done);
            // The bytes of the current blob.
            let mut now = b"revision 0".to_vec();
            for n in 1..=150u32 {
                let s = *serial.lock().unwrap();
                let revision = commit_blob(&store, blob, s, format!("revision {n}").as_bytes());
                *serial.lock().unwrap() = revision;
                // The undo gives the revision before back, whose blob file
                // a pack at the last tid can remove meanwhile.
                let mut undo = store.tpc_begin(b"", b"undo", b"").unwrap();
                let outcome = undo
                    .undo(revision)
                    .and_then(|_| undo.vote())
                    .and_then(|()| undo.finish());
                match outcome {
                    Ok(tid) => {
                        *serial.lock().unwrap() = tid;
                        assert_eq!(read_blob(&store, blob, tid), now);
                    }
                    Err(error) => {
                        now = format!("revision {n}").into_bytes();
                        errors.lock().unwrap().push(error);
                    }
                }
            }
        });
    });
    for error in errors.into_inner().unwrap() {
        assert!(
            matches!(error, ZoeError::Undo { oid: None, reason } if reason.contains("pack")),
            "a blob undo that races a pack gives {error:?}"
        );
    }
}

#[test]
fn an_undo_in_shadow_mode_logs_its_merge_and_leaves_the_oid_to_the_caller() {
    use zoe_core::{Mode, Resolved, StructuralMerges};

    let options = Options {
        structural_merges: StructuralMerges {
            mode: Mode::Shadow,
            ..StructuralMerges::default()
        },
        ..Options::default()
    };
    let store = Store::open_full(MemVfs::new(), SystemClock, options).unwrap();
    let oid = store.new_oid().unwrap();
    let first = commit(&store, oid, Tid::ZERO, &length(10));
    let undone = commit(&store, oid, first, &length(15));
    let last = commit(&store, oid, undone, &length(17));
    let mut txn = store.tpc_begin(b"", b"undo", b"").unwrap();
    let staged = txn.undo(undone).unwrap();
    assert_eq!(
        staged.conflicts.iter().map(|c| c.oid).collect::<Vec<_>>(),
        [oid]
    );
    assert_eq!(txn.staged(oid).unwrap(), None);
    let [entry] = txn.resolutions() else {
        panic!("{:?}", txn.resolutions());
    };
    assert_eq!(
        (entry.mode, entry.outcome),
        (Mode::Shadow, Resolved::Merged)
    );
    assert!(entry.undo && !entry.applied);
    assert_eq!(txn.undo_resolution_count(), 1);
    // The caller resolves the undo and stores its record over the serial.
    txn.store(oid, last, &length(12)).unwrap();
    assert_eq!(txn.vote_resolving().unwrap(), []);
    // The check of shadow mode is for the merges of the vote.
    assert_eq!(txn.verifications(), []);
    assert!(txn.resolutions()[0].undo);
    assert_eq!(txn.natively_resolved(), []);
    txn.finish().unwrap();
    assert_eq!(current(&store, oid).unwrap(), length(12));
}
