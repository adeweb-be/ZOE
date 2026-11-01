//! The Rust side of Loupe, the desktop inspector and editor. The window
//! code lives in `apps/loupe/`.
//!
//! [`Folder`] opens a store folder for reads only and holds the lock of
//! the store. It shows each object as an [`Object`]: the class, the oid,
//! the serial, the size, the decoded state as JSON and the outbound
//! references. It resolves the paths of a Plone site. It writes to the
//! folder only to repair a store that a writer left open, at the open.
//!
//! Loupe also travels in time. [`Folder::as_of`] gives a [`Snapshot`]: the
//! store as it was after a transaction. [`Folder::history`] lists the
//! revisions of an object, and [`Folder::diff`] compares the decoded
//! states of two revisions.
//!
//! The space screen measures the store. [`Folder::space`] gives the space
//! of each class, the blobs, the largest records and the growth.
//! [`Folder::path_size`] and [`Folder::path_space`] give the size of a
//! path and of its items. [`Folder::reclaimable`] gives what a pack would
//! remove. A long call takes a progress function that can stop it.

mod diff;
mod path;
mod space;
mod state;
mod text;

use std::ops::ControlFlow;
use std::path::Path;

use serde::{Serialize, Serializer};
use zoe_core::{Oid, Options, PackReport, Store, Tid, ZoeError};

pub use diff::Change;
pub use space::{
    Growth, ITEM_LIMIT, Large, Others, PathItem, PathSpace, Progress, Reclaim, Space, Totals, Walk,
};

/// The most revisions that [`Folder::history`] gives, newest first.
pub const HISTORY_LIMIT: usize = 1 << 12;

/// The most bytes of the user or the description of a revision.
pub const MAX_NOTE: usize = 1 << 12;

/// An error of Loupe.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The folder holds no store.
    #[error("{path} holds no store")]
    NoStore {
        /// The path of the folder.
        path: String,
    },

    /// The store failed. For example, another process holds the lock, the
    /// repair of a store that was not closed cleanly failed, or the oid has
    /// no record.
    #[error(transparent)]
    Store(#[from] ZoeError),

    /// A path leads to no object.
    #[error("{path}: {segment} leads to no object")]
    NoObject {
        /// The path.
        path: String,
        /// The segment of the path where the walk stops.
        segment: String,
    },

    /// zoe-pickle refuses a record that a path goes through.
    #[error("oid {}: {source}", .oid.repr())]
    Refused {
        /// The oid of the record.
        oid: Oid,
        /// The refusal of the decoder.
        source: zoe_pickle::Error,
    },

    /// A text is not an oid or a tid in ZODB form.
    #[error("{0}: not a ZODB id")]
    BadId(String),

    /// The object has no record at or before the tid of a view. A later
    /// transaction creates it.
    #[error("oid {}: no record as of tid {}", .oid.repr(), .tid.repr())]
    NoRecordAsOf {
        /// The oid of the object.
        oid: Oid,
        /// The tid of the view.
        tid: Tid,
    },

    /// The progress function of a long call stopped it.
    #[error("the walk stopped before its end")]
    Stopped,
}

/// The result type of Loupe.
pub type Result<T> = std::result::Result<T, Error>;

/// One object of the store, as Loupe shows it.
///
/// In JSON, `oid`, `serial` and the oids of the references are ZODB-form
/// strings, for example `0x00`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Object {
    /// The oid of the object.
    #[serde(serialize_with = "serialize_oid")]
    pub oid: Oid,
    /// The tid of the current record.
    #[serde(serialize_with = "serialize_tid")]
    pub serial: Tid,
    /// The module and the name of the class, from the class pickle. It is
    /// `None` for a persistent class or a record that does not decode.
    pub class: Option<String>,
    /// The size of the record in bytes.
    pub size: u64,
    /// The decoded state. The crate documentation of `state` gives the
    /// form of each value. It is null when the record does not decode.
    pub state: serde_json::Value,
    /// The strong outbound references, in pickle order, as ZODB's
    /// `referencesf` gives them. The references inside opaque nodes are in
    /// the list too. It is empty when `error` is set.
    pub references: Vec<Reference>,
    /// The refusal of the decoder, when the record does not decode, or when
    /// its references do not decode. Then the references are not known.
    pub error: Option<String>,
}

/// An outbound reference of an object.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Reference {
    /// The oid that the reference leads to.
    #[serde(serialize_with = "serialize_oid")]
    pub oid: Oid,
    /// A JSON pointer into the state of the object. The value at the
    /// pointer holds the reference: its place when the state shows it,
    /// else a value that holds every place that the state does not show.
    /// A reference from the class pickle, as an instance of a persistent
    /// class has, is the exception: the state does not hold it, and it
    /// points to the root, `""`.
    pub at: String,
}

/// One revision of an object: a record and the transaction that wrote
/// it.
///
/// In JSON, `tid` is a ZODB-form string.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Revision {
    /// The tid of the transaction that wrote the record.
    #[serde(serialize_with = "serialize_tid")]
    pub tid: Tid,
    /// The user name of the transaction.
    pub user: String,
    /// The description of the transaction.
    pub description: String,
    /// The size of the record in bytes. A deletion has size 0.
    pub size: u64,
}

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
        Snapshot { folder: self, tid }
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

/// A view of a store folder as of a tid. It shows each object as it was
/// after the last transaction at or before that tid.
///
/// Every read of a view loads with `load_before` at the same tid. So a
/// view never mixes the revisions of two tids.
#[derive(Clone, Copy)]
pub struct Snapshot<'a> {
    folder: &'a Folder,
    tid: Tid,
}

impl Snapshot<'_> {
    /// Return the tid of the view.
    pub fn tid(&self) -> Tid {
        self.tid
    }

    /// Return the root object, oid 0.
    pub fn root(&self) -> Result<Object> {
        self.object(Oid(0))
    }

    /// Return the object `oid`.
    ///
    /// A record that zoe-pickle refuses gives an object with the refusal
    /// in `error`. So does a record whose references `persistent_ids`
    /// refuses, with its state. An oid without a record at or before the
    /// tid of the view is an error.
    pub fn object(&self, oid: Oid) -> Result<Object> {
        let (data, serial) = self.load(oid)?;
        let size = data.len() as u64;
        let object = match zoe_pickle::decode_record(&data) {
            Ok(tree) => {
                let class = tree
                    .class_name()
                    .map(|(module, name)| format!("{module}.{name}"));
                let (state, found) = state::state(&tree);
                // `persistent_ids` decodes the record again. Drop the first
                // tree, so that one tree at a time is in memory. The tree
                // held at most the default node budget, so the second
                // decode also stays inside it.
                drop(tree);
                let (references, error) = match strong_references(&data) {
                    Ok(oids) => (found.place(&oids), None),
                    Err(error) => (Vec::new(), Some(error)),
                };
                Object {
                    oid,
                    serial,
                    class,
                    size,
                    state,
                    references,
                    error,
                }
            }
            Err(e) => Object {
                oid,
                serial,
                class: None,
                size,
                state: serde_json::Value::Null,
                references: Vec::new(),
                error: Some(e.to_string()),
            },
        };
        Ok(object)
    }

    /// Return the object that the Plone path `path` leads to. Each step of
    /// the path reads as of the tid of the view. [`Folder::resolve`] gives
    /// the form of a path.
    pub fn resolve(&self, path: &str) -> Result<Object> {
        self.object(path::resolve(self, path)?)
    }

    /// Return the record of `oid` and its serial.
    fn load(&self, oid: Oid) -> Result<(Vec<u8>, Tid)> {
        // `load_before` gives the newest record before its tid, and the
        // view includes its own tid. `Tid::MAX` is after every
        // transaction, so the saturated add loses no record.
        let before = Tid(self.tid.0.saturating_add(1));
        match self.folder.store.load_before(oid, before)? {
            Some(record) => Ok((record.data, record.serial)),
            None => Err(Error::NoRecordAsOf { oid, tid: self.tid }),
        }
    }

    /// Return the record of `oid`, decoded.
    pub(crate) fn tree(&self, oid: Oid) -> Result<zoe_pickle::Tree> {
        let (data, _) = self.load(oid)?;
        zoe_pickle::decode_record(&data).map_err(|source| Error::Refused { oid, source })
    }

    /// Return the record of `oid`, decoded, or `None` when the record of
    /// `oid` at or before the tid of the view is a deletion.
    fn tree_or_deletion(&self, oid: Oid) -> Result<Option<zoe_pickle::Tree>> {
        match self.tree(oid) {
            Ok(tree) => Ok(Some(tree)),
            // `load_before` gives `NotFound` for a deletion and for an oid
            // without a record. Only the oid of a deletion has a history.
            Err(Error::Store(ZoeError::NotFound { .. }))
                if self.folder.store.history(oid, 1).is_ok() =>
            {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }
}

/// Return the oids of the strong persistent references of `record`, in
/// pickle order, as pack reads them. Weak references, references to
/// another database and the other lists that ZODB's `referencesf` skips
/// are not in the list. An oid that is not 8 bytes is an error, as it is
/// for pack.
fn strong_references(record: &[u8]) -> std::result::Result<Vec<Oid>, String> {
    let ids = zoe_pickle::persistent_ids(record)
        .map_err(|e| format!("the references do not decode: {e}"))?;
    ids.iter()
        .filter_map(zoe_pickle::PersistentId::oid_u64)
        .map(|oid| {
            oid.map(Oid)
                .map_err(|_| "the references do not decode: an oid is not 8 bytes".to_owned())
        })
        .collect()
}

/// Return the user or the description of a transaction as text, with at
/// most `MAX_NOTE` bytes of the record. Bytes that are not UTF-8 become
/// U+FFFD. A longer text shows its start, cut before a character, and
/// ends with `…`.
pub fn note(bytes: &[u8]) -> String {
    if bytes.len() <= MAX_NOTE {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    // Cut before a UTF-8 character, not inside it. A continuation byte is
    // `0b10xxxxxx`, and a character has at most three of them.
    let mut end = MAX_NOTE;
    while end > MAX_NOTE - 3 && bytes[end] & 0xc0 == 0x80 {
        end -= 1;
    }
    let mut text = String::from_utf8_lossy(&bytes[..end]).into_owned();
    text.push('…');
    text
}

/// Return `oid` in ZODB form, as `ZODB.utils.oid_repr` writes it, for
/// example `0x00` or `0x0100`.
pub fn oid_repr(oid: Oid) -> String {
    oid.repr()
}

/// Return `tid` in ZODB form, for example `0x033f2eeca6666666`.
pub fn tid_repr(tid: Tid) -> String {
    tid.repr()
}

/// Read an oid in ZODB form: `0x` and 1 to 16 hex digits.
pub fn parse_oid(text: &str) -> Result<Oid> {
    Oid::parse(text).ok_or_else(|| Error::BadId(text.to_owned()))
}

/// Read a tid in ZODB form: `0x` and 1 to 16 hex digits.
pub fn parse_tid(text: &str) -> Result<Tid> {
    Tid::parse(text).ok_or_else(|| Error::BadId(text.to_owned()))
}

fn serialize_oid<S: Serializer>(oid: &Oid, serializer: S) -> std::result::Result<S::Ok, S::Error> {
    serializer.serialize_str(&oid_repr(*oid))
}

fn serialize_tid<S: Serializer>(tid: &Tid, serializer: S) -> std::result::Result<S::Ok, S::Error> {
    serializer.serialize_str(&tid_repr(*tid))
}

#[cfg(test)]
mod tests {
    use zoe_pickle::testing::{record_with_put, reference_to_put as reference};

    use super::*;

    #[test]
    fn ids_have_the_zodb_form() {
        assert_eq!(oid_repr(Oid(0)), "0x00");
        assert_eq!(oid_repr(Oid(1)), "0x01");
        assert_eq!(oid_repr(Oid(0x76)), "0x76");
        assert_eq!(oid_repr(Oid(0x100)), "0x0100");
        assert_eq!(tid_repr(Tid(0x033f2eeca6666666)), "0x033f2eeca6666666");
        assert_eq!(oid_repr(Oid(u64::MAX)), "0xffffffffffffffff");
    }

    #[test]
    fn an_oid_in_zodb_form_parses() {
        for oid in [0, 1, 0x76, 0x100, u64::MAX] {
            assert_eq!(parse_oid(&oid_repr(Oid(oid))).unwrap(), Oid(oid));
        }
        assert_eq!(parse_oid("0x1").unwrap(), Oid(1));
        for bad in [
            "",
            "0x",
            "12",
            "0xg1",
            "0x00000000000000001",
            "0x-1",
            "0x+1",
        ] {
            assert!(parse_oid(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_tid_in_zodb_form_parses() {
        let tid = Tid(0x033f2eeca6666666);
        assert_eq!(parse_tid(&tid_repr(tid)).unwrap(), tid);
        assert!(matches!(parse_tid("latest"), Err(Error::BadId(_))));
    }

    #[test]
    fn a_long_note_shows_its_start() {
        assert_eq!(note(b"edit"), "edit");
        assert_eq!(note(b"caf\xe9"), "caf\u{fffd}");
        let long = note(&b"a".repeat(MAX_NOTE + 1));
        assert_eq!(long, format!("{}\u{2026}", "a".repeat(MAX_NOTE)));
        // The cut does not split a character.
        let text = format!("{}\u{e9}a", "a".repeat(MAX_NOTE - 1));
        assert_eq!(
            note(text.as_bytes()),
            format!("{}\u{2026}", "a".repeat(MAX_NOTE - 1))
        );
    }

    #[test]
    fn a_revision_serializes_with_a_zodb_tid() {
        let revision = Revision {
            tid: Tid(0x0100),
            user: "editor".to_owned(),
            description: "edit".to_owned(),
            size: 3,
        };
        assert_eq!(
            serde_json::to_value(&revision).unwrap(),
            serde_json::json!({"tid": "0x0100", "user": "editor", "description": "edit", "size": 3})
        );
    }

    #[test]
    fn a_view_before_the_first_record_of_an_object_has_no_record() {
        let dir = std::env::temp_dir().join(format!("zoe-loupe-as-of-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        {
            let store = Store::open(&dir).unwrap();
            for (tid, oid) in [(Tid(0x10), Oid(0)), (Tid(0x20), Oid(1))] {
                let mut txn = store.tpc_begin_restore(tid, b"", b"", b"").unwrap();
                txn.restore(oid, Some(b"\x80\x03cm\nC\nq\x00.\x80\x03N."))
                    .unwrap();
                txn.vote().unwrap();
                txn.finish().unwrap();
            }
        }
        let folder = Folder::open(&dir).unwrap();
        let error = folder.as_of(Tid(0x1f)).object(Oid(1)).err();
        assert!(
            matches!(
                error,
                Some(Error::NoRecordAsOf {
                    oid: Oid(1),
                    tid: Tid(0x1f)
                })
            ),
            "{error:?}"
        );
        assert_eq!(
            folder.as_of(Tid(0x20)).object(Oid(1)).unwrap().serial,
            Tid(0x20)
        );
        assert_eq!(
            folder.as_of(Tid(u64::MAX)).object(Oid(1)).unwrap().serial,
            Tid(0x20)
        );
        let history = folder.history(Oid(1)).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].tid, Tid(0x20));
    }

    #[test]
    fn a_diff_against_a_deletion_is_one_change_of_the_whole_state() {
        let dir = std::env::temp_dir().join(format!("zoe-loupe-deletion-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
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

    /// A store with one transaction that writes `records`, by oid.
    fn store_with(name: &str, records: &[(u64, Vec<u8>)]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("zoe-loupe-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(&dir).unwrap();
        let mut txn = store.tpc_begin_restore(Tid(0x10), b"", b"", b"").unwrap();
        for (oid, data) in records {
            txn.restore(Oid(*oid), Some(data)).unwrap();
        }
        txn.vote().unwrap();
        txn.finish().unwrap();
        dir
    }

    /// A record of the class `m.C` with the state pickle `state`. Its
    /// class pickle has BINPUT 0.
    fn record(state: &[u8]) -> Vec<u8> {
        record_with_put("m", "C", state)
    }

    #[test]
    fn the_references_inside_a_plain_object_are_references() {
        // The root holds 1, a plain object that holds 2, and 3. ZODB's
        // `referencesf` gives 1, 2 and 3, and pack keeps 2.
        let plain = [
            &b"cm\nPlain\n)\x81}X\x01\x00\x00\x00x"[..],
            &reference(2),
            b"sb",
        ]
        .concat();
        let root = record(
            &[
                &b"}(X\x01\x00\x00\x00a"[..],
                &reference(1),
                b"X\x01\x00\x00\x00b",
                &plain,
                b"X\x01\x00\x00\x00c",
                &reference(3),
                b"u",
            ]
            .concat(),
        );
        let empty = record(b"}");
        let records = [
            (0, root),
            (1, empty.clone()),
            (2, empty.clone()),
            (3, empty),
        ];
        let folder = Folder::open(store_with("plain", &records)).unwrap();
        let root = folder.root().unwrap();
        assert_eq!(root.error, None);
        let found: Vec<(u64, &str)> = root
            .references
            .iter()
            .map(|r| (r.oid.0, r.at.as_str()))
            .collect();
        assert_eq!(found, [(1, "/a"), (2, "/b"), (3, "/c")]);
        assert!(root.state.pointer("/b/$opaque").is_some(), "{root:?}");
    }

    #[test]
    fn references_that_do_not_decode_are_an_error_beside_the_state() {
        // A persistent id that is an int, and an oid of 3 bytes. The state
        // decodes. `referencesf` and pack refuse both records.
        let records = [(0, record(b"K\x05Q")), (1, record(b"C\x03abcQ"))];
        let folder = Folder::open(store_with("bad-references", &records)).unwrap();
        for oid in [Oid(0), Oid(1)] {
            let object = folder.object(oid).unwrap();
            assert!(object.state.is_object(), "{object:?}");
            assert!(object.references.is_empty(), "{object:?}");
            let error = object.error.unwrap_or_default();
            assert!(error.starts_with("the references do not decode"), "{error}");
        }
    }

    #[test]
    fn a_folder_without_a_store_is_no_store() {
        let missing = std::env::temp_dir().join(format!("zoe-loupe-none-{}", std::process::id()));
        let error = Folder::open(&missing).err();
        assert!(matches!(error, Some(Error::NoStore { .. })), "{error:?}");
    }

    #[test]
    fn a_folder_repairs_a_store_that_a_writer_left_open() {
        let root = std::env::temp_dir().join(format!("zoe-loupe-repair-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (live, copy) = (root.join("live"), root.join("copy"));
        // A copy of the database of an open store is what a crash leaves.
        let store = Store::open(&live).unwrap();
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.store(Oid(0), Tid(0), b"root").unwrap();
        txn.vote().unwrap();
        let tid = txn.finish().unwrap();
        // The commit goes into the tables, so the copy of the database file
        // alone holds it.
        store.fold().unwrap();
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
    fn an_object_serializes_with_zodb_ids() {
        let object = Object {
            oid: Oid(1),
            serial: Tid(0x0100),
            class: Some("m.C".to_owned()),
            size: 3,
            state: serde_json::json!({"a": 1}),
            references: vec![Reference {
                oid: Oid(2),
                at: "/a".to_owned(),
            }],
            error: None,
        };
        assert_eq!(
            serde_json::to_value(&object).unwrap(),
            serde_json::json!({
                "oid": "0x01",
                "serial": "0x0100",
                "class": "m.C",
                "size": 3,
                "state": {"a": 1},
                "references": [{"oid": "0x02", "at": "/a"}],
                "error": null,
            })
        );
    }

    #[test]
    fn a_folder_opens_with_the_blob_directory_of_the_store() {
        use std::io::Write;
        let root = std::env::temp_dir().join(format!("zoe-loupe-blobs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
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
