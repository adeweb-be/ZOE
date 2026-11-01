//! The directories of a blob store other than the store directory: the
//! directory that a read-only store reads its blob files in, and a blob
//! directory outside the store directory.

use std::io::{self, Read};
use std::path::PathBuf;
use std::sync::Arc;

use crate::format::blob::BLOB_DIR;
use crate::vfs::{Dir, DirLock, WriteFile, list_dir};

/// A directory that the blob store only reads: the directory that holds
/// the blob directory of a read-only store.
///
/// It reads through `std::fs`, as the read-only open of the store does.
/// Each call that writes, syncs or locks fails with `ReadOnlyFilesystem`.
/// So a read-only open changes no byte and creates nothing there. The
/// directory need not exist.
pub(crate) struct ReadOnlyDir {
    dir: PathBuf,
}

impl ReadOnlyDir {
    /// Return the directory `dir`. This reads nothing and creates nothing.
    pub(crate) fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// Return the error of a call that would change `name`.
    fn refused(&self, name: &str) -> io::Error {
        let message = format!("read-only: {}", self.dir.join(name).display());
        io::Error::new(io::ErrorKind::ReadOnlyFilesystem, message)
    }
}

impl Dir for ReadOnlyDir {
    fn exists(&self, name: &str) -> io::Result<bool> {
        std::fs::exists(self.dir.join(name))
    }

    fn create_new(&self, name: &str) -> io::Result<Box<dyn redb::StorageBackend>> {
        Err(self.refused(name))
    }

    fn open_log(&self, name: &str) -> io::Result<Box<dyn redb::StorageBackend>> {
        Err(self.refused(name))
    }

    fn rename(&self, from: &str, _to: &str) -> io::Result<()> {
        Err(self.refused(from))
    }

    fn hard_link(&self, from: &str, _to: &str) -> io::Result<()> {
        Err(self.refused(from))
    }

    fn remove(&self, name: &str) -> io::Result<()> {
        Err(self.refused(name))
    }

    fn sync_dir_at(&self, dir: &str) -> io::Result<()> {
        Err(self.refused(dir))
    }

    fn create_dir(&self, name: &str) -> io::Result<()> {
        Err(self.refused(name))
    }

    fn list_files(&self, dir: &str) -> io::Result<Vec<String>> {
        list_dir(&self.dir.join(dir), false)
    }

    fn list_entries(&self, dir: &str) -> io::Result<Vec<String>> {
        list_dir(&self.dir.join(dir), true)
    }

    fn create_file(&self, name: &str) -> io::Result<Box<dyn WriteFile>> {
        Err(self.refused(name))
    }

    fn read_file(&self, name: &str) -> io::Result<Box<dyn Read + Send>> {
        Ok(Box::new(std::fs::File::open(self.dir.join(name))?))
    }

    fn sync_file(&self, name: &str) -> io::Result<()> {
        Err(self.refused(name))
    }

    fn set_read_only(&self, name: &str) -> io::Result<()> {
        Err(self.refused(name))
    }

    fn lock_dir(&self, name: &str) -> io::Result<DirLock> {
        Err(self.refused(name))
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }
}

/// The blob directory of a store whose blob files live outside the store
/// directory: the directory `name` in `parent`.
///
/// It maps `blobs` and the names in `blobs/` onto that directory. It maps
/// the store directory `""`, the parent of `blobs`, onto `parent`, so the
/// sync of the parent of `blobs` syncs the directory that holds the blob
/// directory. Any other name stays in the store directory. Every operation
/// goes through the `Vfs` of `parent` or of the store directory, so the
/// crash simulation covers it.
pub(crate) struct Relocated {
    /// The store directory. It holds the lock of the store.
    store: Arc<dyn Dir>,
    /// The directory that holds the blob directory.
    parent: Box<dyn Dir>,
    /// The name of the blob directory in `parent`: one part, no `/`.
    name: String,
}

impl Relocated {
    /// Return the blob directory `name` in `parent`, for the store in the
    /// directory `store`.
    pub(crate) fn new(store: Arc<dyn Dir>, parent: Box<dyn Dir>, name: String) -> Self {
        Self {
            store,
            parent,
            name,
        }
    }

    /// Return the directory that holds the file or directory `name` of the
    /// blob store, and its name there.
    fn locate(&self, name: &str) -> (&dyn Dir, String) {
        if name.is_empty() {
            return (&*self.parent, String::new());
        }
        match name.strip_prefix(BLOB_DIR) {
            Some("") => (&*self.parent, self.name.clone()),
            Some(rest) if rest.starts_with('/') => (&*self.parent, format!("{}{rest}", self.name)),
            _ => (&*self.store, name.to_owned()),
        }
    }
}

impl Dir for Relocated {
    fn exists(&self, name: &str) -> io::Result<bool> {
        let (dir, name) = self.locate(name);
        dir.exists(&name)
    }

    fn create_new(&self, name: &str) -> io::Result<Box<dyn redb::StorageBackend>> {
        let (dir, name) = self.locate(name);
        dir.create_new(&name)
    }

    fn open_log(&self, name: &str) -> io::Result<Box<dyn redb::StorageBackend>> {
        let (dir, name) = self.locate(name);
        dir.open_log(&name)
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        let (dir, from) = self.locate(from);
        let (to_dir, to) = self.locate(to);
        if !std::ptr::addr_eq(dir, to_dir) {
            return Err(io::Error::other(
                "a rename between the store directory and the blob directory",
            ));
        }
        dir.rename(&from, &to)
    }

    fn hard_link(&self, from: &str, to: &str) -> io::Result<()> {
        let (dir, from) = self.locate(from);
        let (to_dir, to) = self.locate(to);
        if !std::ptr::addr_eq(dir, to_dir) {
            return Err(io::Error::other(
                "a hard link between the store directory and the blob directory",
            ));
        }
        dir.hard_link(&from, &to)
    }

    fn remove(&self, name: &str) -> io::Result<()> {
        let (dir, name) = self.locate(name);
        dir.remove(&name)
    }

    fn sync_dir_at(&self, name: &str) -> io::Result<()> {
        let (dir, name) = self.locate(name);
        dir.sync_dir_at(&name)
    }

    fn create_dir(&self, name: &str) -> io::Result<()> {
        let (dir, name) = self.locate(name);
        dir.create_dir(&name)
    }

    fn list_files(&self, name: &str) -> io::Result<Vec<String>> {
        let (dir, name) = self.locate(name);
        dir.list_files(&name)
    }

    fn list_entries(&self, name: &str) -> io::Result<Vec<String>> {
        let (dir, name) = self.locate(name);
        dir.list_entries(&name)
    }

    fn create_file(&self, name: &str) -> io::Result<Box<dyn WriteFile>> {
        let (dir, name) = self.locate(name);
        dir.create_file(&name)
    }

    fn read_file(&self, name: &str) -> io::Result<Box<dyn Read + Send>> {
        let (dir, name) = self.locate(name);
        dir.read_file(&name)
    }

    fn sync_file(&self, name: &str) -> io::Result<()> {
        let (dir, name) = self.locate(name);
        dir.sync_file(&name)
    }

    fn set_read_only(&self, name: &str) -> io::Result<()> {
        let (dir, name) = self.locate(name);
        dir.set_read_only(&name)
    }

    fn lock_dir(&self, name: &str) -> io::Result<DirLock> {
        let (dir, name) = self.locate(name);
        dir.lock_dir(&name)
    }

    fn path(&self, name: &str) -> PathBuf {
        let (dir, name) = self.locate(name);
        dir.path(&name)
    }
}
