//! `records_of_classes`: find the current records of some classes.
//!
//! The walk reads the record keys of the store in chunks, and the newest
//! record of each oid, its current record. It decodes only the class
//! pickle of each current record, as `verify` does. It decodes the state
//! only of a record of one of the classes, to tell a retired record. No
//! unpickler runs.

use zoe_core::{Oid, Store, ZoeError};
use zoe_pickle::{Int, Node, decode_class_key, decode_record};

use crate::walk::OidVersions;

/// A current record of one of the classes of a walk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClassRecord {
    /// The oid of the record.
    pub oid: Oid,
    /// Its class, `module.name`.
    pub class: String,
}

/// Return the current records of `store` whose class is one of `classes`,
/// each as `module.name`, in oid order. A retired record is left out: its
/// state is a dict with one item, whose key is the int 0. The classes of
/// `zoe.plone` write this state when their uninstall gives the stock value
/// back, and no live record of these classes has it.
///
/// A record that does not decode is left out: the conversion and `verify`
/// report it.
pub fn records_of_classes(store: &Store, classes: &[&str]) -> zoe_core::Result<Vec<ClassRecord>> {
    let mut found = Vec::new();
    let mut walk = OidVersions::new(store);
    while let Some(versions) = walk.next()? {
        let Some(&(oid, tid)) = versions.last() else {
            continue;
        };
        let data = match store.load_serial(oid, tid) {
            Ok(data) => data,
            // A deletion: the oid has no current record.
            Err(ZoeError::NotFound { .. }) => continue,
            Err(error) => return Err(error),
        };
        if let Some(class) = class_of(&data, classes) {
            found.push(ClassRecord { oid, class });
        }
    }
    Ok(found)
}

/// Return the class of the record `data` when it is one of `classes` and
/// the record is not retired.
fn class_of(data: &[u8], classes: &[&str]) -> Option<String> {
    let class = decode_class_key(data).ok()??;
    if !classes.contains(&class.as_str()) || retired(data) {
        return None;
    }
    Some(class)
}

/// Tell if the state of the record `data` is a dict with one item whose
/// key is the int 0.
fn retired(data: &[u8]) -> bool {
    let Ok(tree) = decode_record(data) else {
        return false;
    };
    let Some(Node::Dict(dict)) = tree.resolve(tree.state()) else {
        return false;
    };
    let mut items = dict.entries();
    let (Some((key, _)), None) = (items.next(), items.next()) else {
        return false;
    };
    match tree.resolve(key) {
        Some(Node::Int(Int::U8(0) | Int::U16(0) | Int::I32(0))) => true,
        Some(Node::Int(Int::Long { bytes, .. })) => bytes.iter().all(|byte| *byte == 0),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use zoe_core::vfs::MemVfs;
    use zoe_core::{Oid, Store};
    use zoe_pickle::testing::record_with_put as pickle;

    use super::{ClassRecord, records_of_classes};
    use crate::testing::commit;

    const LIVE: &str = "zoe.plone.properties.PrincipalProperties";

    #[test]
    fn the_walk_finds_the_live_records_of_the_classes() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        // `{"fullname": "x"}`, `{0: None}`, `{}`, and `{0: None, 1: None}`.
        let live = pickle(
            "zoe.plone.properties",
            "PrincipalProperties",
            b"}X\x08\x00\x00\x00fullnameX\x01\x00\x00\x00xs",
        );
        let retired = pickle("zoe.plone.properties", "PrincipalProperties", b"}K\x00Ns");
        let empty = pickle("zoe.plone.history", "WorkflowHistory", b"}");
        let two = pickle(
            "zoe.plone.properties",
            "PrincipalProperties",
            b"}(K\x00NK\x01Nu",
        );
        let other = pickle("persistent.mapping", "PersistentMapping", b"}");
        commit(
            &store,
            &[
                (1, Some(live.clone())),
                (2, Some(retired)),
                (3, Some(empty)),
                (4, Some(two)),
                (5, Some(other)),
                (6, Some(live)),
            ],
        );
        // A deleted record has no current record.
        commit(&store, &[(6, None)]);
        let classes = [LIVE, "zoe.plone.history.WorkflowHistory"];
        let found = records_of_classes(&store, &classes).unwrap();
        let expect = |oid, class: &str| ClassRecord {
            oid: Oid(oid),
            class: class.to_owned(),
        };
        assert_eq!(
            found,
            vec![
                expect(1, LIVE),
                expect(3, "zoe.plone.history.WorkflowHistory"),
                expect(4, LIVE),
            ]
        );
    }
}
