//! The file system helpers of the conversions, the backups and the
//! restores. An error names the path of its file or its directory.

use std::fs::{self, File, TryLockError};
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};

use crate::Error;

/// The size of the buffer of a blob copy or a blob comparison.
pub(crate) const CHUNK: usize = 1 << 16;

/// Create the directory `path` and its missing parents. Sync the parent of
/// each directory that the call creates, so that its entry is durable.
pub(crate) fn create_dirs(path: &Path) -> io::Result<()> {
    match fs::create_dir(path) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::AlreadyExists => return Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => {
            create_dirs(parent(path))?;
            match fs::create_dir(path) {
                Ok(()) => {}
                Err(error) if error.kind() == ErrorKind::AlreadyExists => return Ok(()),
                Err(error) => return Err(error),
            }
        }
        Err(error) => return Err(error),
    }
    File::open(parent(path))?.sync_all()
}

/// Sync the directory `path`, so that its entries are durable.
pub(crate) fn sync_dir(path: &Path) -> Result<(), Error> {
    File::open(path)
        .and_then(|dir| dir.sync_all())
        .map_err(|source| io_error(path, source))
}

/// Return the directory of `path`. A bare name is in the current directory.
pub(crate) fn parent(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

/// Return the path beside `path` whose name is the name of `path` with
/// `suffix` after it.
pub(crate) fn sibling(path: &Path, suffix: &str) -> Result<PathBuf, Error> {
    let Some(name) = path.file_name() else {
        let error = io::Error::new(ErrorKind::InvalidInput, "the path has no file name");
        return Err(io_error(path, error));
    };
    let mut name = name.to_owned();
    name.push(suffix);
    Ok(path.with_file_name(name))
}

/// Tell if something exists at `path`.
pub(crate) fn exists(path: &Path) -> Result<bool, Error> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(error) => Err(io_error(path, error)),
    }
}

/// Take an exclusive `flock` on `file`, which is open at `path`, without a
/// wait. Return the file, which holds the lock until its drop. When another
/// file handle holds the lock, the call fails with `Locked`.
pub(crate) fn try_lock(file: File, path: &Path) -> Result<File, Error> {
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => Err(Error::Locked {
            path: path.to_owned(),
        }),
        Err(TryLockError::Error(source)) => Err(io_error(path, source)),
    }
}

/// Return the `Io` error of `source` at `path`.
pub(crate) fn io_error(path: &Path, source: io::Error) -> Error {
    Error::Io {
        path: path.to_owned(),
        source,
    }
}
