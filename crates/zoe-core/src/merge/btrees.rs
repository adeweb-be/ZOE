//! The merges of BTrees: `_bucket__p_resolveConflict`, `bucket_merge` and
//! `BTree__p_resolveConflict` of BTrees, and `Length._p_resolveConflict`.
//!
//! The code follows the C code of BTrees step by step, so that each merge
//! and each refusal is the one of BTrees. Python compares the keys and the
//! values of an object family; `Heap::compare` does it here, and a
//! comparison that it cannot make stops the merge with no answer.

use std::cell::Cell;
use std::cmp::Ordering;

use zoe_pickle::btrees::{
    BucketState, Class, Family, KeyType, Shape, TreeState, ValueType, bucket_state,
    new_bucket_state, new_tree_state, tree_state,
};
use zoe_pickle::python::{Heap, Object, Pickler, Undecided, Value};

use super::posting;
use super::{MAX_KEYS, Merge, Mode, Outcome, Reason, Trees};

/// Merges the three records of a conflict of `class`, and notes in `merge`
/// the keys that the new record changed.
pub(super) fn merge(
    trees: &Trees,
    class: Class,
    posting_dicts: Mode,
    merge: &mut Merge,
) -> Outcome {
    let (outcome, ruled) = merge_once(trees, class, posting_dicts != Mode::Off, merge);
    // In shadow mode, the rules only tell what they would do. The vote then
    // does what it does with the rules off, so the conflict goes on as it
    // does without them.
    if posting_dicts == Mode::Shadow && ruled {
        merge.shadow = Some(outcome);
        // The merge without the rules can stop early, so the keys of the
        // line are those of the merge of the rules.
        let (keys, changed) = (std::mem::take(&mut merge.keys), merge.changed);
        let outcome = merge_once(trees, class, false, merge).0;
        merge.keys = keys;
        merge.changed = changed;
        return outcome;
    }
    outcome
}

/// Merges the three records once, with the rules of ADR 0042 when `rules`
/// is true. Also tells if a rule compared a posting dict or let a
/// delete of a first key through.
fn merge_once(trees: &Trees, class: Class, rules: bool, merge: &mut Merge) -> (Outcome, bool) {
    // The values of a bucket can hold a `DateTime`, a `Message` or
    // `Missing.Value`, as the metadata of a catalog does.
    let mut heap = Heap::with_values();
    heap.set_budget(super::budget(trees.len));
    // ZODB's resolver loads the new record, then the old one, then the
    // committed one. A persistent reference takes its persistent id from
    // the first record that holds it.
    let loaded = heap.load(&trees.new).and_then(|(class, new)| {
        let (_, old) = heap.load(&trees.old)?;
        let (_, committed) = heap.load(&trees.committed)?;
        Ok((class, [old, committed, new]))
    });
    let (class_value, states) = match loaded {
        Ok(loaded) => loaded,
        Err(unsupported) => return (Outcome::Undecided(unsupported.0), false),
    };
    // ZODB writes the class of a bucket, a set and a Length as a GLOBAL
    // alone, and the resolver writes it again so.
    if heap.class(class_value).is_none() {
        return (
            Outcome::Undecided("a class pickle of another format"),
            false,
        );
    }
    let mut ruled = false;
    let state = match class {
        Class::Length => length(&states),
        Class::Family(shape, family) => {
            let mut merger = Merger {
                heap: &mut heap,
                family,
                mapping: shape.is_mapping(),
                rules,
                posting: rules && posting::applies(family),
                first_key: false,
                key_refs: false,
                ruled: Cell::new(false),
                items: Vec::new(),
                keys: Vec::new(),
                changed: 0,
            };
            let state = merger.resolve(shape, &states);
            merge.keys = std::mem::take(&mut merger.keys);
            merge.changed = merger.changed;
            ruled = merger.ruled.get();
            state
        }
    };
    let outcome = match state {
        Ok(state) => {
            let mut pickler = Pickler::new(&heap);
            match pickler.dump(class_value).and_then(|()| pickler.dump(state)) {
                Ok(()) => Outcome::Merged(pickler.into_bytes()),
                Err(unsupported) => Outcome::Undecided(unsupported.0),
            }
        }
        Err(outcome) => outcome,
    };
    (outcome, ruled)
}

/// `Length._p_resolveConflict(old, s1, s2)`: `s1 + s2 - old`.
fn length(states: &[Value; 3]) -> Result<Value, Outcome> {
    let [Value::Int(old), Value::Int(committed), Value::Int(new)] = *states else {
        return Err(Outcome::Undecided("a Length whose value is not an int"));
    };
    committed
        .checked_add(new)
        .and_then(|sum| sum.checked_sub(old))
        .map(Value::Int)
        .ok_or(Outcome::Undecided("a Length that does not fit 128 bits"))
}

/// A bucket of the merge: its keys and its values, as the C bucket keeps
/// them, and its next bucket.
struct Bucket {
    keys: Vec<Value>,
    values: Vec<Value>,
    next: Option<Value>,
}

/// The `SetIteration` of BTrees over a bucket. `position` is 0 before the
/// first `next`, the index of the current item plus 1 after it, and -1 at
/// the end.
struct Cursor<'a> {
    bucket: &'a Bucket,
    position: isize,
}

impl<'a> Cursor<'a> {
    /// A cursor on the first item, as `initSetIteration` and the first
    /// `next` of the merge leave it.
    fn first(bucket: &'a Bucket) -> Self {
        let mut cursor = Cursor {
            bucket,
            position: 0,
        };
        cursor.next();
        cursor
    }

    fn next(&mut self) {
        if self.position >= 0 {
            if (self.position as usize) < self.bucket.keys.len() {
                self.position += 1;
            } else {
                self.position = -1;
            }
        }
    }

    fn on(&self) -> bool {
        self.position >= 0
    }

    fn index(&self) -> usize {
        self.position as usize - 1
    }

    fn key(&self) -> Value {
        self.bucket.keys[self.index()]
    }

    fn value(&self) -> Value {
        self.bucket.values[self.index()]
    }
}

/// The state of one merge of buckets.
struct Merger<'h> {
    heap: &'h mut Heap,
    family: Family,
    mapping: bool,
    /// True when the rules of ADR 0042 are on or in shadow mode.
    rules: bool,
    /// True when the posting-dict rule applies to this family.
    posting: bool,
    /// True when the first-key rule applies: the rules are on or in shadow
    /// mode, and the merge is of the one bucket that a tree holds inline,
    /// with no next bucket.
    first_key: bool,
    /// True when a rule compared or merged a posting dict, or let a delete
    /// of a first key through.
    ruled: Cell<bool>,
    /// True when the merge orders key references by the oids of their
    /// objects: the split merge of ADR 0048.
    key_refs: bool,
    /// The keys and the values of the merged bucket, in order.
    items: Vec<Value>,
    /// The keys that the new record changed, as text, at most `MAX_KEYS`.
    keys: Vec<String>,
    /// The number of keys that the new record changed.
    changed: usize,
}

/// Returns the refusal of a reason code of BTrees.
fn refuse(code: u8) -> Outcome {
    Outcome::Refused(Reason::BTrees(code))
}

impl Merger<'_> {
    /// `_bucket__p_resolveConflict`, and `BTree__p_resolveConflict` for a
    /// tree. Returns the merged state.
    fn resolve(&mut self, shape: Shape, states: &[Value; 3]) -> Result<Value, Outcome> {
        let tree = matches!(shape, Shape::Tree | Shape::TreeSet);
        let mut buckets = Vec::with_capacity(3);
        for &state in states {
            // `get_bucket_state` reads the old state first, then the
            // committed one, then the new one.
            let bucket = if tree {
                match tree_state(self.heap, state) {
                    Some(TreeState::Empty) => None,
                    Some(TreeState::Inline(bucket)) => Some(bucket),
                    Some(TreeState::Split) => return Err(refuse(11)),
                    None => return Err(Outcome::Undecided("a tree state of another shape")),
                }
            } else {
                Some(state)
            };
            buckets.push(self.bucket(bucket)?);
        }
        let [old, committed, new] = <[Bucket; 3]>::try_from(buckets)
            .ok()
            .expect("three buckets");
        // A root that holds its one bucket inline has no parent node to
        // change. BTrees also writes the one bucket of a node inside a tree
        // inline, when the bucket has no oid at the pickle of the node. The
        // bucket of such a node has a next bucket, except in the node of
        // the last bucket. A record cannot tell a root from that last node,
        // so the rule takes only a delete by the committed transaction
        // (ADR 0042).
        self.first_key = self.rules && tree && old.next.is_none();
        // The next buckets must be the same objects.
        for other in [committed.next, new.next] {
            if old.next != other {
                return Err(self.next_differs(old.next, other));
            }
        }
        self.bucket_merge(&old, &committed, &new)?;
        let items = std::mem::take(&mut self.items);
        let state = new_bucket_state(self.heap, items, old.next);
        Ok(if tree {
            new_tree_state(self.heap, state)
        } else {
            state
        })
    }

    /// Returns the outcome of two next buckets that are not the same object.
    /// ZODB's resolver makes one persistent reference of each persistent id,
    /// with the class that `find_global` gives. Two spellings of one class,
    /// such as an alias module, give one reference there and two here. So
    /// two references to one oid and one database give no answer. Other
    /// next buckets are a bucket split.
    fn next_differs(&self, a: Option<Value>, b: Option<Value>) -> Outcome {
        let reference = |value: Option<Value>| value.and_then(|value| self.heap.reference(value));
        match (reference(a), reference(b)) {
            (Some(a), Some(b)) if a.oid == b.oid && a.database == b.database => {
                Outcome::Undecided("next buckets of one oid with two persistent ids")
            }
            _ => refuse(0),
        }
    }

    /// Reads the state of a bucket as its `__setstate__` does. `None` is an
    /// empty bucket.
    fn bucket(&self, state: Option<Value>) -> Result<Bucket, Outcome> {
        let Some(state) = state else {
            return Ok(Bucket {
                keys: Vec::new(),
                values: Vec::new(),
                next: None,
            });
        };
        let Some(BucketState { items, next }) = bucket_state(self.heap, state, self.mapping) else {
            return Err(Outcome::Undecided("a bucket state of another shape"));
        };
        let untaken = Outcome::Undecided("a key or a value that the family does not take");
        let mut bucket = Bucket {
            keys: Vec::new(),
            values: Vec::new(),
            next,
        };
        if self.mapping {
            for &[key, value] in items.as_chunks::<2>().0 {
                let key = self.family.keys.stored(key).ok_or(untaken.clone())?;
                let value = self.family.values.stored(value).ok_or(untaken.clone())?;
                bucket.keys.push(key);
                bucket.values.push(value);
            }
        } else {
            for key in items {
                bucket
                    .keys
                    .push(self.family.keys.stored(key).ok_or(untaken.clone())?);
            }
        }
        Ok(bucket)
    }

    /// `TEST_KEY_SET_OR`: compares two keys.
    fn compare_keys(&self, a: Value, b: Value) -> Result<Ordering, Outcome> {
        compare_keys(&*self.heap, self.family.keys, self.key_refs, a, b)
    }

    /// `TEST_VALUE(a, b) == 0`: tells if two values are equal for the
    /// merge.
    fn values_equal(&self, a: Value, b: Value) -> Result<bool, Outcome> {
        match self.family.values {
            ValueType::Object => {
                if self.posting
                    && let Some(equal) = posting::equal(self.heap, a, b)
                {
                    self.ruled.set(true);
                    return Ok(equal);
                }
                Ok(compare_objects(&*self.heap, self.key_refs, a, b)? == Ordering::Equal)
            }
            // `(K < T) ? -1 : (K > T) ? 1 : 0` of the C floats: a NaN is
            // equal to every value.
            ValueType::Float32 => {
                let (Value::Float(a), Value::Float(b)) = (a, b) else {
                    unreachable!("a bucket of floats keeps floats");
                };
                Ok(!matches!(
                    a.partial_cmp(&b),
                    Some(Ordering::Less | Ordering::Greater)
                ))
            }
            _ => Ok(int(a) == int(b)),
        }
    }

    /// `merge_output`: adds the current item of `cursor` to the merged
    /// bucket.
    fn output(&mut self, cursor: &Cursor) {
        self.items.push(cursor.key());
        if self.mapping {
            self.items.push(cursor.value());
        }
    }

    /// Notes a key that the new record changed, inserted or deleted.
    fn changed(&mut self, key: Value) {
        self.changed += 1;
        if self.keys.len() < MAX_KEYS {
            self.keys.push(text(self.heap, key));
        }
    }

    /// Notes the key of `new` when its value differs from the value of
    /// `old`. The note changes nothing in the merge: a comparison without
    /// an answer counts as a change.
    fn changed_value(&mut self, old: &Cursor, new: &Cursor) {
        if self.mapping && !matches!(self.values_equal(old.value(), new.value()), Ok(true)) {
            self.changed(new.key());
        }
    }

    /// Refuses the delete of the first item of a bucket by the committed
    /// transaction: in a tree of many buckets, it can change the parent
    /// node. The first-key rule lets it through in the bucket that a tree
    /// holds inline, with no next bucket. The new transaction marked each
    /// node on its path to the bucket as read-current, so a parent node
    /// that the committed transaction changed fails the vote.
    fn delete_first_key(&self) -> Result<(), Outcome> {
        if !self.first_key {
            return Err(refuse(13));
        }
        self.ruled.set(true);
        Ok(())
    }

    /// `bucket_merge(s1, s2, s3)` of `MergeTemplate.c`: `old`, `committed`
    /// and `new`. The merged items go to `self.items`.
    fn bucket_merge(
        &mut self,
        old: &Bucket,
        committed: &Bucket,
        new: &Bucket,
    ) -> Result<(), Outcome> {
        if committed.keys.is_empty() || new.keys.is_empty() {
            return Err(refuse(12));
        }
        let set = !self.mapping;
        let (mut i1, mut i2, mut i3) = (
            Cursor::first(old),
            Cursor::first(committed),
            Cursor::first(new),
        );
        while i1.on() && i2.on() && i3.on() {
            let cmp12 = self.compare_keys(i1.key(), i2.key())?;
            let cmp13 = self.compare_keys(i1.key(), i3.key())?;
            if cmp12 == Ordering::Equal {
                if cmp13 == Ordering::Equal {
                    if set || self.values_equal(i1.value(), i2.value())? {
                        // A change in the new value, or no change.
                        self.changed_value(&i1, &i3);
                        self.output(&i3);
                    } else if self.values_equal(i1.value(), i3.value())? {
                        // A change in the committed value.
                        self.output(&i2);
                    } else if self.posting {
                        // Both changed the value: the posting-dict rule.
                        let merged = posting::merge(self.heap, i1.value(), i2.value(), i3.value())?;
                        self.changed(i3.key());
                        self.items.push(i3.key());
                        self.items.push(merged);
                    } else {
                        return Err(refuse(1));
                    }
                    i1.next();
                    i2.next();
                    i3.next();
                } else if cmp13 == Ordering::Greater {
                    // An insert in the new bucket.
                    self.changed(i3.key());
                    self.output(&i3);
                    i3.next();
                } else if set || self.values_equal(i1.value(), i2.value())? {
                    // A delete in the new bucket.
                    if i3.position == 1 {
                        // The new transaction can have moved the separator
                        // of a parent node in its own write of that node.
                        return Err(refuse(13));
                    }
                    self.changed(i1.key());
                    i1.next();
                    i2.next();
                } else {
                    return Err(refuse(2));
                }
            } else if cmp13 == Ordering::Equal {
                if cmp12 == Ordering::Greater {
                    // An insert in the committed bucket.
                    self.output(&i2);
                    i2.next();
                } else if set || self.values_equal(i1.value(), i3.value())? {
                    // A delete in the committed bucket.
                    if i2.position == 1 {
                        self.delete_first_key()?;
                    }
                    i1.next();
                    i3.next();
                } else {
                    return Err(refuse(3));
                }
            } else {
                // Both keys changed.
                let cmp23 = self.compare_keys(i2.key(), i3.key())?;
                if cmp23 == Ordering::Equal {
                    // Two inserts or two deletes of one key.
                    return Err(refuse(4));
                }
                if cmp12 == Ordering::Greater {
                    if cmp23 == Ordering::Greater {
                        self.changed(i3.key());
                        self.output(&i3);
                        i3.next();
                    } else {
                        self.output(&i2);
                        i2.next();
                    }
                } else if cmp13 == Ordering::Greater {
                    self.changed(i3.key());
                    self.output(&i3);
                    i3.next();
                } else {
                    // Both deleted the key of the old bucket.
                    return Err(refuse(5));
                }
            }
        }
        // The inserts after the end of the old bucket.
        while i2.on() && i3.on() {
            let cmp23 = self.compare_keys(i2.key(), i3.key())?;
            if cmp23 == Ordering::Equal {
                return Err(refuse(6));
            }
            if cmp23 == Ordering::Greater {
                self.changed(i3.key());
                self.output(&i3);
                i3.next();
            } else {
                self.output(&i2);
                i2.next();
            }
        }
        // The rest of the old bucket, deleted in the new one.
        while i1.on() && i2.on() {
            let cmp12 = self.compare_keys(i1.key(), i2.key())?;
            if cmp12 == Ordering::Greater {
                self.output(&i2);
                i2.next();
            } else if cmp12 == Ordering::Equal
                && (set || self.values_equal(i1.value(), i2.value())?)
            {
                self.changed(i1.key());
                i1.next();
                i2.next();
            } else {
                return Err(refuse(7));
            }
        }
        // The rest of the old bucket, deleted in the committed one.
        while i1.on() && i3.on() {
            let cmp13 = self.compare_keys(i1.key(), i3.key())?;
            if cmp13 == Ordering::Greater {
                self.changed(i3.key());
                self.output(&i3);
                i3.next();
            } else if cmp13 == Ordering::Equal
                && (set || self.values_equal(i1.value(), i3.value())?)
            {
                i1.next();
                i3.next();
            } else {
                return Err(refuse(8));
            }
        }
        if i1.on() {
            return Err(refuse(9));
        }
        while i2.on() {
            self.output(&i2);
            i2.next();
        }
        while i3.on() {
            self.changed(i3.key());
            self.output(&i3);
            i3.next();
        }
        if self.items.is_empty() {
            // The merge cannot unlink an empty bucket from its BTree.
            return Err(refuse(10));
        }
        Ok(())
    }
}

/// `TEST_KEY_SET_OR`: compares two keys of the type `keys`. With
/// `key_refs`, two key references of one class compare by the oids of
/// their objects (ADR 0048).
pub(super) fn compare_keys(
    heap: &Heap,
    keys: KeyType,
    key_refs: bool,
    a: Value,
    b: Value,
) -> Result<Ordering, Outcome> {
    if keys != KeyType::Object {
        return Ok(int(a).cmp(&int(b)));
    }
    compare_objects(heap, key_refs, a, b)
}

/// `object_compare` of BTrees on two keys or two values of the type `O`.
/// With `key_refs`, two key references of one class compare by the oids of
/// their objects, as their `_get_cmp_keys` does in conflict resolution:
/// BTrees calls the same comparison for the keys and for the values.
fn compare_objects(heap: &Heap, key_refs: bool, a: Value, b: Value) -> Result<Ordering, Outcome> {
    if key_refs {
        match (key_reference(heap, a), key_reference(heap, b)) {
            (None, None) => {}
            (Some(Some((p, x))), Some(Some((q, y)))) if p == q => return Ok(x.cmp(y)),
            _ => {
                return Err(Outcome::Undecided(
                    "key references that the order of key references does not take",
                ));
            }
        }
    }
    heap.compare(a, b).map_err(undecided)
}

/// The classes of key references whose order the split merge knows. Their
/// `_get_cmp_keys` compares two references of one class, in conflict
/// resolution, by the oids of their objects when neither object names a
/// database.
const KEY_REFERENCES: [(&str, &str); 2] = [
    ("five.intid.keyreference", "KeyReferenceToPersistent"),
    ("zope.keyreference.persistent", "KeyReferenceToPersistent"),
];

/// Returns `None` when `value` is not an object of a class of
/// [`KEY_REFERENCES`]. Else returns the index of its class and the oid of
/// its object, or `Some(None)` when the order does not take it: its state
/// holds no `object`, or that object is not a reference to an oid of the
/// same database.
fn key_reference(heap: &Heap, value: Value) -> Option<Option<(usize, &[u8])>> {
    let instance = heap.instance(value)?;
    let class = heap.class(instance.class)?;
    let index = KEY_REFERENCES.iter().position(|&known| known == class)?;
    let object = instance
        .state
        .and_then(|state| heap.dict(state))
        .and_then(|items| {
            items
                .iter()
                .find(|&&(key, _)| heap.str(key) == Some("object"))
                .map(|&(_, object)| object)
        });
    let reference = object.and_then(|object| heap.reference(object));
    Some(
        reference
            .filter(|reference| reference.database.is_none() && !reference.weak)
            .map(|reference| (index, &reference.oid[..])),
    )
}

/// Merges the items of the range of a bucket that the split merge (ADR
/// 0048) merges, with the rules of `bucket_merge` and without the rules of
/// ADR 0042. `old`, `committed` and `new` hold the items of the range on
/// each side: each key, then its value for a mapping. Two key references
/// of one class compare by the oids of their objects, as keys and as
/// values.
///
/// Returns the merged items, the keys that the new side changed and their
/// number. A key of `old` that a side deleted gives no answer.
pub(super) fn merge_range(
    heap: &mut Heap,
    family: Family,
    mapping: bool,
    old: &[Value],
    committed: &[Value],
    new: &[Value],
) -> Result<(Vec<Value>, Vec<String>, usize), Outcome> {
    let mut merger = Merger {
        heap,
        family,
        mapping,
        rules: false,
        posting: false,
        first_key: false,
        ruled: Cell::new(false),
        key_refs: true,
        items: Vec::new(),
        keys: Vec::new(),
        changed: 0,
    };
    let old = merger.range(old)?;
    let committed = merger.range(committed)?;
    let new = merger.range(new)?;
    for side in [&committed, &new] {
        if merger.deletes(&old, side)? {
            return Err(Outcome::Undecided(
                "a delete in a range that a split merges",
            ));
        }
    }
    merger.bucket_merge(&old, &committed, &new)?;
    Ok((merger.items, merger.keys, merger.changed))
}

impl Merger<'_> {
    /// Reads the items of a range as a bucket without a next bucket.
    fn range(&self, items: &[Value]) -> Result<Bucket, Outcome> {
        let untaken = Outcome::Undecided("a key or a value that the family does not take");
        let mut bucket = Bucket {
            keys: Vec::new(),
            values: Vec::new(),
            next: None,
        };
        if self.mapping {
            if !items.len().is_multiple_of(2) {
                return Err(Outcome::Undecided("a bucket state of another shape"));
            }
            for &[key, value] in items.as_chunks::<2>().0 {
                let key = self.family.keys.stored(key).ok_or(untaken.clone())?;
                let value = self.family.values.stored(value).ok_or(untaken.clone())?;
                bucket.keys.push(key);
                bucket.values.push(value);
            }
        } else {
            for &key in items {
                let key = self.family.keys.stored(key).ok_or(untaken.clone())?;
                bucket.keys.push(key);
            }
        }
        Ok(bucket)
    }

    /// Tells if `side` lacks a key of `old`. Both hold their keys in
    /// ascending order.
    fn deletes(&self, old: &Bucket, side: &Bucket) -> Result<bool, Outcome> {
        let mut next = 0;
        for &key in &old.keys {
            while next < side.keys.len()
                && self.compare_keys(side.keys[next], key)? == Ordering::Less
            {
                next += 1;
            }
            if next == side.keys.len()
                || self.compare_keys(side.keys[next], key)? != Ordering::Equal
            {
                return Ok(true);
            }
            next += 1;
        }
        Ok(false)
    }
}

/// Returns the outcome of a comparison without an answer.
fn undecided(undecided: Undecided) -> Outcome {
    match undecided {
        Undecided::Unknown => Outcome::Undecided("values that Python compares, and Rust cannot"),
        Undecided::Raises => Outcome::Refused(Reason::Uncomparable),
        Undecided::Budget => Outcome::Undecided("comparisons over the step budget"),
    }
}

/// Returns the int of a key or a value of an int family.
fn int(value: Value) -> i128 {
    match value {
        Value::Int(int) => int,
        _ => unreachable!("a bucket of ints keeps ints"),
    }
}

/// The longest text of a key in the resolution log, in characters.
const KEY_TEXT: usize = 80;

/// Returns a short text of a key for the resolution log, close to its
/// `repr` in Python.
pub(super) fn text(heap: &Heap, key: Value) -> String {
    let text = match key {
        Value::None => "None".to_owned(),
        Value::Bool(true) => "True".to_owned(),
        Value::Bool(false) => "False".to_owned(),
        Value::Int(int) => int.to_string(),
        Value::Float(float) => format!("{float:?}"),
        Value::Object(_) => match heap.object(key) {
            Some(Object::Str(text)) => format!("{text:?}"),
            Some(Object::Bytes(bytes)) => format!("b'{}'", bytes.escape_ascii()),
            Some(Object::Tuple(items)) => {
                let shown: Vec<String> = items
                    .iter()
                    .map(|&item| match heap.object(item) {
                        Some(Object::Tuple(_)) => "(...)".to_owned(),
                        _ => text(heap, item),
                    })
                    .collect();
                format!("({})", shown.join(", "))
            }
            Some(Object::Reference(reference)) => {
                let oid: String = reference.oid.iter().map(|b| format!("{b:02x}")).collect();
                format!("<reference 0x{oid}>")
            }
            Some(_) => "<object>".to_owned(),
            None => unreachable!("an object value has an object"),
        },
    };
    match text.char_indices().nth(KEY_TEXT) {
        Some((end, _)) => format!("{}...", &text[..end]),
        None => text,
    }
}
