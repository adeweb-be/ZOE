//! The records of a catalog as Products.ZCatalog writes them, for the tests
//! of this crate and of the crates that serve the mirror. The feature
//! `testing` builds this module.
//!
//! - `Pickle` writes a record as ZODB's `ObjectWriter` does with the C
//!   pickler of zodbpickle and protocol 3: one memo for the class pickle
//!   and the state pickle, a new tuple `(oid, class)` for each persistent
//!   reference, and the oid bytes and the class shared in the record.
//! - The record writers (`catalog_record`, `index_record`,
//!   `date_index_record`, `uuid_index_record`, `boolean_index_record`,
//!   `date_range_index_record`, `extended_path_index_record`,
//!   `length_record`, `tree_record`, `bucket_record`) give the records of
//!   the classes that a catalog holds. A test compares them with records
//!   that CPython wrote.
//! - `CatalogModel` keeps a catalog with its indexes and its trees, as
//!   `Catalog.catalogObject` and `uncatalogObject` change them, and gives
//!   the records that changed since the last call. Its indexes are
//!   FieldIndexes, KeywordIndexes, DateIndexes, UUIDIndexes, BooleanIndexes,
//!   DateRangeIndexes and ExtendedPathIndexes, each with its own trees. Its trees split a
//!   bucket at half of its items, past the largest bucket of the family,
//!   and an inner node at half of its children, past the largest inner
//!   node of the family or the fan-out of `with_fanout`; the root splits as
//!   `BTree_split_root` does. Its tree sets stay one inline bucket.
//! - `commit` writes records and registration keys to a store.
//! - With the feature `text`, `commit_text` writes the deltas of the text
//!   indexes of a catalog and the settings of `zoe.text` (ADR 0060).

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
            Val::DateTime(..) | Val::Message(_) | Val::Cell(_) => {
                panic!("the testing pickler writes no DateTime, no Message and no cell")
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
    /// The most children of an inner node: past it, the node splits.
    pub internal: usize,
}

/// `IOBTree`.
pub const IO: Family = Family {
    module: "BTrees.IOBTree",
    tree: "IOBTree",
    bucket: "IOBucket",
    leaf: 60,
    set: false,
    internal: 500,
};
/// `OIBTree`.
pub const OI: Family = Family {
    module: "BTrees.OIBTree",
    tree: "OIBTree",
    bucket: "OIBucket",
    leaf: 60,
    set: false,
    internal: 250,
};
/// `OOBTree`.
pub const OO: Family = Family {
    module: "BTrees.OOBTree",
    tree: "OOBTree",
    bucket: "OOBucket",
    leaf: 30,
    set: false,
    internal: 250,
};
/// `IIBTree`.
pub const II: Family = Family {
    module: "BTrees.IIBTree",
    tree: "IIBTree",
    bucket: "IIBucket",
    leaf: 120,
    set: false,
    internal: 500,
};
/// `IITreeSet`.
pub const II_SET: Family = Family {
    module: "BTrees.IIBTree",
    tree: "IITreeSet",
    bucket: "IISet",
    leaf: 120,
    set: true,
    internal: 500,
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
    /// Children with their own oids, the key before each child but the
    /// first, and the first bucket of the tree.
    Split {
        /// The oid of each child, and true for an inner node, false for a
        /// bucket.
        children: Vec<(u64, bool)>,
        /// The separating keys.
        keys: Vec<Val>,
        /// The oid of the first bucket.
        first: u64,
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
        TreeRecord::Split {
            children,
            keys,
            first,
        } => {
            let mut data = Vec::new();
            for (i, (child, inner)) in children.iter().enumerate() {
                if i > 0 {
                    data.push(p.val(&keys[i - 1]));
                }
                let class = if *inner { family.tree } else { family.bucket };
                data.push(p.reference(*child, family.module, class));
            }
            let data = p.tuple(data);
            let first = p.reference(*first, family.module, family.bucket);
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

/// One attribute of the state of an index object.
enum Attr<'a> {
    /// The id of the index.
    Id,
    /// `None`.
    None,
    /// An int.
    Int(i64),
    /// A str.
    Str(&'a str),
    /// The list `[id]`.
    IdList,
    /// A persistent reference to `oid`, of the class of `family`'s tree.
    Tree(u64, Family),
    /// A persistent reference to a `BTrees.Length.Length`.
    Length(u64),
    /// A bool.
    Bool(bool),
}

/// The record of an index of the class `class` with the id `id` and the
/// attributes `attrs`, in the order of its `__dict__`.
fn index_object(class: (&str, &str), id: &str, attrs: &[(&str, Attr<'_>)]) -> Vec<u8> {
    let mut p = Pickle::default();
    let mut items = Vec::new();
    let mut id_str = None;
    for (name, attr) in attrs {
        let key = p.str(name);
        let value = match attr {
            Attr::Id => *id_str.get_or_insert_with(|| p.str(id)),
            Attr::None => Py::None,
            Attr::Int(i) => Py::Int(*i),
            Attr::Str(text) => p.str(text),
            Attr::IdList => {
                let id = *id_str.get_or_insert_with(|| p.str(id));
                p.list(vec![id])
            }
            Attr::Tree(oid, family) => p.reference(*oid, family.module, family.tree),
            Attr::Length(oid) => p.reference(*oid, "BTrees.Length", "Length"),
            Attr::Bool(b) => Py::Bool(*b),
        };
        items.push((key, value));
    }
    let state = p.dict(items);
    p.record(class.0, class.1, state)
}

/// The record of a `DateIndex`, with the oids of its `_index`, its
/// `_unindex`, its `_length` and its `_counter`.
pub fn date_index_record(id: &str, index: u64, unindex: u64, length: u64, counter: u64) -> Vec<u8> {
    index_object(
        crate::base::DATE_INDEX,
        id,
        &[
            ("id", Attr::Id),
            ("ignore_ex", Attr::None),
            ("call_methods", Attr::None),
            ("indexed_attrs", Attr::IdList),
            ("_index", Attr::Tree(index, IO)),
            ("_unindex", Attr::Tree(unindex, II)),
            ("_length", Attr::Length(length)),
            ("_counter", Attr::Length(counter)),
        ],
    )
}

/// The record of a `UUIDIndex`, with the oids of its `_length`, its
/// `_index`, its `_unindex` and its `_counter`.
pub fn uuid_index_record(id: &str, length: u64, index: u64, unindex: u64, counter: u64) -> Vec<u8> {
    index_object(
        crate::base::UUID_INDEX,
        id,
        &[
            ("id", Attr::Id),
            ("ignore_ex", Attr::None),
            ("call_methods", Attr::None),
            ("indexed_attrs", Attr::IdList),
            ("_length", Attr::Length(length)),
            ("_index", Attr::Tree(index, OI)),
            ("_unindex", Attr::Tree(unindex, IO)),
            ("_counter", Attr::Length(counter)),
        ],
    )
}

/// The oids and the indexed value of a `BooleanIndex`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BooleanOids {
    /// `_index`, an `IITreeSet`.
    pub index: u64,
    /// `_index_length`.
    pub index_length: u64,
    /// `_index_value`: the value of the rows of `_index`.
    pub index_value: i64,
    /// `_unindex`, an `IIBTree`.
    pub unindex: u64,
    /// `_length`.
    pub length: u64,
    /// `_counter`.
    pub counter: u64,
}

/// The record of a `BooleanIndex`.
pub fn boolean_index_record(id: &str, oids: &BooleanOids) -> Vec<u8> {
    index_object(
        crate::base::BOOLEAN_INDEX,
        id,
        &[
            ("id", Attr::Id),
            ("ignore_ex", Attr::None),
            ("call_methods", Attr::None),
            ("indexed_attrs", Attr::IdList),
            ("_index", Attr::Tree(oids.index, II_SET)),
            ("_index_length", Attr::Length(oids.index_length)),
            ("_index_value", Attr::Int(oids.index_value)),
            ("_unindex", Attr::Tree(oids.unindex, II)),
            ("_length", Attr::Length(oids.length)),
            ("_counter", Attr::Length(oids.counter)),
        ],
    )
}

/// The fields and the oids of a `DateRangeIndex`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DateRangeOids<'a> {
    /// `_since_field`.
    pub since_field: &'a str,
    /// `_until_field`.
    pub until_field: &'a str,
    /// `_always`, an `IITreeSet`.
    pub always: u64,
    /// `_since_only`, `_until_only`, `_since` and `_until`, each an
    /// `IOBTree`.
    pub trees: [u64; 4],
    /// `_unindex`, an `IOBTree`.
    pub unindex: u64,
    /// `_length`.
    pub length: u64,
    /// `_counter`.
    pub counter: u64,
}

/// The record of a `DateRangeIndex`.
pub fn date_range_index_record(id: &str, oids: &DateRangeOids<'_>) -> Vec<u8> {
    let [since_only, until_only, since, until] = oids.trees;
    index_object(
        crate::base::DATE_RANGE_INDEX,
        id,
        &[
            ("id", Attr::Id),
            ("_since_field", Attr::Str(oids.since_field)),
            ("_until_field", Attr::Str(oids.until_field)),
            ("_always", Attr::Tree(oids.always, II_SET)),
            ("_since_only", Attr::Tree(since_only, IO)),
            ("_until_only", Attr::Tree(until_only, IO)),
            ("_since", Attr::Tree(since, IO)),
            ("_until", Attr::Tree(until, IO)),
            ("_unindex", Attr::Tree(oids.unindex, IO)),
            ("_length", Attr::Length(oids.length)),
            ("_counter", Attr::Length(oids.counter)),
        ],
    )
}

/// The `_depth`, the oids and the `multi_valued` of an `ExtendedPathIndex`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PathOids {
    /// `_depth`.
    pub depth: i64,
    /// `_index`, an `OOBTree`.
    pub index: u64,
    /// `_unindex`, an `IOBTree`.
    pub unindex: u64,
    /// `_length`.
    pub length: u64,
    /// `_index_parents`, an `OOBTree`.
    pub parents: u64,
    /// `_index_items`, an `OIBTree`.
    pub items: u64,
    /// `multi_valued`.
    pub multi_valued: bool,
}

/// The record of an `ExtendedPathIndex`.
pub fn extended_path_index_record(id: &str, oids: &PathOids) -> Vec<u8> {
    index_object(
        crate::base::EXTENDED_PATH_INDEX,
        id,
        &[
            ("id", Attr::Id),
            ("_depth", Attr::Int(oids.depth)),
            ("_index", Attr::Tree(oids.index, OO)),
            ("_unindex", Attr::Tree(oids.unindex, IO)),
            ("_length", Attr::Length(oids.length)),
            ("_index_parents", Attr::Tree(oids.parents, OO)),
            ("_index_items", Attr::Tree(oids.items, OI)),
            ("multi_valued", Attr::Bool(oids.multi_valued)),
        ],
    )
}

/// A bucket of a tree of the model.
#[derive(Clone, Debug)]
struct Bucket {
    oid: Option<u64>,
    items: Vec<(Val, Item)>,
}

/// A child of an inner node of the model.
#[derive(Clone, Debug)]
enum Child {
    Bucket(Bucket),
    Node(Inner),
}

/// An inner node of the model: the tree object itself at the root.
#[derive(Clone, Debug)]
struct Inner {
    oid: u64,
    /// The key before each child but the first.
    keys: Vec<Val>,
    children: Vec<Child>,
}

/// What a change of a tree of the model needs: oids, and the oids that
/// changed.
struct Work<'a, F> {
    family: Family,
    fanout: usize,
    new_oid: &'a mut F,
    changed: &'a mut BTreeSet<u64>,
    /// The splits of an inner node below the root, and of the root.
    splits: &'a mut (usize, usize),
}

/// A tree of the model, with the oids that changed.
///
/// An inner node splits at half of its children past `fanout` children, as
/// a bucket splits past the largest bucket of its family. The root splits
/// as `BTree_split_root` does: its children move to a new inner node, which
/// splits.
#[derive(Clone, Debug)]
pub struct Tree {
    /// The oid of the tree.
    pub root: u64,
    family: Family,
    fanout: usize,
    top: Inner,
    changed: BTreeSet<u64>,
    /// The next bucket of each bucket, and the first bucket of each inner
    /// node, as the last records gave them.
    written: HashMap<u64, Option<u64>>,
    splits: (usize, usize),
}

/// The order of two keys of a tree. `None` comes before each other key,
/// as BTrees orders it.
fn order(a: &Val, b: &Val) -> Ordering {
    match (a, b) {
        (Val::None, Val::None) => Ordering::Equal,
        (Val::None, _) => Ordering::Less,
        (_, Val::None) => Ordering::Greater,
        _ => py_cmp(a, b).expect("keys that the model orders"),
    }
}

/// The place of the child of `key` in `node`.
fn child_of(node: &Inner, key: &Val) -> usize {
    node.keys
        .partition_point(|k| order(k, key) != Ordering::Greater)
}

/// Sets `key` in the subtree of `node`. Returns the key and the new sibling
/// of `node` when `node` splits.
fn set_in<F: FnMut() -> u64>(
    node: &mut Inner,
    key: Val,
    value: Item,
    work: &mut Work<'_, F>,
) -> Option<(Val, Inner)> {
    if node.children.is_empty() {
        node.children.push(Child::Bucket(Bucket {
            oid: None,
            items: Vec::new(),
        }));
    }
    let i = child_of(node, &key);
    let oid = node.oid;
    let mut grown: Option<(Val, Child)> = None;
    match &mut node.children[i] {
        Child::Bucket(b) => {
            match b.items.binary_search_by(|(k, _)| order(k, &key)) {
                Ok(at) if b.items[at].1 == value => return None,
                Ok(at) => b.items[at].1 = value,
                Err(at) => b.items.insert(at, (key, value)),
            }
            work.changed.insert(b.oid.unwrap_or(oid));
            if b.items.len() > work.family.leaf {
                let half = b.items.len() / 2;
                let moved = b.items.split_off(half);
                if b.oid.is_none() {
                    b.oid = Some((work.new_oid)());
                }
                work.changed.insert(b.oid.unwrap_or(oid));
                let new = (work.new_oid)();
                work.changed.insert(new);
                grown = Some((
                    moved[0].0.clone(),
                    Child::Bucket(Bucket {
                        oid: Some(new),
                        items: moved,
                    }),
                ));
            }
        }
        Child::Node(n) => {
            if let Some((sep, sibling)) = set_in(n, key, value, work) {
                grown = Some((sep, Child::Node(sibling)));
            }
        }
    }
    if let Some((sep, child)) = grown {
        node.keys.insert(i, sep);
        node.children.insert(i + 1, child);
        work.changed.insert(oid);
    }
    if node.children.len() <= work.fanout {
        return None;
    }
    let half = node.children.len() / 2;
    let children = node.children.split_off(half);
    let mut keys = node.keys.split_off(half - 1);
    let sep = keys.remove(0);
    work.splits.0 += 1;
    let sibling = Inner {
        oid: (work.new_oid)(),
        keys,
        children,
    };
    work.changed.insert(sibling.oid);
    Some((sep, sibling))
}

/// Removes `key` from the subtree of `node`. Returns its value, and true
/// when `node` holds no child after it.
fn remove_in(node: &mut Inner, key: &Val, changed: &mut BTreeSet<u64>) -> Option<(Item, bool)> {
    let i = child_of(node, key);
    let oid = node.oid;
    let (value, empty) = match node.children.get_mut(i)? {
        Child::Bucket(b) => {
            let at = b.items.binary_search_by(|(k, _)| order(k, key)).ok()?;
            let (_, value) = b.items.remove(at);
            changed.insert(b.oid.unwrap_or(oid));
            (value, b.items.is_empty())
        }
        Child::Node(n) => remove_in(n, key, changed)?,
    };
    if empty {
        node.children.remove(i);
        if !node.keys.is_empty() {
            node.keys.remove(i.saturating_sub(1));
        }
        changed.insert(oid);
    }
    Some((value, node.children.is_empty()))
}

/// The buckets of the subtree of `node`, in key order.
fn leaves<'a>(node: &'a Inner, out: &mut Vec<&'a Bucket>) {
    for child in &node.children {
        match child {
            Child::Bucket(b) => out.push(b),
            Child::Node(n) => leaves(n, out),
        }
    }
}

/// The inner nodes of the subtree of `node`, `node` included.
fn nodes<'a>(node: &'a Inner, out: &mut Vec<&'a Inner>) {
    out.push(node);
    for child in &node.children {
        if let Child::Node(n) = child {
            nodes(n, out);
        }
    }
}

impl Tree {
    fn new(root: u64, family: Family, fanout: Option<usize>) -> Tree {
        let fanout = fanout.unwrap_or(family.internal).max(2);
        Tree {
            root,
            family,
            fanout,
            top: Inner {
                oid: root,
                keys: Vec::new(),
                children: Vec::new(),
            },
            changed: BTreeSet::from([root]),
            written: HashMap::new(),
            splits: (0, 0),
        }
    }

    /// The value of `key`.
    pub fn get(&self, key: &Val) -> Option<&Item> {
        let mut node = &self.top;
        loop {
            match node.children.get(child_of(node, key))? {
                Child::Bucket(b) => {
                    let i = b.items.binary_search_by(|(k, _)| order(k, key)).ok()?;
                    return Some(&b.items[i].1);
                }
                Child::Node(n) => node = n,
            }
        }
    }

    /// Sets `key` to `value`. `new_oid` gives an oid to a bucket or an
    /// inner node that a split makes.
    pub fn set(&mut self, key: Val, value: Item, new_oid: &mut impl FnMut() -> u64) {
        let mut work = Work {
            family: self.family,
            fanout: self.fanout,
            new_oid,
            changed: &mut self.changed,
            splits: &mut self.splits,
        };
        if let Some((sep, sibling)) = set_in(&mut self.top, key, value, &mut work) {
            // The root splits: its first half moves to a new inner node.
            let left = Inner {
                oid: (work.new_oid)(),
                keys: std::mem::take(&mut self.top.keys),
                children: std::mem::take(&mut self.top.children),
            };
            work.changed.insert(left.oid);
            work.changed.insert(self.root);
            self.top.keys = vec![sep];
            self.top.children = vec![Child::Node(left), Child::Node(sibling)];
            work.splits.0 -= 1;
            work.splits.1 += 1;
        }
    }

    /// Removes `key`, and gives its value.
    pub fn remove(&mut self, key: &Val) -> Option<Item> {
        remove_in(&mut self.top, key, &mut self.changed).map(|(value, _)| value)
    }

    /// The items of the tree, in key order.
    fn items(&self) -> Vec<&(Val, Item)> {
        let mut out = Vec::new();
        leaves(&self.top, &mut out);
        out.into_iter().flat_map(|b| b.items.iter()).collect()
    }

    /// The number of items.
    pub fn len(&self) -> usize {
        self.items().len()
    }

    /// Tells if the tree holds no item.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The splits of an inner node below the root, and of the root.
    pub fn splits(&self) -> (usize, usize) {
        self.splits
    }

    /// The records of the tree, of its inner nodes and of its buckets that
    /// changed since the last call.
    fn records(&mut self, out: &mut Vec<(u64, Vec<u8>)>) {
        let mut changed = std::mem::take(&mut self.changed);
        let mut buckets = Vec::new();
        leaves(&self.top, &mut buckets);
        // A bucket whose next bucket changed, and an inner node whose first
        // bucket changed, change too.
        for (i, b) in buckets.iter().enumerate() {
            if let Some(oid) = b.oid {
                let next = buckets.get(i + 1).and_then(|n| n.oid);
                if self.written.insert(oid, next) != Some(next) {
                    changed.insert(oid);
                }
            }
        }
        let mut inner = Vec::new();
        nodes(&self.top, &mut inner);
        for node in &inner {
            let mut first = Vec::new();
            leaves(node, &mut first);
            let first = first.first().and_then(|b| b.oid);
            if self.written.insert(node.oid, first) != Some(first) {
                changed.insert(node.oid);
            }
        }
        for node in &inner {
            if changed.contains(&node.oid) {
                out.push((node.oid, tree_record(self.family, &state(node))));
            }
        }
        for (i, b) in buckets.iter().enumerate() {
            if let Some(oid) = b.oid
                && changed.contains(&oid)
            {
                let next = buckets.get(i + 1).and_then(|n| n.oid);
                out.push((oid, bucket_record(self.family, &b.items, next)));
            }
        }
    }
}

/// The state of an inner node.
fn state(node: &Inner) -> TreeRecord {
    match &node.children[..] {
        [] => TreeRecord::Empty,
        [Child::Bucket(only)] if only.oid.is_none() => TreeRecord::Inline(only.items.clone()),
        children => {
            let mut first = Vec::new();
            leaves(node, &mut first);
            TreeRecord::Split {
                children: children
                    .iter()
                    .map(|c| match c {
                        Child::Bucket(b) => (b.oid.expect("a bucket with an oid"), false),
                        Child::Node(n) => (n.oid, true),
                    })
                    .collect(),
                keys: node.keys.clone(),
                first: first[0].oid.expect("a bucket with an oid"),
            }
        }
    }
}

/// The kind of an index of a `CatalogModel`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexKind {
    /// A FieldIndex.
    Field,
    /// A KeywordIndex: the value of a row is a list or a tuple of keys.
    Keyword,
    /// A DateIndex: the value of a row is its date in minutes.
    Date,
    /// A UUIDIndex: the value of a row is a str.
    Uuid,
    /// A BooleanIndex: the value of a row is a bool.
    Boolean,
    /// A DateRangeIndex on the attributes `since` and `until`: the value of
    /// a row is the tuple `(since, until)`, each in minutes or `None`.
    DateRange(&'static str, &'static str),
    /// An ExtendedPathIndex: the value of a row is its path, a str. A row
    /// without a value in `catalog` takes the path of the entry.
    Path,
    /// An index of another class, `module.name`, whose trees stay empty.
    Other(&'static str, &'static str),
}

impl IndexKind {
    /// The families of `_index` and of `_unindex`. The `_index` of a
    /// DateRangeIndex is its `_always`.
    fn families(self) -> (Family, Family) {
        match self {
            IndexKind::Field | IndexKind::Keyword | IndexKind::Path | IndexKind::Other(..) => {
                (OO, IO)
            }
            IndexKind::Date => (IO, II),
            IndexKind::Uuid => (OI, IO),
            IndexKind::Boolean => (II_SET, II),
            IndexKind::DateRange(..) => (II_SET, IO),
        }
    }
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
    /// A BooleanIndex: `_index_length`, and `_index_value`.
    boolean: ((u64, i64), i64),
    /// The `_index_length` that the last records gave.
    written: Option<(u64, i64)>,
    /// A DateRangeIndex: `_since_only`, `_until_only`, `_since` and
    /// `_until`, with the tree set of each of their keys.
    ranges: Vec<(Tree, Vec<(Val, Tree)>)>,
    /// An ExtendedPathIndex: its trees beyond `_index` and `_unindex`.
    path: Option<PathTrees>,
    /// The `_length` that the last records gave, for an index whose
    /// `_length` changes without its record.
    length_written: Option<i64>,
    dirty: bool,
}

/// A component of `_index` of an ExtendedPathIndex, `None` for the end of
/// a path: its `IOBTree`, with the tree set of each level.
type Levels = (Val, Tree, Vec<(i64, Tree)>);

/// The trees of an ExtendedPathIndex beyond `_index` and `_unindex`, and
/// its `_depth`.
#[derive(Clone, Debug)]
struct PathTrees {
    depth: i64,
    /// The components of `_index`.
    levels: Vec<Levels>,
    /// `_index_parents`, with the tree set of each parent path.
    parents: Tree,
    parent_sets: Vec<(Val, Tree)>,
    /// `_index_items`.
    items: Tree,
}

impl IndexModel {
    fn new(id: &str, kind: IndexKind, next: &mut u64, fanout: Option<usize>) -> IndexModel {
        let (index_family, unindex_family) = kind.families();
        let oid = take(next);
        let length = (take(next), 0);
        let counter = (take(next), 0);
        let index = Tree::new(take(next), index_family, fanout);
        let unindex = Tree::new(take(next), unindex_family, fanout);
        let mut model = IndexModel {
            id: id.to_owned(),
            kind,
            oid,
            length,
            counter,
            index,
            unindex,
            sets: Vec::new(),
            boolean: ((0, 0), 1),
            written: None,
            ranges: Vec::new(),
            path: None,
            length_written: None,
            dirty: true,
        };
        model.clear_extra(next, fanout);
        model
    }

    /// New objects for the attributes of a BooleanIndex or of a
    /// DateRangeIndex beyond `_index` and `_unindex`, as `clear` makes them.
    fn clear_extra(&mut self, next: &mut u64, fanout: Option<usize>) {
        match self.kind {
            IndexKind::Boolean => self.boolean = ((take(next), 0), 1),
            IndexKind::DateRange(..) => {
                self.ranges = (0..4)
                    .map(|_| (Tree::new(take(next), IO, fanout), Vec::new()))
                    .collect();
            }
            IndexKind::Path => {
                self.path = Some(PathTrees {
                    depth: 0,
                    levels: Vec::new(),
                    parents: Tree::new(take(next), OO, fanout),
                    parent_sets: Vec::new(),
                    items: Tree::new(take(next), OI, fanout),
                });
            }
            _ => {}
        }
    }

    fn class(&self) -> (&'static str, &'static str) {
        match self.kind {
            IndexKind::Field => crate::base::FIELD_INDEX,
            IndexKind::Keyword => crate::base::KEYWORD_INDEX,
            IndexKind::Date => crate::base::DATE_INDEX,
            IndexKind::Uuid => crate::base::UUID_INDEX,
            IndexKind::Boolean => crate::base::BOOLEAN_INDEX,
            IndexKind::DateRange(..) => crate::base::DATE_RANGE_INDEX,
            IndexKind::Path => crate::base::EXTENDED_PATH_INDEX,
            IndexKind::Other(module, name) => (module, name),
        }
    }

    /// The record of the index object.
    fn record(&self) -> Vec<u8> {
        let (module, name) = self.class();
        match self.kind {
            IndexKind::Field | IndexKind::Keyword | IndexKind::Other(..) => index_record(
                module,
                name,
                &self.id,
                self.length.0,
                self.index.root,
                self.unindex.root,
                self.counter.0,
            ),
            IndexKind::Date => date_index_record(
                &self.id,
                self.index.root,
                self.unindex.root,
                self.length.0,
                self.counter.0,
            ),
            IndexKind::Uuid => uuid_index_record(
                &self.id,
                self.length.0,
                self.index.root,
                self.unindex.root,
                self.counter.0,
            ),
            IndexKind::Boolean => boolean_index_record(
                &self.id,
                &BooleanOids {
                    index: self.index.root,
                    index_length: self.boolean.0.0,
                    index_value: self.boolean.1,
                    unindex: self.unindex.root,
                    length: self.length.0,
                    counter: self.counter.0,
                },
            ),
            IndexKind::Path => {
                let trees = self.path.as_ref().expect("the trees of a path index");
                extended_path_index_record(
                    &self.id,
                    &PathOids {
                        depth: trees.depth,
                        index: self.index.root,
                        unindex: self.unindex.root,
                        length: self.length.0,
                        parents: trees.parents.root,
                        items: trees.items.root,
                        multi_valued: false,
                    },
                )
            }
            IndexKind::DateRange(since_field, until_field) => date_range_index_record(
                &self.id,
                &DateRangeOids {
                    since_field,
                    until_field,
                    always: self.index.root,
                    trees: [0, 1, 2, 3].map(|k| self.ranges[k].0.root),
                    unindex: self.unindex.root,
                    length: self.length.0,
                    counter: self.counter.0,
                },
            ),
        }
    }

    /// The value of `doc` in `_unindex`.
    fn old(&self, doc: &Val) -> Option<Val> {
        match self.unindex.get(doc) {
            Some(Item::Val(v)) => Some(v.clone()),
            _ => None,
        }
    }

    /// Indexes `datum` for `rid`, as `index_object` of the class does. Tells
    /// if the counter changes.
    fn index_object(
        &mut self,
        rid: i32,
        datum: Option<Val>,
        new_oid: &mut impl FnMut() -> u64,
    ) -> bool {
        let doc = Val::Int(i64::from(rid));
        let old = self.old(&doc);
        match self.kind {
            IndexKind::Other(..) => false,
            IndexKind::Path => {
                let Some(Val::Str(path)) = datum else {
                    return false;
                };
                match &old {
                    Some(Val::Str(old)) if **old != *path => {
                        // `unindex_object` takes one from `_length`.
                        self.path_unindex(rid, old);
                        self.length.1 += 1;
                    }
                    Some(_) => {}
                    None => self.length.1 += 1,
                }
                let comps = crate::base::components(&path);
                for (level, comp) in comps.iter().enumerate() {
                    self.insert_entry(Val::Str((*comp).into()), rid, level as i64, new_oid);
                }
                self.insert_entry(Val::None, rid, comps.len() as i64 - 1, new_oid);
                let parent = Val::Str(crate::base::parent_of(&comps).into());
                let trees = self.path.as_mut().expect("the trees of a path index");
                let row = Val::Int(i64::from(rid));
                match trees.parent_sets.iter().position(|(k, _)| *k == parent) {
                    Some(i) => trees.parent_sets[i]
                        .1
                        .set(row, Item::Val(Val::None), new_oid),
                    None => {
                        let mut set = Tree::new(new_oid(), II_SET, None);
                        set.set(row, Item::Val(Val::None), new_oid);
                        let reference = Item::Ref(set.root, II_SET.module, II_SET.tree);
                        trees.parents.set(parent.clone(), reference, new_oid);
                        trees.parent_sets.push((parent, set));
                    }
                }
                trees.items.set(
                    Val::Str(path.clone()),
                    Item::Val(Val::Int(i64::from(rid))),
                    new_oid,
                );
                self.unindex.set(doc, Item::Val(Val::Str(path)), new_oid);
                true
            }
            IndexKind::Field | IndexKind::Date | IndexKind::Uuid => {
                if old == datum {
                    return false;
                }
                if let Some(old) = &old {
                    self.remove_forward(old, rid);
                }
                match datum {
                    Some(datum) => {
                        self.insert_forward(&datum, rid, new_oid);
                        self.unindex.set(doc, Item::Val(datum), new_oid);
                    }
                    None => {
                        self.unindex.remove(&doc);
                    }
                }
                true
            }
            IndexKind::Keyword => {
                let new: Vec<Val> = match &datum {
                    Some(Val::List(items) | Val::Tuple(items)) => {
                        let mut keys: Vec<Val> = Vec::new();
                        for item in items.iter() {
                            if !keys.contains(item) {
                                keys.push(item.clone());
                            }
                        }
                        keys.sort_by(order);
                        keys
                    }
                    _ => Vec::new(),
                };
                let old: Vec<Val> = old
                    .map(|v| crate::base::keywords_of(&v).to_vec())
                    .unwrap_or_default();
                for key in old.iter().filter(|k| !new.contains(k)) {
                    self.remove_forward(key, rid);
                }
                for key in new.iter().filter(|k| !old.contains(k)) {
                    self.insert_forward(key, rid, new_oid);
                }
                if new.is_empty() {
                    self.unindex.remove(&doc);
                } else if new != old {
                    self.unindex
                        .set(doc, Item::Val(Val::List(new.into())), new_oid);
                }
                // `_index_object` of a KeywordIndex always tells of a change.
                true
            }
            IndexKind::Boolean => {
                let datum = datum.map(|v| {
                    Val::Int(i64::from(!matches!(
                        v,
                        Val::None | Val::Bool(false) | Val::Int(0)
                    )))
                });
                if old == datum {
                    return false;
                }
                if let Some(old) = &old {
                    self.boolean_remove(old, rid, false, new_oid);
                    if datum.is_none() {
                        self.unindex.remove(&doc);
                        self.length.1 -= 1;
                    }
                }
                if let Some(datum) = datum {
                    self.boolean_insert(datum, rid, new_oid);
                }
                true
            }
            IndexKind::DateRange(..) => {
                let datum = datum.unwrap_or_else(|| Val::Tuple(vec![Val::None, Val::None].into()));
                if old.as_ref() == Some(&datum) {
                    return false;
                }
                if let Some(old) = &old {
                    self.range_forward(old, rid, false, new_oid);
                }
                self.range_forward(&datum, rid, true, new_oid);
                self.unindex.set(doc, Item::Val(datum), new_oid);
                true
            }
        }
    }

    /// Unindexes `rid`, as `unindex_object` of the class does. Tells if the
    /// counter changes.
    fn unindex_object(&mut self, rid: i32, new_oid: &mut impl FnMut() -> u64) -> bool {
        let doc = Val::Int(i64::from(rid));
        let Some(old) = self.old(&doc) else {
            return false;
        };
        match self.kind {
            IndexKind::Other(..) => return false,
            IndexKind::Path => {
                if let Val::Str(path) = &old {
                    self.path_unindex(rid, path);
                }
                return true;
            }
            IndexKind::Field | IndexKind::Date | IndexKind::Uuid => self.remove_forward(&old, rid),
            IndexKind::Keyword => {
                for key in crate::base::keywords_of(&old).to_vec() {
                    self.remove_forward(&key, rid);
                }
            }
            IndexKind::Boolean => {
                self.boolean_remove(&old, rid, true, new_oid);
                self.length.1 -= 1;
            }
            IndexKind::DateRange(..) => self.range_forward(&old, rid, false, new_oid),
        }
        self.unindex.remove(&doc);
        true
    }

    /// `insertEntry` of an ExtendedPathIndex: adds `rid` to the rows of
    /// `comp` at `level` in `_index`, and raises `_depth` to `level`.
    fn insert_entry(&mut self, comp: Val, rid: i32, level: i64, new_oid: &mut impl FnMut() -> u64) {
        let trees = self.path.as_mut().expect("the trees of a path index");
        let i = match trees.levels.iter().position(|(k, _, _)| *k == comp) {
            Some(i) => i,
            None => {
                let tree = Tree::new(new_oid(), IO, None);
                let reference = Item::Ref(tree.root, IO.module, IO.tree);
                self.index.set(comp.clone(), reference, new_oid);
                trees.levels.push((comp, tree, Vec::new()));
                trees.levels.len() - 1
            }
        };
        let (_, tree, sets) = &mut trees.levels[i];
        let row = Val::Int(i64::from(rid));
        match sets.iter().position(|(l, _)| *l == level) {
            Some(j) => sets[j].1.set(row, Item::Val(Val::None), new_oid),
            None => {
                let mut set = Tree::new(new_oid(), II_SET, None);
                set.set(row, Item::Val(Val::None), new_oid);
                let reference = Item::Ref(set.root, II_SET.module, II_SET.tree);
                tree.set(Val::Int(level), reference, new_oid);
                sets.push((level, set));
            }
        }
        if level > trees.depth {
            trees.depth = level;
            self.dirty = true;
        }
    }

    /// Removes `rid` from the rows of `comp` at `level` in `_index`. `None`
    /// where ExtendedPathIndex meets a `KeyError`.
    fn remove_entry(&mut self, comp: &Val, rid: i32, level: i64) -> Option<()> {
        let trees = self.path.as_mut().expect("the trees of a path index");
        let i = trees.levels.iter().position(|(k, _, _)| k == comp)?;
        let (_, tree, sets) = &mut trees.levels[i];
        let j = sets.iter().position(|(l, _)| *l == level)?;
        sets[j].1.remove(&Val::Int(i64::from(rid)))?;
        if sets[j].1.is_empty() {
            sets.remove(j);
            tree.remove(&Val::Int(level));
        }
        if tree.is_empty() {
            trees.levels.remove(i);
            self.index.remove(comp);
        }
        Some(())
    }

    /// `unindex_object` of an ExtendedPathIndex for `rid` at `path`. Its
    /// steps stop at the first `KeyError`, as the `try` of the class does;
    /// `_length` and `_unindex` change then all the same.
    fn path_unindex(&mut self, rid: i32, path: &str) {
        let comps = crate::base::components(path);
        let _ = (|| -> Option<()> {
            for (level, comp) in comps.iter().enumerate() {
                self.remove_entry(&Val::Str((*comp).into()), rid, level as i64)?;
            }
            self.remove_entry(&Val::None, rid, comps.len() as i64 - 1)?;
            let parent = crate::base::parent_of(&comps);
            let trees = self.path.as_mut().expect("the trees of a path index");
            let key = Val::Str(parent.as_str().into());
            if let Some(i) = trees.parent_sets.iter().position(|(k, _)| *k == key) {
                trees.parent_sets[i].1.remove(&Val::Int(i64::from(rid)))?;
                if trees.parent_sets[i].1.is_empty() {
                    trees.parent_sets.remove(i);
                    trees.parents.remove(&key);
                }
            }
            // The key of `_index_items` joins the parent path and the last
            // component with a slash: `//name` for a path of one component.
            let last = comps.last().expect("a path with a component");
            trees
                .items
                .remove(&Val::Str(format!("{parent}/{last}").into()))?;
            Some(())
        })();
        self.length.1 -= 1;
        self.unindex.remove(&Val::Int(i64::from(rid)));
    }

    /// Adds `rid` to the rows of `key` in `_index`.
    fn insert_forward(&mut self, key: &Val, rid: i32, new_oid: &mut impl FnMut() -> u64) {
        let row = Val::Int(i64::from(rid));
        if self.kind == IndexKind::Uuid {
            // One row for each UUID: the first row keeps it.
            if self.index.get(key).is_none() {
                self.index.set(key.clone(), Item::Val(row), new_oid);
                self.length.1 += 1;
                self.dirty = true;
            }
            return;
        }
        match self.sets.iter().position(|(k, _)| k == key) {
            Some(i) => self.sets[i].1.set(row, Item::Val(Val::None), new_oid),
            None => {
                let mut set = Tree::new(new_oid(), II_SET, None);
                set.set(row, Item::Val(Val::None), new_oid);
                let reference = Item::Ref(set.root, II_SET.module, II_SET.tree);
                self.index.set(key.clone(), reference, new_oid);
                self.sets.push((key.clone(), set));
                self.length.1 += 1;
                self.dirty = true;
            }
        }
    }

    /// Removes `rid` from the rows of `key` in `_index`.
    fn remove_forward(&mut self, key: &Val, rid: i32) {
        if self.kind == IndexKind::Uuid {
            // The key goes, whichever row holds it.
            if self.index.remove(key).is_some() {
                self.length.1 -= 1;
                self.dirty = true;
            }
            return;
        }
        let Some(i) = self.sets.iter().position(|(k, _)| k == key) else {
            return;
        };
        self.sets[i].1.remove(&Val::Int(i64::from(rid)));
        if self.sets[i].1.is_empty() {
            self.sets.remove(i);
            self.index.remove(key);
            self.length.1 -= 1;
            self.dirty = true;
        }
    }

    /// `insertForwardIndexEntry` of a BooleanIndex.
    fn boolean_insert(&mut self, entry: Val, rid: i32, new_oid: &mut impl FnMut() -> u64) {
        let truth = entry != Val::Int(0);
        if self.length.1 == 0 {
            let value = i64::from(!truth);
            if self.boolean.1 != value {
                self.boolean.1 = value;
                self.dirty = true;
            }
        }
        let indexed = truth == (self.boolean.1 != 0);
        if indexed {
            self.boolean.0.1 += 1;
            self.index
                .set(Val::Int(i64::from(rid)), Item::Val(Val::None), new_oid);
        }
        self.unindex
            .set(Val::Int(i64::from(rid)), Item::Val(entry), new_oid);
        self.length.1 += 1;
        if indexed && self.boolean.0.1 as f64 >= self.length.1 as f64 * 0.6 {
            self.invert(None, new_oid);
        }
    }

    /// `removeForwardIndexEntry` of a BooleanIndex.
    fn boolean_remove(
        &mut self,
        entry: &Val,
        rid: i32,
        check: bool,
        new_oid: &mut impl FnMut() -> u64,
    ) {
        let truth = *entry != Val::Int(0);
        if truth == (self.boolean.1 != 0) {
            if self.index.remove(&Val::Int(i64::from(rid))).is_some() {
                self.boolean.0.1 -= 1;
            }
        } else if check && self.boolean.0.1 as f64 >= (self.length.1 - 1) as f64 * 0.6 {
            self.invert(Some(rid), new_oid);
        }
    }

    /// `_invert_index` of a BooleanIndex: `_index` holds the rows of the
    /// other value, with a new `_index_length`.
    fn invert(&mut self, without: Option<i32>, new_oid: &mut impl FnMut() -> u64) {
        self.boolean.1 = i64::from(self.boolean.1 == 0);
        for (key, _) in self.index.items().into_iter().cloned().collect::<Vec<_>>() {
            self.index.remove(&key);
        }
        let value = Val::Int(self.boolean.1);
        let rows: Vec<Val> = self
            .unindex
            .items()
            .into_iter()
            .filter(|(_, v)| *v == Item::Val(value.clone()))
            .map(|(k, _)| k.clone())
            .collect();
        let mut length = 0;
        for row in rows {
            if without.is_some_and(|rid| row == Val::Int(i64::from(rid))) {
                continue;
            }
            self.index.set(row, Item::Val(Val::None), new_oid);
            length += 1;
        }
        self.boolean.0 = (new_oid(), length);
        self.dirty = true;
    }

    /// Adds or removes `rid` in the trees of a DateRangeIndex for the
    /// value `(since, until)`.
    fn range_forward(
        &mut self,
        value: &Val,
        rid: i32,
        add: bool,
        new_oid: &mut impl FnMut() -> u64,
    ) {
        let row = Val::Int(i64::from(rid));
        let (since, until) = match value {
            Val::Tuple(items) => (items[0].clone(), items[1].clone()),
            _ => (Val::None, Val::None),
        };
        // The trees: `_since_only`, `_until_only`, `_since`, `_until`.
        let places: Vec<(usize, Val)> = match (&since, &until) {
            (Val::None, Val::None) => {
                if add {
                    self.index.set(row, Item::Val(Val::None), new_oid);
                } else {
                    self.index.remove(&row);
                }
                return;
            }
            (Val::None, until) => vec![(1, until.clone())],
            (since, Val::None) => vec![(0, since.clone())],
            (since, until) => vec![(2, since.clone()), (3, until.clone())],
        };
        for (k, key) in places {
            let (tree, sets) = &mut self.ranges[k];
            let found = sets.iter().position(|(s, _)| *s == key);
            match (add, found) {
                (true, Some(i)) => sets[i].1.set(row.clone(), Item::Val(Val::None), new_oid),
                (true, None) => {
                    let mut set = Tree::new(new_oid(), II_SET, None);
                    set.set(row.clone(), Item::Val(Val::None), new_oid);
                    tree.set(
                        key.clone(),
                        Item::Ref(set.root, II_SET.module, II_SET.tree),
                        new_oid,
                    );
                    sets.push((key, set));
                }
                (false, Some(i)) => {
                    sets[i].1.remove(&row);
                    if sets[i].1.is_empty() {
                        sets.remove(i);
                        tree.remove(&key);
                    }
                }
                (false, None) => {}
            }
        }
    }

    /// The records of the trees of the index that changed.
    fn tree_records(&mut self, out: &mut Vec<(u64, Vec<u8>)>) {
        self.index.records(out);
        self.unindex.records(out);
        for (_, set) in &mut self.sets {
            set.records(out);
        }
        for (tree, sets) in &mut self.ranges {
            tree.records(out);
            for (_, set) in sets {
                set.records(out);
            }
        }
        if let Some(trees) = &mut self.path {
            for (_, tree, sets) in &mut trees.levels {
                tree.records(out);
                for (_, set) in sets {
                    set.records(out);
                }
            }
            trees.parents.records(out);
            for (_, set) in &mut trees.parent_sets {
                set.records(out);
            }
            trees.items.records(out);
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
        Self::with_fanout(first_oid, indexes, columns, None)
    }

    /// A new catalog as `new` gives it, whose inner nodes split past
    /// `fanout` children; `None` keeps the sizes of the families.
    pub fn with_fanout(
        first_oid: u64,
        indexes: &[(&str, IndexKind)],
        columns: &[&str],
        fanout: Option<usize>,
    ) -> CatalogModel {
        let mut next = first_oid;
        let oid = take(&mut next);
        let data = Tree::new(take(&mut next), IO, fanout);
        let uids = Tree::new(take(&mut next), OI, fanout);
        let paths = Tree::new(take(&mut next), IO, fanout);
        let length = (take(&mut next), 0);
        let indexes = indexes
            .iter()
            .map(|(id, kind)| IndexModel::new(id, *kind, &mut next, fanout))
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

    /// Gives the next new objects of the model the oids from `next`, as a
    /// transaction names its new objects with provisional oids.
    pub fn next_oids_from(&mut self, next: u64) {
        self.next = next;
    }

    /// The registration key of the catalog.
    pub fn key(&self) -> [u8; 8] {
        self.oid.to_be_bytes()
    }

    /// The splits of an inner node below a root, and of a root, over the
    /// trees of the catalog.
    pub fn splits(&self) -> (usize, usize) {
        let mut trees = vec![&self.data, &self.uids, &self.paths];
        for index in &self.indexes {
            trees.push(&index.index);
            trees.push(&index.unindex);
        }
        trees.iter().fold((0, 0), |(a, b), t| {
            let (x, y) = t.splits();
            (a + x, b + y)
        })
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
            .items()
            .into_iter()
            .filter_map(|(rid, path)| match (rid, path) {
                (Val::Int(rid), Item::Val(Val::Str(path))) => Some((*rid as i32, path.to_string())),
                _ => None,
            })
            .collect()
    }

    /// Catalogs the object at `path` with `values`, as
    /// `Catalog.catalogObject` does. A new path takes the rid `rid`. An
    /// index without a value in `values` has no key for the row; a
    /// DateRangeIndex then holds `(None, None)`, as for an object without
    /// its two attributes.
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
            let datum = value_of(&index.id)
                .or_else(|| (index.kind == IndexKind::Path).then(|| Val::Str(path.into())));
            if index.index_object(rid, datum, &mut new_oid) && index.kind != IndexKind::Path {
                index.counter.1 += 1;
                self.dirty.insert(index.counter.0);
            }
        }
    }

    /// Uncatalogs the object at `path`, as `Catalog.uncatalogObject` does.
    pub fn uncatalog(&mut self, path: &str) {
        let Some(rid) = self.rid(path) else {
            return;
        };
        let next = &mut self.next;
        let mut new_oid = || take(next);
        for index in &mut self.indexes {
            if index.unindex_object(rid, &mut new_oid) && index.kind != IndexKind::Path {
                index.counter.1 += 1;
                self.dirty.insert(index.counter.0);
            }
        }
        let doc = Val::Int(i64::from(rid));
        self.data.remove(&doc);
        self.paths.remove(&doc);
        self.uids.remove(&Val::Str(path.into()));
        self.length.1 -= 1;
        self.dirty.insert(self.length.0);
    }

    /// Unindexes the object at `path` from the index `id` alone, as a call
    /// of its `unindex_object` does.
    pub fn unindex_from(&mut self, id: &str, path: &str) {
        let Some(rid) = self.rid(path) else {
            return;
        };
        let next = &mut self.next;
        let mut new_oid = || take(next);
        let index = self
            .indexes
            .iter_mut()
            .find(|i| i.id == id)
            .expect("an index of the model");
        if index.unindex_object(rid, &mut new_oid) && index.kind != IndexKind::Path {
            index.counter.1 += 1;
            self.dirty.insert(index.counter.0);
        }
    }

    /// Clears the index `id`, as `clear` of the index does: new trees and
    /// a new `_length`, so the index object changes.
    pub fn clear_index(&mut self, id: &str) {
        let next = &mut self.next;
        let index = self
            .indexes
            .iter_mut()
            .find(|i| i.id == id)
            .expect("an index of the model");
        index.length = (take(next), 0);
        let fanout = Some(index.unindex.fanout);
        let (index_family, unindex_family) = index.kind.families();
        index.index = Tree::new(take(next), index_family, fanout);
        index.unindex = Tree::new(take(next), unindex_family, fanout);
        index.sets.clear();
        index.clear_extra(next, fanout);
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
            if std::mem::take(&mut index.dirty) {
                out.push((index.oid, index.record()));
                self.dirty.insert(index.length.0);
                // An ExtendedPathIndex has no `_counter`.
                if index.kind != IndexKind::Path {
                    self.dirty.insert(index.counter.0);
                }
            }
            if index.kind == IndexKind::Path && index.length_written != Some(index.length.1) {
                index.length_written = Some(index.length.1);
                self.dirty.insert(index.length.0);
            }
            if self.dirty.remove(&index.length.0) {
                out.push((index.length.0, length_record(index.length.1)));
            }
            if self.dirty.remove(&index.counter.0) {
                out.push((index.counter.0, length_record(index.counter.1)));
            }
            if index.kind == IndexKind::Boolean && index.written != Some(index.boolean.0) {
                index.written = Some(index.boolean.0);
                out.push((index.boolean.0.0, length_record(index.boolean.0.1)));
            }
            index.tree_records(&mut out);
        }
        out
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

/// A delta of a text index, for `commit_text`.
#[cfg(feature = "text")]
#[derive(Clone, Debug)]
pub enum TextOp<'a> {
    /// The text of `rid` in `index`, in `language`.
    Set {
        /// The index id.
        index: &'a str,
        /// The rid.
        rid: i32,
        /// The language.
        language: &'a str,
        /// The text.
        text: &'a str,
    },
    /// The unindex of `rid` in `index`.
    Unindex {
        /// The index id.
        index: &'a str,
        /// The rid.
        rid: i32,
    },
    /// The clear of `index`.
    Clear {
        /// The index id.
        index: &'a str,
    },
}

/// Commits the deltas `ops` of the catalog `catalog` to `store` in one
/// transaction, with the snapshot of the last tid. `settings` writes the
/// settings of the text indexes of the catalog in `zoe.text`, and
/// `register` its registration key in `zoe.catalog`. Returns the tid.
#[cfg(feature = "text")]
pub fn commit_text(
    store: &Store,
    catalog: u64,
    ops: &[TextOp<'_>],
    settings: Option<&crate::text::CatalogSettings>,
    register: bool,
) -> Result<u64, ZoeError> {
    let before = Tid(store.last_transaction().0 + 1);
    let mut txn = store.tpc_begin(b"", b"", b"")?;
    let oid = Oid(catalog);
    for op in ops {
        match op {
            TextOp::Set {
                index,
                rid,
                language,
                text,
            } => txn.text_set(oid, index, *rid, language, text, before)?,
            TextOp::Unindex { index, rid } => txn.text_unindex(oid, index, *rid, before)?,
            TextOp::Clear { index } => txn.text_clear(oid, index)?,
        }
    }
    if let Some(settings) = settings {
        txn.kv_set(
            Flavour::Durable,
            crate::text::settings::NAMESPACE,
            &catalog.to_be_bytes(),
            &settings.to_json(),
            None,
        )?;
    }
    if register {
        txn.kv_set(
            Flavour::Durable,
            NAMESPACE,
            &catalog.to_be_bytes(),
            b"",
            None,
        )?;
    }
    txn.vote()?;
    Ok(txn.finish()?.0)
}
