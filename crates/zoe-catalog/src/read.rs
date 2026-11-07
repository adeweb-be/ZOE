//! The reads of the mirror: the store as the mirror sees it, the decode of
//! a record, and the walk of a BTree over several threads.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::sync::{Condvar, Mutex, PoisonError};

use zoe_core::{Flavour, Oid, Store, Tid, ZoeError};
use zoe_pickle::btrees::{self, Class, Shape, TreeState};
use zoe_pickle::python::{Heap, Value};
use zoe_pickle::{Limits, decode_record_with};

use crate::value::{Refused, Val};

/// The durable namespace of the registration keys.
pub const NAMESPACE: &str = "zoe.catalog";

/// One commit of the store: its tid and its records. A record without a
/// pickle is a deletion.
pub struct Commit {
    /// The tid of the commit.
    pub tid: u64,
    /// The oid and the pickle of each record.
    pub records: Vec<(u64, Option<Vec<u8>>)>,
}

/// The reads of a store that the mirror makes. `zoe_core::Store` gives
/// them.
pub trait Reads: Send + Sync {
    /// The tid of the last commit.
    fn last_tid(&self) -> u64;

    /// The pickle of `oid` that was current before `tid`, as `loadBefore`
    /// gives it. `None` when the oid has no record before `tid`, or when
    /// that record is a deletion.
    fn load_before(&self, oid: u64, tid: u64) -> Result<Option<Vec<u8>>, String>;

    /// Calls `each` with each commit from `from` to `to`, both included, in
    /// tid order. Stops when `each` returns false.
    fn commits(
        &self,
        from: u64,
        to: u64,
        each: &mut dyn FnMut(Commit) -> bool,
    ) -> Result<(), String>;

    /// The keys of the durable namespace `zoe.catalog`.
    fn registrations(&self) -> Result<Vec<Vec<u8>>, String>;

    /// The generation of the durable namespace `zoe.catalog`: the tid of
    /// the last commit that wrote it.
    fn registration_generation(&self) -> Result<u64, String>;
}

impl Reads for Store {
    fn last_tid(&self) -> u64 {
        self.last_transaction().0
    }

    fn load_before(&self, oid: u64, tid: u64) -> Result<Option<Vec<u8>>, String> {
        match Store::load_before(self, Oid(oid), Tid(tid)) {
            Ok(found) => Ok(found.map(|record| record.data)),
            Err(ZoeError::NotFound { .. }) => Ok(None),
            Err(error) => Err(format!("load of {oid:#x}: {error}")),
        }
    }

    fn commits(
        &self,
        from: u64,
        to: u64,
        each: &mut dyn FnMut(Commit) -> bool,
    ) -> Result<(), String> {
        let iterator = self
            .iterator(Some(Tid(from)), Some(Tid(to)))
            .map_err(|e| format!("the transactions: {e}"))?;
        for txn in iterator {
            let txn = txn.map_err(|e| format!("a transaction: {e}"))?;
            let commit = Commit {
                tid: txn.tid.0,
                records: txn
                    .records
                    .into_iter()
                    .map(|record| (record.oid.0, record.data))
                    .collect(),
            };
            if !each(commit) {
                break;
            }
        }
        Ok(())
    }

    fn registrations(&self) -> Result<Vec<Vec<u8>>, String> {
        let mut keys = Vec::new();
        let mut start: Option<Vec<u8>> = None;
        loop {
            let page = self
                .kv_scan(Flavour::Durable, NAMESPACE, start.as_deref(), None, 1000)
                .map_err(|e| format!("the registration keys: {e}"))?;
            let Some((last, _)) = page.last() else {
                break;
            };
            let mut next = last.clone();
            next.push(0);
            keys.extend(page.into_iter().map(|(key, _)| key));
            start = Some(next);
        }
        Ok(keys)
    }

    fn registration_generation(&self) -> Result<u64, String> {
        self.kv_generation(NAMESPACE)
            .map(|tid| tid.0)
            .map_err(|e| format!("the generation of {NAMESPACE}: {e}"))
    }
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
        let (_, state) = heap
            .load(&tree)
            .map_err(|e| Refused(format!("load: {e}")))?;
        Ok(Record {
            module,
            name,
            heap,
            state,
        })
    }

    /// Tells if the class of the record is `module.name`.
    pub fn is(&self, module: &str, name: &str) -> bool {
        self.module == module && self.name == name
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

    /// The state as an int, as the state of a `Length` is.
    pub fn int_state(&self) -> Option<i64> {
        match self.state {
            Value::Int(i) => i64::try_from(i).ok(),
            _ => None,
        }
    }
}

/// The oid of a persistent reference.
pub fn oid_of(heap: &Heap, value: Value) -> Option<u64> {
    let reference = heap.reference(value)?;
    if reference.database.is_some() || reference.weak {
        return None;
    }
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
                let children = state
                    .first()
                    .and_then(|c| heap.tuple(*c))
                    .ok_or_else(|| Refused("the children of a tree".into()))?;
                let mut oids = Vec::with_capacity(children.len() / 2 + 1);
                for child in children.iter().step_by(2) {
                    oids.push(
                        oid_of(heap, *child)
                            .ok_or_else(|| Refused("a child without an oid".into()))?,
                    );
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

/// The structure of a tree that the maintenance follows: the children of
/// each inner node, and the first and the last int key of each bucket.
#[derive(Clone, Debug, Default)]
pub struct TreeShape {
    /// The children of each inner node.
    pub inner: HashMap<u64, Vec<u64>>,
    /// The first and the last key of each bucket.
    pub buckets: HashMap<u64, (i64, i64)>,
}

impl TreeShape {
    /// The bytes that the shape takes, estimated.
    pub fn heap_bytes(&self) -> usize {
        self.inner.capacity() * 40
            + self.inner.values().map(|v| v.capacity() * 8).sum::<usize>()
            + self.buckets.capacity() * 32
    }
}

/// What a walk read.
#[derive(Clone, Copy, Debug, Default)]
pub struct WalkStats {
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

/// Walks the tree at `root` as of before `tid` with `threads` threads.
/// Each thread keeps a state that `make` gives, and calls `each` with it
/// for the items of each bucket that it reads. Returns the shape of the
/// tree, the state of each thread and what the walk read. A record that
/// does not decode, or that `each` refuses, fails the walk.
pub fn walk_tree<W, M, F>(
    reads: &dyn Reads,
    tid: u64,
    root: u64,
    threads: usize,
    make: M,
    each: F,
) -> Result<(TreeShape, Vec<W>, WalkStats), String>
where
    W: Send,
    M: Fn() -> W + Sync,
    F: Fn(&mut W, Vec<(Val, Val)>) -> Result<(), String> + Sync,
{
    let queue = Mutex::new(Queue {
        todo: vec![root],
        busy: 0,
        failed: None,
    });
    let ready = Condvar::new();
    let records = AtomicUsize::new(0);
    let bytes = AtomicUsize::new(0);
    let shape = Mutex::new(TreeShape::default());
    let states = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads.max(1))
            .map(|_| {
                let (queue, ready, records, bytes, shape, each, make) =
                    (&queue, &ready, &records, &bytes, &shape, &each, &make);
                scope.spawn(move || {
                    let mut state = make();
                    loop {
                        let oid = {
                            let mut q = queue.lock().unwrap_or_else(PoisonError::into_inner);
                            loop {
                                if q.failed.is_some() {
                                    return state;
                                }
                                if let Some(oid) = q.todo.pop() {
                                    q.busy += 1;
                                    break oid;
                                }
                                if q.busy == 0 {
                                    ready.notify_all();
                                    return state;
                                }
                                q = ready.wait(q).unwrap_or_else(PoisonError::into_inner);
                            }
                        };
                        let result = reads.load_before(oid, tid).and_then(|data| {
                            let data = data.ok_or_else(|| format!("no record {oid:#x}"))?;
                            records.fetch_add(1, AtomicOrdering::Relaxed);
                            bytes.fetch_add(data.len(), AtomicOrdering::Relaxed);
                            let record = Record::decode(&data).map_err(|e| e.0)?;
                            node(&record).map_err(|e| e.0)
                        });
                        let result = match result {
                            Ok(Node::Items(items)) => {
                                if let (Some(first), Some(last)) = (items.first(), items.last())
                                    && let (Some(a), Some(b)) = (first.0.int(), last.0.int())
                                {
                                    shape
                                        .lock()
                                        .unwrap_or_else(PoisonError::into_inner)
                                        .buckets
                                        .insert(oid, (a, b));
                                }
                                each(&mut state, items).map(|()| Vec::new())
                            }
                            Ok(Node::Inner(children)) => {
                                shape
                                    .lock()
                                    .unwrap_or_else(PoisonError::into_inner)
                                    .inner
                                    .insert(oid, children.clone());
                                Ok(children)
                            }
                            Err(e) => Err(e),
                        };
                        let mut q = queue.lock().unwrap_or_else(PoisonError::into_inner);
                        q.busy -= 1;
                        match result {
                            Ok(children) => q.todo.extend(children),
                            Err(e) => {
                                q.failed.get_or_insert(format!("oid {oid:#x}: {e}"));
                            }
                        }
                        ready.notify_all();
                    }
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join())
            .collect::<Result<Vec<W>, _>>()
    });
    let states = states.map_err(|_| "a thread of the walk panicked".to_owned())?;
    let q = queue.into_inner().unwrap_or_else(PoisonError::into_inner);
    if let Some(e) = q.failed {
        return Err(e);
    }
    let stats = WalkStats {
        records: records.into_inner(),
        bytes: bytes.into_inner(),
    };
    Ok((
        shape.into_inner().unwrap_or_else(PoisonError::into_inner),
        states,
        stats,
    ))
}
