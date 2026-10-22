//! The directory of a store, and the simulated filesystem of the crash
//! tests.
//!
//! The core reaches the files of a store only through the `Vfs` trait.
//! `OsVfs` is a directory of the operating system. `MemVfs` keeps its
//! files and directories in memory and records every write, length change,
//! sync and directory change. `MemVfs::crash_images` replays that record
//! and returns the states that a crash can leave on disk.
//! `MemVfs::bytes_read` counts the bytes that its files give to reads.
//!
//! A name is a path in the store directory, with `/` between its parts,
//! for example `blobs/tmp/1.tmp`. The empty name `""` is the store
//! directory itself.
//!
//! `os` holds `OsVfs`, `mem` holds `MemVfs` and its crash images.

use std::any::Any;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::Arc;

mod mem;
mod os;

pub use mem::{CrashImage, MemFile, MemVfs};
#[cfg(test)]
pub(crate) use os::LOCK;
pub(crate) use os::list_dir;
pub use os::{OsFile, OsVfs};

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

    /// Give the existing file `from` the second name `to`, a hard link.
    /// Fail when `to` exists. The two names then give the same bytes, and
    /// no byte is copied. The new name is durable after a sync of its
    /// directory. Only a file that nothing writes again gets a link. A
    /// filesystem that cannot link fails with its error, for example
    /// `CrossesDevices`, `PermissionDenied` or `Unsupported`.
    fn hard_link(&self, from: &str, to: &str) -> io::Result<()>;

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

    /// Take an exclusive lock on the directory `name` itself, and keep it
    /// until the returned lock drops. Another holder of the lock fails the
    /// call with `WouldBlock`. A filesystem that cannot lock a directory
    /// fails the call with its error: the call never skips the lock.
    fn lock_dir(&self, name: &str) -> io::Result<DirLock>;

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

    fn hard_link(&self, from: &str, to: &str) -> io::Result<()> {
        (**self).hard_link(from, to)
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

    fn lock_dir(&self, name: &str) -> io::Result<DirLock> {
        (**self).lock_dir(name)
    }

    fn path(&self, name: &str) -> PathBuf {
        (**self).path(name)
    }
}

/// The operations of a `Vfs` that the blob store and the staging area
/// use, in a form that a trait object can hold. Every `Vfs` has them, and
/// each method does what the method of `Vfs` with its name does.
pub(crate) trait Dir: Send + Sync {
    fn exists(&self, name: &str) -> io::Result<bool>;
    fn create_new(&self, name: &str) -> io::Result<Box<dyn redb::StorageBackend>>;
    fn rename(&self, from: &str, to: &str) -> io::Result<()>;
    fn hard_link(&self, from: &str, to: &str) -> io::Result<()>;
    fn remove(&self, name: &str) -> io::Result<()>;
    fn sync_dir_at(&self, dir: &str) -> io::Result<()>;
    fn create_dir(&self, name: &str) -> io::Result<()>;
    fn list_files(&self, dir: &str) -> io::Result<Vec<String>>;
    fn list_entries(&self, dir: &str) -> io::Result<Vec<String>>;
    fn create_file(&self, name: &str) -> io::Result<Box<dyn WriteFile>>;
    fn read_file(&self, name: &str) -> io::Result<Box<dyn Read + Send>>;
    fn sync_file(&self, name: &str) -> io::Result<()>;
    fn set_read_only(&self, name: &str) -> io::Result<()>;
    fn lock_dir(&self, name: &str) -> io::Result<DirLock>;
    fn path(&self, name: &str) -> PathBuf;
}

impl<V: Vfs> Dir for V {
    fn exists(&self, name: &str) -> io::Result<bool> {
        Vfs::exists(self, name)
    }

    fn create_new(&self, name: &str) -> io::Result<Box<dyn redb::StorageBackend>> {
        Ok(Box::new(Vfs::create_new(self, name)?))
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        Vfs::rename(self, from, to)
    }

    fn hard_link(&self, from: &str, to: &str) -> io::Result<()> {
        Vfs::hard_link(self, from, to)
    }

    fn remove(&self, name: &str) -> io::Result<()> {
        Vfs::remove(self, name)
    }

    fn sync_dir_at(&self, dir: &str) -> io::Result<()> {
        Vfs::sync_dir_at(self, dir)
    }

    fn create_dir(&self, name: &str) -> io::Result<()> {
        Vfs::create_dir(self, name)
    }

    fn list_files(&self, dir: &str) -> io::Result<Vec<String>> {
        Vfs::list_files(self, dir)
    }

    fn list_entries(&self, dir: &str) -> io::Result<Vec<String>> {
        Vfs::list_entries(self, dir)
    }

    fn create_file(&self, name: &str) -> io::Result<Box<dyn WriteFile>> {
        Vfs::create_file(self, name)
    }

    fn read_file(&self, name: &str) -> io::Result<Box<dyn Read + Send>> {
        Vfs::read_file(self, name)
    }

    fn sync_file(&self, name: &str) -> io::Result<()> {
        Vfs::sync_file(self, name)
    }

    fn set_read_only(&self, name: &str) -> io::Result<()> {
        Vfs::set_read_only(self, name)
    }

    fn lock_dir(&self, name: &str) -> io::Result<DirLock> {
        Vfs::lock_dir(self, name)
    }

    fn path(&self, name: &str) -> PathBuf {
        Vfs::path(self, name)
    }
}

/// A lock that `Vfs::lock_dir` took. Dropping it releases the lock.
pub struct DirLock {
    _guard: Box<dyn Any + Send + Sync>,
}

impl DirLock {
    /// Return the lock that `guard` holds until it drops.
    pub fn new(guard: impl Any + Send + Sync) -> Self {
        Self {
            _guard: Box::new(guard),
        }
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

