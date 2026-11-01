//! The comparisons of the values of a [`Heap`], as Python and the
//! `object_compare` of BTrees make them, and the keys of a dict that the
//! model keeps.

use std::cmp::Ordering;

use super::{Heap, Object, Reference, Unsupported, Value};

/// Why a comparison of [`Heap::compare`], [`Heap::eq`] or [`Heap::lt`] has
/// no answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Undecided {
    /// The model does not know what Python gives: values of mixed types,
    /// a list or a dict, an int against a float. Python can raise or give
    /// an answer.
    Unknown,
    /// Python raises. A persistent reference compares only with an equal
    /// one.
    Raises,
    /// The comparison would take more steps than the budget of the heap
    /// leaves ([`Heap::set_budget`]).
    Budget,
}

/// A key of a dict that the model keeps: an int, a str, a bytes, or, in a
/// heap of [`Heap::with_instances`], a tuple of those. Two keys are one key
/// of a Python dict when they are equal here. The order is the order of
/// the variants, then of the values: ints by value, strs by code point,
/// bytes by byte, and tuples item by item, with the same order for two
/// items. Two tuples compare at the first place where their items are not
/// equal. When the shorter tuple ends before such a place, it comes first.
/// Python gives the same order for each pair that it orders: two ints, two
/// strs, two bytes, and two tuples with no such place, or whose items at
/// that place are of one kind. Python raises `TypeError` for the other
/// pairs: two keys of two kinds, and two tuples whose items at the first
/// such place are of two kinds. Here these pairs order by the kind of the
/// two keys, or, for two tuples, by the kind of their items at that place:
/// ints, strs, bytes, then tuples. Only the resolution log uses this order.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DictKey {
    /// An int.
    Int(i128),
    /// A str.
    Str(String),
    /// A bytes.
    Bytes(Vec<u8>),
    /// A tuple of ints, strs and bytes.
    Tuple(Vec<KeyItem>),
}

/// An item of a tuple key of a dict.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum KeyItem {
    /// An int.
    Int(i128),
    /// A str.
    Str(String),
    /// A bytes.
    Bytes(Vec<u8>),
}

impl Heap {
    /// Gives the comparisons of the heap a budget of `steps` steps in all:
    /// a step is one pair of values that [`Heap::eq`] or [`Heap::lt`] looks
    /// at. Past the budget, a comparison gives [`Undecided::Budget`]. A
    /// merge sets a budget in proportion to its records, so a value that
    /// shares its parts many times cannot make it take more time than its
    /// size allows.
    pub fn set_budget(&mut self, steps: usize) {
        self.limit = Some(steps);
        self.spent.set(0);
    }

    /// Takes one step of the budget.
    fn step(&self) -> Result<(), Undecided> {
        if let Some(limit) = self.limit {
            let spent = self.spent.get() + 1;
            self.spent.set(spent);
            if spent > limit {
                return Err(Undecided::Budget);
            }
        }
        Ok(())
    }

    /// Compares two keys or two values as BTrees does: `object_compare` of
    /// `BTreeModuleTemplate.c`. `None` is below every other value. Else the
    /// result is `a < b`, then `a == b`.
    pub fn compare(&self, a: Value, b: Value) -> Result<Ordering, Undecided> {
        match (a, b) {
            (Value::None, Value::None) => return Ok(Ordering::Equal),
            (Value::None, _) => return Ok(Ordering::Less),
            (_, Value::None) => return Ok(Ordering::Greater),
            _ => {}
        }
        if self.lt(a, b)? {
            return Ok(Ordering::Less);
        }
        Ok(if self.eq(a, b)? {
            Ordering::Equal
        } else {
            Ordering::Greater
        })
    }

    /// Tells if `a == b`, as `PyObject_RichCompareBool(a, b, Py_EQ)`: an
    /// object is equal to itself, and two tuples or two lists are equal
    /// item by item. Two lists of different lengths are not equal, without
    /// a look at their items.
    pub fn eq(&self, a: Value, b: Value) -> Result<bool, Undecided> {
        // Each frame is a pair of tuples or of lists and the index of their
        // next pair of items.
        let mut frames: Vec<(&[Value], &[Value], usize)> = Vec::new();
        let mut pair = (a, b);
        loop {
            self.step()?;
            let mut answer = match self.eq_leaf(pair.0, pair.1)? {
                Leaf::Answer(answer) => answer,
                Leaf::Tuples(x, y) => {
                    frames.push((x, y, 0));
                    true
                }
            };
            // Give the answer to the frames, until a frame has a next pair.
            loop {
                let Some(frame) = frames.last_mut() else {
                    return Ok(answer);
                };
                let (x, y, next) = *frame;
                if answer && next < x.len().min(y.len()) {
                    frame.2 += 1;
                    pair = (x[next], y[next]);
                    break;
                }
                if answer {
                    answer = x.len() == y.len();
                }
                frames.pop();
            }
        }
    }

    /// Tells if `a < b`, as `PyObject_RichCompare(a, b, Py_LT)`. Two tuples,
    /// or two lists, compare at their first pair of items that are not
    /// equal, or by their length.
    pub fn lt(&self, mut a: Value, mut b: Value) -> Result<bool, Undecided> {
        loop {
            self.step()?;
            let Some((x, y)) = self.sequences(a, b) else {
                return self.lt_leaf(a, b);
            };
            let mut differ = None;
            for (index, (&p, &q)) in x.iter().zip(y).enumerate() {
                if !self.eq(p, q)? {
                    differ = Some(index);
                    break;
                }
            }
            let Some(index) = differ else {
                return Ok(x.len() < y.len());
            };
            a = x[index];
            b = y[index];
        }
    }

    /// Returns the items of `a` and `b` when both are tuples or both are
    /// lists.
    fn sequences(&self, a: Value, b: Value) -> Option<(&[Value], &[Value])> {
        match (self.object(a)?, self.object(b)?) {
            (Object::Tuple(x), Object::Tuple(y)) | (Object::List(x), Object::List(y)) => {
                Some((x, y))
            }
            _ => None,
        }
    }

    /// Compares two values for `==`, or gives the items of two tuples or of
    /// two lists of one length.
    fn eq_leaf(&self, a: Value, b: Value) -> Result<Leaf<'_>, Undecided> {
        if let Some(answer) = self.known_eq(a, b) {
            return answer.map(Leaf::Answer);
        }
        let answer = match (a, b) {
            (Value::Object(x), Value::Object(y)) if x == y => true,
            (Value::None, Value::None) => true,
            (Value::Bool(_) | Value::Int(_), Value::Bool(_) | Value::Int(_)) => {
                int_of(a) == int_of(b)
            }
            (Value::Float(x), Value::Float(y)) => x == y,
            (Value::Int(_) | Value::Bool(_), Value::Float(_))
            | (Value::Float(_), Value::Int(_) | Value::Bool(_)) => {
                return Err(Undecided::Unknown);
            }
            (Value::Object(x), Value::Object(y)) => match (self.get(x), self.get(y)) {
                (Object::Tuple(x), Object::Tuple(y)) => return Ok(Leaf::Tuples(x, y)),
                (Object::Reference(x), Object::Reference(y)) => {
                    if equivalent(x, y) {
                        true
                    } else {
                        return Err(Undecided::Raises);
                    }
                }
                (Object::Reference(_), _) | (_, Object::Reference(_)) => {
                    return Err(Undecided::Raises);
                }
                (Object::Str(x), Object::Str(y)) => x == y,
                (Object::Bytes(x), Object::Bytes(y)) => x == y,
                (Object::List(x), Object::List(y)) => {
                    if x.len() != y.len() {
                        false
                    } else {
                        return Ok(Leaf::Tuples(x, y));
                    }
                }
                (Object::Dict(_), Object::Dict(_)) => return Err(Undecided::Unknown),
                // A class of its own can define `==`.
                (Object::Instance(_) | Object::Set { .. }, _)
                | (_, Object::Instance(_) | Object::Set { .. }) => {
                    return Err(Undecided::Unknown);
                }
                // Objects of two types, or two classes that are not the
                // same object.
                _ => false,
            },
            (Value::Object(x), _) | (_, Value::Object(x)) => match self.get(x) {
                Object::Reference(_) => return Err(Undecided::Raises),
                Object::Instance(_) | Object::Set { .. } => return Err(Undecided::Unknown),
                _ => false,
            },
            // None, a bool, an int or a float against another of them.
            _ => false,
        };
        Ok(Leaf::Answer(answer))
    }

    /// Compares two values that are not two tuples or two lists for `<`.
    fn lt_leaf(&self, a: Value, b: Value) -> Result<bool, Undecided> {
        if let Some(answer) = self.known_lt(a, b) {
            return answer;
        }
        match (a, b) {
            (Value::Bool(_) | Value::Int(_), Value::Bool(_) | Value::Int(_)) => {
                Ok(int_of(a) < int_of(b))
            }
            (Value::Float(x), Value::Float(y)) => Ok(x < y),
            (Value::Object(x), Value::Object(y)) => match (self.get(x), self.get(y)) {
                (Object::Reference(p), Object::Reference(q)) => {
                    // `__lt__` of a reference is `__cmp__(other) < 0`: false
                    // for an equal reference, and a ValueError for another.
                    if x == y || equivalent(p, q) {
                        Ok(false)
                    } else {
                        Err(Undecided::Raises)
                    }
                }
                (Object::Reference(_), _) | (_, Object::Reference(_)) => Err(Undecided::Raises),
                (Object::Str(p), Object::Str(q)) => Ok(p < q),
                (Object::Bytes(p), Object::Bytes(q)) => Ok(p < q),
                _ => Err(Undecided::Unknown),
            },
            (Value::Object(x), _) | (_, Value::Object(x)) => match self.get(x) {
                Object::Reference(_) => Err(Undecided::Raises),
                _ => Err(Undecided::Unknown),
            },
            _ => Err(Undecided::Unknown),
        }
    }

    /// Returns the key of `key`, a key of a dict, when the model keeps it:
    /// an int, a str, a bytes, or, in a heap of [`Heap::with_instances`], a
    /// tuple of those. A bool and a float are not keys here: Python finds
    /// them equal to an int.
    pub fn dict_key(&self, key: Value) -> Result<DictKey, Unsupported> {
        let other = Unsupported("a dict key of another type");
        let item = |value: Value| match value {
            Value::Int(int) => Some(KeyItem::Int(int)),
            Value::Object(id) => match self.get(id) {
                Object::Str(text) => Some(KeyItem::Str(text.clone())),
                Object::Bytes(bytes) => Some(KeyItem::Bytes(bytes.clone())),
                _ => None,
            },
            _ => None,
        };
        if let Some(Object::Tuple(items)) = self.object(key) {
            if !self.instances {
                return Err(other);
            }
            let items: Option<Vec<KeyItem>> = items.iter().map(|&value| item(value)).collect();
            return items.map(DictKey::Tuple).ok_or(other);
        }
        Ok(match item(key).ok_or(other)? {
            KeyItem::Int(int) => DictKey::Int(int),
            KeyItem::Str(text) => DictKey::Str(text),
            KeyItem::Bytes(bytes) => DictKey::Bytes(bytes),
        })
    }
}

/// The answer of [`Heap::eq_leaf`].
enum Leaf<'a> {
    Answer(bool),
    /// The items of two tuples, or of two lists of one length.
    Tuples(&'a [Value], &'a [Value]),
}

/// Returns the int of a bool or an int.
fn int_of(value: Value) -> i128 {
    match value {
        Value::Bool(value) => i128::from(value),
        Value::Int(value) => value,
        _ => unreachable!("int_of takes a bool or an int"),
    }
}

/// Tells if two references are equal for `PersistentReference.__cmp__`:
/// the same oid and database, neither of them weak.
fn equivalent(a: &Reference, b: &Reference) -> bool {
    a.oid == b.oid && a.database == b.database && !a.weak && !b.weak
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode_record;
    use crate::python::tests::{CLASS, dump, load};

    #[test]
    fn comparisons_follow_python() {
        let mut heap = Heap::new();
        let state = b"\x80\x03(X\x01\x00\x00\x00aX\x01\x00\x00\x00bC\x01aK\x01X\x01\x00\x00\x00a\
                      \x86K\x01X\x01\x00\x00\x00b\x86K\x01\x85t.";
        let (_, value) = load(&mut heap, state);
        let v = heap.tuple(value).unwrap().to_vec();
        let (a, b, bytes_a, t1a, t1b, t1) = (v[0], v[1], v[2], v[3], v[4], v[5]);
        assert_eq!(heap.compare(a, b), Ok(Ordering::Less));
        assert_eq!(heap.compare(b, a), Ok(Ordering::Greater));
        assert_eq!(heap.compare(Value::None, a), Ok(Ordering::Less));
        assert_eq!(heap.compare(t1a, t1b), Ok(Ordering::Less));
        assert_eq!(heap.compare(t1, t1a), Ok(Ordering::Less));
        assert_eq!(
            heap.compare(Value::Int(2), Value::Bool(true)),
            Ok(Ordering::Greater)
        );
        assert_eq!(heap.eq(a, bytes_a), Ok(false));
        assert_eq!(heap.compare(a, bytes_a), Err(Undecided::Unknown));
        assert_eq!(
            heap.compare(Value::Int(1), Value::Float(1.0)),
            Err(Undecided::Unknown)
        );
        assert_eq!(
            heap.compare(Value::Float(f64::NAN), Value::Float(f64::NAN)),
            Ok(Ordering::Greater)
        );
        // Two tuples that differ at their first items never compare the
        // items after them.
        let mixed = heap.new_tuple(vec![Value::Int(1), a]);
        let other = heap.new_tuple(vec![Value::Int(2), Value::Int(3)]);
        assert_eq!(heap.compare(mixed, other), Ok(Ordering::Less));
    }

    #[test]
    fn lists_compare_as_python_lists() {
        let mut heap = Heap::new();
        let list = |heap: &mut Heap, items: Vec<Value>| {
            let id = heap.push(Object::List(items));
            Value::Object(id)
        };
        let ab = list(&mut heap, vec![Value::Int(1), Value::Int(2)]);
        let ac = list(&mut heap, vec![Value::Int(1), Value::Int(3)]);
        let a = list(&mut heap, vec![Value::Int(1)]);
        let tuple = heap.new_tuple(vec![Value::Int(1)]);
        assert_eq!(heap.compare(ab, ac), Ok(Ordering::Less));
        assert_eq!(heap.compare(a, ab), Ok(Ordering::Less));
        let again = list(&mut heap, vec![Value::Int(1), Value::Int(2)]);
        assert_eq!(heap.compare(ab, again), Ok(Ordering::Equal));
        // A list and a tuple are never equal, and do not order.
        assert_eq!(heap.eq(a, tuple), Ok(false));
        assert_eq!(heap.compare(a, tuple), Err(Undecided::Unknown));
        // Lists of different lengths are not equal, whatever their items.
        let mixed = list(&mut heap, vec![Value::Int(1), Value::Float(1.0)]);
        assert_eq!(heap.eq(a, mixed), Ok(false));
    }

    #[test]
    fn a_reference_compares_only_with_an_equal_one() {
        let mut heap = Heap::new();
        let one = b"\x80\x03C\x08\x00\x00\x00\x00\x00\x00\x00\x07h\x00\x86q\x01Q.";
        let two = b"\x80\x03C\x08\x00\x00\x00\x00\x00\x00\x00\x08h\x00\x86q\x01Q.";
        let alone = b"\x80\x03C\x08\x00\x00\x00\x00\x00\x00\x00\x07Q.";
        let (_, seven) = load(&mut heap, one);
        let (_, eight) = load(&mut heap, two);
        let (_, spelled) = load(&mut heap, alone);
        assert_ne!(seven, spelled);
        assert_eq!(heap.compare(seven, spelled), Ok(Ordering::Equal));
        assert_eq!(heap.compare(seven, eight), Err(Undecided::Raises));
        assert_eq!(heap.compare(seven, Value::Int(1)), Err(Undecided::Raises));
        assert_eq!(heap.compare(Value::None, seven), Ok(Ordering::Less));
    }

    #[test]
    fn a_heap_with_instances_keeps_the_tuple_keys_of_a_dict() {
        // `{('a', 1): 2, (b'xy',): 3, (): 4}`, as Python 3.13 pickles it.
        let state = b"\x80\x03}q\x01(X\x01\x00\x00\x00aq\x02K\x01\x86q\x03K\x02\
            C\x02xyq\x04\x85q\x05K\x03)K\x04u.";
        let mut heap = Heap::with_instances();
        let (class, dict) = load(&mut heap, state);
        assert_eq!(dump(&heap, class, dict), [CLASS, &state[..]].concat());
        let keys: Vec<DictKey> = heap
            .dict(dict)
            .unwrap()
            .iter()
            .map(|&(key, _)| heap.dict_key(key).unwrap())
            .collect();
        assert_eq!(
            keys,
            [
                DictKey::Tuple(vec![KeyItem::Str("a".to_owned()), KeyItem::Int(1)]),
                DictKey::Tuple(vec![KeyItem::Bytes(b"xy".to_vec())]),
                DictKey::Tuple(Vec::new()),
            ]
        );
        // The heap of tier 1 refuses them, and a tuple of a tuple or of a
        // bool is not a key of the model.
        let tree = decode_record(&[CLASS, &state[..]].concat()).unwrap();
        assert_eq!(
            Heap::new().load(&tree),
            Err(Unsupported("a dict key of another type"))
        );
        for state in [
            &b"\x80\x03}q\x01X\x01\x00\x00\x00aq\x02\x85q\x03\x85q\x04K\x01s."[..],
            b"\x80\x03}q\x01X\x01\x00\x00\x00aq\x02\x88\x86q\x03K\x01s.",
        ] {
            let tree = decode_record(&[CLASS, state].concat()).unwrap();
            assert_eq!(
                Heap::with_instances().load(&tree),
                Err(Unsupported("a dict key of another type")),
                "{state:?}"
            );
        }
    }

    #[test]
    fn dict_keys_of_one_kind_are_in_the_order_of_python() {
        let mut keys = vec![
            DictKey::Str("b".to_owned()),
            DictKey::Int(10),
            DictKey::Tuple(vec![KeyItem::Int(1), KeyItem::Int(2)]),
            DictKey::Str("a".to_owned()),
            DictKey::Int(-3),
            DictKey::Tuple(vec![KeyItem::Int(1)]),
            DictKey::Bytes(b"a".to_vec()),
            DictKey::Str("\u{e9}".to_owned()),
        ];
        keys.sort();
        assert_eq!(
            keys,
            [
                DictKey::Int(-3),
                DictKey::Int(10),
                DictKey::Str("a".to_owned()),
                DictKey::Str("b".to_owned()),
                DictKey::Str("\u{e9}".to_owned()),
                DictKey::Bytes(b"a".to_vec()),
                DictKey::Tuple(vec![KeyItem::Int(1)]),
                DictKey::Tuple(vec![KeyItem::Int(1), KeyItem::Int(2)]),
            ]
        );
    }

    #[test]
    fn the_comparisons_of_a_heap_stop_at_its_budget() {
        let mut heap = Heap::new();
        let mut shared = heap.new_tuple(vec![Value::Int(1)]);
        for _ in 0..60 {
            shared = heap.new_tuple(vec![shared, shared]);
        }
        let mut other = heap.new_tuple(vec![Value::Int(1)]);
        for _ in 0..60 {
            other = heap.new_tuple(vec![other, other]);
        }
        heap.set_budget(10_000);
        assert_eq!(heap.eq(shared, other), Err(Undecided::Budget));
        heap.set_budget(10_000);
        assert_eq!(heap.lt(shared, other), Err(Undecided::Budget));
        // Within its budget, a comparison gives its answer.
        heap.set_budget(10_000);
        assert_eq!(
            heap.compare(Value::Int(1), Value::Int(2)),
            Ok(Ordering::Less)
        );
    }
}
