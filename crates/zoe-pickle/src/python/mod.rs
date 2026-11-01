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

mod canon;
mod compare;
mod known;
mod load;
mod opaque;
mod pickle;

use std::collections::HashMap;

pub use canon::Canon;
pub use compare::{DictKey, KeyItem, Undecided};
pub use pickle::Pickler;

use known::Known;

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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode_record;

    /// The class pickle of a record of `m.C`, as ZODB writes it.
    pub(super) const CLASS: &[u8] = b"\x80\x03cm\nC\nq\x00.";

    /// Loads the record of the class pickle `CLASS` and `state`.
    pub(super) fn load(heap: &mut Heap, state: &[u8]) -> (Value, Value) {
        let record = [CLASS, state].concat();
        heap.load(&decode_record(&record).unwrap()).unwrap()
    }

    /// Pickles the class and the state as ZODB's resolver does.
    pub(super) fn dump(heap: &Heap, class: Value, state: Value) -> Vec<u8> {
        let mut pickler = Pickler::new(heap);
        pickler.dump(class).unwrap();
        pickler.dump(state).unwrap();
        pickler.into_bytes()
    }

    /// The record of a persistent object of `__main__.Doc` with a str, a
    /// `DateTime`, a tuple and a set, as ZODB 6 writes it with Python 3.13.
    pub(super) const DOC: &[u8] = b"\x80\x03c__main__\nDoc\nq\x00.\x80\x03}q\x01(X\x05\x00\x00\x00titleq\x02\
        X\x01\x00\x00\x00aq\x03X\x11\x00\x00\x00modification_dateq\x04cDateTime.DateTime\nDateTime\n\
        q\x05)\x81q\x06\x8a\x07@\xf3L\xf6^G\x06\x89X\x03\x00\x00\x00UTCq\x07\x87q\x08b\
        X\x07\x00\x00\x00subjectq\tX\x01\x00\x00\x00xq\nX\x01\x00\x00\x00yq\x0b\x86q\x0c\
        X\x04\x00\x00\x00tagsq\rcbuiltins\nset\nq\x0e]q\x0f(K\x01K\x02e\x85q\x10Rq\x11u.";

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
}
