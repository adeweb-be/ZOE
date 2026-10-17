//! Tier 2 of the conflict resolution: the merges of the fields of a
//! persistent object whose class the merge policy marks mergeable (ADR
//! 0046).
//!
//! The state of such an object is the dict of its attributes, its fields.
//! A field changed on one side when its value there does not have the
//! structure of its value in the old state (`Canon`). A field that one
//! side changed takes that change; a field that both sides changed to the
//! same value takes it. A field that both sides changed to different values
//! takes the strategy of the policy. Coupled fields stay strict. The
//! strategy `keys` merges a dict with the same rules, key by key.
//!
//! The merged state is the committed dict with the merged values in place,
//! then the fields that only the new side added. The record is the class
//! pickle of the new record and that state, pickled as zodbpickle does.
//!
//! The work is linear in the three records, within a step budget in
//! proportion to their lengths: past it, the merge gives no answer.

use std::collections::{BTreeSet, HashMap, HashSet};

use zoe_pickle::python::{Canon, DictKey, Heap, Object, ObjectId, Pickler, Undecided, Value};

use super::btrees::text;
use super::policy::{ClassPolicy, Strategy};
use super::{MAX_KEYS, Merge, Outcome, Reason, Trees, budget};

/// Merges the three records of a conflict with the entry of their class,
/// and notes in `merge` the fields that the new record changed.
pub(super) fn merge(trees: &Trees, entry: &ClassPolicy, merge: &mut Merge) -> Outcome {
    let mut heap = Heap::with_instances();
    heap.set_budget(budget(trees.len));
    // As ZODB's resolver: the new record, then the old one, then the
    // committed one. A persistent reference takes its persistent id from
    // the first record that holds it.
    let loaded = heap.load(&trees.new).and_then(|(class, new)| {
        let (old_class, old) = heap.load(&trees.old)?;
        let (committed_class, committed) = heap.load(&trees.committed)?;
        Ok(([old_class, committed_class, class], [old, committed, new]))
    });
    let (classes, states) = match loaded {
        Ok(loaded) => loaded,
        Err(unsupported) => return Outcome::Undecided(unsupported.0),
    };
    // ZODB writes the class of an ordinary persistent object as a GLOBAL
    // alone, and the heap keeps one class object for each name.
    let class = classes[2];
    if heap.class(class).is_none() {
        return Outcome::Undecided("a class pickle of another format");
    }
    // A merge that took the fields of an old class over the record of a
    // new one would undo a change of class.
    if classes.iter().any(|&other| other != class) {
        return Outcome::Undecided("records of different classes");
    }
    let mut canon = Canon::new(budget(trees.len));
    match merge_fields(&mut heap, &mut canon, entry, &states, merge) {
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

/// Returns the outcome of a comparison without an answer.
fn undecided(undecided: Undecided) -> Outcome {
    match undecided {
        Undecided::Budget => Outcome::Undecided("a merge over its step budget"),
        Undecided::Raises | Undecided::Unknown => {
            Outcome::Undecided("values that the model cannot compare")
        }
    }
}

/// The fields of one state, in order.
type Fields = Vec<(Value, Value)>;

/// Merges the fields of the three states. Returns the merged state.
fn merge_fields(
    heap: &mut Heap,
    canon: &mut Canon,
    entry: &ClassPolicy,
    states: &[Value; 3],
    merge: &mut Merge,
) -> Result<Value, Outcome> {
    // A client loads each state with `Persistent.__setstate__`, which
    // interns its keys.
    for &state in states {
        heap.intern_keys(state);
    }
    let mut all: Vec<Fields> = Vec::with_capacity(3);
    for &state in states {
        let fields = fields_of(heap, state)?;
        check_shared(heap, &fields, "an object that two fields share")?;
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
    let changed = |canon: &mut Canon, side: &HashMap<String, Value>, name: &str| match (
        old_map.get(name),
        side.get(name),
    ) {
        (None, None) => Ok(false),
        (Some(&a), Some(&b)) => Ok(!canon.same(heap, a, b).map_err(undecided)?),
        _ => Ok(true),
    };
    let mut by_committed: BTreeSet<&String> = BTreeSet::new();
    let mut by_new: BTreeSet<&String> = BTreeSet::new();
    for &name in &names {
        if changed(canon, &committed_map, name)? {
            by_committed.insert(name);
        }
        if changed(canon, &new_map, name)? {
            by_new.insert(name);
        }
    }
    // The keys of the dicts that the strategy `keys` merged, by field.
    let mut expanded: HashMap<&str, Noted> = HashMap::new();
    note(merge, &by_new, &expanded);
    // Coupled fields stay strict: both sides changed the group, and they
    // do not agree on each of its fields.
    for group in &entry.coupled {
        let touched = |set: &BTreeSet<&String>| group.iter().any(|field| set.contains(field));
        if touched(&by_committed) && touched(&by_new) {
            let mut agree = true;
            for field in group {
                agree &= match (committed_map.get(field), new_map.get(field)) {
                    (None, None) => true,
                    (Some(&a), Some(&b)) => canon.same(heap, a, b).map_err(undecided)?,
                    _ => false,
                };
            }
            if !agree {
                merge.fields = group.clone();
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
                (Some(c), Some(n)) if canon.same(heap, c, n).map_err(undecided)? => Some(c),
                (Some(c), Some(n)) => {
                    let old = old_map.get(name).copied();
                    let strategy = entry.strategy(name);
                    let mut noted = None;
                    let merged = both_changed(heap, canon, strategy, old, c, n, &mut noted);
                    if let Some(noted) = noted {
                        expanded.insert(name.as_str(), noted);
                        note(merge, &by_new, &expanded);
                    }
                    match merged {
                        Ok(value) => Some(value),
                        Err(outcome) => {
                            merge.fields = vec![name.clone()];
                            return Err(outcome);
                        }
                    }
                }
                _ => {
                    merge.fields = vec![name.clone()];
                    return Err(Outcome::Refused(Reason::FieldDeleted));
                }
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

/// The keys of a dict that the new record changed, added or deleted, as the
/// strategy `keys` found them.
struct Noted {
    /// The number of the keys.
    changed: usize,
    /// The text of the first [`MAX_KEYS`] keys, in the order of the model.
    texts: Vec<String>,
}

/// Notes in `merge` the fields that the new record changed, in their order.
/// A field in `expanded` gives the keys of its dict in place of its name.
fn note(merge: &mut Merge, by_new: &BTreeSet<&String>, expanded: &HashMap<&str, Noted>) {
    merge.changed = 0;
    merge.keys.clear();
    for name in by_new {
        let (changed, texts) = match expanded.get(name.as_str()) {
            Some(noted) => (noted.changed, noted.texts.as_slice()),
            None => (1, std::slice::from_ref(*name)),
        };
        merge.changed += changed;
        let room = MAX_KEYS - merge.keys.len();
        merge.keys.extend(texts.iter().take(room).cloned());
    }
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

/// Tells if an object can change in place: a list, a dict, a set, or an
/// object of another class but `DateTime.DateTime`, a value that no code
/// changes in place. A str, a bytes, a tuple, a frozenset, a class and a
/// persistent reference cannot.
fn is_mutable(heap: &Heap, id: ObjectId) -> bool {
    match heap.get(id) {
        Object::List(_) | Object::Dict(_) => true,
        Object::Instance(instance) => {
            heap.class(instance.class) != Some(("DateTime.DateTime", "DateTime"))
        }
        Object::Set { class, .. } => heap.class(*class) != Some(("builtins", "frozenset")),
        Object::Str(_)
        | Object::Bytes(_)
        | Object::Tuple(_)
        | Object::Class { .. }
        | Object::Reference(_) => false,
    }
}

/// Returns the objects that an object holds directly. A persistent
/// reference and a class hold nothing that a merge can split.
fn parts(heap: &Heap, id: ObjectId) -> Vec<ObjectId> {
    let objects = |values: &mut dyn Iterator<Item = Value>| -> Vec<ObjectId> {
        values
            .filter_map(|value| match value {
                Value::Object(id) => Some(id),
                _ => None,
            })
            .collect()
    };
    match heap.get(id) {
        Object::Tuple(items) | Object::List(items) => objects(&mut items.iter().copied()),
        Object::Dict(items) => objects(&mut items.iter().flat_map(|&(k, v)| [k, v])),
        Object::Instance(instance) => objects(
            &mut [instance.args]
                .into_iter()
                .chain(instance.state)
                .chain([instance.class]),
        ),
        Object::Set { items, .. } => objects(&mut items.iter().copied()),
        Object::Str(_) | Object::Bytes(_) | Object::Class { .. } | Object::Reference(_) => {
            Vec::new()
        }
    }
}

/// Gives no answer, with the reason `shared`, for a state where two fields
/// hold one mutable object: a merge that takes the fields from two records
/// would split it in two, and a change of it through one field would not
/// show through the other. Two fields may share an immutable value that
/// holds no mutable object. The strategy `keys` checks the values of the
/// keys of a dict in the same way.
///
/// One walk of the state: each object is walked once, under the first field
/// that reaches it. A second field that reaches it looks only at whether it
/// holds a mutable object, which `holds` knows once for each object.
fn check_shared(heap: &Heap, fields: &Fields, shared: &'static str) -> Result<(), Outcome> {
    let shared = Outcome::Undecided(shared);
    let mut owner: HashMap<ObjectId, usize> = HashMap::new();
    let mut holds: HashMap<ObjectId, bool> = HashMap::new();
    for (field, &(_, value)) in fields.iter().enumerate() {
        let Value::Object(root) = value else {
            continue;
        };
        let mut todo = vec![root];
        while let Some(id) = todo.pop() {
            match owner.get(&id) {
                Some(&first) if first == field => continue,
                Some(_) => {
                    if holds_mutable(heap, id, &mut holds) {
                        return Err(shared);
                    }
                    continue;
                }
                None => {
                    owner.insert(id, field);
                    todo.extend(parts(heap, id));
                }
            }
        }
    }
    Ok(())
}

/// Tells if the object of `id` is mutable or holds a mutable object. The
/// answer of each object goes into `memo`, so each object is looked at
/// once. The walk uses an explicit stack.
fn holds_mutable(heap: &Heap, id: ObjectId, memo: &mut HashMap<ObjectId, bool>) -> bool {
    let mut stack = vec![(id, false)];
    while let Some(&(id, expanded)) = stack.last() {
        if memo.contains_key(&id) {
            stack.pop();
            continue;
        }
        if !expanded {
            stack.last_mut().expect("an entry").1 = true;
            if is_mutable(heap, id) {
                memo.insert(id, true);
                stack.pop();
                continue;
            }
            for part in parts(heap, id) {
                if !memo.contains_key(&part) {
                    stack.push((part, false));
                }
            }
            continue;
        }
        stack.pop();
        let held = parts(heap, id)
            .iter()
            .any(|part| memo.get(part).copied().unwrap_or(true));
        memo.insert(id, held);
    }
    memo[&id]
}

/// Merges a field that both sides changed to different values, with its
/// strategy. `old` is its value in the old state, if it had one. The
/// strategy `keys` puts into `noted` the keys of the dict that the new
/// record changed.
fn both_changed(
    heap: &mut Heap,
    canon: &mut Canon,
    strategy: Strategy,
    old: Option<Value>,
    committed: Value,
    new: Value,
    noted: &mut Option<Noted>,
) -> Result<Value, Outcome> {
    match strategy {
        Strategy::Strict => Err(Outcome::Refused(Reason::FieldChanged)),
        Strategy::Lww => Ok(new),
        // `max(committed, new)` of Python 3: the new value when it is
        // greater, else the committed one.
        Strategy::Max => match less(heap, committed, new) {
            Ok(true) => Ok(new),
            Ok(false) => Ok(committed),
            Err(Undecided::Budget) => Err(undecided(Undecided::Budget)),
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
        Strategy::Union => union(heap, canon, old, committed, new),
        Strategy::Append => append(heap, canon, old, committed, new),
        Strategy::Keys => keys(heap, canon, old, committed, new, noted),
    }
}

/// Tells if `a < b` as Python 3 does (`Heap::lt`). Two `DateTime`s compare
/// by their time, the first item of their state, as their `__lt__` does.
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

/// The strategy `union`: the items of the old value that neither side
/// removed, then the items that the committed side added, then the items
/// that the new side added, each item once, in the kind of the new value.
/// The three values are lists, tuples or sets, and two items are one when
/// they have the same structure. A reorder of the items on one side does
/// not show in the result: the order is that of the old value, then of the
/// committed one, then of the new one.
fn union(
    heap: &mut Heap,
    canon: &mut Canon,
    old: Option<Value>,
    committed: Value,
    new: Value,
) -> Result<Value, Outcome> {
    let not_items = Outcome::Undecided("values that the strategy union does not take");
    let (_, old_items) = old
        .and_then(|old| items(heap, old))
        .ok_or(not_items.clone())?;
    let (_, committed_items) = items(heap, committed).ok_or(not_items.clone())?;
    let (kind, new_items) = items(heap, new).ok_or(not_items)?;
    let mut keys = |items: &[Value]| -> Result<Vec<u32>, Outcome> {
        items
            .iter()
            .map(|&item| canon.id(heap, item).map_err(undecided))
            .collect()
    };
    let old_keys = keys(&old_items)?;
    let committed_keys = keys(&committed_items)?;
    let new_keys = keys(&new_items)?;
    let old_set: HashSet<u32> = old_keys.iter().copied().collect();
    let committed_set: HashSet<u32> = committed_keys.iter().copied().collect();
    let new_set: HashSet<u32> = new_keys.iter().copied().collect();
    let mut taken: HashSet<u32> = HashSet::new();
    let mut result = Vec::new();
    for (item, &key) in old_items.iter().zip(&old_keys) {
        if committed_set.contains(&key) && new_set.contains(&key) && taken.insert(key) {
            result.push(*item);
        }
    }
    for (side, side_keys) in [(&committed_items, &committed_keys), (&new_items, &new_keys)] {
        for (item, &key) in side.iter().zip(side_keys) {
            if !old_set.contains(&key) && taken.insert(key) {
                result.push(*item);
            }
        }
    }
    Ok(make(heap, kind, result))
}

/// The strategy `append`: the three values are lists, or the three are
/// tuples, and the old value is a prefix of both others; the result is the
/// committed value, then the items that the new side added after the old
/// value.
fn append(
    heap: &mut Heap,
    canon: &mut Canon,
    old: Option<Value>,
    committed: Value,
    new: Value,
) -> Result<Value, Outcome> {
    let not_append = Outcome::Refused(Reason::NotAppend);
    let sequence = |heap: &Heap, value: Value| match items(heap, value) {
        Some((kind @ (Kind::List | Kind::Tuple), items)) => Some((kind, items)),
        _ => None,
    };
    let (old_kind, old_items) = old
        .and_then(|old| sequence(heap, old))
        .ok_or(not_append.clone())?;
    let (committed_kind, committed_items) = sequence(heap, committed).ok_or(not_append.clone())?;
    let (kind, new_items) = sequence(heap, new).ok_or(not_append.clone())?;
    let tuple = |kind| matches!(kind, Kind::Tuple);
    if tuple(old_kind) != tuple(kind) || tuple(committed_kind) != tuple(kind) {
        return Err(not_append);
    }
    for items in [&committed_items, &new_items] {
        if items.len() < old_items.len() {
            return Err(not_append);
        }
        for (&a, &b) in old_items.iter().zip(items.iter()) {
            if !canon.same(heap, a, b).map_err(undecided)? {
                return Err(not_append);
            }
        }
    }
    let mut result = committed_items;
    result.extend_from_slice(&new_items[old_items.len()..]);
    Ok(make(heap, kind, result))
}

/// One dict of the strategy `keys`: its items, the key of each item, and
/// the place of each key.
struct Mapping {
    items: Vec<(Value, Value)>,
    keys: Vec<DictKey>,
    places: HashMap<DictKey, usize>,
}

impl Mapping {
    /// Returns the dict of `value`, or `None` when `value` is not a dict.
    /// A key that the model does not keep gives no answer.
    fn of(heap: &Heap, value: Value) -> Option<Result<Mapping, Outcome>> {
        let items = heap.dict(value)?.to_vec();
        let keys: Result<Vec<DictKey>, _> =
            items.iter().map(|&(key, _)| heap.dict_key(key)).collect();
        let Ok(keys) = keys else {
            return Some(Err(Outcome::Undecided(
                "a key that the strategy keys does not take",
            )));
        };
        let places = keys.iter().cloned().zip(0..).collect();
        Some(Ok(Mapping {
            items,
            keys,
            places,
        }))
    }

    /// Returns the value of `key`, if the dict holds it.
    fn get(&self, key: &DictKey) -> Option<Value> {
        self.places.get(key).map(|&place| self.items[place].1)
    }

    /// Returns the key object of `key`, if the dict holds it.
    fn key(&self, key: &DictKey) -> Option<Value> {
        self.places.get(key).map(|&place| self.items[place].0)
    }
}

/// The strategy `keys`: the three values are dicts, merged key by key. A key
/// that only one side changed, added or deleted takes that change; a key
/// that both sides changed to values of one structure takes that value;
/// two other changes of one key refuse. The merged dict holds the keys of
/// the committed dict in its order, then the keys that only the new side
/// added, in the order of the new dict, each with the key object of its
/// dict. Puts into `noted` the keys that the new record changed.
fn keys(
    heap: &mut Heap,
    canon: &mut Canon,
    old: Option<Value>,
    committed: Value,
    new: Value,
    noted: &mut Option<Noted>,
) -> Result<Value, Outcome> {
    let not_dicts = Outcome::Undecided("values that the strategy keys does not take");
    let read = |value: Option<Value>| -> Result<Mapping, Outcome> {
        let mapping = value
            .and_then(|value| Mapping::of(heap, value))
            .ok_or(not_dicts.clone())??;
        check_shared(
            heap,
            &mapping.items,
            "an object that two keys of a mapping share",
        )?;
        Ok(mapping)
    };
    let (old, committed, new) = (read(old)?, read(Some(committed))?, read(Some(new))?);
    // Every key of the three dicts, in the order of the model.
    let all: BTreeSet<&DictKey> = old
        .keys
        .iter()
        .chain(&committed.keys)
        .chain(&new.keys)
        .collect();
    let changed =
        |canon: &mut Canon, side: &Mapping, key: &DictKey| match (old.get(key), side.get(key)) {
            (None, None) => Ok(false),
            (Some(a), Some(b)) => Ok(!canon.same(heap, a, b).map_err(undecided)?),
            _ => Ok(true),
        };
    let mut by_committed: HashSet<&DictKey> = HashSet::new();
    let mut by_new: Vec<&DictKey> = Vec::new();
    for &key in &all {
        if changed(canon, &committed, key)? {
            by_committed.insert(key);
        }
        if changed(canon, &new, key)? {
            by_new.push(key);
        }
    }
    *noted = Some(Noted {
        changed: by_new.len(),
        texts: by_new
            .iter()
            .take(MAX_KEYS)
            .map(|&key| {
                let object = new
                    .key(key)
                    .or_else(|| old.key(key))
                    .expect("a changed key");
                text(heap, object)
            })
            .collect(),
    });
    let by_new: HashSet<&DictKey> = by_new.into_iter().collect();
    // The value of each key after the merge. `None` deletes it.
    let mut merged: HashMap<&DictKey, Option<Value>> = HashMap::with_capacity(all.len());
    for &key in &all {
        let (c, n) = (committed.get(key), new.get(key));
        let value = match (by_committed.contains(key), by_new.contains(key)) {
            (_, false) => c,
            (false, true) => n,
            (true, true) => match (c, n) {
                (None, None) => None,
                (Some(c), Some(n)) if canon.same(heap, c, n).map_err(undecided)? => Some(c),
                (Some(_), Some(_)) => return Err(Outcome::Refused(Reason::KeyChanged)),
                _ => return Err(Outcome::Refused(Reason::KeyDeleted)),
            },
        };
        merged.insert(key, value);
    }
    let mut items = Vec::with_capacity(all.len());
    for (key, &(object, _)) in committed.keys.iter().zip(&committed.items) {
        if let Some(&Some(value)) = merged.get(key) {
            items.push((object, value));
        }
    }
    for (key, &(object, _)) in new.keys.iter().zip(&new.items) {
        if !committed.places.contains_key(key)
            && let Some(&Some(value)) = merged.get(key)
        {
            items.push((object, value));
        }
    }
    Ok(heap.new_dict(items))
}
