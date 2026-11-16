//! The walk of an index tree in parts: a thread of the walk hands its
//! values over each time it holds 65,536 or more. A build with one thread
//! over a KeywordIndex of more pairs gives the keys of each row and the
//! rows of each key that the model gives.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use zoe_catalog::base::{Index, Posting};
use zoe_catalog::build::build;
use zoe_catalog::testing::{self, CatalogModel, IndexKind};
use zoe_catalog::value::Val;
use zoe_core::Store;
use zoe_core::vfs::MemVfs;

const ROWS: i32 = 20_000;

fn s(text: &str) -> Val {
    Val::Str(text.into())
}

/// The keys of the row `i`: four shared keys, one of them twice, and a
/// key of its own. So the KeywordIndex holds 100,000 pairs.
fn keys(i: i32) -> Vec<String> {
    let shared = |k: i32| format!("key-{:02}", (i * 7 + k * 13) % 50);
    vec![
        shared(0),
        shared(1),
        shared(2),
        shared(0),
        format!("own-{i:05}"),
    ]
}

#[test]
fn a_walk_thread_that_hands_its_values_over_gives_the_keys_of_the_model() {
    let store = Arc::new(Store::open_vfs(MemVfs::new()).expect("a store"));
    let mut m = CatalogModel::new(
        10,
        &[
            ("Subject", IndexKind::Keyword),
            ("portal_type", IndexKind::Field),
        ],
        &["getId"],
    );
    for i in 0..ROWS {
        let words: Vec<Val> = keys(i).iter().map(|k| s(k)).collect();
        m.catalog(
            i * 3 + 1,
            &format!("/site/item-{i}"),
            &[
                ("Subject", Val::List(words.into())),
                (
                    "portal_type",
                    s(if i % 2 == 0 { "Document" } else { "Folder" }),
                ),
            ],
        );
    }
    let tid = testing::commit(&store, &m.records(), &[m.key()], &[]).expect("a commit");
    let built = build(&*store, m.oid(), tid, 1, &|_| true).expect("a build");
    let base = &built.base;
    let entry = &base.indexes[base.by_name["Subject"]];
    let Index::Keyword(index) = &entry.index else {
        panic!("the KeywordIndex is not built: {:?}", entry.index);
    };
    // The model: the keys of each row, and the rows of each key.
    let mut rows_of: BTreeMap<String, BTreeSet<u32>> = BTreeMap::new();
    for i in 0..ROWS {
        let row = base.rows.row(i * 3 + 1).expect("a row");
        let distinct: BTreeSet<String> = keys(i).into_iter().collect();
        let expected: Vec<Val> = distinct.iter().map(|k| s(k)).collect();
        assert_eq!(
            index.value(row),
            Some(Val::List(expected.into())),
            "row {row}"
        );
        for key in distinct {
            rows_of.entry(key).or_default().insert(row);
        }
    }
    assert_eq!(index.keys.len(), rows_of.len());
    for (slot, (key, rows)) in rows_of.iter().enumerate() {
        let slot = slot as u32;
        assert_eq!(index.keys.get(slot), s(key));
        let got: BTreeSet<u32> = match index.postings.get(slot) {
            Posting::One(row) => BTreeSet::from([row]),
            Posting::Many(bitmap) => bitmap.iter().collect(),
        };
        assert_eq!(&got, rows, "the rows of {key}");
    }
    let entry = &base.indexes[base.by_name["portal_type"]];
    assert!(matches!(entry.index, Index::Field(_)), "{:?}", entry.index);
}
