//! The records of a catalog as Products.ZCatalog writes them, for the tests
//! of this crate and of the crates that serve the mirror. The feature
//! `testing` builds this module.
//!
//! - `Pickle` writes a record as ZODB's `ObjectWriter` does with the C
//!   pickler of zodbpickle and protocol 3: one memo for the class pickle
//!   and the state pickle, a new tuple `(oid, class)` for each persistent
//!   reference, and the oid bytes and the class shared in the record.
//! - The record writers (`catalog_record`, `index_record`, `length_record`,
//!   `tree_record`, `bucket_record`) give the records of the classes that
//!   a catalog holds. A test compares them with records that CPython wrote.
//! - `CatalogModel` keeps a catalog with its indexes and its trees, as
//!   `Catalog.catalogObject` and `uncatalogObject` change them, and gives
//!   the records that changed since the last call. Its trees split a
//!   bucket as BTrees does: at half of its items, past the largest bucket
//!   of the family. Its tree sets stay one inline bucket, and its trees
//!   never split an inner node: the catalogs of the tests stay small.
//! - `commit` writes records and registration keys to a store.

use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap};

use zoe_core::{Flavour, Oid, Store, Tid, ZoeError};

use crate::read::NAMESPACE;
use crate::value::{Val, py_cmp};

/// A value of a pickle: an atom, or an object with an identity.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Py {
    /// `None`.
    None,
    /// A bool.
    Bool(bool),
    /// An int.
    Int(i64),
    /// A float.
    Float(f64),
    /// An object of the record.
    Obj(usize),
}

#[derive(Debug)]
enum Obj {
    Str(String),
    Bytes(Vec<u8>),
    Tuple(Vec<Py>),
    List(Vec<Py>),
    Dict(Vec<(Py, Py)>),
    Global(String, String),
    /// A persistent reference: its persistent id, then BINPERSID.
    Pid(Py),
}

/// The objects of one record, and the pickler that writes it.
#[derive(Debug, Default)]
pub struct Pickle {
    objs: Vec<Obj>,
    classes: HashMap<(String, String), usize>,
    oids: HashMap<u64, usize>,
}

impl Pickle {
    fn push(&mut self, obj: Obj) -> Py {
        self.objs.push(obj);
        Py::Obj(self.objs.len() - 1)
    }

    /// A new str.
    pub fn str(&mut self, text: &str) -> Py {
        self.push(Obj::Str(text.to_owned()))
    }

    /// A new tuple.
    pub fn tuple(&mut self, items: Vec<Py>) -> Py {
        self.push(Obj::Tuple(items))
    }

    /// A new list.
    pub fn list(&mut self, items: Vec<Py>) -> Py {
        self.push(Obj::List(items))
    }

    /// A new dict.
    pub fn dict(&mut self, items: Vec<(Py, Py)>) -> Py {
        self.push(Obj::Dict(items))
    }

    /// The class `module.name`: one object for each class in the record.
    pub fn class(&mut self, module: &str, name: &str) -> Py {
        let key = (module.to_owned(), name.to_owned());
        if let Some(&i) = self.classes.get(&key) {
            return Py::Obj(i);
        }
        self.objs.push(Obj::Global(key.0.clone(), key.1.clone()));
        let i = self.objs.len() - 1;
        self.classes.insert(key, i);
        Py::Obj(i)
    }

    /// A persistent reference to `oid` of the class `module.name`: a new
    /// tuple, with the oid bytes of the record.
    pub fn reference(&mut self, oid: u64, module: &str, name: &str) -> Py {
        let bytes = match self.oids.get(&oid) {
            Some(&i) => Py::Obj(i),
            None => {
                self.objs.push(Obj::Bytes(oid.to_be_bytes().to_vec()));
                let i = self.objs.len() - 1;
                self.oids.insert(oid, i);
                Py::Obj(i)
            }
        };
        let class = self.class(module, name);
        let id = self.tuple(vec![bytes, class]);
        self.push(Obj::Pid(id))
    }

    /// A value of the model.
    pub fn val(&mut self, value: &Val) -> Py {
        match value {
            Val::None => Py::None,
            Val::Bool(b) => Py::Bool(*b),
            Val::Int(i) => Py::Int(*i),
            Val::Float(f) => Py::Float(*f),
            Val::Str(s) => self.str(s),
            Val::Bytes(b) => self.push(Obj::Bytes(b.to_vec())),
            Val::Tuple(items) => {
                let items = items.iter().map(|v| self.val(v)).collect();
                self.tuple(items)
            }
            Val::List(items) => {
                let items = items.iter().map(|v| self.val(v)).collect();
                self.list(items)
            }
            Val::Missing => self.class("Missing", "V"),
            Val::DateTime(..) | Val::Message(_) => {
                panic!("the testing pickler writes no DateTime and no Message")
            }
        }
    }

    /// The record of the class `module.name` with `state`.
    pub fn record(mut self, module: &str, name: &str, state: Py) -> Vec<u8> {
        let class = self.class(module, name);
        let mut writer = Writer {
            objs: &self.objs,
            memo: HashMap::new(),
            out: Vec::new(),
        };
        writer.dump(class);
        writer.dump(state);
        writer.out
    }
}

struct Writer<'a> {
    objs: &'a [Obj],
    memo: HashMap<usize, u32>,
    out: Vec<u8>,
}

impl Writer<'_> {
    fn dump(&mut self, value: Py) {
        self.out.extend([0x80, 3]);
        self.save(value);
        self.out.push(b'.');
    }

    fn memoize(&mut self, id: usize) {
        let n = self.memo.len() as u32;
        self.memo.insert(id, n);
        if n < 256 {
            self.out.extend([b'q', n as u8]);
        } else {
            self.out.push(b'r');
            self.out.extend(n.to_le_bytes());
        }
    }

    fn save(&mut self, value: Py) {
        let id = match value {
            Py::None => return self.out.push(b'N'),
            Py::Bool(true) => return self.out.push(0x88),
            Py::Bool(false) => return self.out.push(0x89),
            Py::Int(i) => return self.int(i),
            Py::Float(f) => {
                self.out.push(b'G');
                return self.out.extend(f.to_be_bytes());
            }
            Py::Obj(id) => id,
        };
        if let Some(&n) = self.memo.get(&id) {
            if n < 256 {
                self.out.extend([b'h', n as u8]);
            } else {
                self.out.push(b'j');
                self.out.extend(n.to_le_bytes());
            }
            return;
        }
        let objs = self.objs;
        match &objs[id] {
            Obj::Str(s) => {
                self.out.push(b'X');
                self.out.extend((s.len() as u32).to_le_bytes());
                self.out.extend(s.as_bytes());
                self.memoize(id);
            }
            Obj::Bytes(b) => {
                if b.len() < 256 {
                    self.out.extend([b'C', b.len() as u8]);
                } else {
                    self.out.push(b'B');
                    self.out.extend((b.len() as u32).to_le_bytes());
                }
                self.out.extend(b);
                self.memoize(id);
            }
            Obj::Tuple(items) => match items.len() {
                0 => self.out.push(b')'),
                n @ 1..=3 => {
                    for item in items {
                        self.save(*item);
                    }
                    self.out.push([0x85, 0x86, 0x87][n - 1]);
                    self.memoize(id);
                }
                _ => {
                    self.out.push(b'(');
                    for item in items {
                        self.save(*item);
                    }
                    self.out.push(b't');
                    self.memoize(id);
                }
            },
            Obj::List(items) => {
                self.out.push(b']');
                self.memoize(id);
                if let [item] = items[..] {
                    self.save(item);
                    self.out.push(b'a');
                } else {
                    for batch in items.chunks(1000) {
                        self.out.push(b'(');
                        for item in batch {
                            self.save(*item);
                        }
                        self.out.push(b'e');
                    }
                }
            }
            Obj::Dict(items) => {
                self.out.push(b'}');
                self.memoize(id);
                if let [(k, v)] = items[..] {
                    self.save(k);
                    self.save(v);
                    self.out.push(b's');
                } else {
                    for batch in items.chunks(1000) {
                        self.out.push(b'(');
                        for (k, v) in batch {
                            self.save(*k);
                            self.save(*v);
                        }
                        self.out.push(b'u');
                    }
                }
            }
            Obj::Global(module, name) => {
                self.out.push(b'c');
                self.out.extend(module.as_bytes());
                self.out.push(b'\n');
                self.out.extend(name.as_bytes());
                self.out.push(b'\n');
                self.memoize(id);
            }
            Obj::Pid(pid) => {
                self.save(*pid);
                self.out.push(b'Q');
            }
        }
    }

    fn int(&mut self, i: i64) {
        if (0..256).contains(&i) {
            self.out.extend([b'K', i as u8]);
        } else if (0..65536).contains(&i) {
            self.out.push(b'M');
            self.out.extend((i as u16).to_le_bytes());
        } else if let Ok(small) = i32::try_from(i) {
            self.out.push(b'J');
            self.out.extend(small.to_le_bytes());
        } else {
            // LONG1: the fewest bytes of two's complement, little endian.
            let bytes = i.to_le_bytes();
            let mut n = 8;
            while n > 1 {
                let (top, next) = (bytes[n - 1], bytes[n - 2]);
                if (top == 0 && next & 0x80 == 0) || (top == 0xff && next & 0x80 != 0) {
                    n -= 1;
                } else {
                    break;
                }
            }
            self.out.extend([0x8a, n as u8]);
            self.out.extend(&bytes[..n]);
        }
    }
}

/// A family of BTrees: its module, its classes, its largest bucket and
/// whether it is a set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Family {
    /// The module, such as `BTrees.IOBTree`.
    pub module: &'static str,
    /// The class of a tree.
    pub tree: &'static str,
    /// The class of a bucket.
    pub bucket: &'static str,
    /// The most items of a bucket: past it, the bucket splits.
    pub leaf: usize,
    /// True for a set.
    pub set: bool,
}

/// `IOBTree`.
pub const IO: Family = Family {
    module: "BTrees.IOBTree",
    tree: "IOBTree",
    bucket: "IOBucket",
    leaf: 60,
    set: false,
};
/// `OIBTree`.
pub const OI: Family = Family {
    module: "BTrees.OIBTree",
    tree: "OIBTree",
    bucket: "OIBucket",
    leaf: 60,
    set: false,
};
/// `OOBTree`.
pub const OO: Family = Family {
    module: "BTrees.OOBTree",
    tree: "OOBTree",
    bucket: "OOBucket",
    leaf: 30,
    set: false,
};
/// `IITreeSet`.
pub const II_SET: Family = Family {
    module: "BTrees.IIBTree",
    tree: "IITreeSet",
    bucket: "IISet",
    leaf: 120,
    set: true,
};

/// A value of a tree: a value of the model, or a persistent reference.
#[derive(Clone, Debug, PartialEq)]
pub enum Item {
    /// A value.
    Val(Val),
    /// A reference to an oid of the class `module.name`.
    Ref(u64, &'static str, &'static str),
}

impl Item {
    fn py(&self, p: &mut Pickle) -> Py {
        match self {
            Item::Val(v) => p.val(v),
            Item::Ref(oid, module, name) => p.reference(*oid, module, name),
        }
    }
}

/// The items of a bucket as its state holds them.
fn flat(p: &mut Pickle, family: Family, items: &[(Val, Item)]) -> Py {
    let mut out = Vec::with_capacity(items.len() * 2);
    for (k, v) in items {
        out.push(p.val(k));
        if !family.set {
            out.push(v.py(p));
        }
    }
    p.tuple(out)
}

/// The state of a tree, as its record holds it.
#[derive(Clone, Debug, PartialEq)]
pub enum TreeRecord {
    /// An empty tree: `None`.
    Empty,
    /// One bucket, inline: its items.
    Inline(Vec<(Val, Item)>),
    /// Buckets with their own oids, and the key before each bucket but the
    /// first.
    Split {
        /// The oids of the buckets.
        children: Vec<u64>,
        /// The separating keys.
        keys: Vec<Val>,
    },
}

/// The record of a tree of `family`.
pub fn tree_record(family: Family, state: &TreeRecord) -> Vec<u8> {
    let mut p = Pickle::default();
    let state = match state {
        TreeRecord::Empty => Py::None,
        TreeRecord::Inline(items) => {
            let items = flat(&mut p, family, items);
            let bucket = p.tuple(vec![items]);
            let inner = p.tuple(vec![bucket]);
            p.tuple(vec![inner])
        }
        TreeRecord::Split { children, keys } => {
            let mut data = Vec::new();
            for (i, child) in children.iter().enumerate() {
                if i > 0 {
                    data.push(p.val(&keys[i - 1]));
                }
                data.push(p.reference(*child, family.module, family.bucket));
            }
            let data = p.tuple(data);
            let first = p.reference(children[0], family.module, family.bucket);
            p.tuple(vec![data, first])
        }
    };
    p.record(family.module, family.tree, state)
}

/// The record of a bucket of `family`, with the oid of the next bucket.
pub fn bucket_record(family: Family, items: &[(Val, Item)], next: Option<u64>) -> Vec<u8> {
    let mut p = Pickle::default();
    let items = flat(&mut p, family, items);
    let state = match next {
        Some(next) => {
            let next = p.reference(next, family.module, family.bucket);
            p.tuple(vec![items, next])
        }
        None => p.tuple(vec![items]),
    };
    p.record(family.module, family.bucket, state)
}

/// The record of a `BTrees.Length.Length` of `n`.
pub fn length_record(n: i64) -> Vec<u8> {
    Pickle::default().record("BTrees.Length", "Length", Py::Int(n))
}

/// The record of a `Catalog` with the columns `columns`, the indexes
/// `indexes` (id, oid, module and class of the index object) in the order
/// of their addition, and the oids of its trees and its `_length`. An
/// index and a column of one name share one str.
pub fn catalog_record(
    columns: &[&str],
    indexes: &[(&str, u64, &str, &str)],
    data: u64,
    uids: u64,
    paths: u64,
    length: u64,
) -> Vec<u8> {
    let mut p = Pickle::default();
    let mut items = Vec::new();
    let key = p.str("schema");
    let names: Vec<Py> = columns.iter().map(|c| p.str(c)).collect();
    let schema: Vec<(Py, Py)> = names
        .iter()
        .enumerate()
        .map(|(i, n)| (*n, Py::Int(i as i64)))
        .collect();
    let schema = p.dict(schema);
    items.push((key, schema));
    let key = p.str("names");
    let names_of = names.clone();
    let names = p.tuple(names);
    items.push((key, names));
    let key = p.str("indexes");
    let mut index_items = Vec::new();
    for (id, oid, module, class) in indexes {
        // An index and a column of one name share one str, as the interned
        // names of Python code do.
        let id = match columns.iter().position(|c| c == id) {
            Some(i) => names_of[i],
            None => p.str(id),
        };
        index_items.push((id, p.reference(*oid, module, class)));
    }
    let index_dict = p.dict(index_items);
    items.push((key, index_dict));
    for (name, oid, family) in [("data", data, IO), ("uids", uids, OI), ("paths", paths, IO)] {
        let key = p.str(name);
        items.push((key, p.reference(oid, family.module, family.tree)));
    }
    let key = p.str("_length");
    items.push((key, p.reference(length, "BTrees.Length", "Length")));
    let state = p.dict(items);
    p.record(crate::build::CATALOG.0, crate::build::CATALOG.1, state)
}

/// The record of an index of the class `module.name` that derives from
/// `UnIndex`, with the oids of its `_length`, `_index`, `_unindex` and
/// `_counter`.
pub fn index_record(
    module: &str,
    name: &str,
    id: &str,
    length: u64,
    index: u64,
    unindex: u64,
    counter: u64,
) -> Vec<u8> {
    let mut p = Pickle::default();
    let mut items = Vec::new();
    let key = p.str("id");
    let id = p.str(id);
    items.push((key, id));
    let key = p.str("ignore_ex");
    items.push((key, Py::None));
    let key = p.str("call_methods");
    items.push((key, Py::None));
    let key = p.str("indexed_attrs");
    let attrs = p.list(vec![id]);
    items.push((key, attrs));
    let key = p.str("_length");
    items.push((key, p.reference(length, "BTrees.Length", "Length")));
    let key = p.str("_index");
    items.push((key, p.reference(index, OO.module, OO.tree)));
    let key = p.str("_unindex");
    items.push((key, p.reference(unindex, IO.module, IO.tree)));
    let key = p.str("_counter");
    items.push((key, p.reference(counter, "BTrees.Length", "Length")));
    let state = p.dict(items);
    p.record(module, name, state)
}

/// A bucket of a tree of the model.
#[derive(Clone, Debug)]
struct Bucket {
    oid: Option<u64>,
    items: Vec<(Val, Item)>,
}

/// A tree of the model, with the oids that changed.
#[derive(Clone, Debug)]
pub struct Tree {
    /// The oid of the tree.
    pub root: u64,
    family: Family,
    buckets: Vec<Bucket>,
    /// The key before each bucket but the first.
    keys: Vec<Val>,
    changed: BTreeSet<u64>,
}

fn order(a: &Val, b: &Val) -> Ordering {
    py_cmp(a, b).expect("keys that the model orders")
}

impl Tree {
    fn new(root: u64, family: Family) -> Tree {
        Tree {
            root,
            family,
            buckets: Vec::new(),
            keys: Vec::new(),
            changed: BTreeSet::from([root]),
        }
    }

    /// The place of the bucket of `key`.
    fn bucket_of(&self, key: &Val) -> usize {
        self.keys
            .partition_point(|k| order(k, key) != Ordering::Greater)
    }

    fn touch(&mut self, b: usize) {
        match self.buckets[b].oid {
            Some(oid) => self.changed.insert(oid),
            None => self.changed.insert(self.root),
        };
    }

    /// The value of `key`.
    pub fn get(&self, key: &Val) -> Option<&Item> {
        let b = self.buckets.get(self.bucket_of(key))?;
        let i = b.items.binary_search_by(|(k, _)| order(k, key)).ok()?;
        Some(&b.items[i].1)
    }

    /// Sets `key` to `value`. `new_oid` gives an oid to a bucket that a
    /// split makes.
    pub fn set(&mut self, key: Val, value: Item, new_oid: &mut impl FnMut() -> u64) {
        if self.buckets.is_empty() {
            self.buckets.push(Bucket {
                oid: None,
                items: Vec::new(),
            });
        }
        let b = self.bucket_of(&key);
        let items = &mut self.buckets[b].items;
        match items.binary_search_by(|(k, _)| order(k, &key)) {
            Ok(i) => {
                if items[i].1 == value {
                    return;
                }
                items[i].1 = value;
            }
            Err(i) => items.insert(i, (key, value)),
        }
        self.touch(b);
        if self.buckets[b].items.len() > self.family.leaf {
            let half = self.buckets[b].items.len() / 2;
            let moved = self.buckets[b].items.split_off(half);
            if self.buckets[b].oid.is_none() {
                self.buckets[b].oid = Some(new_oid());
            }
            let oid = new_oid();
            self.keys.insert(b, moved[0].0.clone());
            self.buckets.insert(
                b + 1,
                Bucket {
                    oid: Some(oid),
                    items: moved,
                },
            );
            self.touch(b);
            self.changed.insert(oid);
            self.changed.insert(self.root);
        }
    }

    /// Removes `key`, and gives its value.
    pub fn remove(&mut self, key: &Val) -> Option<Item> {
        let b = self.bucket_of(key);
        let bucket = self.buckets.get_mut(b)?;
        let i = bucket.items.binary_search_by(|(k, _)| order(k, key)).ok()?;
        let (_, value) = bucket.items.remove(i);
        self.touch(b);
        if self.buckets[b].items.is_empty() {
            if self.buckets.len() == 1 {
                self.buckets.clear();
                self.keys.clear();
            } else {
                self.buckets.remove(b);
                self.keys.remove(b.saturating_sub(1));
                if b > 0 {
                    self.touch(b - 1);
                }
            }
            self.changed.insert(self.root);
        }
        Some(value)
    }

    /// The number of items.
    pub fn len(&self) -> usize {
        self.buckets.iter().map(|b| b.items.len()).sum()
    }

    /// Tells if the tree holds no item.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The records of the tree and of its buckets that changed since the
    /// last call.
    fn records(&mut self, out: &mut Vec<(u64, Vec<u8>)>) {
        let changed = std::mem::take(&mut self.changed);
        for oid in changed {
            if oid == self.root {
                out.push((oid, tree_record(self.family, &self.state())));
                continue;
            }
            if let Some(b) = self.buckets.iter().position(|b| b.oid == Some(oid)) {
                let next = self.buckets.get(b + 1).and_then(|n| n.oid);
                out.push((
                    oid,
                    bucket_record(self.family, &self.buckets[b].items, next),
                ));
            }
        }
    }

    fn state(&self) -> TreeRecord {
        match &self.buckets[..] {
            [] => TreeRecord::Empty,
            [only] if only.oid.is_none() => TreeRecord::Inline(only.items.clone()),
            buckets => TreeRecord::Split {
                children: buckets.iter().filter_map(|b| b.oid).collect(),
                keys: self.keys.clone(),
            },
        }
    }
}

/// The kind of an index of a `CatalogModel`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexKind {
    /// A FieldIndex.
    Field,
    /// An index of another class, `module.name`, whose trees stay empty.
    Other(&'static str, &'static str),
}

#[derive(Clone, Debug)]
struct IndexModel {
    id: String,
    kind: IndexKind,
    oid: u64,
    length: (u64, i64),
    counter: (u64, i64),
    index: Tree,
    unindex: Tree,
    /// The tree set of each key of `_index`.
    sets: Vec<(Val, Tree)>,
    dirty: bool,
}

impl IndexModel {
    fn class(&self) -> (&'static str, &'static str) {
        match self.kind {
            IndexKind::Field => crate::base::FIELD_INDEX,
            IndexKind::Other(module, name) => (module, name),
        }
    }
}

fn take(next: &mut u64) -> u64 {
    *next += 1;
    *next - 1
}

/// A catalog of the model: a `Catalog` with its indexes and its trees.
#[derive(Clone, Debug)]
pub struct CatalogModel {
    oid: u64,
    next: u64,
    columns: Vec<String>,
    indexes: Vec<IndexModel>,
    data: Tree,
    uids: Tree,
    paths: Tree,
    length: (u64, i64),
    dirty: BTreeSet<u64>,
}

impl CatalogModel {
    /// A new catalog whose objects take oids from `first_oid`, with the
    /// indexes `indexes` and the columns `columns`.
    pub fn new(first_oid: u64, indexes: &[(&str, IndexKind)], columns: &[&str]) -> CatalogModel {
        let mut next = first_oid;
        let oid = take(&mut next);
        let data = Tree::new(take(&mut next), IO);
        let uids = Tree::new(take(&mut next), OI);
        let paths = Tree::new(take(&mut next), IO);
        let length = (take(&mut next), 0);
        let indexes = indexes
            .iter()
            .map(|(id, kind)| IndexModel {
                id: (*id).to_owned(),
                kind: *kind,
                oid: take(&mut next),
                length: (take(&mut next), 0),
                counter: (take(&mut next), 0),
                index: Tree::new(take(&mut next), OO),
                unindex: Tree::new(take(&mut next), IO),
                sets: Vec::new(),
                dirty: true,
            })
            .collect();
        CatalogModel {
            oid,
            next,
            columns: columns.iter().map(|c| (*c).to_owned()).collect(),
            indexes,
            data,
            uids,
            paths,
            length,
            dirty: BTreeSet::from([oid, length.0]),
        }
    }

    /// The oid of the catalog object.
    pub fn oid(&self) -> u64 {
        self.oid
    }

    /// The registration key of the catalog.
    pub fn key(&self) -> [u8; 8] {
        self.oid.to_be_bytes()
    }

    /// The oid of the root of the `_unindex` of the index `id`.
    pub fn unindex_root(&self, id: &str) -> u64 {
        self.index(id).unindex.root
    }

    fn index(&self, id: &str) -> &IndexModel {
        self.indexes
            .iter()
            .find(|i| i.id == id)
            .expect("an index of the model")
    }

    /// The rid of `path`.
    pub fn rid(&self, path: &str) -> Option<i32> {
        match self.uids.get(&Val::Str(path.into())) {
            Some(Item::Val(Val::Int(rid))) => Some(*rid as i32),
            _ => None,
        }
    }

    /// The paths of the catalog, with their rids, by rid.
    pub fn entries(&self) -> Vec<(i32, String)> {
        self.paths
            .buckets
            .iter()
            .flat_map(|b| b.items.iter())
            .filter_map(|(rid, path)| match (rid, path) {
                (Val::Int(rid), Item::Val(Val::Str(path))) => Some((*rid as i32, path.to_string())),
                _ => None,
            })
            .collect()
    }

    /// Catalogs the object at `path` with `values`, as
    /// `Catalog.catalogObject` does. A new path takes the rid `rid`. A
    /// FieldIndex without a value in `values` has no key for the row.
    pub fn catalog(&mut self, rid: i32, path: &str, values: &[(&str, Val)]) {
        let next = &mut self.next;
        let mut new_oid = || take(next);
        let rid = match self.uids.get(&Val::Str(path.into())) {
            Some(Item::Val(Val::Int(old))) => *old as i32,
            _ => {
                let key = Val::Str(path.into());
                self.uids
                    .set(key, Item::Val(Val::Int(i64::from(rid))), &mut new_oid);
                let path = Item::Val(Val::Str(path.into()));
                self.paths.set(Val::Int(i64::from(rid)), path, &mut new_oid);
                self.length.1 += 1;
                self.dirty.insert(self.length.0);
                rid
            }
        };
        let value_of = |name: &str| {
            values
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| v.clone())
        };
        let record: Vec<Val> = self
            .columns
            .iter()
            .map(|c| value_of(c).unwrap_or(Val::Missing))
            .collect();
        let doc = Val::Int(i64::from(rid));
        self.data.set(
            doc.clone(),
            Item::Val(Val::Tuple(record.into())),
            &mut new_oid,
        );
        for index in &mut self.indexes {
            if index.kind != IndexKind::Field {
                continue;
            }
            let datum = value_of(&index.id);
            let old = match index.unindex.get(&doc) {
                Some(Item::Val(v)) => Some(v.clone()),
                _ => None,
            };
            if old == datum {
                continue;
            }
            if let Some(old) = &old {
                remove_forward(index, old, rid);
            }
            match datum {
                Some(datum) => {
                    insert_forward(index, &datum, rid, &mut new_oid);
                    index
                        .unindex
                        .set(doc.clone(), Item::Val(datum), &mut new_oid);
                }
                None => {
                    index.unindex.remove(&doc);
                }
            }
            index.counter.1 += 1;
            self.dirty.insert(index.counter.0);
        }
    }

    /// Uncatalogs the object at `path`, as `Catalog.uncatalogObject` does.
    pub fn uncatalog(&mut self, path: &str) {
        let Some(rid) = self.rid(path) else {
            return;
        };
        let doc = Val::Int(i64::from(rid));
        for index in &mut self.indexes {
            if index.kind != IndexKind::Field {
                continue;
            }
            if let Some(Item::Val(old)) = index.unindex.remove(&doc) {
                remove_forward(index, &old, rid);
                index.counter.1 += 1;
                self.dirty.insert(index.counter.0);
            }
        }
        self.data.remove(&doc);
        self.paths.remove(&doc);
        self.uids.remove(&Val::Str(path.into()));
        self.length.1 -= 1;
        self.dirty.insert(self.length.0);
    }

    /// Clears the index `id`, as `clear` of an `UnIndex` does: new trees
    /// and a new `_length`, so the index object changes.
    pub fn clear_index(&mut self, id: &str) {
        let next = &mut self.next;
        let index = self
            .indexes
            .iter_mut()
            .find(|i| i.id == id)
            .expect("an index of the model");
        index.length = (take(next), 0);
        index.index = Tree::new(take(next), OO);
        index.unindex = Tree::new(take(next), IO);
        index.sets.clear();
        index.counter.1 += 1;
        index.dirty = true;
        self.dirty.insert(index.counter.0);
    }

    /// The records of the objects that changed since the last call: each
    /// object at the first call.
    pub fn records(&mut self) -> Vec<(u64, Vec<u8>)> {
        let mut out = Vec::new();
        if self.dirty.remove(&self.oid) {
            let indexes: Vec<(&str, u64, &str, &str)> = self
                .indexes
                .iter()
                .map(|i| {
                    let (module, name) = i.class();
                    (i.id.as_str(), i.oid, module, name)
                })
                .collect();
            let columns: Vec<&str> = self.columns.iter().map(String::as_str).collect();
            out.push((
                self.oid,
                catalog_record(
                    &columns,
                    &indexes,
                    self.data.root,
                    self.uids.root,
                    self.paths.root,
                    self.length.0,
                ),
            ));
        }
        if self.dirty.remove(&self.length.0) {
            out.push((self.length.0, length_record(self.length.1)));
        }
        for tree in [&mut self.data, &mut self.uids, &mut self.paths] {
            tree.records(&mut out);
        }
        for index in &mut self.indexes {
            let (module, name) = index.class();
            if std::mem::take(&mut index.dirty) {
                out.push((
                    index.oid,
                    index_record(
                        module,
                        name,
                        &index.id,
                        index.length.0,
                        index.index.root,
                        index.unindex.root,
                        index.counter.0,
                    ),
                ));
                out.push((index.length.0, length_record(index.length.1)));
                self.dirty.insert(index.counter.0);
            }
            if self.dirty.remove(&index.counter.0) {
                out.push((index.counter.0, length_record(index.counter.1)));
            }
            index.index.records(&mut out);
            index.unindex.records(&mut out);
            for (_, set) in &mut index.sets {
                set.records(&mut out);
            }
        }
        out
    }
}

fn insert_forward(index: &mut IndexModel, key: &Val, rid: i32, new_oid: &mut impl FnMut() -> u64) {
    let row = Val::Int(i64::from(rid));
    match index.sets.iter().position(|(k, _)| k == key) {
        Some(i) => index.sets[i].1.set(row, Item::Val(Val::None), new_oid),
        None => {
            let mut set = Tree::new(new_oid(), II_SET);
            set.set(row, Item::Val(Val::None), new_oid);
            let reference = Item::Ref(set.root, II_SET.module, II_SET.tree);
            index.index.set(key.clone(), reference, new_oid);
            index.sets.push((key.clone(), set));
            index.length.1 += 1;
            index.dirty = true;
        }
    }
}

fn remove_forward(index: &mut IndexModel, key: &Val, rid: i32) {
    let Some(i) = index.sets.iter().position(|(k, _)| k == key) else {
        return;
    };
    index.sets[i].1.remove(&Val::Int(i64::from(rid)));
    if index.sets[i].1.is_empty() {
        index.sets.remove(i);
        index.index.remove(key);
        index.length.1 -= 1;
        index.dirty = true;
    }
}

/// Commits `records` to `store` in one transaction, with the registration
/// keys `register` and without the keys `unregister`. Returns the tid.
pub fn commit(
    store: &Store,
    records: &[(u64, Vec<u8>)],
    register: &[[u8; 8]],
    unregister: &[[u8; 8]],
) -> Result<u64, ZoeError> {
    let mut txn = store.tpc_begin(b"", b"", b"")?;
    for (oid, data) in records {
        let serial = match store.load(Oid(*oid)) {
            Ok((_, serial)) => serial,
            Err(ZoeError::NotFound { .. }) => Tid::ZERO,
            Err(e) => return Err(e),
        };
        txn.store(Oid(*oid), serial, data)?;
    }
    for key in register {
        txn.kv_set(Flavour::Durable, NAMESPACE, key, b"", None)?;
    }
    for key in unregister {
        txn.kv_delete(Flavour::Durable, NAMESPACE, key)?;
    }
    txn.vote()?;
    Ok(txn.finish()?.0)
}
