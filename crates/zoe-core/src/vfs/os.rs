//! The directory of a store in the filesystem of the operating system.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, BufWriter, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(test)]
use std::sync::{Arc, Barrier};
use std::sync::{Mutex, MutexGuard};

use super::{DirLock, Vfs, WriteFile};
use crate::error::{Result, ZoeError};

/// A directory of the operating system.
///
/// It holds an exclusive lock on the file `lock` in the directory while it
/// exists, so that only one process writes the store.
#[derive(Debug)]
pub struct OsVfs {
    dir: PathBuf,
    /// The lock file of the store. A directory that holds the blob
    /// directory of a store has none.
    _lock: Option<File>,
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

    /// Open the directory `dir` without a lock, and create it if necessary.
    /// It is the directory that holds the blob directory of a store whose
    /// blob files are outside the store directory. The lock of the store
    /// stays in the store directory.
    pub(crate) fn unlocked(dir: &Path) -> Result<Self> {
        create_dir_durable(dir)?;
        Ok(Self {
            dir: dir.to_owned(),
            _lock: None,
        })
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
                _lock: Some(lock),
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
    type File = OsFile;

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
        OsFile::new(file, Mode::Database)
    }

    fn open_log(&self, name: &str) -> io::Result<Self::File> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.dir.join(name))?;
        OsFile::new(file, Mode::Log)
    }

    fn create_new(&self, name: &str) -> io::Result<Self::File> {
        // `create_new` sets `O_EXCL`. With it, the creation fails on any
        // entry of the name, a symbolic link included.
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(self.dir.join(name))?;
        OsFile::new(file, Mode::Spill)
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        fs::rename(self.dir.join(from), self.dir.join(to))
    }

    fn hard_link(&self, from: &str, to: &str) -> io::Result<()> {
        fs::hard_link(self.dir.join(from), self.dir.join(to))
    }

    fn remove(&self, name: &str) -> io::Result<()> {
        fs::remove_file(self.dir.join(name))
    }

    fn sync_dir(&self) -> io::Result<()> {
        self.sync_dir_at("")
    }

    fn sync_dir_at(&self, dir: &str) -> io::Result<()> {
        sync(&File::open(self.dir.join(dir))?)
    }

    fn create_dir(&self, name: &str) -> io::Result<()> {
        fs::create_dir(self.dir.join(name))
    }

    fn list_files(&self, dir: &str) -> io::Result<Vec<String>> {
        list_dir(&self.dir.join(dir), false)
    }

    fn list_entries(&self, dir: &str) -> io::Result<Vec<String>> {
        list_dir(&self.dir.join(dir), true)
    }

    fn create_file(&self, name: &str) -> io::Result<Box<dyn WriteFile>> {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(self.dir.join(name))?;
        Ok(Box::new(OsWriteFile(BufWriter::new(file))))
    }

    fn read_file(&self, name: &str) -> io::Result<Box<dyn Read + Send>> {
        Ok(Box::new(File::open(self.dir.join(name))?))
    }

    fn sync_file(&self, name: &str) -> io::Result<()> {
        let path = self.dir.join(name);
        // O_NOFOLLOW refuses a symbolic link. O_NONBLOCK keeps the open of
        // a pipe from waiting for a writer. A sync needs no write
        // permission.
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&path)?;
        // The type comes from the open file, so no other file can take its
        // place between the check and the sync.
        if !file.metadata()?.is_file() {
            let message = format!("not a regular file: {}", path.display());
            return Err(io::Error::new(io::ErrorKind::InvalidInput, message));
        }
        sync(&file)
    }

    fn set_read_only(&self, name: &str) -> io::Result<()> {
        let path = self.dir.join(name);
        let mut permissions = fs::metadata(&path)?.permissions();
        permissions.set_readonly(true);
        fs::set_permissions(path, permissions)
    }

    fn lock_dir(&self, name: &str) -> io::Result<DirLock> {
        let path = self.dir.join(name);
        // flock on a descriptor of the directory itself: no file is
        // created in it.
        let dir = File::open(&path)?;
        if !dir.metadata()?.is_dir() {
            let message = format!("not a directory: {}", path.display());
            return Err(io::Error::new(io::ErrorKind::InvalidInput, message));
        }
        match dir.try_lock() {
            Ok(()) => Ok(DirLock::new(dir)),
            Err(TryLockError::WouldBlock) => {
                let message = format!("{} is locked", path.display());
                Err(io::Error::new(io::ErrorKind::WouldBlock, message))
            }
            Err(TryLockError::Error(e)) => {
                let message = format!("cannot lock the directory {}: {e}", path.display());
                Err(io::Error::new(e.kind(), message))
            }
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }
}

/// Return the names in the directory `dir`, without the directory part, in
/// order: the names of the regular files, as `Vfs::list_files` gives them,
/// or with `entries` the names of all the entries, as `Vfs::list_entries`
/// gives them.
pub(crate) fn list_dir(dir: &Path, entries: bool) -> io::Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entries {
            names.push(entry.file_name().to_string_lossy().into_owned());
            continue;
        }
        // The file type of an entry does not follow a symbolic link.
        if !entry.file_type()?.is_file() {
            continue;
        }
        // A name that is not UTF-8 is not a name of the core.
        if let Ok(name) = entry.file_name().into_string() {
            names.push(name);
        }
    }
    names.sort();
    Ok(names)
}

/// A file of an `OsVfs` that the core writes.
struct OsWriteFile(BufWriter<File>);

impl Write for OsWriteFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl WriteFile for OsWriteFile {
    fn sync(&mut self) -> io::Result<()> {
        self.0.flush()?;
        sync(self.0.get_ref())
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
    sync(&File::open(path)?)?;
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    sync(&File::open(parent)?)
}

/// The largest growth of a file of an `OsVfs` that gets zeros. redb
/// doubles its file until it reaches 4 GiB, so the growths of a store up to
/// 128 MiB get them.
const ZERO_FILL_MAX: u64 = 64 << 20;

/// The size of one write of zeros.
const ZERO_CHUNK: usize = 1 << 20;

/// The most bytes of one write that `OsFile` makes of adjacent writes.
const MERGED_WRITE_MAX: usize = 1 << 20;

/// The most bytes of writes that an `OsFile` holds. A write beyond it
/// makes them all first.
const PENDING_MAX: usize = 64 << 20;

/// A file of an `OsVfs`: the file backend of redb, with the writes, the
/// sync and the growth below.
///
/// redb writes the pages of a commit one at a time, in the order of its
/// cache, and then syncs the file. A database file holds these writes, and makes
/// them in the order of their offsets before the sync, each run of
/// adjacent writes as one write. A read, a change of the length and the
/// close make them first too, so every call sees every write before it.
/// The sync makes them durable as before: the writes before a sync are
/// durable after it, and the writes after it are not.
#[derive(Debug)]
pub struct OsFile {
    backend: redb::backends::FileBackend,
    /// A second descriptor of the same open file, for the sync.
    file: File,
    /// The writes that the file has not made yet, by offset. They do not
    /// overlap.
    pending: Mutex<Pending>,
    /// True while `pending` holds a write. It turns false only after the
    /// writes are made. A read checks it without the mutex.
    has_pending: AtomicBool,
    /// What the file holds, which tells how it writes and grows.
    mode: Mode,
    /// A test stops a `make` after its first write here: it waits on the
    /// first barrier, then on the second.
    #[cfg(test)]
    pause: Mutex<Option<(Arc<Barrier>, Arc<Barrier>)>>,
}

/// What a file of an `OsVfs` holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// The database file, the file of `Vfs::open`. It holds its writes
    /// until the next sync, read, change of the length or close, and fills
    /// a growth of at most `ZERO_FILL_MAX` bytes with zeros.
    Database,
    /// A segment of the write-ahead log, the file of `Vfs::open_log`. It
    /// makes each write at once, so it holds no copy of an entry, and fills
    /// each growth with zeros.
    Log,
    /// A spill file of `Vfs::create_new`. It makes each write at once and
    /// grows sparse.
    Spill,
}

#[derive(Debug, Default)]
struct Pending {
    writes: BTreeMap<u64, Vec<u8>>,
    bytes: usize,
}

impl OsFile {
    fn new(file: File, mode: Mode) -> io::Result<Self> {
        let clone = file.try_clone()?;
        let backend = redb::backends::FileBackend::new(file).map_err(io::Error::other)?;
        Ok(Self {
            backend,
            file: clone,
            pending: Mutex::new(Pending::default()),
            has_pending: AtomicBool::new(false),
            mode,
            #[cfg(test)]
            pause: Mutex::new(None),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Pending> {
        crate::lock(&self.pending)
    }

    /// Wait on the barriers of a test, once.
    #[cfg(test)]
    fn paused(&self) {
        let pause = self.pause.lock().unwrap().take();
        if let Some((entered, release)) = pause {
            entered.wait();
            release.wait();
        }
    }

    /// Make the pending writes, if any, before a read. A read that finds no
    /// pending write takes no mutex: the flag turns false only after the
    /// writes are made.
    fn make_pending(&self) -> io::Result<()> {
        if self.has_pending.load(Ordering::Acquire) {
            self.make(&mut self.lock())?;
        }
        Ok(())
    }

    /// Make the writes of `pending` in the order of their offsets, each run
    /// of adjacent writes as one write of at most `MERGED_WRITE_MAX` bytes.
    ///
    /// The caller holds the mutex, so no other thread makes them at the
    /// same time. The writes leave `pending` only when all of them are
    /// made. After an error they stay, and the next call makes them again.
    fn make(&self, pending: &mut Pending) -> io::Result<()> {
        let mut run = Vec::new();
        let mut start = 0;
        for (&offset, data) in &pending.writes {
            let next = start + run.len() as u64;
            if !run.is_empty() && (offset != next || run.len() + data.len() > MERGED_WRITE_MAX) {
                redb::StorageBackend::write(&self.backend, start, &run)?;
                #[cfg(test)]
                self.paused();
                run.clear();
            }
            if run.is_empty() {
                start = offset;
            }
            run.extend_from_slice(data);
        }
        if !run.is_empty() {
            redb::StorageBackend::write(&self.backend, start, &run)?;
        }
        pending.writes.clear();
        pending.bytes = 0;
        self.has_pending.store(false, Ordering::Release);
        Ok(())
    }
}

impl redb::StorageBackend for OsFile {
    fn len(&self) -> io::Result<u64> {
        self.make_pending()?;
        self.backend.len()
    }

    fn read(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
        self.make_pending()?;
        self.backend.read(offset, out)
    }

    /// Set the length of the file. A growth of a database file of at most
    /// `ZERO_FILL_MAX` bytes gets zeros in its new bytes, and each growth of
    /// a segment of the log.
    ///
    /// A write into a hole of a sparse file allocates the blocks of the
    /// hole, and the next sync then writes that metadata too. redb grows
    /// its file and then writes its pages into the new space, so each
    /// commit of a young store pays this. Zeros written once give the new
    /// space its blocks. A later commit into it syncs only its data. The
    /// bound keeps the time of one growth short. A larger growth stays
    /// sparse.
    fn set_len(&self, len: u64) -> io::Result<()> {
        // The mutex stays held, so no held write runs past the new end.
        let mut pending = self.lock();
        self.make(&mut pending)?;
        let old = self.backend.len()?;
        self.backend.set_len(len)?;
        let zeros = match self.mode {
            Mode::Database => len.saturating_sub(old) <= ZERO_FILL_MAX,
            Mode::Log => true,
            Mode::Spill => false,
        };
        if len > old && zeros {
            let zeros = vec![0; ZERO_CHUNK];
            let mut at = old;
            while at < len {
                let n = usize::try_from(len - at).map_or(ZERO_CHUNK, |n| n.min(ZERO_CHUNK));
                self.backend.write(at, &zeros[..n])?;
                at += n as u64;
            }
        }
        Ok(())
    }

    fn sync_data(&self) -> io::Result<()> {
        // A segment of the log and a spill file hold no write, so their sync
        // does not wait for a growth that another thread makes.
        if self.mode != Mode::Database {
            return sync_data(&self.file);
        }
        // The mutex makes this sync wait for a `make` that another thread
        // runs: its writes can be writes of the commit that syncs.
        self.make(&mut self.lock())?;
        sync_data(&self.file)
    }

    fn write(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        if self.mode != Mode::Database {
            return self.backend.write(offset, data);
        }
        if data.is_empty() {
            return Ok(());
        }
        let mut pending = self.lock();
        let end = offset + data.len() as u64;
        // The writes do not overlap. So the last write that starts before
        // `end` is the only one to check: an overlap includes it.
        let overlap = pending
            .writes
            .range(..end)
            .next_back()
            .map(|(&at, bytes)| (at, bytes.len()))
            .filter(|&(at, len)| at + len as u64 > offset);
        match overlap {
            // The same place again: the later bytes take the place.
            Some((at, len)) if at == offset && len == data.len() => {}
            // Another overlap: the writes before it go first, so the later
            // bytes win.
            Some(_) => self.make(&mut pending)?,
            None => {}
        }
        if pending.bytes + data.len() > PENDING_MAX {
            self.make(&mut pending)?;
        }
        if let Some(old) = pending.writes.insert(offset, data.to_vec()) {
            pending.bytes -= old.len();
        }
        pending.bytes += data.len();
        self.has_pending.store(true, Ordering::Release);
        Ok(())
    }

    fn close(&self) -> io::Result<()> {
        self.make(&mut self.lock())?;
        self.backend.close()
    }
}

/// Make the data and the metadata of a file durable.
///
/// On Apple, `File::sync_all` and `File::sync_data` also flush the cache of
/// the drive (`F_FULLFSYNC`). There, ZOE calls `fsync(2)`, as FileStorage
/// and SQLite do by default. macOS is a platform for development only
/// (ADR 0038, ADR 0040). On the other systems, this is `File::sync_all`.
fn sync(file: &File) -> io::Result<()> {
    #[cfg(target_vendor = "apple")]
    return fsync(file);
    #[cfg(not(target_vendor = "apple"))]
    file.sync_all()
}

/// Make the data of a file durable, as `sync` does. On the other systems
/// than Apple, this is `File::sync_data`.
fn sync_data(file: &File) -> io::Result<()> {
    #[cfg(target_vendor = "apple")]
    return fsync(file);
    #[cfg(not(target_vendor = "apple"))]
    file.sync_data()
}

#[cfg(target_vendor = "apple")]
fn fsync(file: &File) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: the descriptor belongs to `file`, which stays open for the
    // call. fsync(2) reads no memory of the process.
    loop {
        #[allow(unsafe_code)]
        let done = unsafe { libc::fsync(file.as_raw_fd()) };
        if done == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        // A signal can stop the call before it ends. Then it runs again.
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

#[cfg(test)]
mod tests {
    use redb::StorageBackend;

    use super::*;
    use crate::vfs::MemVfs;

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
    fn the_files_of_an_os_directory_work_as_the_core_needs() {
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir().join(format!("zoe-vfs-files-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let vfs = OsVfs::new(&root).unwrap();
        vfs.create_dir("d").unwrap();
        vfs.sync_dir().unwrap();
        let mut file = vfs.create_file("d/f").unwrap();
        file.write_all(b"abc").unwrap();
        file.sync().unwrap();
        drop(file);
        assert!(vfs.create_file("d/f").is_err());
        vfs.sync_dir_at("d").unwrap();
        vfs.create_dir("d/e").unwrap();
        assert_eq!(vfs.list_files("d").unwrap(), ["f"]);

        vfs.set_read_only("d/f").unwrap();
        let mode = fs::metadata(vfs.path("d/f")).unwrap().permissions().mode();
        assert_eq!(mode & 0o222, 0, "{mode:o}");
        vfs.rename("d/f", "g").unwrap();
        let mut bytes = Vec::new();
        vfs.read_file("g").unwrap().read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"abc");
        vfs.remove("g").unwrap();
        assert!(!vfs.exists("g").unwrap());
        assert!(vfs.exists("d").unwrap());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn an_os_file_sync_takes_only_a_regular_file() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let root = std::env::temp_dir().join(format!("zoe-vfs-sync-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let vfs = OsVfs::new(&root).unwrap();
        fs::write(vfs.path("f"), b"x").unwrap();
        fs::set_permissions(vfs.path("f"), fs::Permissions::from_mode(0o444)).unwrap();
        vfs.sync_file("f").unwrap();

        vfs.create_dir("d").unwrap();
        let error = vfs.sync_file("d").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{error}");
        let _socket = std::os::unix::net::UnixListener::bind(vfs.path("socket")).unwrap();
        assert!(vfs.sync_file("socket").is_err());
        symlink(vfs.path("f"), vfs.path("link")).unwrap();
        let error = vfs.sync_file("link").unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::ELOOP), "{error}");
        assert_eq!(
            vfs.sync_file("missing").unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn an_os_file_writes_zeros_into_a_small_growth() {
        use std::os::unix::fs::MetadataExt;

        let root = std::env::temp_dir().join(format!("zoe-vfs-grow-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let vfs = OsVfs::new(&root).unwrap();
        let file = vfs.open("f").unwrap();
        file.write(0, b"data").unwrap();
        let len = 3 << 20;
        file.set_len(len).unwrap();
        assert_eq!(file.len().unwrap(), len);
        let mut start = [0; 4];
        file.read(0, &mut start).unwrap();
        assert_eq!(&start, b"data");
        let mut rest = vec![1; usize::try_from(len).unwrap() - 4];
        file.read(4, &mut rest).unwrap();
        assert!(rest.iter().all(|&b| b == 0));
        // The new bytes have their blocks: the file has no hole.
        let blocks = fs::metadata(vfs.path("f")).unwrap().blocks();
        assert!(blocks * 512 >= len, "{blocks} blocks");

        // A larger growth gets no zeros, and a shrink writes nothing.
        let large = len + ZERO_FILL_MAX + 1;
        file.set_len(large).unwrap();
        assert_eq!(file.len().unwrap(), large);
        file.set_len(4096).unwrap();
        assert_eq!(file.len().unwrap(), 4096);
        drop(file);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn an_os_file_holds_its_writes_until_a_read_or_a_sync() {
        let root = std::env::temp_dir().join(format!("zoe-vfs-merge-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let vfs = OsVfs::new(&root).unwrap();
        let file = vfs.open("f").unwrap();
        file.set_len(64 << 10).unwrap();
        let on_disk = || fs::read(vfs.path("f")).unwrap();

        // Adjacent writes in any order, and a write that takes the place
        // of another: none reaches the file before a sync.
        file.write(8192, &[3; 4096]).unwrap();
        file.write(0, &[1; 4096]).unwrap();
        file.write(4096, &[2; 4096]).unwrap();
        file.write(0, &[4; 4096]).unwrap();
        assert!(on_disk()[..12288].iter().all(|&b| b == 0));
        file.sync_data().unwrap();
        let bytes = on_disk();
        assert!(bytes[..4096].iter().all(|&b| b == 4));
        assert!(bytes[4096..8192].iter().all(|&b| b == 2));
        assert!(bytes[8192..12288].iter().all(|&b| b == 3));

        // A read sees a held write.
        file.write(20000, b"held").unwrap();
        let mut out = [0; 4];
        file.read(20000, &mut out).unwrap();
        assert_eq!(&out, b"held");

        // A write that overlaps a held write at another place comes after
        // it.
        file.write(30000, &[5; 100]).unwrap();
        file.write(30050, &[6; 100]).unwrap();
        let mut out = [0; 150];
        file.read(30000, &mut out).unwrap();
        assert!(out[..50].iter().all(|&b| b == 5));
        assert!(out[50..].iter().all(|&b| b == 6));

        // A change of the length and the close make the held writes first.
        file.write(40000, b"len").unwrap();
        file.set_len(48 << 10).unwrap();
        assert_eq!(&on_disk()[40000..40003], b"len");
        file.write(41000, b"close").unwrap();
        file.close().unwrap();
        drop(file);
        assert_eq!(&on_disk()[41000..41005], b"close");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_sync_waits_for_the_held_writes_that_a_read_makes() {
        let root = std::env::temp_dir().join(format!("zoe-vfs-race-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let vfs = OsVfs::new(&root).unwrap();
        let file = Arc::new(vfs.open("f").unwrap());
        file.set_len(4 * 4096).unwrap();
        // Two pages that are not adjacent: two writes.
        file.write(0, &[1; 4096]).unwrap();
        file.write(2 * 4096, &[2; 4096]).unwrap();
        let entered = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        *file.pause.lock().unwrap() = Some((entered.clone(), release.clone()));

        // A read makes the held writes, and stops after the first one.
        let reader = {
            let file = file.clone();
            std::thread::spawn(move || file.read(0, &mut [0; 8]).unwrap())
        };
        entered.wait();
        // A sync now must wait for the second write of the read.
        let synced = Arc::new(AtomicBool::new(false));
        let syncer = {
            let (file, synced) = (file.clone(), synced.clone());
            std::thread::spawn(move || {
                file.sync_data().unwrap();
                synced.store(true, Ordering::SeqCst);
            })
        };
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(!synced.load(Ordering::SeqCst), "the sync did not wait");
        release.wait();
        reader.join().unwrap();
        syncer.join().unwrap();
        let bytes = fs::read(vfs.path("f")).unwrap();
        assert!(bytes[..4096].iter().all(|&b| b == 1));
        assert!(bytes[2 * 4096..3 * 4096].iter().all(|&b| b == 2));
        drop(file);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn an_os_file_of_create_new_writes_at_once() {
        let root = std::env::temp_dir().join(format!("zoe-vfs-spill-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let vfs = OsVfs::new(&root).unwrap();
        let file = vfs.create_new("spill").unwrap();
        file.set_len(4096).unwrap();
        file.write(0, b"spilled").unwrap();
        // The bytes are in the file before any sync: a spill file holds no
        // write in memory.
        assert_eq!(&fs::read(vfs.path("spill")).unwrap()[..7], b"spilled");
        drop(file);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_directory_lock_has_one_holder() {
        let root = std::env::temp_dir().join(format!("zoe-vfs-lock-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let vfs = OsVfs::new(&root).unwrap();
        vfs.create_dir("d").unwrap();
        let lock = vfs.lock_dir("d").unwrap();
        let error = vfs.lock_dir("d").err().unwrap();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock, "{error}");
        drop(lock);
        drop(vfs.lock_dir("d").unwrap());
        fs::write(vfs.path("f"), b"x").unwrap();
        let error = vfs.lock_dir("f").err().unwrap();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{error}");
        assert!(vfs.lock_dir("missing").is_err());
        // The lock adds no file to the directory.
        assert!(fs::read_dir(vfs.path("d")).unwrap().next().is_none());
        fs::remove_dir_all(&root).unwrap();

        // A simulated filesystem keeps its locks until they drop, or until
        // the process is killed.
        let vfs = MemVfs::new();
        Vfs::create_dir(&vfs, "d").unwrap();
        let lock = Vfs::lock_dir(&vfs, "d").unwrap();
        let error = Vfs::lock_dir(&vfs, "d").err().unwrap();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        std::mem::forget(lock);
        vfs.restart();
        drop(Vfs::lock_dir(&vfs, "d").unwrap());
    }
}
