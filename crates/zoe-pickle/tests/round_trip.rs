//! Acceptance test of the pickle round trip on the synthetic corpus.

use std::path::PathBuf;

use zoe_pickle::{Node, decode_record, encode_record};

fn corpus() -> Vec<(PathBuf, Vec<u8>)> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus");
    let mut records: Vec<_> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "record"))
        .map(|path| {
            let bytes = std::fs::read(&path).unwrap();
            (path, bytes)
        })
        .collect();
    records.sort();
    records
}

#[test]
fn every_record_round_trips_byte_for_byte() {
    let records = corpus();
    assert!(!records.is_empty(), "the corpus is not empty");
    for (path, bytes) in &records {
        let tree = decode_record(bytes).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let encoded = encode_record(&tree).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        assert_eq!(&encoded, bytes, "{}", path.display());
    }
}

#[test]
fn the_corpus_covers_every_listed_construct() {
    let names: Vec<String> = corpus()
        .iter()
        .map(|(path, _)| path.file_stem().unwrap().to_string_lossy().into_owned())
        .collect();
    for construct in [
        "persistent-reference",
        "oobucket",
        "iitreeset",
        "length",
        "persistent-mapping",
        "persistent-list",
        "zodbpickle-binary",
        "tuple",
        "set",
        "unicode",
        "large-int",
        "float",
        "none-and-bool",
        "custom-reduce",
    ] {
        assert!(
            names.iter().any(|n| n.starts_with(construct)),
            "no record for {construct}"
        );
    }
}

#[test]
fn a_custom_reduce_is_an_opaque_node_that_keeps_its_bytes() {
    let (path, bytes) = corpus()
        .into_iter()
        .find(|(path, _)| {
            path.file_stem()
                .unwrap()
                .to_string_lossy()
                .starts_with("custom-reduce")
        })
        .expect("a custom-reduce record");
    let tree = decode_record(&bytes).unwrap();
    assert!(
        tree.state()
            .walk()
            .any(|node| matches!(node, Node::Opaque(_))),
        "{}",
        path.display()
    );
    assert_eq!(encode_record(&tree).unwrap(), bytes);
}
