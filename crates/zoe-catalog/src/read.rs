//! The reads of the mirror: a source of records, the decode of a record,
//! and the walk of a BTree over several threads.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::sync::{Condvar, Mutex};

use zoe_pickle::btrees::{self, Class, Shape, TreeState};
use zoe_pickle::python::{Heap, Object, Value};
use zoe_pickle::{Limits, decode_record_with};

use crate::value::{Refused, Val};

/// The records of a store, as of one tid.
pub trait Source: Sync {
    /// The pickle of `oid`.
    fn load(&self, oid: u64) -> Result<Vec<u8>, String>;
}

/// A decoded record: the class and the state on a heap of
/// `Heap::with_values`.
pub struct Record {
    /// The module of the class.
    pub module: String,
    /// The name of the class.
    pub name: String,
    /// The heap of the state.
    pub heap: Heap,
    /// The class value.
    pub class: Value,
    /// The state.
    pub state: Value,
}

impl Record {
    /// Decodes `bytes`.
    pub fn decode(bytes: &[u8]) -> Result<Record, Refused> {
        let tree = decode_record_with(
            bytes,
            Limits {
                node_budget: bytes.len().max(64),
            },
        )
        .map_err(|e| Refused(format!("decode: {e}")))?;
        let (module, name) = tree
            .class_name()
            .map(|(m, n)| (m.to_owned(), n.to_owned()))
            .ok_or_else(|| Refused("a record without a class".into()))?;
        let mut heap = Heap::with_values();
        let (class, state) = heap
            .load(&tree)
            .map_err(|e| Refused(format!("load: {e}")))?;
        Ok(Record {
            module,
            name,
            heap,
            class,
            state,
        })
    }

    /// The items of the state when it is a dict.
    pub fn dict(&self) -> Option<&[(Value, Value)]> {
        self.heap.dict(self.state)
    }

    /// The value of the key `name` in the dict state.
    pub fn attr(&self, name: &str) -> Option<Value> {
        self.dict()?
            .iter()
            .find(|(k, _)| self.heap.str(*k) == Some(name))
            .map(|(_, v)| *v)
    }

    /// The oid of the persistent reference at the key `name`.
    pub fn attr_oid(&self, name: &str) -> Option<u64> {
        oid_of(&self.heap, self.attr(name)?)
    }
}

/// The oid of a persistent reference.
pub fn oid_of(heap: &Heap, value: Value) -> Option<u64> {
    let reference = heap.reference(value)?;
    let bytes: [u8; 8] = reference.oid.as_slice().try_into().ok()?;
    Some(u64::from_be_bytes(bytes))
}

/// What a node of a BTree holds.
pub enum Node {
    /// An inner node: its children.
    Inner(Vec<u64>),
    /// A bucket, or a tree of one inline bucket: its items. For a set,
    /// each value is `None`.
    Items(Vec<(Val, Val)>),
}

/// Reads the node of a BTree in `record`.
pub fn node(record: &Record) -> Result<Node, Refused> {
    let class = btrees::classify(&record.module, &record.name)
        .ok_or_else(|| Refused(format!("not a BTree: {}.{}", record.module, record.name)))?;
    let Class::Family(shape, _) = class else {
        return Err(Refused("a Length in a tree".into()));
    };
    let heap = &record.heap;
    let mapping = shape.is_mapping();
    match shape {
        Shape::Bucket | Shape::Set => {
            let state = btrees::bucket_state(heap, record.state, mapping)
                .ok_or_else(|| Refused("a bucket state".into()))?;
            Ok(Node::Items(items(heap, &state.items, mapping)?))
        }
        Shape::Tree | Shape::TreeSet => match btrees::tree_state(heap, record.state)
            .ok_or_else(|| Refused("a tree state".into()))?
        {
            TreeState::Empty => Ok(Node::Items(Vec::new())),
            TreeState::Inline(bucket) => {
                let state = btrees::bucket_state(heap, bucket, mapping)
                    .ok_or_else(|| Refused("an inline bucket".into()))?;
                Ok(Node::Items(items(heap, &state.items, mapping)?))
            }
            TreeState::Split => {
                let state = heap.tuple(record.state).unwrap_or(&[]);
                let children = heap
                    .tuple(state[0])
                    .ok_or_else(|| Refused("the children of a tree".into()))?;
                let mut oids = Vec::with_capacity(children.len() / 2 + 1);
                for (i, child) in children.iter().enumerate() {
                    if i % 2 == 0 {
                        oids.push(
                            oid_of(heap, *child)
                                .ok_or_else(|| Refused("a child without an oid".into()))?,
                        );
                    }
                }
                Ok(Node::Inner(oids))
            }
        },
    }
}

fn items(heap: &Heap, flat: &[Value], mapping: bool) -> Result<Vec<(Val, Val)>, Refused> {
    let step = if mapping { 2 } else { 1 };
    let mut out = Vec::with_capacity(flat.len() / step);
    for pair in flat.chunks(step) {
        let key = Val::read(heap, pair[0])?;
        let value = if mapping {
            Val::read(heap, pair[1])?
        } else {
            Val::None
        };
        out.push((key, value));
    }
    Ok(out)
}

/// What a walk of a tree gives.
#[derive(Default)]
pub struct Walk {
    /// Each item of the tree, in no order.
    pub items: Vec<(Val, Val)>,
    /// The children of each inner node, by oid.
    pub inner: HashMap<u64, Vec<u64>>,
    /// The first and last key of each bucket, by oid, when the keys are
    /// ints.
    pub buckets: HashMap<u64, (i64, i64)>,
    /// The records read.
    pub records: usize,
    /// The bytes of the records read.
    pub bytes: usize,
}

struct Queue {
    todo: Vec<u64>,
    busy: usize,
    failed: Option<String>,
}

/// Walks the tree at `root` with `threads` threads, and calls `each` on
/// the items of each bucket in the thread that read it. Returns the
/// structure of the tree, with no items.
pub fn walk_tree<F>(source: &dyn Source, root: u64, threads: usize, each: F) -> Result<Walk, String>
where
    F: Fn(usize, Vec<(Val, Val)>) + Sync,
{
    let queue = Mutex::new(Queue {
        todo: vec![root],
        busy: 0,
        failed: None,
    });
    let ready = Condvar::new();
    let records = AtomicUsize::new(0);
    let bytes = AtomicUsize::new(0);
    let shapes: Mutex<(HashMap<u64, Vec<u64>>, HashMap<u64, (i64, i64)>)> =
        Mutex::new((HashMap::new(), HashMap::new()));
    let threads = threads.max(1);
    std::thread::scope(|scope| {
        for worker in 0..threads {
            let (queue, ready, records, bytes, shapes, each) =
                (&queue, &ready, &records, &bytes, &shapes, &each);
            scope.spawn(move || {
                loop {
                    let oid = {
                        let mut q = queue.lock().unwrap();
                        loop {
                            if q.failed.is_some() {
                                return;
                            }
                            if let Some(oid) = q.todo.pop() {
                                q.busy += 1;
                                break oid;
                            }
                            if q.busy == 0 {
                                ready.notify_all();
                                return;
                            }
                            q = ready.wait(q).unwrap();
                        }
                    };
                    let result = source.load(oid).and_then(|data| {
                        records.fetch_add(1, AtomicOrdering::Relaxed);
                        bytes.fetch_add(data.len(), AtomicOrdering::Relaxed);
                        let record = Record::decode(&data).map_err(|e| e.0)?;
                        node(&record).map_err(|e| e.0)
                    });
                    let mut q = queue.lock().unwrap();
                    q.busy -= 1;
                    match result {
                        Ok(Node::Inner(children)) => {
                            q.todo.extend(children.iter().copied());
                            shapes.lock().unwrap().0.insert(oid, children);
                        }
                        Ok(Node::Items(items)) => {
                            drop(q);
                            if let (Some(first), Some(last)) = (items.first(), items.last())
                                && let (Some(a), Some(b)) = (first.0.int(), last.0.int())
                            {
                                shapes.lock().unwrap().1.insert(oid, (a, b));
                            }
                            each(worker, items);
                            q = queue.lock().unwrap();
                        }
                        Err(e) => q.failed = Some(format!("oid {oid:#x}: {e}")),
                    }
                    ready.notify_all();
                    drop(q);
                }
            });
        }
    });
    let q = queue.into_inner().unwrap();
    if let Some(e) = q.failed {
        return Err(e);
    }
    let (inner, buckets) = shapes.into_inner().unwrap();
    Ok(Walk {
        items: Vec::new(),
        inner,
        buckets,
        records: records.into_inner(),
        bytes: bytes.into_inner(),
    })
}

/// Collects the items of the tree at `root`.
pub fn collect_tree(source: &dyn Source, root: u64, threads: usize) -> Result<Walk, String> {
    let parts: Vec<Mutex<Vec<(Val, Val)>>> =
        (0..threads.max(1)).map(|_| Mutex::new(Vec::new())).collect();
    let mut walk = walk_tree(source, root, threads, |worker, items| {
        parts[worker].lock().unwrap().extend(items);
    })?;
    let total = parts.iter().map(|p| p.lock().unwrap().len()).sum();
    let mut all = Vec::with_capacity(total);
    for part in parts {
        all.extend(part.into_inner().unwrap());
    }
    walk.items = all;
    Ok(walk)
}

/// Reads the dict items of a record as names and values, for a summary.
pub fn summary(record: &Record) -> Vec<(String, String)> {
    let heap = &record.heap;
    let Some(dict) = record.dict() else {
        return vec![("state".into(), format!("{:?}", heap.object(record.state)))];
    };
    dict.iter()
        .map(|(k, v)| {
            let key = heap.str(*k).unwrap_or("?").to_owned();
            let value = if let Some(oid) = oid_of(heap, *v) {
                format!("ref {oid:#x}")
            } else {
                match heap.object(*v) {
                    Some(Object::Str(s)) => format!("{s:?}"),
                    Some(Object::Dict(items)) => format!("dict of {}", items.len()),
                    Some(Object::Tuple(items)) => format!("tuple of {}", items.len()),
                    Some(Object::List(items)) => format!("list of {}", items.len()),
                    Some(other) => format!("{other:?}").chars().take(80).collect(),
                    None => format!("{v:?}"),
                }
            };
            (key, value)
        })
        .collect()
}
