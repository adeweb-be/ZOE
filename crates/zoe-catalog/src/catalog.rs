//! The build of the base of one catalog from its committed trees.

use std::collections::HashMap;
use std::time::Instant;

use zoe_pickle::python::Value;

use crate::base::{
    self, Index, IndexEntry, Rows, Table, TreeShape, build_date_range, build_keyword, build_path,
    build_table, build_value,
};
use crate::read::{Record, Source, Walk, collect_tree, oid_of};
use crate::value::{StrArena, Val};

/// The kinds of the index classes that the mirror reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// FieldIndex.
    Field,
    /// DateIndex.
    Date,
    /// UUIDIndex.
    Uuid,
    /// BooleanIndex.
    Boolean,
    /// KeywordIndex.
    Keyword,
    /// DateRangeIndex.
    DateRange,
    /// ExtendedPathIndex.
    Path,
}

/// The kind of an index class, or `None` for a class the mirror does not
/// read.
pub fn kind_of(module: &str, name: &str) -> Option<Kind> {
    Some(match (module, name) {
        ("Products.PluginIndexes.FieldIndex.FieldIndex", "FieldIndex") => Kind::Field,
        ("Products.PluginIndexes.DateIndex.DateIndex", "DateIndex") => Kind::Date,
        ("Products.PluginIndexes.UUIDIndex.UUIDIndex", "UUIDIndex") => Kind::Uuid,
        ("Products.PluginIndexes.BooleanIndex.BooleanIndex", "BooleanIndex") => Kind::Boolean,
        ("Products.PluginIndexes.KeywordIndex.KeywordIndex", "KeywordIndex") => Kind::Keyword,
        ("Products.PluginIndexes.DateRangeIndex.DateRangeIndex", "DateRangeIndex") => {
            Kind::DateRange
        }
        ("Products.ExtendedPathIndex.ExtendedPathIndex", "ExtendedPathIndex") => Kind::Path,
        _ => return None,
    })
}

/// The base of a catalog, with what its build read.
pub struct Base {
    /// The tid of the base.
    pub tid: u64,
    /// The oid of the catalog.
    pub catalog: u64,
    /// The rows.
    pub rows: Rows,
    /// The indexes, in the order of their names.
    pub indexes: Vec<IndexEntry>,
    /// The place of each index, by name.
    pub by_name: HashMap<String, usize>,
    /// The metadata table.
    pub table: Option<Table>,
    /// The path of each row, from `paths`.
    pub paths: StrArena,
    /// The oids of `data`, `paths` and `uids`.
    pub trees: CatalogTrees,
    /// The shapes of `data` and `paths`.
    pub data_shape: TreeShape,
    /// The shape of `paths`.
    pub paths_shape: TreeShape,
}

/// The oids of the trees of the catalog itself.
#[derive(Clone, Copy, Debug, Default)]
pub struct CatalogTrees {
    /// `data`.
    pub data: u64,
    /// `paths`.
    pub paths: u64,
    /// `uids`.
    pub uids: u64,
}

/// What a build read and how long each part took.
#[derive(Debug, Default)]
pub struct BuildReport {
    /// The records read.
    pub records: usize,
    /// The bytes of the records read.
    pub bytes: usize,
    /// For each part: the name, the records, the bytes, the seconds of the
    /// walk, the seconds of the build of the structures.
    pub parts: Vec<(String, usize, usize, f64, f64)>,
    /// The seconds of the whole build.
    pub seconds: f64,
}

fn str_items(record: &Record, name: &str) -> Vec<(String, Value)> {
    let Some(value) = record.attr(name) else {
        return Vec::new();
    };
    let Some(items) = record.heap.dict(value) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|(k, v)| Some((record.heap.str(*k)?.to_owned(), *v)))
        .collect()
}

fn int_attr(record: &Record, name: &str) -> Option<i64> {
    match record.attr(name)? {
        Value::Int(i) => i64::try_from(i).ok(),
        Value::Bool(b) => Some(i64::from(b)),
        _ => None,
    }
}

fn length_of(source: &dyn Source, oid: Option<u64>) -> i64 {
    let Some(oid) = oid else { return -1 };
    source
        .load(oid)
        .ok()
        .and_then(|data| Record::decode(&data).ok())
        .and_then(|r| match r.state {
            Value::Int(i) => i64::try_from(i).ok(),
            _ => None,
        })
        .unwrap_or(-1)
}

fn shape_of(walk: &Walk) -> TreeShape {
    TreeShape {
        inner: walk.inner.clone(),
        buckets: walk.buckets.clone(),
    }
}

/// Builds the base of the catalog at `catalog` with `threads` threads.
/// `with_table` also builds the metadata table.
pub fn build(
    source: &dyn Source,
    catalog: u64,
    tid: u64,
    threads: usize,
    with_table: bool,
    only: Option<&[String]>,
) -> Result<(Base, BuildReport), String> {
    let started = Instant::now();
    let mut report = BuildReport::default();
    let record = Record::decode(&source.load(catalog)?).map_err(|e| e.0)?;
    if (record.module.as_str(), record.name.as_str()) != ("Products.ZCatalog.Catalog", "Catalog") {
        return Err(format!("{}.{} is not a Catalog", record.module, record.name));
    }
    let trees = CatalogTrees {
        data: record.attr_oid("data").ok_or("the catalog has no data")?,
        paths: record.attr_oid("paths").ok_or("the catalog has no paths")?,
        uids: record.attr_oid("uids").ok_or("the catalog has no uids")?,
    };
    // The rows: the keys of `paths`, with their paths.
    let t = Instant::now();
    let walk = collect_tree(source, trees.paths, threads)?;
    let walked = t.elapsed().as_secs_f64();
    let t = Instant::now();
    let mut pairs: Vec<(i32, Box<str>)> = Vec::with_capacity(walk.items.len());
    for (rid, path) in &walk.items {
        pairs.push((
            base::rid_of(rid)?,
            path.text().ok_or("a path of paths")?.into(),
        ));
    }
    pairs.sort_unstable_by_key(|(rid, _)| *rid);
    let rows = Rows {
        rids: pairs.iter().map(|(rid, _)| *rid).collect(),
    };
    let mut paths = StrArena::default();
    for (_, path) in &pairs {
        paths.push(path);
    }
    paths.shrink();
    drop(pairs);
    report.records += walk.records;
    report.bytes += walk.bytes;
    report.parts.push((
        "paths".into(),
        walk.records,
        walk.bytes,
        walked,
        t.elapsed().as_secs_f64(),
    ));
    let paths_shape = shape_of(&walk);
    drop(walk);
    // `uids`: read, as the build reads every tree of the catalog, and
    // checked against `paths`.
    let t = Instant::now();
    let walk = collect_tree(source, trees.uids, threads)?;
    let walked = t.elapsed().as_secs_f64();
    let t = Instant::now();
    let mut mismatch = walk.items.len() != rows.len();
    if !mismatch {
        for (path, rid) in &walk.items {
            let rid = base::rid_of(rid)?;
            let ok = rows
                .row(rid)
                .is_some_and(|row| Some(paths.get(row)) == path.text());
            if !ok {
                mismatch = true;
                break;
            }
        }
    }
    if mismatch {
        return Err("uids is not the inverse of paths".into());
    }
    report.records += walk.records;
    report.bytes += walk.bytes;
    report.parts.push((
        "uids".into(),
        walk.records,
        walk.bytes,
        walked,
        t.elapsed().as_secs_f64(),
    ));
    drop(walk);
    // The indexes.
    let mut indexes = Vec::new();
    let mut names = str_items(&record, "indexes");
    names.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, reference) in names {
        let oid = oid_of(&record.heap, reference).ok_or("an index without an oid")?;
        let index_record = Record::decode(&source.load(oid)?).map_err(|e| e.0)?;
        let class = format!("{}.{}", index_record.module, index_record.name);
        report.records += 1;
        let skipped = only.is_some_and(|o| !o.contains(&name));
        let kind = if skipped {
            None
        } else {
            kind_of(&index_record.module, &index_record.name)
        };
        let Some(kind) = kind else {
            indexes.push(IndexEntry {
                name,
                class: class.clone(),
                oid,
                tree: None,
                length_oid: None,
                length: -1,
                index: Index::Unavailable(if skipped {
                    "left out of this build".into()
                } else {
                    format!("the class {class} is not mirrored")
                }),
                shape: TreeShape::default(),
            });
            continue;
        };
        let tree = index_record.attr_oid("_unindex");
        let length_oid = index_record.attr_oid("_length");
        let length = length_of(source, length_oid);
        let multi = int_attr(&index_record, "multi_valued").unwrap_or(0);
        let Some(tree_oid) = tree else {
            indexes.push(IndexEntry {
                name,
                class,
                oid,
                tree,
                length_oid,
                length,
                index: Index::Unavailable("no _unindex".into()),
                shape: TreeShape::default(),
            });
            continue;
        };
        let t = Instant::now();
        let walk = collect_tree(source, tree_oid, threads)?;
        let walked = t.elapsed().as_secs_f64();
        let t = Instant::now();
        let shape = shape_of(&walk);
        report.records += walk.records;
        report.bytes += walk.bytes;
        let (records, bytes) = (walk.records, walk.bytes);
        let items = walk.items;
        let built = match kind {
            Kind::Field | Kind::Date | Kind::Uuid => build_value(&rows, items).map(Index::Value),
            Kind::Boolean => build_value(&rows, items).map(Index::Boolean),
            Kind::Keyword => build_keyword(&rows, items).map(Index::Keyword),
            Kind::DateRange => build_date_range(&rows, items).map(Index::DateRange),
            Kind::Path if multi != 0 => Err("a multi-valued path index".into()),
            Kind::Path => {
                let depth = int_attr(&index_record, "_depth").unwrap_or(0);
                build_path(&rows, items, depth).map(Index::Path)
            }
        };
        let index = built.unwrap_or_else(Index::Unavailable);
        report.parts.push((
            name.clone(),
            records,
            bytes,
            walked,
            t.elapsed().as_secs_f64(),
        ));
        indexes.push(IndexEntry {
            name,
            class,
            oid,
            tree,
            length_oid,
            length,
            index,
            shape,
        });
    }
    // The metadata table.
    let mut data_shape = TreeShape::default();
    let table = if with_table {
        let mut schema: Vec<(String, i64)> = str_items(&record, "schema")
            .into_iter()
            .filter_map(|(k, v)| match v {
                Value::Int(i) => Some((k, i64::try_from(i).ok()?)),
                _ => None,
            })
            .collect();
        schema.sort_by_key(|(_, i)| *i);
        let names: Vec<String> = schema.into_iter().map(|(k, _)| k).collect();
        let t = Instant::now();
        let walk = collect_tree(source, trees.data, threads)?;
        let walked = t.elapsed().as_secs_f64();
        let t = Instant::now();
        report.records += walk.records;
        report.bytes += walk.bytes;
        let (records, bytes) = (walk.records, walk.bytes);
        data_shape = shape_of(&walk);
        let table = build_table(&rows, names, walk.items)?;
        report.parts.push((
            "data".into(),
            records,
            bytes,
            walked,
            t.elapsed().as_secs_f64(),
        ));
        Some(table)
    } else {
        None
    };
    let by_name = indexes
        .iter()
        .enumerate()
        .map(|(i, e)| (e.name.clone(), i))
        .collect();
    report.seconds = started.elapsed().as_secs_f64();
    Ok((
        Base {
            tid,
            catalog,
            rows,
            indexes,
            by_name,
            table,
            paths,
            trees,
            data_shape,
            paths_shape,
        },
        report,
    ))
}

impl Base {
    /// The bytes of each part: the rows, each index, the table, the paths.
    pub fn memory(&self) -> Vec<(String, String, usize)> {
        let mut out = vec![(
            "rows".to_owned(),
            "rows".to_owned(),
            self.rows.rids.capacity() * 4,
        )];
        out.push((
            "paths".into(),
            "catalog paths".into(),
            self.paths.heap_bytes(),
        ));
        for e in &self.indexes {
            let kind = match &e.index {
                Index::Value(_) => "value",
                Index::Boolean(_) => "boolean",
                Index::Keyword(_) => "keyword",
                Index::DateRange(_) => "daterange",
                Index::Path(_) => "path",
                Index::Unavailable(_) => "unavailable",
            };
            out.push((e.name.clone(), kind.into(), e.index.heap_bytes()));
        }
        if let Some(table) = &self.table {
            for (name, codes, cells, _) in table.column_bytes() {
                out.push((format!("data:{name}"), "column".into(), codes + cells));
            }
        }
        let shapes: usize = self.indexes.iter().map(|e| e.shape.heap_bytes()).sum::<usize>()
            + self.data_shape.heap_bytes()
            + self.paths_shape.heap_bytes();
        out.push(("shapes".into(), "tree shapes".into(), shapes));
        out
    }
}

/// Reads the value of a key of an index of the base, for a row.
pub fn base_value(entry: &IndexEntry, row: u32) -> Option<Val> {
    match &entry.index {
        Index::Value(v) | Index::Boolean(v) => {
            let slot = v.reverse[row as usize];
            (slot != base::NONE).then(|| v.keys.get(slot))
        }
        Index::Keyword(k) => k
            .indexed
            .contains(row)
            .then(|| Val::List(k.row_slots(row).iter().map(|s| k.keys.get(*s)).collect())),
        Index::DateRange(d) => d.indexed.contains(row).then(|| {
            let b = |x: i64| if x == base::OPEN { Val::None } else { Val::Int(x) };
            Val::Tuple(vec![b(d.since[row as usize]), b(d.until[row as usize])].into())
        }),
        Index::Path(p) => p
            .indexed
            .contains(row)
            .then(|| Val::Str(p.paths.get(row).into())),
        Index::Unavailable(_) => None,
    }
}
