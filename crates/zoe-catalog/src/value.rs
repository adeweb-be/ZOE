//! The values that the mirror keeps: owned copies of the values of a
//! decoded record, their order as Python orders them, and the encoding of a
//! metadata cell.

use std::cmp::Ordering;

use zoe_pickle::python::{Heap, Made, Object, Value};

/// The deepest value that the mirror reads. A deeper value is refused.
const DEPTH: usize = 32;

/// An owned value of a pickle.
#[derive(Clone, Debug, PartialEq)]
pub enum Val {
    /// `None`.
    None,
    /// `Missing.Value`.
    Missing,
    /// A bool.
    Bool(bool),
    /// An int that fits in 64 bits.
    Int(i64),
    /// A float.
    Float(f64),
    /// A str.
    Str(Box<str>),
    /// A bytes.
    Bytes(Box<[u8]>),
    /// A tuple.
    Tuple(Box<[Val]>),
    /// A list.
    List(Box<[Val]>),
    /// A dict, in the order of its items.
    Dict(Box<[(Val, Val)]>),
    /// A `DateTime`: `_micros`, `_timezone_naive` and `_tz`.
    DateTime(i64, bool, Box<str>),
    /// A `Message`: the arguments of its `__reduce__`. The first is its
    /// text. It compares as a str.
    Message(Box<[Val]>),
}

/// A value that the mirror does not model. The text names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refused(pub String);

impl Val {
    /// Reads `value` of `heap` into an owned value.
    pub fn read(heap: &Heap, value: Value) -> Result<Val, Refused> {
        Self::read_at(heap, value, 0)
    }

    fn read_at(heap: &Heap, value: Value, depth: usize) -> Result<Val, Refused> {
        if depth > DEPTH {
            return Err(Refused("a value nested too deep".into()));
        }
        let read_all = |items: &[Value]| -> Result<Box<[Val]>, Refused> {
            items
                .iter()
                .map(|item| Self::read_at(heap, *item, depth + 1))
                .collect()
        };
        Ok(match value {
            Value::None => Val::None,
            Value::Bool(b) => Val::Bool(b),
            Value::Int(i) => Val::Int(
                i64::try_from(i).map_err(|_| Refused("an int beyond 64 bits".into()))?,
            ),
            Value::Float(f) => Val::Float(f),
            Value::Object(id) => match heap.get(id) {
                Object::Str(s) => Val::Str(s.as_str().into()),
                Object::Bytes(b) => Val::Bytes(b.as_slice().into()),
                Object::Tuple(items) => Val::Tuple(read_all(items)?),
                Object::List(items) => Val::List(read_all(items)?),
                Object::Dict(items) => Val::Dict(
                    items
                        .iter()
                        .map(|(k, v)| {
                            Ok((
                                Self::read_at(heap, *k, depth + 1)?,
                                Self::read_at(heap, *v, depth + 1)?,
                            ))
                        })
                        .collect::<Result<_, Refused>>()?,
                ),
                Object::Class { module, name } if module == "Missing" && name == "V" => {
                    Val::Missing
                }
                Object::Instance(instance) => {
                    let class = heap.class(instance.class);
                    match (class, instance.made) {
                        (Some(("DateTime.DateTime", "DateTime")), Made::New) => {
                            let state = instance
                                .state
                                .and_then(|s| heap.tuple(s))
                                .ok_or_else(|| Refused("a DateTime without state".into()))?;
                            match *state {
                                [Value::Int(micros), Value::Bool(naive), tz] => Val::DateTime(
                                    i64::try_from(micros)
                                        .map_err(|_| Refused("a DateTime out of range".into()))?,
                                    naive,
                                    heap.str(tz)
                                        .ok_or_else(|| Refused("a DateTime zone".into()))?
                                        .into(),
                                ),
                                _ => return Err(Refused("a DateTime state".into())),
                            }
                        }
                        (Some(("zope.i18nmessageid.message", "Message")), Made::Reduce) => {
                            let args = heap
                                .tuple(instance.args)
                                .ok_or_else(|| Refused("the arguments of a Message".into()))?;
                            Val::Message(read_all(args)?)
                        }
                        _ => return Err(Refused(format!("an instance of {class:?}"))),
                    }
                }
                Object::Reference(_) => return Err(Refused("a persistent reference".into())),
                other => return Err(Refused(format!("an object {}", kind(other)))),
            },
        })
    }

    /// The text of a str or of a `Message`.
    pub fn text(&self) -> Option<&str> {
        match self {
            Val::Str(s) => Some(s),
            Val::Message(args) => match args.first() {
                Some(Val::Str(s)) => Some(s),
                _ => None,
            },
            _ => None,
        }
    }

    /// The number of a bool or an int.
    pub fn int(&self) -> Option<i64> {
        match self {
            Val::Bool(b) => Some(i64::from(*b)),
            Val::Int(i) => Some(*i),
            _ => None,
        }
    }

    /// Python's truth value of this value, where the model knows it.
    pub fn truthy(&self) -> bool {
        match self {
            Val::None | Val::Missing => false,
            Val::Bool(b) => *b,
            Val::Int(i) => *i != 0,
            Val::Float(f) => *f != 0.0,
            Val::Str(s) => !s.is_empty(),
            Val::Bytes(b) => !b.is_empty(),
            Val::Tuple(v) | Val::List(v) => !v.is_empty(),
            Val::Dict(v) => !v.is_empty(),
            Val::DateTime(..) => true,
            Val::Message(_) => self.text().is_some_and(|t| !t.is_empty()),
        }
    }

    /// The bytes of this value on the heap, its own size excluded.
    pub fn heap_bytes(&self) -> usize {
        match self {
            Val::Str(s) => s.len(),
            Val::Bytes(b) => b.len(),
            Val::Tuple(v) | Val::List(v) | Val::Message(v) => {
                v.len() * std::mem::size_of::<Val>() + v.iter().map(Val::heap_bytes).sum::<usize>()
            }
            Val::Dict(v) => {
                v.len() * 2 * std::mem::size_of::<Val>()
                    + v.iter()
                        .map(|(k, x)| k.heap_bytes() + x.heap_bytes())
                        .sum::<usize>()
            }
            Val::DateTime(_, _, tz) => tz.len(),
            _ => 0,
        }
    }
}

fn kind(object: &Object) -> &'static str {
    match object {
        Object::Str(_) => "str",
        Object::Bytes(_) => "bytes",
        Object::Tuple(_) => "tuple",
        Object::List(_) => "list",
        Object::Dict(_) => "dict",
        Object::Class { .. } => "class",
        Object::Reference(_) => "reference",
        Object::Instance(_) => "instance",
        Object::Set { .. } => "set",
    }
}

/// Compares two values as Python's `<` and `==` order them, for the keys of
/// one index. `None` when Python raises: two values of two kinds, or `None`.
pub fn py_cmp(a: &Val, b: &Val) -> Option<Ordering> {
    match (a, b) {
        (Val::Bool(_) | Val::Int(_), Val::Bool(_) | Val::Int(_)) => {
            Some(a.int()?.cmp(&b.int()?))
        }
        (Val::Float(x), Val::Float(y)) => x.partial_cmp(y),
        (Val::Float(x), _) if b.int().is_some() => x.partial_cmp(&(b.int()? as f64)),
        (_, Val::Float(y)) if a.int().is_some() => (a.int()? as f64).partial_cmp(y),
        (Val::Str(_) | Val::Message(_), Val::Str(_) | Val::Message(_)) => {
            Some(a.text()?.cmp(b.text()?))
        }
        (Val::Bytes(x), Val::Bytes(y)) => Some(x.cmp(y)),
        (Val::Tuple(x), Val::Tuple(y)) | (Val::List(x), Val::List(y)) => seq_cmp(x, y),
        (Val::DateTime(x, ..), Val::DateTime(y, ..)) => Some(x.cmp(y)),
        _ => None,
    }
}

fn seq_cmp(x: &[Val], y: &[Val]) -> Option<Ordering> {
    for (a, b) in x.iter().zip(y) {
        match py_cmp(a, b)? {
            Ordering::Equal => continue,
            other => return Some(other),
        }
    }
    Some(x.len().cmp(&y.len()))
}

/// The tags of the encoding of a metadata cell.
pub mod tag {
    /// `None`.
    pub const NONE: u8 = 0;
    /// `Missing.Value`.
    pub const MISSING: u8 = 1;
    /// False.
    pub const FALSE: u8 = 2;
    /// True.
    pub const TRUE: u8 = 3;
    /// An int: 8 bytes, little endian.
    pub const INT: u8 = 4;
    /// A float: 8 bytes, little endian.
    pub const FLOAT: u8 = 5;
    /// A str: its length (4 bytes) and its UTF-8 bytes.
    pub const STR: u8 = 6;
    /// A bytes: its length (4 bytes) and the bytes.
    pub const BYTES: u8 = 7;
    /// A tuple: its length (4 bytes) and its cells.
    pub const TUPLE: u8 = 8;
    /// A list: its length (4 bytes) and its cells.
    pub const LIST: u8 = 9;
    /// A dict: its length (4 bytes), then each key and value.
    pub const DICT: u8 = 10;
    /// A `DateTime`: `_micros` (8 bytes), the naive flag (1 byte), `_tz` as a
    /// str cell.
    pub const DATETIME: u8 = 11;
    /// A `Message`: its arguments as a tuple cell.
    pub const MESSAGE: u8 = 12;
}

/// Appends the encoding of `value` as a metadata cell to `out`.
pub fn encode_cell(value: &Val, out: &mut Vec<u8>) {
    let len = |out: &mut Vec<u8>, n: usize| out.extend_from_slice(&(n as u32).to_le_bytes());
    match value {
        Val::None => out.push(tag::NONE),
        Val::Missing => out.push(tag::MISSING),
        Val::Bool(false) => out.push(tag::FALSE),
        Val::Bool(true) => out.push(tag::TRUE),
        Val::Int(i) => {
            out.push(tag::INT);
            out.extend_from_slice(&i.to_le_bytes());
        }
        Val::Float(f) => {
            out.push(tag::FLOAT);
            out.extend_from_slice(&f.to_le_bytes());
        }
        Val::Str(s) => {
            out.push(tag::STR);
            len(out, s.len());
            out.extend_from_slice(s.as_bytes());
        }
        Val::Bytes(b) => {
            out.push(tag::BYTES);
            len(out, b.len());
            out.extend_from_slice(b);
        }
        Val::Tuple(items) | Val::List(items) => {
            out.push(if matches!(value, Val::Tuple(_)) {
                tag::TUPLE
            } else {
                tag::LIST
            });
            len(out, items.len());
            for item in items.iter() {
                encode_cell(item, out);
            }
        }
        Val::Dict(items) => {
            out.push(tag::DICT);
            len(out, items.len());
            for (k, v) in items.iter() {
                encode_cell(k, out);
                encode_cell(v, out);
            }
        }
        Val::DateTime(micros, naive, tz) => {
            out.push(tag::DATETIME);
            out.extend_from_slice(&micros.to_le_bytes());
            out.push(u8::from(*naive));
            encode_cell(&Val::Str(tz.clone()), out);
        }
        Val::Message(args) => {
            out.push(tag::MESSAGE);
            encode_cell(&Val::Tuple(args.clone()), out);
        }
    }
}

/// An arena of strs, each found by its place.
#[derive(Clone, Debug, Default)]
pub struct StrArena {
    bytes: Vec<u8>,
    ends: Vec<u32>,
}

impl StrArena {
    /// Appends `text` and returns its place.
    pub fn push(&mut self, text: &str) -> u32 {
        self.bytes.extend_from_slice(text.as_bytes());
        self.ends.push(self.bytes.len() as u32);
        (self.ends.len() - 1) as u32
    }

    /// The str at `place`.
    pub fn get(&self, place: u32) -> &str {
        let place = place as usize;
        let start = if place == 0 {
            0
        } else {
            self.ends[place - 1] as usize
        };
        let end = self.ends[place] as usize;
        std::str::from_utf8(&self.bytes[start..end]).unwrap_or("")
    }

    /// The number of strs.
    pub fn len(&self) -> usize {
        self.ends.len()
    }

    /// Tells if the arena holds no str.
    pub fn is_empty(&self) -> bool {
        self.ends.is_empty()
    }

    /// The bytes that the arena takes.
    pub fn heap_bytes(&self) -> usize {
        self.bytes.capacity() + self.ends.capacity() * 4
    }

    /// Releases the spare capacity.
    pub fn shrink(&mut self) {
        self.bytes.shrink_to_fit();
        self.ends.shrink_to_fit();
    }
}

/// An arena of byte strings, each found by its place.
#[derive(Clone, Debug, Default)]
pub struct BytesArena {
    bytes: Vec<u8>,
    ends: Vec<u32>,
}

impl BytesArena {
    /// Appends `data` and returns its place.
    pub fn push(&mut self, data: &[u8]) -> u32 {
        self.bytes.extend_from_slice(data);
        self.ends.push(self.bytes.len() as u32);
        (self.ends.len() - 1) as u32
    }

    /// The bytes at `place`.
    pub fn get(&self, place: u32) -> &[u8] {
        let place = place as usize;
        let start = if place == 0 {
            0
        } else {
            self.ends[place - 1] as usize
        };
        &self.bytes[start..self.ends[place] as usize]
    }

    /// The number of byte strings.
    pub fn len(&self) -> usize {
        self.ends.len()
    }

    /// Tells if the arena holds nothing.
    pub fn is_empty(&self) -> bool {
        self.ends.is_empty()
    }

    /// The bytes that the arena takes.
    pub fn heap_bytes(&self) -> usize {
        self.bytes.capacity() + self.ends.capacity() * 4
    }

    /// Releases the spare capacity.
    pub fn shrink(&mut self) {
        self.bytes.shrink_to_fit();
        self.ends.shrink_to_fit();
    }
}
