//! The bytes that a commit writes to the database file, as zodbshootout
//! commits: 100 records of 1 KB in each transaction. `update` writes new
//! versions of 100 existing objects, `add` writes 100 new objects.
//!
//! The bytes do not depend on the clock or on the disk, so the test runs
//! in the simulated filesystem. It prints the bytes of each scenario. Run
//! it with `--nocapture` to see them.

use std::io::{self, Read};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use redb::StorageBackend;
use zoe_core::vfs::{DirLock, MemFile, MemVfs, Vfs, WriteFile};
use zoe_core::{Compression, Oid, Options, Store, SystemClock, Tid};

/// The records of a transaction.
const RECORDS: usize = 100;

/// The size of a record.
const SIZE: usize = 1024;

/// The transactions that write the objects that `update` writes again.
const SLOTS: usize = 20;

/// The measured transactions of each scenario.
const TXNS: usize = 100;

/// The bytes that an `update` commit wrote with the layout keyed by
/// `(oid, tid)`, as this test measured them.
const UPDATE_BYTES_BEFORE: u64 = 631_022;

/// The name of the database file in the store directory.
const DATABASE: &str = "zoe.redb";

/// A file of the store. The database file counts the bytes that it writes.
#[derive(Debug)]
struct CountedFile {
    file: MemFile,
    written: Option<Arc<AtomicU64>>,
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
        self.file.sync_data()
    }

    fn write(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        if let Some(written) = &self.written {
            written.fetch_add(data.len() as u64, Ordering::SeqCst);
        }
        self.file.write(offset, data)
    }

    fn close(&self) -> io::Result<()> {
        self.file.close()
    }
}

/// A simulated directory whose database file counts the bytes it writes.
struct CountedVfs {
    vfs: MemVfs,
    written: Arc<AtomicU64>,
}

impl CountedVfs {
    fn counted(&self, name: &str, file: MemFile) -> CountedFile {
        let written = (name == DATABASE).then(|| self.written.clone());
        CountedFile { file, written }
    }
}

impl Vfs for CountedVfs {
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

/// The record of `oid` in its `version`: 1 KB of pseudo-random bytes.
fn record(oid: Oid, version: u64) -> Vec<u8> {
    let mut state = oid.0 ^ (version << 32) ^ 0x9e37_79b9_7f4a_7c15;
    let mut data = Vec::with_capacity(SIZE);
    while data.len() < SIZE {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        data.extend_from_slice(&state.to_le_bytes());
    }
    data.truncate(SIZE);
    data
}

/// Commit one transaction that writes `oids` in `version`. Return the
/// oids with their new serial.
fn commit(store: &Store, oids: &[(Oid, Tid)], version: u64) -> Vec<(Oid, Tid)> {
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    for &(oid, serial) in oids {
        txn.store(oid, serial, &record(oid, version)).unwrap();
    }
    txn.vote().unwrap();
    let tid = txn.finish().unwrap();
    oids.iter().map(|&(oid, _)| (oid, tid)).collect()
}

/// The bytes that each commit of `add` and of `update` writes to the
/// database file, the mean of `TXNS` commits.
struct Bytes {
    add: u64,
    update: u64,
}

fn measure() -> Bytes {
    let written = Arc::new(AtomicU64::new(0));
    let vfs = CountedVfs {
        vfs: MemVfs::new(),
        written: written.clone(),
    };
    let options = Options {
        compression: Compression::None,
        ..Options::default()
    };
    let store = Store::open_full(vfs, SystemClock, options).unwrap();
    let new_oids = |store: &Store| -> Vec<(Oid, Tid)> {
        (0..RECORDS)
            .map(|_| (store.new_oid().unwrap(), Tid::ZERO))
            .collect()
    };
    let mut slots: Vec<_> = (0..SLOTS)
        .map(|_| commit(&store, &new_oids(&store), 0))
        .collect();

    written.store(0, Ordering::SeqCst);
    for i in 0..TXNS {
        slots[i % SLOTS] = commit(&store, &slots[i % SLOTS], i as u64 + 1);
    }
    let update = written.swap(0, Ordering::SeqCst) / TXNS as u64;
    for _ in 0..TXNS {
        commit(&store, &new_oids(&store), 0);
    }
    let add = written.swap(0, Ordering::SeqCst) / TXNS as u64;
    Bytes { add, update }
}

#[test]
fn an_update_commit_writes_about_half_the_bytes_of_the_layout_keyed_by_oid() {
    let bytes = measure();
    println!(
        "bytes written per commit of 100 records of 1 KB: add {}, update {} \
         (update with the layout keyed by (oid, tid): {UPDATE_BYTES_BEFORE})",
        bytes.add, bytes.update
    );
    assert!(
        bytes.update * 100 <= UPDATE_BYTES_BEFORE * 55,
        "an update commit writes {} bytes, the layout keyed by (oid, tid) \
         wrote {UPDATE_BYTES_BEFORE}",
        bytes.update
    );
    // A commit of new objects does not write more than before either.
    assert!(
        bytes.add <= bytes.update,
        "an add commit writes {} bytes, more than an update commit, {}",
        bytes.add,
        bytes.update
    );
}
