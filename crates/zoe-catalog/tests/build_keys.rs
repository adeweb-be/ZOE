//! The structures of a FieldIndex and of a KeywordIndex that a build makes
//! from its pairs (ADR 0059, section 2): the distinct keys in ascending
//! order, the rows of each key, the key of each row of a FieldIndex and
//! the keys of each row of a KeywordIndex. Each test compares a structure
//! with a model that a `BTreeMap` gives, on pairs in a random order, with
//! repeated pairs and with rows without a key.

use std::collections::{BTreeMap, BTreeSet};

use zoe_catalog::base::{FieldIndex, Keys, KeywordIndex, Pairs, Posting};
use zoe_catalog::value::Val;

/// A small generator of numbers (xorshift), so that each run sees the same
/// pairs.
struct Numbers(u64);

impl Numbers {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// Texts with long shared prefixes, as the dotted names of interfaces are,
/// and texts outside ASCII, whose order is the order of their code points.
fn texts() -> Vec<String> {
    let mut texts = Vec::new();
    for i in 0..40 {
        texts.push(format!("package.module.interfaces.IName{i:03}"));
        texts.push(format!("package.module.interfaces.IName{i:03}.sub"));
    }
    texts.extend(
        [
            "", "a", "Z", "é", "e\u{301}", "ÿ", "\u{fffd}", "😀", "z😀", "zé",
        ]
        .iter()
        .map(|t| (*t).to_owned()),
    );
    texts
}

/// The rows of each key and the keys of each row, as the model gives them.
struct Model<K> {
    rows_of: BTreeMap<K, BTreeSet<u32>>,
    keys_of: BTreeMap<u32, BTreeSet<K>>,
}

impl<K: Ord + Clone> Model<K> {
    fn of(pairs: &[(K, u32)]) -> Model<K> {
        let mut rows_of: BTreeMap<K, BTreeSet<u32>> = BTreeMap::new();
        let mut keys_of: BTreeMap<u32, BTreeSet<K>> = BTreeMap::new();
        for (key, row) in pairs {
            rows_of.entry(key.clone()).or_default().insert(*row);
            keys_of.entry(*row).or_default().insert(key.clone());
        }
        Model { rows_of, keys_of }
    }
}

fn rows_in(posting: Posting<'_>) -> BTreeSet<u32> {
    posting.to_bitmap().iter().collect()
}

fn str_keys(keys: &Keys) -> Vec<String> {
    match keys {
        Keys::Str(arena) => (0..arena.len() as u32)
            .map(|slot| arena.get(slot).to_owned())
            .collect(),
        other => panic!("str keys expected, not {other:?}"),
    }
}

fn int_keys(keys: &Keys) -> Vec<i64> {
    match keys {
        Keys::Int(v) => v.clone(),
        other => panic!("int keys expected, not {other:?}"),
    }
}

/// Checks the keys, the postings and the keys of each row of a
/// KeywordIndex against the model.
fn check_keyword<K: Ord + Clone + std::fmt::Debug>(
    index: &KeywordIndex,
    keys: &[K],
    model: &Model<K>,
    rows: usize,
) {
    let expected: Vec<K> = model.rows_of.keys().cloned().collect();
    assert_eq!(keys, expected.as_slice(), "the distinct keys in order");
    for (slot, key) in keys.iter().enumerate() {
        assert_eq!(
            rows_in(index.postings.get(slot as u32)),
            model.rows_of[key].clone(),
            "the rows of {key:?}"
        );
    }
    for row in 0..rows as u32 {
        let got: Vec<K> = index
            .row_slots(row)
            .iter()
            .map(|slot| keys[*slot as usize].clone())
            .collect();
        let want: Vec<K> = model
            .keys_of
            .get(&row)
            .map(|set| set.iter().cloned().collect())
            .unwrap_or_default();
        assert_eq!(got, want, "the keys of the row {row}");
        assert_eq!(index.indexed.contains(row), !want.is_empty());
    }
}

#[test]
fn a_keyword_index_of_str_keys_equals_the_model() {
    let texts = texts();
    let mut numbers = Numbers(0x9e37_79b9_7f4a_7c15);
    let rows = 3_000;
    let mut pairs: Vec<(String, u32)> = Vec::new();
    for row in 0..rows as u32 {
        // One row in seven has no key.
        if row % 7 == 3 {
            continue;
        }
        for _ in 0..numbers.below(12) + 1 {
            pairs.push((texts[numbers.below(texts.len())].clone(), row));
        }
    }
    // A repeated pair, as a list with a key twice gives it.
    pairs.push(pairs[0].clone());
    for i in (1..pairs.len()).rev() {
        pairs.swap(i, numbers.below(i + 1));
    }
    let model = Model::of(&pairs);
    let mut built = Pairs::Empty;
    for (key, row) in &pairs {
        built.push(Val::Str(key.as_str().into()), *row);
    }
    let index = KeywordIndex::build(built, rows).expect("the build");
    let keys = str_keys(&index.keys);
    check_keyword(&index, &keys, &model, rows);
}

#[test]
fn a_keyword_index_of_str_keys_built_from_parts_equals_the_model() {
    // The walk gives one part for each thread; the build appends them.
    let texts = texts();
    let mut numbers = Numbers(0x2545_f491_4f6c_dd1d);
    let rows = 1_000;
    let mut all: Vec<(String, u32)> = Vec::new();
    let mut built = Pairs::Empty;
    for _ in 0..4 {
        let mut part = Pairs::Empty;
        for _ in 0..2_000 {
            let key = texts[numbers.below(texts.len())].clone();
            let row = numbers.below(rows) as u32;
            part.push_str(&key, row);
            all.push((key, row));
        }
        built.append(part);
    }
    let model = Model::of(&all);
    let index = KeywordIndex::build(built, rows).expect("the build");
    let keys = str_keys(&index.keys);
    check_keyword(&index, &keys, &model, rows);
}

#[test]
fn a_keyword_index_of_int_keys_equals_the_model() {
    let mut numbers = Numbers(0x1234_5678_9abc_def1);
    let rows = 2_000;
    let mut pairs: Vec<(i64, u32)> = Vec::new();
    for _ in 0..10_000 {
        let key = numbers.below(300) as i64 - 150;
        pairs.push((key, numbers.below(rows - 10) as u32));
    }
    pairs.push(pairs[5]);
    let model = Model::of(&pairs);
    let mut built = Pairs::Empty;
    for (key, row) in &pairs {
        built.push(Val::Int(*key), *row);
    }
    let index = KeywordIndex::build(built, rows).expect("the build");
    let keys = int_keys(&index.keys);
    check_keyword(&index, &keys, &model, rows);
}

#[test]
fn a_keyword_index_without_pairs_has_no_key() {
    let index = KeywordIndex::build(Pairs::Empty, 5).expect("the build");
    assert!(index.keys.is_empty());
    for row in 0..5 {
        assert!(index.row_slots(row).is_empty());
        assert!(!index.indexed.contains(row));
    }
}

#[test]
fn a_field_index_of_str_keys_equals_the_model() {
    let texts = texts();
    let mut numbers = Numbers(0x0bad_cafe_dead_beef);
    let rows = 5_000;
    // Many distinct keys, as UID and sortable_title have, and a few that
    // many rows share.
    let mut pairs: Vec<(String, u32)> = Vec::new();
    for row in 0..rows as u32 {
        if row % 11 == 0 {
            continue;
        }
        let key = if row % 3 == 0 {
            texts[numbers.below(texts.len())].clone()
        } else {
            format!("{:016x}", numbers.next())
        };
        pairs.push((key, row));
    }
    for i in (1..pairs.len()).rev() {
        pairs.swap(i, numbers.below(i + 1));
    }
    let model = Model::of(&pairs);
    let mut built = Pairs::Empty;
    for (key, row) in &pairs {
        built.push(Val::Str(key.as_str().into()), *row);
    }
    let index = FieldIndex::build(built, rows).expect("the build");
    let keys = str_keys(&index.keys);
    let expected: Vec<String> = model.rows_of.keys().cloned().collect();
    assert_eq!(keys, expected, "the distinct keys in order");
    for (slot, key) in keys.iter().enumerate() {
        assert_eq!(
            rows_of(&index, slot),
            model.rows_of[key],
            "the rows of {key}"
        );
    }
    for row in 0..rows as u32 {
        let want = model.keys_of.get(&row).map(|set| {
            assert_eq!(set.len(), 1);
            set.first().cloned().unwrap_or_default()
        });
        let got = index.slot(row).map(|slot| keys[slot as usize].clone());
        assert_eq!(got, want, "the key of the row {row}");
        assert_eq!(index.indexed.contains(row), want.is_some());
    }
}

fn rows_of(index: &FieldIndex, slot: usize) -> BTreeSet<u32> {
    rows_in(index.postings.get(slot as u32))
}
