//! The Rust side of Loupe, the desktop inspector and editor. The window
//! code lives in `apps/loupe/`.
//!
//! [`Folder`] opens a store folder for reads only and holds the lock of
//! the store. It shows each object as an [`Object`]: the class, the oid,
//! the serial, the size, the decoded state as JSON and the outbound
//! references. It resolves the paths of a Plone site. It never writes to
//! the folder.

mod path;
mod state;
mod text;

use std::path::Path;

use serde::{Serialize, Serializer};
use zoe_core::{Oid, Store, Tid, ZoeError};

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
    /// store needs a repair, or the oid has no record.
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
    #[error("oid {}: {source}", id_repr(.oid.0))]
    Refused {
        /// The oid of the record.
        oid: Oid,
        /// The refusal of the decoder.
        source: zoe_pickle::Error,
    },

    /// A text is not an oid or a tid in ZODB form.
    #[error("{0}: not a ZODB id")]
    BadId(String),
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
    /// The outbound references, in the order of the state.
    pub references: Vec<Reference>,
    /// The refusal of the decoder, when the record does not decode.
    pub error: Option<String>,
}

/// An outbound reference of an object.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Reference {
    /// The oid that the reference leads to.
    #[serde(serialize_with = "serialize_oid")]
    pub oid: Oid,
    /// A JSON pointer to the reference in the state of the object.
    pub at: String,
}

/// A store folder, open for reads only.
///
/// The folder holds the lock of the store while it is open. So a running
/// instance and Loupe never share a store.
pub struct Folder {
    store: Store,
}

impl Folder {
    /// Open the store in the folder `path` for reads only.
    ///
    /// The call fails with `NoStore` when `path` holds no store, and with
    /// `Locked` when another process holds the lock of the store. A store
    /// that was not closed cleanly fails with `NeedsRepair`. The call
    /// writes nothing.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let store = Store::open_read_only(path).map_err(|e| match e {
            ZoeError::Io(e) if e.kind() == std::io::ErrorKind::NotFound => Error::NoStore {
                path: path.display().to_string(),
            },
            e => Error::Store(e),
        })?;
        Ok(Self { store })
    }

    /// Return the tid of the last transaction of the store.
    pub fn last_tid(&self) -> Tid {
        self.store.last_transaction()
    }

    /// Return the root object, oid 0.
    pub fn root(&self) -> Result<Object> {
        self.object(Oid(0))
    }

    /// Return the object `oid` as of the last transaction.
    ///
    /// A record that zoe-pickle refuses gives an object with the refusal
    /// in `error`. An oid without a record is an error.
    pub fn object(&self, oid: Oid) -> Result<Object> {
        let (data, serial) = self.store.load(oid)?;
        let size = data.len() as u64;
        let object = match zoe_pickle::decode_record(&data) {
            Ok(tree) => {
                let class = tree
                    .class_name()
                    .map(|(module, name)| format!("{module}.{name}"));
                let (state, references) = state::state(&tree);
                Object {
                    oid,
                    serial,
                    class,
                    size,
                    state,
                    references,
                    error: None,
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

    /// Return the object that the Plone path `path` leads to.
    ///
    /// A path starts at the Zope application, `root["Application"]`. Each
    /// segment is an item of the `_tree` of a folder, a key of a BTree or a
    /// mapping, or an attribute that holds a reference. So `/plone/folder`
    /// and `/plone/folder/doc/__annotations__/key` resolve.
    pub fn resolve(&self, path: &str) -> Result<Object> {
        self.object(path::resolve(self, path)?)
    }

    /// Return the current record of `oid`, decoded.
    pub(crate) fn tree(&self, oid: Oid) -> Result<zoe_pickle::Tree> {
        let (data, _) = self.store.load(oid)?;
        zoe_pickle::decode_record(&data).map_err(|source| Error::Refused { oid, source })
    }
}

/// Return an 8-byte id in ZODB form, as `ZODB.utils.oid_repr` writes it:
/// `0x`, then the hex digits without leading zeros, padded to an even
/// count. Zero is `0x00`.
fn id_repr(id: u64) -> String {
    let digits = format!("{id:x}");
    let pad = if digits.len() % 2 == 1 { "0" } else { "" };
    format!("0x{pad}{digits}")
}

/// Return `oid` in ZODB form, for example `0x00` or `0x0100`.
pub fn oid_repr(oid: Oid) -> String {
    id_repr(oid.0)
}

/// Return `tid` in ZODB form, for example `0x033f2eeca6666666`.
pub fn tid_repr(tid: Tid) -> String {
    id_repr(tid.0)
}

/// Read an oid in ZODB form: `0x` and 1 to 16 hex digits.
pub fn parse_oid(text: &str) -> Result<Oid> {
    let digits = text
        .strip_prefix("0x")
        .filter(|digits| (1..=16).contains(&digits.len()))
        .filter(|digits| digits.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or_else(|| Error::BadId(text.to_owned()))?;
    u64::from_str_radix(digits, 16)
        .map(Oid)
        .map_err(|_| Error::BadId(text.to_owned()))
}

fn serialize_oid<S: Serializer>(oid: &Oid, serializer: S) -> std::result::Result<S::Ok, S::Error> {
    serializer.serialize_str(&oid_repr(*oid))
}

fn serialize_tid<S: Serializer>(tid: &Tid, serializer: S) -> std::result::Result<S::Ok, S::Error> {
    serializer.serialize_str(&tid_repr(*tid))
}

#[cfg(test)]
mod tests {
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
    fn a_folder_without_a_store_is_no_store() {
        let missing = std::env::temp_dir().join(format!("zoe-loupe-none-{}", std::process::id()));
        let error = Folder::open(&missing).err();
        assert!(matches!(error, Some(Error::NoStore { .. })), "{error:?}");
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
}
