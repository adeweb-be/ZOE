//! The blob store: one file per blob, beside the store, in the BushyLayout
//! of ZODB. `format::blob` gives the names.
//!
//! A new blob goes into a temporary file in `blobs/tmp/`. The file
//! computes the BLAKE3 hash of the bytes while they are written.
//! `store_blob` syncs the file. A client can also write the file itself in
//! `blobs/tmp/` and give its path: `store_blob_at` syncs the file, renames
//! it to a temporary name of the core and computes its hash. The core never
//! copies the file. `finish` places the blobs before the
//! backend commit: it writes and syncs an intent file that names the tid
//! and the oids, renames each blob file into its place, and syncs each
//! directory. The blob record holds the hash and the size, in the same
//! backend write transaction as the record. After the commit, `finish`
//! removes the intent file. A deferred commit has no sync, so it keeps its
//! intent file until a durable commit covers it.
//!
//! So a durable blob record always has its file. A crash can leave a blob
//! file without its blob record, but then the intent file is on disk.
//! Opening a store reads only `blobs/tmp/`: it removes the blob files that
//! an intent file names and that have no blob record, then the intent
//! files and the temporary files. In `blobs/tmp/` it removes only the
//! regular files whose names the store owns: `<digits>.tmp`, the intent
//! files, `layout.tmp` and `upload-<digits>`. Every other file stays. With
//! a shared blob directory, `blobs/tmp/` is also the temporary directory of
//! each ZEO client, so the files of the clients stay.
//!
//! Pack uses the same intent files. Before it commits the removal of blob
//! records, it writes and syncs an intent file for each tid of them. After
//! the commit, it removes the blob files, syncs their directories and
//! removes the intent files. So a crash after the commit leaves the intent
//! file, and the next open removes the files.
//!
//! `dir` holds the directories of a blob store other than the store
//! directory, `recover` the checks and the recovery of an open, and `file`
//! the blob files that a transaction writes and reads.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use crate::backend::BlobRecord;
use crate::error::{Result, ZoeError};
use crate::format::blob::{
    BLOB_DIR, LAYOUT, LAYOUT_FILE, NEW_LAYOUT_FILE, TMP_DIR, blob_name, decode_intent,
    encode_intent, intent_name, intent_tid, oid_dir,
};
use crate::store::Opened;
use crate::types::{Oid, Tid};
use crate::vfs::{Dir, DirLock, parent};

mod dir;
mod file;
mod recover;
#[cfg(test)]
mod tests;

pub(crate) use dir::{ReadOnlyDir, Relocated};
pub(crate) use file::PendingBlob;
pub use file::{BlobFile, BlobReader};

/// The blob files of a store.
pub(crate) struct BlobStore {
    /// The store directory. It holds the single-writer lock while the
    /// store is open. The staging area shares it through an `Arc`, and
    /// nothing outside `Opened` holds it.
    dir: Arc<dyn Dir>,
    /// Tells if `blobs/`, `blobs/tmp/` and the layout file are durable.
    ready: Mutex<bool>,
    /// The lock on the blob directory itself, once a writer took it. Two
    /// open stores never share a blob directory.
    dir_lock: Mutex<Option<DirLock>>,
    /// The number of the next temporary file.
    next_temp: AtomicU64,
    /// The intent files of the deferred commits that no durable commit
    /// covers yet. Only a holder of the commit sequencer takes this
    /// mutex, and it takes no other lock while it holds it.
    kept: Mutex<Vec<String>>,
}

impl BlobStore {
    /// Return the blob store of the store directory `dir`. This reads
    /// nothing: `open` checks the store directory.
    pub(crate) fn new(dir: Arc<dyn Dir>) -> Self {
        Self {
            dir,
            ready: Mutex::new(false),
            dir_lock: Mutex::new(None),
            next_temp: AtomicU64::new(0),
            kept: Mutex::new(Vec::new()),
        }
    }


    /// Tell if the blob file of `oid` at `tid` exists. This reads no byte
    /// of the file.
    pub(crate) fn has_file(&self, oid: Oid, tid: Tid) -> Result<bool> {
        Ok(self.dir.exists(&blob_name(oid, tid))?)
    }

    /// Take the lock on the blob directory itself, when this store does not
    /// hold it yet. The lock is `flock` on a descriptor of the directory,
    /// so it adds no file to the directory. A lock that another open store
    /// holds fails with `Locked`, and that store keeps its files. A
    /// filesystem that cannot lock a directory fails the call: the store
    /// never runs without the lock.
    fn lock_dir(&self) -> Result<()> {
        let mut held = crate::lock(&self.dir_lock);
        if held.is_some() {
            return Ok(());
        }
        match self.dir.lock_dir(BLOB_DIR) {
            Ok(lock) => {
                *held = Some(lock);
                Ok(())
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Err(ZoeError::Locked),
            Err(e) => {
                let path = self.dir.path(BLOB_DIR);
                let message = format!("cannot lock the blob directory {}: {e}", path.display());
                Err(io::Error::new(e.kind(), message).into())
            }
        }
    }

    /// Create `blobs/`, `blobs/tmp/` and the layout file when they do not
    /// exist, and make them durable.
    ///
    /// A process that stops before a sync can leave these entries in a
    /// state that exists but is not durable. So the parents are synced
    /// also when the entries exist.
    fn make_ready(&self) -> Result<()> {
        let mut ready = crate::lock(&self.ready);
        if *ready {
            return Ok(());
        }
        if !self.dir.exists(BLOB_DIR)? {
            self.dir.create_dir(BLOB_DIR)?;
        }
        self.lock_dir()?;
        self.dir.sync_dir_at(parent(BLOB_DIR))?;
        // The directory can come after the open. A layout file goes only
        // into a directory that holds no file of another program.
        if !self.dir.exists(LAYOUT_FILE)? {
            self.check_new()?;
        }
        if !self.dir.exists(TMP_DIR)? {
            self.dir.create_dir(TMP_DIR)?;
        }
        if !self.dir.exists(LAYOUT_FILE)? {
            // The layout file is renamed into place, so a crash cannot
            // leave a partial one.
            if self.dir.exists(NEW_LAYOUT_FILE)? {
                self.dir.remove(NEW_LAYOUT_FILE)?;
            }
            self.write_synced(NEW_LAYOUT_FILE, LAYOUT.as_bytes())?;
            // The creation is durable before the rename. So the sync of
            // `blobs/` below makes the layout file durable, before any
            // temporary file or intent file exists: the open recovers only
            // a directory with a layout file.
            self.dir.sync_dir_at(TMP_DIR)?;
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

    /// Return a new name for a temporary file in `blobs/tmp/`. The core
    /// gives only names of this form to its temporary files.
    fn new_temp_name(&self) -> String {
        let n = self.next_temp.fetch_add(1, Ordering::Relaxed);
        format!("{TMP_DIR}/{n}.tmp")
    }

    /// Return a new temporary file for a blob of the store `store`.
    pub(crate) fn new_file(&self, store: Weak<Opened>) -> Result<BlobFile> {
        self.make_ready()?;
        let name = self.new_temp_name();
        let file = self.dir.create_file(&name)?;
        Ok(BlobFile {
            store,
            name,
            file: Some(file),
            hasher: blake3::Hasher::new(),
            size: 0,
        })
    }

    /// Return the path of `blobs/tmp/`. Create the blob store when it does
    /// not exist.
    pub(crate) fn temp_dir(&self) -> Result<PathBuf> {
        self.make_ready()?;
        Ok(self.dir.path(TMP_DIR))
    }

    /// Take the file at `path` for a blob of the store `store`, and return
    /// the blob.
    ///
    /// The file must be a regular file in `blobs/tmp/` or in one of its
    /// subdirectories. The call syncs the file. It syncs the directory of a
    /// file in a subdirectory, so that the creation of the file is durable.
    /// Then it renames the file to a new temporary name in `blobs/tmp/` and
    /// computes the BLAKE3 hash of the file. So the file is durable before
    /// the intent file of the finish, as a file from `new_file` is.
    ///
    /// A path outside `blobs/tmp/`, or the name of a file that the core
    /// makes in `blobs/tmp/`, is refused with `Protocol`. An error before
    /// the rename leaves the file in its place. After the rename, the blob
    /// owns the file: an error drops the blob, and that removes the file.
    pub(crate) fn take(&self, store: Weak<Opened>, path: &Path) -> Result<PendingBlob> {
        self.make_ready()?;
        let name = self.temp_name(path).ok_or(ZoeError::Protocol(
            "the blob file is not in the blob temporary directory",
        ))?;
        self.dir.sync_file(&name)?;
        let dir = parent(&name);
        if dir != TMP_DIR {
            self.dir.sync_dir_at(dir)?;
        }
        self.adopt(store, &name)
    }

    /// Take the file `name` in the directory of `oid` for a blob of the
    /// store `store`, and return the blob. This is the file of a client
    /// that shares the blob directory, as ZEO's `storeBlobShared` gives it.
    ///
    /// `name` is a name, never a path: it has no `/`, and it ends in `.tmp`
    /// or `.tmp-`. Another name is refused with `Protocol`, before the call
    /// reads or changes anything. The file must be a regular file when the
    /// call syncs it. The client has the trust of the user of the store: a
    /// link that it puts in place of the file after the sync is followed.
    ///
    /// The client created the levels of the oid directory, and a stop of
    /// the client can leave them not durable. So the call syncs the parent
    /// of each level from the top down, then the oid directory, so that the
    /// directory is durable before `finish` puts the blob file in it. Then
    /// it takes the file as `take` does: it syncs it, renames it to a new
    /// temporary name in `blobs/tmp/` and computes its BLAKE3 hash. An error
    /// before the rename leaves the file in its place. After the rename,
    /// the blob owns the file.
    pub(crate) fn take_shared(
        &self,
        store: Weak<Opened>,
        oid: Oid,
        name: &str,
    ) -> Result<PendingBlob> {
        if !is_shared_name(name) {
            return Err(ZoeError::Protocol(
                "the blob file name is not a temporary name in the directory of its oid",
            ));
        }
        self.make_ready()?;
        let dir = oid_dir(oid);
        let file = format!("{dir}/{name}");
        self.dir.sync_file(&file)?;
        for level in oid_levels(oid) {
            self.dir.sync_dir_at(parent(&level))?;
        }
        self.dir.sync_dir_at(&dir)?;
        self.adopt(store, &file)
    }

    /// Rename the synced file `name` to a new temporary name in
    /// `blobs/tmp/`, compute its BLAKE3 hash and return the blob. After the
    /// rename, the blob owns the file: an error drops the blob, and that
    /// removes the file.
    fn adopt(&self, store: Weak<Opened>, name: &str) -> Result<PendingBlob> {
        let temp = self.new_temp_name();
        self.dir.rename(name, &temp)?;
        let mut blob = PendingBlob {
            store,
            temp: Some(temp.clone()),
            record: BlobRecord {
                hash: [0; 32],
                size: 0,
            },
            linked: false,
        };
        blob.record = self.hash(&temp)?;
        Ok(blob)
    }

    /// Return a blob of the store `store` whose file is a hard link to the
    /// committed blob file of `oid` at `tid`. `record` is the blob record of
    /// that file. The link gets a new temporary name in `blobs/tmp/`, so
    /// `place` renames it into its place as it does any other blob. No byte
    /// is copied, and the committed file does not change. A missing file is
    /// a corrupt blob.
    ///
    /// The committed file is durable, so the link needs no sync before the
    /// intent file of the finish. A crash before the finish leaves the
    /// temporary name, and the next open removes it, not the committed
    /// file.
    ///
    /// When the filesystem cannot link (`cannot_link`), the call copies the
    /// bytes into a new temporary file and syncs it. So it writes a new
    /// file, and no blob file is written again.
    pub(crate) fn link(
        &self,
        store: Weak<Opened>,
        oid: Oid,
        tid: Tid,
        record: BlobRecord,
    ) -> Result<PendingBlob> {
        self.make_ready()?;
        let committed = blob_name(oid, tid);
        let temp = self.new_temp_name();
        let linked = match self.dir.hard_link(&committed, &temp) {
            Ok(()) => true,
            Err(e) if cannot_link(&e) => false,
            Err(e) => return Err(missing_is_corrupt(e, oid, tid)),
        };
        if linked {
            return Ok(PendingBlob {
                store,
                temp: Some(temp),
                record,
                linked,
            });
        }
        let mut file = self.dir.create_file(&temp)?;
        // The blob owns the new file from here, so an error removes it.
        let blob = PendingBlob {
            store,
            temp: Some(temp),
            record,
            linked,
        };
        let mut source = self
            .dir
            .read_file(&committed)
            .map_err(|e| missing_is_corrupt(e, oid, tid))?;
        io::copy(&mut source, &mut file)?;
        file.sync()?;
        Ok(blob)
    }

    /// Return the name of the file at `path` in `blobs/tmp/`. Return `None`
    /// when `path` is not in `blobs/tmp/`, or when it names a file that the
    /// core makes there: a temporary file, an intent file or the new
    /// layout file. Only a file in a subdirectory can have any name.
    fn temp_name(&self, path: &Path) -> Option<String> {
        let rest = path.strip_prefix(self.dir.path(TMP_DIR)).ok()?;
        let mut parts = Vec::new();
        for component in rest.components() {
            match component {
                Component::Normal(part) => parts.push(part.to_str()?),
                _ => return None,
            }
        }
        let name = format!("{TMP_DIR}/{}", parts.join("/"));
        let own = match parts[..] {
            [] => true,
            [file] => is_temp_name(file) || intent_tid(file).is_some() || name == NEW_LAYOUT_FILE,
            _ => false,
        };
        (!own).then_some(name)
    }

    /// Read the file `name` and return its blob record: the BLAKE3 hash and
    /// the size of its bytes.
    fn hash(&self, name: &str) -> Result<BlobRecord> {
        let mut hasher = blake3::Hasher::new();
        let size = io::copy(&mut self.dir.read_file(name)?, &mut hasher)?;
        Ok(BlobRecord {
            hash: *hasher.finalize().as_bytes(),
            size,
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
        self.write_synced(&intent, &encode_intent(tid, &oids))?;
        self.dir.sync_dir_at(TMP_DIR)?;

        for (&oid, blob) in blobs.iter_mut() {
            let dir = oid_dir(oid);
            // Each level of the oid directory is durable before the next
            // one is created.
            for level in oid_levels(oid) {
                self.create_dir(&level)?;
            }
            let temp = blob.temp.as_deref().expect("a blob is placed once");
            // A link names a committed file, which is read-only already.
            if !blob.linked {
                self.dir.set_read_only(temp)?;
            }
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

    /// Keep the intent file of a deferred commit after its backend commit.
    /// A crash before a durable commit leaves the file, and the next open
    /// removes the blob files of the lost commit.
    pub(crate) fn keep(&self, intent: Option<String>) {
        if let Some(intent) = intent {
            self.lock_kept().push(intent);
        }
    }

    /// Remove the kept intent files after a durable commit that covers
    /// their commits. A file that a pack removed already is not an error.
    pub(crate) fn release_kept(&self) {
        let kept = std::mem::take(&mut *self.lock_kept());
        for intent in kept {
            self.finished(Some(intent));
        }
    }

    fn lock_kept(&self) -> std::sync::MutexGuard<'_, Vec<String>> {
        crate::lock(&self.kept)
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
            // An intent that does not decode was not synced, so it names
            // nothing that a commit removed.
            if let Some(old) = self.read_intent(&intent, tid)? {
                named.extend(old);
            }
        }
        let named: Vec<Oid> = named.into_iter().collect();
        let temp = self.new_temp_name();
        self.write_synced(&temp, &encode_intent(tid, &named))?;
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
        let file = self
            .dir
            .read_file(&blob_name(oid, tid))
            .map_err(|e| missing_is_corrupt(e, oid, tid))?;
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

    /// Write `bytes` into the new file `name` and sync the file. An intent
    /// file and the layout file are written so.
    fn write_synced(&self, name: &str, bytes: &[u8]) -> Result<()> {
        let mut file = self.dir.create_file(name)?;
        file.write_all(bytes)?;
        file.sync()?;
        Ok(())
    }

    /// Read the intent file `name` of `tid`, and return the oids that it
    /// names. Return `None` when the intent does not decode.
    fn read_intent(&self, name: &str, tid: Tid) -> Result<Option<Vec<Oid>>> {
        let mut bytes = Vec::new();
        self.dir.read_file(name)?.read_to_end(&mut bytes)?;
        Ok(decode_intent(tid, &bytes).ok())
    }
}

/// Tell if `error`, of a hard link, says that the filesystem cannot link
/// the file: the two names are on two devices, the filesystem refuses or
/// does not have links, or the file has the most links that it can have.
/// FAT and exFAT on macOS give `ENOTSUP`, which Rust does not map to
/// `Unsupported`, so the code of the system counts too. On Linux
/// `ENOTSUP` is `EOPNOTSUPP`.
fn cannot_link(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::CrossesDevices
            | io::ErrorKind::PermissionDenied
            | io::ErrorKind::Unsupported
    ) || matches!(error.raw_os_error(), Some(code) if code == libc::EMLINK || code == libc::ENOTSUP)
}

/// Tell if `file`, a name in `blobs/tmp/`, has the form of a temporary file
/// of the core: digits, then `.tmp`.
fn is_temp_name(file: &str) -> bool {
    file.strip_suffix(".tmp")
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// Tell if `file`, a name in `blobs/tmp/`, is a name that the store owns:
/// a temporary file of the core, an intent file, the new layout file, or
/// `upload-` and one or more ASCII digits. A store can hold a file with
/// this last name from an upload of `zoe serve`. A writer open removes
/// only these files in `blobs/tmp/`.
fn is_owned_name(file: &str) -> bool {
    is_temp_name(file)
        || intent_tid(file).is_some()
        || format!("{TMP_DIR}/{file}") == NEW_LAYOUT_FILE
        || file
            .strip_prefix("upload-")
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// Tell if `name` is a name that `take_shared` takes: one part without `/`
/// or NUL, that ends in `.tmp` or `.tmp-`. A ZEO client with a shared blob
/// directory gives a name that ends in `.tmp-`.
fn is_shared_name(name: &str) -> bool {
    !name.contains(['/', '\0']) && (name.ends_with(".tmp") || name.ends_with(".tmp-"))
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

/// Return the error of `error`, of a read or a link of the blob file of
/// `oid` at `tid`. A missing file is a corrupt blob.
fn missing_is_corrupt(error: io::Error, oid: Oid, tid: Tid) -> ZoeError {
    if error.kind() == io::ErrorKind::NotFound {
        ZoeError::CorruptBlob { oid, tid }
    } else {
        error.into()
    }
}
