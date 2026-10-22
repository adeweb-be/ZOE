//! The blob files of a transaction: a new blob that a client writes, a
//! synced blob that waits for its commit, and a reader of a committed blob.

use std::io::{self, Read, Write};
use std::sync::{Arc, Weak};

use crate::backend::BlobRecord;
use crate::error::{Result, ZoeError};
use crate::store::Opened;
use crate::types::{Oid, Tid};
use crate::vfs::WriteFile;

/// A new blob, in a temporary file. Write the bytes of the blob, then give
/// the file to `Txn::store_blob` of the same store. Dropping the file
/// removes it.
///
/// The file holds only a weak reference to its store, so it does not keep
/// the store open. After the store is dropped, a write fails with
/// `Closed`, and the next open of the store removes the file.
pub struct BlobFile {
    pub(super) store: Weak<Opened>,
    pub(super) name: String,
    /// The open file. It is `None` after `close`.
    pub(super) file: Option<Box<dyn WriteFile>>,
    pub(super) hasher: blake3::Hasher,
    pub(super) size: u64,
}

impl BlobFile {
    /// Return the number of bytes written to the file.
    pub(crate) fn size(&self) -> u64 {
        self.size
    }

    /// Check that the file belongs to `store`, which is open.
    ///
    /// A file of another store is refused with `Protocol`. Its temporary
    /// name can also exist in `store`, so a rename would move the wrong
    /// file. A file whose store is closed is refused with `Closed`.
    pub(crate) fn check_owner(&self, store: &Arc<Opened>) -> Result<()> {
        match self.store.upgrade() {
            None => Err(ZoeError::Closed),
            Some(own) if !Arc::ptr_eq(&own, store) => {
                Err(ZoeError::Protocol("a blob file of another store"))
            }
            Some(_) => Ok(()),
        }
    }

    /// Sync the file and return the blob, ready for a commit in `store`.
    /// The file must belong to `store`, as `check_owner` checks.
    pub(crate) fn close(mut self, store: &Arc<Opened>) -> Result<PendingBlob> {
        self.check_owner(store)?;
        let mut file = self.file.take().expect("a blob file is closed once");
        file.sync()?;
        drop(file);
        Ok(PendingBlob {
            store: Weak::clone(&self.store),
            temp: Some(std::mem::take(&mut self.name)),
            record: BlobRecord {
                hash: *self.hasher.finalize().as_bytes(),
                size: self.size,
            },
            linked: false,
        })
    }
}

impl Write for BlobFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.store.strong_count() == 0 {
            return Err(io::Error::other(ZoeError::Closed));
        }
        let file = self.file.as_mut().expect("a blob file is open until close");
        let n = file.write(buf)?;
        self.hasher.update(&buf[..n]);
        self.size += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file
            .as_mut()
            .expect("a blob file is open until close")
            .flush()
    }
}

impl Drop for BlobFile {
    fn drop(&mut self) {
        // `close` empties the name. The file then belongs to the blob.
        if !self.name.is_empty() {
            drop(self.file.take());
            if let Some(store) = self.store.upgrade() {
                let _ = store.blobs.dir.remove(&self.name);
            }
        }
    }
}

/// A synced blob that a transaction holds until its finish or its abort.
/// Dropping a blob that `place` did not rename removes its file.
///
/// The blob holds only a weak reference to its store, as a blob file does,
/// so a transaction that shares its store or borrows it holds the same
/// blob.
pub(crate) struct PendingBlob {
    pub(super) store: Weak<Opened>,
    /// The temporary file. It is `None` after `place` renamed it.
    pub(super) temp: Option<String>,
    pub(super) record: BlobRecord,
    /// True when the file is a hard link of a committed blob file.
    pub(super) linked: bool,
}

impl PendingBlob {
    /// Return the blob record of the blob.
    pub(crate) fn record(&self) -> &BlobRecord {
        &self.record
    }
}

impl Drop for PendingBlob {
    fn drop(&mut self) {
        if let Some(temp) = self.temp.take()
            && let Some(store) = self.store.upgrade()
        {
            let _ = store.blobs.dir.remove(&temp);
        }
    }
}

/// A reader of a committed blob.
///
/// It hashes the bytes as it reads them. At the end of the file, it checks
/// the size and the BLAKE3 hash against the blob record. A mismatch fails
/// the read with `InvalidData`, and the error holds `CorruptBlob`. A
/// reader that stops before the end checks nothing.
pub struct BlobReader {
    pub(super) file: Box<dyn Read + Send>,
    pub(super) hasher: blake3::Hasher,
    pub(super) record: BlobRecord,
    /// The number of bytes read.
    pub(super) read: u64,
    pub(super) oid: Oid,
    pub(super) tid: Tid,
}

impl BlobReader {
    fn corrupt(&self) -> io::Error {
        let error = ZoeError::CorruptBlob {
            oid: self.oid,
            tid: self.tid,
        };
        io::Error::new(io::ErrorKind::InvalidData, error)
    }
}

impl Read for BlobReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.file.read(buf)?;
        if n == 0 && !buf.is_empty() {
            let intact = self.read == self.record.size
                && *self.hasher.finalize().as_bytes() == self.record.hash;
            return if intact { Ok(0) } else { Err(self.corrupt()) };
        }
        self.read += n as u64;
        if self.read > self.record.size {
            return Err(self.corrupt());
        }
        self.hasher.update(&buf[..n]);
        Ok(n)
    }
}
