//! Checks of the tree that the decoder builds from the synthetic corpus.

use std::collections::BTreeMap;
use std::path::PathBuf;

use zoe_pickle::{Global, Node, Tree, decode_record};

/// Returns every record of the corpus as (file stem, tree), sorted by stem.
fn corpus() -> Vec<(String, Tree)> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus");
    let mut records: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "record"))
        .map(|path| {
            let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
            let tree = decode_record(&std::fs::read(&path).unwrap()).unwrap();
            (stem, tree)
        })
        .collect();
    records.sort_by(|a, b| a.0.cmp(&b.0));
    records
}

fn record(stem: &str) -> Tree {
    corpus()
        .into_iter()
        .find(|(name, _)| name == stem)
        .unwrap_or_else(|| panic!("no record {stem}"))
        .1
}

fn nodes(tree: &Tree) -> impl Iterator<Item = &Node> {
    tree.class().walk().chain(tree.state().walk())
}

#[test]
fn only_the_unmodeled_constructs_are_opaque() {
    let mut opaque = BTreeMap::new();
    for (stem, tree) in corpus() {
        let opcodes: Vec<u8> = nodes(&tree)
            .filter_map(|node| match node {
                Node::Opaque(opaque) => Some(opaque.opcode),
                _ => None,
            })
            .collect();
        if !opcodes.is_empty() {
            opaque.insert(stem, opcodes);
        }
    }
    let expected = BTreeMap::from([
        ("custom-reduce-1".to_owned(), vec![b'R']),
        ("custom-reduce-2".to_owned(), vec![b'R']),
        ("custom-reduce-3".to_owned(), vec![b'R'; 4]),
        ("datetime-1".to_owned(), vec![b'R']),
        ("inline-object-1".to_owned(), vec![0x81]),
        ("unicode-4".to_owned(), vec![b'X']),
    ]);
    assert_eq!(opaque, expected);
}

#[test]
fn class_names_come_from_the_class_pickle() {
    for (stem, module, name) in [
        (
            "persistent-mapping-1",
            "persistent.mapping",
            "PersistentMapping",
        ),
        ("persistent-list-1", "persistent.list", "PersistentList"),
        ("oobucket-1", "BTrees.OOBTree", "OOBucket"),
        ("iitreeset-1", "BTrees.IIBTree", "IITreeSet"),
        ("length-1", "BTrees.Length", "Length"),
        ("tuple-1", "zoe_corpus", "Holder"),
        // The class and the arguments of __getnewargs__, as a tuple.
        ("class-with-newargs-1", "zoe_corpus", "WithNewArgs"),
    ] {
        assert_eq!(record(stem).class_name(), Some((module, name)), "{stem}");
    }
}

#[test]
fn every_record_names_its_class() {
    for (stem, tree) in corpus() {
        assert!(tree.class_name().is_some(), "{stem}");
    }
}

#[test]
fn a_persistent_reference_names_its_class_through_the_shared_memo() {
    // Holder(ref=Holder(...)): the class in the persistent id is a get of
    // the GLOBAL in the class pickle.
    let tree = record("persistent-reference-1");
    let pid = nodes(&tree)
        .find_map(|node| match node {
            Node::Persistent(pid) => Some(pid),
            _ => None,
        })
        .expect("a persistent reference");
    let Node::Tuple(pid) = pid.value() else {
        panic!("{pid:?}");
    };
    let [oid, class] = &pid.items[..] else {
        panic!("{pid:?}");
    };
    let Node::Bytes(oid) = oid.value() else {
        panic!("{oid:?}");
    };
    assert_eq!(oid.data.len(), 8);
    assert!(matches!(class, Node::Get(_)));
    let holder = Global {
        module: "zoe_corpus".to_owned(),
        name: "Holder".to_owned(),
    };
    assert_eq!(tree.resolve(class), Some(&Node::Global(holder)));
}

#[test]
fn every_get_outside_opaque_bytes_resolves() {
    for (stem, tree) in corpus() {
        for node in nodes(&tree) {
            if matches!(node, Node::Get(_)) {
                assert!(tree.resolve(node).is_some(), "{stem}: {node:?}");
            }
        }
    }
}

#[test]
fn the_corpus_holds_every_kind_of_persistent_id() {
    let mut forms = Vec::new();
    for (_, tree) in corpus() {
        for node in nodes(&tree) {
            if let Node::Persistent(pid) = node {
                let form = match pid.value() {
                    Node::Tuple(_) => "(oid, class)",
                    Node::Bytes(_) => "oid",
                    Node::List(_) => "weak",
                    other => panic!("{other:?}"),
                };
                if !forms.contains(&form) {
                    forms.push(form);
                }
            }
        }
    }
    forms.sort();
    assert_eq!(forms, ["(oid, class)", "oid", "weak"]);
}

#[test]
fn a_set_is_a_modeled_reduce() {
    let tree = record("set-1");
    let reduce = nodes(&tree)
        .find_map(|node| match node {
            Node::Reduce(reduce) => Some(reduce),
            _ => None,
        })
        .expect("a reduce");
    let set = Global {
        module: "builtins".to_owned(),
        name: "set".to_owned(),
    };
    assert_eq!(tree.resolve(&reduce.callable), Some(&Node::Global(set)));
}

#[test]
fn every_tree_clones_into_an_equal_tree() {
    for (stem, tree) in corpus() {
        let copy = tree.clone();
        assert_eq!(copy, tree, "{stem}");
        assert_eq!(format!("{copy:?}"), format!("{tree:?}"), "{stem}");
    }
}
