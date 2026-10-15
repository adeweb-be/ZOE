//! The layout `main`: the store of zoe-core as it is, through a Vfs whose
//! database file counts its writes and syncs.
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use zoe_core::vfs::{DirLock, OsVfs, Vfs, WriteFile};
use zoe_core::{Compression, Oid, Options, Store, SystemClock, Tid};

use crate::frame::record;
use crate::io::{CountedFile, Io};

const DATABASE: &str = "zoe.redb";

pub struct CountedVfs {
    vfs: OsVfs,
    io: Arc<Io>,
}

impl CountedVfs {
    fn counted(&self, name: &str, file: redb::backends::FileBackend) -> CountedFile {
        let io = (name == DATABASE).then(|| self.io.clone());
        CountedFile { file, io }
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

pub struct Baseline {
    pub store: Store,
    pub io: Arc<Io>,
}

impl Baseline {
    pub fn open(dir: &Path, compress: bool) -> Baseline {
        let io = Arc::new(Io::default());
        let vfs = CountedVfs {
            vfs: OsVfs::new(dir).unwrap(),
            io: io.clone(),
        };
        let options = Options {
            compression: if compress {
                Compression::default()
            } else {
                Compression::None
            },
            ..Options::default()
        };
        let store = Store::open_full(vfs, SystemClock, options).unwrap();
        Baseline { store, io }
    }

    pub fn new_oids(&self, n: usize) -> Vec<u64> {
        (0..n).map(|_| self.store.new_oid().unwrap().0).collect()
    }

    /// Commit `oids` with their serials in `version`. Return the time of
    /// the finish and the new tid.
    pub fn commit(&self, oids: &[(u64, u64)], version: u64, size: usize) -> (Duration, u64) {
        let mut txn = self.store.tpc_begin(b"user", b"walproto", b"").unwrap();
        for &(oid, serial) in oids {
            txn.store(Oid(oid), Tid(serial), &record(oid, version, size))
                .unwrap();
        }
        txn.vote().unwrap();
        let start = Instant::now();
        let tid = txn.finish().unwrap();
        (start.elapsed(), tid.0)
    }

    pub fn load(&self, oid: u64, tid: u64) -> usize {
        self.store
            .load_before(Oid(oid), Tid(tid))
            .unwrap()
            .unwrap()
            .data
            .len()
    }
}
