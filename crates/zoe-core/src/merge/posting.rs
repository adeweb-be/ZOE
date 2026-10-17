//! The posting-dict rule (ADR 0042).
//!
//! ZCTextIndex keeps the postings of a word, a dict of document id to
//! score, inside the bucket of `_wordinfo` until the dict holds 10 entries
//! (`DICT_CUTOFF`). Then it puts them in a tree of their own. When two
//! transactions index documents that share a word, both change the dict of
//! that word. BTrees cannot compare two dicts, so ZODB's resolver refuses
//! the bucket.
//!
//! In a bucket with integer keys, the rule compares such dicts entry by
//! entry, and merges two changes of one dict against the old dict. It
//! refuses:
//!
//! - an entry that both sides changed to different values, or that one
//!   side changed and the other side deleted;
//! - a value that is not a dict on one side, such as the reference to the
//!   tree of a posting that ZCTextIndex converted;
//! - a merged dict of more than 10 entries, which ZCTextIndex would never
//!   convert, or of no entry, and a dict of more than 10 entries on one
//!   side, which ZCTextIndex never keeps.
//!
//! A posting dict maps int keys to int values, as the scores of the
//! indexes of ZCTextIndex are ints. Another dict is not a posting dict, and
//! the rule leaves it to BTrees.

use std::collections::HashMap;

use zoe_pickle::btrees::{Family, KeyType, ValueType};
use zoe_pickle::python::{Heap, Value};

use super::{Outcome, Reason};

/// The most entries of a merged posting dict: `DICT_CUTOFF` of
/// ZCTextIndex.
const MAX_ENTRIES: usize = 10;

/// Tells if the rule applies to the buckets of `family`: integer keys and
/// object values.
pub(super) fn applies(family: Family) -> bool {
    family.keys != KeyType::Object && family.values == ValueType::Object
}

/// Returns the entries of `value` if it is a posting dict.
fn entries(heap: &Heap, value: Value) -> Option<&[(Value, Value)]> {
    let items = heap.dict(value)?;
    let posting = items
        .iter()
        .all(|&(key, value)| matches!(key, Value::Int(_)) && matches!(value, Value::Int(_)));
    posting.then_some(items)
}

/// Tells if two values of a bucket are equal when one of them is a posting
/// dict: two posting dicts are equal entry by entry, and a posting dict is
/// never equal to another value. `None` when neither is a posting dict, or
/// when the other is a dict that is not one.
pub(super) fn equal(heap: &Heap, a: Value, b: Value) -> Option<bool> {
    match (entries(heap, a), entries(heap, b)) {
        (Some(a), Some(b)) => Some(same_entries(a, b)),
        (Some(_), None) if heap.dict(b).is_none() => Some(false),
        (None, Some(_)) if heap.dict(a).is_none() => Some(false),
        _ => None,
    }
}

/// Tells if two posting dicts have the same entries, in any order.
fn same_entries(a: &[(Value, Value)], b: &[(Value, Value)]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let b: HashMap<i128, i128> = b
        .iter()
        .map(|&(key, value)| (int(key), int(value)))
        .collect();
    a.iter()
        .all(|&(key, value)| b.get(&int(key)) == Some(&int(value)))
}

/// Merges two changes of the value of one key, against the old value.
/// Called when both sides changed it.
pub(super) fn merge(
    heap: &mut Heap,
    old: Value,
    committed: Value,
    new: Value,
) -> Result<Value, Outcome> {
    let (Some(old), Some(committed), Some(new)) = (
        entries(heap, old),
        entries(heap, committed),
        entries(heap, new),
    ) else {
        let any = [old, committed, new]
            .into_iter()
            .any(|value| entries(heap, value).is_some());
        return Err(if any {
            Outcome::Refused(Reason::PostingNotDict)
        } else {
            Outcome::Refused(Reason::BTrees(1))
        });
    };
    let ints = |entries: &[(Value, Value)]| -> Vec<(i128, i128)> {
        entries
            .iter()
            .map(|&(key, value)| (int(key), int(value)))
            .collect()
    };
    let merged = merge_entries(&ints(old), &ints(committed), &ints(new))?;
    let items = merged
        .into_iter()
        .map(|(key, value)| (Value::Int(key), Value::Int(value)))
        .collect();
    Ok(heap.new_dict(items))
}

/// Merges the entries of two posting dicts against the old one. The merged
/// dict keeps the order of the committed dict, with the new entries of the
/// new dict after them, as the new transaction would have changed the
/// committed dict. The work is linear in the entries.
fn merge_entries(
    old: &[(i128, i128)],
    committed: &[(i128, i128)],
    new: &[(i128, i128)],
) -> Result<Vec<(i128, i128)>, Outcome> {
    // ZCTextIndex never keeps a dict of more than 10 entries, and ZODB's
    // resolver refuses every dict. So a larger dict is a refusal at once.
    if [old, committed, new]
        .iter()
        .any(|dict| dict.len() > MAX_ENTRIES)
    {
        return Err(Outcome::Refused(Reason::PostingTooLarge));
    }
    let map =
        |entries: &[(i128, i128)]| -> HashMap<i128, i128> { entries.iter().copied().collect() };
    let (old_map, committed_map, new_map) = (map(old), map(committed), map(new));
    let mut merged = committed.to_vec();
    let places: HashMap<i128, usize> = merged
        .iter()
        .enumerate()
        .map(|(place, &(key, _))| (key, place))
        .collect();
    let changed = Outcome::Refused(Reason::PostingChanged);
    for &(key, value) in new {
        match (old_map.get(&key), committed_map.get(&key)) {
            (Some(&before), committed) => {
                if before == value {
                    continue;
                }
                match committed {
                    Some(&now) if now == before => merged[places[&key]].1 = value,
                    Some(&now) if now == value => {}
                    _ => return Err(changed),
                }
            }
            (None, None) => merged.push((key, value)),
            (None, Some(&now)) => {
                if now != value {
                    return Err(changed);
                }
            }
        }
    }
    let mut deleted = std::collections::HashSet::new();
    for &(key, before) in old {
        if new_map.contains_key(&key) {
            continue;
        }
        // The new transaction deleted the entry.
        if let Some(&now) = committed_map.get(&key) {
            if now != before {
                return Err(changed);
            }
            deleted.insert(key);
        }
    }
    merged.retain(|(key, _)| !deleted.contains(key));
    if merged.len() > MAX_ENTRIES {
        return Err(Outcome::Refused(Reason::PostingTooLarge));
    }
    if merged.is_empty() {
        return Err(Outcome::Refused(Reason::PostingEmpty));
    }
    Ok(merged)
}

/// Returns the int of a key or a score of a posting dict.
fn int(value: Value) -> i128 {
    match value {
        Value::Int(int) => int,
        _ => unreachable!("a posting dict holds ints"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_new_documents_join_one_word() {
        assert_eq!(
            merge_entries(
                &[(1, 3), (2, 1)],
                &[(1, 3), (2, 1), (3, 4)],
                &[(1, 3), (2, 1), (4, 2)]
            ),
            Ok(vec![(1, 3), (2, 1), (3, 4), (4, 2)])
        );
    }

    #[test]
    fn a_change_a_delete_and_an_insert_merge() {
        assert_eq!(
            merge_entries(
                &[(1, 3), (2, 1), (5, 5)],
                &[(1, 7), (2, 1), (5, 5)],
                &[(1, 3), (5, 5), (4, 2)]
            ),
            Ok(vec![(1, 7), (5, 5), (4, 2)])
        );
        // The same change on both sides, and a delete on both sides.
        assert_eq!(
            merge_entries(&[(1, 3), (2, 1)], &[(1, 4)], &[(1, 4)]),
            Ok(vec![(1, 4)])
        );
    }

    #[test]
    fn the_rule_refuses() {
        let changed = Err(Outcome::Refused(Reason::PostingChanged));
        assert_eq!(merge_entries(&[(1, 3)], &[(1, 5)], &[(1, 6)]), changed);
        assert_eq!(
            merge_entries(&[(1, 3), (2, 1)], &[(2, 1)], &[(1, 6), (2, 1)]),
            changed
        );
        assert_eq!(
            merge_entries(&[(1, 3), (2, 1)], &[(1, 6), (2, 1)], &[(2, 1)]),
            changed
        );
        assert_eq!(
            merge_entries(&[(2, 1)], &[(2, 1), (1, 3)], &[(2, 1), (1, 4)]),
            changed
        );
        let nine: Vec<(i128, i128)> = (1..10).map(|d| (d, 1)).collect();
        let committed = [&nine[..], &[(20, 1)]].concat();
        let new = [&nine[..], &[(21, 1)]].concat();
        assert_eq!(
            merge_entries(&nine, &committed, &new),
            Err(Outcome::Refused(Reason::PostingTooLarge))
        );
        assert_eq!(
            merge_entries(&[(1, 3), (2, 1)], &[(1, 3)], &[(2, 1)]),
            Err(Outcome::Refused(Reason::PostingEmpty))
        );
    }

    #[test]
    fn a_dict_of_float_scores_is_not_a_posting_dict() {
        let mut heap = Heap::new();
        let floats = heap.new_dict(vec![(Value::Int(1), Value::Float(0.5))]);
        let ints = heap.new_dict(vec![(Value::Int(1), Value::Int(3))]);
        assert!(entries(&heap, floats).is_none());
        assert!(entries(&heap, ints).is_some());
        // Neither side is a posting dict, so BTrees compares them.
        assert_eq!(equal(&heap, floats, floats), None);
    }
}
