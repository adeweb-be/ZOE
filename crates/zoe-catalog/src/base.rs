//! The base of a mirror: the indexes, the metadata table and the paths of
//! one catalog at one tid, built once from the committed trees and never
//! changed. The commits after its tid live in the versions (`version`).

use std::cmp::Ordering;
use std::collections::HashMap;

use roaring::RoaringBitmap;

use crate::value::{BytesArena, StrArena, Val, encode_cell, py_cmp};

/// The marker of a row without a value.
pub const NONE: u32 = u32::MAX;
/// The high bit of a posting that names a bitmap.
const MANY: u32 = 0x8000_0000;

/// The rows of a base: the rids of the catalog in ascending order. A row
/// is the place of its rid, so the order of rows is the order of rids.
#[derive(Default)]
pub struct Rows {
    /// The rid of each row.
    pub rids: Vec<i32>,
}

impl Rows {
    /// The row of `rid`.
    pub fn row(&self, rid: i32) -> Option<u32> {
        self.rids.binary_search(&rid).ok().map(|r| r as u32)
    }

    /// The number of rows.
    pub fn len(&self) -> usize {
        self.rids.len()
    }

    /// Tells if the base has no row.
    pub fn is_empty(&self) -> bool {
        self.rids.is_empty()
    }
}

/// The distinct keys of an index, in ascending order.
pub enum Keys {
    /// Ints and bools.
    Int(Vec<i64>),
    /// Strs and `Message`s, by their text.
    Str(StrArena),
    /// Other values.
    Any(Vec<Val>),
}

impl Keys {
    /// Builds the keys from distinct values in ascending order.
    pub fn from_sorted(values: &[Val]) -> Keys {
        if values.iter().all(|v| v.int().is_some()) {
            Keys::Int(values.iter().map(|v| v.int().unwrap_or(0)).collect())
        } else if values.iter().all(|v| v.text().is_some()) {
            let mut arena = StrArena::default();
            for v in values {
                arena.push(v.text().unwrap_or(""));
            }
            arena.shrink();
            Keys::Str(arena)
        } else {
            Keys::Any(values.to_vec())
        }
    }

    /// The number of keys.
    pub fn len(&self) -> usize {
        match self {
            Keys::Int(v) => v.len(),
            Keys::Str(a) => a.len(),
            Keys::Any(v) => v.len(),
        }
    }

    /// Tells if there is no key.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The key at `slot`, as a value.
    pub fn get(&self, slot: u32) -> Val {
        match self {
            Keys::Int(v) => Val::Int(v[slot as usize]),
            Keys::Str(a) => Val::Str(a.get(slot).into()),
            Keys::Any(v) => v[slot as usize].clone(),
        }
    }

    /// Compares the key at `slot` with `value`. `None` when Python raises.
    pub fn cmp_at(&self, slot: u32, value: &Val) -> Option<Ordering> {
        match self {
            Keys::Int(v) => match value {
                Val::Float(f) => (v[slot as usize] as f64).partial_cmp(f),
                _ => Some(v[slot as usize].cmp(&value.int()?)),
            },
            Keys::Str(a) => Some(a.get(slot).cmp(value.text()?)),
            Keys::Any(v) => py_cmp(&v[slot as usize], value),
        }
    }

    /// Finds `value`: `Ok(slot)` when a key equals it, `Err(place)` where
    /// it would go, `None` when Python would raise on the comparison.
    pub fn search(&self, value: &Val) -> Option<Result<u32, u32>> {
        let (mut low, mut high) = (0u32, self.len() as u32);
        while low < high {
            let mid = low + (high - low) / 2;
            match self.cmp_at(mid, value)? {
                Ordering::Less => low = mid + 1,
                Ordering::Greater => high = mid,
                Ordering::Equal => return Some(Ok(mid)),
            }
        }
        Some(Err(low))
    }

    /// The bytes that the keys take.
    pub fn heap_bytes(&self) -> usize {
        match self {
            Keys::Int(v) => v.capacity() * 8,
            Keys::Str(a) => a.heap_bytes(),
            Keys::Any(v) => {
                v.capacity() * std::mem::size_of::<Val>()
                    + v.iter().map(Val::heap_bytes).sum::<usize>()
            }
        }
    }
}

/// The rows of each key: one row inline, or a bitmap.
#[derive(Default)]
pub struct Postings {
    /// For each key, its one row, or `MANY` and the place of its bitmap.
    pub heads: Vec<u32>,
    /// The bitmaps of the keys of more than one row.
    pub bitmaps: Vec<RoaringBitmap>,
}

/// The rows of one key.
pub enum Posting<'a> {
    /// No row.
    Empty,
    /// One row.
    One(u32),
    /// A bitmap of rows.
    Many(&'a RoaringBitmap),
}

impl Posting<'_> {
    /// The rows as a bitmap.
    pub fn to_bitmap(&self) -> RoaringBitmap {
        match self {
            Posting::Empty => RoaringBitmap::new(),
            Posting::One(row) => {
                let mut b = RoaringBitmap::new();
                b.insert(*row);
                b
            }
            Posting::Many(b) => (*b).clone(),
        }
    }

    /// The number of rows.
    pub fn len(&self) -> u64 {
        match self {
            Posting::Empty => 0,
            Posting::One(_) => 1,
            Posting::Many(b) => b.len(),
        }
    }

    /// Tells if there is no row.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Adds the rows to `out`.
    pub fn add_to(&self, out: &mut RoaringBitmap) {
        match self {
            Posting::Empty => {}
            Posting::One(row) => {
                out.insert(*row);
            }
            Posting::Many(b) => *out |= *b,
        }
    }
}

impl Postings {
    /// Appends the posting of the next key.
    pub fn push(&mut self, rows: &[u32]) {
        if rows.len() == 1 {
            self.heads.push(rows[0]);
        } else {
            self.heads.push(MANY | self.bitmaps.len() as u32);
            self.bitmaps
                .push(RoaringBitmap::from_sorted_iter(rows.iter().copied()).unwrap_or_default());
        }
    }

    /// The rows of the key at `slot`.
    pub fn get(&self, slot: u32) -> Posting<'_> {
        let head = self.heads[slot as usize];
        if head & MANY != 0 {
            Posting::Many(&self.bitmaps[(head & !MANY) as usize])
        } else {
            Posting::One(head)
        }
    }

    /// The bytes that the postings take.
    pub fn heap_bytes(&self) -> usize {
        self.heads.capacity() * 4
            + self
                .bitmaps
                .iter()
                .map(|b| b.serialized_size() + 48)
                .sum::<usize>()
    }

    fn shrink(&mut self) {
        self.heads.shrink_to_fit();
        self.bitmaps.shrink_to_fit();
    }
}

/// A FieldIndex, DateIndex, UUIDIndex or BooleanIndex: one key per row.
pub struct ValueIndex {
    /// The distinct keys.
    pub keys: Keys,
    /// The rows of each key.
    pub postings: Postings,
    /// The slot of the key of each row, or `NONE`.
    pub reverse: Vec<u32>,
    /// The rows with a key.
    pub indexed: RoaringBitmap,
}

/// A KeywordIndex: a list of keys per row.
pub struct KeywordIndex {
    /// The distinct keys.
    pub keys: Keys,
    /// The rows of each key.
    pub postings: Postings,
    /// For each row, the end of its slots in `slots`.
    pub ends: Vec<u32>,
    /// The slots of the keys of each row, row after row.
    pub slots: Vec<u32>,
    /// The rows with a list.
    pub indexed: RoaringBitmap,
}

impl KeywordIndex {
    /// The slots of the keys of `row`.
    pub fn row_slots(&self, row: u32) -> &[u32] {
        let row = row as usize;
        let start = if row == 0 { 0 } else { self.ends[row - 1] } as usize;
        &self.slots[start..self.ends[row] as usize]
    }
}

/// The marker of a bound that is `None` in a DateRangeIndex.
pub const OPEN: i64 = i64::MIN;

/// A DateRangeIndex: a `since` and an `until` per row, in minutes.
pub struct DateRangeIndex {
    /// `since` of each indexed row, or `OPEN`.
    pub since: Vec<i64>,
    /// `until` of each indexed row, or `OPEN`.
    pub until: Vec<i64>,
    /// The rows with an entry.
    pub indexed: RoaringBitmap,
    /// The rows with a `since`, by `since`.
    pub by_since: Vec<(i64, u32)>,
    /// The rows with an `until`, by `until`.
    pub by_until: Vec<(i64, u32)>,
}

/// An ExtendedPathIndex.
pub struct PathIndex {
    /// The distinct components, in ascending order.
    pub comps: StrArena,
    /// For each pair of a component and a level, sorted: the component,
    /// the level.
    pub pairs: Vec<(u32, u16)>,
    /// The rows of each pair.
    pub pair_rows: Postings,
    /// The rows whose last component is at each level.
    pub ends: Vec<RoaringBitmap>,
    /// The path of each row, or the empty str.
    pub paths: StrArena,
    /// The rows with a path, sorted by path.
    pub by_path: Vec<u32>,
    /// The rows of each parent path.
    pub parents: HashMap<Box<str>, RoaringBitmap>,
    /// The rows with a path.
    pub indexed: RoaringBitmap,
    /// `_depth` of the index.
    pub depth: i64,
}

impl PathIndex {
    /// The place of `comp` among the components.
    pub fn comp(&self, comp: &str) -> Option<u32> {
        let (mut low, mut high) = (0u32, self.comps.len() as u32);
        while low < high {
            let mid = low + (high - low) / 2;
            match self.comps.get(mid).cmp(comp) {
                Ordering::Less => low = mid + 1,
                Ordering::Greater => high = mid,
                Ordering::Equal => return Some(mid),
            }
        }
        None
    }

    /// The rows that have `comp` at `level`.
    pub fn at(&self, comp: &str, level: i64) -> Option<Posting<'_>> {
        let comp = self.comp(comp)?;
        let level = u16::try_from(level).ok()?;
        let slot = self.pairs.binary_search(&(comp, level)).ok()?;
        Some(self.pair_rows.get(slot as u32))
    }

    /// The row whose path is `path`.
    pub fn item(&self, path: &str) -> Option<u32> {
        self.by_path
            .binary_search_by(|row| self.paths.get(*row).cmp(path))
            .ok()
            .map(|i| self.by_path[i])
    }
}

/// The kind of a mirrored index, or why the index is not mirrored.
pub enum Index {
    /// FieldIndex, DateIndex, UUIDIndex.
    Value(ValueIndex),
    /// BooleanIndex.
    Boolean(ValueIndex),
    /// KeywordIndex.
    Keyword(KeywordIndex),
    /// DateRangeIndex.
    DateRange(DateRangeIndex),
    /// ExtendedPathIndex.
    Path(PathIndex),
    /// Not mirrored: the reason.
    Unavailable(String),
}

/// One index of a catalog in the base.
pub struct IndexEntry {
    /// The id of the index.
    pub name: String,
    /// The class of the index.
    pub class: String,
    /// The oid of the index object.
    pub oid: u64,
    /// The oid of its `_unindex` tree.
    pub tree: Option<u64>,
    /// The oid of its `_length`.
    pub length_oid: Option<u64>,
    /// The value of its `_length`: the distinct keys for ZCatalog's choice
    /// of a sort.
    pub length: i64,
    /// The structures.
    pub index: Index,
    /// The structure of its tree, for the maintenance.
    pub shape: TreeShape,
}

/// The structure of a tree that the maintenance follows.
#[derive(Default)]
pub struct TreeShape {
    /// The children of each inner node.
    pub inner: HashMap<u64, Vec<u64>>,
    /// The first and last key of each bucket.
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

/// The metadata table: one column per name of the schema.
pub struct Table {
    /// The names of the columns, in the order of the schema.
    pub names: Vec<String>,
    /// The columns.
    pub columns: Vec<Column>,
}

/// One column: the code of the cell of each row and the distinct cells.
pub struct Column {
    /// The code of each row.
    pub codes: Codes,
    /// The encoded distinct cells, by code.
    pub cells: BytesArena,
}

/// The codes of a column, as narrow as its distinct cells allow.
pub enum Codes {
    /// Up to 256 cells.
    U8(Vec<u8>),
    /// Up to 65,536 cells.
    U16(Vec<u16>),
    /// More.
    U32(Vec<u32>),
}

impl Codes {
    /// The code of `row`.
    pub fn get(&self, row: usize) -> u32 {
        match self {
            Codes::U8(v) => u32::from(v[row]),
            Codes::U16(v) => u32::from(v[row]),
            Codes::U32(v) => v[row],
        }
    }

    fn heap_bytes(&self) -> usize {
        match self {
            Codes::U8(v) => v.capacity(),
            Codes::U16(v) => v.capacity() * 2,
            Codes::U32(v) => v.capacity() * 4,
        }
    }
}

impl Table {
    /// The encoded cells of `row`, one per column.
    pub fn row(&self, row: u32) -> Vec<&[u8]> {
        self.columns
            .iter()
            .map(|c| c.cells.get(c.codes.get(row as usize)))
            .collect()
    }

    /// The bytes of each column: its codes and its cells.
    pub fn column_bytes(&self) -> Vec<(String, usize, usize, usize)> {
        self.names
            .iter()
            .zip(&self.columns)
            .map(|(n, c)| {
                (
                    n.clone(),
                    c.codes.heap_bytes(),
                    c.cells.heap_bytes(),
                    c.cells.len(),
                )
            })
            .collect()
    }
}

/// Builds a value index from the items of its `_unindex`: (rid, key).
pub fn build_value(rows: &Rows, items: Vec<(Val, Val)>) -> Result<ValueIndex, String> {
    let mut pairs: Vec<(u32, Val)> = Vec::with_capacity(items.len());
    for (rid, key) in items {
        let rid = rid_of(&rid)?;
        let Some(row) = rows.row(rid) else {
            return Err(format!("rid {rid} is not in the catalog"));
        };
        pairs.push((row, key));
    }
    sort_values(&mut pairs)?;
    let mut distinct: Vec<Val> = Vec::new();
    let mut postings = Postings::default();
    let mut reverse = vec![NONE; rows.len()];
    let mut indexed = Vec::with_capacity(pairs.len());
    let mut group: Vec<u32> = Vec::new();
    for (i, (row, key)) in pairs.iter().enumerate() {
        let new_key = i == 0 || py_cmp(&pairs[i - 1].1, key) != Some(Ordering::Equal);
        if new_key {
            if !group.is_empty() {
                group.sort_unstable();
                postings.push(&group);
                group.clear();
            }
            distinct.push(key.clone());
        }
        group.push(*row);
        reverse[*row as usize] = (distinct.len() - 1) as u32;
        indexed.push(*row);
    }
    if !group.is_empty() {
        group.sort_unstable();
        postings.push(&group);
    }
    indexed.sort_unstable();
    postings.shrink();
    Ok(ValueIndex {
        keys: Keys::from_sorted(&distinct),
        postings,
        reverse,
        indexed: RoaringBitmap::from_sorted_iter(indexed).unwrap_or_default(),
    })
}

/// Sorts (row, key) pairs by key as Python orders the keys.
fn sort_values(pairs: &mut [(u32, Val)]) -> Result<(), String> {
    let mut failed = false;
    pairs.sort_by(|a, b| {
        py_cmp(&a.1, &b.1).unwrap_or_else(|| {
            failed = true;
            Ordering::Equal
        })
    });
    if failed {
        return Err("keys that Python does not order".into());
    }
    Ok(())
}

/// Builds a KeywordIndex from the items of its `_unindex`: (rid, list).
pub fn build_keyword(rows: &Rows, items: Vec<(Val, Val)>) -> Result<KeywordIndex, String> {
    let mut lists: Vec<(u32, Vec<Val>)> = Vec::with_capacity(items.len());
    let mut flat: Vec<(u32, Val)> = Vec::new();
    for (rid, list) in items {
        let rid = rid_of(&rid)?;
        let row = rows
            .row(rid)
            .ok_or_else(|| format!("rid {rid} is not in the catalog"))?;
        let keys: Vec<Val> = match list {
            Val::List(v) | Val::Tuple(v) => v.into_vec(),
            other => return Err(format!("a keyword list {other:?}")),
        };
        for k in &keys {
            flat.push((row, k.clone()));
        }
        lists.push((row, keys));
    }
    sort_values(&mut flat)?;
    let mut distinct: Vec<Val> = Vec::new();
    let mut postings = Postings::default();
    let mut group: Vec<u32> = Vec::new();
    for (i, (row, key)) in flat.iter().enumerate() {
        if i == 0 || py_cmp(&flat[i - 1].1, key) != Some(Ordering::Equal) {
            if !group.is_empty() {
                group.sort_unstable();
                group.dedup();
                postings.push(&group);
                group.clear();
            }
            distinct.push(key.clone());
        }
        group.push(*row);
    }
    if !group.is_empty() {
        group.sort_unstable();
        group.dedup();
        postings.push(&group);
    }
    drop(flat);
    let keys = Keys::from_sorted(&distinct);
    drop(distinct);
    lists.sort_unstable_by_key(|(row, _)| *row);
    let mut ends = Vec::with_capacity(rows.len());
    let mut slots = Vec::new();
    let mut indexed = Vec::with_capacity(lists.len());
    let mut next = lists.into_iter().peekable();
    for row in 0..rows.len() as u32 {
        if let Some((r, keys_of_row)) = next.peek()
            && *r == row
        {
            for k in keys_of_row {
                if let Some(Ok(slot)) = keys.search(k) {
                    slots.push(slot);
                }
            }
            indexed.push(row);
            next.next();
        }
        ends.push(slots.len() as u32);
    }
    slots.shrink_to_fit();
    postings.shrink();
    Ok(KeywordIndex {
        keys,
        postings,
        ends,
        slots,
        indexed: RoaringBitmap::from_sorted_iter(indexed).unwrap_or_default(),
    })
}

/// Builds a DateRangeIndex from the items of its `_unindex`:
/// (rid, (since, until)).
pub fn build_date_range(rows: &Rows, items: Vec<(Val, Val)>) -> Result<DateRangeIndex, String> {
    let mut since = vec![OPEN; rows.len()];
    let mut until = vec![OPEN; rows.len()];
    let mut indexed = Vec::with_capacity(items.len());
    let mut by_since = Vec::new();
    let mut by_until = Vec::new();
    let bound = |v: &Val| -> Result<i64, String> {
        match v {
            Val::None => Ok(OPEN),
            other => other
                .int()
                .ok_or_else(|| format!("a date range bound {other:?}")),
        }
    };
    for (rid, pair) in items {
        let rid = rid_of(&rid)?;
        let row = rows
            .row(rid)
            .ok_or_else(|| format!("rid {rid} is not in the catalog"))?;
        let (s, u) = match &pair {
            Val::Tuple(v) if v.len() == 2 => (bound(&v[0])?, bound(&v[1])?),
            other => return Err(format!("a date range {other:?}")),
        };
        since[row as usize] = s;
        until[row as usize] = u;
        if s != OPEN {
            by_since.push((s, row));
        }
        if u != OPEN {
            by_until.push((u, row));
        }
        indexed.push(row);
    }
    indexed.sort_unstable();
    by_since.sort_unstable();
    by_until.sort_unstable();
    Ok(DateRangeIndex {
        since,
        until,
        indexed: RoaringBitmap::from_sorted_iter(indexed).unwrap_or_default(),
        by_since,
        by_until,
    })
}

/// The components of a path, as ExtendedPathIndex splits it.
pub fn components(path: &str) -> Vec<&str> {
    path.split('/').filter(|p| !p.is_empty()).collect()
}

/// Builds an ExtendedPathIndex from the items of its `_unindex`:
/// (rid, path).
pub fn build_path(rows: &Rows, items: Vec<(Val, Val)>, depth: i64) -> Result<PathIndex, String> {
    let mut by_row: Vec<Option<Box<str>>> = vec![None; rows.len()];
    for (rid, path) in items {
        let rid = rid_of(&rid)?;
        let row = rows
            .row(rid)
            .ok_or_else(|| format!("rid {rid} is not in the catalog"))?;
        let path = path
            .text()
            .ok_or_else(|| format!("a path {path:?}"))?
            .into();
        by_row[row as usize] = Some(path);
    }
    let mut distinct: Vec<&str> = by_row
        .iter()
        .flatten()
        .flat_map(|p| components(p))
        .collect();
    distinct.sort_unstable();
    distinct.dedup();
    let mut comps = StrArena::default();
    for c in &distinct {
        comps.push(c);
    }
    comps.shrink();
    let comp_id = |c: &str| distinct.binary_search(&c).map(|i| i as u32).unwrap_or(0);
    let mut pair_list: Vec<(u32, u16, u32)> = Vec::new();
    let mut ends: Vec<Vec<u32>> = Vec::new();
    let mut parents: HashMap<Box<str>, Vec<u32>> = HashMap::new();
    let mut paths = StrArena::default();
    let mut indexed = Vec::new();
    for (row, path) in by_row.iter().enumerate() {
        let row = row as u32;
        let Some(path) = path else {
            paths.push("");
            continue;
        };
        paths.push(path);
        indexed.push(row);
        let cs = components(path);
        for (level, c) in cs.iter().enumerate() {
            pair_list.push((comp_id(c), level as u16, row));
        }
        // A path of no component ends at level -1: its terminator is at
        // `len(comps) - 1`, which the index keeps under the key -1.
        let end = cs.len() as i64 - 1;
        if end >= 0 {
            let end = end as usize;
            if ends.len() <= end {
                ends.resize_with(end + 1, Vec::new);
            }
            ends[end].push(row);
        }
        let parent = format!("/{}", cs[..cs.len().saturating_sub(1)].join("/"));
        parents.entry(parent.into()).or_default().push(row);
    }
    pair_list.sort_unstable();
    let mut pairs = Vec::new();
    let mut pair_rows = Postings::default();
    let mut group = Vec::new();
    for (i, (c, l, row)) in pair_list.iter().enumerate() {
        if i > 0 && (pair_list[i - 1].0, pair_list[i - 1].1) != (*c, *l) {
            pair_rows.push(&group);
            group.clear();
        }
        if group.is_empty() {
            pairs.push((*c, *l));
        }
        group.push(*row);
    }
    if !group.is_empty() {
        pair_rows.push(&group);
    }
    drop(pair_list);
    pair_rows.shrink();
    pairs.shrink_to_fit();
    paths.shrink();
    let mut by_path: Vec<u32> = indexed.clone();
    by_path.sort_unstable_by(|a, b| paths.get(*a).cmp(paths.get(*b)));
    Ok(PathIndex {
        comps,
        pairs,
        pair_rows,
        ends: ends
            .into_iter()
            .map(|v| RoaringBitmap::from_sorted_iter(v).unwrap_or_default())
            .collect(),
        paths,
        by_path,
        parents: parents
            .into_iter()
            .map(|(k, v)| (k, RoaringBitmap::from_sorted_iter(v).unwrap_or_default()))
            .collect(),
        indexed: RoaringBitmap::from_sorted_iter(indexed).unwrap_or_default(),
        depth,
    })
}

/// Builds the metadata table from the items of `data`: (rid, tuple).
pub fn build_table(rows: &Rows, names: Vec<String>, items: Vec<(Val, Val)>) -> Result<Table, String> {
    let width = names.len();
    let mut by_row: Vec<Option<Box<[Val]>>> = vec![None; rows.len()];
    for (rid, record) in items {
        let rid = rid_of(&rid)?;
        let row = rows
            .row(rid)
            .ok_or_else(|| format!("rid {rid} of data is not in the catalog"))?;
        match record {
            Val::Tuple(cells) if cells.len() == width => by_row[row as usize] = Some(cells),
            other => {
                return Err(format!(
                    "a metadata record of {} cells for {width} columns",
                    match other {
                        Val::Tuple(c) => c.len(),
                        _ => 0,
                    }
                ));
            }
        }
    }
    let columns = build_columns(&by_row, width);
    Ok(Table { names, columns })
}

fn build_columns(by_row: &[Option<Box<[Val]>>], width: usize) -> Vec<Column> {
    // One thread per group of columns.
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).min(width.max(1));
    let mut columns: Vec<Option<Column>> = (0..width).map(|_| None).collect();
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|t| {
                scope.spawn(move || {
                    (t..width)
                        .step_by(threads)
                        .map(|c| (c, build_column(by_row, c)))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        for h in handles {
            for (c, column) in h.join().unwrap_or_default() {
                columns[c] = Some(column);
            }
        }
    });
    columns.into_iter().map(|c| c.expect("a column")).collect()
}

fn build_column(by_row: &[Option<Box<[Val]>>], c: usize) -> Column {
    let mut buffer = Vec::new();
    let mut cells = BytesArena::default();
    let mut seen: HashMap<Vec<u8>, u32> = HashMap::new();
    let mut codes = Vec::with_capacity(by_row.len());
    for row in by_row {
        buffer.clear();
        match row {
            Some(cells_of_row) => encode_cell(&cells_of_row[c], &mut buffer),
            None => encode_cell(&Val::Missing, &mut buffer),
        }
        let code = match seen.get(&buffer) {
            Some(code) => *code,
            None => {
                let code = cells.push(&buffer);
                seen.insert(buffer.clone(), code);
                code
            }
        };
        codes.push(code);
    }
    cells.shrink();
    let codes = if cells.len() <= 256 {
        Codes::U8(codes.iter().map(|c| *c as u8).collect())
    } else if cells.len() <= 65_536 {
        Codes::U16(codes.iter().map(|c| *c as u16).collect())
    } else {
        Codes::U32(codes)
    };
    Column { codes, cells }
}

/// The rid of a key of an IO tree.
pub fn rid_of(value: &Val) -> Result<i32, String> {
    value
        .int()
        .and_then(|i| i32::try_from(i).ok())
        .ok_or_else(|| format!("a rid {value:?}"))
}

impl ValueIndex {
    /// The bytes that the index takes.
    pub fn heap_bytes(&self) -> usize {
        self.keys.heap_bytes()
            + self.postings.heap_bytes()
            + self.reverse.capacity() * 4
            + self.indexed.serialized_size()
    }
}

impl KeywordIndex {
    /// The bytes that the index takes.
    pub fn heap_bytes(&self) -> usize {
        self.keys.heap_bytes()
            + self.postings.heap_bytes()
            + self.ends.capacity() * 4
            + self.slots.capacity() * 4
            + self.indexed.serialized_size()
    }
}

impl DateRangeIndex {
    /// The bytes that the index takes.
    pub fn heap_bytes(&self) -> usize {
        self.since.capacity() * 8
            + self.until.capacity() * 8
            + self.by_since.capacity() * 16
            + self.by_until.capacity() * 16
            + self.indexed.serialized_size()
    }
}

impl PathIndex {
    /// The bytes that the index takes.
    pub fn heap_bytes(&self) -> usize {
        self.comps.heap_bytes()
            + self.pairs.capacity() * 8
            + self.pair_rows.heap_bytes()
            + self.ends.iter().map(|b| b.serialized_size()).sum::<usize>()
            + self.paths.heap_bytes()
            + self.by_path.capacity() * 4
            + self
                .parents
                .iter()
                .map(|(k, v)| k.len() + 48 + v.serialized_size())
                .sum::<usize>()
            + self.indexed.serialized_size()
    }
}

impl Index {
    /// The bytes that the index takes.
    pub fn heap_bytes(&self) -> usize {
        match self {
            Index::Value(v) | Index::Boolean(v) => v.heap_bytes(),
            Index::Keyword(k) => k.heap_bytes(),
            Index::DateRange(d) => d.heap_bytes(),
            Index::Path(p) => p.heap_bytes(),
            Index::Unavailable(_) => 0,
        }
    }
}
