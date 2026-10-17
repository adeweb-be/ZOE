//! The objects of `zoe_pickle::python` on the synthetic corpus: each record
//! of a BTrees class, of `Length` and of posting dicts goes through the
//! model and back to its bytes, so the pickler writes what ZODB wrote.
//!
//! ZODB's `ObjectWriter` writes a new persistent id for each reference, and
//! the resolver of ZODB writes one for each referenced object, as the model
//! does: the second reference to an object is then a memo get. So a record
//! that refers to one object twice, such as the first bucket of a tree of
//! many buckets, is left out here. The Python tests compare those with the
//! resolver.

use std::path::PathBuf;

use zoe_pickle::python::{Heap, Pickler};
use zoe_pickle::{PersistentId, decode_record, persistent_ids};

/// The prefixes of the names of the records that the model must hold.
const MODELED: [&str; 9] = [
    "bucket-family",
    "set-family",
    "btree-inline",
    "postings",
    "length",
    "oobucket",
    "oobtree",
    "iitreeset",
    "iibtree",
];

#[test]
fn the_states_of_btrees_pickle_again_as_zodb_wrote_them() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus");
    let mut checked = Vec::new();
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
        let record = path.extension().is_some_and(|ext| ext == "record");
        if !record || !MODELED.iter().any(|prefix| stem.starts_with(prefix)) {
            continue;
        }
        let bytes = std::fs::read(&path).unwrap();
        if refers_twice(&bytes) {
            continue;
        }
        let tree = decode_record(&bytes).unwrap();
        let mut heap = Heap::new();
        let (class, state) = heap
            .load(&tree)
            .unwrap_or_else(|error| panic!("{stem}: {error}"));
        let mut pickler = Pickler::new(&heap);
        pickler.dump(class).unwrap();
        pickler.dump(state).unwrap();
        assert_eq!(pickler.into_bytes(), bytes, "{stem}");
        checked.push(stem);
    }
    checked.sort();
    assert_eq!(checked.len(), 29, "{checked:?}");
}

/// Tells if the record refers to one oid twice.
fn refers_twice(record: &[u8]) -> bool {
    let mut oids: Vec<Vec<u8>> = persistent_ids(record)
        .unwrap()
        .into_iter()
        .filter_map(|id| match id {
            PersistentId::Oid(oid) | PersistentId::Weak(oid) => Some(oid),
            _ => None,
        })
        .collect();
    let all = oids.len();
    oids.sort();
    oids.dedup();
    oids.len() != all
}
