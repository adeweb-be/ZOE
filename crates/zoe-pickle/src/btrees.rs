//! The classes of BTrees and the shapes of their states, as their
//! `__getstate__` gives them and their `__setstate__` reads them.
//!
//! - A bucket: `(items,)`, or `(items, next)` for a bucket of a BTree that
//!   has a next bucket. `items` holds each key, then its value.
//! - A set: the same, with the keys alone.
//! - A BTree or a tree set: `None` when it is empty; `((bucket,),)` when it
//!   holds one bucket that has no oid, so that its state is inline; else
//!   `(children, firstbucket)`.
//! - `BTrees.Length.Length`: an int.
//!
//! A bucket of an int family keeps C numbers. Its `__setstate__` converts
//! each key and each value, and its `__getstate__` gives new Python ints
//! or floats. [`KeyType::stored`] and [`ValueType::stored`] give the value
//! after that round trip.

use crate::python::{Heap, Value};

/// The type of the keys of a family.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyType {
    /// Any object: `O`.
    Object,
    /// A C `int`: `I`.
    Int32,
    /// A C `unsigned int`: `U`.
    Uint32,
    /// A C `long long`: `L`.
    Int64,
    /// A C `unsigned long long`: `Q`.
    Uint64,
}

/// The type of the values of a family.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueType {
    /// Any object: `O`.
    Object,
    /// A C `int`: `I`.
    Int32,
    /// A C `unsigned int`: `U`.
    Uint32,
    /// A C `long long`: `L`.
    Int64,
    /// A C `unsigned long long`: `Q`.
    Uint64,
    /// A C `float`: `F`.
    Float32,
}

/// A family of BTrees: the type of its keys and of its values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Family {
    /// The type of the keys.
    pub keys: KeyType,
    /// The type of the values. A set has keys alone, but its family names
    /// a type of values all the same.
    pub values: ValueType,
}

/// The families of BTrees: the two letters of each module.
const FAMILIES: [&str; 21] = [
    "OO", "OI", "OU", "OL", "OQ", "IO", "II", "IU", "IF", "UO", "UI", "UU", "UF", "LO", "LL", "LQ",
    "LF", "QO", "QL", "QQ", "QF",
];

/// The kind of a class of a BTrees family.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    /// `XYBucket`: a mapping of one bucket.
    Bucket,
    /// `XYSet`: a set of one bucket.
    Set,
    /// `XYBTree`: a mapping.
    Tree,
    /// `XYTreeSet`: a set.
    TreeSet,
}

impl Shape {
    /// Tells if the shape holds values: a bucket or a BTree.
    pub fn is_mapping(self) -> bool {
        matches!(self, Shape::Bucket | Shape::Tree)
    }
}

/// A class of BTrees whose conflicts BTrees resolves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    /// A bucket, a set, a BTree or a tree set of a family.
    Family(Shape, Family),
    /// `BTrees.Length.Length`.
    Length,
}

/// Returns the class of BTrees of `module` and `name`, if it is one. The
/// fs family, whose keys and values are bytes of a fixed size, is not one.
pub fn classify(module: &str, name: &str) -> Option<Class> {
    if (module, name) == ("BTrees.Length", "Length") {
        return Some(Class::Length);
    }
    let letters = module.strip_prefix("BTrees.")?.strip_suffix("BTree")?;
    if !FAMILIES.contains(&letters) {
        return None;
    }
    let shape = match name.strip_prefix(letters)? {
        "Bucket" => Shape::Bucket,
        "Set" => Shape::Set,
        "BTree" => Shape::Tree,
        "TreeSet" => Shape::TreeSet,
        _ => return None,
    };
    let bytes = letters.as_bytes();
    let keys = match bytes[0] {
        b'O' => KeyType::Object,
        b'I' => KeyType::Int32,
        b'U' => KeyType::Uint32,
        b'L' => KeyType::Int64,
        _ => KeyType::Uint64,
    };
    let values = match bytes[1] {
        b'O' => ValueType::Object,
        b'I' => ValueType::Int32,
        b'U' => ValueType::Uint32,
        b'L' => ValueType::Int64,
        b'Q' => ValueType::Uint64,
        _ => ValueType::Float32,
    };
    Some(Class::Family(shape, Family { keys, values }))
}

/// Returns the int of `value` if it is an int in `range`. A bool is not
/// one: the model keeps it apart.
fn int_in(value: Value, low: i128, high: i128) -> Option<Value> {
    match value {
        Value::Int(int) if (low..=high).contains(&int) => Some(value),
        _ => None,
    }
}

impl KeyType {
    /// Returns the key that a bucket of this type keeps for `value`, as its
    /// `__getstate__` gives it back. `None` when the type does not take it,
    /// or when the model cannot tell.
    pub fn stored(self, value: Value) -> Option<Value> {
        match self {
            KeyType::Object => Some(value),
            KeyType::Int32 => int_in(value, i32::MIN.into(), i32::MAX.into()),
            KeyType::Uint32 => int_in(value, 0, u32::MAX.into()),
            KeyType::Int64 => int_in(value, i64::MIN.into(), i64::MAX.into()),
            KeyType::Uint64 => int_in(value, 0, u64::MAX.into()),
        }
    }
}

impl ValueType {
    /// Returns the value that a bucket of this type keeps for `value`, as
    /// its `__getstate__` gives it back: a float goes through a C `float`.
    /// `None` when the type does not take it, or when the model cannot
    /// tell.
    pub fn stored(self, value: Value) -> Option<Value> {
        match self {
            ValueType::Object => Some(value),
            ValueType::Int32 => int_in(value, i32::MIN.into(), i32::MAX.into()),
            ValueType::Uint32 => int_in(value, 0, u32::MAX.into()),
            ValueType::Int64 => int_in(value, i64::MIN.into(), i64::MAX.into()),
            ValueType::Uint64 => int_in(value, 0, u64::MAX.into()),
            ValueType::Float32 => match value {
                Value::Float(float) => Some(Value::Float(f64::from(float as f32))),
                _ => None,
            },
        }
    }
}

/// The state of a bucket or a set.
#[derive(Clone, Debug, PartialEq)]
pub struct BucketState {
    /// For a bucket, each key and then its value. For a set, the keys.
    pub items: Vec<Value>,
    /// The next bucket in its BTree, if the state names one.
    pub next: Option<Value>,
}

/// Reads the state of a bucket or a set: `(items,)` or `(items, next)`.
/// `None` for another shape. A bucket whose items are odd in number is
/// another shape: its `__setstate__` would drop the last item.
pub fn bucket_state(heap: &Heap, state: Value, mapping: bool) -> Option<BucketState> {
    let (items, next) = match *heap.tuple(state)? {
        [items] => (items, None),
        [items, next] => (items, Some(next)),
        _ => return None,
    };
    let items = heap.tuple(items)?.to_vec();
    if mapping && items.len() % 2 != 0 {
        return None;
    }
    Some(BucketState { items, next })
}

/// Returns a new state of a bucket or a set: `(items,)`, or
/// `(items, next)`.
pub fn new_bucket_state(heap: &mut Heap, items: Vec<Value>, next: Option<Value>) -> Value {
    let items = heap.new_tuple(items);
    match next {
        Some(next) => heap.new_tuple(vec![items, next]),
        None => heap.new_tuple(vec![items]),
    }
}

/// The state of a BTree or a tree set, as its conflict resolution reads it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TreeState {
    /// `None`: an empty tree.
    Empty,
    /// `((bucket,),)`: one bucket, inline. The value is the state of the
    /// bucket.
    Inline(Value),
    /// `(children, firstbucket)`: a tree of more than one bucket, or of a
    /// bucket that has its own oid.
    Split,
}

/// Reads the state of a BTree or a tree set, as `get_bucket_state` of
/// `BTreeTemplate.c` does. `None` for another shape.
pub fn tree_state(heap: &Heap, state: Value) -> Option<TreeState> {
    if state == Value::None {
        return Some(TreeState::Empty);
    }
    match *heap.tuple(state)? {
        [_, _] => Some(TreeState::Split),
        [inner] => match *heap.tuple(inner)? {
            [bucket] => {
                heap.tuple(bucket)?;
                Some(TreeState::Inline(bucket))
            }
            _ => None,
        },
        _ => None,
    }
}

/// Returns a new state of a BTree or a tree set that holds one bucket:
/// `((bucket,),)`.
pub fn new_tree_state(heap: &mut Heap, bucket: Value) -> Value {
    let inner = heap.new_tuple(vec![bucket]);
    heap.new_tuple(vec![inner])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode_record;

    #[test]
    fn classify_names_every_family_and_shape() {
        assert_eq!(classify("BTrees.Length", "Length"), Some(Class::Length));
        let io = Family {
            keys: KeyType::Int32,
            values: ValueType::Object,
        };
        assert_eq!(
            classify("BTrees.IOBTree", "IOBucket"),
            Some(Class::Family(Shape::Bucket, io))
        );
        assert_eq!(
            classify("BTrees.IOBTree", "IOBTree"),
            Some(Class::Family(Shape::Tree, io))
        );
        let qf = Family {
            keys: KeyType::Uint64,
            values: ValueType::Float32,
        };
        assert_eq!(
            classify("BTrees.QFBTree", "QFTreeSet"),
            Some(Class::Family(Shape::TreeSet, qf))
        );
        for (module, name) in [
            ("BTrees.fsBTree", "fsBucket"),
            ("BTrees.OOBTree", "IOBucket"),
            ("BTrees.OFBTree", "OFBucket"),
            ("BTrees.OOBTree", "OOBTreeItems"),
            ("persistent.mapping", "PersistentMapping"),
        ] {
            assert_eq!(classify(module, name), None, "{module}.{name}");
        }
    }

    #[test]
    fn stored_values_take_the_c_types() {
        assert_eq!(KeyType::Int32.stored(Value::Int(1 << 31)), None);
        assert_eq!(KeyType::Uint32.stored(Value::Int(-1)), None);
        assert_eq!(KeyType::Int64.stored(Value::Bool(true)), None);
        assert_eq!(
            KeyType::Uint64.stored(Value::Int(u64::MAX.into())),
            Some(Value::Int(u64::MAX.into()))
        );
        assert_eq!(
            ValueType::Float32.stored(Value::Float(0.1)),
            Some(Value::Float(f64::from(0.1f32)))
        );
        assert_eq!(ValueType::Float32.stored(Value::Int(1)), None);
    }

    /// Loads the state pickle `state` of a record of `m.C`.
    fn load(heap: &mut Heap, state: &[u8]) -> Value {
        let record = [&b"\x80\x03cm\nC\nq\x00."[..], state].concat();
        heap.load(&decode_record(&record).unwrap()).unwrap().1
    }

    #[test]
    fn bucket_and_tree_states_read_and_build() {
        let mut heap = Heap::new();
        // ((((1, 2),),),): an IIBTree of one inline bucket.
        let state = load(
            &mut heap,
            b"\x80\x03K\x01K\x02\x86q\x01\x85q\x02\x85q\x03\x85q\x04.",
        );
        let Some(TreeState::Inline(bucket)) = tree_state(&heap, state) else {
            panic!("{state:?}");
        };
        let read = bucket_state(&heap, bucket, true).unwrap();
        assert_eq!(read.items, [Value::Int(1), Value::Int(2)]);
        assert_eq!(read.next, None);
        assert_eq!(bucket_state(&heap, bucket, false).unwrap().items.len(), 2);
        let built = new_bucket_state(&mut heap, read.items, None);
        let tree = new_tree_state(&mut heap, built);
        let mut pickler = crate::python::Pickler::new(&heap);
        pickler.dump(tree).unwrap();
        assert_eq!(
            pickler.into_bytes(),
            b"\x80\x03K\x01K\x02\x86q\x00\x85q\x01\x85q\x02\x85q\x03."
        );
        assert_eq!(tree_state(&heap, Value::None), Some(TreeState::Empty));
        // A tree of two buckets: (children, firstbucket).
        let split = load(&mut heap, b"\x80\x03NN\x86q\x01.");
        assert_eq!(tree_state(&heap, split), Some(TreeState::Split));
        // Odd items are another shape for a bucket.
        let odd = load(&mut heap, b"\x80\x03K\x01\x85q\x01\x85q\x02.");
        assert_eq!(bucket_state(&heap, odd, true), None);
        assert!(bucket_state(&heap, odd, false).is_some());
    }
}
