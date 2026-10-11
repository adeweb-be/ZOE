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
        create_dir_durable(dir)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join(LOCK))?;
        Self::locked(dir, lock)
    }

    /// Open the directory `dir` of an existing store to read it, and take
    /// the lock.
    ///
    /// The file `database` must be in `dir`. Otherwise the call fails and
    /// changes nothing. The call opens the lock file read-only. It creates
    /// the lock file only when the store has none.
    pub(crate) fn existing(dir: &Path, database: &str) -> Result<Self> {
        if !dir.join(database).is_file() {
            let message = format!("no store in {}", dir.display());
            return Err(io::Error::new(io::ErrorKind::NotFound, message).into());
        }
        let path = dir.join(LOCK);
        let lock = match File::open(&path) {
            Ok(lock) => lock,
            Err(e) if e.kind() == io::ErrorKind::NotFound => OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)?,
            Err(e) => return Err(e.into()),
        };
        Self::locked(dir, lock)
    }

    /// Take the exclusive lock on the open lock file `lock` of `dir`.
    fn locked(dir: &Path, lock: File) -> Result<Self> {
        match lock.try_lock() {
            Ok(()) => Ok(Self {
                dir: dir.to_owned(),
                _lock: lock,
            }),
            Err(TryLockError::WouldBlock) => Err(ZoeError::Locked),
            Err(TryLockError::Error(e)) => Err(e.into()),
        }
    }

    /// Return the path of the file `name` in the directory.
    pub(crate) fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }
}

/// The name of the lock file in the directory of a store.
pub(crate) const LOCK: &str = "lock";

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

/// Create `dir` and its missing parents, one level at a time.
///
/// Each new directory and its parent are synced, so a power loss cannot
/// remove a directory that holds a durable store. `dir` and its parent are
/// synced also when `dir` exists: a run that stopped before its sync can
/// leave a directory that is not durable.
fn create_dir_durable(dir: &Path) -> io::Result<()> {
    let missing: Vec<&Path> = dir
        .ancestors()
        .take_while(|path| !path.as_os_str().is_empty() && !path.is_dir())
        .collect();
    for path in missing.into_iter().rev() {
        match fs::create_dir(path) {
            Ok(()) => {}
            // Another process can create the directory first.
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists && path.is_dir() => {}
            Err(e) => return Err(e),
        }
        sync_with_parent(path)?;
    }
    sync_with_parent(dir)
}

/// Sync the directory `path`, then its parent.
fn sync_with_parent(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()?;
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    File::open(parent)?.sync_all()
}

/// The most pending operations for which `MemVfs::crash_images` makes an
/// image of every subset. It makes at most 2^10 such images for one crash
/// point.
const MAX_SUBSET_PENDING: usize = 10;

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
    /// A crash point comes before the first operation and after each
    /// operation. At a crash point, the synced operations are on disk. A
    /// write or a length change is synced by a later sync of its file. A
    /// creation, a rename or a removal is synced by a later sync of the
    /// directory. The other operations are pending. The images of a crash
    /// point are these:
    ///
    /// - With at most `MAX_SUBSET_PENDING` pending operations, one image
    ///   for each subset of them, the empty subset and the full one
    ///   included.
    /// - With more pending operations, one image with none of them, one
    ///   with all of them, one for each prefix of 1 to n - 1 of them, and
    ///   one for each single one.
    /// - For each pending write of 2 bytes or more, two images with all
    ///   the pending operations, where only the first half of that write
    ///   lands in one, and only the second half in the other.
    ///
    /// The operations of an image land in their recorded order. A write to
    /// a file whose creation is not on disk is lost.
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
            let mut push = |variant: Variant| {
                let (chosen, torn) = match &variant {
                    Variant::Pending(chosen) => (&chosen[..], None),
                    Variant::Torn(i, half) => (&pending[..], Some((*i, *half))),
                };
                let mut which: Vec<usize> = synced.iter().chain(chosen).copied().collect();
                which.sort_unstable();
                images.push(CrashImage {
                    point,
                    files: apply(&state.initial, ops, &which, torn),
                    variant,
                    marks: marks.clone(),
                });
            };

            if pending.len() <= MAX_SUBSET_PENDING {
                for subset in 0..1u32 << pending.len() {
                    let chosen = (0..pending.len())
                        .filter(|bit| subset & 1 << bit != 0)
                        .map(|bit| pending[bit])
                        .collect();
                    push(Variant::Pending(chosen));
                }
            } else {
                push(Variant::Pending(Vec::new()));
                push(Variant::Pending(pending.clone()));
                for n in 1..pending.len() {
                    push(Variant::Pending(pending[..n].to_vec()));
                }
                for &i in &pending {
                    push(Variant::Pending(vec![i]));
                }
            }
            for &i in &pending {
                if matches!(&ops[i], Op::Write { data, .. } if data.len() > 1) {
                    push(Variant::Torn(i, Half::First));
                    push(Variant::Torn(i, Half::Second));
                }
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
/// Of the write `torn`, only the given half of its bytes lands.
fn apply(
    files: &BTreeMap<String, Vec<u8>>,
    ops: &[Op],
    which: &[usize],
    torn: Option<(usize, Half)>,
) -> BTreeMap<String, Vec<u8>> {
    let mut files = files.clone();
    for &i in which {
        match &ops[i] {
            Op::Create { file } => {
                files.entry(file.clone()).or_default();
            }
            Op::Write { file, offset, data } => {
                let middle = data.len() / 2;
                let (offset, data) = match torn {
                    Some((t, Half::First)) if t == i => (*offset, &data[..middle]),
                    Some((t, Half::Second)) if t == i => (*offset + middle as u64, &data[middle..]),
                    _ => (*offset, &data[..]),
                };
                if let Some(bytes) = files.get_mut(file) {
                    write_at(bytes, offset, data);
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

/// The half of a torn write that lands.
#[derive(Clone, Copy)]
enum Half {
    First,
    Second,
}

/// Which pending operations of a crash point reached the disk.
enum Variant {
    /// These pending operations, each one whole.
    Pending(Vec<usize>),
    /// All the pending operations, but only one half of this write.
    Torn(usize, Half),
}

impl fmt::Display for Variant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Variant::Pending(ops) if ops.is_empty() => write!(f, "synced operations only"),
            Variant::Pending(ops) => write!(f, "pending operations {ops:?}"),
            Variant::Torn(i, Half::First) => {
                write!(f, "all pending operations, first half of operation {i}")
            }
            Variant::Torn(i, Half::Second) => {
                write!(f, "all pending operations, second half of operation {i}")
            }
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
    fn a_store_directory_is_created_with_its_missing_parents() {
        let root = std::env::temp_dir().join(format!("zoe-vfs-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let dir = root.join("a").join("b");
        drop(OsVfs::new(&dir).unwrap());
        assert!(dir.join("lock").is_file());

        fs::write(root.join("file"), b"").unwrap();
        assert!(OsVfs::new(&root.join("file").join("c")).is_err());
        fs::remove_dir_all(&root).unwrap();
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
    fn each_pending_write_can_tear_in_either_half() {
        let (vfs, file) = with_file();
        file.write(0, b"ab").unwrap();
        file.write(2, b"cd").unwrap();

        let images = vfs.crash_images();
        for torn in [&b"a\0cd"[..], b"\0bcd", b"abc", b"ab\0d"] {
            assert!(
                images
                    .iter()
                    .any(|i| contents(i, "f").as_deref() == Some(torn)),
                "{torn:?}"
            );
        }
    }

    /// Return the contents of `f` in the images of the last crash point.
    fn last_point(vfs: &MemVfs) -> Vec<Vec<u8>> {
        let last = vfs.lock().log.len();
        vfs.crash_images()
            .iter()
            .filter(|i| i.point == last)
            .map(|i| contents(i, "f").unwrap())
            .collect()
    }

    #[test]
    fn every_subset_of_few_pending_operations_is_an_image() {
        let (vfs, file) = with_file();
        for (offset, byte) in b"abc".iter().enumerate() {
            file.write(offset as u64, &[*byte]).unwrap();
        }

        let mut found = last_point(&vfs);
        found.sort();
        let mut subsets: Vec<Vec<u8>> = [
            &b""[..],
            b"a",
            b"\0b",
            b"ab",
            b"\0\0c",
            b"a\0c",
            b"\0bc",
            b"abc",
        ]
        .map(<[u8]>::to_vec)
        .into();
        subsets.sort();
        assert_eq!(found, subsets);
    }

    #[test]
    fn many_pending_operations_give_prefixes_and_single_ones() {
        let (vfs, file) = with_file();
        let n = MAX_SUBSET_PENDING + 1;
        for offset in 0..n {
            file.write(offset as u64, b"x").unwrap();
        }

        // None, all, the n - 1 proper prefixes and the n single ones.
        assert_eq!(last_point(&vfs).len(), 2 + (n - 1) + n);
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
