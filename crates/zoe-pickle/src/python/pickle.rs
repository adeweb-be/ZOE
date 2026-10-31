//! The pickler of the objects of a [`Heap`], as the C pickler of
//! zodbpickle writes them with protocol 3.

use std::collections::HashMap;

use super::{Heap, Made, Object, ObjectId, Unsupported, Value};
use crate::opcode::*;

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
    use crate::python::tests::{CLASS, dump, load};

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
}
