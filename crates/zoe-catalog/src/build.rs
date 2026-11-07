//! The build of the base of one catalog from its committed trees.
//!
//! The build reads as of one tid: `paths`, `uids`, `data`, and the record
//! of each index with its `_unindex` and its `_length`; a `ZoeSecurityIndex`
//! gives `_inputs` in the place of `_unindex`. It reads no `_index` tree.
//! It decodes each value of a tree straight into the structure of its
//! index: the keys go into arenas, which the build frees whole when the
//! structure is built. The structure of a tree builds in a thread of its
//! own, beside the walk of the next tree.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering as AtomicOrdering};
use std::time::Instant;

use zoe_pickle::python::Value;

use crate::base::{
    Base, CatalogTrees, DateRangeIndex, FieldIndex, Index, IndexEntry, KeywordIndex, Kind, Pairs,
    PathIndex, Rows, date_range_of, rid_of,
};
use crate::change::{Owner, Shapes, Slot};
use crate::security::SecurityIndex;
use crate::read::{RECORDS, Reads, Record, TreeShape, oid_of, walk_tree, walk_tree_with};
use crate::table::{Table, TablePart};
use crate::value::{StrArena, Val};

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
    /// The path of each row of an ExtendedPathIndex: the paths, and the row
    /// and the place of each.
    Paths(StrArena, Vec<(u32, u32)>),
    /// The inputs of each row of a `ZoeSecurityIndex`.
    Inputs(Vec<(u32, Val)>),
}

impl Part {
    fn heap_bytes(&self) -> usize {
        match self {
            Part::Pairs(p) => p.heap_bytes(),
            Part::Ranges(v) => v.capacity() * 24,
            Part::Paths(texts, v) => texts.heap_bytes() + v.capacity() * 8,
            Part::Inputs(v) => v.iter().map(|(_, value)| value.heap_bytes() + 8).sum(),
        }
    }

    fn append(&mut self, other: Part) {
        match (self, other) {
            (Part::Pairs(a), Part::Pairs(b)) => a.append(b),
            (Part::Ranges(a), Part::Ranges(b)) => a.extend(b),
            (Part::Paths(texts, a), Part::Paths(other, b)) => {
                let offset = texts.len() as u32;
                texts.extend(&other);
                a.extend(b.into_iter().map(|(r, p)| (r, p + offset)));
            }
            (Part::Inputs(a), Part::Inputs(b)) => a.extend(b),
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
            (Part::Inputs(v), _) => v.push((row, value)),
            (Part::Paths(texts, v), _) => match value {
                Val::Str(path) => v.push((row, texts.push(&path))),
                other => return Err(format!("a path that is not a str: {other:?}")),
            },
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
        data: record
            .attr_oid("data")
            .ok_or_else(|| BuildError::Refused("the catalog has no data".into()))?,
    };
    let schema = schema_of(&record);
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
    // The records of `data`, cell by cell. A record of a rid that `paths`
    // does not hold is in no result: the table does not keep it. The cells
    // count against the bound as the walk collects them.
    let width = schema.len();
    let walked = AtomicUsize::new(0);
    let over = AtomicBool::new(false);
    let walk = walk_tree_with(
        reads,
        before,
        trees.data,
        threads,
        RECORDS,
        || TablePart::new(width),
        |part: &mut TablePart, items| {
            let start = part.heap_bytes();
            for (rid, value) in items {
                if let Some(row) = rows_ref.row(rid_of(&rid)?) {
                    part.push(row, &value)?;
                }
            }
            let grown = part.heap_bytes().saturating_sub(start);
            let now = walked.fetch_add(grown, AtomicOrdering::Relaxed) + grown;
            if !room(held.load(AtomicOrdering::Relaxed) + now) {
                over.store(true, AtomicOrdering::Relaxed);
                return Err("the memory bound has no room for data".into());
            }
            Ok(())
        },
    );
    if over.load(AtomicOrdering::Relaxed) {
        return Err(BuildError::Dropped);
    }
    let (data_shape, parts, stats) = walk.map_err(|e| refused("data", e))?;
    read_records += stats.records;
    read_bytes += stats.bytes;
    let mut table_part = TablePart::new(width);
    for part in parts {
        table_part.append(part).map_err(|e| refused("data", e))?;
    }
    if !room(held.load(AtomicOrdering::Relaxed) + table_part.heap_bytes() + data_shape.heap_bytes())
    {
        return Err(BuildError::Dropped);
    }
    let mut shapes = Shapes::default();
    shapes.owners.insert(catalog, Owner::Catalog);
    shapes.add_tree(Slot::Paths, trees.paths, &paths_shape);
    shapes.add_tree(Slot::Data, trees.data, &data_shape);
    drop(data_shape);
    let mut objects = HashMap::from([(catalog, catalog_bytes)]);
    // The indexes. The structure of each index builds in a thread
    // while the next tree is walked.
    let mut entries: Vec<IndexEntry> = Vec::with_capacity(names.len());
    let mut tree_shapes: Vec<Option<TreeShape>> = Vec::with_capacity(names.len());
    let mut stopped = false;
    let mut table = Table::default();
    std::thread::scope(|scope| {
        let count = rows_ref.len();
        let held_ref = &held;
        let table_build = scope.spawn(move || {
            // A table that does not build serves no row.
            let table = table_part.build(schema, count).unwrap_or_default();
            held_ref.fetch_add(table.heap_bytes(), AtomicOrdering::Relaxed);
            table
        });
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
            // `_depth` and `multi_valued` of an ExtendedPathIndex.
            let mut path_record = (0i64, false);
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
                if kind == Kind::Path {
                    path_record = path_meta(&index_record)?;
                }
                let make = || match kind {
                    Kind::DateRange => Part::Ranges(Vec::new()),
                    Kind::Path => Part::Paths(StrArena::default(), Vec::new()),
                    Kind::Security => Part::Inputs(Vec::new()),
                    _ => Part::Pairs(Pairs::Empty),
                };
                let tree = index_record
                    .attr_oid(kind.tree())
                    .ok_or_else(|| format!("no {}", kind.tree()))?;
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
                    match (entry.kind, &part) {
                        (Some(Kind::Keyword), Part::Pairs(pairs)) => {
                            transient += KeywordIndex::build_bytes(pairs);
                        }
                        (_, Part::Paths(texts, items)) => {
                            transient += PathIndex::build_bytes(texts, items);
                        }
                        _ => {}
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
                    let (depth, multi_valued) = path_record;
                    pending.push(Some(scope.spawn(move || {
                        let built = match (kind, part) {
                            (Some(Kind::Keyword), Part::Pairs(pairs)) => {
                                KeywordIndex::build(pairs, count)
                                    .map(|i| Index::Keyword(Arc::new(i)))
                            }
                            (_, Part::Ranges(items)) => Ok(Index::DateRange(Arc::new(
                                DateRangeIndex::build(items, count),
                            ))),
                            (_, Part::Paths(texts, items)) => Ok(Index::Path(Arc::new(
                                PathIndex::build(texts, items, count, depth, multi_valued),
                            ))),
                            (_, Part::Inputs(items)) => SecurityIndex::build(items, rows_ref)
                                .map(|i| Index::Security(Arc::new(i))),
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
        // A table that did not build serves no row: each brain then comes
        // from `data`.
        if let Ok(built) = table_build.join() {
            table = built;
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
            table,
        },
        shapes,
        objects,
        records: read_records,
        bytes: read_bytes,
        seconds: started.elapsed().as_secs_f64(),
    })
}

/// The position of each column of the `schema` of the catalog record. It
/// is empty when the schema is not a dict of strs to ints that gives each
/// position from 0 once: `Catalog.instantiate` then reads the records in a
/// way that the table does not model, and no brain comes from a row.
fn schema_of(record: &Record) -> HashMap<String, usize> {
    let Some(items) = record.attr("schema").and_then(|v| record.heap.dict(v)) else {
        return HashMap::new();
    };
    let mut out = HashMap::with_capacity(items.len());
    for (k, v) in items {
        let (Some(name), Value::Int(place)) = (record.heap.str(*k), *v) else {
            return HashMap::new();
        };
        let Ok(place) = usize::try_from(place) else {
            return HashMap::new();
        };
        out.insert(name.to_owned(), place);
    }
    let mut seen = vec![false; out.len()];
    for place in out.values() {
        match seen.get_mut(*place) {
            Some(slot) if !*slot => *slot = true,
            _ => return HashMap::new(),
        }
    }
    out
}

/// `_depth` and `multi_valued` of the record of an ExtendedPathIndex. A
/// record without `multi_valued` takes the default of the class: false.
fn path_meta(record: &Record) -> Result<(i64, bool), String> {
    let depth = match record.attr("_depth") {
        Some(Value::Int(i)) => i64::try_from(i).map_err(|_| "a _depth beyond 64 bits")?,
        other => return Err(format!("a _depth that is not an int: {other:?}")),
    };
    let multi_valued = match record.attr("multi_valued") {
        None => false,
        Some(Value::Bool(b)) => b,
        Some(Value::Int(i)) => i != 0,
        Some(other) => return Err(format!("a multi_valued that is not a bool: {other:?}")),
    };
    Ok((depth, multi_valued))
}
