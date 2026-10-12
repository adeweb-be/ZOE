//! Tests of the references of an object, on the fixture of `browse.rs`.
//!
//! Each reference has a JSON pointer into the state of the object, and the
//! value at the pointer holds the reference. The test checks every
//! revision of every object of the fixture.

mod common;

use serde_json::{Value, json};
use zoe_core::Oid;
use zoe_loupe::{Folder, oid_repr};

use common::{fresh_dir, write_store};

/// Tell if the value at `at` in `state` holds a reference to `oid`: a
/// strong `$ref` of the oid, or a value that can hide a reference, as an
/// opaque node, a persistent id that Loupe does not read, or a value that
/// the state leaves out.
fn holds(state: &Value, at: &str, oid: Oid) -> bool {
    let Some(value) = state.pointer(at) else {
        return false;
    };
    let mut stack = vec![value];
    while let Some(value) = stack.pop() {
        match value {
            Value::Object(map) => {
                let strong =
                    map.get("$ref") == Some(&json!(oid_repr(oid))) && map.get("weak").is_none();
                let hides = ["$opaque", "$persistent", "$elided"]
                    .iter()
                    .any(|tag| map.contains_key(*tag));
                if strong || hides {
                    return true;
                }
                stack.extend(map.values());
            }
            Value::Array(items) => stack.extend(items),
            _ => {}
        }
    }
    false
}

#[test]
fn the_value_at_each_pointer_holds_its_reference() {
    let dir = fresh_dir("references");
    let fixture = write_store(&dir);
    let folder = Folder::open(&dir).unwrap();
    let mut checked = 0;
    for oid in fixture.created().into_keys() {
        for (tid, _) in fixture.revisions(oid) {
            let object = folder.as_of(tid).object(oid).unwrap();
            assert_eq!(object.error, None, "{oid:?} as of {tid:?}");
            for reference in &object.references {
                assert!(
                    holds(&object.state, &reference.at, reference.oid),
                    "{oid:?} as of {tid:?}: the value at {} does not hold a reference to {:?}",
                    reference.at,
                    reference.oid
                );
                checked += 1;
            }
        }
    }
    assert!(checked > 0, "the fixture has no reference");
}
