//! The directory of a store, and the simulated filesystem of the crash
//! tests.
//!
//! The core reaches the files of a store only through the `Vfs` trait.
//! `OsVfs` is a directory of the operating system. `MemVfs` keeps its
//! files in memory and records every write, length change, sync and
//! directory change. `MemVfs::crash_images` replays that record and returns
//! the states that a crash can leave on disk.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::error::{Result, ZoeError};

/// The directory of a store, as the core sees it.
pub trait Vfs: Send + Sync + 'static {
    /// An open file. redb stores its database in one.
    type File: redb::StorageBackend;

    /// Tell if the file `name` exists.
    fn exists(&self, name: &str) -> io::Result<bool>;

    /// Open the file `name`. Create it empty when it does not exist.
    fn open(&self, name: &str) -> io::Result<Self::File>;

    /// Rename the file `from` to `to`, and replace `to` if it exists.
    fn rename(&self, from: &str, to: &str) -> io::Result<()>;

    /// Remove the file `name`.
    fn remove(&self, name: &str) -> io::Result<()>;

    /// Make the creations, renames and removals in the directory durable.
    fn sync_dir(&self) -> io::Result<()>;
}

/// A directory of the operating system.
///
/// It holds an exclusive lock on the file `lock` in the directory while it
/// exists, so that only one process writes the store.
#[derive(Debug)]
pub struct OsVfs {
    dir: PathBuf,
    _lock: File,
}

impl OsVfs {
    /// Open the directory `dir`, create it if necessary, and take the lock.
    pub fn new(dir: &Path) -> Result<Self> {
        fs::create_dir_all(dir)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join("lock"))?;
        match lock.try_lock() {
            Ok(()) => Ok(Self {
                dir: dir.to_owned(),
                _lock: lock,
            }),
            Err(TryLockError::WouldBlock) => Err(ZoeError::Locked),
            Err(TryLockError::Error(e)) => Err(e.into()),
        }
    }
}

impl Vfs for OsVfs {
    type File = redb::backends::FileBackend;

    fn exists(&self, name: &str) -> io::Result<bool> {
        fs::exists(self.dir.join(name))
    }

    fn open(&self, name: &str) -> io::Result<Self::File> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.dir.join(name))?;
        redb::backends::FileBackend::new(file).map_err(io::Error::other)
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        fs::rename(self.dir.join(from), self.dir.join(to))
    }

    fn remove(&self, name: &str) -> io::Result<()> {
        fs::remove_file(self.dir.join(name))
    }

    fn sync_dir(&self) -> io::Result<()> {
        File::open(&self.dir)?.sync_all()
    }
}

/// One recorded operation.
#[derive(Clone, Debug)]
enum Op {
    Create {
        file: String,
    },
    Write {
        file: String,
        offset: u64,
        data: Vec<u8>,
    },
    SetLen {
        file: String,
        len: u64,
    },
    Sync {
        file: String,
    },
    Rename {
        from: String,
        to: String,
    },
    Remove {
        file: String,
    },
    SyncDir,
    Mark {
        name: String,
    },
}

#[derive(Debug, Default)]
struct State {
    /// The files when the record starts.
    initial: BTreeMap<String, Vec<u8>>,
    /// The files as the program sees them.
    files: BTreeMap<String, Vec<u8>>,
    /// Every operation since the record started, in order.
    log: Vec<Op>,
}

/// An in-memory filesystem that records every operation.
///
/// Clones share the same files and the same record. The files that exist
/// when the record starts are durable.
#[derive(Clone, Debug, Default)]
pub struct MemVfs {
    state: Arc<Mutex<State>>,
}

impl MemVfs {
    /// Return an empty filesystem.
    pub fn new() -> Self {
        Self::default()
    }

    fn with_files(files: BTreeMap<String, Vec<u8>>) -> Self {
        let state = State {
            initial: files.clone(),
            files,
            log: Vec::new(),
        };
        Self {
            state: Arc::new(Mutex::new(state)),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // A panic in a test thread must not hide the files from the others.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Return the file `name`. Create it empty when it does not exist.
    pub fn file(&self, name: &str) -> MemFile {
        let mut state = self.lock();
        if !state.files.contains_key(name) {
            state.files.insert(name.to_owned(), Vec::new());
            state.log.push(Op::Create {
                file: name.to_owned(),
            });
        }
        MemFile {
            vfs: self.clone(),
            name: name.to_owned(),
        }
    }

    /// Record a named point. `CrashImage::is_after` tells if a crash came
    /// after it.
    pub fn mark(&self, name: &str) {
        self.lock().log.push(Op::Mark {
            name: name.to_owned(),
        });
    }

    /// Return the states that a crash can leave on disk.
    ///
    /// For every crash point (after 0, 1, ... n operations), the synced
    /// operations are on disk. A write or a length change is synced by a
    /// later sync of its file; a creation, a rename or a removal by a later
    /// sync of the directory. The pending operations can be on disk or not,
    /// in any subset: all, none, each prefix, each single one, and the last
    /// write torn in half. A write to a file whose creation is not on disk
    /// is lost.
    pub fn crash_images(&self) -> Vec<CrashImage> {
        let state = self.lock();
        let mut images = Vec::new();
        for point in 0..=state.log.len() {
            let ops = &state.log[..point];
            let (synced, pending) = split_at_syncs(ops);
            let marks: Vec<String> = ops
                .iter()
                .filter_map(|op| match op {
                    Op::Mark { name } => Some(name.clone()),
                    _ => None,
                })
                .collect();
            let mut push = |variant: Variant, chosen: &[usize], torn: Option<usize>| {
                let mut which: Vec<usize> = synced.iter().chain(chosen).copied().collect();
                which.sort_unstable();
                images.push(CrashImage {
                    point,
                    variant,
                    marks: marks.clone(),
                    files: apply(&state.initial, ops, &which, torn),
                });
            };

            push(Variant::SyncedOnly, &[], None);
            if pending.is_empty() {
                continue;
            }
            push(Variant::AllPending, &pending, None);
            for n in 1..pending.len() {
                push(Variant::PendingPrefix(n), &pending[..n], None);
            }
            if pending.len() > 1 {
                for &i in &pending {
                    push(Variant::OnlyPending(i), &[i], None);
                }
            }
            if let Some(&last) = pending.last()
                && matches!(&ops[last], Op::Write { data, .. } if data.len() > 1)
            {
                push(Variant::Torn(last), &pending, Some(last));
            }
        }
        images
    }

    fn exists(&self, name: &str) -> bool {
        self.lock().files.contains_key(name)
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        let mut state = self.lock();
        let bytes = state
            .files
            .remove(from)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, from.to_owned()))?;
        state.files.insert(to.to_owned(), bytes);
        state.log.push(Op::Rename {
            from: from.to_owned(),
            to: to.to_owned(),
        });
        Ok(())
    }

    fn remove(&self, name: &str) -> io::Result<()> {
        let mut state = self.lock();
        state
            .files
            .remove(name)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, name.to_owned()))?;
        state.log.push(Op::Remove {
            file: name.to_owned(),
        });
        Ok(())
    }

    fn sync_dir(&self) {
        self.lock().log.push(Op::SyncDir);
    }
}

impl Vfs for MemVfs {
    type File = MemFile;

    fn exists(&self, name: &str) -> io::Result<bool> {
        Ok(MemVfs::exists(self, name))
    }

    fn open(&self, name: &str) -> io::Result<Self::File> {
        Ok(self.file(name))
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        MemVfs::rename(self, from, to)
    }

    fn remove(&self, name: &str) -> io::Result<()> {
        MemVfs::remove(self, name)
    }

    fn sync_dir(&self) -> io::Result<()> {
        MemVfs::sync_dir(self);
        Ok(())
    }
}

/// Split the indexes of the file and directory operations in `ops` into
/// the synced ones and the pending ones.
fn split_at_syncs(ops: &[Op]) -> (Vec<usize>, Vec<usize>) {
    let mut last_sync: HashMap<&str, usize> = HashMap::new();
    let mut last_dir_sync = None;
    for (i, op) in ops.iter().enumerate() {
        match op {
            Op::Sync { file } => {
                last_sync.insert(file, i);
            }
            Op::SyncDir => last_dir_sync = Some(i),
            _ => {}
        }
    }
    let mut synced = Vec::new();
    let mut pending = Vec::new();
    for (i, op) in ops.iter().enumerate() {
        let is_synced = match op {
            Op::Write { file, .. } | Op::SetLen { file, .. } => {
                last_sync.get(file.as_str()).is_some_and(|&s| i < s)
            }
            Op::Create { .. } | Op::Rename { .. } | Op::Remove { .. } => {
                last_dir_sync.is_some_and(|s| i < s)
            }
            Op::Sync { .. } | Op::SyncDir | Op::Mark { .. } => continue,
        };
        if is_synced {
            synced.push(i);
        } else {
            pending.push(i);
        }
    }
    (synced, pending)
}

/// Apply the operations `which` of `ops` to a copy of `files`, in order.
/// The write at index `torn` lands with only the first half of its bytes.
fn apply(
    files: &BTreeMap<String, Vec<u8>>,
    ops: &[Op],
    which: &[usize],
    torn: Option<usize>,
) -> BTreeMap<String, Vec<u8>> {
    let mut files = files.clone();
    for &i in which {
        match &ops[i] {
            Op::Create { file } => {
                files.entry(file.clone()).or_default();
            }
            Op::Write { file, offset, data } => {
                let data = if torn == Some(i) {
                    &data[..data.len() / 2]
                } else {
                    &data[..]
                };
                if let Some(bytes) = files.get_mut(file) {
                    write_at(bytes, *offset, data);
                }
            }
            Op::SetLen { file, len } => {
                if let Some(bytes) = files.get_mut(file) {
                    bytes.resize(to_usize(*len), 0);
                }
            }
            Op::Rename { from, to } => {
                if let Some(bytes) = files.remove(from) {
                    files.insert(to.clone(), bytes);
                }
            }
            Op::Remove { file } => {
                files.remove(file);
            }
            Op::Sync { .. } | Op::SyncDir | Op::Mark { .. } => {}
        }
    }
    files
}

/// Write `data` at `offset`. Like a real file, a write past the end
/// extends the file with zeros.
fn write_at(bytes: &mut Vec<u8>, offset: u64, data: &[u8]) {
    let start = to_usize(offset);
    let end = start + data.len();
    if bytes.len() < end {
        bytes.resize(end, 0);
    }
    bytes[start..end].copy_from_slice(data);
}

fn to_usize(n: u64) -> usize {
    usize::try_from(n).expect("the file fits in memory")
}

/// One file of a `MemVfs`. It is a redb storage backend.
#[derive(Clone, Debug)]
pub struct MemFile {
    vfs: MemVfs,
    name: String,
}

impl redb::StorageBackend for MemFile {
    fn len(&self) -> io::Result<u64> {
        let state = self.vfs.lock();
        Ok(state.files.get(&self.name).map_or(0, |f| f.len() as u64))
    }

    fn read(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
        let state = self.vfs.lock();
        let empty = Vec::new();
        let bytes = state.files.get(&self.name).unwrap_or(&empty);
        let start = to_usize(offset);
        let end = start + out.len();
        if end > bytes.len() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("read past the end of {}", self.name),
            ));
        }
        out.copy_from_slice(&bytes[start..end]);
        Ok(())
    }

    fn set_len(&self, len: u64) -> io::Result<()> {
        let mut state = self.vfs.lock();
        state
            .files
            .entry(self.name.clone())
            .or_default()
            .resize(to_usize(len), 0);
        state.log.push(Op::SetLen {
            file: self.name.clone(),
            len,
        });
        Ok(())
    }

    fn sync_data(&self) -> io::Result<()> {
        self.vfs.lock().log.push(Op::Sync {
            file: self.name.clone(),
        });
        Ok(())
    }

    fn write(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        let mut state = self.vfs.lock();
        write_at(
            state.files.entry(self.name.clone()).or_default(),
            offset,
            data,
        );
        state.log.push(Op::Write {
            file: self.name.clone(),
            offset,
            data: data.to_vec(),
        });
        Ok(())
    }
}

/// Which pending operations of a crash point reached the disk.
#[derive(Clone, Copy)]
enum Variant {
    SyncedOnly,
    AllPending,
    PendingPrefix(usize),
    OnlyPending(usize),
    Torn(usize),
}

impl fmt::Display for Variant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Variant::SyncedOnly => write!(f, "synced operations only"),
            Variant::AllPending => write!(f, "all pending operations"),
            Variant::PendingPrefix(n) => write!(f, "the first {n} pending operations"),
            Variant::OnlyPending(i) => write!(f, "only pending operation {i}"),
            Variant::Torn(i) => write!(f, "pending operations, operation {i} torn"),
        }
    }
}

/// A state that a crash can leave on disk.
pub struct CrashImage {
    point: usize,
    variant: Variant,
    marks: Vec<String>,
    files: BTreeMap<String, Vec<u8>>,
}

impl CrashImage {
    /// Return a filesystem with the files of this image and an empty
    /// record.
    pub fn vfs(&self) -> MemVfs {
        MemVfs::with_files(self.files.clone())
    }

    /// Tell if the crash came after the mark `name`.
    pub fn is_after(&self, name: &str) -> bool {
        self.marks.iter().any(|m| m == name)
    }
}

impl fmt::Debug for CrashImage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "crash image after operation {} ({})",
            self.point, self.variant
        )
    }
}

#[cfg(test)]
mod tests {
    use redb::{ReadableDatabase, StorageBackend, TableDefinition};

    use super::*;

    fn contents(image: &CrashImage, name: &str) -> Option<Vec<u8>> {
        image.files.get(name).cloned()
    }

    /// Return a filesystem with a durable, empty file `f`.
    fn with_file() -> (MemVfs, MemFile) {
        let vfs = MemVfs::new();
        let file = vfs.file("f");
        vfs.sync_dir();
        (vfs, file)
    }

    #[test]
    fn synced_writes_are_in_every_later_image() {
        let (vfs, file) = with_file();
        file.write(0, b"synced").unwrap();
        file.sync_data().unwrap();
        vfs.mark("synced");
        file.write(6, b"+pending").unwrap();

        let images = vfs.crash_images();
        let after: Vec<_> = images.iter().filter(|i| i.is_after("synced")).collect();
        assert!(
            after
                .iter()
                .all(|i| contents(i, "f").unwrap().starts_with(b"synced"))
        );
        assert!(after.iter().any(|i| contents(i, "f").unwrap() == b"synced"));
        assert!(
            after
                .iter()
                .any(|i| contents(i, "f").unwrap() == b"synced+pending")
        );
    }

    #[test]
    fn a_pending_write_can_land_without_an_earlier_one() {
        let (vfs, file) = with_file();
        file.write(0, b"a").unwrap();
        file.write(1, b"b").unwrap();

        let images = vfs.crash_images();
        assert!(
            images
                .iter()
                .any(|i| contents(i, "f").as_deref() == Some(&b"\0b"[..]))
        );
    }

    #[test]
    fn a_torn_write_keeps_its_first_half() {
        let (vfs, file) = with_file();
        file.write(0, b"abcd").unwrap();

        let images = vfs.crash_images();
        assert!(
            images
                .iter()
                .any(|i| contents(i, "f").as_deref() == Some(&b"ab"[..]))
        );
    }

    #[test]
    fn a_file_without_a_directory_sync_can_vanish() {
        let vfs = MemVfs::new();
        let file = vfs.file("f");
        file.write(0, b"x").unwrap();
        file.sync_data().unwrap();

        let images = vfs.crash_images();
        assert!(images.iter().any(|i| contents(i, "f").is_none()));
        assert!(
            images
                .iter()
                .any(|i| contents(i, "f").as_deref() == Some(b"x"))
        );
    }

    #[test]
    fn a_rename_is_durable_after_a_directory_sync() {
        let (vfs, file) = with_file();
        file.write(0, b"x").unwrap();
        file.sync_data().unwrap();
        vfs.rename("f", "g").unwrap();
        vfs.sync_dir();
        vfs.mark("renamed");

        let images = vfs.crash_images();
        let before: Vec<_> = images.iter().filter(|i| !i.is_after("renamed")).collect();
        assert!(before.iter().any(|i| contents(i, "f").is_some()));
        assert!(before.iter().any(|i| contents(i, "g").is_some()));
        for image in images.iter().filter(|i| i.is_after("renamed")) {
            assert_eq!(contents(image, "f"), None);
            assert_eq!(contents(image, "g").as_deref(), Some(&b"x"[..]));
        }
    }

    #[test]
    fn marks_split_the_images() {
        let (vfs, file) = with_file();
        file.write(0, b"x").unwrap();
        vfs.mark("done");
        file.write(1, b"y").unwrap();

        let images = vfs.crash_images();
        let first_after = images.iter().position(|i| i.is_after("done")).unwrap();
        assert!(images[..first_after].iter().all(|i| !i.is_after("done")));
        assert!(images[first_after..].iter().all(|i| i.is_after("done")));
    }

    #[test]
    fn an_image_starts_a_fresh_record() {
        let (vfs, file) = with_file();
        file.write(0, b"x").unwrap();
        file.sync_data().unwrap();

        let image = vfs.crash_images().pop().unwrap();
        let reopened = image.vfs();
        assert_eq!(reopened.file("f").len().unwrap(), 1);
        assert_eq!(reopened.crash_images().len(), 1);
    }

    /// redb with immediate durability survives every crash image: the
    /// database opens, and a commit that returned is never lost.
    #[test]
    fn redb_survives_every_crash_image() {
        const TABLE: TableDefinition<u64, &[u8]> = TableDefinition::new("simulation");

        // Create the database first and record only the commit. A crash
        // during the creation is a different case: see the next test.
        let created = MemVfs::new();
        drop(
            redb::Builder::new()
                .create_with_backend(created.file("db"))
                .unwrap(),
        );
        let vfs = MemVfs::with_files(created.lock().files.clone());
        {
            let db = redb::Builder::new()
                .create_with_backend(vfs.file("db"))
                .unwrap();
            let txn = db.begin_write().unwrap();
            txn.open_table(TABLE)
                .unwrap()
                .insert(1, &b"value"[..])
                .unwrap();
            txn.commit().unwrap();
            vfs.mark("committed");
        }

        let images = vfs.crash_images();
        assert!(images.len() > 10);
        for image in &images {
            let db = redb::Builder::new()
                .create_with_backend(image.vfs().file("db"))
                .unwrap_or_else(|e| panic!("{image:?} does not open: {e}"));
            let txn = db.begin_read().unwrap();
            let found = match txn.open_table(TABLE) {
                Ok(table) => table.get(1).unwrap().map(|v| v.value().to_vec()),
                Err(redb::TableError::TableDoesNotExist(_)) => None,
                Err(e) => panic!("{image:?}: {e}"),
            };
            match found {
                Some(value) => assert_eq!(value, b"value", "{image:?}"),
                None => assert!(!image.is_after("committed"), "{image:?} lost a commit"),
            }
        }
    }

    /// A crash while redb creates a database can leave a file that redb
    /// refuses to open. So the store must create its database atomically.
    #[test]
    fn a_crash_during_creation_can_leave_an_unreadable_file() {
        let vfs = MemVfs::new();
        drop(
            redb::Builder::new()
                .create_with_backend(vfs.file("db"))
                .unwrap(),
        );

        let refused = vfs.crash_images().iter().any(|image| {
            redb::Builder::new()
                .create_with_backend(image.vfs().file("db"))
                .is_err()
        });
        assert!(refused);
    }
}
