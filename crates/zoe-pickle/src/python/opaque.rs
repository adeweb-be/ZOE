//! The small loader of the opcodes of an opaque node, for a heap of
//! [`Heap::with_instances`] or of [`Heap::with_values`].

use std::collections::HashMap;

use super::known::{Known, MESSAGE};
use super::load::Loader;
use super::{DictKey, Heap, Instance, Made, Object, ObjectId, Unsupported, Value};
use crate::opcode::*;
use crate::tree::Int;

impl Heap {
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
    pub(super) fn reduce(&mut self, callable: Value, args: Value) -> Result<Value, Unsupported> {
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
    pub(super) fn opaque(&mut self, bytes: &[u8]) -> Result<Value, Unsupported> {
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
                    let index = memo_index(opcode, arg).ok_or(other)?;
                    let Some(Slot::Value(value, _)) = stack.slots.last() else {
                        return Err(other);
                    };
                    self.memo.insert(index, *value);
                    continue;
                }
                BINGET | LONG_BINGET => {
                    let index = memo_index(opcode, arg).ok_or(other)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode_record;
    use crate::python::tests::{CLASS, DOC, dump, load};

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
}
