//! The objects that Python's unpickler builds from a record, with their
//! identity, and a pickler that writes them as zodbpickle does with
//! protocol 3.
//!
//! A merge of states, as the conflict resolution of BTrees does it, works
//! on these objects. ZODB's resolver unpickles three states, merges them
//! and pickles the result with one `Pickler` for the class and the state.
//! The bytes of that pickle depend on the identity of the objects: the
//! pickler writes an object that it saw before as a memo get. So a
//! [`Heap`] keeps the identity of each object as Python's unpickler gives
//! it:
//!
//! - Each opcode that makes a str, bytes, tuple, list or dict makes a new
//!   object. A memo get gives the object of its put.
//! - CPython shares some objects: the empty str, each str of one character
//!   below U+0100, the empty bytes and each bytes of one byte. A heap
//!   shares them too, across all the pickles that it loads.
//! - A class is one object for each module and name, as the `find_global`
//!   of ZODB's resolver caches it.
//! - A persistent reference is one object for each persistent id, as the
//!   `PersistentReferenceFactory` of ZODB's resolver keeps it across the
//!   states of one resolution.
//!
//! The model refuses what it does not model, with [`Unsupported`]: an
//! opaque node, a REDUCE, a Python 2 str, a protocol 0 unicode or list, an
//! int that does not fit 128 bits, a dict key that is not an int, a str or
//! bytes ([`DictKey`]), a persistent id of an unknown shape, a container
//! that holds itself. A caller then leaves the work to Python.
//!
//! A heap of [`Heap::with_instances`] also models the objects of other
//! classes: an object that NEWOBJ or REDUCE makes, with the state that
//! BUILD gives it, a `set` or a `frozenset`, and a dict key that is a tuple
//! of ints, strs and bytes. The merges of fields use such a heap. The
//! merges of BTrees do not: for them, these values stay constructs that
//! the model refuses.
//!
//! A heap of [`Heap::with_values`] models three objects of other classes
//! that the metadata of a catalog holds, and no other: a `DateTime` of
//! `DateTime.DateTime`, a `Message` of `zope.i18nmessageid.message`, and
//! `Missing.V`, the value `Missing.Value`. It keeps each of them as Python
//! does after the unpickler made it: the state that `DateTime.__setstate__`
//! leaves, the arguments that `Message.__reduce__` gives again. It compares
//! them as their classes do. The merges of BTrees use such a heap.
//!
//! No function here calls itself once per nesting level. The loader, the
//! comparisons and the pickler use explicit stacks. The objects live in
//! one vector and refer to each other by index, so a drop does not
//! recurse either.

use std::cmp::Ordering;
use std::collections::HashMap;

use crate::opcode::*;
use crate::tree::{Bool, Float, Int, Node, Tree, Unicode};

/// What a [`Heap`] or a [`Pickler`] does not model. The text names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("not modeled: {0}")]
pub struct Unsupported(pub &'static str);

/// The index of an object in its [`Heap`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjectId(u32);

/// A value of a pickle. An atom holds its value. Other values are objects
/// of a [`Heap`], with an identity.
///
/// The pickler never memoizes an atom, so an atom needs no identity.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Value {
    /// `None`.
    None,
    /// A bool.
    Bool(bool),
    /// An int.
    Int(i128),
    /// A float.
    Float(f64),
    /// An object of the heap.
    Object(ObjectId),
}

/// An object of a [`Heap`].
#[derive(Clone, Debug, PartialEq)]
pub enum Object {
    /// A `str`.
    Str(String),
    /// A `bytes`.
    Bytes(Vec<u8>),
    /// A tuple.
    Tuple(Vec<Value>),
    /// A list.
    List(Vec<Value>),
    /// A dict: the items in the order of insertion, each key once.
    Dict(Vec<(Value, Value)>),
    /// A class, from a GLOBAL.
    Class {
        /// The module, for example `BTrees.OOBTree`.
        module: String,
        /// The name in the module, for example `OOBucket`.
        name: String,
    },
    /// A persistent reference: the `PersistentReference` of ZODB's
    /// resolver.
    Reference(Reference),
    /// An object of another class, that NEWOBJ or REDUCE makes. Only a
    /// heap of [`Heap::with_instances`] holds one.
    Instance(Instance),
    /// A `set` or a `frozenset`: REDUCE of `builtins.set` or
    /// `builtins.frozenset` with the list of its items. Only a heap of
    /// [`Heap::with_instances`] holds one.
    Set {
        /// The class: `builtins.set` or `builtins.frozenset`.
        class: Value,
        /// The items, in the order of the pickle.
        items: Vec<Value>,
    },
}

/// An object of another class, as the unpickler makes it.
#[derive(Clone, Debug, PartialEq)]
pub struct Instance {
    /// The opcode that made the object: NEWOBJ or REDUCE.
    pub made: Made,
    /// The class of NEWOBJ, or the callable of REDUCE.
    pub class: Value,
    /// The tuple of the arguments.
    pub args: Value,
    /// The state that BUILD gives the object, if it comes.
    pub state: Option<Value>,
}

/// The opcode that made an [`Instance`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Made {
    /// NEWOBJ: `cls.__new__(cls, *args)`.
    New,
    /// REDUCE: `callable(*args)`.
    Reduce,
}

/// A persistent reference, as ZODB's resolver reads it.
#[derive(Clone, Debug, PartialEq)]
pub struct Reference {
    /// The persistent id: the value that the pickler writes before
    /// BINPERSID.
    pub data: Value,
    /// The oid of the object that it refers to.
    pub oid: Vec<u8>,
    /// The name of the database of the object, for a reference into
    /// another database.
    pub database: Option<String>,
    /// True for a weak reference.
    pub weak: bool,
}

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

/// The identity of a persistent id: the key of the
/// `PersistentReferenceFactory` of ZODB's resolver, `tuple(ref)`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum ReferenceKey {
    /// An oid alone.
    Oid(Vec<u8>),
    /// `(oid, class)`.
    WithClass(Vec<u8>, ObjectId),
    /// `['w', (oid,)]` or `['w', (oid, database)]`.
    Weak(Vec<u8>, Option<String>),
    /// `['m', (database, oid, class)]`.
    Multi(String, Vec<u8>, ObjectId),
    /// `['n', (database, oid)]`.
    MultiOid(String, Vec<u8>),
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

/// The class of a `DateTime`, as its GLOBAL names it.
const DATE_TIME: (&str, &str) = ("DateTime.DateTime", "DateTime");
/// The class of a `Message`, as its GLOBAL names it.
const MESSAGE: (&str, &str) = ("zope.i18nmessageid.message", "Message");
/// The module of `Missing.Value`. Its GLOBAL is `Missing.V`.
const MISSING: &str = "Missing";
/// The first and the last instant of a `DateTime` that the model takes, in
/// microseconds since the epoch: 1900-01-01 and the end of 2999. In this
/// range `DateTime.__setstate__` gives a state for each zone of the table.
const DATE_TIME_MICROS: std::ops::RangeInclusive<i128> =
    -2_208_988_800_000_000..=32_503_679_999_999_999;
/// The most arguments that `Message` takes.
const MESSAGE_ARGS: usize = 7;

/// An object of a class that a heap of [`Heap::with_values`] models.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Known {
    /// A `DateTime` that NEWOBJ made, before BUILD gives it its state.
    NewDateTime,
    /// A `DateTime`: `_micros`, and the name of `_tz`.
    DateTime(i128, &'static str),
    /// A `Message`.
    Message,
}

/// What a comparison sees of a value of a heap of [`Heap::with_values`].
#[derive(Clone, Copy, Debug)]
enum Kind<'a> {
    /// A `DateTime`: `_micros` and `_tz`.
    DateTime(i128, &'a str),
    /// A `Message`: its text. It compares as a str.
    Message(&'a str),
    /// `Missing.Value`.
    Missing,
}

/// The objects of the pickles that a merge reads, and of the state that it
/// builds.
#[derive(Clone, Debug, Default)]
pub struct Heap {
    objects: Vec<Object>,
    /// The shared str of one character below U+0100, by character.
    chars: HashMap<char, ObjectId>,
    /// The shared empty str.
    empty_str: Option<ObjectId>,
    /// The shared bytes of one byte, by byte.
    bytes: HashMap<u8, ObjectId>,
    /// The shared empty bytes.
    empty_bytes: Option<ObjectId>,
    /// The classes, by module and name.
    classes: HashMap<(String, String), ObjectId>,
    /// The persistent references, by their identity.
    references: HashMap<ReferenceKey, ObjectId>,
    /// True when the heap models the objects of other classes and the
    /// sets.
    instances: bool,
    /// True when the heap models `DateTime`, `Message` and `Missing.V`,
    /// and no other object of another class.
    values: bool,
    /// The objects of these classes that the heap holds, by object.
    known: HashMap<ObjectId, Known>,
    /// The str of each name of a time zone that a `DateTime` holds: one
    /// object for each name, as `_TZINFO._zmap` of `DateTime` keeps it.
    zones: HashMap<&'static str, ObjectId>,
    /// The interned strs, by text: the keys of the dict state of an object,
    /// as BUILD and `Persistent.__setstate__` intern them in CPython.
    interned: HashMap<String, ObjectId>,
    /// The most steps that the comparisons of the heap take in all, or
    /// `None` for no limit.
    limit: Option<usize>,
    /// The steps that the comparisons took.
    spent: std::cell::Cell<usize>,
}

impl Heap {
    /// Returns an empty heap.
    pub fn new() -> Self {
        Self::default()
    }

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

    /// Returns an empty heap that also models the objects of other classes
    /// and the sets, as an [`Object::Instance`] and an [`Object::Set`].
    pub fn with_instances() -> Self {
        Heap {
            instances: true,
            ..Self::default()
        }
    }

    /// Returns an empty heap that also models a `DateTime`, a `Message`
    /// and `Missing.Value`. It refuses each other object of another class
    /// and each set, as a heap of [`Heap::new`] does.
    pub fn with_values() -> Self {
        Heap {
            values: true,
            ..Self::default()
        }
    }

    /// Loads a record: its class pickle, then its state pickle, with one
    /// memo, as ZODB's unpickler reads them. Returns the class value and the
    /// state value.
    ///
    /// The persistent references of the record join those of the records
    /// that the heap loaded before. ZODB's resolver loads the new state
    /// first, then the old one, then the committed one, so a caller loads
    /// them in that order: a reference takes its persistent id from the
    /// first record that holds it.
    pub fn load(&mut self, tree: &Tree) -> Result<(Value, Value), Unsupported> {
        let mut loader = Loader {
            heap: self,
            memo: HashMap::new(),
            open: Vec::new(),
        };
        let class = loader.load(tree.class())?;
        let state = loader.load(tree.state())?;
        Ok((class, state))
    }

    /// Returns the object of `id`.
    pub fn get(&self, id: ObjectId) -> &Object {
        &self.objects[id.0 as usize]
    }

    /// Returns the object of `value`, or `None` for an atom.
    pub fn object(&self, value: Value) -> Option<&Object> {
        match value {
            Value::Object(id) => Some(self.get(id)),
            _ => None,
        }
    }

    /// Returns the items of `value` if it is a tuple.
    pub fn tuple(&self, value: Value) -> Option<&[Value]> {
        match self.object(value)? {
            Object::Tuple(items) => Some(items),
            _ => None,
        }
    }

    /// Returns the items of `value` if it is a dict.
    pub fn dict(&self, value: Value) -> Option<&[(Value, Value)]> {
        match self.object(value)? {
            Object::Dict(items) => Some(items),
            _ => None,
        }
    }

    /// Returns the reference of `value` if it is a persistent reference.
    pub fn reference(&self, value: Value) -> Option<&Reference> {
        match self.object(value)? {
            Object::Reference(reference) => Some(reference),
            _ => None,
        }
    }

    /// Returns the module and the name of `value` if it is a class.
    pub fn class(&self, value: Value) -> Option<(&str, &str)> {
        match self.object(value)? {
            Object::Class { module, name } => Some((module, name)),
            _ => None,
        }
    }

    /// Returns the text of `value` if it is a str.
    pub fn str(&self, value: Value) -> Option<&str> {
        match self.object(value)? {
            Object::Str(text) => Some(text),
            _ => None,
        }
    }

    /// Returns the object of `value` if it is an object of another class.
    pub fn instance(&self, value: Value) -> Option<&Instance> {
        match self.object(value)? {
            Object::Instance(instance) => Some(instance),
            _ => None,
        }
    }

    /// Returns a str of `text`, shared as CPython shares it.
    pub fn new_str(&mut self, text: &str) -> Value {
        self.str_object(text)
    }

    /// Returns a bytes of `data`, shared as CPython shares it.
    pub fn new_bytes(&mut self, data: &[u8]) -> Value {
        self.bytes_object(data)
    }

    /// Returns a new list of `items`.
    pub fn new_list(&mut self, items: Vec<Value>) -> Value {
        Value::Object(self.push(Object::List(items)))
    }

    /// Returns a new object of another class.
    pub fn new_instance(&mut self, instance: Instance) -> Value {
        Value::Object(self.push(Object::Instance(instance)))
    }

    /// Returns a new set of `items` with the class `class`, which is
    /// `builtins.set` or `builtins.frozenset`.
    pub fn new_set(&mut self, class: Value, items: Vec<Value>) -> Value {
        Value::Object(self.push(Object::Set { class, items }))
    }

    /// Interns the str keys of the dict `dict`, as CPython interns the keys
    /// of the state of an object: BUILD does it for an object without
    /// `__setstate__`, and `Persistent.__setstate__` for a persistent
    /// object. The first str of a text in the heap is the interned one; a
    /// later key of that text becomes that str.
    pub fn intern_keys(&mut self, dict: Value) {
        let Some(Object::Dict(items)) = self.object(dict) else {
            return;
        };
        let mut items = items.clone();
        for (key, _) in &mut items {
            let Value::Object(id) = *key else {
                continue;
            };
            let Object::Str(text) = self.get(id) else {
                continue;
            };
            match self.interned.get(text) {
                Some(&interned) => *key = Value::Object(interned),
                None => {
                    self.interned.insert(text.clone(), id);
                }
            }
        }
        let Value::Object(id) = dict else {
            return;
        };
        self.objects[id.0 as usize] = Object::Dict(items);
    }

    /// Returns a new tuple of `items`.
    pub fn new_tuple(&mut self, items: Vec<Value>) -> Value {
        Value::Object(self.push(Object::Tuple(items)))
    }

    /// Returns a new dict of `items`. Each key must be there once.
    pub fn new_dict(&mut self, items: Vec<(Value, Value)>) -> Value {
        Value::Object(self.push(Object::Dict(items)))
    }

    /// Returns the class of `module` and `name`: the same object at each
    /// call.
    pub fn new_class(&mut self, module: &str, name: &str) -> Value {
        let key = (module.to_owned(), name.to_owned());
        if let Some(&id) = self.classes.get(&key) {
            return Value::Object(id);
        }
        let id = self.push(Object::Class {
            module: key.0.clone(),
            name: key.1.clone(),
        });
        self.classes.insert(key, id);
        Value::Object(id)
    }

    fn push(&mut self, object: Object) -> ObjectId {
        let id = ObjectId(u32::try_from(self.objects.len()).expect("fewer than 2^32 objects"));
        self.objects.push(object);
        id
    }

    /// Returns a str object of `text`, shared when CPython shares it.
    fn str_object(&mut self, text: &str) -> Value {
        let mut chars = text.chars();
        let shared = match (chars.next(), chars.next()) {
            (None, _) => Some(&mut self.empty_str),
            (Some(c), None) if (c as u32) < 0x100 => {
                if let Some(&id) = self.chars.get(&c) {
                    return Value::Object(id);
                }
                let id = self.push(Object::Str(text.to_owned()));
                self.chars.insert(c, id);
                return Value::Object(id);
            }
            _ => None,
        };
        match shared {
            Some(Some(id)) => Value::Object(*id),
            Some(None) => {
                let id = self.push(Object::Str(String::new()));
                self.empty_str = Some(id);
                Value::Object(id)
            }
            None => Value::Object(self.push(Object::Str(text.to_owned()))),
        }
    }

    /// Returns a bytes object of `data`, shared when the unpickler shares
    /// it.
    fn bytes_object(&mut self, data: &[u8]) -> Value {
        match data {
            [] => {
                if let Some(id) = self.empty_bytes {
                    return Value::Object(id);
                }
                let id = self.push(Object::Bytes(Vec::new()));
                self.empty_bytes = Some(id);
                Value::Object(id)
            }
            [byte] => {
                if let Some(&id) = self.bytes.get(byte) {
                    return Value::Object(id);
                }
                let id = self.push(Object::Bytes(vec![*byte]));
                self.bytes.insert(*byte, id);
                Value::Object(id)
            }
            _ => Value::Object(self.push(Object::Bytes(data.to_vec()))),
        }
    }

    /// Returns the persistent reference of the persistent id `data`, as
    /// BINPERSID of the unpickler gives it: the reference that the heap has
    /// for its identity, or a new one. `data` takes the forms of ZODB's
    /// `serialize.py`, such as the tuple `(oid, class)`. Only the feature
    /// `testing` builds it: tests use it to build records.
    #[cfg(feature = "testing")]
    pub fn new_reference(&mut self, data: Value) -> Result<Value, Unsupported> {
        self.reference_of(data)
    }

    /// Returns the persistent reference of the persistent id `data`: the
    /// reference that the heap has for its identity, or a new one.
    fn reference_of(&mut self, data: Value) -> Result<Value, Unsupported> {
        let (key, reference) = self.read_reference(data)?;
        if let Some(&id) = self.references.get(&key) {
            return Ok(Value::Object(id));
        }
        let id = self.push(Object::Reference(reference));
        self.references.insert(key, id);
        Ok(Value::Object(id))
    }

    /// Reads a persistent id in one of the forms of ZODB's `serialize.py`.
    fn read_reference(&self, data: Value) -> Result<(ReferenceKey, Reference), Unsupported> {
        let unknown = Unsupported("a persistent id of an unknown shape");
        let bytes = |value: Value| match self.object(value) {
            Some(Object::Bytes(bytes)) => Some(bytes.clone()),
            _ => None,
        };
        let text = |value: Value| match self.object(value) {
            Some(Object::Str(text)) => Some(text.clone()),
            _ => None,
        };
        let class = |value: Value| match (value, self.object(value)) {
            (Value::Object(id), Some(Object::Class { .. })) => Some(id),
            _ => None,
        };
        let reference = |oid: &[u8], database: Option<String>, weak: bool| Reference {
            data,
            oid: oid.to_vec(),
            database,
            weak,
        };
        match self.object(data).ok_or(unknown)? {
            Object::Bytes(oid) => Ok((ReferenceKey::Oid(oid.clone()), reference(oid, None, false))),
            Object::Tuple(items) => match items[..] {
                [oid, klass] => {
                    let (oid, klass) = bytes(oid).zip(class(klass)).ok_or(unknown)?;
                    let made = reference(&oid, None, false);
                    Ok((ReferenceKey::WithClass(oid, klass), made))
                }
                _ => Err(unknown),
            },
            Object::List(items) => {
                let [kind, args] = items[..] else {
                    return Err(unknown);
                };
                let args = self.tuple(args).ok_or(unknown)?;
                match (text(kind).as_deref(), args) {
                    (Some("w"), &[oid]) => {
                        let oid = bytes(oid).ok_or(unknown)?;
                        let made = reference(&oid, None, true);
                        Ok((ReferenceKey::Weak(oid, None), made))
                    }
                    (Some("w"), &[oid, database]) => {
                        let (oid, database) = bytes(oid).zip(text(database)).ok_or(unknown)?;
                        let made = reference(&oid, Some(database.clone()), true);
                        Ok((ReferenceKey::Weak(oid, Some(database)), made))
                    }
                    (Some("m"), &[database, oid, klass]) => {
                        let database = text(database).ok_or(unknown)?;
                        let (oid, klass) = bytes(oid).zip(class(klass)).ok_or(unknown)?;
                        let made = reference(&oid, Some(database.clone()), false);
                        Ok((ReferenceKey::Multi(database, oid, klass), made))
                    }
                    (Some("n"), &[database, oid]) => {
                        let (database, oid) = text(database).zip(bytes(oid)).ok_or(unknown)?;
                        let made = reference(&oid, Some(database.clone()), false);
                        Ok((ReferenceKey::MultiOid(database, oid), made))
                    }
                    _ => Err(unknown),
                }
            }
            _ => Err(unknown),
        }
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
}

impl Heap {
    /// Compares two values for `==` when one of them is a `DateTime`, a
    /// `Message` or `Missing.Value`. `None` when neither is.
    ///
    /// The left value compares first, as in Python. `DateTime.__eq__` and
    /// `Missing.__eq__` give false for an object of another class. A
    /// `Message` compares as a str. A persistent reference raises.
    fn known_eq(&self, a: Value, b: Value) -> Option<Result<bool, Undecided>> {
        let (p, q) = (self.kind(a), self.kind(b));
        if p.is_none() && q.is_none() {
            return None;
        }
        if let (Value::Object(x), Value::Object(y)) = (a, b)
            && x == y
        {
            return Some(Ok(true));
        }
        let reference = |value: Value| self.reference(value).is_some();
        Some(match (p, q) {
            (Some(Kind::DateTime(m, z)), Some(Kind::DateTime(n, w))) => Ok(m == n && z == w),
            (Some(Kind::Message(s)), Some(Kind::Message(t))) => Ok(s == t),
            (Some(Kind::Message(s)), None) => str_eq(s, self.str(b), reference(b)),
            (None, Some(Kind::Message(t))) => str_eq(t, self.str(a), reference(a)),
            (Some(Kind::DateTime(..) | Kind::Missing), _) => Ok(false),
            // A str, a `Message` or a value of a built-in type gives
            // `NotImplemented`, then the right object gives false.
            (_, Some(Kind::DateTime(..) | Kind::Missing)) => {
                if reference(a) {
                    Err(Undecided::Raises)
                } else {
                    Ok(false)
                }
            }
            (None, None) => unreachable!("one value is known"),
        })
    }

    /// Compares two values for `<` when one of them is a `DateTime`, a
    /// `Message` or `Missing.Value`. `None` when neither is.
    ///
    /// `Missing.__lt__` gives false, and `Missing.__gt__` gives true for
    /// another object: a value of a built-in type is below `Missing.Value`.
    /// `DateTime.lessThan` compares `_micros`, and compares that int with
    /// an object without `_micros`, so a str or a tuple raises there.
    /// `None` is below a `DateTime`. A `Message` compares as a str.
    fn known_lt(&self, a: Value, b: Value) -> Option<Result<bool, Undecided>> {
        let (p, q) = (self.kind(a), self.kind(b));
        if p.is_none() && q.is_none() {
            return None;
        }
        let reference = |value: Value| self.reference(value).is_some();
        Some(match (p, q) {
            (Some(Kind::Missing), _) => Ok(false),
            (Some(Kind::DateTime(m, _)), Some(Kind::DateTime(n, _))) => Ok(m < n),
            (Some(Kind::DateTime(..) | Kind::Message(_)), Some(Kind::Missing)) => Ok(true),
            (Some(Kind::DateTime(..)), Some(Kind::Message(_)))
            | (Some(Kind::Message(_)), Some(Kind::DateTime(..))) => Err(Undecided::Raises),
            (Some(Kind::Message(s)), Some(Kind::Message(t))) => Ok(s < t),
            (Some(Kind::Message(s)), None) => match self.str(b) {
                Some(t) => Ok(s < t),
                None if reference(b) => Err(Undecided::Raises),
                None => Err(Undecided::Unknown),
            },
            (None, Some(Kind::Message(t))) => match self.str(a) {
                Some(s) => Ok(s < t),
                None if reference(a) => Err(Undecided::Raises),
                None => Err(Undecided::Unknown),
            },
            (Some(Kind::DateTime(..)), None) => match self.builtin(b) {
                Some(Builtin::None) => Ok(false),
                Some(Builtin::Number) => Err(Undecided::Unknown),
                Some(Builtin::Other | Builtin::Reference) => Err(Undecided::Raises),
                None => Err(Undecided::Unknown),
            },
            (None, Some(Kind::DateTime(..))) => match self.builtin(a) {
                Some(Builtin::None) => Ok(true),
                Some(Builtin::Number) => Err(Undecided::Unknown),
                Some(Builtin::Other | Builtin::Reference) => Err(Undecided::Raises),
                None => Err(Undecided::Unknown),
            },
            (None, Some(Kind::Missing)) => match self.builtin(a) {
                Some(Builtin::None | Builtin::Number | Builtin::Other) => Ok(true),
                Some(Builtin::Reference) => Err(Undecided::Raises),
                None => Err(Undecided::Unknown),
            },
            (None, None) => unreachable!("one value is known"),
        })
    }

    /// Returns the kind of a value that is not a `DateTime`, a `Message`
    /// or `Missing.Value`, for the comparisons with them. `None` for a
    /// class or an object of another class.
    fn builtin(&self, value: Value) -> Option<Builtin> {
        match value {
            Value::None => Some(Builtin::None),
            Value::Bool(_) | Value::Int(_) | Value::Float(_) => Some(Builtin::Number),
            Value::Object(id) => match self.get(id) {
                Object::Str(_)
                | Object::Bytes(_)
                | Object::Tuple(_)
                | Object::List(_)
                | Object::Dict(_) => Some(Builtin::Other),
                Object::Reference(_) => Some(Builtin::Reference),
                Object::Class { .. } | Object::Instance(_) | Object::Set { .. } => None,
            },
        }
    }
}

/// The kinds of values of [`Heap::builtin`].
#[derive(Clone, Copy)]
enum Builtin {
    None,
    /// A bool, an int or a float.
    Number,
    /// A str, a bytes, a tuple, a list or a dict.
    Other,
    /// A persistent reference.
    Reference,
}

/// `s == other` for the text `s` of a `Message`: `str.__eq__`. `text` is the
/// text of `other` if it is a str. A persistent reference raises in its
/// `__eq__`. Another object is not equal.
fn str_eq(s: &str, text: Option<&str>, reference: bool) -> Result<bool, Undecided> {
    match text {
        Some(t) => Ok(s == t),
        None if reference => Err(Undecided::Raises),
        None => Ok(false),
    }
}

/// The structure of one value, as [`Canon`] keys it: an atom by its value,
/// an object by its kind and the canonical ids of its parts. A set keeps
/// the ids of its items in ascending order, so two sets with the same items
/// in another order have one shape. The parts are ids, so a shape holds no
/// other shape, and its hash, its comparison and its drop never recurse.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Shape {
    None,
    Bool(bool),
    Int(i128),
    Float(u64),
    Str(String),
    Bytes(Vec<u8>),
    Tuple(Vec<u32>),
    List(Vec<u32>),
    Dict(Vec<(u32, u32)>),
    Class(String, String),
    Reference(Vec<u8>, Option<String>, bool),
    Instance(bool, u32, u32, Option<u32>),
    Set(u32, Vec<u32>),
}

/// The canonical ids of the values of one heap: two values have one id
/// when they have the same structure: the same kind, the same atoms and
/// texts, the same items in the same order (a set in any order), the same
/// class, arguments and state for an object of another class, and the
/// same oid, database and weakness for a persistent reference. This is not
/// the `==` of Python: two dicts in another order differ here.
///
/// Each object of the heap costs one step, and its items one step each,
/// once, whatever the number of values that share it: the ids are kept
/// for the life of the `Canon`. Past its budget of steps, [`Canon::id`]
/// gives [`Undecided::Budget`]. The walk uses an explicit stack.
#[derive(Debug)]
pub struct Canon {
    ids: HashMap<ObjectId, u32>,
    shapes: HashMap<Shape, u32>,
    left: usize,
}

impl Canon {
    /// Returns a `Canon` with a budget of `steps` steps.
    pub fn new(steps: usize) -> Self {
        Canon {
            ids: HashMap::new(),
            shapes: HashMap::new(),
            left: steps,
        }
    }

    /// Tells if `a` and `b` have the same structure.
    pub fn same(&mut self, heap: &Heap, a: Value, b: Value) -> Result<bool, Undecided> {
        Ok(self.id(heap, a)? == self.id(heap, b)?)
    }

    /// Returns the canonical id of `value`.
    pub fn id(&mut self, heap: &Heap, value: Value) -> Result<u32, Undecided> {
        let Value::Object(root) = value else {
            return self.part(value);
        };
        // Each entry is an object and true once its parts are on the stack.
        let mut stack = vec![(root, false)];
        while let Some(&(id, expanded)) = stack.last() {
            if self.ids.contains_key(&id) {
                stack.pop();
                continue;
            }
            if !expanded {
                stack.last_mut().expect("an entry").1 = true;
                let parts = parts(heap, id);
                self.spend(1 + parts.len())?;
                for part in parts {
                    if let Value::Object(part) = part
                        && !self.ids.contains_key(&part)
                    {
                        stack.push((part, false));
                    }
                }
                continue;
            }
            stack.pop();
            let shape = self.shape(heap, id)?;
            let canonical = self.intern(shape);
            self.ids.insert(id, canonical);
        }
        Ok(self.ids[&root])
    }

    fn spend(&mut self, steps: usize) -> Result<(), Undecided> {
        self.left = self.left.checked_sub(steps).ok_or(Undecided::Budget)?;
        Ok(())
    }

    fn intern(&mut self, shape: Shape) -> u32 {
        let next = u32::try_from(self.shapes.len()).expect("fewer than 2^32 shapes");
        *self.shapes.entry(shape).or_insert(next)
    }

    /// Returns the id of an atom, or of an object whose id the walk gave.
    fn part(&mut self, value: Value) -> Result<u32, Undecided> {
        Ok(match value {
            Value::None => self.intern(Shape::None),
            Value::Bool(b) => self.intern(Shape::Bool(b)),
            Value::Int(int) => self.intern(Shape::Int(int)),
            Value::Float(float) => self.intern(Shape::Float(float.to_bits())),
            // An object that holds itself has no id yet: the model never
            // builds one, and it gets no answer.
            Value::Object(id) => *self.ids.get(&id).ok_or(Undecided::Unknown)?,
        })
    }

    /// Returns the shape of an object whose parts have their ids.
    fn shape(&mut self, heap: &Heap, id: ObjectId) -> Result<Shape, Undecided> {
        let ids = |canon: &mut Canon, values: &[Value]| -> Result<Vec<u32>, Undecided> {
            values.iter().map(|&value| canon.part(value)).collect()
        };
        Ok(match heap.get(id) {
            Object::Str(text) => Shape::Str(text.clone()),
            Object::Bytes(bytes) => Shape::Bytes(bytes.clone()),
            Object::Tuple(items) => Shape::Tuple(ids(self, items)?),
            Object::List(items) => Shape::List(ids(self, items)?),
            Object::Dict(items) => Shape::Dict(
                items
                    .iter()
                    .map(|&(key, value)| Ok((self.part(key)?, self.part(value)?)))
                    .collect::<Result<_, Undecided>>()?,
            ),
            Object::Class { module, name } => Shape::Class(module.clone(), name.clone()),
            Object::Reference(reference) => Shape::Reference(
                reference.oid.clone(),
                reference.database.clone(),
                reference.weak,
            ),
            Object::Instance(instance) => Shape::Instance(
                instance.made == Made::New,
                self.part(instance.class)?,
                self.part(instance.args)?,
                instance.state.map(|state| self.part(state)).transpose()?,
            ),
            Object::Set { class, items } => {
                let mut items = ids(self, items)?;
                items.sort_unstable();
                Shape::Set(self.part(*class)?, items)
            }
        })
    }
}

/// Returns the parts of an object that its shape names.
fn parts(heap: &Heap, id: ObjectId) -> Vec<Value> {
    match heap.get(id) {
        Object::Tuple(items) | Object::List(items) => items.clone(),
        Object::Dict(items) => items
            .iter()
            .flat_map(|&(key, value)| [key, value])
            .collect(),
        Object::Instance(instance) => {
            let mut parts = vec![instance.class, instance.args];
            parts.extend(instance.state);
            parts
        }
        Object::Set { class, items } => {
            let mut parts = vec![*class];
            parts.extend(items);
            parts
        }
        Object::Str(_) | Object::Bytes(_) | Object::Class { .. } | Object::Reference(_) => {
            Vec::new()
        }
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

/// Builds the objects of one record into a heap.
struct Loader<'h> {
    heap: &'h mut Heap,
    /// The memo of the unpickler.
    memo: HashMap<u32, Value>,
    /// The lists and dicts whose items the loader still reads. A memo get
    /// of one of them makes a container that holds itself.
    open: Vec<ObjectId>,
}

/// One step of the loader.
enum Task<'a> {
    /// Load a node and push its value.
    Enter(&'a Node),
    /// Make a tuple of the last values.
    Tuple(usize),
    /// Put the last values into a list.
    List(ObjectId, usize),
    /// Put the last pairs of values into a dict.
    Dict(ObjectId, usize),
    /// Keep the last value in the memo under these indices.
    Remember(Vec<u32>),
    /// Make the persistent reference of the last value.
    Persistent,
    /// Make a set of the last two values: the class and the tuple of the
    /// arguments of REDUCE.
    Set,
}

impl Loader<'_> {
    fn load(&mut self, root: &Node) -> Result<Value, Unsupported> {
        let mut tasks = vec![Task::Enter(root)];
        let mut values: Vec<Value> = Vec::new();
        while let Some(task) = tasks.pop() {
            match task {
                Task::Enter(node) => self.enter(node, &mut tasks, &mut values)?,
                Task::Tuple(len) => {
                    let items = values.split_off(values.len() - len);
                    values.push(self.heap.new_tuple(items));
                }
                Task::List(id, len) => {
                    let items = values.split_off(values.len() - len);
                    self.heap.objects[id.0 as usize] = Object::List(items);
                    self.close(id);
                    values.push(Value::Object(id));
                }
                Task::Dict(id, pairs) => {
                    let flat = values.split_off(values.len() - 2 * pairs);
                    let items = self.dict_items(&flat)?;
                    self.heap.objects[id.0 as usize] = Object::Dict(items);
                    self.close(id);
                    values.push(Value::Object(id));
                }
                Task::Remember(indices) => {
                    let value = *values.last().expect("a value to remember");
                    for index in indices {
                        self.memo.insert(index, value);
                    }
                }
                Task::Persistent => {
                    let data = values.pop().expect("a persistent id");
                    values.push(self.heap.reference_of(data)?);
                }
                Task::Set => {
                    let args = values.pop().expect("the arguments of REDUCE");
                    let class = values.pop().expect("the callable of REDUCE");
                    values.push(self.heap.reduce(class, args)?);
                }
            }
        }
        match values[..] {
            [value] => Ok(value),
            _ => Err(Unsupported("a pickle that leaves other than one value")),
        }
    }

    fn enter<'a>(
        &mut self,
        node: &'a Node,
        tasks: &mut Vec<Task<'a>>,
        values: &mut Vec<Value>,
    ) -> Result<(), Unsupported> {
        let value = match node {
            Node::None => Value::None,
            Node::Bool(Bool::New(value)) => Value::Bool(*value),
            Node::Bool(text @ Bool::Text(_)) => Value::Bool(text.value()),
            Node::Int(int) => Value::Int(int_value(int)?),
            Node::Float(Float::Binary(value)) => Value::Float(*value),
            Node::Float(Float::Text(text)) => Value::Float(
                text.trim()
                    .parse()
                    .map_err(|_| Unsupported("a FLOAT text"))?,
            ),
            Node::Bytes(bytes) => self.heap.bytes_object(&bytes.data),
            Node::Unicode(Unicode::Binary(text)) => self.heap.str_object(text),
            Node::Global(global) => self.heap.global(&global.module, &global.name)?,
            Node::Tuple(tuple) => {
                tasks.push(Task::Tuple(tuple.items.len()));
                tasks.extend(tuple.items.iter().rev().map(Task::Enter));
                return Ok(());
            }
            Node::List(_) | Node::Dict(_) => {
                self.container(node, Vec::new(), tasks)?;
                return Ok(());
            }
            Node::Persistent(pid) => {
                tasks.push(Task::Persistent);
                tasks.push(Task::Enter(pid));
                return Ok(());
            }
            Node::Put(put) => {
                let mut indices = vec![put.index];
                let mut inner: &Node = &put.value;
                while let Node::Put(put) = inner {
                    indices.push(put.index);
                    inner = &put.value;
                }
                if matches!(inner, Node::List(_) | Node::Dict(_)) {
                    // The put comes before the items of a list or a dict.
                    self.container(inner, indices, tasks)?;
                } else {
                    tasks.push(Task::Remember(indices));
                    tasks.push(Task::Enter(inner));
                }
                return Ok(());
            }
            Node::Get(get) => {
                let value = *self
                    .memo
                    .get(&get.index)
                    .ok_or(Unsupported("a memo get without its put"))?;
                if let Value::Object(id) = value
                    && self.open.contains(&id)
                {
                    return Err(Unsupported("a container that holds itself"));
                }
                value
            }
            Node::ByteString(_) => return Err(Unsupported("a Python 2 str")),
            Node::Unicode(Unicode::Text(_)) => return Err(Unsupported("a protocol 0 unicode")),
            Node::Reduce(reduce) if self.heap.instances || self.heap.values => {
                tasks.push(Task::Set);
                tasks.push(Task::Enter(&reduce.args));
                tasks.push(Task::Enter(&reduce.callable));
                return Ok(());
            }
            Node::Opaque(opaque) if self.heap.instances || self.heap.values => {
                self.opaque(&opaque.bytes)?
            }
            Node::Reduce(_) => return Err(Unsupported("a REDUCE")),
            Node::Opaque(_) => return Err(Unsupported("an opaque node")),
        };
        values.push(value);
        Ok(())
    }

    /// Makes an empty list or dict, keeps it under the memo indices of its
    /// puts, and plans the load of its items.
    fn container<'a>(
        &mut self,
        node: &'a Node,
        indices: Vec<u32>,
        tasks: &mut Vec<Task<'a>>,
    ) -> Result<(), Unsupported> {
        let id = self.heap.push(Object::List(Vec::new()));
        for index in indices {
            self.memo.insert(index, Value::Object(id));
        }
        self.open.push(id);
        match node {
            Node::List(list) => {
                if list.initial.is_some() {
                    return Err(Unsupported("a protocol 0 list"));
                }
                let items: Vec<&Node> = list.batches.iter().flat_map(|b| &b.items).collect();
                tasks.push(Task::List(id, items.len()));
                tasks.extend(items.into_iter().rev().map(Task::Enter));
            }
            Node::Dict(dict) => {
                if dict.initial.is_some() {
                    return Err(Unsupported("a protocol 0 dict"));
                }
                let pairs: Vec<&(Node, Node)> =
                    dict.batches.iter().flat_map(|b| &b.items).collect();
                tasks.push(Task::Dict(id, pairs.len()));
                for (key, value) in pairs.into_iter().rev() {
                    tasks.push(Task::Enter(value));
                    tasks.push(Task::Enter(key));
                }
            }
            _ => unreachable!("container takes a list or a dict"),
        }
        Ok(())
    }

    fn close(&mut self, id: ObjectId) {
        self.open.retain(|&open| open != id);
    }

    /// Returns the items of a dict from its keys and values in pickle
    /// order, as `d[key] = value` gives them: a key that comes again keeps
    /// its place and takes the later value.
    fn dict_items(&self, flat: &[Value]) -> Result<Vec<(Value, Value)>, Unsupported> {
        self.heap.set_items(Vec::new(), flat)
    }
}

impl Heap {
    /// Returns the items of a dict after `d[key] = value` of each pair of
    /// `flat`, in order: a key that comes again keeps its place and takes
    /// the later value.
    fn set_items(
        &self,
        mut items: Vec<(Value, Value)>,
        flat: &[Value],
    ) -> Result<Vec<(Value, Value)>, Unsupported> {
        let mut places: HashMap<DictKey, usize> = HashMap::new();
        for (place, &(key, _)) in items.iter().enumerate() {
            places.insert(self.dict_key(key)?, place);
        }
        for &[key, value] in flat.as_chunks::<2>().0 {
            let dict_key = self.dict_key(key)?;
            match places.get(&dict_key) {
                Some(&place) => items[place].1 = value,
                None => {
                    places.insert(dict_key, items.len());
                    items.push((key, value));
                }
            }
        }
        Ok(items)
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

    /// Runs `d[key] = value` on the dict of `id` for each pair of `flat`.
    /// `places` holds the place of each key of the dict, and the call
    /// keeps it up to date, so each pair costs a constant time.
    fn fill_dict(
        &mut self,
        id: ObjectId,
        places: &mut HashMap<DictKey, usize>,
        flat: &[Value],
    ) -> Result<(), Unsupported> {
        let pairs = flat.as_chunks::<2>().0;
        let keys = pairs
            .iter()
            .map(|&[key, _]| self.dict_key(key))
            .collect::<Result<Vec<_>, _>>()?;
        let Object::Dict(items) = &mut self.objects[id.0 as usize] else {
            return Err(Unsupported("an opaque construct of another form"));
        };
        for (dict_key, &[key, value]) in keys.into_iter().zip(pairs) {
            match places.get(&dict_key) {
                Some(&place) => items[place].1 = value,
                None => {
                    places.insert(dict_key, items.len());
                    items.push((key, value));
                }
            }
        }
        Ok(())
    }

    /// Returns the object that REDUCE makes of `callable` and `args`: a set
    /// for `builtins.set` and `builtins.frozenset`, else an object of
    /// another class. A heap of [`Heap::with_values`] takes only a
    /// `Message`.
    fn reduce(&mut self, callable: Value, args: Value) -> Result<Value, Unsupported> {
        if self.values {
            if self.class(callable) != Some(MESSAGE) {
                return Err(Unsupported(
                    "a REDUCE of a callable that the model does not model",
                ));
            }
            return self.new_message(callable, args);
        }
        let Some((module, name)) = self.class(callable) else {
            return Err(Unsupported("a REDUCE of a callable that is not a GLOBAL"));
        };
        let args_items = self
            .tuple(args)
            .ok_or(Unsupported("a REDUCE without a tuple of arguments"))?;
        match (module, name) {
            ("builtins", "set" | "frozenset") => {
                let items = match args_items {
                    [] => Vec::new(),
                    [list] => match self.object(*list) {
                        Some(Object::List(items)) => items.clone(),
                        _ => return Err(Unsupported("a set of another form")),
                    },
                    _ => return Err(Unsupported("a set of another form")),
                };
                Ok(self.new_set(callable, items))
            }
            // The pickler of Python 3 writes such an object with NEWOBJ,
            // so it does not pickle again to the same bytes.
            ("copy_reg" | "copyreg", "_reconstructor") => {
                Err(Unsupported("an object of copy_reg._reconstructor"))
            }
            _ => Ok(Value::Object(self.push(Object::Instance(Instance {
                made: Made::Reduce,
                class: callable,
                args,
                state: None,
            })))),
        }
    }

    /// Returns the class of a GLOBAL. A heap of [`Heap::with_values`]
    /// refuses another name of the module `Missing`: Python can give
    /// `Missing.Value` for it, and the model would keep two objects.
    fn global(&mut self, module: &str, name: &str) -> Result<Value, Unsupported> {
        if self.values && module == MISSING && name != "V" {
            return Err(Unsupported("a name of Missing other than V"));
        }
        Ok(self.new_class(module, name))
    }

    /// Returns the `Message` that REDUCE makes of the class `class` and the
    /// arguments `args`, with the arguments that `Message.__reduce__` gives
    /// again: a new tuple of 7 items, a new str of the text, the domain, the
    /// default, a new dict of the mapping or `None`, the plural, the default
    /// plural and the number. Missing items are `None`.
    fn new_message(&mut self, class: Value, args: Value) -> Result<Value, Unsupported> {
        let other = Unsupported("a Message of another form");
        let items = self.tuple(args).ok_or(other)?.to_vec();
        if items.is_empty() || items.len() > MESSAGE_ARGS {
            return Err(other);
        }
        let text = self.str(items[0]).ok_or(other)?.to_owned();
        let mut fresh = Vec::with_capacity(MESSAGE_ARGS);
        // `str(self)` copies the text. Only the empty str is shared.
        fresh.push(if text.is_empty() {
            self.str_object("")
        } else {
            Value::Object(self.push(Object::Str(text)))
        });
        for index in 1..MESSAGE_ARGS {
            let item = items.get(index).copied().unwrap_or(Value::None);
            fresh.push(match index {
                // `Message` keeps a mapping in a proxy, and `__reduce__`
                // copies it to a new dict.
                3 => match item {
                    Value::None => Value::None,
                    _ => {
                        let mapping = self.dict(item).ok_or(other)?.to_vec();
                        self.new_dict(mapping)
                    }
                },
                // `Message` takes a number only if it is an int or a float.
                6 => match item {
                    Value::None | Value::Bool(_) | Value::Int(_) | Value::Float(_) => item,
                    _ => return Err(other),
                },
                _ => item,
            });
        }
        let args = self.new_tuple(fresh);
        let id = self.push(Object::Instance(Instance {
            made: Made::Reduce,
            class,
            args,
            state: None,
        }));
        self.known.insert(id, Known::Message);
        Ok(Value::Object(id))
    }

    /// Returns the `DateTime` that NEWOBJ makes of the class `class` and the
    /// arguments `args`. Its BUILD comes next.
    fn new_date_time(&mut self, class: Value, args: Value) -> Result<Value, Unsupported> {
        if self.class(class) != Some(DATE_TIME) || self.tuple(args) != Some(&[]) {
            return Err(Unsupported("a NEWOBJ that the model does not model"));
        }
        let id = self.push(Object::Instance(Instance {
            made: Made::New,
            class,
            args,
            state: None,
        }));
        self.known.insert(id, Known::NewDateTime);
        Ok(Value::Object(id))
    }

    /// Gives the `DateTime` of `id` the state that `__setstate__` leaves
    /// of `state`, as `__getstate__` gives it again: a new tuple of
    /// `_micros`, `_timezone_naive` and `_tz`. `_tz` is the str of the
    /// table of `DateTime` for the name of the zone: one object for each
    /// name, in the whole heap.
    fn build_date_time(&mut self, id: ObjectId, state: Value) -> Result<(), Unsupported> {
        let other = Unsupported("a DateTime of another form");
        if self.known.get(&id) != Some(&Known::NewDateTime) {
            return Err(Unsupported("a BUILD of another form"));
        }
        let &[micros, naive, zone] = self.tuple(state).ok_or(other)? else {
            return Err(other);
        };
        let (Value::Int(micros), Value::Bool(naive)) = (micros, naive) else {
            return Err(other);
        };
        if !DATE_TIME_MICROS.contains(&micros) {
            return Err(Unsupported("a DateTime out of the range of the model"));
        }
        let zone = self
            .str(zone)
            .and_then(crate::zones::canonical)
            .ok_or(Unsupported("a DateTime of a zone that is not in the table"))?;
        let text = match self.zones.get(zone) {
            Some(&text) => text,
            None => {
                let text = self.push(Object::Str(zone.to_owned()));
                self.zones.insert(zone, text);
                text
            }
        };
        let state = self.new_tuple(vec![
            Value::Int(micros),
            Value::Bool(naive),
            Value::Object(text),
        ]);
        let Object::Instance(instance) = &mut self.objects[id.0 as usize] else {
            unreachable!("a known DateTime is an instance");
        };
        instance.state = Some(state);
        self.known.insert(id, Known::DateTime(micros, zone));
        Ok(())
    }

    /// Returns what a comparison sees of `value`, when it is an object that
    /// a heap of [`Heap::with_values`] models.
    fn kind(&self, value: Value) -> Option<Kind<'_>> {
        if !self.values {
            return None;
        }
        let Value::Object(id) = value else {
            return None;
        };
        match (self.known.get(&id), self.get(id)) {
            (Some(&Known::DateTime(micros, zone)), _) => Some(Kind::DateTime(micros, zone)),
            (Some(Known::Message), Object::Instance(instance)) => {
                let text = self.tuple(instance.args).and_then(|args| self.str(args[0]));
                Some(Kind::Message(text.expect("a Message holds its text")))
            }
            (None, Object::Class { module, name }) if module == MISSING && name == "V" => {
                Some(Kind::Missing)
            }
            _ => None,
        }
    }
}

/// Returns the object of `value`, if it is an object.
fn value_id(value: Value) -> Option<ObjectId> {
    match value {
        Value::Object(id) => Some(id),
        _ => None,
    }
}

/// One entry of the stack of [`Loader::opaque`].
#[derive(Clone, Copy)]
enum Slot {
    Mark,
    /// A value, and true when a memo get pushed it.
    Value(Value, bool),
}

impl Loader<'_> {
    /// Loads the bytes of an opaque node: whole opcodes that push one
    /// value. The memo is the memo of the record. An object that NEWOBJ or
    /// REDUCE makes, and the state of BUILD, become an [`Object::Instance`].
    /// A list, a dict or an object takes its items or its state only right
    /// after it is made, as the pickler writes them, so no value holds
    /// itself.
    fn opaque(&mut self, bytes: &[u8]) -> Result<Value, Unsupported> {
        let other = Unsupported("an opaque construct of another form");
        let mut stack = Stack::default();
        // The place of each key of each dict that the bytes fill.
        let mut places: HashMap<ObjectId, HashMap<DictKey, usize>> = HashMap::new();
        // The `DateTime` objects that NEWOBJ made in the bytes.
        let mut made: Vec<ObjectId> = Vec::new();
        let mut pos = 0;
        while let Some(&opcode) = bytes.get(pos) {
            let rest = &bytes[pos + 1..];
            let len = crate::opcode::argument_len(opcode, rest).ok_or(other)?;
            let arg = &rest[..len];
            pos += 1 + len;
            let value = match opcode {
                MARK => {
                    stack.push(Slot::Mark, self.heap);
                    continue;
                }
                NONE => Value::None,
                NEWTRUE => Value::Bool(true),
                NEWFALSE => Value::Bool(false),
                BININT1 => Value::Int(i128::from(arg[0])),
                BININT2 => Value::Int(i128::from(u16::from_le_bytes([arg[0], arg[1]]))),
                BININT => Value::Int(i128::from(i32::from_le_bytes(
                    arg.try_into().map_err(|_| other)?,
                ))),
                LONG1 => Value::Int(long(&arg[1..])?),
                LONG4 => Value::Int(long(&arg[4..])?),
                BINFLOAT => Value::Float(f64::from_be_bytes(arg.try_into().map_err(|_| other)?)),
                BINUNICODE => {
                    let text = std::str::from_utf8(&arg[4..])
                        .map_err(|_| Unsupported("a str that is not UTF-8"))?;
                    self.heap.str_object(text)
                }
                SHORT_BINBYTES => self.heap.bytes_object(&arg[1..]),
                BINBYTES => self.heap.bytes_object(&arg[4..]),
                GLOBAL => {
                    let text = std::str::from_utf8(arg).map_err(|_| other)?;
                    let mut lines = text.split('\n');
                    let (Some(module), Some(name)) = (lines.next(), lines.next()) else {
                        return Err(other);
                    };
                    self.heap.global(module, name)?
                }
                EMPTY_TUPLE => self.heap.new_tuple(Vec::new()),
                TUPLE1 | TUPLE2 | TUPLE3 => {
                    let n = usize::from(opcode - TUPLE1) + 1;
                    let items = stack.pop_values(n)?;
                    self.heap.new_tuple(items)
                }
                TUPLE => {
                    let items = stack.pop_mark()?;
                    self.heap.new_tuple(items)
                }
                EMPTY_LIST => Value::Object(self.heap.push(Object::List(Vec::new()))),
                EMPTY_DICT => Value::Object(self.heap.push(Object::Dict(Vec::new()))),
                APPEND | APPENDS => {
                    let items = if opcode == APPEND {
                        stack.pop_values(1)?
                    } else {
                        stack.pop_mark()?
                    };
                    let id = stack.fillable()?;
                    match &mut self.heap.objects[id.0 as usize] {
                        Object::List(list) => list.extend(items),
                        _ => return Err(other),
                    }
                    continue;
                }
                SETITEM | SETITEMS => {
                    let flat = if opcode == SETITEM {
                        stack.pop_values(2)?
                    } else {
                        stack.pop_mark()?
                    };
                    let id = stack.fillable()?;
                    // A dict that the bytes fill comes from EMPTY_DICT of
                    // the bytes, so its places start empty.
                    let places = places.entry(id).or_default();
                    self.heap.fill_dict(id, places, &flat)?;
                    continue;
                }
                BINPUT | LONG_BINPUT => {
                    let index = if opcode == BINPUT {
                        u32::from(arg[0])
                    } else {
                        u32::from_le_bytes(arg.try_into().map_err(|_| other)?)
                    };
                    let Some(Slot::Value(value, _)) = stack.slots.last() else {
                        return Err(other);
                    };
                    self.memo.insert(index, *value);
                    continue;
                }
                BINGET | LONG_BINGET => {
                    let index = if opcode == BINGET {
                        u32::from(arg[0])
                    } else {
                        u32::from_le_bytes(arg.try_into().map_err(|_| other)?)
                    };
                    let value = *self
                        .memo
                        .get(&index)
                        .ok_or(Unsupported("a memo get without its put"))?;
                    if let Value::Object(id) = value
                        && (stack.holds(id) || self.open.contains(&id))
                    {
                        return Err(Unsupported("a container that holds itself"));
                    }
                    stack.push(Slot::Value(value, true), self.heap);
                    continue;
                }
                BINPERSID => {
                    let [data] = stack.pop_values(1)?[..] else {
                        return Err(other);
                    };
                    self.heap.reference_of(data)?
                }
                REDUCE => {
                    let [callable, args] = stack.pop_values(2)?[..] else {
                        return Err(other);
                    };
                    self.heap.reduce(callable, args)?
                }
                NEWOBJ => {
                    let [class, args] = stack.pop_values(2)?[..] else {
                        return Err(other);
                    };
                    if self.heap.class(class).is_none() || self.heap.tuple(args).is_none() {
                        return Err(Unsupported("a NEWOBJ of another form"));
                    }
                    if self.heap.values {
                        let value = self.heap.new_date_time(class, args)?;
                        made.extend(value_id(value));
                        value
                    } else {
                        Value::Object(self.heap.push(Object::Instance(Instance {
                            made: Made::New,
                            class,
                            args,
                            state: None,
                        })))
                    }
                }
                BUILD => {
                    let [state] = stack.pop_values(1)?[..] else {
                        return Err(other);
                    };
                    let id = stack.fillable()?;
                    if self.heap.values {
                        self.heap.build_date_time(id, state)?;
                        continue;
                    }
                    match &mut self.heap.objects[id.0 as usize] {
                        Object::Instance(instance) if instance.state.is_none() => {
                            instance.state = Some(state);
                        }
                        _ => return Err(Unsupported("a BUILD of another form")),
                    }
                    // `load_build` interns the keys of a dict state, also of
                    // the dict of a `(dict, slots)` state.
                    let dict = match self.heap.tuple(state) {
                        Some(&[dict, _]) => dict,
                        _ => state,
                    };
                    self.heap.intern_keys(dict);
                    continue;
                }
                _ => return Err(Unsupported("an opcode that the model does not model")),
            };
            stack.push(Slot::Value(value, false), self.heap);
        }
        // A `DateTime` without its BUILD has no state to pickle again.
        if made
            .iter()
            .any(|id| self.heap.known.get(id) == Some(&Known::NewDateTime))
        {
            return Err(Unsupported("a DateTime without a state"));
        }
        match stack.slots[..] {
            [Slot::Value(value, _)] => Ok(value),
            _ => Err(other),
        }
    }
}

impl Heap {
    /// Tells if the object of `id` can take items or a state: a list, a
    /// dict or an object of another class.
    fn fillable(&self, id: ObjectId) -> bool {
        matches!(
            self.get(id),
            Object::List(_) | Object::Dict(_) | Object::Instance(_)
        )
    }
}

/// The stack of [`Loader::opaque`], with the number of times that each
/// list, dict or object of another class is on it, so a memo get tells in a
/// constant time if it would make a value that holds itself.
#[derive(Default)]
struct Stack {
    slots: Vec<Slot>,
    counts: HashMap<ObjectId, usize>,
}

impl Stack {
    fn push(&mut self, slot: Slot, heap: &Heap) {
        if let Slot::Value(Value::Object(id), _) = slot
            && heap.fillable(id)
        {
            *self.counts.entry(id).or_default() += 1;
        }
        self.slots.push(slot);
    }

    /// Tells if the list, dict or object of `id` is on the stack: it is
    /// still taking its items or its state.
    fn holds(&self, id: ObjectId) -> bool {
        self.counts.get(&id).is_some_and(|&count| count > 0)
    }

    /// Returns the object on top of the stack, to fill it. An object that a
    /// memo get pushed is complete and takes nothing more.
    fn fillable(&self) -> Result<ObjectId, Unsupported> {
        match self.slots.last() {
            Some(Slot::Value(Value::Object(id), false)) => Ok(*id),
            _ => Err(Unsupported("a fill of a value that is complete")),
        }
    }

    /// Pops the last `n` values, in order.
    fn pop_values(&mut self, n: usize) -> Result<Vec<Value>, Unsupported> {
        let other = Unsupported("an opaque construct of another form");
        let start = self.slots.len().checked_sub(n).ok_or(other)?;
        let mut values = Vec::with_capacity(n);
        for slot in self.slots.drain(start..) {
            let Slot::Value(value, _) = slot else {
                return Err(other);
            };
            if let Value::Object(id) = value
                && let Some(count) = self.counts.get_mut(&id)
            {
                *count -= 1;
            }
            values.push(value);
        }
        Ok(values)
    }

    /// Pops the values above the last mark, and the mark.
    fn pop_mark(&mut self) -> Result<Vec<Value>, Unsupported> {
        let mark = self
            .slots
            .iter()
            .rposition(|slot| matches!(slot, Slot::Mark))
            .ok_or(Unsupported("an opaque construct of another form"))?;
        let values = self.pop_values(self.slots.len() - mark - 1)?;
        self.slots.pop();
        Ok(values)
    }
}

/// Returns the int of the two's complement bytes of LONG1 or LONG4,
/// little-endian, if it fits 128 bits.
fn long(bytes: &[u8]) -> Result<i128, Unsupported> {
    Int::long_to_i128(bytes).ok_or(Unsupported("an int that does not fit 128 bits"))
}

/// Returns the value of an int node, if it fits 128 bits.
fn int_value(int: &Int) -> Result<i128, Unsupported> {
    int.to_i128()
        .ok_or(Unsupported("an int that does not fit 128 bits"))
}

/// The size of a batch of SETITEMS or APPENDS: `BATCHSIZE` of the pickler.
const BATCH: usize = 1000;

/// A pickler of the objects of a [`Heap`]: the C pickler of zodbpickle with
/// protocol 3, as ZODB's `PersistentPickler` runs it.
///
/// Like one `Pickler` object of Python, it keeps its memo from one
/// [`Pickler::dump`] to the next. ZODB writes the class pickle and the
/// state pickle of a record with one pickler, so the state can refer to
/// the class with a memo get.
///
/// A persistent reference writes its persistent id, then BINPERSID, as
/// `persistent_id` of ZODB's resolver makes the pickler do. The reference
/// itself never goes into the memo.
pub struct Pickler<'h> {
    heap: &'h Heap,
    memo: HashMap<ObjectId, u32>,
    /// The next memo index.
    next: u32,
    out: Vec<u8>,
}

/// One step of the pickler.
enum Step {
    /// Save a value: a memo get if the pickler saw it, else its opcodes.
    Save(Value),
    /// Write one opcode byte.
    Byte(u8),
    /// Put an object in the memo, and write the put.
    Memoize(ObjectId),
    /// Write the put of an object that the pickler makes for one save,
    /// such as the list of the items of a set: it takes a memo index, and
    /// nothing gets it.
    MemoizeFresh,
}

impl<'h> Pickler<'h> {
    /// Returns a pickler of the objects of `heap` with an empty memo.
    pub fn new(heap: &'h Heap) -> Self {
        Pickler {
            heap,
            memo: HashMap::new(),
            next: 0,
            out: Vec::new(),
        }
    }

    /// Writes one pickle of `value`: PROTO 3, the value, STOP.
    pub fn dump(&mut self, value: Value) -> Result<(), Unsupported> {
        self.out.extend([PROTO, 3]);
        let mut steps = vec![Step::Save(value)];
        while let Some(step) = steps.pop() {
            match step {
                Step::Byte(byte) => self.out.push(byte),
                Step::Memoize(id) => self.memoize(id),
                Step::MemoizeFresh => self.memoize_fresh(),
                Step::Save(value) => self.save(value, &mut steps)?,
            }
        }
        self.out.push(STOP);
        Ok(())
    }

    /// Returns the bytes of the pickles.
    pub fn into_bytes(self) -> Vec<u8> {
        self.out
    }

    fn save(&mut self, value: Value, steps: &mut Vec<Step>) -> Result<(), Unsupported> {
        let id = match value {
            Value::None => {
                self.out.push(NONE);
                return Ok(());
            }
            Value::Bool(true) => {
                self.out.push(NEWTRUE);
                return Ok(());
            }
            Value::Bool(false) => {
                self.out.push(NEWFALSE);
                return Ok(());
            }
            Value::Int(int) => {
                self.save_int(int);
                return Ok(());
            }
            Value::Float(float) => {
                self.out.push(BINFLOAT);
                self.out.extend(float.to_be_bytes());
                return Ok(());
            }
            Value::Object(id) => id,
        };
        let object = self.heap.get(id);
        // `persistent_id` comes first: the pickler saves the persistent id
        // of a reference and never looks for the reference in the memo.
        if let Object::Reference(reference) = object {
            steps.push(Step::Byte(BINPERSID));
            steps.push(Step::Save(reference.data));
            return Ok(());
        }
        if let Some(&index) = self.memo.get(&id) {
            self.memo_op(index, BINGET, LONG_BINGET);
            return Ok(());
        }
        match object {
            Object::Str(text) => {
                self.out.push(BINUNICODE);
                self.length(text.len(), 4)?;
                self.out.extend(text.as_bytes());
                self.memoize(id);
            }
            Object::Bytes(bytes) => {
                if bytes.len() <= 0xff {
                    self.out.extend([SHORT_BINBYTES, bytes.len() as u8]);
                } else {
                    self.out.push(BINBYTES);
                    self.length(bytes.len(), 4)?;
                }
                self.out.extend(bytes);
                self.memoize(id);
            }
            Object::Class { module, name } => {
                self.out.push(GLOBAL);
                self.out.extend(module.as_bytes());
                self.out.push(b'\n');
                self.out.extend(name.as_bytes());
                self.out.push(b'\n');
                self.memoize(id);
            }
            Object::Tuple(items) => match items.len() {
                0 => self.out.push(EMPTY_TUPLE),
                len @ 1..=3 => {
                    steps.push(Step::Memoize(id));
                    steps.push(Step::Byte([TUPLE1, TUPLE2, TUPLE3][len - 1]));
                    steps.extend(items.iter().rev().map(|&item| Step::Save(item)));
                }
                _ => {
                    steps.push(Step::Memoize(id));
                    steps.push(Step::Byte(TUPLE));
                    steps.extend(items.iter().rev().map(|&item| Step::Save(item)));
                    steps.push(Step::Byte(MARK));
                }
            },
            Object::List(items) => {
                self.out.push(EMPTY_LIST);
                self.memoize(id);
                steps.extend(list_steps(items).into_iter().rev());
            }
            Object::Dict(items) => {
                self.out.push(EMPTY_DICT);
                self.memoize(id);
                steps.extend(dict_steps(items).into_iter().rev());
            }
            Object::Instance(instance) => {
                // `save_reduce`: the class or the callable, the arguments,
                // NEWOBJ or REDUCE, the put of the object, then the state
                // and BUILD.
                if let Some(state) = instance.state {
                    steps.push(Step::Byte(BUILD));
                    steps.push(Step::Save(state));
                }
                steps.push(Step::Memoize(id));
                steps.push(Step::Byte(match instance.made {
                    Made::New => NEWOBJ,
                    Made::Reduce => REDUCE,
                }));
                steps.push(Step::Save(instance.args));
                steps.push(Step::Save(instance.class));
            }
            Object::Set { class, items } => {
                // `set.__reduce__` gives `(type, (list(self),), None)`: a
                // new list and a new tuple for each save.
                steps.push(Step::Memoize(id));
                steps.push(Step::Byte(REDUCE));
                steps.push(Step::MemoizeFresh);
                steps.push(Step::Byte(TUPLE1));
                steps.extend(list_steps(items).into_iter().rev());
                steps.push(Step::MemoizeFresh);
                steps.push(Step::Byte(EMPTY_LIST));
                steps.push(Step::Save(*class));
            }
            Object::Reference(_) => unreachable!("a reference is saved above"),
        }
        Ok(())
    }

    /// Writes an int as `save_long` does with protocol 2 and later.
    fn save_int(&mut self, int: i128) {
        if let Ok(small) = i32::try_from(int) {
            let le = small.to_le_bytes();
            if le[2] != 0 || le[3] != 0 {
                self.out.push(BININT);
                self.out.extend(le);
            } else if le[1] != 0 {
                self.out.extend([BININT2, le[0], le[1]]);
            } else {
                self.out.extend([BININT1, le[0]]);
            }
            return;
        }
        // LONG1: the two's complement, little-endian, in the fewest bytes
        // that keep the sign. An int that needs no 32 bits took a form
        // above, so it is never 0 here.
        let le = int.to_le_bytes();
        let mut len = 16;
        let fill = if int < 0 { 0xff } else { 0 };
        while len > 1 && le[len - 1] == fill && (le[len - 2] & 0x80) == (fill & 0x80) {
            len -= 1;
        }
        self.out.extend([LONG1, len as u8]);
        self.out.extend(&le[..len]);
    }

    /// Writes a length of `width` bytes, little-endian.
    fn length(&mut self, len: usize, width: usize) -> Result<(), Unsupported> {
        let len = u32::try_from(len).map_err(|_| Unsupported("a value of 4 GiB or more"))?;
        self.out.extend(&len.to_le_bytes()[..width]);
        Ok(())
    }

    fn memoize(&mut self, id: ObjectId) {
        let index = self.next;
        self.next = index.checked_add(1).expect("fewer than 2^32 memo entries");
        self.memo.insert(id, index);
        self.memo_op(index, BINPUT, LONG_BINPUT);
    }

    fn memoize_fresh(&mut self) {
        let index = self.next;
        self.next = index.checked_add(1).expect("fewer than 2^32 memo entries");
        self.memo_op(index, BINPUT, LONG_BINPUT);
    }

    fn memo_op(&mut self, index: u32, short: u8, long: u8) {
        match u8::try_from(index) {
            Ok(index) => self.out.extend([short, index]),
            Err(_) => {
                self.out.push(long);
                self.out.extend(index.to_le_bytes());
            }
        }
    }
}

/// The steps of the items of a list, as `batch_list_exact` writes them: one
/// item and APPEND, or batches of MARK, at most 1,000 items, APPENDS.
fn list_steps(items: &[Value]) -> Vec<Step> {
    let mut steps = Vec::new();
    if let [item] = items {
        steps.push(Step::Save(*item));
        steps.push(Step::Byte(APPEND));
        return steps;
    }
    for batch in items.chunks(BATCH) {
        steps.push(Step::Byte(MARK));
        steps.extend(batch.iter().map(|&item| Step::Save(item)));
        steps.push(Step::Byte(APPENDS));
    }
    steps
}

/// The steps of the items of a dict, as `batch_dict_exact` writes them:
/// one item and SETITEM, or batches of MARK, at most 1,000 items, SETITEMS.
/// A batch of 1,000 items is always followed by one more batch, empty when
/// no item is left.
fn dict_steps(items: &[(Value, Value)]) -> Vec<Step> {
    let mut steps = Vec::new();
    match items {
        [] => {}
        [(key, value)] => {
            steps.extend([Step::Save(*key), Step::Save(*value), Step::Byte(SETITEM)]);
        }
        _ => {
            let mut rest = items;
            loop {
                let (batch, after) = rest.split_at(rest.len().min(BATCH));
                steps.push(Step::Byte(MARK));
                for &(key, value) in batch {
                    steps.push(Step::Save(key));
                    steps.push(Step::Save(value));
                }
                steps.push(Step::Byte(SETITEMS));
                if batch.len() < BATCH {
                    break;
                }
                rest = after;
            }
        }
    }
    steps
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode_record;

    /// The class pickle of a record of `m.C`, as ZODB writes it.
    const CLASS: &[u8] = b"\x80\x03cm\nC\nq\x00.";

    /// Loads the record of the class pickle `CLASS` and `state`.
    fn load(heap: &mut Heap, state: &[u8]) -> (Value, Value) {
        let record = [CLASS, state].concat();
        heap.load(&decode_record(&record).unwrap()).unwrap()
    }

    /// Pickles the class and the state as ZODB's resolver does.
    fn dump(heap: &Heap, class: Value, state: Value) -> Vec<u8> {
        let mut pickler = Pickler::new(heap);
        pickler.dump(class).unwrap();
        pickler.dump(state).unwrap();
        pickler.into_bytes()
    }

    #[test]
    fn a_state_pickles_to_the_bytes_that_it_came_from() {
        // The pickles of Python 3.13 with ZODB's pickler.
        for state in [
            &b"\x80\x03X\x03\x00\x00\x00abcq\x01h\x01\x86q\x02."[..],
            b"\x80\x03X\x03\x00\x00\x00abcq\x01X\x03\x00\x00\x00abcq\x02\x86q\x03.",
            b"\x80\x03(\x8a\x05\x00\x00\x00\x80\x00\x8a\x05\xff\xff\xff\x7f\xff\
              \x8a\x05\x00\x00\x00\x00\x80\x8a\t\x00\x00\x00\x00\x00\x00\x00\x80\x00\
              \x8a\t\xff\xff\xff\xff\xff\xff\xff\xff\x00K\x00J\xff\xff\xff\xffK\xffM\x00\x01\
              M\xff\xffJ\x00\x00\x01\x00tq\x01.",
            b"\x80\x03}q\x01K\x01K\x02s.",
            b"\x80\x03}q\x01(K\x01K\x02K\x03G@\x12\x00\x00\x00\x00\x00\x00u.",
            b"\x80\x03]q\x01K\x01a.",
            b"\x80\x03h\x00.",
            b"\x80\x03C\x08\x00\x00\x00\x00\x00\x00\x00\x07q\x01h\x00\x86q\x02Q.",
            b"\x80\x03(N\x88\x89)tq\x01.",
        ] {
            let mut heap = Heap::new();
            let (class, value) = load(&mut heap, state);
            assert_eq!(
                dump(&heap, class, value),
                [CLASS, state].concat(),
                "{state:?}"
            );
        }
    }

    #[test]
    fn a_dict_of_1000_items_ends_with_an_empty_batch() {
        let mut heap = Heap::new();
        let items = (0..1000).map(|i| (Value::Int(i), Value::Int(i))).collect();
        let dict = heap.new_dict(items);
        let mut pickler = Pickler::new(&heap);
        pickler.dump(dict).unwrap();
        let bytes = pickler.into_bytes();
        assert!(
            bytes.ends_with(b"M\xe7\x03M\xe7\x03u(u."),
            "{:?}",
            &bytes[bytes.len() - 12..]
        );
        assert!(bytes.starts_with(b"\x80\x03}q\x00(K\x00K\x00K\x01K\x01"));
        let items = (0..1001).map(|i| (Value::Int(i), Value::Int(0))).collect();
        let dict = heap.new_dict(items);
        let mut pickler = Pickler::new(&heap);
        pickler.dump(dict).unwrap();
        assert!(pickler.into_bytes().ends_with(b"M\xe8\x03K\x00u."));
    }

    #[test]
    fn short_strs_and_bytes_are_shared_across_records() {
        let mut heap = Heap::new();
        let state = b"\x80\x03(X\x00\x00\x00\x00X\x01\x00\x00\x00aX\x02\x00\x00\x00\xc3\xa9\
                      X\x02\x00\x00\x00\xc4\x80C\x00C\x01aC\x02abt.";
        let (_, first) = load(&mut heap, state);
        let (_, second) = load(&mut heap, state);
        let first = heap.tuple(first).unwrap().to_vec();
        let second = heap.tuple(second).unwrap().to_vec();
        let shared: Vec<bool> = first.iter().zip(&second).map(|(a, b)| a == b).collect();
        assert_eq!(shared, [true, true, true, false, true, true, false]);
        // The pickler writes a shared object again as a memo get.
        let both = heap.new_tuple(vec![first[1], second[1], first[6], second[6]]);
        let mut pickler = Pickler::new(&heap);
        pickler.dump(both).unwrap();
        assert_eq!(
            pickler.into_bytes(),
            b"\x80\x03(X\x01\x00\x00\x00aq\x00h\x00C\x02abq\x01C\x02abq\x02tq\x03."
        );
    }

    #[test]
    fn a_reference_is_one_object_for_its_persistent_id() {
        let mut heap = Heap::new();
        let state = b"\x80\x03C\x08\x00\x00\x00\x00\x00\x00\x00\x07q\x01h\x00\x86q\x02Q.";
        let (class, first) = load(&mut heap, state);
        let (_, second) = load(&mut heap, state);
        assert_eq!(first, second);
        let reference = heap.reference(first).unwrap();
        assert_eq!(reference.oid, 7u64.to_be_bytes());
        assert!(!reference.weak);
        // Two uses of one reference: the persistent id is a memo get.
        let pair = heap.new_tuple(vec![first, second]);
        assert_eq!(
            dump(&heap, class, pair),
            b"\x80\x03cm\nC\nq\x00.\x80\x03C\x08\x00\x00\x00\x00\x00\x00\x00\x07q\x01h\x00\x86q\x02Qh\x02Q\x86q\x03."
        );
    }

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
    fn the_model_refuses_what_it_does_not_model() {
        for (state, what) in [
            (
                &b"\x80\x03]q\x01h\x01a."[..],
                "a container that holds itself",
            ),
            (b"\x80\x03U\x01a.", "a Python 2 str"),
            (b"\x80\x03cbuiltins\nset\n]\x85R.", "a REDUCE"),
            (b"\x80\x03}(N\x88u.", "a dict key of another type"),
            (
                b"\x80\x03X\x01\x00\x00\x00aQ.",
                "a persistent id of an unknown shape",
            ),
        ] {
            let mut heap = Heap::new();
            let record = [CLASS, state].concat();
            let tree = decode_record(&record).unwrap();
            assert_eq!(heap.load(&tree), Err(Unsupported(what)), "{state:?}");
        }
    }

    /// The record of a persistent object of `__main__.Doc` with a str, a
    /// `DateTime`, a tuple and a set, as ZODB 6 writes it with Python 3.13.
    const DOC: &[u8] = b"\x80\x03c__main__\nDoc\nq\x00.\x80\x03}q\x01(X\x05\x00\x00\x00titleq\x02\
        X\x01\x00\x00\x00aq\x03X\x11\x00\x00\x00modification_dateq\x04cDateTime.DateTime\nDateTime\n\
        q\x05)\x81q\x06\x8a\x07@\xf3L\xf6^G\x06\x89X\x03\x00\x00\x00UTCq\x07\x87q\x08b\
        X\x07\x00\x00\x00subjectq\tX\x01\x00\x00\x00xq\nX\x01\x00\x00\x00yq\x0b\x86q\x0c\
        X\x04\x00\x00\x00tagsq\rcbuiltins\nset\nq\x0e]q\x0f(K\x01K\x02e\x85q\x10Rq\x11u.";

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
    fn a_heap_with_instances_loads_objects_and_sets_and_pickles_them_again() {
        let mut heap = Heap::with_instances();
        let (class, state) = heap.load(&decode_record(DOC).unwrap()).unwrap();
        assert_eq!(dump(&heap, class, state), DOC);
        let fields = heap.dict(state).unwrap().to_vec();
        let date = heap.instance(fields[1].1).unwrap();
        assert_eq!(date.made, Made::New);
        assert_eq!(
            heap.class(date.class),
            Some(("DateTime.DateTime", "DateTime"))
        );
        let micros = heap.tuple(date.state.unwrap()).unwrap()[0];
        assert_eq!(micros, Value::Int(1_767_323_045_000_000));
        let Some(Object::Set { items, .. }) = heap.object(fields[3].1) else {
            panic!("a set");
        };
        assert_eq!(items, &[Value::Int(1), Value::Int(2)]);
        // A heap without instances refuses the same record.
        let mut strict = Heap::new();
        assert!(strict.load(&decode_record(DOC).unwrap()).is_err());
    }

    #[test]
    fn a_reduce_and_a_new_set_pickle_as_python_does() {
        // `datetime.date(2026, 1, 2)` and `{3}`, in a tuple, with Python
        // 3.13 and protocol 3.
        let state: &[u8] =
            b"\x80\x03cdatetime\ndate\nq\x01C\x04\x07\xea\x01\x02q\x02\x85q\x03Rq\x04\
            cbuiltins\nset\nq\x05]q\x06K\x03a\x85q\x07Rq\x08\x86q\x09.";
        let mut heap = Heap::with_instances();
        let (class, value) = load(&mut heap, state);
        assert_eq!(dump(&heap, class, value), [CLASS, state].concat());
        let items = heap.tuple(value).unwrap().to_vec();
        assert_eq!(heap.instance(items[0]).unwrap().made, Made::Reduce);
        // A set that a merge makes takes a new list and a new tuple.
        let Some(Object::Set { class: set, .. }) = heap.object(items[1]) else {
            panic!("a set");
        };
        let made = heap.new_set(*set, vec![Value::Int(3)]);
        let pair = heap.new_tuple(vec![items[0], made]);
        assert_eq!(dump(&heap, class, pair), [CLASS, state].concat());
    }

    #[test]
    fn canonical_ids_compare_the_structure() {
        let mut heap = Heap::with_instances();
        let (_, one) = heap.load(&decode_record(DOC).unwrap()).unwrap();
        let (_, two) = heap.load(&decode_record(DOC).unwrap()).unwrap();
        assert_ne!(one, two);
        let mut canon = Canon::new(usize::MAX);
        assert_eq!(canon.same(&heap, one, two), Ok(true));
        assert_eq!(
            canon.same(&heap, Value::Int(1), Value::Bool(true)),
            Ok(false)
        );
        assert_eq!(
            canon.same(&heap, Value::Float(f64::NAN), Value::Float(f64::NAN)),
            Ok(true)
        );
        let a = heap.new_dict(vec![
            (Value::Int(1), Value::Int(1)),
            (Value::Int(2), Value::Int(2)),
        ]);
        let b = heap.new_dict(vec![
            (Value::Int(2), Value::Int(2)),
            (Value::Int(1), Value::Int(1)),
        ]);
        assert_eq!(canon.same(&heap, a, b), Ok(false));
        // Two sets with the same items in another order are the same.
        let class = heap.new_class("builtins", "set");
        let x = heap.new_set(class, vec![Value::Int(1), Value::Int(2)]);
        let y = heap.new_set(class, vec![Value::Int(2), Value::Int(1)]);
        assert_eq!(canon.same(&heap, x, y), Ok(true));
        // Python cannot tell if two objects of another class are equal.
        let fields = heap.dict(one).unwrap().to_vec();
        let other = heap.dict(two).unwrap().to_vec();
        assert_eq!(heap.eq(fields[1].1, other[1].1), Err(Undecided::Unknown));
    }

    #[test]
    fn a_shared_or_deep_value_takes_steps_in_proportion_to_its_objects() {
        let mut heap = Heap::new();
        // `x = (x, x)` 60 times: 2^60 leaves, 61 objects.
        let mut shared = heap.new_tuple(vec![Value::Int(1)]);
        for _ in 0..60 {
            shared = heap.new_tuple(vec![shared, shared]);
        }
        let mut canon = Canon::new(1_000);
        assert!(canon.id(&heap, shared).is_ok());
        // A tuple nested 1,000,000 deep, without recursion.
        let mut deep = heap.new_tuple(Vec::new());
        for _ in 0..1_000_000 {
            deep = heap.new_tuple(vec![deep]);
        }
        let mut canon = Canon::new(usize::MAX);
        assert!(canon.id(&heap, deep).is_ok());
        // Past its budget, a canon gives no answer.
        let mut small = Canon::new(100);
        assert_eq!(small.id(&heap, deep), Err(Undecided::Budget));
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

    #[test]
    fn a_large_opaque_value_loads_in_linear_time() {
        // An object whose state is a dict filled by 100,000 SETITEMs, and an
        // object of 200,000 memo gets of one str.
        let mut setitems = b"\x80\x03cm\nM\nq\x01)\x81}".to_vec();
        for i in 0..100_000 {
            let key = format!("k{i}");
            setitems.push(b'X');
            setitems.extend(u32::try_from(key.len()).unwrap().to_le_bytes());
            setitems.extend(key.as_bytes());
            setitems.extend(b"K\x01s");
        }
        setitems.extend(b"b.");
        let mut gets = b"\x80\x03cm\nM\nq\x01(X\x03\x00\x00\x00abcq\x02".to_vec();
        for _ in 0..200_000 {
            gets.extend(b"h\x02");
        }
        gets.extend(b"t\x81.");
        for state in [setitems, gets] {
            let record = [CLASS, &state].concat();
            let tree = crate::decode_record_with(
                &record,
                crate::Limits {
                    node_budget: record.len(),
                },
            )
            .unwrap();
            let started = std::time::Instant::now();
            let mut heap = Heap::with_instances();
            heap.load(&tree).unwrap();
            assert!(started.elapsed() < std::time::Duration::from_secs(5));
        }
    }

    #[test]
    fn the_keys_of_a_dict_state_are_interned_across_records() {
        // An object of `m.M` whose `__dict__` is `{'micros': 1}`.
        let state: &[u8] =
            b"\x80\x03cm\nM\nq\x01)\x81q\x02}q\x03X\x06\x00\x00\x00microsq\x04K\x01sb.";
        let mut heap = Heap::with_instances();
        let (class, one) = load(&mut heap, state);
        let (_, two) = load(&mut heap, state);
        let key = |heap: &Heap, value: Value| {
            let state = heap.instance(value).unwrap().state.unwrap();
            heap.dict(state).unwrap()[0].0
        };
        assert_eq!(key(&heap, one), key(&heap, two));
        // The pickler writes the key once, then a memo get, as CPython does
        // for two loaded objects.
        let pair = heap.new_tuple(vec![one, two]);
        let bytes = dump(&heap, class, pair);
        assert_eq!(bytes.windows(6).filter(|w| w == b"micros").count(), 1);
    }

    #[test]
    fn an_opaque_value_never_holds_itself() {
        // An object whose state names the object.
        let state: &[u8] = b"\x80\x03cm\nD\nq\x01)\x81q\x02}q\x03X\x01\x00\x00\x00sq\x04h\x02sb.";
        let mut heap = Heap::with_instances();
        let record = [CLASS, state].concat();
        assert_eq!(
            heap.load(&decode_record(&record).unwrap()),
            Err(Unsupported("a container that holds itself"))
        );
        // A REDUCE of `copyreg._reconstructor` pickles again with NEWOBJ.
        let state: &[u8] =
            b"\x80\x03ccopyreg\n_reconstructor\nq\x01cm\nD\nq\x02cbuiltins\nobject\n\
            q\x03N\x87q\x04Rq\x05.";
        let record = [CLASS, state].concat();
        assert_eq!(
            heap.load(&decode_record(&record).unwrap()),
            Err(Unsupported("an object of copy_reg._reconstructor"))
        );
    }

    #[test]
    fn a_key_that_comes_again_keeps_its_place_and_takes_the_later_value() {
        let mut heap = Heap::new();
        let (_, dict) = load(
            &mut heap,
            b"\x80\x03}q\x01(K\x01K\x02K\x03K\x04K\x01K\x05u.",
        );
        assert_eq!(
            heap.dict(dict).unwrap(),
            [
                (Value::Int(1), Value::Int(5)),
                (Value::Int(3), Value::Int(4))
            ]
        );
    }

    /// A record whose state is a tuple of 20 values: `None`, `1`, `1.5`,
    /// `True`, `'s'`, `'Page'`, `b'b'`, `(1,)`, `[1]`, `{}`,
    /// `Missing.Value`, a `DateTime` `a` in `GMT+2`, `a` in `UTC`, a
    /// `DateTime` one second later in `UTC`, `Message('Page', 'plone')`,
    /// `Message('Page', 'plone.app.contenttypes')`, the class `int`,
    /// `Message('a')`, `'a'` and `Message('')`, as ZODB 6 writes it with
    /// Python 3.13. ZODB's resolver pickles it again to the same bytes.
    const VALUES: &[u8] =
        b"\x80\x03cBTrees.OOBTree\nOOBucket\nq\x00.\x80\x03(NK\x01G?\xf8\x00\x00\x00\x00\x00\x00\
        \x88X\x01\x00\x00\x00sq\x01X\x04\x00\x00\x00Pageq\x02C\x01bq\x03K\x01\x85q\x04]q\x05K\
        \x01a}q\x06cMissing\nV\nq\x07cDateTime.DateTime\nDateTime\nq\x08)\x81q\x09\x8a\x07\x00\
        @ FHG\x06\x89X\x05\x00\x00\x00GMT+2q\n\x87q\x0bbh\x08)\x81q\x0c\x8a\x07\x00@ FHG\x06\
        \x89X\x03\x00\x00\x00UTCq\x0d\x87q\x0ebh\x08)\x81q\x0f\x8a\x07@\x82/FHG\x06\x89h\x0d\
        \x87q\x10bczope.i18nmessageid.message\nMessage\nq\x11(X\x04\x00\x00\x00Pageq\x12X\x05\
        \x00\x00\x00ploneq\x13NNNNNtq\x14Rq\x15h\x11(X\x04\x00\x00\x00Pageq\x16X\x16\x00\x00\
        \x00plone.app.contenttypesq\x17NNNNNtq\x18Rq\x19cbuiltins\nint\nq\x1ah\x11(X\x01\x00\
        \x00\x00aq\x1bNNNNNNtq\x1cRq\x1dX\x01\x00\x00\x00aq\x1eh\x11(X\x00\x00\x00\x00q\x1fNNN\
        NNNtq Rq!tq\".";

    /// `x < y` in Python 3.13 for the values of `VALUES`, row `x`, column
    /// `y`: `T` true, `F` false, `R` raises.
    const LT: [&str; 20] = [
        "RRRRRRRRRRTTTTRRRRRR",
        "RFTFRRRRRRTTTTRRRRRR",
        "RFFFRRRRRRTTTTRRRRRR",
        "RFTFRRRRRRTTTTRRRRRR",
        "RRRRFFRRRRTRRRFFRFFF",
        "RRRRTFRRRRTRRRFFRTTF",
        "RRRRRRFRRRTRRRRRRRRR",
        "RRRRRRRFRRTRRRRRRRRR",
        "RRRRRRRRFRTRRRRRRRRR",
        "RRRRRRRRRRTRRRRRRRRR",
        "FFFFFFFFFFFFFFFFFFFF",
        "FFFFRRRRRRTFFTRRRRRR",
        "FFFFRRRRRRTFFTRRRRRR",
        "FFFFRRRRRRTFFFRRRRRR",
        "RRRRTFRRRRTRRRFFRTTF",
        "RRRRTFRRRRTRRRFFRTTF",
        "RRRRRRRRRRTRRRRRRRRR",
        "RRRRTFRRRRTRRRFFRFFF",
        "RRRRTFRRRRTRRRFFRFFF",
        "RRRRTTRRRRTRRRTTRTTF",
    ];

    /// `x == y` in Python 3.13 for the values of `VALUES`.
    const EQ: [&str; 20] = [
        "TFFFFFFFFFFFFFFFFFFF",
        "FTFTFFFFFFFFFFFFFFFF",
        "FFTFFFFFFFFFFFFFFFFF",
        "FTFTFFFFFFFFFFFFFFFF",
        "FFFFTFFFFFFFFFFFFFFF",
        "FFFFFTFFFFFFFFTTFFFF",
        "FFFFFFTFFFFFFFFFFFFF",
        "FFFFFFFTFFFFFFFFFFFF",
        "FFFFFFFFTFFFFFFFFFFF",
        "FFFFFFFFFTFFFFFFFFFF",
        "FFFFFFFFFFTFFFFFFFFF",
        "FFFFFFFFFFFTFFFFFFFF",
        "FFFFFFFFFFFFTFFFFFFF",
        "FFFFFFFFFFFFFTFFFFFF",
        "FFFFFTFFFFFFFFTTFFFF",
        "FFFFFTFFFFFFFFTTFFFF",
        "FFFFFFFFFFFFFFFFTFFF",
        "FFFFFFFFFFFFFFFFFTTF",
        "FFFFFFFFFFFFFFFFFTTF",
        "FFFFFFFFFFFFFFFFFFFT",
    ];

    /// Returns the answer of a comparison as a letter of `LT` and `EQ`, or
    /// `?` for no answer.
    fn letter(answer: Result<bool, Undecided>) -> char {
        match answer {
            Ok(true) => 'T',
            Ok(false) => 'F',
            Err(Undecided::Raises) => 'R',
            Err(_) => '?',
        }
    }

    #[test]
    fn a_heap_with_values_pickles_them_as_zodbs_resolver_does() {
        let mut heap = Heap::with_values();
        let (class, state) = heap.load(&decode_record(VALUES).unwrap()).unwrap();
        assert_eq!(dump(&heap, class, state), VALUES);
        // The default heap and the heap of instances do not take them so.
        let tree = decode_record(VALUES).unwrap();
        assert_eq!(Heap::new().load(&tree), Err(Unsupported("an opaque node")));
    }

    #[test]
    fn values_compare_as_python_compares_them() {
        let mut heap = Heap::with_values();
        let (_, state) = heap.load(&decode_record(VALUES).unwrap()).unwrap();
        let values = heap.tuple(state).unwrap().to_vec();
        let mut decided = 0;
        for (i, &x) in values.iter().enumerate() {
            for (j, &y) in values.iter().enumerate() {
                let python = (LT[i].as_bytes()[j] as char, EQ[i].as_bytes()[j] as char);
                let ours = (letter(heap.lt(x, y)), letter(heap.eq(x, y)));
                // No answer is always allowed. An answer is the answer of
                // Python.
                for (ours, python) in [(ours.0, python.0), (ours.1, python.1)] {
                    assert!(ours == '?' || ours == python, "{i} {j}: {ours} {python}");
                    decided += usize::from(ours != '?');
                }
            }
        }
        // Each `==` with a DateTime, a Message or Missing.Value has an
        // answer, and each `<` of them that the metadata of a catalog
        // makes.
        let known = [10, 11, 12, 13, 14, 15, 17, 19];
        for &i in &known {
            for j in 0..values.len() {
                assert_ne!(letter(heap.eq(values[i], values[j])), '?', "{i} {j}");
                assert_ne!(letter(heap.eq(values[j], values[i])), '?', "{j} {i}");
            }
        }
        for (i, j) in [
            (10, 11),
            (11, 10),
            (11, 12),
            (12, 13),
            (13, 11),
            (0, 11),
            (11, 0),
            (4, 11),
            (11, 4),
            (14, 5),
            (5, 14),
            (14, 15),
            (14, 10),
            (10, 14),
            (7, 10),
            (4, 10),
            (0, 10),
            (11, 14),
            (14, 11),
            (19, 17),
        ] {
            assert_ne!(letter(heap.lt(values[i], values[j])), '?', "{i} {j}");
        }
        assert!(decided > 500, "{decided}");
    }

    #[test]
    fn a_datetime_keeps_its_zone_and_its_instant() {
        let mut heap = Heap::with_values();
        let (_, state) = heap.load(&decode_record(VALUES).unwrap()).unwrap();
        let values = heap.tuple(state).unwrap().to_vec();
        let (a, a_utc, b) = (values[11], values[12], values[13]);
        // One instant in two zones: not equal, neither is below the other.
        assert_eq!(heap.compare(a, a_utc), Ok(Ordering::Greater));
        assert_eq!(heap.compare(a_utc, a), Ok(Ordering::Greater));
        assert_eq!(heap.compare(a_utc, b), Ok(Ordering::Less));
        // The str of a zone is one object for each name, across records.
        let mut other = Heap::with_values();
        let (_, first) = other.load(&decode_record(VALUES).unwrap()).unwrap();
        let (_, second) = other.load(&decode_record(VALUES).unwrap()).unwrap();
        let zone = |heap: &Heap, value: Value| {
            let state = heap.instance(value).unwrap().state.unwrap();
            heap.tuple(state).unwrap()[2]
        };
        let (x, y) = (
            other.tuple(first).unwrap()[12],
            other.tuple(second).unwrap()[13],
        );
        assert_eq!(zone(&other, x), zone(&other, y));
        assert_ne!(x, y);
    }

    #[test]
    fn a_heap_with_values_refuses_what_it_does_not_model() {
        let date_time = |state: &[u8]| -> Vec<u8> {
            [
                &b"\x80\x03cDateTime.DateTime\nDateTime\n)\x81"[..],
                state,
                b"b.",
            ]
            .concat()
        };
        let instant = b"\x8a\x07\x00@ FHG\x06";
        for (state, what) in [
            (
                date_time(&[&instant[..], b"\x89X\x03\x00\x00\x00EST\x87"].concat()),
                "a DateTime of a zone that is not in the table",
            ),
            (
                date_time(b"G?\xf8\x00\x00\x00\x00\x00\x00\x89X\x03\x00\x00\x00UTC\x87"),
                "a DateTime of another form",
            ),
            (
                date_time(
                    b"\x8a\x08\x00\x00\x00\x00\x00\x00\x00\x40\x89X\x03\x00\x00\x00UTC\x87",
                ),
                "a DateTime out of the range of the model",
            ),
            (date_time(b"}"), "a DateTime of another form"),
            (
                b"\x80\x03cDateTime.DateTime\nDateTime\n)\x81.".to_vec(),
                "a DateTime without a state",
            ),
            (
                b"\x80\x03cDateTime.DateTime\nDateTime\nK\x01\x85\x81.".to_vec(),
                "a NEWOBJ that the model does not model",
            ),
            (
                b"\x80\x03cm\nK\n)\x81.".to_vec(),
                "a NEWOBJ that the model does not model",
            ),
            (
                b"\x80\x03cm\nK\n)\x81}b.".to_vec(),
                "a NEWOBJ that the model does not model",
            ),
            (
                b"\x80\x03cbuiltins\nset\n]\x85R.".to_vec(),
                "a REDUCE of a callable that the model does not model",
            ),
            (b"\x80\x03cMissing\nValue\n.".to_vec(), "a name of Missing other than V"),
            (
                b"\x80\x03czope.i18nmessageid.message\nMessage\n(X\x01\x00\x00\x00aNNNNNNNtR."
                    .to_vec(),
                "a Message of another form",
            ),
            (
                b"\x80\x03czope.i18nmessageid.message\nMessage\n(X\x01\x00\x00\x00aNNNNNX\x01\x00\x00\x00ntR."
                    .to_vec(),
                "a Message of another form",
            ),
            (
                b"\x80\x03czope.i18nmessageid.message\nMessage\n(K\x01tR.".to_vec(),
                "a Message of another form",
            ),
        ] {
            let mut heap = Heap::with_values();
            let record = [CLASS, &state].concat();
            let tree = decode_record(&record).unwrap();
            assert_eq!(heap.load(&tree), Err(Unsupported(what)), "{state:?}");
        }
    }

    #[test]
    fn a_message_pickles_its_mapping_as_a_new_dict() {
        // `(m, m, Message('b', m.domain))` with `m = Message('a', 'd', None,
        // {'n': 1})`, as ZODB 6 writes it with Python 3.13. ZODB's resolver
        // pickles it again to the same bytes.
        let mut heap = Heap::with_values();
        let state = b"\x80\x03czope.i18nmessageid.message\nMessage\nq\x01(X\x01\x00\x00\x00aq\x02\
            X\x01\x00\x00\x00dq\x03N}q\x04X\x01\x00\x00\x00nq\x05K\x01sNNNtq\x06Rq\x07h\x07h\x01\
            (X\x01\x00\x00\x00bq\x08h\x03NNNNNtq\tRq\n\x87q\x0b.";
        let (class, value) = load(&mut heap, state);
        assert_eq!(dump(&heap, class, value), [CLASS, &state[..]].concat());
    }

    #[test]
    fn an_empty_message_keeps_the_shared_empty_str() {
        // `(Message(''), '')`, as ZODB 6 writes it with Python 3.13. ZODB's
        // resolver pickles it again to the same bytes. `Message.__reduce__`
        // gives the shared empty str, so the second `''` is a memo get.
        let mut heap = Heap::with_values();
        let state = b"\x80\x03czope.i18nmessageid.message\nMessage\nq\x01(X\x00\x00\x00\x00q\x02\
            NNNNNNtq\x03Rq\x04h\x02\x86q\x05.";
        let (class, value) = load(&mut heap, state);
        let items = heap.tuple(value).unwrap().to_vec();
        assert_eq!(dump(&heap, class, value), [CLASS, &state[..]].concat());
        let args = heap.instance(items[0]).unwrap().args;
        assert_eq!(heap.tuple(args).unwrap()[0], items[1]);
    }
}
