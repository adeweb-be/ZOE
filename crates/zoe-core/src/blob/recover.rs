//! The checks of a blob directory when the store opens, and the removal
//! of the leftovers of a crash in `blobs/tmp/`.

use std::collections::BTreeSet;
use std::io::Read;

use super::{BlobStore, is_owned_name, oid_levels};
use crate::backend::Backend;
use crate::error::{Result, ZoeError};
use crate::format::blob::{
    BLOB_DIR, LAYOUT, LAYOUT_FILE, NEW_LAYOUT_FILE, TMP_DIR, blob_name, intent_tid, oid_dir,
};
use crate::types::{Oid, Tid};
use crate::vfs::parent;

impl BlobStore {
    /// Check the blob store when the store opens.
    ///
    /// The layout file must name the bushy layout. A writer refuses a blob
    /// directory without a layout file in two cases. When `backend` has
    /// blob records, it is not the blob directory of the store:
    /// `MissingBlobDir`. When the directory holds files of another
    /// program, the store must not take it: `NotBlobDir`.
    ///
    /// A writer takes the lock on the blob directory first, when the
    /// directory exists. A blob directory that another open store holds
    /// fails the open with `Locked`, before the open reads or removes
    /// anything there.
    ///
    /// Then a writer removes the leftovers of a crash in `blobs/tmp/`: the
    /// blob files that an intent file names and that `backend` has no blob
    /// record for, then the intent files and the temporary files. In
    /// `blobs/tmp/` it removes only the regular files whose names the store
    /// owns (`is_owned_name`). Other files stay. It does this only when the
    /// layout file exists. The first blob writes the layout
    /// file before any intent file or temporary file, so a directory
    /// without it holds none, and its other files are never removed.
    ///
    /// A read-only store only reads the layout file and changes nothing.
    /// It leaves the leftovers of a crash to the next writer. A blob file
    /// that a crash left without its blob record stays invisible, because
    /// every blob read starts from the blob record.
    pub(crate) fn open(&self, backend: &dyn Backend, read_only: bool) -> Result<()> {
        if !read_only && self.dir.exists(BLOB_DIR)? {
            self.lock_dir()?;
        }
        let layout = self.dir.exists(LAYOUT_FILE)?;
        if layout {
            let mut text = String::new();
            self.dir.read_file(LAYOUT_FILE)?.read_to_string(&mut text)?;
            if text.trim() != LAYOUT {
                return Err(ZoeError::Format(format!(
                    "blob layout {:?}: only {LAYOUT:?} is supported",
                    text.trim()
                )));
            }
        }
        if read_only {
            return Ok(());
        }
        if !layout {
            self.check_present(backend)?;
            return self.check_new();
        }
        if self.dir.exists(TMP_DIR)? {
            self.recover(backend)?;
        }
        Ok(())
    }

    /// Fail with `MissingBlobDir` when `backend` has blob records and the
    /// blob directory has no layout file. The first blob of a store makes
    /// its layout file durable before any blob record. So such a directory
    /// is not the blob directory of the store. The call changes nothing.
    pub(crate) fn check_present(&self, backend: &dyn Backend) -> Result<()> {
        if !self.dir.exists(LAYOUT_FILE)? && backend.has_blobs()? {
            return Err(ZoeError::MissingBlobDir {
                path: self.dir.path(BLOB_DIR),
            });
        }
        Ok(())
    }

    /// Check that a blob directory without a layout file holds no file of
    /// another program, and fail with `NotBlobDir` when it does. A missing
    /// directory passes. A directory passes when it is empty, or when it
    /// holds only `tmp/`, empty or with only the new layout file: the first
    /// blob of a store creates these before the layout file.
    pub(super) fn check_new(&self) -> Result<()> {
        if !self.dir.exists(BLOB_DIR)? {
            return Ok(());
        }
        let tmp = &TMP_DIR[BLOB_DIR.len() + 1..];
        let new_layout = &NEW_LAYOUT_FILE[TMP_DIR.len() + 1..];
        let fresh = match &self.dir.list_entries(BLOB_DIR)?[..] {
            [] => true,
            [name] if name == tmp && !self.dir.list_files(BLOB_DIR)?.contains(name) => {
                let entries = self.dir.list_entries(TMP_DIR)?;
                entries.is_empty()
                    || (entries == [new_layout] && self.dir.list_files(TMP_DIR)? == [new_layout])
            }
            _ => false,
        };
        if fresh {
            Ok(())
        } else {
            Err(ZoeError::NotBlobDir {
                path: self.dir.path(BLOB_DIR),
            })
        }
    }

    /// Remove the leftovers of a crash in `blobs/tmp/`. Only regular
    /// files with a name that the store owns are removed there. Other
    /// files stay: with a shared blob directory, a ZEO client keeps its
    /// temporary files in `blobs/tmp/`.
    fn recover(&self, backend: &dyn Backend) -> Result<()> {
        let mut leftovers = Vec::new();
        let mut intents = Vec::new();
        for file in self.dir.list_files(TMP_DIR)? {
            if !is_owned_name(&file) {
                continue;
            }
            let name = format!("{TMP_DIR}/{file}");
            if let Some(tid) = intent_tid(&file) {
                // An intent that does not decode was not synced, so its
                // finish created no directory and renamed no blob file.
                if let Some(oids) = self.read_intent(&name, tid)? {
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
