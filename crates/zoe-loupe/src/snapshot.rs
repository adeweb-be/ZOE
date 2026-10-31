//! A view of a store folder as of a tid, and the reads of a record in it.
//!
//! Each read of a record goes through `Snapshot::load`, which loads with
//! `load_before` at the tid of the view. The other reads change only what
//! they give for a record that does not decode, or for an oid without a
//! record in the view.

use zoe_core::{Oid, Tid, ZoeError};
use zoe_pickle::Tree;

use crate::folder::Folder;
use crate::model::Object;
use crate::{Error, Result, path, state};

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

impl<'a> Snapshot<'a> {
    /// Return the view of `folder` as of `tid`.
    pub(crate) fn new(folder: &'a Folder, tid: Tid) -> Self {
        Snapshot { folder, tid }
    }
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

    /// Return the record of `oid` and its serial. Each other read of a
    /// record calls this one.
    fn load(&self, oid: Oid) -> Result<(Vec<u8>, Tid)> {
        // `load_before` gives the newest record before its tid, and the
        // view includes its own tid. `Tid::MAX` is after every
        // transaction, so the saturated add loses no record.
        let before = Tid(self.tid.0.saturating_add(1));
        match self.folder.store().load_before(oid, before)? {
            Some(record) => Ok((record.data, record.serial)),
            None => Err(Error::NoRecordAsOf { oid, tid: self.tid }),
        }
    }

    /// Return the record of `oid` and its serial, or `None` when `oid` has
    /// no record in the view or its record is a deletion.
    pub(crate) fn record(&self, oid: Oid) -> Result<Option<(Vec<u8>, Tid)>> {
        match self.load(oid) {
            Ok(found) => Ok(Some(found)),
            Err(Error::NoRecordAsOf { .. } | Error::Store(ZoeError::NotFound { .. })) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Return the record of `oid`, decoded.
    pub(crate) fn tree(&self, oid: Oid) -> Result<Tree> {
        let (data, _) = self.load(oid)?;
        zoe_pickle::decode_record(&data).map_err(|source| Error::Refused { oid, source })
    }

    /// Return the record of `oid`, decoded, or `None` when it does not
    /// decode or when `oid` has no record in the view.
    pub(crate) fn readable_tree(&self, oid: Oid) -> Result<Option<Tree>> {
        Ok(self
            .record(oid)?
            .and_then(|(data, _)| zoe_pickle::decode_record(&data).ok()))
    }

    /// Return the record of `oid`, decoded, or `None` when the record of
    /// `oid` at or before the tid of the view is a deletion.
    pub(crate) fn tree_or_deletion(&self, oid: Oid) -> Result<Option<Tree>> {
        match self.tree(oid) {
            Ok(tree) => Ok(Some(tree)),
            // `load_before` gives `NotFound` for a deletion and for an oid
            // without a record. Only the oid of a deletion has a history.
            Err(Error::Store(ZoeError::NotFound { .. }))
                if self.folder.store().history(oid, 1).is_ok() =>
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
pub(crate) fn strong_references(record: &[u8]) -> std::result::Result<Vec<Oid>, String> {
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

#[cfg(test)]
mod tests {
    use zoe_core::Store;
    use zoe_core::testing::TempDir;
    use zoe_pickle::testing::{record_with_put, reference_to_put as reference};

    use super::*;
    use crate::fresh_dir;

    #[test]
    fn a_view_before_the_first_record_of_an_object_has_no_record() {
        let dir = fresh_dir("as-of");
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

    /// A store with one transaction that writes `records`, by oid.
    fn store_with(name: &str, records: &[(u64, Vec<u8>)]) -> TempDir {
        let dir = fresh_dir(name);
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
        let dir = store_with("plain", &records);
        let folder = Folder::open(&dir).unwrap();
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
        let dir = store_with("bad-references", &records);
        let folder = Folder::open(&dir).unwrap();
        for oid in [Oid(0), Oid(1)] {
            let object = folder.object(oid).unwrap();
            assert!(object.state.is_object(), "{object:?}");
            assert!(object.references.is_empty(), "{object:?}");
            let error = object.error.unwrap_or_default();
            assert!(error.starts_with("the references do not decode"), "{error}");
        }
    }
}
