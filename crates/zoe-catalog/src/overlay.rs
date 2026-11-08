//! The transaction overlay of a query (ADR 0059, section 8): the records
//! that a transaction changed and did not commit, sent with one query.
//!
//! The overlay holds the pickles of the objects that the Connection
//! registered, the records of its savepoint storage that no registered
//! object replaces, and the new objects that they reach. A new object that
//! no savepoint stored has a provisional oid: the high bit set, then a
//! counter of the transaction. The persistent ids that point to it carry
//! that oid.
//!
//! The mirror reads the overlay as it reads a commit (`read_records`), over
//! the version at the tid of the query, and gives the overrides of that
//! version with the overlay. These overrides serve the one query. The
//! overlay never enters the change sets, and no other query sees it.
//!
//! The read takes the shapes of the trees (the children of each inner
//! node, the key range of each bucket) of the last commit that the mirror
//! applied. A commit after the tid of the query can have changed the shape
//! of a node of the overlay. The shapes keep the tid of the last change of
//! each node, so the read refuses such a node (`overlay`): the shape at the
//! tid of the query is not known. The thread forgets the changes at or
//! before the floor of the versions, so a query at a tid before the floor
//! that it pruned at is refused too.
//!
//! On the wire, an overlay is nil or `[[oid, pickle]...]`: each oid is a
//! bin of 8 bytes, each pickle a bin.

use std::collections::HashMap;
use std::time::Instant;

use rmpv::Value;

use crate::base::Base;
use crate::change::{ChangeSet, Overrides, Owner, Shapes, read_records};
use crate::reason::{Fallback, Reason};

/// The high bit of a provisional oid.
pub const PROVISIONAL: u64 = 1 << 63;

/// The records of an overlay: the oid and the pickle of each.
pub type Records = Vec<(u64, Vec<u8>)>;

/// Tells if `oid` is a provisional oid.
pub fn is_provisional(oid: u64) -> bool {
    oid & PROVISIONAL != 0
}

/// Reads the argument `overlay` of a `catalog_query` call: `None` for nil.
/// Another form is an error: its text. The frame decoder bounded the value
/// before.
pub fn decode(value: &Value) -> Result<Option<Records>, String> {
    let items = match value {
        Value::Nil => return Ok(None),
        Value::Array(items) => items,
        _ => return Err("the overlay is not nil or an array".into()),
    };
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let Value::Array(pair) = item else {
            return Err("a record of the overlay is not an array".into());
        };
        let [Value::Binary(oid), Value::Binary(pickle)] = pair.as_slice() else {
            return Err("a record of the overlay is not [oid, pickle]".into());
        };
        let oid = <[u8; 8]>::try_from(oid.as_slice())
            .map_err(|_| "an oid of the overlay is not 8 bytes".to_owned())?;
        out.push((u64::from_be_bytes(oid), pickle.clone()));
    }
    Ok(Some(out))
}

/// The bytes that the host holds for the records of an overlay during its
/// query, estimated: two copies of each pickle (the value of the call and
/// the records), and the oid of each.
pub fn heap_bytes(records: &[(u64, Vec<u8>)]) -> usize {
    records.iter().map(|(_, p)| 2 * p.len() + 48).sum()
}

/// The overrides of the version of `base` with `over` at the tid `v`, with
/// the records of an overlay over them. `shapes` and `objects` are those
/// of the maintenance of the mirror, and `moved` gives the tid of the last
/// change of the shape of each node. A record that does not read, a change
/// of the catalog object or of an index object, and a node whose shape
/// changed after `v` give the reason `overlay`.
pub fn read(
    base: &Base,
    over: &Overrides,
    shapes: &Shapes,
    objects: &HashMap<u64, Vec<u8>>,
    moved: &HashMap<u64, u64>,
    v: u64,
    records: &[(u64, Vec<u8>)],
) -> Result<Overrides, Fallback> {
    let after = |oid: &u64| moved.get(oid).is_some_and(|t| *t > v);
    for (oid, _) in records {
        if !matches!(shapes.owners.get(oid), Some(Owner::Node(_))) {
            continue;
        }
        let children = shapes.inner.get(oid).into_iter().flatten();
        if let Some(node) = std::iter::once(oid).chain(children).find(|o| after(o)) {
            return Err(Fallback::new(
                Reason::Overlay,
                format!("a commit after the tid changed the shape of the node {node:#x}"),
            ));
        }
    }
    let records: Vec<(u64, Option<&[u8]>)> = records
        .iter()
        .map(|(oid, pickle)| (*oid, Some(pickle.as_slice())))
        .collect();
    let read = read_records(base, over, shapes, objects, &records);
    if let Some(why) = read.stale {
        return Err(Fallback::new(Reason::Overlay, why));
    }
    let mut out = over.clone();
    out.forward(&ChangeSet {
        tid: v,
        at: Instant::now(),
        changes: read.changes,
        lengths: read.lengths,
    });
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bin(bytes: &[u8]) -> Value {
        Value::Binary(bytes.to_vec())
    }

    #[test]
    fn the_overlay_reads_nil_or_pairs_of_an_oid_and_a_pickle() {
        assert_eq!(decode(&Value::Nil), Ok(None));
        assert_eq!(decode(&Value::Array(vec![])), Ok(Some(vec![])));
        let oid = (PROVISIONAL | 3).to_be_bytes();
        let pair = Value::Array(vec![bin(&oid), bin(b"pickle")]);
        assert_eq!(
            decode(&Value::Array(vec![pair])),
            Ok(Some(vec![(PROVISIONAL | 3, b"pickle".to_vec())]))
        );
        assert!(is_provisional(PROVISIONAL | 3));
        assert!(!is_provisional(3));
    }

    #[test]
    fn an_overlay_of_another_form_is_an_error() {
        for bad in [
            Value::from(1),
            Value::Array(vec![Value::Nil]),
            Value::Array(vec![Value::Array(vec![bin(&[0; 8])])]),
            Value::Array(vec![Value::Array(vec![bin(&[0; 7]), bin(b"")])]),
            Value::Array(vec![Value::Array(vec![bin(&[0; 8]), Value::from("p")])]),
            Value::Array(vec![Value::Array(vec![bin(&[0; 8]), bin(b""), bin(b"")])]),
        ] {
            assert!(decode(&bad).is_err(), "{bad:?}");
        }
    }
}
