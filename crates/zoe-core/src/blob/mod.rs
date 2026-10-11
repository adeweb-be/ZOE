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

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::backend::{Backend, BlobRecord};
use crate::error::{Result, ZoeError};
use crate::format::blob::{
    BLOB_DIR, LAYOUT, LAYOUT_FILE, NEW_LAYOUT_FILE, TMP_DIR, blob_name, decode_intent,
    encode_intent, intent_name, intent_tid, oid_dir,
};
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
    /// store is open.
    dir: Arc<dyn BlobDir>,
    /// Tells if `blobs/`, `blobs/tmp/` and the layout file are durable.
    ready: Mutex<bool>,
    /// The number of the next temporary file.
    next_temp: AtomicU64,
}

impl BlobStore {
    /// Open the blob store of the store directory `dir`.
    ///
    /// The layout file must name the bushy layout. Then the leftovers of a
    /// crash in `blobs/tmp/` are removed: the blob files that an intent
    /// file names and that `backend` has no blob record for, the intent
    /// files and the temporary files.
    pub(crate) fn open(dir: Arc<dyn BlobDir>, backend: &dyn Backend) -> Result<Self> {
        if dir.exists(LAYOUT_FILE)? {
            let mut layout = String::new();
            dir.read_file(LAYOUT_FILE)?.read_to_string(&mut layout)?;
            if layout.trim() != LAYOUT {
                return Err(ZoeError::Format(format!(
                    "blob layout {:?}: only {LAYOUT:?} is supported",
                    layout.trim()
                )));
            }
        }
        let store = Self {
            dir,
            ready: Mutex::new(false),
            next_temp: AtomicU64::new(0),
        };
        if store.dir.exists(TMP_DIR)? {
            store.recover(backend)?;
        }
        Ok(store)
    }

    /// Remove the leftovers of a crash in `blobs/tmp/`. Only regular
    /// files are removed there.
    fn recover(&self, backend: &dyn Backend) -> Result<()> {
        let mut leftovers = Vec::new();
        let mut orphan_dirs = BTreeSet::new();
        for file in self.dir.list_files(TMP_DIR)? {
            let name = format!("{TMP_DIR}/{file}");
            if let Some(tid) = intent_tid(&file) {
                let mut bytes = Vec::new();
                self.dir.read_file(&name)?.read_to_end(&mut bytes)?;
                // An intent that does not decode was not synced, so its
                // finish renamed no blob file.
                for oid in decode_intent(tid, &bytes).unwrap_or_default() {
                    let blob = blob_name(oid, tid);
                    if !has_blob_record(backend, oid, tid)? && self.dir.exists(&blob)? {
                        self.dir.remove(&blob)?;
                        orphan_dirs.insert(oid_dir(oid));
                    }
                }
            }
            leftovers.push(name);
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
    fn make_ready(&self) -> Result<()> {
        let mut ready = self.ready.lock().unwrap_or_else(|e| e.into_inner());
        if *ready {
            return Ok(());
        }
        for dir in [BLOB_DIR, TMP_DIR] {
            self.create_dir(dir)?;
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
            self.dir.sync_dir_at(BLOB_DIR)?;
        }
        *ready = true;
        Ok(())
    }

    /// Create the directory `dir` when it does not exist, and make it
    /// durable. Its parent must exist and be durable.
    fn create_dir(&self, dir: &str) -> Result<()> {
        if !self.dir.exists(dir)? {
            self.dir.create_dir(dir)?;
            self.dir.sync_dir_at(parent(dir))?;
        }
        Ok(())
    }

    /// Return a new temporary file for a blob.
    pub(crate) fn new_file(&self) -> Result<BlobFile> {
        self.make_ready()?;
        let n = self.next_temp.fetch_add(1, Ordering::Relaxed);
        let name = format!("{TMP_DIR}/{n}.tmp");
        let file = self.dir.create_file(&name)?;
        Ok(BlobFile {
            dir: Arc::clone(&self.dir),
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
            for (end, _) in dir.match_indices('/').skip(1) {
                self.create_dir(&dir[..end])?;
            }
            self.create_dir(&dir)?;
            let temp = blob.temp.as_deref().expect("a blob is placed once");
            self.dir.set_read_only(temp)?;
            self.dir.rename(temp, &blob_name(oid, tid))?;
            blob.temp = None;
            self.dir.sync_dir_at(&dir)?;
        }
        Ok(Some(intent))
    }

    /// Remove the intent file of a finish after its backend commit. A
    /// failure leaves the file for the next open, which keeps the blob
    /// files that have their blob record.
    pub(crate) fn finished(&self, intent: Option<String>) {
        if let Some(intent) = intent {
            let _ = self.dir.remove(&intent);
        }
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
/// the file to `Txn::store_blob`. Dropping the file removes it.
pub struct BlobFile {
    dir: Arc<dyn BlobDir>,
    name: String,
    /// The open file. It is `None` after `close`.
    file: Option<Box<dyn WriteFile>>,
    hasher: blake3::Hasher,
    size: u64,
}

impl BlobFile {
    /// Sync the file and return the blob, ready for a commit.
    pub(crate) fn close(mut self) -> Result<PendingBlob> {
        let mut file = self.file.take().expect("a blob file is closed once");
        file.sync()?;
        drop(file);
        Ok(PendingBlob {
            dir: Arc::clone(&self.dir),
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
            let _ = self.dir.remove(&self.name);
        }
    }
}

/// A synced blob that a transaction holds until its finish or its abort.
/// Dropping a blob that `place` did not rename removes its file.
pub(crate) struct PendingBlob {
    dir: Arc<dyn BlobDir>,
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
        if let Some(temp) = self.temp.take() {
            let _ = self.dir.remove(&temp);
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
    use crate::vfs::MemVfs;

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
    fn a_later_store_drops_the_blob_of_its_oid() {
        let vfs = MemVfs::new();
        let store = Store::open_vfs(vfs.clone()).unwrap();
        let oid = store.new_oid().unwrap();
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.store_blob(oid, Tid::ZERO, PICKLE, blob_file(&store, b"blob"))
            .unwrap();
        txn.store(oid, Tid::ZERO, PICKLE).unwrap();
        txn.vote().unwrap();
        let tid = txn.finish().unwrap();
        assert!(matches!(
            store.blob_path(oid, tid),
            Err(ZoeError::NotFound { .. })
        ));
        assert!(temp_files(&vfs).is_empty());
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
        let vfs = MemVfs::new();
        let (committed, orphan, tid) = {
            let store = Store::open_vfs(vfs.clone()).unwrap();
            let committed = store.new_oid().unwrap();
            let orphan = store.new_oid().unwrap();
            let tid = commit_blobs(&store, &[committed], b"kept");
            (committed, orphan, tid)
        };

        // A finish that placed its blob files and crashed before its
        // backend commit, the intent of a finish that committed, and a
        // temporary file.
        let crashed = Tid(tid.0 + 1);
        let dir = oid_dir(orphan);
        let ends = dir.match_indices('/').map(|(end, _)| end);
        for end in ends.chain([dir.len()]) {
            if !Vfs::exists(&vfs, &dir[..end]).unwrap() {
                Vfs::create_dir(&vfs, &dir[..end]).unwrap();
            }
        }
        for (name, bytes) in [
            (blob_name(orphan, crashed), b"orphan".to_vec()),
            (intent_name(crashed), encode_intent(crashed, &[orphan])),
            (intent_name(tid), encode_intent(tid, &[committed])),
            (format!("{TMP_DIR}/7.tmp"), b"temporary".to_vec()),
        ] {
            let mut file = Vfs::create_file(&vfs, &name).unwrap();
            file.write_all(&bytes).unwrap();
        }

        let store = Store::open_vfs(vfs.clone()).unwrap();
        assert!(!Vfs::exists(&vfs, &blob_name(orphan, crashed)).unwrap());
        assert_eq!(read(&store, committed, tid).unwrap(), b"kept");
        assert!(temp_files(&vfs).is_empty());
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
