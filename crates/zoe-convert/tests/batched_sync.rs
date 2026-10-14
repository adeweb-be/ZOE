//! Acceptance test of the batched sync of the `Data.fs` converter (F14).
//!
//! The converter restores each transaction of the file without a sync of
//! the database file, and makes the store durable once per batch of at
//! most 1,000 transactions, and once at the end, before the verification.
//! A crash loses only the restores of the open batch. The store then ends
//! at a tid of the file, and the next conversion resumes after it.
//!
//! The test writes the `Data.fs` itself: one transaction for each oid,
//! with one record each. A transaction with a blob has a file in a lawn
//! blob directory.

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use redb::StorageBackend;
use redb::backends::FileBackend;
use zoe_convert::from_datafs;
use zoe_core::vfs::{DirLock, MemVfs, OsVfs, Vfs, WriteFile};
use zoe_core::{Oid, Store, SystemClock, Tid};

/// The record of a blob: the class pickle of `ZODB.blob.Blob`, then the
/// state `None`.
const BLOB: &[u8] = b"\x80\x03cZODB.blob\nBlob\nq\x00.\x80\x03N.";

/// The tid of the first transaction of the file.
const FIRST_TID: u64 = 0x03f0_0000_0000_0000;

/// The most transactions in one batch.
const BATCH: u64 = 1_000;

/// The database file of a store.
const DATABASE: &str = "zoe.redb";

fn fresh_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("zoe-batched-sync-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// A `PersistentMapping` whose state holds the number `n`.
fn record(n: u64) -> Vec<u8> {
    let mut pickle = b"\x80\x03cpersistent.mapping\nPersistentMapping\nq\x00.".to_vec();
    pickle.extend_from_slice(b"\x80\x03}q\x00X\x01\x00\x00\x00vq\x01\x8a\x08");
    pickle.extend_from_slice(&n.to_le_bytes());
    pickle.extend_from_slice(b"s.");
    pickle
}

/// Write an oid or a tid as ZODB's `oid_repr` does.
fn repr(n: u64) -> String {
    let digits = format!("{n:x}");
    if digits.len() % 2 == 1 {
        format!("0x0{digits}")
    } else {
        format!("0x{digits}")
    }
}

/// The bytes of the blob of `oid`.
fn blob_bytes(oid: Oid) -> Vec<u8> {
    format!("the blob of oid {}", oid.0).into_bytes()
}

/// Write a `Data.fs` of `count` transactions in `dir`, and its lawn blob
/// directory. The transaction `i` writes the oid `i + 1`. It writes a blob
/// when `i` is in `blobs`, else a record. Return the path of the file, the
/// path of the blob directory and the tids.
fn write_data_fs(dir: &Path, count: u64, blobs: &[u64]) -> (PathBuf, PathBuf, Vec<Tid>) {
    let blob_dir = dir.join("blobs");
    fs::create_dir_all(&blob_dir).unwrap();
    fs::write(blob_dir.join(".layout"), "lawn\n").unwrap();
    let mut bytes = b"FS30".to_vec();
    let mut tids = Vec::new();
    for i in 0..count {
        let tid = FIRST_TID + i;
        let oid = i + 1;
        let data = if blobs.contains(&i) {
            let oid_dir = blob_dir.join(repr(oid));
            fs::create_dir_all(&oid_dir).unwrap();
            let path = oid_dir.join(format!("{}.blob", repr(tid)));
            fs::write(path, blob_bytes(Oid(oid))).unwrap();
            BLOB.to_vec()
        } else {
            record(i)
        };
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
        body.extend_from_slice(&data);
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

/// The directory of a store that counts the syncs of its database file.
struct Counting {
    vfs: OsVfs,
    syncs: Arc<AtomicU64>,
}

/// A file of `Counting`.
#[derive(Debug)]
struct CountedFile {
    file: FileBackend,
    /// The counter of the database file, `None` for another file.
    syncs: Option<Arc<AtomicU64>>,
}

impl Counting {
    fn counted(&self, name: &str, file: FileBackend) -> CountedFile {
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

    fn close(&self) -> io::Result<()> {
        self.file.close()
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
fn a_conversion_syncs_once_per_batch() {
    const COUNT: u64 = 10_000;
    let dir = fresh_dir("count");
    let (data_fs, blob_dir, tids) = write_data_fs(&dir, COUNT, &[]);
    let syncs = Arc::new(AtomicU64::new(0));
    let vfs = Counting {
        vfs: OsVfs::new(&dir.join("target")).unwrap(),
        syncs: syncs.clone(),
    };
    let store = Store::open_in(vfs, SystemClock).unwrap();

    let before = syncs.load(Ordering::SeqCst);
    let report = from_datafs(&data_fs, Some(&blob_dir), &store).unwrap();
    let taken = syncs.load(Ordering::SeqCst) - before;

    assert!(report.problems.is_empty(), "{report:?}");
    assert_eq!((report.transactions, report.copied), (COUNT, COUNT));
    assert_eq!(store.last_transaction(), *tids.last().unwrap());
    // One sync for each batch, and a few more for the end.
    assert!(
        taken <= COUNT / BATCH + 5,
        "{COUNT} transactions took {taken} syncs of the database file"
    );
}

/// Return the blob files of the store in `vfs`.
fn blob_files(vfs: &MemVfs) -> Vec<String> {
    let mut names = vfs.names();
    names.retain(|name| name.starts_with("blobs/") && name.ends_with(".blob"));
    names
}

/// Return the directory of the blob files of `oid` in a store.
fn store_oid_dir(oid: Oid) -> String {
    let mut dir = String::from("blobs");
    for byte in oid.0.to_be_bytes() {
        dir.push_str(&format!("/0x{byte:02x}"));
    }
    dir
}

#[test]
fn a_crash_loses_only_the_open_batch_and_the_conversion_resumes() {
    const COUNT: u64 = 3_000;
    // A blob in the third batch, and one later in the same batch, where
    // the process dies.
    const FIRST_BLOB: u64 = 2_100;
    const KILLED_AT: u64 = 2_500;
    let dir = fresh_dir("crash");
    let (data_fs, blob_dir, tids) = write_data_fs(&dir, COUNT, &[FIRST_BLOB, KILLED_AT]);

    // The process dies at the first sync of the directory of the second
    // blob: the converter then restores the transaction `KILLED_AT`.
    let vfs = MemVfs::new();
    let store = Store::open_vfs(vfs.clone()).unwrap();
    vfs.kill_at_dir_sync(&store_oid_dir(Oid(KILLED_AT + 1)));
    assert!(from_datafs(&data_fs, Some(&blob_dir), &store).is_err());
    // A killed process runs no destructor.
    std::mem::forget(store);
    vfs.restart();

    // A power loss keeps what was synced.
    let image = vfs.crash_now(|_| 0);
    let store = Store::open_vfs(image.clone()).unwrap();
    let last = store.last_transaction();
    let durable = tids
        .iter()
        .position(|tid| *tid == last)
        .unwrap_or_else(|| panic!("the last tid {last:?} is a tid of the file"));
    let durable = durable as u64;
    // The batches before the open one are durable. The restores of the
    // open batch are lost.
    assert!(
        durable + 1 >= 2 * BATCH,
        "the store ends at the transaction {durable}"
    );
    assert!(
        durable < KILLED_AT - 1,
        "the restores of the open batch are lost: the store ends at the transaction {durable}"
    );
    // The blob file of a lost restore is gone. Nothing stays in the
    // temporary directory of the blobs.
    let kept = usize::from(durable >= FIRST_BLOB);
    assert_eq!(blob_files(&image).len(), kept, "{:?}", image.names());
    let mut leftovers = image.names();
    leftovers.retain(|name| name.starts_with("blobs/tmp/"));
    assert_eq!(leftovers, Vec::<String>::new());

    // The next conversion resumes after the last durable tid and gives the
    // same store as a conversion in one run: the verification compares
    // every transaction of the file with the store.
    let report = from_datafs(&data_fs, Some(&blob_dir), &store).unwrap();
    assert!(report.problems.is_empty(), "{report:?}");
    assert_eq!(report.transactions, COUNT);
    assert_eq!(report.copied, COUNT - durable - 1);
    assert_eq!(store.last_transaction(), *tids.last().unwrap());
    for i in [FIRST_BLOB, KILLED_AT] {
        let oid = Oid(i + 1);
        let mut bytes = Vec::new();
        store
            .open_blob(oid, tids[i as usize])
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        assert_eq!(bytes, blob_bytes(oid));
    }
    assert_eq!(blob_files(&image).len(), 2);
}
