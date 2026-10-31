//! The loader of a decoded record into a [`Heap`].

use std::collections::HashMap;

use super::{DictKey, Heap, Object, ObjectId, Unsupported, Value};
use crate::tree::{Bool, Float, Int, Node, Tree, Unicode};

impl Heap {
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
}

/// Builds the objects of one record into a heap.
pub(super) struct Loader<'h> {
    pub(super) heap: &'h mut Heap,
    /// The memo of the unpickler.
    pub(super) memo: HashMap<u32, Value>,
    /// The lists and dicts whose items the loader still reads. A memo get
    /// of one of them makes a container that holds itself.
    pub(super) open: Vec<ObjectId>,
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
}

/// Returns the value of an int node, if it fits 128 bits.
fn int_value(int: &Int) -> Result<i128, Unsupported> {
    int.to_i128()
        .ok_or(Unsupported("an int that does not fit 128 bits"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode_record;
    use crate::python::tests::{CLASS, load};

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
}
