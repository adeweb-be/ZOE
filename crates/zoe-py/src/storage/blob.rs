//! The take of a blob file that a client gives to `storeBlob` or
//! `restoreBlob`: by rename from the blob temporary directory, by copy
//! from elsewhere, never from the store.

use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

use zoe_core::{BlobFile, Store, ZoeError};

use super::SharedTxn;
use super::table::Entry;
use crate::errors::Failure;

/// Stage the blob in the file `path` in the transaction of `entry`.
///
/// A file in the blob temporary directory of `store`, or in one of its
/// subdirectories, goes to `at`, which takes it by rename. A file of the
/// store directory `store_dir` or of the blob directory outside that
/// temporary directory, the database or a committed blob file for example,
/// is refused with `Protocol`: the store owns it. Another file is copied
/// into a new blob file of `store`, which goes to `copied`, and then it is
/// removed. A refused call leaves the file in its place.
pub(super) fn take_blob(
    store: &Store,
    store_dir: &Path,
    entry: &Entry,
    path: &Path,
    at: impl FnOnce(&mut SharedTxn, &Path) -> zoe_core::Result<()>,
    copied: impl FnOnce(&mut SharedTxn, BlobFile) -> zoe_core::Result<()>,
) -> Result<(), Failure> {
    match blob_source(store, store_dir, path)? {
        Source::Temporary(temporary) => entry.with(|txn| Ok(at(txn, &temporary)?)),
        Source::Store => {
            Err(ZoeError::Protocol("the blob file is a file of the store, not a new blob").into())
        }
        Source::Elsewhere => {
            let mut file = store.new_blob_file()?;
            io::copy(&mut File::open(path)?, &mut file)?;
            entry.with(|txn| Ok(copied(txn, file)?))?;
            fs::remove_file(path)?;
            Ok(())
        }
    }
}

/// Where a blob file that a client gives is.
enum Source {
    /// In the blob temporary directory or in one of its subdirectories,
    /// with the path that the core gives to the file.
    Temporary(PathBuf),
    /// In the store directory or in the blob directory, outside the blob
    /// temporary directory.
    Store,
    /// Elsewhere.
    Elsewhere,
}

/// Tell where the file `path` is. `store_dir` is the real path of the
/// store directory.
///
/// The directories compare by their real paths, so a path through a
/// symbolic link or a relative path is found too. The name of the file
/// stays as it is: the core refuses a symbolic link.
fn blob_source(store: &Store, store_dir: &Path, path: &Path) -> Result<Source, Failure> {
    let temp = store.blob_temp_dir()?;
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return Ok(Source::Elsewhere);
    };
    let dir = if dir.as_os_str().is_empty() {
        Path::new(".")
    } else {
        dir
    };
    let (Ok(dir), Ok(real_temp)) = (fs::canonicalize(dir), fs::canonicalize(&temp)) else {
        return Ok(Source::Elsewhere);
    };
    if let Ok(rest) = dir.strip_prefix(&real_temp) {
        return Ok(Source::Temporary(temp.join(rest).join(name)));
    }
    let in_blob_dir = real_temp
        .parent()
        .is_some_and(|blobs| dir.starts_with(blobs));
    if in_blob_dir || dir.starts_with(store_dir) {
        return Ok(Source::Store);
    }
    Ok(Source::Elsewhere)
}
