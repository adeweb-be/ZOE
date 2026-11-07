//! The build of the base of one catalog from its committed trees.
//!
//! The build reads as of one tid: `paths`, `uids`, and the record of each
//! index with its `_unindex` and its `_length`. It reads no `_index` tree.
//! It decodes each value of a tree straight into the structure of its
//! index: the keys go into arenas, which the build frees whole when the
//! structure is built. The structure of a tree builds in a thread of its
//! own, beside the walk of the next tree.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::time::Instant;

use zoe_pickle::python::Value;

use crate::base::{
    Base, CatalogTrees, DateRangeIndex, FieldIndex, Index, IndexEntry, KeywordIndex, Kind, Pairs,
    Rows, date_range_of, rid_of,
};
use crate::change::{Owner, Shapes, Slot};
use crate::read::{Reads, Record, TreeShape, oid_of, walk_tree};
use crate::value::Val;

/// The class of the catalog object.
pub const CATALOG: (&str, &str) = ("Products.ZCatalog.Catalog", "Catalog");

/// A base, with what the maintenance of its mirror needs.
pub struct Built {
    /// The base.
    pub base: Base,
    /// The shapes of the mirrored trees.
    pub shapes: Shapes,
    /// The pickles of the catalog object and of each mirrored index
    /// object, by oid: a commit that writes other bytes for one of them
    /// makes the mirror stale.
    pub objects: HashMap<u64, Vec<u8>>,
    /// The records that the build read.
    pub records: usize,
    /// The bytes of these records.
    pub bytes: usize,
    /// The seconds of the build.
    pub seconds: f64,
}

/// The values of an `_unindex` tree as a build collects them.
enum Part {
    /// The keys and their rows: one pair for each key of a row.
    Pairs(Pairs),
    /// The bounds of each row of a DateRangeIndex.
    Ranges(Vec<(u32, i64, i64)>),
}

impl Part {
    fn heap_bytes(&self) -> usize {
        match self {
            Part::Pairs(p) => p.heap_bytes(),
            Part::Ranges(v) => v.capacity() * 24,
        }
    }

    fn append(&mut self, other: Part) {
        match (self, other) {
            (Part::Pairs(a), Part::Pairs(b)) => a.append(b),
            (Part::Ranges(a), Part::Ranges(b)) => a.extend(b),
            _ => {}
        }
    }

    /// Adds the value of `_unindex` of `row`, for an index of `kind`.
    fn push(&mut self, kind: Kind, value: Val, row: u32) -> Result<(), String> {
        match (self, kind) {
            (Part::Ranges(v), _) => {
                let (since, until) = date_range_of(&value)
                    .ok_or_else(|| format!("a value of a DateRangeIndex: {value:?}"))?;
                v.push((row, since, until));
            }
            (Part::Pairs(p), Kind::Keyword) => match value {
                Val::List(items) | Val::Tuple(items) => {
                    for key in items.into_vec() {
                        p.push(key, row);
                    }
                }
                other => {
                    return Err(format!(
                        "a value of a KeywordIndex that is not a list: {other:?}"
                    ));
                }
            },
            (Part::Pairs(p), _) => p.push(value, row),
        }
        Ok(())
    }
}

/// Why a build gives no base.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BuildError {
    /// The catalog gets no mirror: the reason.
    Refused(String),
    /// The memory bound has no room for the build.
    Dropped,
}

/// Builds the base of the catalog at `catalog` as of `tid`, with
/// `threads` threads. The base holds each commit up to `tid`. `room` tells
/// if the bound has room for a build that holds the given bytes; when it
/// has none, the build stops with `Dropped`.
pub fn build(
    reads: &dyn Reads,
    catalog: u64,
    tid: u64,
    threads: usize,
    room: &(dyn Fn(usize) -> bool + Sync),
) -> Result<Built, BuildError> {
    let started = Instant::now();
    let before = tid.saturating_add(1);
    let refused = |what: &str, e: String| BuildError::Refused(format!("{what}: {e}"));
    let load = |oid: u64| -> Result<Vec<u8>, String> {
        reads
            .load_before(oid, before)?
            .ok_or_else(|| format!("no record {oid:#x}"))
    };
    let mut read_records = 1usize;
    let catalog_bytes = load(catalog).map_err(|e| refused("the catalog", e))?;
    let mut read_bytes = catalog_bytes.len();
    let record = Record::decode(&catalog_bytes).map_err(|e| refused("the catalog", e.0))?;
    if !record.is(CATALOG.0, CATALOG.1) {
        return Err(BuildError::Refused(format!(
            "{catalog:#x} is a {}.{}, not a Catalog",
            record.module, record.name
        )));
    }
    let trees = CatalogTrees {
        paths: record
            .attr_oid("paths")
            .ok_or_else(|| BuildError::Refused("the catalog has no paths".into()))?,
        uids: record
            .attr_oid("uids")
            .ok_or_else(|| BuildError::Refused("the catalog has no uids".into()))?,
    };
    let names: Vec<(String, Value)> = match record.attr("indexes").and_then(|v| record.heap.dict(v))
    {
        Some(items) => {
            let mut names = Vec::new();
            for (k, v) in items {
                let name = record
                    .heap
                    .str(*k)
                    .ok_or_else(|| BuildError::Refused("an index without a str id".into()))?;
                names.push((name.to_owned(), *v));
            }
            names.sort_by(|a, b| a.0.cmp(&b.0));
            names
        }
        None => {
            return Err(BuildError::Refused(
                "the catalog has no dict of indexes".into(),
            ));
        }
    };
    // The rows: the rids of `paths`, with their paths.
    let (paths_shape, parts, stats) = walk_tree(
        reads,
        before,
        trees.paths,
        threads,
        Vec::new,
        |part: &mut Vec<(i32, String)>, items| {
            for (rid, path) in items {
                let path = path.text().ok_or("a path of paths that is not a str")?;
                part.push((rid_of(&rid)?, path.to_owned()));
            }
            Ok(())
        },
    )
    .map_err(|e| refused("paths", e))?;
    read_records += stats.records;
    read_bytes += stats.bytes;
    let mut pairs: Vec<(i32, String)> = parts.into_iter().flatten().collect();
    pairs.sort_unstable_by_key(|(rid, _)| *rid);
    if pairs.windows(2).any(|w| w[0].0 == w[1].0) {
        return Err(BuildError::Refused("paths holds a rid twice".into()));
    }
    let rows = Rows::from_sorted(&pairs);
    drop(pairs);
    let held = AtomicUsize::new(rows.heap_bytes() + paths_shape.heap_bytes());
    if !room(held.load(AtomicOrdering::Relaxed)) {
        return Err(BuildError::Dropped);
    }
    // `uids` must be the inverse of `paths`.
    let rows_ref = &rows;
    let (_, counts, stats) = walk_tree(
        reads,
        before,
        trees.uids,
        threads,
        || 0usize,
        |count: &mut usize, items| {
            for (path, rid) in items {
                let rid = rid_of(&rid)?;
                let found = rows_ref
                    .row(rid)
                    .is_some_and(|row| Some(rows_ref.paths.get(row)) == path.text());
                if !found {
                    return Err(format!(
                        "uids maps a path to the rid {rid}, which paths does not give it"
                    ));
                }
                *count += 1;
            }
            Ok(())
        },
    )
    .map_err(|e| refused("uids is not the inverse of paths", e))?;
    read_records += stats.records;
    read_bytes += stats.bytes;
    if counts.iter().sum::<usize>() != rows.len() {
        return Err(BuildError::Refused(
            "uids is not the inverse of paths: they differ in length".into(),
        ));
    }
    let mut shapes = Shapes::default();
    shapes.owners.insert(catalog, Owner::Catalog);
    shapes.add_tree(Slot::Paths, trees.paths, &paths_shape);
    let mut objects = HashMap::from([(catalog, catalog_bytes)]);
    // The indexes. The structure of each index builds in a thread
    // while the next tree is walked.
    let mut entries: Vec<IndexEntry> = Vec::with_capacity(names.len());
    let mut tree_shapes: Vec<Option<TreeShape>> = Vec::with_capacity(names.len());
    let mut stopped = false;
    std::thread::scope(|scope| {
        let mut pending = Vec::new();
        for (name, reference) in &names {
            let Some(oid) = oid_of(&record.heap, *reference) else {
                pending.push(None);
                entries.push(IndexEntry {
                    name: name.clone(),
                    class: String::new(),
                    kind: None,
                    oid: 0,
                    tree: None,
                    length_oid: None,
                    length: -1,
                    index: Index::Unavailable("the index is not a persistent object".into()),
                });
                tree_shapes.push(None);
                continue;
            };
            let mut entry = IndexEntry {
                name: name.clone(),
                class: String::new(),
                kind: None,
                oid,
                tree: None,
                length_oid: None,
                length: -1,
                index: Index::Unavailable(String::new()),
            };
            let read = (|| -> Result<Option<(Part, TreeShape, Vec<u8>)>, String> {
                let bytes = load(oid)?;
                read_records += 1;
                read_bytes += bytes.len();
                let index_record = Record::decode(&bytes).map_err(|e| e.0)?;
                entry.class = format!("{}.{}", index_record.module, index_record.name);
                entry.kind = Kind::of(&index_record.module, &index_record.name);
                let Some(kind) = entry.kind else {
                    return Ok(None);
                };
                let make = || match kind {
                    Kind::DateRange => Part::Ranges(Vec::new()),
                    _ => Part::Pairs(Pairs::Empty),
                };
                let tree = index_record.attr_oid("_unindex").ok_or("no _unindex")?;
                let length_oid = index_record.attr_oid("_length").ok_or("no _length")?;
                entry.tree = Some(tree);
                entry.length_oid = Some(length_oid);
                let length = load(length_oid)?;
                read_records += 1;
                read_bytes += length.len();
                entry.length = Record::decode(&length)
                    .map_err(|e| e.0)?
                    .int_state()
                    .ok_or("a _length that is not an int")?;
                let (shape, parts, stats) = walk_tree(
                    reads,
                    before,
                    tree,
                    threads,
                    make,
                    |part: &mut Part, items| {
                        for (rid, value) in items {
                            let rid = rid_of(&rid)?;
                            let row = rows_ref
                                .row(rid)
                                .ok_or_else(|| format!("the rid {rid} is not in paths"))?;
                            part.push(kind, value, row)?;
                        }
                        Ok(())
                    },
                )?;
                read_records += stats.records;
                read_bytes += stats.bytes;
                let mut all = make();
                for part in parts {
                    all.append(part);
                }
                Ok(Some((all, shape, bytes)))
            })();
            match read {
                Ok(None) => {
                    entry.index =
                        Index::Unavailable(format!("the class {} is not mirrored", entry.class));
                    pending.push(None);
                    tree_shapes.push(None);
                }
                Err(e) => {
                    entry.index = Index::Unavailable(e);
                    pending.push(None);
                    tree_shapes.push(None);
                }
                Ok(Some((part, shape, bytes))) => {
                    let mut transient = part.heap_bytes() + shape.heap_bytes();
                    if let (Some(Kind::Keyword), Part::Pairs(pairs)) = (entry.kind, &part) {
                        transient += KeywordIndex::build_bytes(pairs);
                    }
                    if !room(held.load(AtomicOrdering::Relaxed) + transient) {
                        stopped = true;
                        pending.push(None);
                        tree_shapes.push(None);
                        entries.push(entry);
                        break;
                    }
                    objects.insert(oid, bytes);
                    tree_shapes.push(Some(shape));
                    let held = &held;
                    let count = rows_ref.len();
                    let kind = entry.kind;
                    pending.push(Some(scope.spawn(move || {
                        let built = match (kind, part) {
                            (Some(Kind::Keyword), Part::Pairs(pairs)) => {
                                KeywordIndex::build(pairs, count)
                                    .map(|i| Index::Keyword(Arc::new(i)))
                            }
                            (_, Part::Ranges(items)) => Ok(Index::DateRange(Arc::new(
                                DateRangeIndex::build(items, count),
                            ))),
                            (_, Part::Pairs(pairs)) => {
                                FieldIndex::build(pairs, count).map(|i| Index::Field(Arc::new(i)))
                            }
                        };
                        if let Ok(index) = &built {
                            held.fetch_add(index.heap_bytes(), AtomicOrdering::Relaxed);
                        }
                        built
                    })));
                }
            }
            entries.push(entry);
        }
        for (entry, handle) in entries.iter_mut().zip(pending) {
            if let Some(handle) = handle {
                entry.index = match handle.join() {
                    Ok(Ok(index)) => index,
                    Ok(Err(e)) => Index::Unavailable(e),
                    Err(_) => Index::Unavailable("the build of the index panicked".into()),
                };
            }
        }
    });
    if stopped || !room(held.load(AtomicOrdering::Relaxed)) {
        return Err(BuildError::Dropped);
    }
    for (i, (entry, shape)) in entries.iter().zip(&tree_shapes).enumerate() {
        let mirrored = !matches!(entry.index, Index::Unavailable(_));
        if let (true, Some(shape), Some(tree)) = (mirrored, shape, entry.tree) {
            shapes.owners.insert(entry.oid, Owner::IndexObject(i));
            if let Some(length) = entry.length_oid {
                shapes.owners.insert(length, Owner::Length(i));
            }
            shapes.add_tree(Slot::Index(i), tree, shape);
        } else {
            objects.remove(&entry.oid);
        }
    }
    let by_name = entries
        .iter()
        .enumerate()
        .map(|(i, e)| (e.name.clone(), i))
        .collect();
    Ok(Built {
        base: Base {
            tid,
            catalog,
            rows: Arc::new(rows),
            indexes: entries,
            by_name,
            trees,
        },
        shapes,
        objects,
        records: read_records,
        bytes: read_bytes,
        seconds: started.elapsed().as_secs_f64(),
    })
}
