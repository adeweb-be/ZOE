//! The values that the mirror keeps: owned copies of the keys of a decoded
//! record, their order as Python orders them, and an arena of strs.

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
            Value::Int(i) => {
                Val::Int(i64::try_from(i).map_err(|_| Refused("an int beyond 64 bits".into()))?)
            }
            Value::Float(f) => Val::Float(f),
            Value::Object(id) => match heap.get(id) {
                Object::Str(s) => Val::Str(s.as_str().into()),
                Object::Bytes(b) => Val::Bytes(b.as_slice().into()),
                Object::Tuple(items) => Val::Tuple(read_all(items)?),
                Object::List(items) => Val::List(read_all(items)?),
                Object::Class { module, name } if module == "Missing" && name == "V" => {
                    Val::Missing
                }
                Object::Instance(instance) => match (heap.class(instance.class), instance.made) {
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
                    (class, _) => return Err(Refused(format!("an instance of {class:?}"))),
                },
                Object::Reference(_) => return Err(Refused("a persistent reference".into())),
                Object::Dict(_) => return Err(Refused("a dict".into())),
                Object::Class { .. } => return Err(Refused("a class".into())),
                Object::Set { .. } => return Err(Refused("a set".into())),
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

    /// The bytes of this value on the heap, its own size excluded.
    pub fn heap_bytes(&self) -> usize {
        match self {
            Val::Str(s) => s.len(),
            Val::Bytes(b) => b.len(),
            Val::Tuple(v) | Val::List(v) | Val::Message(v) => {
                v.len() * std::mem::size_of::<Val>() + v.iter().map(Val::heap_bytes).sum::<usize>()
            }
            Val::DateTime(_, _, tz) => tz.len(),
            _ => 0,
        }
    }
}

/// Compares two values as Python's `<` and `==` order them, for the keys of
/// one index. `None` when Python raises, or when the model does not know
/// the answer: two values of two kinds, or `None`.
pub fn py_cmp(a: &Val, b: &Val) -> Option<Ordering> {
    match (a, b) {
        (Val::Bool(_) | Val::Int(_), Val::Bool(_) | Val::Int(_)) => Some(a.int()?.cmp(&b.int()?)),
        (Val::Float(x), Val::Float(y)) => x.partial_cmp(y),
        (Val::Float(x), _) if b.int().is_some() => int_float(b.int()?, *x).map(Ordering::reverse),
        (_, Val::Float(y)) if a.int().is_some() => int_float(a.int()?, *y),
        (Val::Str(_) | Val::Message(_), Val::Str(_) | Val::Message(_)) => {
            Some(a.text()?.cmp(b.text()?))
        }
        (Val::Bytes(x), Val::Bytes(y)) => Some(x.cmp(y)),
        (Val::Tuple(x), Val::Tuple(y)) | (Val::List(x), Val::List(y)) => seq_cmp(x, y),
        (Val::DateTime(x, ..), Val::DateTime(y, ..)) => Some(x.cmp(y)),
        _ => None,
    }
}

/// Compares an int with a float as Python does. `None` for a NaN, and for
/// an int that a float does not hold exactly.
fn int_float(i: i64, f: f64) -> Option<Ordering> {
    let as_float = i as f64;
    if as_float as i64 != i {
        return None;
    }
    as_float.partial_cmp(&f)
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
        // Each span holds the bytes of one str, so it is UTF-8.
        std::str::from_utf8(&self.bytes[start..end]).unwrap_or_default()
    }

    /// The number of strs.
    pub fn len(&self) -> usize {
        self.ends.len()
    }

    /// Tells if the arena holds no str.
    pub fn is_empty(&self) -> bool {
        self.ends.is_empty()
    }

    /// Appends each str of `other`.
    pub fn extend(&mut self, other: &StrArena) {
        let offset = self.bytes.len() as u32;
        self.bytes.extend_from_slice(&other.bytes);
        self.ends.extend(other.ends.iter().map(|end| end + offset));
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
