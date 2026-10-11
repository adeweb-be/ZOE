//! The directory of a store, and the simulated filesystem of the crash
//! tests.
//!
//! The core reaches the files of a store only through the `Vfs` trait.
//! `OsVfs` is a directory of the operating system. `MemVfs` keeps its
//! files and directories in memory and records every write, length change,
//! sync and directory change. `MemVfs::crash_images` replays that record
//! and returns the states that a crash can leave on disk.
//!
//! A name is a path in the store directory, with `/` between its parts,
//! for example `blobs/tmp/1.tmp`. The empty name `""` is the store
//! directory itself.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, BufWriter, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::error::{Result, ZoeError};

/// The directory of a store, as the core sees it.
pub trait Vfs: Send + Sync + 'static {
    /// An open file that the core reads and writes at any offset. redb
    /// stores its database in one, and a transaction its spill file.
    type File: redb::StorageBackend;

    /// Tell if the file or directory `name` exists.
    fn exists(&self, name: &str) -> io::Result<bool>;

    /// Open the file `name`. Create it empty when it does not exist.
    fn open(&self, name: &str) -> io::Result<Self::File>;

    /// Create the new, empty file `name` and open it, as `open` does. Fail
    /// when `name` exists. A symbolic link exists too, so the call never
    /// opens the target of a link.
    fn create_new(&self, name: &str) -> io::Result<Self::File>;

    /// Rename the file `from` to `to`, and replace `to` if it exists.
    fn rename(&self, from: &str, to: &str) -> io::Result<()>;

    /// Remove the file `name`.
    fn remove(&self, name: &str) -> io::Result<()>;

    /// Make the creations, renames and removals in the directory durable.
    fn sync_dir(&self) -> io::Result<()>;

    /// Make the creations, renames and removals in the directory `dir`
    /// durable. A rename is durable when the directory of its new name is.
    fn sync_dir_at(&self, dir: &str) -> io::Result<()>;

    /// Create the directory `name`. Its parent must exist. The new
    /// directory is durable after a sync of its parent.
    fn create_dir(&self, name: &str) -> io::Result<()>;

    /// Return the names of the regular files in the directory `dir`,
    /// without the directory part, in order. Directories and other
    /// entries are not in the list.
    fn list_files(&self, dir: &str) -> io::Result<Vec<String>>;

    /// Return the names of all the entries in the directory `dir`, without
    /// the directory part, in order: files, directories and any other
    /// entry. A name that is not UTF-8 comes back with its bytes replaced.
    fn list_entries(&self, dir: &str) -> io::Result<Vec<String>>;

    /// Create the new, empty file `name` and open it for writing. Fail
    /// when `name` exists.
    fn create_file(&self, name: &str) -> io::Result<Box<dyn WriteFile>>;

    /// Open the file `name` for reading from its start.
    fn read_file(&self, name: &str) -> io::Result<Box<dyn Read + Send>>;

    /// Make the bytes of the existing file `name` durable. Fail when
    /// `name` is not a regular file: a symbolic link is refused too.
    fn sync_file(&self, name: &str) -> io::Result<()>;

    /// Remove the write permissions of the file `name`.
    fn set_read_only(&self, name: &str) -> io::Result<()>;

    /// Return the path of `name`. A simulated filesystem returns `name`
    /// as a relative path.
    fn path(&self, name: &str) -> PathBuf;
}

/// A shared directory. The clones share the directory and its lock. The
/// lock goes when the last clone is dropped.
impl<V: Vfs> Vfs for Arc<V> {
    type File = V::File;

    fn exists(&self, name: &str) -> io::Result<bool> {
        (**self).exists(name)
    }

    fn open(&self, name: &str) -> io::Result<Self::File> {
        (**self).open(name)
    }

    fn create_new(&self, name: &str) -> io::Result<Self::File> {
        (**self).create_new(name)
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        (**self).rename(from, to)
    }

    fn remove(&self, name: &str) -> io::Result<()> {
        (**self).remove(name)
    }

    fn sync_dir(&self) -> io::Result<()> {
        (**self).sync_dir()
    }

    fn sync_dir_at(&self, dir: &str) -> io::Result<()> {
        (**self).sync_dir_at(dir)
    }

    fn create_dir(&self, name: &str) -> io::Result<()> {
        (**self).create_dir(name)
    }

    fn list_files(&self, dir: &str) -> io::Result<Vec<String>> {
        (**self).list_files(dir)
    }

    fn list_entries(&self, dir: &str) -> io::Result<Vec<String>> {
        (**self).list_entries(dir)
    }

    fn create_file(&self, name: &str) -> io::Result<Box<dyn WriteFile>> {
        (**self).create_file(name)
    }

    fn read_file(&self, name: &str) -> io::Result<Box<dyn Read + Send>> {
        (**self).read_file(name)
    }

    fn sync_file(&self, name: &str) -> io::Result<()> {
        (**self).sync_file(name)
    }

    fn set_read_only(&self, name: &str) -> io::Result<()> {
        (**self).set_read_only(name)
    }

    fn path(&self, name: &str) -> PathBuf {
        (**self).path(name)
    }
}

/// A file that the core writes from its start to its end.
pub trait WriteFile: Write + Send {
    /// Write the buffered bytes and make the bytes of the file durable.
    fn sync(&mut self) -> io::Result<()>;
}

/// Return the directory that holds `name`: the part before the last `/`,
/// or `""` for a name in the store directory.
pub(crate) fn parent(name: &str) -> &str {
    name.rsplit_once('/').map_or("", |(dir, _)| dir)
}

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

    fn create_new(&self, name: &str) -> io::Result<Self::File> {
        // `create_new` sets `O_EXCL`. With it, the creation fails on any
        // entry of the name, a symbolic link included.
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
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
        self.sync_dir_at("")
    }

    fn sync_dir_at(&self, dir: &str) -> io::Result<()> {
        File::open(self.dir.join(dir))?.sync_all()
    }

    fn create_dir(&self, name: &str) -> io::Result<()> {
        fs::create_dir(self.dir.join(name))
    }

    fn list_files(&self, dir: &str) -> io::Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in fs::read_dir(self.dir.join(dir))? {
            let entry = entry?;
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

    fn list_entries(&self, dir: &str) -> io::Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in fs::read_dir(self.dir.join(dir))? {
            names.push(entry?.file_name().to_string_lossy().into_owned());
        }
        names.sort();
        Ok(names)
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
        file.sync_all()
    }

    fn set_read_only(&self, name: &str) -> io::Result<()> {
        let path = self.dir.join(name);
        let mut permissions = fs::metadata(&path)?.permissions();
        permissions.set_readonly(true);
        fs::set_permissions(path, permissions)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }
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
        self.0.get_ref().sync_all()
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
    CreateDir {
        dir: String,
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
    SyncDir {
        dir: String,
    },
    Mark {
        name: String,
    },
}

/// The state of the process that uses a `MemVfs`.
#[derive(Debug, Default)]
enum Process {
    #[default]
    Alive,
    /// The process dies at the next sync of this directory.
    KillAtDirSync(String),
    /// The process is dead. Every operation fails.
    Dead,
}

#[derive(Debug, Default)]
struct State {
    /// The files when the record starts.
    initial: BTreeMap<String, Vec<u8>>,
    /// The directories when the record starts.
    initial_dirs: BTreeSet<String>,
    /// The files as the program sees them.
    files: BTreeMap<String, Vec<u8>>,
    /// The directories as the program sees them, without the store
    /// directory.
    dirs: BTreeSet<String>,
    /// Every operation since the record started, in order.
    log: Vec<Op>,
    process: Process,
}

impl State {
    /// Return an error when the process is dead.
    fn alive(&self) -> io::Result<()> {
        match self.process {
            Process::Dead => Err(io::Error::other("the process was killed")),
            _ => Ok(()),
        }
    }

    /// Record the sync of the directory `dir`, or kill the process when
    /// it dies at this sync.
    fn sync_dir(&mut self, dir: &str) -> io::Result<()> {
        self.alive()?;
        if matches!(&self.process, Process::KillAtDirSync(at) if at == dir) {
            self.process = Process::Dead;
            return Err(io::Error::other("the process was killed"));
        }
        self.log.push(Op::SyncDir {
            dir: dir.to_owned(),
        });
        Ok(())
    }

    fn has_dir(&self, dir: &str) -> bool {
        dir.is_empty() || self.dirs.contains(dir)
    }

    /// Return an error unless the directory of `name` exists.
    fn check_parent(&self, name: &str) -> io::Result<()> {
        if self.has_dir(parent(name)) {
            Ok(())
        } else {
            Err(not_found(parent(name)))
        }
    }
}

fn not_found(name: &str) -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, name.to_owned())
}

/// An in-memory filesystem that records every operation.
///
/// Clones share the same files and the same record. The files and
/// directories that exist when the record starts are durable.
#[derive(Clone, Debug, Default)]
pub struct MemVfs {
    state: Arc<Mutex<State>>,
}

impl MemVfs {
    /// Return an empty filesystem.
    pub fn new() -> Self {
        Self::default()
    }

    fn with_tree(files: BTreeMap<String, Vec<u8>>, dirs: BTreeSet<String>) -> Self {
        let state = State {
            initial: files.clone(),
            initial_dirs: dirs.clone(),
            files,
            dirs,
            log: Vec::new(),
            process: Process::Alive,
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

    /// Return the names of the files, in order.
    pub fn names(&self) -> Vec<String> {
        self.lock().files.keys().cloned().collect()
    }

    /// Kill the process at the next sync of the directory `dir`, as
    /// `kill -9` does. That sync and every later operation fail and are
    /// not recorded. The files keep every earlier operation, synced or
    /// not, as the page cache of the system does.
    pub fn kill_at_dir_sync(&self, dir: &str) {
        self.lock().process = Process::KillAtDirSync(dir.to_owned());
    }

    /// Start a new process on the files of a killed one. The record goes
    /// on, so an operation that the killed process did not sync stays
    /// pending in the crash images.
    pub fn restart(&self) {
        self.lock().process = Process::Alive;
    }

    /// Return a filesystem with the files and the directories of this one,
    /// all durable, and an empty record. The crash images of the copy
    /// cover only what comes after this call.
    pub fn durable_copy(&self) -> MemVfs {
        let state = self.lock();
        MemVfs::with_tree(state.files.clone(), state.dirs.clone())
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
    /// creation, a removal or the creation of a directory is synced by a
    /// later sync of the directory that holds it. A rename is synced by a
    /// later sync of the directory of its new name. The other operations
    /// are pending. The images of a crash point are these:
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
    /// a file whose creation is not on disk is lost. A file or a directory
    /// whose directory is not on disk is lost.
    ///
    /// An image holds only the list of the operations that land. It builds
    /// its files when `CrashImage::vfs` is called, so a test that opens
    /// one image at a time holds the files of one image at a time.
    pub fn crash_images(&self) -> Vec<CrashImage> {
        let recording = {
            let state = self.lock();
            Arc::new(Recording {
                files: state.initial.clone(),
                dirs: state.initial_dirs.clone(),
                log: state.log.clone(),
            })
        };
        let mut images = Vec::new();
        for point in 0..=recording.log.len() {
            let ops = &recording.log[..point];
            let (synced, pending) = split_at_syncs(ops);
            let mut push = |variant: Variant| {
                let chosen = match &variant {
                    Variant::Pending(chosen) => &chosen[..],
                    Variant::Torn(..) => &pending[..],
                };
                let mut which: Vec<usize> = synced.iter().chain(chosen).copied().collect();
                which.sort_unstable();
                images.push(CrashImage {
                    recording: Arc::clone(&recording),
                    point,
                    which,
                    variant,
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
        let state = self.lock();
        state.files.contains_key(name) || state.has_dir(name)
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        let mut state = self.lock();
        state.alive()?;
        state.check_parent(to)?;
        let bytes = state.files.remove(from).ok_or_else(|| not_found(from))?;
        state.files.insert(to.to_owned(), bytes);
        state.log.push(Op::Rename {
            from: from.to_owned(),
            to: to.to_owned(),
        });
        Ok(())
    }

    fn remove(&self, name: &str) -> io::Result<()> {
        let mut state = self.lock();
        state.alive()?;
        state.files.remove(name).ok_or_else(|| not_found(name))?;
        state.log.push(Op::Remove {
            file: name.to_owned(),
        });
        Ok(())
    }

    #[cfg(test)]
    fn sync_dir(&self) {
        // The tests that call this kill no process.
        let _ = self.lock().sync_dir("");
    }

    fn sync_dir_at(&self, dir: &str) -> io::Result<()> {
        let mut state = self.lock();
        if !state.has_dir(dir) {
            return Err(not_found(dir));
        }
        state.sync_dir(dir)
    }

    fn create_dir(&self, name: &str) -> io::Result<()> {
        let mut state = self.lock();
        state.alive()?;
        if state.files.contains_key(name) || state.has_dir(name) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                name.to_owned(),
            ));
        }
        state.check_parent(name)?;
        state.dirs.insert(name.to_owned());
        state.log.push(Op::CreateDir {
            dir: name.to_owned(),
        });
        Ok(())
    }

    fn list_files(&self, dir: &str) -> io::Result<Vec<String>> {
        let state = self.lock();
        state.alive()?;
        if !state.has_dir(dir) {
            return Err(not_found(dir));
        }
        let mut names: Vec<String> = state
            .files
            .keys()
            .filter(|name| parent(name) == dir)
            .map(|name| name.rsplit('/').next().unwrap_or_default().to_owned())
            .collect();
        names.sort();
        Ok(names)
    }

    /// Create the new, empty file `name`. Fail when `name` exists or its
    /// directory does not.
    fn create_new(&self, name: &str) -> io::Result<MemFile> {
        let mut state = self.lock();
        state.alive()?;
        if state.files.contains_key(name) || state.has_dir(name) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                name.to_owned(),
            ));
        }
        state.check_parent(name)?;
        state.files.insert(name.to_owned(), Vec::new());
        state.log.push(Op::Create {
            file: name.to_owned(),
        });
        Ok(MemFile {
            vfs: self.clone(),
            name: name.to_owned(),
        })
    }

    fn list_entries(&self, dir: &str) -> io::Result<Vec<String>> {
        let state = self.lock();
        state.alive()?;
        if !state.has_dir(dir) {
            return Err(not_found(dir));
        }
        let mut names: Vec<String> = state
            .files
            .keys()
            .chain(state.dirs.iter())
            .filter(|name| parent(name) == dir && !name.is_empty())
            .map(|name| name.rsplit('/').next().unwrap_or_default().to_owned())
            .collect();
        names.sort();
        Ok(names)
    }

    fn create_file(&self, name: &str) -> io::Result<MemWriteFile> {
        let file = self.create_new(name)?;
        Ok(MemWriteFile {
            vfs: file.vfs,
            name: file.name,
            offset: 0,
        })
    }

    fn read_file(&self, name: &str) -> io::Result<MemReadFile> {
        let state = self.lock();
        state.alive()?;
        if !state.files.contains_key(name) {
            return Err(not_found(name));
        }
        Ok(MemReadFile {
            vfs: self.clone(),
            name: name.to_owned(),
            offset: 0,
        })
    }
}

impl Vfs for MemVfs {
    type File = MemFile;

    fn exists(&self, name: &str) -> io::Result<bool> {
        self.lock().alive()?;
        Ok(MemVfs::exists(self, name))
    }

    fn open(&self, name: &str) -> io::Result<Self::File> {
        self.lock().alive()?;
        Ok(self.file(name))
    }

    fn create_new(&self, name: &str) -> io::Result<Self::File> {
        MemVfs::create_new(self, name)
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        MemVfs::rename(self, from, to)
    }

    fn remove(&self, name: &str) -> io::Result<()> {
        MemVfs::remove(self, name)
    }

    fn sync_dir(&self) -> io::Result<()> {
        self.lock().sync_dir("")
    }

    fn sync_dir_at(&self, dir: &str) -> io::Result<()> {
        MemVfs::sync_dir_at(self, dir)
    }

    fn create_dir(&self, name: &str) -> io::Result<()> {
        MemVfs::create_dir(self, name)
    }

    fn list_files(&self, dir: &str) -> io::Result<Vec<String>> {
        MemVfs::list_files(self, dir)
    }

    fn list_entries(&self, dir: &str) -> io::Result<Vec<String>> {
        MemVfs::list_entries(self, dir)
    }

    fn create_file(&self, name: &str) -> io::Result<Box<dyn WriteFile>> {
        Ok(Box::new(MemVfs::create_file(self, name)?))
    }

    fn read_file(&self, name: &str) -> io::Result<Box<dyn Read + Send>> {
        Ok(Box::new(MemVfs::read_file(self, name)?))
    }

    fn sync_file(&self, name: &str) -> io::Result<()> {
        let mut state = self.lock();
        state.alive()?;
        if !state.files.contains_key(name) {
            return Err(not_found(name));
        }
        state.log.push(Op::Sync {
            file: name.to_owned(),
        });
        Ok(())
    }

    /// The simulation keeps no permissions, so this only checks that the
    /// file exists.
    fn set_read_only(&self, name: &str) -> io::Result<()> {
        let state = self.lock();
        state.alive()?;
        if state.files.contains_key(name) {
            Ok(())
        } else {
            Err(not_found(name))
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        PathBuf::from(name)
    }
}

/// Split the indexes of the file and directory operations in `ops` into
/// the synced ones and the pending ones.
fn split_at_syncs(ops: &[Op]) -> (Vec<usize>, Vec<usize>) {
    let mut last_sync: HashMap<&str, usize> = HashMap::new();
    let mut last_dir_sync: HashMap<&str, usize> = HashMap::new();
    for (i, op) in ops.iter().enumerate() {
        match op {
            Op::Sync { file } => {
                last_sync.insert(file, i);
            }
            Op::SyncDir { dir } => {
                last_dir_sync.insert(dir, i);
            }
            _ => {}
        }
    }
    let mut synced = Vec::new();
    let mut pending = Vec::new();
    for (i, op) in ops.iter().enumerate() {
        let dir_synced = |name: &str| last_dir_sync.get(parent(name)).is_some_and(|&s| i < s);
        let is_synced = match op {
            Op::Write { file, .. } | Op::SetLen { file, .. } => {
                last_sync.get(file.as_str()).is_some_and(|&s| i < s)
            }
            Op::Create { file } | Op::Remove { file } => dir_synced(file),
            Op::CreateDir { dir } => dir_synced(dir),
            Op::Rename { to, .. } => dir_synced(to),
            Op::Sync { .. } | Op::SyncDir { .. } | Op::Mark { .. } => continue,
        };
        if is_synced {
            synced.push(i);
        } else {
            pending.push(i);
        }
    }
    (synced, pending)
}

/// Apply the operations `which` of `ops` to a copy of `files` and `dirs`,
/// in order. Of the write `torn`, only the given half of its bytes lands.
fn apply(
    files: &BTreeMap<String, Vec<u8>>,
    dirs: &BTreeSet<String>,
    ops: &[Op],
    which: &[usize],
    torn: Option<(usize, Half)>,
) -> (BTreeMap<String, Vec<u8>>, BTreeSet<String>) {
    let mut files = files.clone();
    let mut dirs = dirs.clone();
    for &i in which {
        let has_dir = |dirs: &BTreeSet<String>, name: &str| {
            let dir = parent(name);
            dir.is_empty() || dirs.contains(dir)
        };
        match &ops[i] {
            Op::Create { file } => {
                if has_dir(&dirs, file) {
                    files.entry(file.clone()).or_default();
                }
            }
            Op::CreateDir { dir } => {
                if has_dir(&dirs, dir) {
                    dirs.insert(dir.clone());
                }
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
                if has_dir(&dirs, to)
                    && let Some(bytes) = files.remove(from)
                {
                    files.insert(to.clone(), bytes);
                }
            }
            Op::Remove { file } => {
                files.remove(file);
            }
            Op::Sync { .. } | Op::SyncDir { .. } | Op::Mark { .. } => {}
        }
    }
    (files, dirs)
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
        state.alive()?;
        Ok(state.files.get(&self.name).map_or(0, |f| f.len() as u64))
    }

    fn read(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
        let state = self.vfs.lock();
        state.alive()?;
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
        state.alive()?;
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
        let mut state = self.vfs.lock();
        state.alive()?;
        state.log.push(Op::Sync {
            file: self.name.clone(),
        });
        Ok(())
    }

    fn write(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        let mut state = self.vfs.lock();
        state.alive()?;
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

/// A file of a `MemVfs` that the core writes from its start. Each write
/// is one recorded operation.
#[derive(Debug)]
struct MemWriteFile {
    vfs: MemVfs,
    name: String,
    offset: u64,
}

impl Write for MemWriteFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut state = self.vfs.lock();
        state.alive()?;
        let bytes = state
            .files
            .get_mut(&self.name)
            .ok_or_else(|| not_found(&self.name))?;
        write_at(bytes, self.offset, buf);
        state.log.push(Op::Write {
            file: self.name.clone(),
            offset: self.offset,
            data: buf.to_vec(),
        });
        self.offset += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl WriteFile for MemWriteFile {
    fn sync(&mut self) -> io::Result<()> {
        let mut state = self.vfs.lock();
        state.alive()?;
        state.log.push(Op::Sync {
            file: self.name.clone(),
        });
        Ok(())
    }
}

/// A file of a `MemVfs` that the core reads from its start.
#[derive(Debug)]
struct MemReadFile {
    vfs: MemVfs,
    name: String,
    offset: usize,
}

impl Read for MemReadFile {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let state = self.vfs.lock();
        state.alive()?;
        let bytes = state
            .files
            .get(&self.name)
            .ok_or_else(|| not_found(&self.name))?;
        let rest = bytes.get(self.offset..).unwrap_or_default();
        let n = rest.len().min(buf.len());
        buf[..n].copy_from_slice(&rest[..n]);
        self.offset += n;
        Ok(n)
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

/// The files and directories when a record starts, and the record. The
/// crash images of one call share it.
struct Recording {
    files: BTreeMap<String, Vec<u8>>,
    dirs: BTreeSet<String>,
    log: Vec<Op>,
}

/// A state that a crash can leave on disk.
pub struct CrashImage {
    recording: Arc<Recording>,
    /// The number of operations before the crash.
    point: usize,
    /// The indexes of the operations that land, in order.
    which: Vec<usize>,
    variant: Variant,
}

impl CrashImage {
    /// Return a filesystem with the files and directories of this image
    /// and an empty record.
    pub fn vfs(&self) -> MemVfs {
        let (files, dirs) = self.tree();
        MemVfs::with_tree(files, dirs)
    }

    /// Tell if the crash came after the mark `name`.
    pub fn is_after(&self, name: &str) -> bool {
        self.recording.log[..self.point]
            .iter()
            .any(|op| matches!(op, Op::Mark { name: mark } if mark == name))
    }

    /// Build the files and the directories of this image.
    fn tree(&self) -> (BTreeMap<String, Vec<u8>>, BTreeSet<String>) {
        let torn = match self.variant {
            Variant::Torn(i, half) => Some((i, half)),
            Variant::Pending(_) => None,
        };
        let recording = &self.recording;
        let ops = &recording.log[..self.point];
        apply(&recording.files, &recording.dirs, ops, &self.which, torn)
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
        image.tree().0.get(name).cloned()
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

    /// Write `bytes` into the new file `name` and sync the file.
    fn write_synced(vfs: &MemVfs, name: &str, bytes: &[u8]) {
        let mut file = Vfs::create_file(vfs, name).unwrap();
        file.write_all(bytes).unwrap();
        file.sync().unwrap();
    }

    #[test]
    fn a_file_whose_directory_is_not_durable_can_vanish() {
        let vfs = MemVfs::new();
        Vfs::create_dir(&vfs, "d").unwrap();
        write_synced(&vfs, "d/f", b"x");
        vfs.sync_dir_at("d").unwrap();

        let images = vfs.crash_images();
        let last = images.iter().filter(|i| i.point == vfs.lock().log.len());
        let found: Vec<_> = last.map(|i| contents(i, "d/f")).collect();
        assert!(found.contains(&None));
        assert!(found.contains(&Some(b"x".to_vec())));
    }

    #[test]
    fn a_rename_is_durable_after_a_sync_of_its_new_directory() {
        let vfs = MemVfs::new();
        for dir in ["a", "b"] {
            Vfs::create_dir(&vfs, dir).unwrap();
        }
        vfs.sync_dir();
        write_synced(&vfs, "a/f", b"x");
        vfs.sync_dir_at("a").unwrap();
        Vfs::rename(&vfs, "a/f", "b/f").unwrap();
        vfs.sync_dir_at("b").unwrap();
        vfs.mark("renamed");

        for image in vfs.crash_images().iter().filter(|i| i.is_after("renamed")) {
            assert_eq!(contents(image, "a/f"), None);
            assert_eq!(contents(image, "b/f").as_deref(), Some(&b"x"[..]));
        }
    }

    #[test]
    fn a_sync_of_another_directory_leaves_a_rename_pending() {
        let vfs = MemVfs::new();
        for dir in ["a", "b"] {
            Vfs::create_dir(&vfs, dir).unwrap();
        }
        vfs.sync_dir();
        write_synced(&vfs, "a/f", b"x");
        vfs.sync_dir_at("a").unwrap();
        Vfs::rename(&vfs, "a/f", "b/f").unwrap();
        vfs.sync_dir_at("a").unwrap();
        vfs.mark("synced");

        let images = vfs.crash_images();
        let after: Vec<_> = images.iter().filter(|i| i.is_after("synced")).collect();
        assert!(after.iter().any(|i| contents(i, "a/f").is_some()));
        assert!(after.iter().any(|i| contents(i, "b/f").is_some()));
    }

    #[test]
    fn a_directory_lists_only_its_own_files() {
        let vfs = MemVfs::new();
        Vfs::create_dir(&vfs, "d").unwrap();
        Vfs::create_dir(&vfs, "d/e").unwrap();
        write_synced(&vfs, "d/f", b"");
        write_synced(&vfs, "d/e/g", b"");
        write_synced(&vfs, "h", b"");
        assert_eq!(Vfs::list_files(&vfs, "d").unwrap(), ["f"]);
        assert_eq!(Vfs::list_files(&vfs, "").unwrap(), ["h"]);
        assert_eq!(vfs.names(), ["d/e/g", "d/f", "h"]);
        assert!(Vfs::list_files(&vfs, "x").is_err());
        assert!(Vfs::create_file(&vfs, "x/f").is_err());
        assert!(Vfs::create_file(&vfs, "h").is_err());
        assert!(Vfs::create_dir(&vfs, "d").is_err());
    }

    #[test]
    fn a_written_file_reads_back() {
        let vfs = MemVfs::new();
        write_synced(&vfs, "f", b"abc");
        let mut bytes = Vec::new();
        Vfs::read_file(&vfs, "f")
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        assert_eq!(bytes, b"abc");
        assert_eq!(Vfs::path(&vfs, "d/f"), PathBuf::from("d/f"));
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
    fn a_file_sync_makes_the_earlier_writes_durable() {
        let vfs = MemVfs::new();
        let mut file = Vfs::create_file(&vfs, "f").unwrap();
        file.write_all(b"x").unwrap();
        drop(file);
        vfs.sync_dir();
        Vfs::sync_file(&vfs, "f").unwrap();
        vfs.mark("synced");
        assert!(Vfs::sync_file(&vfs, "g").is_err());

        for image in vfs.crash_images().iter().filter(|i| i.is_after("synced")) {
            assert_eq!(contents(image, "f").as_deref(), Some(&b"x"[..]));
        }
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
        let vfs = MemVfs::with_tree(created.lock().files.clone(), BTreeSet::new());
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
