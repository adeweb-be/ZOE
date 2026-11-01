//! The values that cross the IPC boundary: an object, its references, the
//! revisions of an object, and the text of a transaction.

use serde::Serialize;
use zoe_core::{Oid, Tid};

use crate::ids::{serialize_oid, serialize_tid};

/// The most bytes of the user or the description of a revision.
pub const MAX_NOTE: usize = 1 << 12;

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

#[cfg(test)]
mod tests {
    use super::*;

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
