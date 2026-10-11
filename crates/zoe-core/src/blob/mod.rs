//! The blob store: one file per blob, beside the store, in the BushyLayout
//! of ZODB. `format::blob` gives the names.
//!
//! A new blob goes into a temporary file in `blobs/tmp/`. The file
//! computes the BLAKE3 hash of the bytes while they are written.
//! `store_blob` syncs the file. `finish` places the blobs before the
//! backend commit: it writes and syncs an intent file that names the tid
//! and the oids, renames each blob file into its place, and syncs each
//! directory. The blob record holds the hash and the size, in the same
//! backend write transaction as the record. After the commit, `finish`
//! removes the intent file.
//!
//! So a durable blob record always has its file. A crash can leave a blob
//! file without its blob record, but then the intent file is on disk.
//! Opening a store reads only `blobs/tmp/`: it removes the blob files that
//! an intent file names and that have no blob record, then the intent
//! files and the temporary files.
//!
//! Pack uses the same intent files. Before it commits the removal of blob
//! records, it writes and syncs an intent file for each tid of them. After
//! the commit, it removes the blob files, syncs their directories and
//! removes the intent files. So a crash after the commit leaves the intent
//! file, and the next open removes the files.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use crate::backend::{Backend, BlobRecord};
use crate::error::{Result, ZoeError};
use crate::format::blob::{
    BLOB_DIR, LAYOUT, LAYOUT_FILE, NEW_LAYOUT_FILE, TMP_DIR, blob_name, decode_intent,
    encode_intent, intent_name, intent_tid, oid_dir,
};
use crate::store::Opened;
use crate::types::{Oid, Tid};
use crate::vfs::{Vfs, WriteFile, parent};

/// The operations of a `Vfs` that the blob store uses, in a form that a
/// trait object can hold. Every `Vfs` has them.
pub(crate) trait BlobDir: Send + Sync {
    fn exists(&self, name: &str) -> io::Result<bool>;
    fn rename(&self, from: &str, to: &str) -> io::Result<()>;
    fn remove(&self, name: &str) -> io::Result<()>;
    fn sync_dir_at(&self, dir: &str) -> io::Result<()>;
    fn create_dir(&self, name: &str) -> io::Result<()>;
    fn list_files(&self, dir: &str) -> io::Result<Vec<String>>;
    fn create_file(&self, name: &str) -> io::Result<Box<dyn WriteFile>>;
    fn read_file(&self, name: &str) -> io::Result<Box<dyn Read + Send>>;
    fn set_read_only(&self, name: &str) -> io::Result<()>;
    fn path(&self, name: &str) -> PathBuf;
}

impl<V: Vfs> BlobDir for V {
    fn exists(&self, name: &str) -> io::Result<bool> {
        Vfs::exists(self, name)
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        Vfs::rename(self, from, to)
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

    fn create_file(&self, name: &str) -> io::Result<Box<dyn WriteFile>> {
        Vfs::create_file(self, name)
    }

    fn read_file(&self, name: &str) -> io::Result<Box<dyn Read + Send>> {
        Vfs::read_file(self, name)
    }

    fn set_read_only(&self, name: &str) -> io::Result<()> {
        Vfs::set_read_only(self, name)
    }

    fn path(&self, name: &str) -> PathBuf {
        Vfs::path(self, name)
    }
}

/// The blob files of a store.
pub(crate) struct BlobStore {
    /// The store directory. It holds the single-writer lock while the
    /// store is open. Nothing else owns it.
    dir: Box<dyn BlobDir>,
    /// Tells if `blobs/`, `blobs/tmp/` and the layout file are durable.
    ready: Mutex<bool>,
    /// The number of the next temporary file.
    next_temp: AtomicU64,
}

impl BlobStore {
    /// Return the blob store of the store directory `dir`. This reads
    /// nothing: `open` checks the store directory.
    pub(crate) fn new(dir: Box<dyn BlobDir>) -> Self {
        Self {
            dir,
            ready: Mutex::new(false),
            next_temp: AtomicU64::new(0),
        }
    }

    /// Check the blob store when the store opens.
    ///
    /// The layout file must name the bushy layout. Then the leftovers of a
    /// crash in `blobs/tmp/` are removed: the blob files that an intent
    /// file names and that `backend` has no blob record for, the intent
    /// files and the temporary files.
    ///
    /// A read-only store only reads the layout file and changes nothing.
    /// It leaves the leftovers of a crash to the next writer. A blob file
    /// that a crash left without its blob record stays invisible, because
    /// every blob read starts from the blob record.
    pub(crate) fn open(&self, backend: &dyn Backend, read_only: bool) -> Result<()> {
        if self.dir.exists(LAYOUT_FILE)? {
            let mut layout = String::new();
            self.dir
                .read_file(LAYOUT_FILE)?
                .read_to_string(&mut layout)?;
            if layout.trim() != LAYOUT {
                return Err(ZoeError::Format(format!(
                    "blob layout {:?}: only {LAYOUT:?} is supported",
                    layout.trim()
                )));
            }
        }
        if !read_only && self.dir.exists(TMP_DIR)? {
            self.recover(backend)?;
        }
        Ok(())
    }

    /// Remove the leftovers of a crash in `blobs/tmp/`. Only regular
    /// files are removed there.
    fn recover(&self, backend: &dyn Backend) -> Result<()> {
        let mut leftovers = Vec::new();
        let mut intents = Vec::new();
        for file in self.dir.list_files(TMP_DIR)? {
            let name = format!("{TMP_DIR}/{file}");
            if let Some(tid) = intent_tid(&file) {
                let mut bytes = Vec::new();
                self.dir.read_file(&name)?.read_to_end(&mut bytes)?;
                // An intent that does not decode was not synced, so its
                // finish created no directory and renamed no blob file.
                if let Ok(oids) = decode_intent(tid, &bytes) {
                    intents.push((tid, oids));
                }
            }
            leftovers.push(name);
        }

        // A finish creates oid directories only after its intent file is
        // durable. A process that stops between a creation and the sync
        // of the parent leaves a directory that exists but that a power
        // loss can remove. A later finish does not create it again, so
        // the sync happens here. The set gives a parent before its
        // children.
        let mut parents = BTreeSet::new();
        for oid in intents.iter().flat_map(|(_, oids)| oids) {
            for level in oid_levels(*oid) {
                if !self.dir.exists(&level)? {
                    break;
                }
                parents.insert(parent(&level).to_owned());
            }
        }
        for dir in &parents {
            self.dir.sync_dir_at(dir)?;
        }

        let mut orphan_dirs = BTreeSet::new();
        for (tid, oids) in &intents {
            for &oid in oids {
                if has_blob_record(backend, oid, *tid)? {
                    continue;
                }
                let blob = blob_name(oid, *tid);
                if self.dir.exists(&blob)? {
                    self.dir.remove(&blob)?;
                }
                // An open that a crash stopped can have removed the file
                // without a sync of its directory. A power loss then brings
                // the file back. So the directory is synced also when the
                // file is gone.
                let dir = oid_dir(oid);
                if self.dir.exists(&dir)? {
                    orphan_dirs.insert(dir);
                }
            }
        }
        // The removals of the blob files are durable before the removal of
        // their intent file.
        for dir in &orphan_dirs {
            self.dir.sync_dir_at(dir)?;
        }
        for name in &leftovers {
            self.dir.remove(name)?;
        }
        if !leftovers.is_empty() {
            self.dir.sync_dir_at(TMP_DIR)?;
        }
        Ok(())
    }

    /// Create `blobs/`, `blobs/tmp/` and the layout file when they do not
    /// exist, and make them durable.
    ///
    /// A process that stops before a sync can leave these entries in a
    /// state that exists but is not durable. So the parents are synced
    /// also when the entries exist.
    fn make_ready(&self) -> Result<()> {
        let mut ready = self.ready.lock().unwrap_or_else(|e| e.into_inner());
        if *ready {
            return Ok(());
        }
        if !self.dir.exists(BLOB_DIR)? {
            self.dir.create_dir(BLOB_DIR)?;
        }
        self.dir.sync_dir_at(parent(BLOB_DIR))?;
        if !self.dir.exists(TMP_DIR)? {
            self.dir.create_dir(TMP_DIR)?;
        }
        if !self.dir.exists(LAYOUT_FILE)? {
            // The layout file is renamed into place, so a crash cannot
            // leave a partial one.
            if self.dir.exists(NEW_LAYOUT_FILE)? {
                self.dir.remove(NEW_LAYOUT_FILE)?;
            }
            let mut file = self.dir.create_file(NEW_LAYOUT_FILE)?;
            file.write_all(LAYOUT.as_bytes())?;
            file.sync()?;
            drop(file);
            self.dir.rename(NEW_LAYOUT_FILE, LAYOUT_FILE)?;
        }
        self.dir.sync_dir_at(BLOB_DIR)?;
        *ready = true;
        Ok(())
    }

    /// Create the directory `dir` when it does not exist, and make it
    /// durable. Its parent must exist and be durable. A directory that
    /// exists is durable: `make_ready` and `recover` sync the ones that a
    /// stopped process can leave.
    fn create_dir(&self, dir: &str) -> Result<()> {
        if !self.dir.exists(dir)? {
            self.dir.create_dir(dir)?;
            self.dir.sync_dir_at(parent(dir))?;
        }
        Ok(())
    }

    /// Return a new temporary file for a blob of the store `store`.
    pub(crate) fn new_file(&self, store: Weak<Opened>) -> Result<BlobFile> {
        self.make_ready()?;
        let n = self.next_temp.fetch_add(1, Ordering::Relaxed);
        let name = format!("{TMP_DIR}/{n}.tmp");
        let file = self.dir.create_file(&name)?;
        Ok(BlobFile {
            store,
            name,
            file: Some(file),
            hasher: blake3::Hasher::new(),
            size: 0,
        })
    }

    /// Place the blobs of the finish `tid` before the backend commit, and
    /// return the name of the intent file.
    ///
    /// The intent file is durable before the first rename. Each blob file
    /// becomes read-only, then it is renamed into its place, then its
    /// directory is synced. So each blob file is durable when this
    /// returns.
    pub(crate) fn place(
        &self,
        tid: Tid,
        blobs: &mut BTreeMap<Oid, PendingBlob>,
    ) -> Result<Option<String>> {
        if blobs.is_empty() {
            return Ok(None);
        }
        let intent = intent_name(tid);
        let oids: Vec<Oid> = blobs.keys().copied().collect();
        let mut file = self.dir.create_file(&intent)?;
        file.write_all(&encode_intent(tid, &oids))?;
        file.sync()?;
        drop(file);
        self.dir.sync_dir_at(TMP_DIR)?;

        for (&oid, blob) in blobs.iter_mut() {
            let dir = oid_dir(oid);
            // Each level of the oid directory is durable before the next
            // one is created.
            for level in oid_levels(oid) {
                self.create_dir(&level)?;
            }
            let temp = blob.temp.as_deref().expect("a blob is placed once");
            self.dir.set_read_only(temp)?;
            self.dir.rename(temp, &blob_name(oid, tid))?;
            blob.temp = None;
            self.dir.sync_dir_at(&dir)?;
        }
        Ok(Some(intent))
    }

    /// Remove the intent file of a finish after its backend commit, or of
    /// a pack after its blob files are gone. A failure leaves the file for
    /// the next open, which keeps the blob files that have their blob
    /// record.
    pub(crate) fn finished(&self, intent: Option<String>) {
        if let Some(intent) = intent {
            let _ = self.dir.remove(&intent);
        }
    }

    /// Write the intent file of `tid` before a pack commits the removal of
    /// the blob records of `oids` at `tid`, and make it durable. Return
    /// its name.
    ///
    /// An intent file of `tid` that exists keeps its oids: the new file
    /// names them too. The new file goes in under a temporary name and
    /// replaces the old one by a rename, so a crash leaves one of the two.
    pub(crate) fn intend_removal(&self, tid: Tid, oids: &[Oid]) -> Result<String> {
        self.make_ready()?;
        let intent = intent_name(tid);
        let mut named: BTreeSet<Oid> = oids.iter().copied().collect();
        if self.dir.exists(&intent)? {
            let mut bytes = Vec::new();
            self.dir.read_file(&intent)?.read_to_end(&mut bytes)?;
            // An intent that does not decode was not synced, so it names
            // nothing that a commit removed.
            if let Ok(old) = decode_intent(tid, &bytes) {
                named.extend(old);
            }
        }
        let named: Vec<Oid> = named.into_iter().collect();
        let n = self.next_temp.fetch_add(1, Ordering::Relaxed);
        let temp = format!("{TMP_DIR}/{n}.tmp");
        let mut file = self.dir.create_file(&temp)?;
        file.write_all(&encode_intent(tid, &named))?;
        file.sync()?;
        drop(file);
        self.dir.rename(&temp, &intent)?;
        self.dir.sync_dir_at(TMP_DIR)?;
        Ok(intent)
    }

    /// Remove the blob files of `blobs` after a pack made the removal of
    /// their blob records durable, and sync their directories. A file that
    /// is not there is not an error.
    ///
    /// The directory of a file that is not there is synced too, when it
    /// exists. A killed process can remove a file and stop before the
    /// sync, and then the removal is not durable.
    pub(crate) fn remove_files(&self, blobs: &[(Oid, Tid)]) -> Result<()> {
        let mut dirs = BTreeSet::new();
        for &(oid, tid) in blobs {
            let dir = oid_dir(oid);
            match self.dir.remove(&blob_name(oid, tid)) {
                Ok(()) => {
                    dirs.insert(dir);
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    if self.dir.exists(&dir)? {
                        dirs.insert(dir);
                    }
                }
                Err(e) => return Err(e.into()),
            }
        }
        for dir in &dirs {
            self.dir.sync_dir_at(dir)?;
        }
        Ok(())
    }

    /// Open the blob file of `oid` at `tid`, whose blob record is
    /// `record`. A missing file is a corrupt blob.
    pub(crate) fn reader(&self, oid: Oid, tid: Tid, record: BlobRecord) -> Result<BlobReader> {
        let file = match self.dir.read_file(&blob_name(oid, tid)) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(ZoeError::CorruptBlob { oid, tid });
            }
            Err(e) => return Err(e.into()),
        };
        Ok(BlobReader {
            file,
            hasher: blake3::Hasher::new(),
            record,
            read: 0,
            oid,
            tid,
        })
    }

    /// Return the path of the blob file of `oid` at `tid`.
    pub(crate) fn path(&self, oid: Oid, tid: Tid) -> PathBuf {
        self.dir.path(&blob_name(oid, tid))
    }
}

/// Return the 8 levels of the directory of `oid`, from the top down:
/// `blobs/0xAA`, `blobs/0xAA/0xBB`, and so on to `oid_dir(oid)`.
fn oid_levels(oid: Oid) -> Vec<String> {
    let dir = oid_dir(oid);
    let ends = dir.match_indices('/').skip(1).map(|(end, _)| end);
    ends.chain([dir.len()])
        .map(|end| dir[..end].to_owned())
        .collect()
}

/// Tell if `backend` has the blob record of `oid` at `tid`. A damaged blob
/// record is a blob record: its commit is durable.
fn has_blob_record(backend: &dyn Backend, oid: Oid, tid: Tid) -> Result<bool> {
    match backend.load_blob(oid, tid) {
        Ok(found) => Ok(found.is_some()),
        Err(ZoeError::CorruptBlob { .. } | ZoeError::Format(_)) => Ok(true),
        Err(e) => Err(e),
    }
}

/// A new blob, in a temporary file. Write the bytes of the blob, then give
/// the file to `Txn::store_blob` of the same store. Dropping the file
/// removes it.
///
/// The file holds only a weak reference to its store, so it does not keep
/// the store open. After the store is dropped, a write fails with
/// `Closed`, and the next open of the store removes the file.
pub struct BlobFile {
    store: Weak<Opened>,
    name: String,
    /// The open file. It is `None` after `close`.
    file: Option<Box<dyn WriteFile>>,
    hasher: blake3::Hasher,
    size: u64,
}

impl BlobFile {
    /// Sync the file and return the blob, ready for a commit in `store`.
    ///
    /// A file of another store is refused with `Protocol`. Its temporary
    /// name can also exist in `store`, so a rename would move the wrong
    /// file. A file whose store is closed is refused with `Closed`.
    pub(crate) fn close(mut self, store: &Arc<Opened>) -> Result<PendingBlob> {
        match self.store.upgrade() {
            None => return Err(ZoeError::Closed),
            Some(own) if !Arc::ptr_eq(&own, store) => {
                return Err(ZoeError::Protocol("a blob file of another store"));
            }
            Some(_) => {}
        }
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
    store: Weak<Opened>,
    /// The temporary file. It is `None` after `place` renamed it.
    temp: Option<String>,
    record: BlobRecord,
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
    file: Box<dyn Read + Send>,
    hasher: blake3::Hasher,
    record: BlobRecord,
    /// The number of bytes read.
    read: u64,
    oid: Oid,
    tid: Tid,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use crate::vfs::{CrashImage, MemVfs};

    const PICKLE: &[u8] = b"pickle";

    /// Return a new blob file of `store` that holds `bytes`.
    fn blob_file(store: &Store, bytes: &[u8]) -> BlobFile {
        let mut file = store.new_blob_file().unwrap();
        file.write_all(bytes).unwrap();
        file
    }

    /// Commit a blob that holds `bytes` for each oid of `oids`.
    fn commit_blobs(store: &Store, oids: &[Oid], bytes: &[u8]) -> Tid {
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        for &oid in oids {
            txn.store_blob(oid, Tid::ZERO, PICKLE, blob_file(store, bytes))
                .unwrap();
        }
        txn.vote().unwrap();
        txn.finish().unwrap()
    }

    fn read(store: &Store, oid: Oid, tid: Tid) -> io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        store.open_blob(oid, tid).unwrap().read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    /// Return the names of the files in `blobs/tmp/`.
    fn temp_files(vfs: &MemVfs) -> Vec<String> {
        vfs.names()
            .into_iter()
            .filter(|name| name.starts_with("blobs/tmp/"))
            .collect()
    }

    #[test]
    fn one_commit_holds_several_blobs() {
        let vfs = MemVfs::new();
        let store = Store::open_vfs(vfs.clone()).unwrap();
        let oids = [store.new_oid().unwrap(), store.new_oid().unwrap()];
        let tid = commit_blobs(&store, &oids, b"two");
        for oid in oids {
            assert_eq!(read(&store, oid, tid).unwrap(), b"two");
        }
        assert!(temp_files(&vfs).is_empty());
    }

    #[test]
    fn a_store_without_blobs_has_no_blob_directory() {
        let vfs = MemVfs::new();
        let store = Store::open_vfs(vfs.clone()).unwrap();
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        let oid = store.new_oid().unwrap();
        txn.store(oid, Tid::ZERO, PICKLE).unwrap();
        txn.vote().unwrap();
        let tid = txn.finish().unwrap();
        assert!(!Vfs::exists(&vfs, BLOB_DIR).unwrap());
        assert!(matches!(
            store.open_blob(oid, tid),
            Err(ZoeError::NotFound { .. })
        ));
        assert!(matches!(
            store.blob_path(oid, tid),
            Err(ZoeError::NotFound { .. })
        ));
    }

    #[test]
    fn an_unused_blob_file_is_removed() {
        let vfs = MemVfs::new();
        let store = Store::open_vfs(vfs.clone()).unwrap();
        drop(blob_file(&store, b"dropped"));
        assert!(temp_files(&vfs).is_empty());

        let oid = store.new_oid().unwrap();
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.store_blob(oid, Tid::ZERO, PICKLE, blob_file(&store, b"voted"))
            .unwrap();
        txn.vote().unwrap();
        assert_eq!(temp_files(&vfs).len(), 1);
        txn.abort();
        assert!(temp_files(&vfs).is_empty());
        assert!(vfs.names().iter().all(|name| !name.ends_with(".blob")));
    }

    #[test]
    fn a_later_store_keeps_the_blob_of_its_oid() {
        let vfs = MemVfs::new();
        let store = Store::open_vfs(vfs.clone()).unwrap();
        let oid = store.new_oid().unwrap();
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.store_blob(oid, Tid::ZERO, PICKLE, blob_file(&store, b"blob"))
            .unwrap();
        txn.store(oid, Tid::ZERO, b"later").unwrap();
        txn.vote().unwrap();
        let tid = txn.finish().unwrap();
        assert_eq!(store.load(oid).unwrap(), (b"later".to_vec(), tid));
        assert_eq!(read(&store, oid, tid).unwrap(), b"blob");
        assert!(temp_files(&vfs).is_empty());
    }

    #[test]
    fn a_blob_survives_a_resolved_conflict() {
        let vfs = MemVfs::new();
        let store = Store::open_vfs(vfs.clone()).unwrap();
        let oid = store.new_oid().unwrap();
        let first = commit_blobs(&store, &[oid], b"first");

        // This transaction read `first`, then another one commits.
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.store_blob(oid, first, b"mine", blob_file(&store, b"mine"))
            .unwrap();
        let other = {
            let mut other = store.tpc_begin(b"", b"", b"").unwrap();
            other
                .store_blob(oid, first, b"other", blob_file(&store, b"other"))
                .unwrap();
            other.vote().unwrap();
            other.finish().unwrap()
        };

        let conflicts = txn.vote_resolving().unwrap();
        assert_eq!(conflicts.len(), 1);
        assert_eq!((conflicts[0].0, conflicts[0].1.tid), (oid, other));
        assert_eq!(temp_files(&vfs).len(), 1, "the blob survives the vote");
        txn.store(oid, other, b"resolved").unwrap();
        txn.vote().unwrap();
        let tid = txn.finish().unwrap();
        assert_eq!(store.load(oid).unwrap(), (b"resolved".to_vec(), tid));
        assert_eq!(read(&store, oid, tid).unwrap(), b"mine");
        assert_eq!(read(&store, oid, other).unwrap(), b"other");
        assert!(temp_files(&vfs).is_empty());
    }

    #[test]
    fn store_blob_from_another_thread_is_refused() {
        let vfs = MemVfs::new();
        let store = Store::open_vfs(vfs.clone()).unwrap();
        let oid = store.new_oid().unwrap();
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        let file = blob_file(&store, b"blob");
        let refused = std::thread::scope(|scope| {
            scope
                .spawn(|| txn.store_blob(oid, Tid::ZERO, PICKLE, file))
                .join()
                .unwrap()
        });
        assert!(matches!(refused, Err(ZoeError::WrongThread)), "{refused:?}");
        assert!(txn.staged(oid).is_none());
        assert!(temp_files(&vfs).is_empty());
    }

    #[test]
    fn the_last_share_of_a_store_removes_the_staged_blob_files() {
        let vfs = MemVfs::new();
        let store = std::sync::Arc::new(Store::open_vfs(vfs.clone()).unwrap());
        let oid = store.new_oid().unwrap();
        let mut txn = crate::commit::Txn::begin(std::sync::Arc::clone(&store), b"", b"", b"");
        txn.store_blob(oid, Tid::ZERO, PICKLE, blob_file(&store, b"blob"))
            .unwrap();
        // The transaction holds the last share of the store.
        drop(store);
        assert_eq!(temp_files(&vfs).len(), 1);
        drop(txn);
        assert!(temp_files(&vfs).is_empty());
    }

    #[test]
    fn a_transaction_that_shares_its_store_commits_a_blob() {
        let store = std::sync::Arc::new(Store::open_vfs(MemVfs::new()).unwrap());
        let oid = store.new_oid().unwrap();
        let mut txn = crate::commit::Txn::begin(std::sync::Arc::clone(&store), b"", b"", b"");
        txn.store_blob(oid, Tid::ZERO, PICKLE, blob_file(&store, b"shared"))
            .unwrap();
        txn.vote().unwrap();
        let tid = txn.finish().unwrap();
        assert_eq!(read(&store, oid, tid).unwrap(), b"shared");
    }

    #[test]
    fn a_blob_file_of_another_size_fails_the_read() {
        let vfs = MemVfs::new();
        let store = Store::open_vfs(vfs.clone()).unwrap();
        let oid = store.new_oid().unwrap();
        let tid = commit_blobs(&store, &[oid], b"blob");
        let name = blob_name(oid, tid);

        for bytes in [&b"blob+"[..], b"blo", b""] {
            Vfs::remove(&vfs, &name).unwrap();
            let mut file = Vfs::create_file(&vfs, &name).unwrap();
            file.write_all(bytes).unwrap();
            drop(file);
            let error = read(&store, oid, tid).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{bytes:?}");
            let inner = error.into_inner().unwrap().downcast::<ZoeError>().unwrap();
            assert!(matches!(*inner, ZoeError::CorruptBlob { .. }), "{bytes:?}");
        }
    }

    #[test]
    fn a_missing_blob_file_is_a_corrupt_blob() {
        let vfs = MemVfs::new();
        let store = Store::open_vfs(vfs.clone()).unwrap();
        let oid = store.new_oid().unwrap();
        let tid = commit_blobs(&store, &[oid], b"blob");
        Vfs::remove(&vfs, &blob_name(oid, tid)).unwrap();
        for result in [
            store.open_blob(oid, tid).map(drop),
            store.verify_blob(oid, tid),
        ] {
            assert!(
                matches!(result, Err(ZoeError::CorruptBlob { oid: o, tid: t }) if o == oid && t == tid),
                "{result:?}"
            );
        }
    }

    #[test]
    fn opening_removes_only_the_blob_files_without_a_record() {
        let (vfs, leftovers) = leftovers_of_a_crash();
        let store = Store::open_vfs(vfs.clone()).unwrap();
        assert!(!Vfs::exists(&vfs, &leftovers.orphan).unwrap());
        assert_eq!(read(&store, leftovers.oid, leftovers.tid).unwrap(), b"kept");
        assert!(temp_files(&vfs).is_empty());
    }

    /// What `leftovers_of_a_crash` leaves.
    struct Leftovers {
        /// An oid with a committed blob that holds `kept`.
        oid: Oid,
        /// The tid of that blob.
        tid: Tid,
        /// The name of a blob file that has no blob record.
        orphan: String,
    }

    /// Return a store with a committed blob, and the leftovers of a crash:
    /// a blob file that a finish placed before its backend commit, with
    /// its intent file, the intent file of a finish that committed, and a
    /// temporary file.
    fn leftovers_of_a_crash() -> (MemVfs, Leftovers) {
        let vfs = MemVfs::new();
        let (oid, orphan, tid) = {
            let store = Store::open_vfs(vfs.clone()).unwrap();
            let oid = store.new_oid().unwrap();
            let orphan = store.new_oid().unwrap();
            let tid = commit_blobs(&store, &[oid], b"kept");
            (oid, orphan, tid)
        };
        let crashed = Tid(tid.0 + 1);
        for level in oid_levels(orphan) {
            if !Vfs::exists(&vfs, &level).unwrap() {
                Vfs::create_dir(&vfs, &level).unwrap();
            }
        }
        for (name, bytes) in [
            (blob_name(orphan, crashed), b"orphan".to_vec()),
            (intent_name(crashed), encode_intent(crashed, &[orphan])),
            (intent_name(tid), encode_intent(tid, &[oid])),
            (format!("{TMP_DIR}/7.tmp"), b"temporary".to_vec()),
        ] {
            let mut file = Vfs::create_file(&vfs, &name).unwrap();
            file.write_all(&bytes).unwrap();
        }
        let orphan = blob_name(orphan, crashed);
        (vfs, Leftovers { oid, tid, orphan })
    }

    #[test]
    fn a_pack_syncs_the_directory_of_a_blob_file_that_is_gone() {
        let vfs = MemVfs::new();
        let (oid, tid) = {
            let store = Store::open_vfs(vfs.clone()).unwrap();
            let oid = store.new_oid().unwrap();
            (oid, commit_blobs(&store, &[oid], b"blob"))
        };
        let vfs = vfs.durable_copy();
        let store = Store::open_vfs(vfs.clone()).unwrap();
        // A killed process removed the file and did not sync its
        // directory.
        Vfs::remove(&vfs, &blob_name(oid, tid)).unwrap();
        store.blobs().remove_files(&[(oid, tid)]).unwrap();
        vfs.mark("synced");

        for image in vfs.crash_images().iter().filter(|i| i.is_after("synced")) {
            let names = image.vfs().names();
            assert!(!names.contains(&blob_name(oid, tid)), "{image:?}");
        }
    }

    #[test]
    fn a_removal_intent_keeps_the_oids_of_an_older_intent() {
        let vfs = MemVfs::new();
        let store = Store::open_vfs(vfs.clone()).unwrap();
        let tid = Tid(7);
        let first = store.blobs().intend_removal(tid, &[Oid(2)]).unwrap();
        let second = store
            .blobs()
            .intend_removal(tid, &[Oid(1), Oid(3)])
            .unwrap();
        assert_eq!(first, second);
        let mut bytes = Vec::new();
        Vfs::read_file(&vfs, &second)
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        assert_eq!(
            decode_intent(tid, &bytes).unwrap(),
            [Oid(1), Oid(2), Oid(3)]
        );
        // The temporary files are gone: each one became the intent file.
        assert_eq!(temp_files(&vfs), [second]);
    }

    #[test]
    fn a_torn_intent_is_removed() {
        let vfs = MemVfs::new();
        let tid = {
            let store = Store::open_vfs(vfs.clone()).unwrap();
            let oid = store.new_oid().unwrap();
            commit_blobs(&store, &[oid], b"blob")
        };
        let crashed = Tid(tid.0 + 1);
        let bytes = encode_intent(crashed, &[Oid(9)]);
        let mut file = Vfs::create_file(&vfs, &intent_name(crashed)).unwrap();
        file.write_all(&bytes[..bytes.len() / 2]).unwrap();
        drop(file);

        Store::open_vfs(vfs.clone()).unwrap();
        assert!(temp_files(&vfs).is_empty());
    }

    #[test]
    fn a_layout_other_than_bushy_is_refused() {
        let vfs = MemVfs::new();
        drop(Store::open_vfs(vfs.clone()).unwrap());
        Vfs::create_dir(&vfs, BLOB_DIR).unwrap();
        let mut file = Vfs::create_file(&vfs, LAYOUT_FILE).unwrap();
        file.write_all(b"lawn").unwrap();
        drop(file);
        assert!(matches!(Store::open_vfs(vfs), Err(ZoeError::Format(_))));
    }

    /// The crash simulation reaches the states that the recovery handles:
    /// a blob file in its place without its blob record.
    #[test]
    fn a_crash_can_leave_a_placed_blob_without_its_record() {
        let vfs = MemVfs::new();
        let (oid, tid) = {
            let store = Store::open_vfs(vfs.clone()).unwrap();
            let oid = store.new_oid().unwrap();
            (oid, commit_blobs(&store, &[oid], b"blob"))
        };
        let name = blob_name(oid, tid);
        let orphans = vfs
            .crash_images()
            .iter()
            .filter(|image| {
                let image_vfs = image.vfs();
                if !Vfs::exists(&image_vfs, &name).unwrap() {
                    return false;
                }
                let store = Store::open_vfs(image_vfs.clone()).unwrap();
                let orphan = store.load(oid).is_err();
                if orphan {
                    assert!(!Vfs::exists(&image_vfs, &name).unwrap(), "{image:?}");
                }
                orphan
            })
            .count();
        assert!(orphans > 0);
    }

    /// Return a filesystem with an empty store, all durable.
    fn empty_store() -> MemVfs {
        let vfs = MemVfs::new();
        drop(Store::open_vfs(vfs.clone()).unwrap());
        vfs.durable_copy()
    }

    /// Open each crash image of `vfs`. A blob record must have its file,
    /// and each image after the mark `acknowledged` must have the blob
    /// `bytes` of `oid` at `tid`.
    fn check_images(vfs: &MemVfs, oid: Oid, tid: Tid, bytes: &[u8]) {
        for image in vfs.crash_images() {
            let image_vfs = image.vfs();
            let store = Store::open_vfs(image_vfs.clone())
                .unwrap_or_else(|e| panic!("{image:?} does not open: {e}"));
            assert_no_orphan(&store, &image_vfs, &image);
            match store.load(oid) {
                Ok((_, found)) => {
                    assert_eq!(found, tid, "{image:?}");
                    let mut read = Vec::new();
                    let result = store
                        .open_blob(oid, tid)
                        .and_then(|mut blob| Ok(blob.read_to_end(&mut read)?));
                    assert!(result.is_ok(), "{image:?} lost a blob file: {result:?}");
                    assert_eq!(read, bytes, "{image:?}");
                }
                Err(ZoeError::NotFound { .. }) => {
                    assert!(!image.is_after("acknowledged"), "{image:?} lost a blob");
                }
                Err(e) => panic!("{image:?}: {e}"),
            }
        }
    }

    /// Return the oid and the tid of the blob file `name`, or `None` when
    /// `name` is not the name of a blob file.
    fn blob_of(name: &str) -> Option<(Oid, Tid)> {
        let parts: Vec<&str> = name.strip_prefix("blobs/")?.split('/').collect();
        let [bytes @ .., file] = &parts[..] else {
            return None;
        };
        if bytes.len() != 8 {
            return None;
        }
        let mut oid = 0;
        for byte in bytes {
            oid = oid << 8 | u64::from(u8::from_str_radix(byte.strip_prefix("0x")?, 16).ok()?);
        }
        let tid = file.strip_prefix("0x")?.strip_suffix(".blob")?;
        Some((Oid(oid), Tid(u64::from_str_radix(tid, 16).ok()?)))
    }

    /// Check that every blob file of `vfs` has its blob record in `store`.
    fn assert_no_orphan(store: &Store, vfs: &MemVfs, image: &CrashImage) {
        for name in vfs.names().iter().filter(|name| name.ends_with(".blob")) {
            let (oid, tid) =
                blob_of(name).unwrap_or_else(|| panic!("{image:?}: {name} is no blob file"));
            assert!(
                store.blob_path(oid, tid).is_ok(),
                "{image:?} kept {name}, which has no blob record"
            );
        }
    }

    /// No crash image of a blob commit keeps a blob file without its blob
    /// record after an open.
    #[test]
    fn every_crash_image_of_a_blob_commit_opens_without_an_orphan() {
        let vfs = empty_store();
        let store = Store::open_vfs(vfs.clone()).unwrap();
        let oid = store.new_oid().unwrap();
        let tid = commit_blobs(&store, &[oid], b"blob");
        vfs.mark("acknowledged");
        drop(store);
        check_images(&vfs, oid, tid, b"blob");
    }

    /// A finish dies after it places its blob file. The first open
    /// removes the file and dies at the sync of its directory. A second
    /// open finds the intent file but no blob file. A power loss after it
    /// must not bring back a blob file that no record names.
    #[test]
    fn a_killed_recovery_then_an_open_then_a_power_loss() {
        let built = MemVfs::new();
        let oid = {
            let store = Store::open_vfs(built.clone()).unwrap();
            let oid = store.new_oid().unwrap();
            commit_blobs(&store, &[oid], b"one");
            oid
        };
        let vfs = built.durable_copy();
        let dir = oid_dir(oid);

        let store = Store::open_vfs(vfs.clone()).unwrap();
        let serial = store.load(oid).unwrap().1;
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.store_blob(oid, serial, PICKLE, blob_file(&store, b"two"))
            .unwrap();
        txn.vote().unwrap();
        vfs.kill_at_dir_sync(&dir);
        assert!(txn.finish().is_err());
        std::mem::forget(store);
        vfs.restart();

        vfs.kill_at_dir_sync(&dir);
        assert!(Store::open_vfs(vfs.clone()).is_err());
        vfs.restart();
        drop(Store::open_vfs(vfs.clone()).unwrap());
        vfs.mark("opened");

        for image in vfs.crash_images() {
            if !image.is_after("opened") {
                continue;
            }
            let image_vfs = image.vfs();
            let store = Store::open_vfs(image_vfs.clone()).unwrap();
            assert_no_orphan(&store, &image_vfs, &image);
        }
    }

    /// A process dies after it creates `blobs/` and before it syncs the
    /// store directory. The next process finds `blobs/` and must still
    /// make it durable.
    #[test]
    fn a_blob_directory_of_a_killed_process_becomes_durable() {
        let vfs = empty_store();
        let store = Store::open_vfs(vfs.clone()).unwrap();
        vfs.kill_at_dir_sync("");
        assert!(store.new_blob_file().is_err());
        // A killed process runs no destructor.
        std::mem::forget(store);
        vfs.restart();
        assert!(Vfs::exists(&vfs, BLOB_DIR).unwrap());

        let store = Store::open_vfs(vfs.clone()).unwrap();
        let oid = store.new_oid().unwrap();
        let tid = commit_blobs(&store, &[oid], b"blob");
        vfs.mark("acknowledged");
        drop(store);
        check_images(&vfs, oid, tid, b"blob");
    }

    /// A finish dies after it creates the second level of an oid
    /// directory and before it syncs the first level. The next process
    /// finds the level and must still make it durable.
    #[test]
    fn an_oid_directory_of_a_killed_finish_becomes_durable() {
        let vfs = empty_store();
        let store = Store::open_vfs(vfs.clone()).unwrap();
        let oid = store.new_oid().unwrap();
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.store_blob(oid, Tid::ZERO, PICKLE, blob_file(&store, b"lost"))
            .unwrap();
        txn.vote().unwrap();
        vfs.kill_at_dir_sync(&oid_levels(oid)[0]);
        assert!(txn.finish().is_err());
        std::mem::forget(store);
        vfs.restart();
        assert!(Vfs::exists(&vfs, &oid_levels(oid)[1]).unwrap());

        let store = Store::open_vfs(vfs.clone()).unwrap();
        let tid = commit_blobs(&store, &[oid], b"blob");
        vfs.mark("acknowledged");
        drop(store);
        check_images(&vfs, oid, tid, b"blob");
    }

    /// A crash while an open removes an orphan must leave its intent file
    /// while the orphan is there. Then the next open removes the orphan.
    #[test]
    fn a_crash_during_recovery_leaves_no_orphan_behind() {
        let (vfs, leftovers) = leftovers_of_a_crash();
        // The leftovers are on disk when the recovery starts.
        let vfs = vfs.durable_copy();
        drop(Store::open_vfs(vfs.clone()).unwrap());

        let images = vfs.crash_images();
        assert!(images.len() > 1);
        for image in images {
            let image_vfs = image.vfs();
            let store = Store::open_vfs(image_vfs.clone()).unwrap();
            let kept = read(&store, leftovers.oid, leftovers.tid);
            assert_eq!(kept.ok().as_deref(), Some(&b"kept"[..]), "{image:?}");
            assert_no_orphan(&store, &image_vfs, &image);
            assert!(
                !Vfs::exists(&image_vfs, &leftovers.orphan).unwrap(),
                "{image:?} kept an orphan"
            );
        }
    }

    #[test]
    fn a_blob_file_of_another_store_is_refused() {
        let (vfs_a, vfs_b) = (MemVfs::new(), MemVfs::new());
        let a = Store::open_vfs(vfs_a.clone()).unwrap();
        let b = Store::open_vfs(vfs_b.clone()).unwrap();
        // Both temporary files have the same name, each in its own store.
        let own = blob_file(&b, b"own");
        let other = blob_file(&a, b"other");
        assert_eq!(temp_files(&vfs_a), temp_files(&vfs_b));

        let oid = b.new_oid().unwrap();
        let mut txn = b.tpc_begin(b"", b"", b"").unwrap();
        let refused = txn.store_blob(oid, Tid::ZERO, PICKLE, other);
        assert!(matches!(refused, Err(ZoeError::Protocol(_))), "{refused:?}");
        assert!(temp_files(&vfs_a).is_empty());
        assert_eq!(temp_files(&vfs_b).len(), 1);
        drop(own);
        assert!(temp_files(&vfs_b).is_empty());
    }

    #[test]
    fn a_blob_file_does_not_keep_its_store_open() {
        let dir = std::env::temp_dir().join(format!("zoe-blob-closed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(&dir).unwrap();
        let mut file = store.new_blob_file().unwrap();
        file.write_all(b"before").unwrap();
        drop(store);

        // The lock of the store is free, so the store opens again.
        let store = Store::open(&dir).unwrap();
        let error = file.write_all(b"after").unwrap_err();
        let inner = error.into_inner().unwrap().downcast::<ZoeError>().unwrap();
        assert!(matches!(*inner, ZoeError::Closed), "{inner:?}");

        let oid = store.new_oid().unwrap();
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        let refused = txn.store_blob(oid, Tid::ZERO, PICKLE, file);
        assert!(matches!(refused, Err(ZoeError::Closed)), "{refused:?}");
        drop(txn);
        drop(store);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Return every directory and file under `dir`, with the bytes of each
    /// file.
    fn tree(dir: &std::path::Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
        let mut found = BTreeMap::new();
        let mut todo = vec![dir.to_owned()];
        while let Some(dir) = todo.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    found.insert(path.clone(), None);
                    todo.push(path);
                } else {
                    let bytes = std::fs::read(&path).unwrap();
                    found.insert(path, Some(bytes));
                }
            }
        }
        found
    }

    /// A read-only open of a store with the leftovers of a crash reads the
    /// committed blobs and changes no file under `blobs/`.
    #[test]
    fn a_read_only_open_changes_nothing_under_blobs() {
        let dir = std::env::temp_dir().join(format!("zoe-blob-read-only-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (oid, tid, orphan) = {
            let store = Store::open(&dir).unwrap();
            let oid = store.new_oid().unwrap();
            let orphan = store.new_oid().unwrap();
            let tid = commit_blobs(&store, &[oid], b"kept");
            (oid, tid, orphan)
        };

        // A finish that placed its blob file and stopped before its backend
        // commit, with its intent file, and a temporary file.
        let crashed = Tid(tid.0 + 1);
        std::fs::create_dir_all(dir.join(oid_dir(orphan))).unwrap();
        for (name, bytes) in [
            (blob_name(orphan, crashed), b"orphan".to_vec()),
            (intent_name(crashed), encode_intent(crashed, &[orphan])),
            (format!("{TMP_DIR}/7.tmp"), b"temporary".to_vec()),
        ] {
            std::fs::write(dir.join(name), bytes).unwrap();
        }
        let before = tree(&dir.join(BLOB_DIR));

        let store = Store::open_read_only(&dir).unwrap();
        assert_eq!(read(&store, oid, tid).unwrap(), b"kept");
        let orphan_blob = store.open_blob(orphan, crashed);
        assert!(matches!(orphan_blob, Err(ZoeError::NotFound { .. })));
        assert!(matches!(store.new_blob_file(), Err(ZoeError::ReadOnly)));
        drop(store);
        assert_eq!(tree(&dir.join(BLOB_DIR)), before);

        // The next writer removes the leftovers.
        drop(Store::open(&dir).unwrap());
        assert!(!dir.join(blob_name(orphan, crashed)).exists());
        assert!(!dir.join(intent_name(crashed)).exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_directory_in_tmp_does_not_stop_an_open() {
        let vfs = MemVfs::new();
        drop(blob_file(&Store::open_vfs(vfs.clone()).unwrap(), b"x"));
        Vfs::create_dir(&vfs, &format!("{TMP_DIR}/stray")).unwrap();
        let mut file = Vfs::create_file(&vfs, &format!("{TMP_DIR}/stray/f")).unwrap();
        file.write_all(b"x").unwrap();
        drop(file);
        Store::open_vfs(vfs.clone()).unwrap();
        assert!(Vfs::exists(&vfs, &format!("{TMP_DIR}/stray/f")).unwrap());
    }
}
