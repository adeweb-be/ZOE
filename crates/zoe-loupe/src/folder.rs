//! A store folder, open for reads only, and the calls of the window on it.

use std::ops::ControlFlow;
use std::path::Path;

use zoe_core::{Oid, Options, PackReport, Store, Tid, ZoeError};

use crate::diff::{self, Change};
use crate::model::{Object, Revision, note};
use crate::snapshot::Snapshot;
use crate::space::{self, PathSpace, Progress, Space};
use crate::{Error, Result, state};

/// The most revisions that [`Folder::history`] gives, newest first.
pub const HISTORY_LIMIT: usize = 1 << 12;

/// A store folder, open for reads only.
///
/// The folder holds the lock of the store while it is open. So a running
/// instance and Loupe never share a store.
pub struct Folder {
    store: Store,
}

impl Folder {
    /// Open the store in the folder `path` for reads only, with
    /// `Store::open_read_only_repairing`.
    ///
    /// The call fails with `NoStore` when `path` holds no store, and with
    /// `Locked` when another process holds the lock of the store. On a
    /// store that was closed cleanly, the call writes nothing. A store that
    /// a writer left open gets its repair first: the call writes only the
    /// database file, and `Store::repaired` of [`Folder::store`] tells it.
    /// The caller shows the operator that the open repaired the store. A
    /// repair that fails gives `RepairFailed`, for example on a read-only
    /// file system.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_blob_dir(path, None)
    }

    /// Open the store in the folder `path` for reads only, as
    /// [`Folder::open`] does, with the blob directory `blob_dir`.
    ///
    /// The store does not record its blob directory. With `None`, the blob
    /// directory is `blobs` in the folder. The call writes nothing in the
    /// blob directory and in its parent, also when it repairs the store.
    pub fn open_with_blob_dir(path: impl AsRef<Path>, blob_dir: Option<&Path>) -> Result<Self> {
        let path = path.as_ref();
        let options = Options {
            blob_dir: blob_dir.map(Path::to_path_buf),
            ..Options::default()
        };
        let store = Store::open_read_only_repairing(path, options).map_err(|e| match e {
            ZoeError::Io(e) if e.kind() == std::io::ErrorKind::NotFound => Error::NoStore {
                path: path.display().to_string(),
            },
            e => Error::Store(e),
        })?;
        Ok(Self { store })
    }

    /// Return the store of the folder. It is open for reads only, so each
    /// write call fails with `ReadOnly`.
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// Return the tid of the last transaction of the store.
    pub fn last_tid(&self) -> Tid {
        self.store.last_transaction()
    }

    /// Return a view of the store as of `tid`: each object as it was after
    /// the last transaction at or before `tid`.
    pub fn as_of(&self, tid: Tid) -> Snapshot<'_> {
        Snapshot::new(self, tid)
    }

    /// Return the root object, oid 0, as of the last transaction.
    pub fn root(&self) -> Result<Object> {
        self.latest().root()
    }

    /// Return the object `oid` as of the last transaction.
    ///
    /// A record that zoe-pickle refuses gives an object with the refusal
    /// in `error`. An oid without a record is an error.
    pub fn object(&self, oid: Oid) -> Result<Object> {
        self.latest().object(oid)
    }

    /// Return the object that the Plone path `path` leads to, as of the
    /// last transaction.
    ///
    /// A path starts at the Zope application, `root["Application"]`. Each
    /// segment is an item of the `_tree` of a folder, a key of a BTree or a
    /// mapping, or an attribute that holds a reference. So `/plone/folder`
    /// and `/plone/folder/doc/__annotations__/key` resolve.
    pub fn resolve(&self, path: &str) -> Result<Object> {
        self.latest().resolve(path)
    }

    /// Return the revisions of `oid`, newest first. Give at most
    /// [`HISTORY_LIMIT`] of them.
    ///
    /// The user and the description are text. Bytes that are not UTF-8
    /// become U+FFFD. A text longer than 4 KiB shows its start and ends
    /// with `…`. An oid without a record is an error.
    pub fn history(&self, oid: Oid) -> Result<Vec<Revision>> {
        let entries = self.store.history(oid, HISTORY_LIMIT)?;
        Ok(entries
            .into_iter()
            .map(|entry| Revision {
                tid: entry.tid,
                user: note(&entry.user),
                description: note(&entry.description),
                size: entry.size,
            })
            .collect())
    }

    /// Return the changes from the state of `oid` as of `before` to its
    /// state as of `after`.
    ///
    /// The diff compares the decoded trees, not the bytes. The module
    /// documentation of `diff` gives the rules and the limits.
    ///
    /// A deletion has no state. Against a deletion, the diff is one change
    /// of the whole state, `""`, without a value on the side of the
    /// deletion. Two deletions give no change. The call fails when the
    /// object has no record as of one of the tids, or when zoe-pickle
    /// refuses one of the two records.
    pub fn diff(&self, oid: Oid, before: Tid, after: Tid) -> Result<Vec<Change>> {
        let before = self.as_of(before).tree_or_deletion(oid)?;
        let after = self.as_of(after).tree_or_deletion(oid)?;
        let whole = |tree: &zoe_pickle::Tree| state::state(tree).0;
        Ok(match (before, after) {
            (Some(before), Some(after)) => diff::diff(&before, &after),
            (None, None) => Vec::new(),
            (before, after) => vec![Change {
                path: String::new(),
                before: before.as_ref().map(whole),
                after: after.as_ref().map(whole),
            }],
        })
    }

    /// Return the space of the store as of the last transaction: the
    /// number of current records and their bytes for each class, the
    /// current blob files, the `largest` largest current records and what
    /// each transaction wrote.
    ///
    /// The call reads every current record and every transaction. A record
    /// with a blob record and no blob file fails it with `CorruptBlob`.
    pub fn space(&self, largest: usize) -> Result<Space> {
        self.space_with(largest, |_| ControlFlow::Continue(()))
    }

    /// Return the space of the store as [`Folder::space`] does. Call
    /// `progress` after each record and each transaction that the call
    /// reads. When it gives `Break`, the call stops with `Stopped`.
    pub fn space_with(
        &self,
        largest: usize,
        progress: impl FnMut(Progress) -> ControlFlow<()>,
    ) -> Result<Space> {
        space::space(&self.store, largest, progress)
    }

    /// Return the size of the Plone path `path` as of the last
    /// transaction: the bytes of the current records of every object that
    /// the object at `path` reaches through its strong references, itself
    /// included, each object once.
    ///
    /// The references are the ones that [`Object`] gives, so a record that
    /// does not decode adds its own bytes and no reference. A reference to
    /// an oid without a current record adds nothing. [`Folder::resolve`]
    /// gives the form of a path.
    pub fn path_size(&self, path: &str) -> Result<u64> {
        space::path_size(self.latest(), path)
    }

    /// Return the space of the Plone path `path` and of its items as of the
    /// last transaction. Its `reach` is [`Folder::path_size`], with the
    /// number of objects. The items and the object itself share it, each
    /// object once. The module documentation of `space` gives the rules.
    ///
    /// Call `progress` after each object and each BTree node that the call
    /// reads. When it gives `Break`, the call stops with `Stopped`.
    pub fn path_space(
        &self,
        path: &str,
        progress: impl FnMut(Progress) -> ControlFlow<()>,
    ) -> Result<PathSpace> {
        space::path_space(self.latest(), path, progress)
    }

    /// Return what a pack at `at` would remove: the report of the dry run
    /// of the core, `Store::pack_dry_run`. The dry run writes nothing.
    pub fn reclaimable(&self, at: Tid) -> Result<PackReport> {
        Ok(self.store.pack_dry_run(at)?)
    }

    /// Return the view as of the last transaction.
    fn latest(&self) -> Snapshot<'_> {
        self.as_of(self.last_tid())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fresh_dir;

    #[test]
    fn a_diff_against_a_deletion_is_one_change_of_the_whole_state() {
        let dir = fresh_dir("deletion");
        let record = b"\x80\x03cm\nC\nq\x00.\x80\x03K\x07.";
        {
            let store = Store::open(&dir).unwrap();
            for (tid, data) in [(Tid(0x10), Some(&record[..])), (Tid(0x20), None)] {
                let mut txn = store.tpc_begin_restore(tid, b"", b"", b"").unwrap();
                txn.restore(Oid(1), data).unwrap();
                txn.vote().unwrap();
                txn.finish().unwrap();
            }
        }
        let folder = Folder::open(&dir).unwrap();
        let history = folder.history(Oid(1)).unwrap();
        assert_eq!(
            history.iter().map(|r| (r.tid, r.size)).collect::<Vec<_>>(),
            [(Tid(0x20), 0), (Tid(0x10), record.len() as u64)]
        );
        let whole = |before: Option<serde_json::Value>, after: Option<serde_json::Value>| {
            vec![Change {
                path: String::new(),
                before,
                after,
            }]
        };
        let seven = Some(serde_json::json!(7));
        assert_eq!(
            folder.diff(Oid(1), Tid(0x10), Tid(0x20)).unwrap(),
            whole(seven.clone(), None)
        );
        assert_eq!(
            folder.diff(Oid(1), Tid(0x20), Tid(0x10)).unwrap(),
            whole(None, seven)
        );
        assert!(
            folder
                .diff(Oid(1), Tid(0x20), Tid(0x30))
                .unwrap()
                .is_empty()
        );
        // An oid without a record is still an error.
        assert!(matches!(
            folder.diff(Oid(2), Tid(0x10), Tid(0x20)),
            Err(Error::Store(ZoeError::NotFound { .. }))
        ));
    }

    #[test]
    fn a_folder_without_a_store_is_no_store() {
        let missing = std::env::temp_dir().join(format!("zoe-loupe-none-{}", std::process::id()));
        let error = Folder::open(&missing).err();
        assert!(matches!(error, Some(Error::NoStore { .. })), "{error:?}");
    }

    #[test]
    fn a_folder_repairs_a_store_that_a_writer_left_open() {
        let root = fresh_dir("repair");
        let (live, copy) = (root.join("live"), root.join("copy"));
        // A copy of the database of an open store is what a crash leaves.
        let store = Store::open(&live).unwrap();
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.store(Oid(0), Tid(0), b"root").unwrap();
        txn.vote().unwrap();
        let tid = txn.finish().unwrap();
        std::fs::create_dir_all(&copy).unwrap();
        std::fs::copy(live.join("zoe.redb"), copy.join("zoe.redb")).unwrap();
        drop(store);

        let folder = Folder::open(&copy).unwrap();
        assert!(folder.store().repaired());
        assert_eq!(folder.last_tid(), tid);
        drop(folder);
        let folder = Folder::open(&copy).unwrap();
        assert!(!folder.store().repaired());
    }

    #[test]
    fn a_folder_opens_with_the_blob_directory_of_the_store() {
        use std::io::Write;
        let root = fresh_dir("blobs");
        let (dir, blobs) = (root.join("store"), root.join("elsewhere"));
        let options = Options {
            blob_dir: Some(blobs.clone()),
            ..Options::default()
        };
        let store = Store::open_with(&dir, options).unwrap();
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        let mut file = store.new_blob_file().unwrap();
        file.write_all(b"blob").unwrap();
        let blob = b"\x80\x03cZODB.blob\nBlob\nq\x00.\x80\x03N.";
        txn.store_blob(Oid(1), Tid(0), blob, file).unwrap();
        txn.vote().unwrap();
        txn.finish().unwrap();
        drop(store);

        // The folder reads the blob directory that the call gives.
        let folder = Folder::open_with_blob_dir(&dir, Some(&blobs)).unwrap();
        folder.store().check_blob_dir().unwrap();
        drop(folder);

        // Without it, the blob directory is `blobs` in the folder, and the
        // open creates nothing there.
        let folder = Folder::open(&dir).unwrap();
        assert!(matches!(
            folder.store().check_blob_dir(),
            Err(ZoeError::MissingBlobDir { .. })
        ));
        assert!(!dir.join("blobs").exists());
    }
}
