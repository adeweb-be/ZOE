//! Acceptance test of the batched sync of the blobs of the `Data.fs`
//! converter (F14, ADR 0027).
//!
//! The converter restores each transaction with
//! `tpc_begin_restore_deferred_blobs`: the blob files wait in the blob
//! temporary directory without a sync, and the sync of each batch places
//! them all and makes them durable before the database file. So the syncs
//! of the blob files and of the blob directories grow with the batches,
//! not with the blobs. A crash loses only the open batch, and the next
//! open leaves no blob file of a tid after the last tid of the store.
//!
//! The test writes the `Data.fs` itself: one transaction for each oid,
//! with one blob record each, and a lawn blob directory.

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use zoe_convert::from_datafs;
use zoe_core::vfs::{DirLock, MemVfs, OsFile, OsVfs, Vfs, WriteFile};
use zoe_core::{Oid, Store, SystemClock, Tid};

mod support;
use support::fresh_dir;

/// The record of a blob: the class pickle of `ZODB.blob.Blob`, then the
/// state `None`.
const BLOB: &[u8] = b"\x80\x03cZODB.blob\nBlob\nq\x00.\x80\x03N.";

/// The tid of the first transaction of the file.
const FIRST_TID: u64 = 0x03f0_0000_0000_0000;

/// The most transactions in one batch of the converter.
const BATCH: u64 = 1_000;

/// The count of transactions of the file. Each writes one blob.
const COUNT: u64 = 3_000;

/// The bytes of the blob of `oid`.
fn blob_bytes(oid: Oid) -> Vec<u8> {
    format!("the blob of oid {}", oid.0).into_bytes()
}

/// Write a `Data.fs` of `count` transactions in `dir`, and its lawn blob
/// directory. The transaction `i` writes a blob record of the oid `i + 1`.
/// Return the path of the file, the path of the blob directory and the
/// tids.
fn write_data_fs(dir: &Path, count: u64) -> (PathBuf, PathBuf, Vec<Tid>) {
    let blob_dir = dir.join("blobs");
    fs::create_dir_all(&blob_dir).unwrap();
    fs::write(blob_dir.join(".layout"), "lawn\n").unwrap();
    let mut bytes = b"FS30".to_vec();
    let mut tids = Vec::new();
    for i in 0..count {
        let tid = FIRST_TID + i;
        let oid = i + 1;
        let oid_dir = blob_dir.join(Oid(oid).repr());
        fs::create_dir_all(&oid_dir).unwrap();
        let path = oid_dir.join(format!("{}.blob", Tid(tid).repr()));
        fs::write(path, blob_bytes(Oid(oid))).unwrap();
        let data = BLOB;
        let user = b"admin".as_slice();
        let description = format!("transaction {i}").into_bytes();
        let pos = bytes.len() as u64;
        let mut body = Vec::new();
        body.extend_from_slice(user);
        body.extend_from_slice(&description);
        body.extend_from_slice(&oid.to_be_bytes());
        body.extend_from_slice(&tid.to_be_bytes());
        body.extend_from_slice(&0u64.to_be_bytes());
        body.extend_from_slice(&pos.to_be_bytes());
        body.extend_from_slice(&[0, 0]);
        body.extend_from_slice(&(data.len() as u64).to_be_bytes());
        body.extend_from_slice(data);
        let len = 23 + body.len() as u64;
        bytes.extend_from_slice(&tid.to_be_bytes());
        bytes.extend_from_slice(&len.to_be_bytes());
        bytes.push(b' ');
        bytes.extend_from_slice(&(user.len() as u16).to_be_bytes());
        bytes.extend_from_slice(&(description.len() as u16).to_be_bytes());
        bytes.extend_from_slice(&0u16.to_be_bytes());
        bytes.extend_from_slice(&body);
        bytes.extend_from_slice(&len.to_be_bytes());
        tids.push(Tid(tid));
    }
    let data_fs = dir.join("Data.fs");
    fs::write(&data_fs, bytes).unwrap();
    (data_fs, blob_dir, tids)
}

/// Tell if `name` is in the blob directory of a store.
fn in_blobs(name: &str) -> bool {
    name == "blobs" || name.starts_with("blobs/")
}

/// The directory of a store that counts the syncs of the files and the
/// directories of its blob directory. One `sync_many` counts as one sync:
/// on Linux, `OsVfs` makes it one `syncfs(2)`.
struct Counting {
    vfs: OsVfs,
    syncs: Arc<AtomicU64>,
}

impl Counting {
    fn count(&self, name: &str) {
        if in_blobs(name) {
            self.syncs.fetch_add(1, Ordering::SeqCst);
        }
    }
}

/// A file of the blob directory that counts its syncs.
struct CountedWrite {
    file: Box<dyn WriteFile>,
    syncs: Arc<AtomicU64>,
}

impl Write for CountedWrite {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.file.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

impl WriteFile for CountedWrite {
    fn sync(&mut self) -> io::Result<()> {
        self.syncs.fetch_add(1, Ordering::SeqCst);
        self.file.sync()
    }
}

impl Vfs for Counting {
    type File = OsFile;

    fn exists(&self, name: &str) -> io::Result<bool> {
        self.vfs.exists(name)
    }

    fn open(&self, name: &str) -> io::Result<OsFile> {
        self.vfs.open(name)
    }

    fn create_new(&self, name: &str) -> io::Result<OsFile> {
        self.vfs.create_new(name)
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
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
        self.count(dir);
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
        let file = self.vfs.create_file(name)?;
        if !in_blobs(name) {
            return Ok(file);
        }
        Ok(Box::new(CountedWrite {
            file,
            syncs: self.syncs.clone(),
        }))
    }

    fn read_file(&self, name: &str) -> io::Result<Box<dyn Read + Send>> {
        self.vfs.read_file(name)
    }

    fn sync_file(&self, name: &str) -> io::Result<()> {
        self.count(name);
        self.vfs.sync_file(name)
    }

    fn sync_many(&self, files: &[String], dirs: &[String]) -> io::Result<()> {
        self.syncs.fetch_add(1, Ordering::SeqCst);
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

/// Return the names of the blob files of a store in `vfs`.
fn blob_files(vfs: &MemVfs) -> Vec<String> {
    let mut names = vfs.names();
    names.retain(|name| name.starts_with("blobs/") && name.ends_with(".blob"));
    names
}

/// Return the tid of the blob file `name` of a store: the name of the file
/// is `0x<16 hex digits of the tid>.blob`.
fn blob_tid(name: &str) -> Tid {
    let file = name.rsplit('/').next().unwrap();
    let hex = file
        .strip_prefix("0x")
        .and_then(|rest| rest.strip_suffix(".blob"))
        .unwrap_or_else(|| panic!("{name} is not a blob file"));
    Tid(u64::from_str_radix(hex, 16).unwrap())
}

/// Return the names of the blob files under `dir` in the filesystem, with
/// `/` between their parts, from `dir`.
fn disk_blob_files(dir: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_owned()];
    while let Some(path) = stack.pop() {
        for entry in fs::read_dir(&path).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "blob") {
                let name = path.strip_prefix(dir).unwrap().to_str().unwrap();
                found.push(name.replace(std::path::MAIN_SEPARATOR, "/"));
            }
        }
    }
    found.sort();
    found
}

/// Write each file of `vfs` into the directory `dir` of the filesystem.
fn write_to_disk(vfs: &MemVfs, dir: &Path) {
    for name in vfs.names() {
        let path = dir.join(&name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut bytes = Vec::new();
        Vfs::read_file(vfs, &name)
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        fs::write(path, bytes).unwrap();
    }
}

#[test]
fn the_blob_syncs_of_a_conversion_grow_with_the_batches() {
    let dir = fresh_dir("count");
    let (data_fs, blob_dir, tids) = write_data_fs(&dir, COUNT);
    let open = |name: &str| {
        let syncs = Arc::new(AtomicU64::new(0));
        let vfs = Counting {
            vfs: OsVfs::new(&dir.join(name)).unwrap(),
            syncs: syncs.clone(),
        };
        (Store::open_in(vfs, SystemClock).unwrap(), syncs)
    };

    // The counter works: a durable restore of one blob, the first of its
    // store, syncs the blob file and the blob directories several times.
    let (store, syncs) = open("check");
    let mut txn = store.tpc_begin_restore(Tid(1), b"", b"", b"").unwrap();
    let mut file = store.new_blob_file().unwrap();
    file.write_all(b"a durable blob").unwrap();
    txn.restore_blob(Oid(1), BLOB, file).unwrap();
    txn.vote().unwrap();
    txn.finish().unwrap();
    let durable = syncs.load(Ordering::SeqCst);
    assert!(durable >= 6, "a durable blob took {durable} syncs");
    drop(store);

    let (store, syncs) = open("target");
    let before = syncs.load(Ordering::SeqCst);
    let report = from_datafs(&data_fs, Some(&blob_dir), &store).unwrap();
    let taken = syncs.load(Ordering::SeqCst) - before;

    assert!(report.problems.is_empty(), "{report:?}");
    assert_eq!((report.transactions, report.copied), (COUNT, COUNT));
    assert_eq!(store.last_transaction(), *tids.last().unwrap());
    // Two syncs for each batch, and a few more for the end. One sync for
    // each blob would take 3,000.
    let batches = COUNT / BATCH;
    assert!(
        taken <= 2 * batches + 5,
        "{COUNT} blobs in {batches} batches took {taken} syncs of the blob directory"
    );
    for (i, tid) in tids.iter().enumerate() {
        let oid = Oid(i as u64 + 1);
        let mut bytes = Vec::new();
        store
            .open_blob(oid, *tid)
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        assert_eq!(bytes, blob_bytes(oid));
    }
}

#[test]
fn a_crash_in_an_open_batch_leaves_no_blob_after_the_last_tid() {
    // The process dies in the sync of the third batch, at the first sync
    // of the directory of the blob of this transaction. The batch is open:
    // the database file has not synced it.
    const KILLED_AT: u64 = 2_500;
    let dir = fresh_dir("crash");
    let (data_fs, blob_dir, tids) = write_data_fs(&dir, COUNT);

    let vfs = MemVfs::new();
    let store = Store::open_vfs(vfs.clone()).unwrap();
    let mut oid_dir = String::from("blobs");
    for byte in (KILLED_AT + 1).to_be_bytes() {
        oid_dir.push_str(&format!("/0x{byte:02x}"));
    }
    vfs.kill_at_dir_sync(&oid_dir);
    assert!(from_datafs(&data_fs, Some(&blob_dir), &store).is_err());
    // A killed process runs no destructor.
    std::mem::forget(store);
    vfs.restart();

    // A power loss keeps what was synced. The open removes the blob files
    // that the lost restores placed.
    let image = vfs.crash_now(|_| 0);
    let store = Store::open_vfs(image.clone()).unwrap();
    let last = store.last_transaction();
    let durable =
        tids.iter()
            .position(|tid| *tid == last)
            .unwrap_or_else(|| panic!("the last tid {last:?} is a tid of the file")) as u64;
    assert_eq!(durable + 1, 2 * BATCH, "the store ends after two batches");
    let files = blob_files(&image);
    assert_eq!(files.len() as u64, durable + 1, "{files:?}");
    for name in &files {
        assert!(
            blob_tid(name) <= last,
            "{name} is after the last tid {last:?}"
        );
    }
    let mut leftovers = image.names();
    leftovers.retain(|name| name.starts_with("blobs/tmp/"));
    assert_eq!(leftovers, Vec::<String>::new());
    drop(store);

    // The next conversion runs on the files of the image on disk, so the
    // verification reads each blob file. It resumes after the last tid,
    // and finds no problem.
    let target = dir.join("target");
    write_to_disk(&image, &target);
    let store = Store::open(&target).unwrap();
    let report = from_datafs(&data_fs, Some(&blob_dir), &store).unwrap();
    assert!(report.problems.is_empty(), "{report:?}");
    assert_eq!(report.transactions, COUNT);
    assert_eq!(report.copied, COUNT - durable - 1);
    let last = store.last_transaction();
    assert_eq!(last, *tids.last().unwrap());
    let files = disk_blob_files(&target.join("blobs"));
    assert_eq!(files.len() as u64, COUNT);
    for name in &files {
        assert!(
            blob_tid(name) <= last,
            "{name} is after the last tid {last:?}"
        );
    }
}
