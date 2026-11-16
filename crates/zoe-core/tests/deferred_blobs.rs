//! The restore transactions whose blobs wait for the next durable commit
//! (F14, ADR 0027).
//!
//! `Store::tpc_begin_restore_deferred_blobs` begins a deferred restore
//! whose blob files stay in `blobs/tmp/` without a sync. The next durable
//! commit places them all first: `Store::sync`, a durable finish, or the
//! close of the store. A read of a waiting blob places them too. A crash
//! loses the deferred restores after the last durable commit, and the
//! next writer open leaves no blob file of them. A failed placement fails
//! every later one, and its restores never become durable.

use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

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

/// The bytes of the blob of `oid`.
fn blob_bytes(oid: Oid) -> Vec<u8> {
    format!("the blob of oid {}", oid.0).into_bytes()
}

/// Restore one blob record of `oid` under `tid`. A restore with deferred
/// blobs commits without a sync, and its blob waits.
fn restore_blob(store: &Store, tid: Tid, oid: Oid, deferred: bool) {
    let mut file = store.new_blob_file().unwrap();
    file.write_all(&blob_bytes(oid)).unwrap();
    let mut txn = if deferred {
        store
            .tpc_begin_restore_deferred_blobs(tid, b"copier", b"deferred", b"")
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

/// Return the blob files in their place of the store in `vfs`.
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

/// Read the blob of `oid` at `tid` and check its bytes.
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

/// Open a store in `vfs` with one durable blob restore under `BASE`.
fn base_store(vfs: &MemVfs) -> Store {
    let store = Store::open_vfs(vfs.clone()).unwrap();
    restore_blob(&store, BASE, Oid(1), false);
    store
}

#[test]
fn the_blobs_wait_in_the_temporary_directory_until_a_read_places_them() {
    let vfs = MemVfs::new();
    let store = base_store(&vfs);
    for n in 1..=3 {
        restore_blob(&store, Tid(BASE.0 + n), Oid(1 + n), true);
    }
    assert_eq!(store.last_transaction(), Tid(BASE.0 + 3));
    assert_eq!(blob_files(&vfs).len(), 1, "{:?}", vfs.names());
    assert_eq!(tmp_files(&vfs).len(), 3, "{:?}", vfs.names());

    // A read places every waiting blob, and keeps their intent files.
    assert_blob(&store, Oid(3), Tid(BASE.0 + 2));
    assert_eq!(blob_files(&vfs).len(), 4, "{:?}", vfs.names());
    for n in 0..=3 {
        assert_blob(&store, Oid(1 + n), Tid(BASE.0 + n));
    }
    // The sync removes the intent files.
    store.sync().unwrap();
    assert_eq!(tmp_files(&vfs), Vec::<String>::new());
}

#[test]
fn a_crash_loses_the_restores_and_their_waiting_blobs() {
    let vfs = MemVfs::new();
    let store = base_store(&vfs);
    for n in 1..=5 {
        restore_blob(&store, Tid(BASE.0 + n), Oid(1 + n), true);
    }

    let image = vfs.crash_now(|_| 0);
    let reopened = Store::open_vfs(image.clone()).unwrap();
    assert_eq!(reopened.last_transaction(), BASE);
    assert_blob(&reopened, Oid(1), BASE);
    for n in 1..=5 {
        assert!(matches!(
            reopened.load(Oid(1 + n)),
            Err(ZoeError::NotFound { .. })
        ));
    }
    assert_eq!(blob_files(&image).len(), 1, "{:?}", image.names());
    assert_eq!(tmp_files(&image), Vec::<String>::new());
    drop(store);
}

#[test]
fn every_crash_image_of_a_sync_holds_a_prefix_without_orphans() {
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

#[test]
fn a_durable_finish_places_the_waiting_blobs_first() {
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
    assert_eq!(tmp_files(&image), Vec::<String>::new());
    drop(store);
}

#[test]
fn the_close_places_the_waiting_blobs() {
    let vfs = MemVfs::new();
    let store = base_store(&vfs);
    for n in 1..=3 {
        restore_blob(&store, Tid(BASE.0 + n), Oid(1 + n), true);
    }
    drop(store);

    let image = vfs.crash_now(|_| 0);
    let reopened = Store::open_vfs(image.clone()).unwrap();
    assert_eq!(reopened.last_transaction(), Tid(BASE.0 + 3));
    for n in 0..=3 {
        assert_blob(&reopened, Oid(1 + n), Tid(BASE.0 + n));
    }
}

/// The operation of the blob store that `Faulty` fails once.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Fault {
    /// The creation of an intent file.
    IntentWrite,
    /// A `sync_many`.
    SyncMany,
    /// The rename of a blob file into its place.
    Rename,
}

/// A `MemVfs` that fails one operation of the blob store once.
struct Faulty {
    vfs: MemVfs,
    fault: Arc<Mutex<Option<Fault>>>,
}

impl Faulty {
    /// Fail the operation `fault` when it comes, once.
    fn hit(&self, fault: Fault) -> io::Result<()> {
        let mut armed = self.fault.lock().unwrap();
        if *armed == Some(fault) {
            *armed = None;
            return Err(io::Error::other(format!("injected fault: {fault:?}")));
        }
        Ok(())
    }
}

impl Vfs for Faulty {
    type File = MemFile;

    fn exists(&self, name: &str) -> io::Result<bool> {
        self.vfs.exists(name)
    }

    fn open(&self, name: &str) -> io::Result<MemFile> {
        self.vfs.open(name)
    }

    fn create_new(&self, name: &str) -> io::Result<MemFile> {
        self.vfs.create_new(name)
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        if to.ends_with(".blob") {
            self.hit(Fault::Rename)?;
        }
        self.vfs.rename(from, to)
    }

    fn hard_link(&self, from: &str, to: &str) -> io::Result<()> {
        self.vfs.hard_link(from, to)
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
        if name.ends_with(".intent") {
            self.hit(Fault::IntentWrite)?;
        }
        self.vfs.create_file(name)
    }

    fn read_file(&self, name: &str) -> io::Result<Box<dyn Read + Send>> {
        self.vfs.read_file(name)
    }

    fn sync_file(&self, name: &str) -> io::Result<()> {
        self.vfs.sync_file(name)
    }

    fn sync_many(&self, files: &[String], dirs: &[String]) -> io::Result<()> {
        self.hit(Fault::SyncMany)?;
        self.vfs.sync_many(files, dirs)
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

/// Check the store in `vfs` after the loss of the restores with deferred
/// blobs: it ends at `BASE`, its one blob reads, and no blob file of a
/// lost restore and no temporary file stay.
fn assert_lost_restores(vfs: &MemVfs, what: &str) {
    let reopened = Store::open_vfs(vfs.clone()).unwrap_or_else(|e| panic!("{what}: {e}"));
    assert_eq!(reopened.last_transaction(), BASE, "{what}");
    assert_blob(&reopened, Oid(1), BASE);
    assert_eq!(blob_files(vfs).len(), 1, "{what}: {:?}", vfs.names());
    assert_eq!(tmp_files(vfs), Vec::<String>::new(), "{what}");
}

#[test]
fn a_failed_placement_never_makes_the_records_durable() {
    for fault in [Fault::IntentWrite, Fault::SyncMany, Fault::Rename] {
        let vfs = MemVfs::new();
        let armed = Arc::new(Mutex::new(None));
        let faulty = Faulty {
            vfs: vfs.clone(),
            fault: armed.clone(),
        };
        let store = Store::open_in(faulty, SystemClock).unwrap();
        restore_blob(&store, BASE, Oid(1), false);
        for n in 1..=3 {
            restore_blob(&store, Tid(BASE.0 + n), Oid(1 + n), true);
        }

        *armed.lock().unwrap() = Some(fault);
        assert!(store.sync().is_err(), "{fault:?}");
        assert_eq!(*armed.lock().unwrap(), None, "{fault:?}: the fault came");
        // A retry fails: the sync, and a read of a waiting blob. A read of
        // a blob that does not wait still works.
        assert!(store.sync().is_err(), "{fault:?}");
        assert!(
            store.open_blob(Oid(3), Tid(BASE.0 + 2)).is_err(),
            "{fault:?}"
        );
        assert_blob(&store, Oid(1), BASE);
        // The drop leaves the deferred restores to the next open.
        drop(store);

        let what = format!("{fault:?}, after the drop");
        assert_lost_restores(&vfs.crash_now(|_| 0), &format!("{what} and a crash"));
        vfs.restart();
        assert_lost_restores(&vfs, &what);
    }
}

#[test]
fn every_crash_image_of_a_sequence_reservation_holds_a_prefix_without_orphans() {
    let vfs = MemVfs::new();
    drop(base_store(&vfs));
    let vfs = vfs.durable_copy();
    let store = Store::open_vfs(vfs.clone()).unwrap();
    restore_blob(&store, Tid(BASE.0 + 1), Oid(2), true);
    restore_blob(&store, Tid(BASE.0 + 2), Oid(3), true);
    // The reservation of a sequence commits with a sync, and the hook of
    // the backend places the waiting blobs first.
    store.sequence("counter").next().unwrap();
    vfs.mark("reserved");

    for image in vfs.crash_images() {
        let files = image.vfs();
        let reopened = Store::open_vfs(files.clone())
            .unwrap_or_else(|e| panic!("{image:?} does not open: {e}"));
        let last = reopened.last_transaction();
        assert!(
            (BASE.0..=BASE.0 + 2).contains(&last.0),
            "{image:?}: the last tid {last:?} is a tid of the copy"
        );
        if image.is_after("reserved") {
            assert_eq!(last, Tid(BASE.0 + 2), "{image:?}: the reservation returned");
        }
        for n in 0..=last.0 - BASE.0 {
            assert_blob(&reopened, Oid(1 + n), Tid(BASE.0 + n));
        }
        let expected = (last.0 - BASE.0 + 1) as usize;
        assert_eq!(blob_files(&files).len(), expected, "{image:?}");
        assert_eq!(tmp_files(&files), Vec::<String>::new(), "{image:?}");
    }
    drop(store);
}
