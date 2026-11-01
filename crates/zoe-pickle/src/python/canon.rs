//! The canonical ids of the values of a [`Heap`]: one id for each
//! structure.

use std::collections::HashMap;

use super::{Heap, Made, Object, ObjectId, Undecided, Value};

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode_record;
    use crate::python::tests::DOC;

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
}
