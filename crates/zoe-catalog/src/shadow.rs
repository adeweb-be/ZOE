//! The shadow rebuild of a mirror (ADR 0059, section 5).
//!
//! Some commits change what the mirror reads: a new root of a mirrored
//! tree, a new pickle of the catalog object or of an index object, or a
//! new record that an unavailable index refused before. The change sets
//! cannot follow the structures that such a commit changed (`Changed`).
//! The mirror keeps its base, and its change sets keep the changes of the
//! other structures. A query at a tid before the stale commit reads the
//! base as before. A query at that tid or later that reads a changed
//! structure takes the Python path (`stale`); every other query stays
//! native.
//!
//! The shadow rebuild (`build`) reads the changed structures as of the tid
//! of the stale commit, one structure at a time, beside the base. The
//! other structures come from the version of the mirror at that tid: a
//! rebase of the base. The shadow then applies the later commits to the
//! changed structures, in tid order (`Shadow::catch_up`). When it reached
//! the last commit that the mirror applied, the mirror lays the new base
//! in at one step (`swap`): the change sets lose the changes of the
//! changed structures and take those of the shadow. A query at a tid
//! before the stale commit that reads a changed structure then takes
//! `outside_window`: the base does not hold that structure at those tids.
//!
//! The copy of a shadow rebuild is at most the changed structures. New
//! rows would need a second whole base, so a commit that changes them
//! starts no shadow rebuild: the mirror drops its base and builds again
//! from nothing. So does a shadow rebuild that fails, for want of room
//! or for another reason.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::Arc;
use std::time::Instant;

use crate::base::{Base, Index, IndexEntry};
use crate::build::{
    BuildError, add_index_shapes, build_index, catalog_meta, read_index, read_table,
};
use crate::change::{ChangeSet, Overrides, Owner, Shapes, Slot, Unfollowed, read_records};
use crate::mirror::{Snapshot, Versions, rebase};
use crate::query::{Columns, Plan};
use crate::read::{Commit, Reads};

/// The structures that commits changed beyond what the change sets follow.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Changed {
    /// `paths` or `uids`: each structure builds again.
    pub rows: bool,
    /// The metadata table: `data` or the schema of the catalog.
    pub table: bool,
    /// The indexes, by id: an added or a removed index, a new pickle of the
    /// index object, a tree that the change sets cannot follow.
    pub indexes: BTreeSet<String>,
}

impl Changed {
    /// Tells if no structure changed.
    pub fn is_empty(&self) -> bool {
        !self.rows && !self.table && self.indexes.is_empty()
    }

    /// Adds the structures of `other`.
    pub fn add(&mut self, other: &Changed) {
        self.rows |= other.rows;
        self.table |= other.table;
        self.indexes.extend(other.indexes.iter().cloned());
    }

    /// Tells if a query of `plan` with the rows `columns` reads a changed
    /// structure: the rows, an index of a clause or of the sort, or the
    /// table for a reply with rows.
    pub fn needed_by(&self, plan: &Plan, columns: &Columns) -> bool {
        self.rows
            || (self.table && columns.cols.is_some())
            || plan.clauses.iter().any(|c| self.indexes.contains(&c.index))
            || plan
                .sort
                .iter()
                .any(|(name, _)| self.indexes.contains(name))
    }

    /// Tells if the index at place `i` of `indexes` changed.
    pub fn has_index(&self, indexes: &[IndexEntry], i: usize) -> bool {
        self.rows
            || indexes
                .get(i)
                .is_none_or(|e| self.indexes.contains(&e.name))
    }

    /// Tells if the tree at `slot` of a base with `indexes` changed.
    pub fn has_slot(&self, indexes: &[IndexEntry], slot: Slot) -> bool {
        match slot {
            Slot::Paths => self.rows,
            Slot::Data | Slot::Column(_) | Slot::Width => self.rows || self.table,
            Slot::Index(i) => self.has_index(indexes, i),
        }
    }

    /// Tells if the owner `who` of a base with `indexes` belongs to a
    /// changed structure. The catalog object belongs to none.
    pub fn has_owner(&self, indexes: &[IndexEntry], who: Owner) -> bool {
        match who {
            Owner::Catalog => false,
            Owner::Node(slot) => self.has_slot(indexes, slot),
            Owner::Length(i) | Owner::IndexObject(i) | Owner::Unavailable(i) => {
                self.has_index(indexes, i)
            }
        }
    }
}

/// The structures that a commit changed, from what the read of its records
/// did not follow. `objects` holds the last pickle of the catalog object
/// that the mirror read: a new pickle that changes no tree, no schema and
/// no index changes nothing.
pub fn changed_by(
    base: &Base,
    objects: &HashMap<u64, Vec<u8>>,
    unfollowed: &[Unfollowed],
) -> Changed {
    let mut out = Changed::default();
    let index = |out: &mut Changed, i: usize| match base.indexes.get(i) {
        Some(e) => {
            out.indexes.insert(e.name.clone());
        }
        None => out.rows = true,
    };
    for what in unfollowed {
        match what {
            Unfollowed::Tree(Slot::Paths) | Unfollowed::Catalog(None) => out.rows = true,
            Unfollowed::Tree(Slot::Index(i)) | Unfollowed::Index(i, _) | Unfollowed::Refused(i) => {
                index(&mut out, *i)
            }
            Unfollowed::Tree(_) => out.table = true,
            Unfollowed::Catalog(Some(new)) => {
                let old = objects
                    .get(&base.catalog)
                    .and_then(|bytes| catalog_meta(base.catalog, bytes).ok());
                let (Some(old), Ok(new)) = (old, catalog_meta(base.catalog, new)) else {
                    out.rows = true;
                    continue;
                };
                out.rows |= old.trees.paths != new.trees.paths || old.trees.uids != new.trees.uids;
                out.table |= old.trees.data != new.trees.data || old.schema != new.schema;
                let before: BTreeMap<&str, Option<u64>> =
                    old.indexes.iter().map(|(n, o)| (n.as_str(), *o)).collect();
                let after: BTreeMap<&str, Option<u64>> =
                    new.indexes.iter().map(|(n, o)| (n.as_str(), *o)).collect();
                for name in before.keys().chain(after.keys()) {
                    if before.get(name) != after.get(name) {
                        out.indexes.insert((*name).to_owned());
                    }
                }
            }
        }
    }
    out
}

/// The change set `set` without the changes of the trees that `keep` does
/// not keep, each tree at the place that `keep` gives it. `None` when it
/// equals `set`.
fn mapped(
    set: &ChangeSet,
    keep: &dyn Fn(Slot) -> Option<Slot>,
    keep_length: &dyn Fn(usize) -> Option<usize>,
) -> Option<ChangeSet> {
    let same = set.changes.iter().all(|c| keep(c.slot) == Some(c.slot))
        && set
            .lengths
            .iter()
            .all(|(i, ..)| keep_length(*i) == Some(*i));
    if same {
        return None;
    }
    let changes = set
        .changes
        .iter()
        .filter_map(|c| {
            keep(c.slot).map(|slot| {
                let mut c = c.clone();
                c.slot = slot;
                c
            })
        })
        .collect();
    let lengths = set
        .lengths
        .iter()
        .filter_map(|(i, old, new)| keep_length(*i).map(|i| (i, *old, *new)))
        .collect();
    Some(ChangeSet {
        tid: set.tid,
        at: set.at,
        changes,
        lengths,
    })
}

/// The change set `set` without the changes of the structures that
/// `changed` holds, for a base with `indexes`.
pub fn strip(set: ChangeSet, indexes: &[IndexEntry], changed: &Changed) -> ChangeSet {
    if changed.is_empty() {
        return set;
    }
    let keep = |slot: Slot| (!changed.has_slot(indexes, slot)).then_some(slot);
    let keep_length = |i: usize| (!changed.has_index(indexes, i)).then_some(i);
    mapped(&set, &keep, &keep_length).unwrap_or(set)
}

/// Why a shadow rebuild gives no base.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShadowError {
    /// A commit at this tid changed a tree that the shadow rebuilt in a way
    /// that its change sets cannot follow: the shadow starts again there.
    Again(u64),
    /// The shadow failed: the reason.
    Failed(String),
}

/// What a shadow rebuild reads: the structures that changed, as of the tid
/// of the last stale commit, and the version of the mirror at that tid.
pub struct Job {
    /// The oid of the catalog.
    pub catalog: u64,
    /// The tid of the last stale commit.
    pub tid: u64,
    /// The structures that changed.
    pub changed: Changed,
    /// The version of the mirror at `tid`.
    pub snapshot: Snapshot,
}

/// What a shadow rebuild built: a new base at the tid of the stale commit,
/// and the changes of the changed structures after it.
pub struct Shadow {
    /// The tid of the stale commit.
    pub tid: u64,
    /// The structures that changed.
    pub changed: Changed,
    /// The new base, at `tid`.
    pub base: Base,
    /// The shapes of the trees that changed, at `applied`.
    pub shapes: Shapes,
    /// The pickles of the index objects that changed.
    pub objects: HashMap<u64, Vec<u8>>,
    /// The tid of the last change of the shape of each node of `shapes`.
    pub moved: HashMap<u64, u64>,
    /// The change sets of the changed structures after `tid`, by ascending
    /// tid.
    pub sets: Vec<ChangeSet>,
    /// The overrides of `sets` over `base`.
    over: Overrides,
    /// The last tid that the shadow applied.
    pub applied: u64,
    /// The bytes of the structures and the shapes that the shadow built:
    /// those that it shares with the mirror do not count.
    held: usize,
    /// The seconds of the build.
    pub seconds: f64,
}

/// The bytes of the structures of `new` that `old` does not share. `new` is
/// a rebase of `old`, so its indexes and its columns are in the same order.
fn own_bytes(new: &Base, old: &Base) -> usize {
    let mut bytes = 0;
    if !Arc::ptr_eq(&new.rows, &old.rows) {
        bytes += new.rows.heap_bytes();
    }
    for (a, b) in new.indexes.iter().zip(&old.indexes) {
        if !a.index.shares(&b.index) {
            bytes += a.index.heap_bytes();
        }
    }
    for (a, b) in new.table.columns.iter().zip(&old.table.columns) {
        if !Arc::ptr_eq(a, b) {
            bytes += a.heap_bytes();
        }
    }
    if !Arc::ptr_eq(&new.table.widths, &old.table.widths) {
        bytes += new.table.widths.capacity() * 4;
    }
    bytes
}

/// Builds the shadow of `job` with `threads` threads. `room` tells if the
/// bound has room for a shadow that holds the given bytes beside the
/// mirrors; when it has none, the shadow fails.
pub fn build(
    reads: &dyn Reads,
    job: Job,
    threads: usize,
    room: &(dyn Fn(usize) -> bool + Sync),
) -> Result<Shadow, ShadowError> {
    let started = Instant::now();
    let Job {
        catalog,
        tid,
        changed,
        snapshot,
    } = job;
    let failed = |e: BuildError| {
        ShadowError::Failed(match e {
            BuildError::Refused(why) => why,
            BuildError::Dropped => "the memory bound has no room for the shadow rebuild".into(),
        })
    };
    let no_room =
        || ShadowError::Failed("the memory bound has no room for the shadow rebuild".into());
    // New rows need a second whole base: the mirror builds again from
    // nothing instead.
    if changed.rows {
        return Err(ShadowError::Failed("the rows changed".into()));
    }
    let (base, shapes, mut objects, held) = {
        let before = tid.saturating_add(1);
        let bytes = reads
            .load_before(catalog, before)
            .and_then(|found| found.ok_or_else(|| format!("no record {catalog:#x}")))
            .map_err(|e| ShadowError::Failed(format!("the catalog: {e}")))?;
        let meta = catalog_meta(catalog, &bytes).map_err(failed)?;
        let (old, over) = snapshot.resolve();
        // The other structures: the version at the tid, without the changes
        // of the changed structures.
        let kept = Overrides {
            trees: over
                .trees
                .iter()
                .filter(|(slot, _)| !changed.has_slot(&old.indexes, **slot))
                .map(|(slot, rows)| (*slot, rows.clone()))
                .collect(),
            lengths: over
                .lengths
                .iter()
                .filter(|(i, _)| !changed.has_index(&old.indexes, **i))
                .map(|(i, n)| (*i, *n))
                .collect(),
        };
        drop(over);
        let rebased = rebase(&old, &kept, tid, room).map_err(ShadowError::Failed)?;
        drop(kept);
        let mut held = own_bytes(&rebased, &old);
        drop(old);
        let rows = Arc::clone(&rebased.rows);
        let mut shapes = Shapes::default();
        let mut objects = HashMap::new();
        let mut indexes = Vec::with_capacity(meta.indexes.len());
        for (name, oid) in &meta.indexes {
            if !changed.indexes.contains(name)
                && let Some(&i) = rebased.by_name.get(name)
            {
                // A structure that did not change keeps its shapes in the
                // mirror.
                indexes.push(rebased.indexes[i].clone());
                continue;
            }
            let mut read = read_index(reads, before, name, *oid, &rows, threads);
            if !room(held + read.transient()) {
                return Err(no_room());
            }
            let place = indexes.len();
            let found = match read.found.take() {
                Some((part, shape, bytes)) => {
                    read.entry.index =
                        match build_index(read.entry.kind, part, &rows, read.path_record) {
                            Ok(index) => index,
                            Err(e) => Index::Unavailable(e),
                        };
                    read.watch = vec![read.entry.oid];
                    Some((shape, bytes))
                }
                None => {
                    // An index that its rebuild refuses again watches only
                    // its index object: a record that each commit rewrites
                    // starts no rebuild at each commit.
                    read.watch.retain(|o| *o == read.entry.oid);
                    None
                }
            };
            held += read.entry.index.heap_bytes()
                + found.as_ref().map_or(0, |(shape, _)| shape.heap_bytes());
            if !room(held) {
                return Err(no_room());
            }
            add_index_shapes(
                &mut shapes,
                &mut objects,
                place,
                &read.entry,
                found,
                &read.watch,
            );
            indexes.push(read.entry);
        }
        let table = if changed.table {
            let (part, shape, _) = read_table(
                reads,
                before,
                meta.trees.data,
                meta.schema.len(),
                &rows,
                threads,
                room,
                held,
            )
            .map_err(failed)?;
            // A table that does not build serves no row.
            let table = part.build(meta.schema, rows.len()).unwrap_or_default();
            held += table.heap_bytes() + shape.heap_bytes();
            if !room(held) {
                return Err(no_room());
            }
            shapes.add_tree(Slot::Data, meta.trees.data, &shape);
            table
        } else {
            rebased.table.clone()
        };
        let by_name = indexes
            .iter()
            .enumerate()
            .map(|(i, e): (usize, &IndexEntry)| (e.name.clone(), i))
            .collect();
        let base = Base {
            tid,
            catalog,
            rows,
            indexes,
            by_name,
            trees: meta.trees,
            table,
        };
        (base, shapes, objects, held)
    };
    objects.remove(&catalog);
    let moved = shapes.owners.keys().map(|oid| (*oid, tid)).collect();
    Ok(Shadow {
        tid,
        changed,
        base,
        shapes,
        objects,
        moved,
        sets: Vec::new(),
        over: Overrides::default(),
        applied: tid,
        held,
        seconds: started.elapsed().as_secs_f64(),
    })
}

impl Shadow {
    /// Applies `commit`, a commit after `applied`, to the changed
    /// structures. A commit that changes them in a way that a change set
    /// cannot follow gives its tid: the shadow starts again there.
    pub fn catch_up(&mut self, commit: &Commit) -> Result<(), u64> {
        if commit.tid <= self.applied {
            return Ok(());
        }
        let got = read_records(
            &self.base,
            &self.over,
            &self.shapes,
            &self.objects,
            &commit.records,
        );
        if got.stale.is_some() || !got.unfollowed.is_empty() {
            return Err(commit.tid);
        }
        for oid in self.shapes.take(got.edits) {
            self.moved.insert(oid, commit.tid);
        }
        self.applied = commit.tid;
        if !got.changes.is_empty() || !got.lengths.is_empty() {
            let set = ChangeSet {
                tid: commit.tid,
                at: Instant::now(),
                changes: got.changes,
                lengths: got.lengths,
            };
            self.over.forward(&set);
            self.sets.push(set);
        }
        Ok(())
    }

    /// The bytes that the shadow holds beside the mirror: the structures
    /// that it built, and its change sets.
    pub fn heap_bytes(&self) -> usize {
        self.held
            + self.sets.iter().map(ChangeSet::heap_bytes).sum::<usize>()
            + self.over.heap_bytes()
    }
}

/// What a swap lays: the new versions, and the new shapes, pickles and
/// moved nodes of the maintenance of the mirror.
pub struct Swapped {
    /// The new versions.
    pub versions: Versions,
    /// The shapes of the mirrored trees.
    pub shapes: Shapes,
    /// The pickles of the catalog object and of the mirrored index objects.
    pub objects: HashMap<u64, Vec<u8>>,
    /// The tid of the last change of the shape of each node.
    pub moved: HashMap<u64, u64>,
}

/// Lays `shadow` over the versions `old` and the maintenance of the mirror
/// (`shapes`, `objects`, `moved`), which follow each commit up to
/// `old.applied`, as the shadow does. The structures that did not change,
/// their change sets and their shapes stay; those that changed come from
/// the shadow.
pub fn swap(
    old: &Versions,
    shapes: &Shapes,
    objects: &HashMap<u64, Vec<u8>>,
    moved: &HashMap<u64, u64>,
    shadow: Shadow,
) -> Swapped {
    let Shadow {
        tid,
        changed,
        base,
        shapes: new_shapes,
        objects: new_objects,
        moved: new_moved,
        sets: new_sets,
        applied,
        ..
    } = shadow;
    let indexes = &old.base.indexes;
    // The place of each index of the old base in the new base.
    let places: Vec<Option<usize>> = indexes
        .iter()
        .enumerate()
        .map(|(i, e)| {
            if changed.has_index(indexes, i) {
                None
            } else {
                base.by_name.get(&e.name).copied()
            }
        })
        .collect();
    let keep = |slot: Slot| -> Option<Slot> {
        if changed.has_slot(indexes, slot) {
            return None;
        }
        match slot {
            Slot::Index(i) => places[i].map(Slot::Index),
            other => Some(other),
        }
    };
    let keep_length = |i: usize| places.get(i).copied().flatten();
    let mut merged: BTreeMap<u64, Arc<ChangeSet>> = BTreeMap::new();
    if !changed.rows {
        for set in &old.sets {
            let set = match mapped(set, &keep, &keep_length) {
                Some(set) => Arc::new(set),
                None => Arc::clone(set),
            };
            merged.insert(set.tid, set);
        }
    }
    for set in new_sets {
        match merged.get_mut(&set.tid) {
            Some(found) => {
                let found = Arc::make_mut(found);
                found.changes.extend(set.changes);
                found.lengths.extend(set.lengths);
            }
            None => {
                merged.insert(set.tid, Arc::new(set));
            }
        }
    }
    let sets: VecDeque<Arc<ChangeSet>> = merged
        .into_values()
        .filter(|s| !s.changes.is_empty() || !s.lengths.is_empty())
        .collect();
    let mut head = Overrides::default();
    for set in sets.iter().filter(|s| s.tid > tid) {
        head.forward(set);
    }
    let floor = if changed.rows {
        old.floor.max(tid)
    } else {
        old.floor
    };
    let versions = Versions {
        base: Arc::new(base),
        sets,
        floor,
        head: Arc::new(head),
        applied,
    };
    // The shapes of the structures that did not change, at their new
    // places, and those of the shadow.
    let mut out = Shapes::default();
    for (oid, who) in &shapes.owners {
        if changed.has_owner(indexes, *who) {
            continue;
        }
        let who = match *who {
            Owner::Node(slot) => keep(slot).map(Owner::Node),
            Owner::Length(i) => places[i].map(Owner::Length),
            Owner::IndexObject(i) => places[i].map(Owner::IndexObject),
            Owner::Unavailable(i) => places[i].map(Owner::Unavailable),
            Owner::Catalog => Some(Owner::Catalog),
        };
        let Some(who) = who else {
            continue;
        };
        out.owners.insert(*oid, who);
        if let Some(children) = shapes.inner.get(oid) {
            out.inner.insert(*oid, children.clone());
        }
        if let Some(range) = shapes.buckets.get(oid) {
            out.buckets.insert(*oid, *range);
        }
    }
    out.owners.extend(new_shapes.owners);
    out.inner.extend(new_shapes.inner);
    out.buckets.extend(new_shapes.buckets);
    let mut kept_objects: HashMap<u64, Vec<u8>> = objects
        .iter()
        .filter(|(oid, _)| {
            matches!(
                out.owners.get(oid),
                Some(Owner::Catalog | Owner::IndexObject(_))
            )
        })
        .map(|(oid, bytes)| (*oid, bytes.clone()))
        .collect();
    kept_objects.extend(new_objects);
    let mut moved = moved.clone();
    moved.extend(new_moved);
    Swapped {
        versions,
        shapes: out,
        objects: kept_objects,
        moved,
    }
}
