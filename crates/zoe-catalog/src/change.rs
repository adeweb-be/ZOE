//! The change sets of a mirror, and the read of the records of a commit.
//!
//! A change set holds what one commit changed in the mirrored trees: the
//! old and the new value of each changed row of each tree, and the old
//! and the new `_length` of each index. The overrides of a version are the
//! value at its tid of each row that a change set changed since the base.
//!
//! The read of a commit looks up each record in the map of the oids of the
//! mirrored trees. It decodes the records that the mirror owns, registers
//! the new buckets that a changed inner node names, and compares the items
//! of each changed bucket with the rows of the old key range of the
//! bucket. A record of another object costs one lookup.

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};
use std::time::Instant;

use crate::base::{Base, Index, rid_of};
use crate::read::{Node, Record, TreeShape, node};
use crate::value::{Val, py_cmp};

/// The place of a tree in a mirror.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Slot {
    /// `paths` of the catalog.
    Paths,
    /// The `_unindex` of the index at this place.
    Index(usize),
}

/// What a record is to the mirror.
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

/// The shapes of the mirrored trees: the owner of each oid, the children
/// of each inner node, the key range of each bucket.
#[derive(Clone, Debug, Default)]
pub struct Shapes {
    /// The owner of each oid.
    pub owners: HashMap<u64, Owner>,
    /// The children of each inner node.
    pub inner: HashMap<u64, Vec<u64>>,
    /// The first and the last rid of each bucket.
    pub buckets: HashMap<u64, (i64, i64)>,
}

impl Shapes {
    /// Adds the tree at `root` of `slot`, with its shape.
    pub fn add_tree(&mut self, slot: Slot, root: u64, shape: &TreeShape) {
        self.owners.insert(root, Owner::Node(slot));
        for (oid, children) in &shape.inner {
            self.owners.insert(*oid, Owner::Node(slot));
            for child in children {
                self.owners.insert(*child, Owner::Node(slot));
            }
            self.inner.insert(*oid, children.clone());
        }
        for (oid, range) in &shape.buckets {
            self.owners.insert(*oid, Owner::Node(slot));
            self.buckets.insert(*oid, *range);
        }
    }

    /// Lays `edits` into the shapes.
    pub fn take(&mut self, edits: ShapeEdits) {
        self.owners.extend(edits.owners);
        self.inner.extend(edits.inner);
        for (oid, range) in edits.buckets {
            match range {
                Some(r) => self.buckets.insert(oid, r),
                None => self.buckets.remove(&oid),
            };
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

/// The changes that a read makes to the shapes.
#[derive(Debug, Default)]
pub struct ShapeEdits {
    owners: HashMap<u64, Owner>,
    inner: HashMap<u64, Vec<u64>>,
    buckets: HashMap<u64, Option<(i64, i64)>>,
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

/// What one commit changed in the mirrored trees.
#[derive(Clone, Debug)]
pub struct ChangeSet {
    /// The tid of the commit.
    pub tid: u64,
    /// When the mirror added the change set.
    pub at: Instant,
    /// The changes of the rows.
    pub changes: Vec<Change>,
    /// The changes of the `_length` of the indexes: place, old, new.
    pub lengths: Vec<(usize, i64, i64)>,
}

impl ChangeSet {
    /// The bytes that the change set takes, estimated.
    pub fn heap_bytes(&self) -> usize {
        self.changes.capacity() * std::mem::size_of::<Change>()
            + self
                .changes
                .iter()
                .map(|c| {
                    c.old.as_ref().map_or(0, Val::heap_bytes)
                        + c.new.as_ref().map_or(0, Val::heap_bytes)
                })
                .sum::<usize>()
            + self.lengths.capacity() * 24
            + 64
    }
}

/// The overrides of a version: the value of each row that a change set
/// changed since the base, by tree, and the `_length` of each index whose
/// length changed.
#[derive(Clone, Debug, Default)]
pub struct Overrides {
    /// The value of each changed rid, by tree. `None` when the rid has no
    /// value.
    pub trees: BTreeMap<Slot, BTreeMap<i32, Option<Val>>>,
    /// The `_length` of each index whose length changed.
    pub lengths: HashMap<usize, i64>,
}

impl Overrides {
    /// The number of changed rows over all trees.
    pub fn rows(&self) -> usize {
        self.trees.values().map(BTreeMap::len).sum()
    }

    /// Takes the new values of `set`, a change set after the version.
    pub fn forward(&mut self, set: &ChangeSet) {
        for c in &set.changes {
            self.trees
                .entry(c.slot)
                .or_default()
                .insert(c.rid, c.new.clone());
        }
        for (i, _, new) in &set.lengths {
            self.lengths.insert(*i, *new);
        }
    }

    /// Takes the old values of `set`, a change set after the version and
    /// before the base. Call it from the newest change set to the oldest,
    /// so the oldest wins.
    pub fn backward(&mut self, set: &ChangeSet) {
        for c in &set.changes {
            self.trees
                .entry(c.slot)
                .or_default()
                .insert(c.rid, c.old.clone());
        }
        for (i, old, _) in &set.lengths {
            self.lengths.insert(*i, *old);
        }
    }

    /// The bytes that the overrides take, estimated.
    pub fn heap_bytes(&self) -> usize {
        self.trees
            .values()
            .flat_map(|rows| rows.values())
            .map(|v| 48 + v.as_ref().map_or(0, Val::heap_bytes))
            .sum::<usize>()
            + self.lengths.capacity() * 16
    }
}

/// Tells if two values are one key of an index.
pub fn same(a: &Val, b: &Val) -> bool {
    match py_cmp(a, b) {
        Some(order) => order == Ordering::Equal,
        None => a == b,
    }
}

/// The value of `rid` in the tree at `slot` of `base`.
pub fn base_value(base: &Base, slot: Slot, rid: i32) -> Option<Val> {
    let row = base.rows.row(rid)?;
    match slot {
        Slot::Paths => base
            .rows
            .live
            .contains(row)
            .then(|| Val::Str(base.rows.paths.get(row).into())),
        Slot::Index(i) => match &base.indexes.get(i)?.index {
            Index::Field(f) => f.slot(row).map(|s| f.keys.get(s)),
            Index::Unavailable(_) => None,
        },
    }
}

/// The value of `rid` in the tree at `slot` of the version of `base` with
/// `overrides`.
pub fn value(base: &Base, overrides: &Overrides, slot: Slot, rid: i32) -> Option<Val> {
    if let Some(rows) = overrides.trees.get(&slot)
        && let Some(found) = rows.get(&rid)
    {
        return found.clone();
    }
    base_value(base, slot, rid)
}

/// The rids that have a value in the tree at `slot` of a version, from
/// `low` to `high`.
fn rids_between(base: &Base, overrides: &Overrides, slot: Slot, low: i64, high: i64) -> Vec<i32> {
    let low = low.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
    let high = high.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
    let changed = overrides.trees.get(&slot);
    let mut out = Vec::new();
    for (rid, row) in base.rows.between(low, high) {
        if changed.is_some_and(|c| c.contains_key(&rid)) {
            continue;
        }
        let has = match slot {
            Slot::Paths => base.rows.live.contains(row),
            Slot::Index(i) => {
                matches!(&base.indexes[i].index, Index::Field(f) if f.slot(row).is_some())
            }
        };
        if has {
            out.push(rid);
        }
    }
    if let Some(changed) = changed {
        out.extend(
            changed
                .range(low..=high)
                .filter(|(_, v)| v.is_some())
                .map(|(rid, _)| *rid),
        );
    }
    out
}

/// What the read of the records of a commit gives.
#[derive(Debug, Default)]
pub struct Read {
    /// The changes of the rows.
    pub changes: Vec<Change>,
    /// The changes of the `_length` of the indexes.
    pub lengths: Vec<(usize, i64, i64)>,
    /// The edits of the shapes.
    pub edits: ShapeEdits,
    /// The records that the mirror owns.
    pub records: usize,
    /// Why the mirror cannot follow the commit, if it cannot.
    pub stale: Option<String>,
}

/// Reads the records of a commit against the version of `base` with
/// `overrides`. `objects` holds the pickles of the catalog object and of
/// the mirrored index objects that the build read.
pub fn read_records(
    base: &Base,
    overrides: &Overrides,
    shapes: &Shapes,
    objects: &HashMap<u64, Vec<u8>>,
    records: &[(u64, Option<Vec<u8>>)],
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
    let mut new_items: BTreeMap<Slot, BTreeMap<i32, Val>> = BTreeMap::new();
    // The old key ranges of the changed buckets, by tree.
    let mut old_ranges: BTreeMap<Slot, Vec<(i64, i64)>> = BTreeMap::new();
    let mut done = vec![false; records.len()];
    // A changed inner node names its new buckets, which can come before it
    // in the commit: read until no record is left to read.
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
            let Some(data) = data else {
                out.stale = Some(format!("the commit deletes the object {oid:#x}"));
                continue;
            };
            if matches!(who, Owner::Catalog | Owner::IndexObject(_)) {
                if objects.get(oid) != Some(data) {
                    out.stale = Some(match who {
                        Owner::IndexObject(i) => {
                            format!("the commit changes the index {}", base.indexes[i].name)
                        }
                        _ => "the commit changes the catalog object".into(),
                    });
                }
                continue;
            }
            let record = match Record::decode(data) {
                Ok(r) => r,
                Err(e) => {
                    out.stale = Some(format!("oid {oid:#x}: {}", e.0));
                    continue;
                }
            };
            match who {
                Owner::Catalog | Owner::IndexObject(_) => {}
                Owner::Length(i) => match record.int_state() {
                    Some(n) => {
                        let old = overrides
                            .lengths
                            .get(&i)
                            .copied()
                            .unwrap_or(base.indexes[i].length);
                        if n != old {
                            out.lengths.push((i, old, n));
                        }
                    }
                    None => out.stale = Some(format!("the _length {oid:#x} is not an int")),
                },
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
                            // A child that left the node takes its rows
                            // with it, unless a bucket of this commit holds
                            // them.
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
                            let mut range: Option<(i64, i64)> = None;
                            for (key, value) in items {
                                let rid = match rid_of(&key) {
                                    Ok(rid) => rid,
                                    Err(e) => {
                                        out.stale = Some(e);
                                        break;
                                    }
                                };
                                let r = i64::from(rid);
                                range = Some(range.map_or((r, r), |(a, _)| (a, r)));
                                target.insert(rid, value);
                            }
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
    // The changes of each tree: the rows of the old ranges that no new
    // bucket holds, and the new items.
    let slots: Vec<Slot> = new_items
        .keys()
        .chain(old_ranges.keys())
        .copied()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    for slot in slots {
        let items = new_items.remove(&slot).unwrap_or_default();
        let mut olds: Vec<i32> = Vec::new();
        for (low, high) in old_ranges.remove(&slot).unwrap_or_default() {
            olds.extend(rids_between(base, overrides, slot, low, high));
        }
        olds.sort_unstable();
        olds.dedup();
        for rid in olds.iter().filter(|r| !items.contains_key(r)) {
            let old = value(base, overrides, slot, *rid);
            if old.is_some() {
                out.changes.push(Change {
                    slot,
                    rid: *rid,
                    old,
                    new: None,
                });
            }
        }
        for (rid, new) in items {
            let old = value(base, overrides, slot, rid);
            if !old
                .as_ref()
                .is_some_and(|old| same(old, &new) && kind(old) == kind(&new))
            {
                out.changes.push(Change {
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

/// The kind of a value, so a key of another type is a change, as a new
/// object in the tree is.
fn kind(value: &Val) -> u8 {
    match value {
        Val::None => 0,
        Val::Missing => 1,
        Val::Bool(_) | Val::Int(_) => 2,
        Val::Float(_) => 3,
        Val::Str(_) | Val::Message(_) => 4,
        Val::Bytes(_) => 5,
        Val::Tuple(_) => 6,
        Val::List(_) => 7,
        Val::DateTime(..) => 8,
    }
}
