//! Acceptance test of the restore transactions that commit without a sync
//! (F14).
//!
//! `Store::tpc_begin_restore_deferred` begins a restore transaction whose
//! finish commits without a sync of the database file. The commit is
//! visible at once. `Store::sync` makes every such commit durable with one
//! sync, and a durable finish does the same. A crash loses the deferred
//! commits after the last durable commit, and the next writer open removes
//! their blob files: the intent file of each deferred commit stays in
//! `blobs/tmp/` until a durable commit covers it.

use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use redb::StorageBackend;

use zoe_core::vfs::{DirLock, MemFile, MemVfs, Vfs, WriteFile};
use zoe_core::{Oid, Store, SystemClock, Tid, ZoeError};

/// A `PersistentMapping` record.
const PICKLE: &[u8] =
    b"\x80\x03cpersistent.mapping\nPersistentMapping\nq\x00.\x80\x03}q\x01X\x04\x00\x00\x00datag\x01s.";

/// The record of a blob: the class pickle of `ZODB.blob.Blob`, then the
/// state `None`.
const BLOB: &[u8] = b"\x80\x03cZODB.blob\nBlob\nq\x00.\x80\x03N.";

/// The tid of the durable restore that each test starts with.
const BASE: Tid = Tid(10);

/// The database file of a store.
const DATABASE: &str = "zoe.redb";

/// The bytes of the blob of `oid`.
fn blob_bytes(oid: Oid) -> Vec<u8> {
    format!("the blob of oid {}", oid.0).into_bytes()
}

/// Restore one blob record of `oid` under `tid`. A deferred restore
/// commits without a sync.
fn restore_blob(store: &Store, tid: Tid, oid: Oid, deferred: bool) {
    let mut file = store.new_blob_file().unwrap();
    file.write_all(&blob_bytes(oid)).unwrap();
    let mut txn = if deferred {
        store
            .tpc_begin_restore_deferred(tid, b"copier", b"deferred", b"")
            .unwrap()
    } else {
        store
            .tpc_begin_restore(tid, b"copier", b"durable", b"")
            .unwrap()
    };
    txn.restore_blob(oid, BLOB, file).unwrap();
    txn.vote().unwrap();
    assert_eq!(txn.finish().unwrap(), tid);
}

/// Restore one record of `oid` under `tid` without a sync.
fn restore_deferred(store: &Store, tid: Tid, oid: Oid) {
    let mut txn = store
        .tpc_begin_restore_deferred(tid, b"copier", b"deferred", b"")
        .unwrap();
    txn.restore(oid, Some(PICKLE)).unwrap();
    txn.vote().unwrap();
    assert_eq!(txn.finish().unwrap(), tid);
}

/// Return the blob files of the store in `vfs`.
fn blob_files(vfs: &MemVfs) -> Vec<String> {
    let mut names = vfs.names();
    names.retain(|name| name.starts_with("blobs/") && name.ends_with(".blob"));
    names
}

/// Return the files in `blobs/tmp/` of the store in `vfs`.
fn tmp_files(vfs: &MemVfs) -> Vec<String> {
    let mut names = vfs.names();
    names.retain(|name| name.starts_with("blobs/tmp/"));
    names
}

/// Read the blob of `oid` at `tid` and check its bytes, its size and its
/// hash.
fn assert_blob(store: &Store, oid: Oid, tid: Tid) {
    let mut bytes = Vec::new();
    store
        .open_blob(oid, tid)
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    assert_eq!(bytes, blob_bytes(oid));
    store.verify_blob(oid, tid).unwrap();
}

/// Open a store in `vfs` with one durable blob restore under `BASE`, for
/// the oid 1.
fn base_store(vfs: &MemVfs) -> Store {
    let store = Store::open_vfs(vfs.clone()).unwrap();
    restore_blob(&store, BASE, Oid(1), false);
    store
}

#[test]
fn a_deferred_restore_is_visible_before_it_is_durable() {
    let vfs = MemVfs::new();
    let store = base_store(&vfs);
    restore_blob(&store, Tid(11), Oid(2), true);
    restore_deferred(&store, Tid(12), Oid(3));

    assert_eq!(store.last_transaction(), Tid(12));
    assert_eq!(store.load(Oid(3)).unwrap(), (PICKLE.to_vec(), Tid(12)));
    assert_eq!(store.load(Oid(2)).unwrap(), (BLOB.to_vec(), Tid(11)));
    assert_blob(&store, Oid(2), Tid(11));
    // The tid of a deferred restore counts: the next restore comes after
    // it.
    let refused = store.tpc_begin_restore_deferred(Tid(12), b"", b"", b"");
    assert!(matches!(refused, Err(ZoeError::Protocol(_))));
}

#[test]
fn a_crash_loses_the_deferred_restores_and_the_next_open_removes_their_blob_files() {
    let vfs = MemVfs::new();
    let store = base_store(&vfs);
    for n in 1..=5 {
        restore_blob(&store, Tid(BASE.0 + n), Oid(1 + n), true);
    }
    // Each blob file is in its place before the backend commit.
    assert_eq!(blob_files(&vfs).len(), 6, "{:?}", vfs.names());

    // A crash keeps only what was synced.
    let image = vfs.crash_now(|_| 0);
    let reopened = Store::open_vfs(image.clone()).unwrap();
    assert_eq!(reopened.last_transaction(), BASE);
    assert_blob(&reopened, Oid(1), BASE);
    for n in 1..=5 {
        assert!(
            matches!(reopened.load(Oid(1 + n)), Err(ZoeError::NotFound { .. })),
            "the deferred restore of oid {} is lost",
            1 + n
        );
    }
    // The open removed the blob files of the lost restores and every
    // intent file.
    assert_eq!(blob_files(&image).len(), 1, "{:?}", image.names());
    assert_eq!(tmp_files(&image), Vec::<String>::new());

    // The copy resumes after the last durable tid.
    for n in 1..=5 {
        restore_blob(&reopened, Tid(BASE.0 + n), Oid(1 + n), true);
    }
    reopened.sync().unwrap();
    for n in 1..=5 {
        assert_blob(&reopened, Oid(1 + n), Tid(BASE.0 + n));
    }
    drop(store);
}

#[test]
fn sync_makes_every_deferred_restore_durable() {
    let vfs = MemVfs::new();
    let store = base_store(&vfs);
    for n in 1..=5 {
        restore_blob(&store, Tid(BASE.0 + n), Oid(1 + n), true);
    }
    restore_deferred(&store, Tid(BASE.0 + 6), Oid(100));
    store.sync().unwrap();

    let image = vfs.crash_now(|_| 0);
    let reopened = Store::open_vfs(image.clone()).unwrap();
    assert_eq!(reopened.last_transaction(), Tid(BASE.0 + 6));
    for n in 0..=5 {
        assert_blob(&reopened, Oid(1 + n), Tid(BASE.0 + n));
    }
    assert_eq!(reopened.load(Oid(100)).unwrap().1, Tid(BASE.0 + 6));
    assert_eq!(blob_files(&image).len(), 6);
    assert_eq!(tmp_files(&image), Vec::<String>::new());
    drop(store);
}

#[test]
fn sync_with_nothing_deferred_changes_nothing() {
    let vfs = MemVfs::new();
    let store = base_store(&vfs);
    store.sync().unwrap();
    store.sync().unwrap();
    assert_eq!(store.last_transaction(), BASE);
    assert_blob(&store, Oid(1), BASE);
}

#[test]
fn a_durable_finish_makes_the_deferred_restores_before_it_durable() {
    let vfs = MemVfs::new();
    let store = base_store(&vfs);
    for n in 1..=3 {
        restore_blob(&store, Tid(BASE.0 + n), Oid(1 + n), true);
    }
    let oid = store.new_oid().unwrap();
    let mut txn = store.tpc_begin(b"user", b"normal commit", b"").unwrap();
    txn.store(oid, Tid::ZERO, PICKLE).unwrap();
    txn.vote().unwrap();
    let tid = txn.finish().unwrap();

    let image = vfs.crash_now(|_| 0);
    let reopened = Store::open_vfs(image.clone()).unwrap();
    assert_eq!(reopened.last_transaction(), tid);
    for n in 0..=3 {
        assert_blob(&reopened, Oid(1 + n), Tid(BASE.0 + n));
    }
    assert_eq!(reopened.load(oid).unwrap(), (PICKLE.to_vec(), tid));
    assert_eq!(tmp_files(&image), Vec::<String>::new());
    drop(store);
}

#[test]
fn every_crash_image_of_deferred_restores_holds_a_prefix_without_orphans() {
    let vfs = MemVfs::new();
    drop(base_store(&vfs));
    // The record of the filesystem starts after the durable restore.
    let vfs = vfs.durable_copy();
    let store = Store::open_vfs(vfs.clone()).unwrap();
    restore_blob(&store, Tid(BASE.0 + 1), Oid(2), true);
    restore_blob(&store, Tid(BASE.0 + 2), Oid(3), true);
    store.sync().unwrap();
    vfs.mark("synced");

    for image in vfs.crash_images() {
        let files = image.vfs();
        let reopened = Store::open_vfs(files.clone())
            .unwrap_or_else(|e| panic!("{image:?} does not open: {e}"));
        let last = reopened.last_transaction();
        assert!(
            (BASE.0..=BASE.0 + 2).contains(&last.0),
            "{image:?}: the last tid {last:?} is a tid of the copy"
        );
        if image.is_after("synced") {
            assert_eq!(last, Tid(BASE.0 + 2), "{image:?}: sync returned");
        }
        // Each restore up to the last tid has its blob file. No blob file
        // of a lost restore stays.
        for n in 0..=last.0 - BASE.0 {
            assert_blob(&reopened, Oid(1 + n), Tid(BASE.0 + n));
        }
        let expected = (last.0 - BASE.0 + 1) as usize;
        assert_eq!(blob_files(&files).len(), expected, "{image:?}");
        assert_eq!(tmp_files(&files), Vec::<String>::new(), "{image:?}");
    }
}

/// A `MemVfs` that counts the syncs of the database file.
struct Counting {
    vfs: MemVfs,
    syncs: Arc<AtomicU64>,
}

/// A file of `Counting`.
#[derive(Debug)]
struct CountedFile {
    file: MemFile,
    /// The counter of the database file, `None` for another file.
    syncs: Option<Arc<AtomicU64>>,
}

impl Counting {
    fn counted(&self, name: &str, file: MemFile) -> CountedFile {
        let syncs = (name == DATABASE).then(|| self.syncs.clone());
        CountedFile { file, syncs }
    }
}

impl StorageBackend for CountedFile {
    fn len(&self) -> io::Result<u64> {
        self.file.len()
    }

    fn read(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
        self.file.read(offset, out)
    }

    fn set_len(&self, len: u64) -> io::Result<()> {
        self.file.set_len(len)
    }

    fn sync_data(&self) -> io::Result<()> {
        if let Some(syncs) = &self.syncs {
            syncs.fetch_add(1, Ordering::SeqCst);
        }
        self.file.sync_data()
    }

    fn write(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        self.file.write(offset, data)
    }
}

impl Vfs for Counting {
    type File = CountedFile;

    fn exists(&self, name: &str) -> io::Result<bool> {
        self.vfs.exists(name)
    }

    fn open(&self, name: &str) -> io::Result<CountedFile> {
        Ok(self.counted(name, self.vfs.open(name)?))
    }

    fn create_new(&self, name: &str) -> io::Result<CountedFile> {
        Ok(self.counted(name, self.vfs.create_new(name)?))
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        self.vfs.rename(from, to)
    }

    fn remove(&self, name: &str) -> io::Result<()> {
        self.vfs.remove(name)
    }

    fn sync_dir(&self) -> io::Result<()> {
        self.vfs.sync_dir()
    }

    fn sync_dir_at(&self, dir: &str) -> io::Result<()> {
        self.vfs.sync_dir_at(dir)
    }

    fn create_dir(&self, name: &str) -> io::Result<()> {
        self.vfs.create_dir(name)
    }

    fn list_files(&self, dir: &str) -> io::Result<Vec<String>> {
        self.vfs.list_files(dir)
    }

    fn list_entries(&self, dir: &str) -> io::Result<Vec<String>> {
        self.vfs.list_entries(dir)
    }

    fn create_file(&self, name: &str) -> io::Result<Box<dyn WriteFile>> {
        self.vfs.create_file(name)
    }

    fn read_file(&self, name: &str) -> io::Result<Box<dyn Read + Send>> {
        self.vfs.read_file(name)
    }

    fn sync_file(&self, name: &str) -> io::Result<()> {
        self.vfs.sync_file(name)
    }

    fn set_read_only(&self, name: &str) -> io::Result<()> {
        self.vfs.set_read_only(name)
    }

    fn lock_dir(&self, name: &str) -> io::Result<DirLock> {
        self.vfs.lock_dir(name)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.vfs.path(name)
    }
}

#[test]
fn deferred_restores_take_one_sync_of_the_database_file() {
    let syncs = Arc::new(AtomicU64::new(0));
    let vfs = Counting {
        vfs: MemVfs::new(),
        syncs: syncs.clone(),
    };
    let store = Store::open_in(vfs, SystemClock).unwrap();

    // The counter works: each durable restore syncs the database file.
    let before = syncs.load(Ordering::SeqCst);
    for n in 1..=20 {
        let mut txn = store
            .tpc_begin_restore(Tid(n), b"", b"durable", b"")
            .unwrap();
        txn.restore(Oid(n), Some(PICKLE)).unwrap();
        txn.vote().unwrap();
        txn.finish().unwrap();
    }
    assert!(syncs.load(Ordering::SeqCst) - before >= 20);

    let before = syncs.load(Ordering::SeqCst);
    for n in 21..=220 {
        restore_deferred(&store, Tid(n), Oid(n));
    }
    store.sync().unwrap();
    let taken = syncs.load(Ordering::SeqCst) - before;
    assert!(
        taken <= 2,
        "200 deferred restores and one sync took {taken} syncs of the database file"
    );
    assert_eq!(store.last_transaction(), Tid(220));
}
