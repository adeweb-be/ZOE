//! The versions of a mirror: the base, and one delta per committed tid
//! after it. A version at a tid is the base with a patch: the value of
//! each changed row at that tid. A commit and the overlay of a
//! transaction are both read the same way: the changed buckets of a tree,
//! against the version that they change.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;

use zoe_pickle::python::Value;

use crate::base::{self, Index, OPEN};
use crate::catalog::{Base, base_value};
use crate::read::{Node, Record, node};
use crate::value::{Val, encode_cell};

/// The place of a tree in a mirror.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Slot {
    /// The `_unindex` of the index at this place.
    Index(usize),
    /// `data`.
    Data,
    /// `paths`.
    Paths,
}

/// What a record of a commit is to the mirror.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Owner {
    /// A node of a tree.
    Node(Slot),
    /// The `_length` of the index at this place.
    Length(usize),
    /// The index object at this place.
    IndexObject(usize),
    /// The catalog object.
    Catalog,
}

/// The change of one row in one tree.
#[derive(Clone, Debug)]
pub struct Change {
    /// The tree.
    pub slot: Slot,
    /// The rid.
    pub rid: i32,
    /// The value before, `None` when the row had none.
    pub old: Option<Val>,
    /// The value after, `None` when the row has none.
    pub new: Option<Val>,
}

/// The changes of one tid.
#[derive(Clone, Debug, Default)]
pub struct Delta {
    /// The tid.
    pub tid: u64,
    /// The changes of the rows.
    pub changes: Vec<Change>,
    /// The changes of the `_length` of indexes: place, old, new.
    pub lengths: Vec<(usize, i64, i64)>,
    /// The changes of `_depth` of path indexes: place, old, new.
    pub depths: Vec<(usize, i64, i64)>,
}

impl Delta {
    /// The bytes that the delta takes, estimated.
    pub fn heap_bytes(&self) -> usize {
        self.changes.capacity() * std::mem::size_of::<Change>()
            + self
                .changes
                .iter()
                .map(|c| {
                    c.old.as_ref().map_or(0, Val::heap_bytes) + c.new.as_ref().map_or(0, Val::heap_bytes)
                })
                .sum::<usize>()
            + self.lengths.capacity() * 24
    }
}

/// The values of the changed rows of a version, by tree.
#[derive(Clone, Debug, Default)]
pub struct Patch {
    /// For each tree, the value of each changed rid, `None` when the rid has
    /// no value.
    pub trees: HashMap<Slot, BTreeMap<i32, Option<Val>>>,
    /// The `_length` of each index whose length changed.
    pub lengths: HashMap<usize, i64>,
    /// The `_depth` of each path index whose depth changed.
    pub depths: HashMap<usize, i64>,
}

impl Patch {
    /// The number of changed rows over all trees.
    pub fn rows(&self) -> usize {
        self.trees.values().map(BTreeMap::len).sum()
    }

    /// Takes the new values of `delta`.
    pub fn forward(&mut self, delta: &Delta) {
        for c in &delta.changes {
            self.trees
                .entry(c.slot)
                .or_default()
                .insert(c.rid, c.new.clone());
        }
        for (i, _, new) in &delta.lengths {
            self.lengths.insert(*i, *new);
        }
        for (i, _, new) in &delta.depths {
            self.depths.insert(*i, *new);
        }
    }

    /// Lays `other` over this patch.
    pub fn overlay(&mut self, other: &Patch) {
        for (slot, rows) in &other.trees {
            let mine = self.trees.entry(*slot).or_default();
            for (rid, value) in rows {
                mine.insert(*rid, value.clone());
            }
        }
        self.lengths.extend(other.lengths.iter().map(|(k, v)| (*k, *v)));
        self.depths.extend(other.depths.iter().map(|(k, v)| (*k, *v)));
    }
}

/// The value of `rid` in the tree at `slot` of `base`, without a patch.
pub fn base_slot_value(base: &Base, slot: Slot, rid: i32) -> Option<Val> {
    let row = base.rows.row(rid)?;
    match slot {
        Slot::Index(i) => base_value(&base.indexes[i], row),
        Slot::Paths => Some(Val::Str(base.paths.get(row).into())),
        Slot::Data => {
            let table = base.table.as_ref()?;
            let mut bytes = Vec::new();
            for cell in table.row(row) {
                bytes.extend_from_slice(cell);
            }
            Some(Val::Bytes(bytes.into()))
        }
    }
}

/// The value of `rid` in the tree at `slot` of a version.
pub fn slot_value(base: &Base, patch: &Patch, slot: Slot, rid: i32) -> Option<Val> {
    if let Some(rows) = patch.trees.get(&slot)
        && let Some(value) = rows.get(&rid)
    {
        return value.clone();
    }
    base_slot_value(base, slot, rid)
}

/// Tells if `rid` has a value in the tree at `slot` of `base`.
fn base_has(base: &Base, slot: Slot, row: u32) -> bool {
    match slot {
        Slot::Index(i) => match &base.indexes[i].index {
            Index::Value(v) | Index::Boolean(v) => v.reverse[row as usize] != base::NONE,
            Index::Keyword(k) => k.indexed.contains(row),
            Index::DateRange(d) => d.indexed.contains(row),
            Index::Path(p) => p.indexed.contains(row),
            Index::Unavailable(_) => false,
        },
        Slot::Paths | Slot::Data => true,
    }
}

/// The rids of the tree at `slot` of a version, from `low` to `high`.
fn rids_between(base: &Base, patch: &Patch, slot: Slot, low: i64, high: i64) -> Vec<i32> {
    let low = low.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
    let high = high.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
    let rids = &base.rows.rids;
    let start = rids.partition_point(|r| *r < low);
    let end = rids.partition_point(|r| *r <= high);
    let changed = patch.trees.get(&slot);
    let mut out = Vec::new();
    for (row, rid) in rids[start..end].iter().enumerate() {
        if changed.is_some_and(|c| c.contains_key(rid)) {
            continue;
        }
        if base_has(base, slot, (start + row) as u32) {
            out.push(*rid);
        }
    }
    if let Some(changed) = changed {
        for (rid, value) in changed.range(low..=high) {
            if value.is_some() {
                out.push(*rid);
            }
        }
    }
    out
}

/// The shape of the trees of a mirror: who owns each oid, the children of
/// each inner node, the key range of each bucket.
#[derive(Default)]
pub struct Shapes {
    /// The owner of each oid.
    pub owners: HashMap<u64, Owner>,
    /// The children of each inner node.
    pub inner: HashMap<u64, Vec<u64>>,
    /// The first and last rid of each bucket.
    pub buckets: HashMap<u64, (i64, i64)>,
}

/// The changes to the shapes that a read of records makes, laid over the
/// shapes of the mirror.
#[derive(Default)]
pub struct ShapeEdits {
    owners: HashMap<u64, Owner>,
    inner: HashMap<u64, Vec<u64>>,
    buckets: HashMap<u64, Option<(i64, i64)>>,
}

impl Shapes {
    /// Lays `edits` into the shapes.
    pub fn take(&mut self, edits: ShapeEdits) {
        self.owners.extend(edits.owners);
        self.inner.extend(edits.inner);
        for (oid, range) in edits.buckets {
            match range {
                Some(r) => {
                    self.buckets.insert(oid, r);
                }
                None => {
                    self.buckets.remove(&oid);
                }
            }
        }
    }

    /// The bytes that the shapes take, estimated.
    pub fn heap_bytes(&self) -> usize {
        self.owners.capacity() * 32
            + self.inner.capacity() * 40
            + self.inner.values().map(|v| v.capacity() * 8).sum::<usize>()
            + self.buckets.capacity() * 32
    }
}

/// What the read of the records of a commit or of an overlay gives.
#[derive(Default)]
pub struct Read {
    /// The changes of the rows.
    pub delta: Delta,
    /// The edits of the shapes.
    pub edits: ShapeEdits,
    /// The records that belong to the catalog.
    pub records: usize,
    /// Their bytes.
    pub bytes: usize,
    /// Why the mirror cannot follow, if it cannot.
    pub stale: Option<String>,
}

/// Reads the changed records `records` against the version `(base,
/// patch)` with the shapes `shapes`. The records are (oid, pickle); an
/// oid of the overlay of a transaction can be a provisional one.
pub fn read_records(
    base: &Base,
    patch: &Patch,
    shapes: &Shapes,
    records: &[(u64, Vec<u8>)],
) -> Read {
    let mut out = Read::default();
    let mut edits = ShapeEdits::default();
    let owner = |edits: &ShapeEdits, oid: u64| -> Option<Owner> {
        edits
            .owners
            .get(&oid)
            .or_else(|| shapes.owners.get(&oid))
            .copied()
    };
    // The items of each changed bucket, by tree.
    let mut new_items: HashMap<Slot, BTreeMap<i32, Val>> = HashMap::new();
    // The old key ranges of the changed buckets, by tree.
    let mut old_ranges: HashMap<Slot, Vec<(i64, i64)>> = HashMap::new();
    let mut done = vec![false; records.len()];
    loop {
        let mut progress = false;
        for (i, (oid, data)) in records.iter().enumerate() {
            if done[i] {
                continue;
            }
            let Some(who) = owner(&edits, *oid) else {
                continue;
            };
            done[i] = true;
            progress = true;
            out.records += 1;
            out.bytes += data.len();
            let record = match Record::decode(data) {
                Ok(r) => r,
                Err(e) => {
                    out.stale = Some(format!("oid {oid:#x}: {}", e.0));
                    continue;
                }
            };
            match who {
                Owner::Catalog => out.stale = Some("the catalog object changed".into()),
                Owner::IndexObject(i) => {
                    let entry = &base.indexes[i];
                    if record.attr_oid("_unindex") != entry.tree {
                        out.stale = Some(format!("the _unindex of {} changed", entry.name));
                    }
                    if let Index::Path(p) = &entry.index
                        && let Some(Value::Int(depth)) = record.attr("_depth")
                    {
                        let depth = depth as i64;
                        let old = patch.depths.get(&i).copied().unwrap_or(p.depth);
                        if depth != old {
                            out.delta.depths.push((i, old, depth));
                        }
                    }
                }
                Owner::Length(i) => {
                    if let Value::Int(n) = record.state {
                        let n = n as i64;
                        let old = patch
                            .lengths
                            .get(&i)
                            .copied()
                            .unwrap_or(base.indexes[i].length);
                        if n != old {
                            out.delta.lengths.push((i, old, n));
                        }
                    }
                }
                Owner::Node(slot) => {
                    let old_range = edits
                        .buckets
                        .get(oid)
                        .copied()
                        .unwrap_or_else(|| shapes.buckets.get(oid).copied());
                    match node(&record) {
                        Err(e) => out.stale = Some(format!("oid {oid:#x}: {}", e.0)),
                        Ok(Node::Inner(children)) => {
                            let old = edits
                                .inner
                                .get(oid)
                                .or_else(|| shapes.inner.get(oid))
                                .cloned()
                                .unwrap_or_default();
                            for child in &children {
                                if owner(&edits, *child).is_none() {
                                    edits.owners.insert(*child, Owner::Node(slot));
                                }
                            }
                            // A child that left the node takes its rows with
                            // it, unless a bucket of this read holds them.
                            for gone in old.iter().filter(|c| !children.contains(c)) {
                                if let Some(range) = shapes.buckets.get(gone) {
                                    old_ranges.entry(slot).or_default().push(*range);
                                    edits.buckets.insert(*gone, None);
                                }
                            }
                            if let Some(range) = old_range {
                                // A bucket that became a tree of buckets.
                                old_ranges.entry(slot).or_default().push(range);
                                edits.buckets.insert(*oid, None);
                            }
                            edits.inner.insert(*oid, children);
                        }
                        Ok(Node::Items(items)) => {
                            if let Some(range) = old_range {
                                old_ranges.entry(slot).or_default().push(range);
                            }
                            let target = new_items.entry(slot).or_default();
                            let mut first = None;
                            let mut last = None;
                            let mut bad = None;
                            for (key, value) in items {
                                let rid = match base::rid_of(&key) {
                                    Ok(rid) => rid,
                                    Err(e) => {
                                        bad = Some(e);
                                        break;
                                    }
                                };
                                first.get_or_insert(rid);
                                last = Some(rid);
                                let value = if slot == Slot::Data {
                                    match data_row(&value) {
                                        Some(v) => v,
                                        None => {
                                            bad = Some("a metadata record".into());
                                            break;
                                        }
                                    }
                                } else {
                                    value
                                };
                                target.insert(rid, value);
                            }
                            if let Some(e) = bad {
                                out.stale = Some(e);
                            }
                            let range = first.zip(last).map(|(a, b)| (i64::from(a), i64::from(b)));
                            edits.buckets.insert(*oid, range);
                        }
                    }
                }
            }
        }
        if !progress {
            break;
        }
    }
    // The changes of each tree: the rows of the old ranges, and the rows of
    // the new items.
    let mut slots: Vec<Slot> = new_items.keys().chain(old_ranges.keys()).copied().collect();
    slots.sort_by_key(|s| match s {
        Slot::Index(i) => *i as i64,
        Slot::Data => -1,
        Slot::Paths => -2,
    });
    slots.dedup();
    for slot in slots {
        let items = new_items.remove(&slot).unwrap_or_default();
        let mut olds: Vec<i32> = Vec::new();
        for (low, high) in old_ranges.remove(&slot).unwrap_or_default() {
            olds.extend(rids_between(base, patch, slot, low, high));
        }
        olds.sort_unstable();
        olds.dedup();
        for rid in olds.iter().filter(|r| !items.contains_key(r)) {
            let old = slot_value(base, patch, slot, *rid);
            if old.is_some() {
                out.delta.changes.push(Change {
                    slot,
                    rid: *rid,
                    old,
                    new: None,
                });
            }
        }
        for (rid, new) in items {
            let old = slot_value(base, patch, slot, rid);
            if old.as_ref() != Some(&new) {
                out.delta.changes.push(Change {
                    slot,
                    rid,
                    old,
                    new: Some(new),
                });
            }
        }
    }
    out.edits = edits;
    out
}

/// The encoded row of a metadata record.
fn data_row(value: &Val) -> Option<Val> {
    let Val::Tuple(cells) = value else {
        return None;
    };
    let mut bytes = Vec::new();
    for cell in cells.iter() {
        encode_cell(cell, &mut bytes);
    }
    Some(Val::Bytes(bytes.into()))
}

/// A mirror: its base, its deltas and the shapes of its trees.
pub struct Mirror {
    /// The base.
    pub base: Arc<Base>,
    /// The deltas after the base, by ascending tid.
    pub deltas: VecDeque<Delta>,
    /// The patch of the last tid.
    pub head: Patch,
    /// The last tid.
    pub tid: u64,
    /// The shapes.
    pub shapes: Shapes,
    /// Why the mirror cannot follow the store, if it cannot.
    pub stale: Option<String>,
}

impl Mirror {
    /// A mirror of `base`, at its tid.
    pub fn new(base: Base) -> Mirror {
        let mut shapes = Shapes::default();
        shapes.owners.insert(base.catalog, Owner::Catalog);
        let mut add = |slot: Slot, root: Option<u64>, shape: &base::TreeShape| {
            if let Some(root) = root {
                shapes.owners.insert(root, Owner::Node(slot));
            }
            for (oid, children) in &shape.inner {
                shapes.owners.insert(*oid, Owner::Node(slot));
                for c in children {
                    shapes.owners.insert(*c, Owner::Node(slot));
                }
                shapes.inner.insert(*oid, children.clone());
            }
            for (oid, range) in &shape.buckets {
                shapes.owners.insert(*oid, Owner::Node(slot));
                shapes.buckets.insert(*oid, *range);
            }
        };
        for (i, e) in base.indexes.iter().enumerate() {
            if matches!(e.index, Index::Unavailable(_)) {
                continue;
            }
            add(Slot::Index(i), e.tree, &e.shape);
        }
        add(Slot::Data, Some(base.trees.data), &base.data_shape);
        add(Slot::Paths, Some(base.trees.paths), &base.paths_shape);
        for (i, e) in base.indexes.iter().enumerate() {
            if matches!(e.index, Index::Unavailable(_)) {
                continue;
            }
            shapes.owners.insert(e.oid, Owner::IndexObject(i));
            if let Some(oid) = e.length_oid {
                shapes.owners.insert(oid, Owner::Length(i));
            }
        }
        let tid = base.tid;
        Mirror {
            base: Arc::new(base),
            deltas: VecDeque::new(),
            head: Patch::default(),
            tid,
            shapes,
            stale: None,
        }
    }

    /// Applies the records of the commit `tid`. Returns the changes.
    pub fn apply(&mut self, tid: u64, records: &[(u64, Vec<u8>)]) -> (usize, usize, usize) {
        let read = read_records(&self.base, &self.head, &self.shapes, records);
        if let Some(reason) = read.stale {
            self.stale = Some(reason);
        }
        self.shapes.take(read.edits);
        let mut delta = read.delta;
        delta.tid = tid;
        self.head.forward(&delta);
        let changes = delta.changes.len();
        self.deltas.push_back(delta);
        self.tid = tid;
        (read.records, read.bytes, changes)
    }

    /// The patch of the version at `tid`. `None` when `tid` is before the
    /// base: the versions before the base are gone.
    pub fn patch_at(&self, tid: u64) -> Option<Patch> {
        if tid < self.base.tid {
            return None;
        }
        if tid >= self.tid {
            return Some(self.head.clone());
        }
        let mut patch = Patch::default();
        for delta in self.deltas.iter().take_while(|d| d.tid <= tid) {
            patch.forward(delta);
        }
        Some(patch)
    }

    /// Drops the deltas at or before `tid` from the window. They stay in the
    /// head patch.
    pub fn deltas_bytes(&self) -> usize {
        self.deltas.iter().map(Delta::heap_bytes).sum()
    }

    /// Reads the overlay of a transaction at `tid`: the changed records of
    /// the catalog that the transaction holds, with provisional oids for
    /// its new objects. Returns the patch of the version laid with them.
    pub fn overlay(&self, tid: u64, records: &[(u64, Vec<u8>)]) -> Result<Patch, String> {
        let mut patch = self
            .patch_at(tid)
            .ok_or("the snapshot is before the window")?;
        let read = read_records(&self.base, &patch, &self.shapes, records);
        if let Some(reason) = read.stale {
            return Err(reason);
        }
        patch.forward(&read.delta);
        Ok(patch)
    }
}

/// Tells if a value is an open bound of a date range.
pub fn open(bound: i64) -> bool {
    bound == OPEN
}
