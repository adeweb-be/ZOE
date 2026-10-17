//! Tier 2 of the conflict resolution: the merges of the fields of a
//! persistent object whose class the merge policy marks mergeable (ADR
//! 0046).
//!
//! The state of such an object is the dict of its attributes, its fields.
//! A field changed on one side when its value there does not have the
//! structure of its value in the old state (`Heap::same`). A field that
//! one side changed takes that change; a field that both sides changed to
//! the same value takes it. A field that both sides changed to different
//! values takes the strategy of the policy. Coupled fields stay strict.
//!
//! The merged state is the committed dict with the merged values in place,
//! then the fields that only the new side added. The record is the class
//! pickle of the new record and that state, pickled as zodbpickle does.

use std::collections::{BTreeSet, HashMap, HashSet};

use zoe_pickle::python::{Heap, Object, Pickler, Undecided, Value};

use super::policy::{ClassPolicy, Strategy};
use super::{MAX_KEYS, Merge, Outcome, Reason, Trees};

/// Merges the three records of a conflict with the entry of their class,
/// and notes in `merge` the fields that the new record changed.
pub(super) fn merge(trees: &Trees, entry: &ClassPolicy, merge: &mut Merge) -> Outcome {
    let mut heap = Heap::with_instances();
    // As ZODB's resolver: the new record, then the old one, then the
    // committed one. A persistent reference takes its persistent id from
    // the first record that holds it.
    let loaded = heap.load(&trees.new).and_then(|(class, new)| {
        let (_, old) = heap.load(&trees.old)?;
        let (_, committed) = heap.load(&trees.committed)?;
        Ok((class, [old, committed, new]))
    });
    let (class, states) = match loaded {
        Ok(loaded) => loaded,
        Err(unsupported) => return Outcome::Undecided(unsupported.0),
    };
    // ZODB writes the class of an ordinary persistent object as a GLOBAL
    // alone.
    if heap.class(class).is_none() {
        return Outcome::Undecided("a class pickle of another format");
    }
    match merge_fields(&mut heap, entry, &states, merge) {
        Ok(state) => {
            let mut pickler = Pickler::new(&heap);
            match pickler.dump(class).and_then(|()| pickler.dump(state)) {
                Ok(()) => Outcome::Merged(pickler.into_bytes()),
                Err(unsupported) => Outcome::Undecided(unsupported.0),
            }
        }
        Err(outcome) => outcome,
    }
}

/// The fields of one state, in order.
type Fields = Vec<(Value, Value)>;

/// Merges the fields of the three states. Returns the merged state.
fn merge_fields(
    heap: &mut Heap,
    entry: &ClassPolicy,
    states: &[Value; 3],
    merge: &mut Merge,
) -> Result<Value, Outcome> {
    let mut all: Vec<Fields> = Vec::with_capacity(3);
    for &state in states {
        let fields = fields_of(heap, state)?;
        check_shared(heap, &fields)?;
        all.push(fields);
    }
    let [old, committed, new] = <[Fields; 3]>::try_from(all).expect("three states");
    let by_name = |fields: &Fields, heap: &Heap| -> HashMap<String, Value> {
        fields
            .iter()
            .map(|&(key, value)| (heap.str(key).expect("a str key").to_owned(), value))
            .collect()
    };
    let (old_map, committed_map, new_map) = (
        by_name(&old, heap),
        by_name(&committed, heap),
        by_name(&new, heap),
    );
    let names: BTreeSet<&String> = old_map
        .keys()
        .chain(committed_map.keys())
        .chain(new_map.keys())
        .collect();
    let changed = |side: &HashMap<String, Value>, name: &str| -> bool {
        match (old_map.get(name), side.get(name)) {
            (None, None) => false,
            (Some(&a), Some(&b)) => !heap.same(a, b),
            _ => true,
        }
    };
    let by_committed: BTreeSet<&String> = names
        .iter()
        .copied()
        .filter(|name| changed(&committed_map, name))
        .collect();
    let by_new: BTreeSet<&String> = names
        .iter()
        .copied()
        .filter(|name| changed(&new_map, name))
        .collect();
    merge.changed = by_new.len();
    merge.keys = by_new
        .iter()
        .take(MAX_KEYS)
        .map(|name| (*name).clone())
        .collect();
    // Coupled fields stay strict: both sides changed the group, and they
    // do not agree on each of its fields.
    for group in &entry.coupled {
        let touched = |set: &BTreeSet<&String>| group.iter().any(|field| set.contains(field));
        if touched(&by_committed) && touched(&by_new) {
            let agree =
                group.iter().all(
                    |field| match (committed_map.get(field), new_map.get(field)) {
                        (None, None) => true,
                        (Some(&a), Some(&b)) => heap.same(a, b),
                        _ => false,
                    },
                );
            if !agree {
                return Err(Outcome::Refused(Reason::Coupled));
            }
        }
    }
    // The value of each field after the merge. `None` deletes it.
    let mut merged: HashMap<&str, Option<Value>> = HashMap::new();
    for &name in &names {
        let (c, n) = (committed_map.get(name).copied(), new_map.get(name).copied());
        let value = match (by_committed.contains(name), by_new.contains(name)) {
            (_, false) => c,
            (false, true) => n,
            (true, true) => match (c, n) {
                (None, None) => None,
                (Some(c), Some(n)) if heap.same(c, n) => Some(c),
                (Some(c), Some(n)) => {
                    let old = old_map.get(name).copied();
                    Some(both_changed(heap, entry.strategy(name), old, c, n)?)
                }
                _ => return Err(Outcome::Refused(Reason::FieldDeleted)),
            },
        };
        merged.insert(name, value);
    }
    // The committed fields in their order, then the fields that only the
    // new side added, in its order.
    let mut items = Vec::with_capacity(names.len());
    for &(key, _) in &committed {
        if let Some(&Some(value)) = merged.get(heap.str(key).expect("a str key")) {
            items.push((key, value));
        }
    }
    for &(key, _) in &new {
        let name = heap.str(key).expect("a str key");
        if !committed_map.contains_key(name)
            && let Some(&Some(value)) = merged.get(name)
        {
            items.push((key, value));
        }
    }
    Ok(heap.new_dict(items))
}

/// Returns the fields of a state: a dict whose keys are all str.
fn fields_of(heap: &Heap, state: Value) -> Result<Fields, Outcome> {
    let not_fields = Outcome::Undecided("a state that is not a dict of fields");
    let items = heap.dict(state).ok_or(not_fields.clone())?;
    if items.iter().any(|&(key, _)| heap.str(key).is_none()) {
        return Err(not_fields);
    }
    Ok(items.to_vec())
}

/// Refuses a state where two fields hold one list, dict, set or object of
/// another class: a merge that takes the fields from two records would
/// split it in two.
fn check_shared(heap: &Heap, fields: &Fields) -> Result<(), Outcome> {
    let mut seen = HashSet::new();
    for &(_, value) in fields {
        for id in heap.mutable_objects(value) {
            if !seen.insert(id) {
                return Err(Outcome::Undecided("an object that two fields share"));
            }
        }
    }
    Ok(())
}

/// Merges a field that both sides changed to different values, with its
/// strategy. `old` is its value in the old state, if it had one.
fn both_changed(
    heap: &mut Heap,
    strategy: Strategy,
    old: Option<Value>,
    committed: Value,
    new: Value,
) -> Result<Value, Outcome> {
    match strategy {
        Strategy::Strict => Err(Outcome::Refused(Reason::FieldChanged)),
        Strategy::Lww => Ok(new),
        // `max(committed, new)` of Python: the new value when it is
        // greater, else the committed one.
        Strategy::Max => match less(heap, committed, new) {
            Ok(true) => Ok(new),
            Ok(false) => Ok(committed),
            Err(_) => Err(Outcome::Undecided(
                "values that the strategy max cannot order",
            )),
        },
        Strategy::Sum => match (old, committed, new) {
            (Some(Value::Int(o)), Value::Int(c), Value::Int(n)) => c
                .checked_add(n)
                .and_then(|sum| sum.checked_sub(o))
                .map(Value::Int)
                .ok_or(Outcome::Undecided("a sum that does not fit 128 bits")),
            _ => Err(Outcome::Undecided(
                "values that the strategy sum does not add",
            )),
        },
        Strategy::Union => union(heap, old, committed, new),
        Strategy::Append => append(heap, old, committed, new),
    }
}

/// Tells if `a < b` as Python does. Two `DateTime`s compare by their time,
/// the first item of their state, as their `__lt__` does.
fn less(heap: &Heap, a: Value, b: Value) -> Result<bool, Undecided> {
    match (date_time(heap, a), date_time(heap, b)) {
        (Some(a), Some(b)) => heap.lt(a, b),
        _ => heap.lt(a, b),
    }
}

/// Returns the time of a `DateTime.DateTime`: the first item of its state.
fn date_time(heap: &Heap, value: Value) -> Option<Value> {
    let instance = heap.instance(value)?;
    if heap.class(instance.class)? != ("DateTime.DateTime", "DateTime") {
        return None;
    }
    let time = *heap.tuple(instance.state?)?.first()?;
    matches!(time, Value::Int(_) | Value::Float(_)).then_some(time)
}

/// The kinds of values that `union` and `append` take.
#[derive(Clone, Copy)]
enum Kind {
    List,
    Tuple,
    /// A set or a frozenset, with its class.
    Set(Value),
}

/// Returns the kind and the items of a list, a tuple or a set.
fn items(heap: &Heap, value: Value) -> Option<(Kind, Vec<Value>)> {
    match heap.object(value)? {
        Object::List(items) => Some((Kind::List, items.clone())),
        Object::Tuple(items) => Some((Kind::Tuple, items.clone())),
        Object::Set { class, items } => Some((Kind::Set(*class), items.clone())),
        _ => None,
    }
}

/// Returns a new value of `kind` with `items`.
fn make(heap: &mut Heap, kind: Kind, items: Vec<Value>) -> Value {
    match kind {
        Kind::List => heap.new_list(items),
        Kind::Tuple => heap.new_tuple(items),
        Kind::Set(class) => heap.new_set(class, items),
    }
}

/// A key of an item for `union`: two items are one when Python finds them
/// equal. An int, a bool and a float of the same value are one.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum ItemKey {
    None,
    Int(i128),
    Float(u64),
    Str(String),
    Bytes(Vec<u8>),
    Tuple(Vec<ItemKey>),
}

/// Returns the key of an item, or `None` for an item that Python cannot
/// hash or that the model cannot compare: a list, a dict, a set, an object
/// of another class, a persistent reference, a NaN.
fn item_key(heap: &Heap, value: Value) -> Option<ItemKey> {
    Some(match value {
        Value::None => ItemKey::None,
        Value::Bool(b) => ItemKey::Int(i128::from(b)),
        Value::Int(int) => ItemKey::Int(int),
        Value::Float(float) => {
            if float.is_nan() {
                return None;
            }
            if float.fract() == 0.0 && float.abs() < 1e38 {
                // Python: `1.0 == 1` and both hash alike.
                ItemKey::Int(float as i128)
            } else {
                ItemKey::Float(float.to_bits())
            }
        }
        Value::Object(_) => match heap.object(value)? {
            Object::Str(text) => ItemKey::Str(text.clone()),
            Object::Bytes(bytes) => ItemKey::Bytes(bytes.clone()),
            Object::Tuple(items) => ItemKey::Tuple(
                items
                    .iter()
                    .map(|&item| item_key(heap, item))
                    .collect::<Option<_>>()?,
            ),
            _ => return None,
        },
    })
}

/// The strategy `union`: the items of the old value that neither side
/// removed, then the items that the committed side added, then the items
/// that the new side added, each item once, in the kind of the new value.
fn union(
    heap: &mut Heap,
    old: Option<Value>,
    committed: Value,
    new: Value,
) -> Result<Value, Outcome> {
    let not_items = Outcome::Undecided("values that the strategy union does not take");
    let (_, old_items) = match old {
        Some(old) => items(heap, old).ok_or(not_items.clone())?,
        None => (Kind::List, Vec::new()),
    };
    let (_, committed_items) = items(heap, committed).ok_or(not_items.clone())?;
    let (kind, new_items) = items(heap, new).ok_or(not_items.clone())?;
    let keys = |items: &[Value]| -> Option<Vec<ItemKey>> {
        items.iter().map(|&item| item_key(heap, item)).collect()
    };
    let unhashable = Outcome::Undecided("items that the strategy union cannot compare");
    let old_keys = keys(&old_items).ok_or(unhashable.clone())?;
    let committed_keys = keys(&committed_items).ok_or(unhashable.clone())?;
    let new_keys = keys(&new_items).ok_or(unhashable)?;
    let old_set: HashSet<&ItemKey> = old_keys.iter().collect();
    let committed_set: HashSet<&ItemKey> = committed_keys.iter().collect();
    let new_set: HashSet<&ItemKey> = new_keys.iter().collect();
    let mut taken: HashSet<&ItemKey> = HashSet::new();
    let mut result = Vec::new();
    for (item, key) in old_items.iter().zip(&old_keys) {
        if committed_set.contains(key) && new_set.contains(key) && taken.insert(key) {
            result.push(*item);
        }
    }
    for (side, side_keys) in [(&committed_items, &committed_keys), (&new_items, &new_keys)] {
        for (item, key) in side.iter().zip(side_keys) {
            if !old_set.contains(key) && taken.insert(key) {
                result.push(*item);
            }
        }
    }
    Ok(make(heap, kind, result))
}

/// The strategy `append`: the old value is a prefix of both values; the
/// result is the committed value, then the items that the new side added
/// after the old value, in the kind of the new value.
fn append(
    heap: &mut Heap,
    old: Option<Value>,
    committed: Value,
    new: Value,
) -> Result<Value, Outcome> {
    let not_append = Outcome::Refused(Reason::NotAppend);
    let sequence = |heap: &Heap, value: Value| match items(heap, value) {
        Some((kind @ (Kind::List | Kind::Tuple), items)) => Some((kind, items)),
        _ => None,
    };
    let (_, old_items) = old
        .and_then(|old| sequence(heap, old))
        .ok_or(not_append.clone())?;
    let (_, committed_items) = sequence(heap, committed).ok_or(not_append.clone())?;
    let (kind, new_items) = sequence(heap, new).ok_or(not_append.clone())?;
    let prefix = |items: &[Value]| {
        items.len() >= old_items.len()
            && old_items.iter().zip(items).all(|(&a, &b)| heap.same(a, b))
    };
    if !prefix(&committed_items) || !prefix(&new_items) {
        return Err(not_append);
    }
    let mut result = committed_items;
    result.extend_from_slice(&new_items[old_items.len()..]);
    Ok(make(heap, kind, result))
}
