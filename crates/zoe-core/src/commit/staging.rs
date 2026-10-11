//! The staging buffer of a transaction.
//!
//! A transaction keeps the pickles that it stages in memory, up to
//! `staging-memory` bytes. A pickle that does not fit goes to the spill
//! file of the transaction, under `staging/` in the store directory. An
//! index in memory keeps the serial of each staged oid and the place of
//! its pickle: in memory, or an offset and a length in the spill file.
//!
//! The spill file holds the spilled pickles one after the other, with
//! nothing between them. The index keeps the xxh3-64 hash of each spilled
//! pickle, and each read checks it. So the bytes that a read gives are
//! the bytes staged.
//!
//! A spill file is never part of the durable store. Nothing syncs it or
//! `staging/`: the transaction reads the file in the same process, and a
//! crash ends the transaction. A transaction removes its spill file when it
//! drops, so finish and abort remove it too. Opening a store removes the
//! spill files that a crash left.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::io;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use redb::StorageBackend;
use xxhash_rust::xxh3::xxh3_64;

use crate::error::{Result, ZoeError};
use crate::types::{Oid, Tid};
use crate::vfs::Vfs;

/// The directory of the spill files.
pub(crate) const STAGING_DIR: &str = "staging";

/// The operations of a `Vfs` that the staging area uses, in a form that a
/// trait object can hold. Every `Vfs` has them.
pub(crate) trait StagingDir: Send + Sync {
    fn exists(&self, name: &str) -> io::Result<bool>;
    fn create_dir(&self, name: &str) -> io::Result<()>;
    fn list_files(&self, dir: &str) -> io::Result<Vec<String>>;
    fn remove(&self, name: &str) -> io::Result<()>;
    fn open(&self, name: &str) -> io::Result<Box<dyn StorageBackend>>;
}

impl<V: Vfs> StagingDir for V {
    fn exists(&self, name: &str) -> io::Result<bool> {
        Vfs::exists(self, name)
    }

    fn create_dir(&self, name: &str) -> io::Result<()> {
        Vfs::create_dir(self, name)
    }

    fn list_files(&self, dir: &str) -> io::Result<Vec<String>> {
        Vfs::list_files(self, dir)
    }

    fn remove(&self, name: &str) -> io::Result<()> {
        Vfs::remove(self, name)
    }

    fn open(&self, name: &str) -> io::Result<Box<dyn StorageBackend>> {
        Ok(Box::new(Vfs::open(self, name)?))
    }
}

/// The staging area of a store: the limit of each transaction and the
/// directory of the spill files.
pub(crate) struct StagingArea {
    /// The store directory. The blob store shares it.
    dir: Box<dyn StagingDir>,
    /// The most bytes of staged pickles that a transaction keeps in
    /// memory.
    memory: usize,
    /// True when the store is open for reads only. Then the area writes
    /// nothing.
    read_only: bool,
    /// Tells if `staging/` exists.
    ready: Mutex<bool>,
    /// The number of the next spill file.
    next: AtomicU64,
}

impl StagingArea {
    /// Return the staging area of the store directory `dir`. Each
    /// transaction keeps at most `memory` bytes of pickles in memory. The
    /// area of a read-only store writes nothing. This reads nothing:
    /// `open` checks the directory.
    pub(crate) fn new(dir: Box<dyn StagingDir>, memory: usize, read_only: bool) -> Self {
        Self {
            dir,
            memory,
            read_only,
            ready: Mutex::new(false),
            next: AtomicU64::new(0),
        }
    }

    /// Remove the spill files that a crash left, when the store opens.
    /// Only regular files are removed, and other entries stay. The area of
    /// a read-only store removes nothing.
    ///
    /// Nothing syncs the removals. If a crash undoes one, the next open
    /// removes the file again.
    pub(crate) fn open(&self) -> Result<()> {
        if self.read_only {
            return Ok(());
        }
        let mut ready = self.lock_ready();
        if self.dir.exists(STAGING_DIR)? {
            for file in self.dir.list_files(STAGING_DIR)? {
                self.dir.remove(&format!("{STAGING_DIR}/{file}"))?;
            }
            *ready = true;
        }
        Ok(())
    }

    fn lock_ready(&self) -> std::sync::MutexGuard<'_, bool> {
        self.ready.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Create a new, empty spill file. Create `staging/` first when it
    /// does not exist. A read-only store fails with `ReadOnly`.
    fn create(&self) -> Result<Spill> {
        if self.read_only {
            return Err(ZoeError::ReadOnly);
        }
        {
            let mut ready = self.lock_ready();
            if !*ready {
                if !self.dir.exists(STAGING_DIR)? {
                    self.dir.create_dir(STAGING_DIR)?;
                }
                *ready = true;
            }
        }
        // The number is new in this store, and the open of a writer
        // removed the files of earlier processes. So the file is new and
        // empty.
        let n = self.next.fetch_add(1, Ordering::Relaxed);
        let name = format!("{STAGING_DIR}/{n}.spill");
        let file = self.dir.open(&name)?;
        Ok(Spill { name, file, len: 0 })
    }
}

/// The spill file of a transaction. `Staging::remove_spill` removes it.
struct Spill {
    name: String,
    file: Box<dyn StorageBackend>,
    /// The number of bytes in the file.
    len: u64,
}

impl Spill {
    /// Write `data` at the end of the file and return its offset.
    ///
    /// Nothing syncs the file, because it is never durable data. The same
    /// process reads it back, and a crash ends the transaction.
    fn append(&mut self, data: &[u8]) -> io::Result<u64> {
        let offset = self.len;
        self.file.write(offset, data)?;
        self.len += data.len() as u64;
        Ok(offset)
    }
}

/// The place of a staged pickle.
enum Pickle {
    /// In memory.
    Memory(Vec<u8>),
    /// In the spill file, at `offset`, with `len` bytes whose xxh3-64 hash
    /// is `hash`.
    Spilled { offset: u64, len: usize, hash: u64 },
}

/// A record waiting for the commit.
pub(crate) struct Staged {
    /// The serial that the client read before it changed the object.
    pub(crate) serial: Tid,
    /// True when `restore` staged the record. A restored record is a copy,
    /// so a conflict on it cannot be resolved.
    pub(crate) restored: bool,
    pickle: Pickle,
}

/// The staged records of one transaction.
///
/// The buffer does not hold its staging area: each call that needs it
/// takes it. The transaction calls `remove_spill` when it drops.
#[derive(Default)]
pub(crate) struct Staging {
    records: BTreeMap<Oid, Staged>,
    /// The bytes of the pickles in memory.
    in_memory: usize,
    /// The spill file. It exists after the first pickle that does not fit
    /// in memory.
    spill: Option<Spill>,
}

impl Staging {
    /// Stage `data` for `oid` with the serial `serial`, in place of an
    /// earlier record of `oid`. `restored` tells if `restore` staged it.
    ///
    /// The pickle stays in memory if it fits in the limit of `area`. Else
    /// it goes to the spill file, in `area`. The bytes of an earlier
    /// spilled pickle of `oid` stay in the file, unused. If the call
    /// fails, the earlier record stays.
    pub(crate) fn put(
        &mut self,
        area: &StagingArea,
        oid: Oid,
        serial: Tid,
        data: &[u8],
        restored: bool,
    ) -> Result<()> {
        let replaced = match self.records.get(&oid) {
            Some(Staged {
                pickle: Pickle::Memory(bytes),
                ..
            }) => bytes.len(),
            _ => 0,
        };
        let others = self.in_memory - replaced;
        let pickle = if others.saturating_add(data.len()) <= area.memory {
            self.in_memory = others + data.len();
            Pickle::Memory(data.to_vec())
        } else {
            let pickle = self.spill(area, data)?;
            self.in_memory = others;
            pickle
        };
        let staged = Staged {
            serial,
            restored,
            pickle,
        };
        self.records.insert(oid, staged);
        Ok(())
    }

    /// Write `data` to the spill file, and create the file in `area` first
    /// when the transaction has none.
    fn spill(&mut self, area: &StagingArea, data: &[u8]) -> Result<Pickle> {
        let spill = match &mut self.spill {
            Some(spill) => spill,
            empty => empty.insert(area.create()?),
        };
        let offset = spill.append(data)?;
        Ok(Pickle::Spilled {
            offset,
            len: data.len(),
            hash: xxh3_64(data),
        })
    }

    /// Return the bytes of the pickles in memory.
    pub(crate) fn in_memory(&self) -> usize {
        self.in_memory
    }

    /// Return the number of staged oids.
    pub(crate) fn len(&self) -> usize {
        self.records.len()
    }

    /// Return the greatest staged oid.
    pub(crate) fn last_oid(&self) -> Option<Oid> {
        self.records.keys().next_back().copied()
    }

    /// Return each staged oid and its record, in ascending oid order. This
    /// reads no pickle.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (Oid, &Staged)> + '_ {
        self.records.iter().map(|(&oid, staged)| (oid, staged))
    }

    /// Return the serial and the pickle of `oid`, or `None` when `oid` is
    /// not staged. A spilled pickle is read from the spill file.
    pub(crate) fn get(&self, oid: Oid) -> Result<Option<(Tid, Cow<'_, [u8]>)>> {
        match self.records.get(&oid) {
            Some(staged) => Ok(Some((staged.serial, self.read(&staged.pickle)?))),
            None => Ok(None),
        }
    }

    /// Return each staged oid with its serial and its pickle, in ascending
    /// oid order. A spilled pickle is read when the iterator reaches it.
    pub(crate) fn records(&self) -> impl Iterator<Item = Result<(Oid, Tid, Cow<'_, [u8]>)>> + '_ {
        self.records
            .iter()
            .map(|(&oid, staged)| Ok((oid, staged.serial, self.read(&staged.pickle)?)))
    }

    /// Return the bytes of `pickle`. A spilled pickle whose bytes do not
    /// match its hash fails with `InvalidData`.
    fn read<'a>(&'a self, pickle: &'a Pickle) -> io::Result<Cow<'a, [u8]>> {
        let (offset, len, hash) = match pickle {
            Pickle::Memory(bytes) => return Ok(Cow::Borrowed(bytes)),
            Pickle::Spilled { offset, len, hash } => (*offset, *len, *hash),
        };
        let spill = self
            .spill
            .as_ref()
            .expect("a spilled pickle has a spill file");
        let mut bytes = vec![0; len];
        spill.file.read(offset, &mut bytes)?;
        if xxh3_64(&bytes) != hash {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("a spilled pickle in {} does not match its hash", spill.name),
            ));
        }
        Ok(Cow::Owned(bytes))
    }

    /// Remove the spill file from `area`, if the transaction has one. A
    /// failed removal leaves the file to the next open.
    pub(crate) fn remove_spill(&mut self, area: &StagingArea) {
        if let Some(spill) = self.spill.take() {
            drop(spill.file);
            let _ = area.dir.remove(&spill.name);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::SystemClock;
    use crate::commit::Txn;
    use crate::store::{Options, Store};
    use crate::vfs::MemVfs;

    /// Return a staging area in a new filesystem. Each transaction keeps
    /// `memory` bytes of pickles in memory.
    fn area(memory: usize) -> (MemVfs, StagingArea) {
        let vfs = MemVfs::new();
        let area = StagingArea::new(Box::new(vfs.clone()), memory, false);
        (vfs, area)
    }

    /// Open the store in `vfs`. Each transaction keeps `memory` bytes of
    /// pickles in memory.
    fn open(vfs: &MemVfs, memory: usize) -> Store {
        let options = Options {
            staging_memory: memory,
            ..Options::default()
        };
        Store::open_full(vfs.clone(), SystemClock, options).unwrap()
    }

    /// Return the names of the files in `staging/`.
    fn spill_files(vfs: &MemVfs) -> Vec<String> {
        let prefix = format!("{STAGING_DIR}/");
        let names = vfs.names().into_iter();
        names.filter(|name| name.starts_with(&prefix)).collect()
    }

    fn pickle(staging: &Staging, oid: Oid) -> Vec<u8> {
        staging.get(oid).unwrap().unwrap().1.into_owned()
    }

    #[test]
    fn pickles_past_the_limit_go_to_the_spill_file() {
        let (vfs, area) = area(8);
        let mut staging = Staging::default();
        staging.put(&area, Oid(1), Tid(1), b"aaaa", false).unwrap();
        staging.put(&area, Oid(2), Tid(2), b"bbbb", false).unwrap();
        assert!(!Vfs::exists(&vfs, STAGING_DIR).unwrap());
        staging.put(&area, Oid(4), Tid(4), b"cc", false).unwrap();
        // A pickle larger than the limit goes to the file too.
        staging
            .put(&area, Oid(3), Tid(3), b"dddddddddd", false)
            .unwrap();
        assert_eq!(staging.in_memory(), 8);
        assert_eq!(spill_files(&vfs), ["staging/0.spill"]);
        assert_eq!(vfs.file("staging/0.spill").len().unwrap(), 12);

        let records: Vec<_> = staging
            .records()
            .map(|record| {
                let (oid, serial, data) = record.unwrap();
                (oid, serial, data.into_owned())
            })
            .collect();
        assert_eq!(
            records,
            [
                (Oid(1), Tid(1), b"aaaa".to_vec()),
                (Oid(2), Tid(2), b"bbbb".to_vec()),
                (Oid(3), Tid(3), b"dddddddddd".to_vec()),
                (Oid(4), Tid(4), b"cc".to_vec()),
            ]
        );
        assert!(staging.get(Oid(5)).unwrap().is_none());
        assert_eq!((staging.len(), staging.last_oid()), (4, Some(Oid(4))));
    }

    #[test]
    fn a_later_put_of_an_oid_replaces_its_pickle() {
        let (_vfs, area) = area(4);
        let mut staging = Staging::default();
        staging.put(&area, Oid(1), Tid(1), b"aaa", false).unwrap();
        // The new pickle takes the place of the old one in memory.
        staging.put(&area, Oid(1), Tid(2), b"bbbb", false).unwrap();
        assert_eq!(staging.in_memory(), 4);
        staging.put(&area, Oid(1), Tid(3), b"ccccc", false).unwrap();
        assert_eq!(staging.in_memory(), 0);
        assert_eq!(pickle(&staging, Oid(1)), b"ccccc");
        staging.put(&area, Oid(1), Tid(4), b"d", false).unwrap();
        assert_eq!(staging.in_memory(), 1);
        assert_eq!(staging.get(Oid(1)).unwrap().unwrap().0, Tid(4));
        assert_eq!(pickle(&staging, Oid(1)), b"d");
        assert_eq!(staging.len(), 1);
    }

    #[test]
    fn each_buffer_has_its_own_spill_file() {
        let (vfs, area) = area(0);
        let mut first = Staging::default();
        let mut second = Staging::default();
        first.put(&area, Oid(1), Tid::ZERO, b"a", false).unwrap();
        second.put(&area, Oid(1), Tid::ZERO, b"b", false).unwrap();
        assert_eq!(spill_files(&vfs), ["staging/0.spill", "staging/1.spill"]);
        assert_eq!(pickle(&first, Oid(1)), b"a");
        assert_eq!(pickle(&second, Oid(1)), b"b");

        first.remove_spill(&area);
        assert_eq!(spill_files(&vfs), ["staging/1.spill"]);
        second.remove_spill(&area);
        second.remove_spill(&area);
        assert!(spill_files(&vfs).is_empty());
        assert!(Vfs::exists(&vfs, STAGING_DIR).unwrap());
    }

    #[test]
    fn an_empty_pickle_never_spills() {
        let (vfs, area) = area(0);
        let mut staging = Staging::default();
        staging.put(&area, Oid(1), Tid::ZERO, b"", false).unwrap();
        assert!(staging.get(Oid(1)).unwrap().unwrap().1.is_empty());
        assert!(!Vfs::exists(&vfs, STAGING_DIR).unwrap());
    }

    #[test]
    fn a_damaged_spill_file_fails_the_read() {
        let (vfs, area) = area(0);
        let mut staging = Staging::default();
        staging
            .put(&area, Oid(1), Tid::ZERO, b"abc", false)
            .unwrap();
        staging
            .put(&area, Oid(2), Tid::ZERO, b"def", false)
            .unwrap();
        let file = vfs.file("staging/0.spill");
        file.write(4, b"x").unwrap();
        assert_eq!(pickle(&staging, Oid(1)), b"abc");
        let error = staging.get(Oid(2)).unwrap_err();
        assert!(
            matches!(&error, ZoeError::Io(e) if e.kind() == io::ErrorKind::InvalidData),
            "{error:?}"
        );

        file.set_len(4).unwrap();
        assert!(staging.get(Oid(2)).is_err());
        assert!(staging.records().any(|record| record.is_err()));
    }

    #[test]
    fn a_store_that_never_spills_has_no_staging_directory() {
        let vfs = MemVfs::new();
        let store = open(&vfs, 1);
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.store(Oid(1), Tid::ZERO, b"a").unwrap();
        txn.vote().unwrap();
        txn.finish().unwrap();
        drop(store);
        open(&vfs, 1);
        assert!(!Vfs::exists(&vfs, STAGING_DIR).unwrap());
    }

    #[test]
    fn opening_removes_the_spill_files_of_a_dead_process() {
        let vfs = MemVfs::new();
        let store = open(&vfs, 0);
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.store(Oid(1), Tid::ZERO, b"lost").unwrap();
        // A dead process runs no destructor.
        std::mem::forget(txn);
        std::mem::forget(store);
        Vfs::create_dir(&vfs, "staging/stray").unwrap();
        assert_eq!(spill_files(&vfs), ["staging/0.spill"]);

        let store = open(&vfs, 0);
        assert!(spill_files(&vfs).is_empty());
        assert!(Vfs::exists(&vfs, "staging/stray").unwrap());

        // The next spill file takes the first number again.
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.store(Oid(1), Tid::ZERO, b"kept").unwrap();
        assert_eq!(spill_files(&vfs), ["staging/0.spill"]);
        txn.vote().unwrap();
        let tid = txn.finish().unwrap();
        assert_eq!(store.load(Oid(1)).unwrap(), (b"kept".to_vec(), tid));
        assert!(spill_files(&vfs).is_empty());
    }

    /// Return a new, empty directory for a store in the temporary
    /// directory of the system.
    fn temp_dir(name: &str) -> std::path::PathBuf {
        let name = format!("zoe-staging-{name}-{}", std::process::id());
        let dir = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// A read-only store writes nothing in `staging/`. Its open keeps the
    /// spill files, and a pickle that would spill fails with `ReadOnly`.
    /// The next writer removes the spill files.
    #[test]
    fn a_read_only_store_writes_nothing_in_staging() {
        let dir = temp_dir("read-only");
        drop(Store::open(&dir).unwrap());
        let spill = dir.join(STAGING_DIR).join("0.spill");
        std::fs::create_dir(dir.join(STAGING_DIR)).unwrap();
        std::fs::write(&spill, b"left").unwrap();

        let store = Store::open_read_only(&dir).unwrap();
        let mut txn = Txn::begin(&store, b"", b"", b"");
        // A pickle in memory stages, and the vote refuses the transaction.
        txn.store(Oid(1), Tid::ZERO, b"small").unwrap();
        let large = vec![1; Options::default().staging_memory];
        let error = txn.store(Oid(2), Tid::ZERO, &large);
        assert!(matches!(error, Err(ZoeError::ReadOnly)), "{error:?}");
        assert!(matches!(txn.vote(), Err(ZoeError::ReadOnly)));
        drop(txn);
        drop(store);
        let names: Vec<_> = std::fs::read_dir(dir.join(STAGING_DIR))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["0.spill"]);
        assert_eq!(std::fs::read(&spill).unwrap(), b"left");

        drop(Store::open(&dir).unwrap());
        assert!(!spill.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The crash simulation covers the spill files. Each state that a
    /// crash leaves opens with no spill file, and a commit of spilled
    /// records is there whole or not at all.
    #[test]
    fn a_crash_leaves_no_spill_file_and_no_part_of_a_commit() {
        let vfs = MemVfs::new();
        {
            // A first spill creates `staging/`. The record starts after it,
            // so it holds fewer pending operations and fewer images.
            let store = open(&vfs, 0);
            let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
            txn.store(Oid(9), Tid::ZERO, b"x").unwrap();
        }
        let vfs = vfs.settled();
        let store = open(&vfs, 2);
        // The first pickle stays in memory, and the second one spills.
        let oids = [Oid(1), Oid(2)];
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        for oid in oids {
            txn.store(oid, Tid::ZERO, &[oid.0 as u8; 2]).unwrap();
        }
        assert_eq!(txn.staged_in_memory(), 2);
        assert_eq!(spill_files(&vfs), ["staging/0.spill"]);
        txn.vote().unwrap();
        let tid = txn.finish().unwrap();
        vfs.mark("acknowledged");
        drop(store);

        let images = vfs.crash_images();
        let with_spill = images.iter().filter(|image| {
            let names = image.vfs().names();
            names.iter().any(|name| name.starts_with("staging/"))
        });
        assert!(with_spill.count() > 0);
        for image in &images {
            let image_vfs = image.vfs();
            let store = Store::open_vfs(image_vfs.clone())
                .unwrap_or_else(|e| panic!("{image:?} does not open: {e}"));
            assert!(spill_files(&image_vfs).is_empty(), "{image:?}");
            let found: Vec<_> = oids.iter().map(|&oid| store.load(oid).ok()).collect();
            if found[0].is_some() {
                for (oid, found) in oids.iter().zip(found) {
                    assert_eq!(found, Some((vec![oid.0 as u8; 2], tid)), "{image:?}");
                }
            } else {
                assert!(found.iter().all(Option::is_none), "{image:?}");
                assert!(!image.is_after("acknowledged"), "{image:?} lost a commit");
            }
        }
    }
}
