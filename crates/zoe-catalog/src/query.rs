//! The query of a typed plan over a version: the clauses as ZCatalog's
//! indexes apply them, and the sort as `Catalog.sortResults` does it.

use std::cmp::Ordering;

use roaring::RoaringBitmap;

use crate::base::{self, Index, Keys, Posting, Postings, ValueIndex, components};
use crate::catalog::Base;
use crate::version::{Patch, Slot};
use crate::value::{Val, py_cmp};

/// A clause of a plan, typed by the client with ZCatalog's `IndexQuery` and
/// the conversion of the index.
#[derive(Clone, Debug)]
pub enum Clause {
    /// FieldIndex, DateIndex, UUIDIndex, KeywordIndex: the keys in the form
    /// of the index, the operator, the range and `not`.
    Value {
        /// The index.
        index: String,
        /// The keys, converted.
        keys: Vec<Val>,
        /// True for the operator `and`.
        and: bool,
        /// `min` and `max` of a range.
        range: Option<(bool, bool)>,
        /// The keys of `not`, converted.
        not: Vec<Val>,
    },
    /// BooleanIndex: the truth of the first key, `None` for no key.
    Boolean {
        /// The index.
        index: String,
        /// The truth of the first key.
        key: Option<bool>,
    },
    /// DateRangeIndex: the term, in minutes.
    DateRange {
        /// The index.
        index: String,
        /// The converted term.
        term: Option<i64>,
    },
    /// ExtendedPathIndex.
    Path {
        /// The index.
        index: String,
        /// Each path, with its level when the query gives one.
        paths: Vec<(String, Option<i64>)>,
        /// `level`.
        level: i64,
        /// True for the operator `and`.
        and: bool,
        /// `depth`.
        depth: i64,
        /// `navtree`.
        navtree: bool,
        /// `navtree_start`.
        navtree_start: i64,
        /// The components of the physical path of the index.
        index_path: Vec<String>,
    },
}

/// A typed plan.
#[derive(Clone, Debug, Default)]
pub struct Plan {
    /// The clauses.
    pub clauses: Vec<Clause>,
    /// The sort: each index and its order, true for reverse. An index past
    /// the end of a list of orders has none.
    pub sort: Vec<(String, Option<bool>)>,
    /// `sort_limit`.
    pub limit: Option<i64>,
    /// `b_start`.
    pub b_start: i64,
    /// `b_size`.
    pub b_size: Option<i64>,
    /// Give the metadata rows and the paths of the result.
    pub rows: bool,
}

/// The answer of a plan.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Reply {
    /// The rids of the result, in order.
    pub rids: Vec<i32>,
    /// The length of the lazy result of ZCatalog.
    pub len: i64,
    /// `actual_result_count`.
    pub actual: i64,
    /// The path and the encoded metadata row of each rid, when asked.
    pub rows: Vec<(String, Vec<u8>)>,
}

/// Why a plan takes the Python path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fallback(pub String);

fn fallback<T>(reason: impl Into<String>) -> Result<T, Fallback> {
    Err(Fallback(reason.into()))
}

/// A version of a mirror seen by one query: the base and a patch.
pub struct View<'a> {
    base: &'a Base,
    patch: &'a Patch,
    /// The rids of the patch that the base has no row for, in order. Their
    /// rows follow the rows of the base.
    extra: Vec<i32>,
}

impl<'a> View<'a> {
    /// The view of `base` with `patch`.
    pub fn new(base: &'a Base, patch: &'a Patch) -> View<'a> {
        let mut extra: Vec<i32> = patch
            .trees
            .values()
            .flat_map(|rows| rows.keys().copied())
            .filter(|rid| base.rows.row(*rid).is_none())
            .collect();
        extra.sort_unstable();
        extra.dedup();
        View { base, patch, extra }
    }

    fn row_of(&self, rid: i32) -> u32 {
        match self.base.rows.row(rid) {
            Some(row) => row,
            None => {
                let i = self.extra.binary_search(&rid).unwrap_or(0);
                (self.base.rows.len() + i) as u32
            }
        }
    }

    fn rid_of(&self, row: u32) -> i32 {
        let n = self.base.rows.len();
        if (row as usize) < n {
            self.base.rows.rids[row as usize]
        } else {
            self.extra[row as usize - n]
        }
    }

    /// The changed rows of the tree at `slot`, with their values.
    fn changed(&self, slot: Slot) -> Vec<(u32, Option<&'a Val>)> {
        match self.patch.trees.get(&slot) {
            Some(rows) => rows
                .iter()
                .map(|(rid, v)| (self.row_of(*rid), v.as_ref()))
                .collect(),
            None => Vec::new(),
        }
    }

    fn touched(&self, slot: Slot) -> RoaringBitmap {
        self.changed(slot).into_iter().map(|(row, _)| row).collect()
    }

    /// The rows of the catalog.
    fn all_rows(&self) -> RoaringBitmap {
        let mut rows = RoaringBitmap::new();
        rows.insert_range(0..self.base.rows.len() as u32);
        for (row, value) in self.changed(Slot::Paths) {
            if value.is_some() {
                rows.insert(row);
            } else {
                rows.remove(row);
            }
        }
        rows
    }

    /// Answers `plan`.
    pub fn query(&self, plan: &Plan) -> Result<Reply, Fallback> {
        let mut rs: Option<RoaringBitmap> = None;
        let mut used = false;
        let mut ranges = Vec::new();
        // ZCatalog applies the indexes in an order of its own. The sets
        // intersect in any order, but for a DateRangeIndex, which keeps a
        // row of the result set that it does not index.
        let mut order: Vec<&Clause> = plan.clauses.iter().collect();
        order.sort_by_key(|c| match c {
            Clause::Value { range: None, .. } | Clause::Boolean { .. } => 0,
            Clause::Path { .. } => 1,
            Clause::Value { .. } => 2,
            Clause::DateRange { .. } => 3,
        });
        for clause in order {
            if let Clause::DateRange { .. } = clause {
                ranges.push(clause);
                continue;
            }
            let Some(set) = self.clause(clause, rs.as_ref())? else {
                continue;
            };
            used = true;
            rs = Some(match rs {
                None => set,
                Some(mut r) => {
                    r &= &set;
                    r
                }
            });
            if rs.as_ref().is_some_and(RoaringBitmap::is_empty) {
                break;
            }
        }
        for clause in ranges {
            if rs.as_ref().is_some_and(RoaringBitmap::is_empty) {
                break;
            }
            let Clause::DateRange { index, term } = clause else {
                continue;
            };
            let (matching, missing) = self.date_range(index, *term)?;
            used = true;
            rs = Some(match rs {
                None => matching,
                Some(r) => {
                    // Before or after the other indexes: the same rows,
                    // unless a row of the set has no entry in this index.
                    if !(&r & &missing).is_empty() {
                        return fallback("a row without a date range: the order of the indexes decides");
                    }
                    r & matching
                }
            });
        }
        let rs = match rs {
            Some(rs) if used && !rs.is_empty() => rs,
            _ => {
                return Ok(Reply::default());
            }
        };
        self.sort(plan, rs)
    }

    fn entry(&self, name: &str) -> Result<(usize, &'a base::IndexEntry), Fallback> {
        let i = *self
            .base
            .by_name
            .get(name)
            .ok_or_else(|| Fallback(format!("no index {name}")))?;
        Ok((i, &self.base.indexes[i]))
    }

    fn clause(&self, clause: &Clause, rs: Option<&RoaringBitmap>) -> Result<Option<RoaringBitmap>, Fallback> {
        match clause {
            Clause::Value {
                index,
                keys,
                and,
                range,
                not,
            } => {
                let (i, entry) = self.entry(index)?;
                let lists = match &entry.index {
                    Index::Value(v) => Lists::Value(v),
                    Index::Keyword(k) => Lists::Keyword(&k.keys, &k.postings),
                    Index::Unavailable(reason) => return fallback(reason.clone()),
                    _ => return fallback(format!("{index}: a value clause on another kind")),
                };
                self.value_clause(i, lists, keys, *and, *range, not, rs)
                    .map(Some)
            }
            Clause::Boolean { index, key } => {
                let (i, entry) = self.entry(index)?;
                let Index::Boolean(v) = &entry.index else {
                    return fallback(format!("{index}: not a BooleanIndex"));
                };
                let Some(key) = key else {
                    return Ok(Some(RoaringBitmap::new()));
                };
                Ok(Some(
                    self.posting(i, Lists::Value(v), &Val::Int(i64::from(*key)))
                        .unwrap_or_default(),
                ))
            }
            Clause::Path {
                index,
                paths,
                level,
                and,
                depth,
                navtree,
                navtree_start,
                index_path,
            } => {
                let (i, entry) = self.entry(index)?;
                let Index::Path(p) = &entry.index else {
                    return fallback(format!("{index}: not a mirrored path index"));
                };
                let paths_view = PathView::new(self, i, p);
                let mut result: Option<RoaringBitmap> = None;
                for (path, path_level) in paths {
                    let rows = paths_view.search(
                        path,
                        path_level.unwrap_or(*level),
                        *depth,
                        *navtree,
                        *navtree_start,
                        index_path,
                    )?;
                    result = Some(match result {
                        None => rows,
                        Some(r) if *and => r & rows,
                        Some(r) => r | rows,
                    });
                }
                Ok(Some(result.unwrap_or_default()))
            }
            Clause::DateRange { .. } => Ok(None),
        }
    }

    /// The rows of `key` in the index at `i`, in this version. `None` when
    /// no row has it, as the BTree of ZCatalog holds no empty set.
    fn posting(&self, i: usize, lists: Lists<'_>, key: &Val) -> Option<RoaringBitmap> {
        let slot = Slot::Index(i);
        let mut rows = match lists.keys().search(key) {
            Some(Ok(s)) => lists.postings().get(s).to_bitmap(),
            _ => RoaringBitmap::new(),
        };
        for (row, value) in self.changed(slot) {
            rows.remove(row);
            if let Some(value) = value
                && lists.has(value, key)
            {
                rows.insert(row);
            }
        }
        (!rows.is_empty()).then_some(rows)
    }

    #[allow(clippy::too_many_arguments)]
    fn value_clause(
        &self,
        i: usize,
        lists: Lists<'_>,
        keys: &[Val],
        and: bool,
        range: Option<(bool, bool)>,
        not: &[Val],
        rs: Option<&RoaringBitmap>,
    ) -> Result<RoaringBitmap, Fallback> {
        let exclude = |this: &Self| -> RoaringBitmap {
            let mut out = RoaringBitmap::new();
            for k in not.iter().filter(|k| **k != Val::None) {
                if let Some(rows) = this.posting(i, lists, k) {
                    out |= rows;
                }
            }
            out
        };
        if keys.is_empty() {
            if not.is_empty() {
                return Ok(RoaringBitmap::new());
            }
            if and || range.is_some() {
                return fallback("a not query with and or a range");
            }
            // ZCatalog removes the rows of `not` from the result set, or
            // from every row of the catalog; when no row has a key of `not`,
            // it gives every row of the index.
            let excluded = exclude(self);
            if !excluded.is_empty() {
                let mut all = match rs {
                    Some(r) => r.clone(),
                    None => self.all_rows(),
                };
                all -= excluded;
                return Ok(all);
            }
            return Ok(self.indexed(i, lists));
        }
        let mut r = if let Some((min, max)) = range {
            if and {
                return fallback("a range with the operator and");
            }
            let lo = if min {
                Some(min_of(keys).ok_or_else(|| Fallback("keys without an order".into()))?)
            } else {
                None
            };
            let hi = if max {
                Some(max_of(keys).ok_or_else(|| Fallback("keys without an order".into()))?)
            } else {
                None
            };
            // `if hi:` of ZCatalog: a false bound is no bound.
            let hi = hi.filter(|h| h.truthy());
            self.range(i, lists, lo.as_ref(), hi.as_ref(), rs)?
        } else {
            let mut sets = Vec::new();
            for k in keys {
                if *k == Val::None {
                    continue;
                }
                match self.posting(i, lists, k) {
                    Some(rows) => sets.push(rows),
                    None if and => return Ok(RoaringBitmap::new()),
                    None => {}
                }
            }
            if and {
                let mut it = sets.into_iter();
                let mut r = it.next().unwrap_or_default();
                for s in it {
                    r &= s;
                }
                r
            } else {
                let mut r = RoaringBitmap::new();
                for s in sets {
                    r |= s;
                }
                r
            }
        };
        if !not.is_empty() {
            r -= exclude(self);
        }
        Ok(r)
    }

    fn indexed(&self, i: usize, lists: Lists<'_>) -> RoaringBitmap {
        let mut rows = match lists {
            Lists::Value(v) => v.indexed.clone(),
            Lists::Keyword(..) => match &self.base.indexes[i].index {
                Index::Keyword(k) => k.indexed.clone(),
                _ => RoaringBitmap::new(),
            },
        };
        for (row, value) in self.changed(Slot::Index(i)) {
            if value.is_some() {
                rows.insert(row);
            } else {
                rows.remove(row);
            }
        }
        rows
    }

    /// The rows with a key from `lo` to `hi`, both included, as
    /// `index.values(lo, hi)` gives them.
    fn range(
        &self,
        i: usize,
        lists: Lists<'_>,
        lo: Option<&Val>,
        hi: Option<&Val>,
        rs: Option<&RoaringBitmap>,
    ) -> Result<RoaringBitmap, Fallback> {
        let keys = lists.keys();
        let first = match lo {
            None => 0,
            Some(lo) => match keys.search(lo) {
                Some(Ok(s)) | Some(Err(s)) => s,
                None => return fallback("a range bound of another type"),
            },
        };
        let end = match hi {
            None => keys.len() as u32,
            Some(hi) => match keys.search(hi) {
                Some(Ok(s)) => s + 1,
                Some(Err(s)) => s,
                None => return fallback("a range bound of another type"),
            },
        };
        let in_range = |v: &Val| -> bool {
            let above = lo.is_none_or(|lo| matches!(py_cmp(v, lo), Some(Ordering::Greater | Ordering::Equal)));
            let below = hi.is_none_or(|hi| matches!(py_cmp(v, hi), Some(Ordering::Less | Ordering::Equal)));
            above && below
        };
        let mut rows = RoaringBitmap::new();
        if end > first {
            match (lists, rs) {
                // A small result set: test the key of each of its rows.
                (Lists::Value(v), Some(rs)) if rs.len() < u64::from(end - first) => {
                    let n = v.reverse.len() as u32;
                    for row in rs.iter().filter(|r| *r < n) {
                        let s = v.reverse[row as usize];
                        if s != base::NONE && s >= first && s < end {
                            rows.insert(row);
                        }
                    }
                }
                // Many keys: test the key of each row.
                (Lists::Value(v), _) if end - first > 4096 => {
                    for (row, s) in v.reverse.iter().enumerate() {
                        if *s != base::NONE && *s >= first && *s < end {
                            let _ = rows.try_push(row as u32);
                        }
                    }
                }
                _ => {
                    for s in first..end {
                        lists.postings().get(s).add_to(&mut rows);
                    }
                }
            }
        }
        for (row, value) in self.changed(Slot::Index(i)) {
            rows.remove(row);
            if let Some(value) = value {
                let hit = match value {
                    Val::List(items) | Val::Tuple(items) if matches!(lists, Lists::Keyword(..)) => {
                        items.iter().any(in_range)
                    }
                    v => in_range(v),
                };
                if hit {
                    rows.insert(row);
                }
            }
        }
        Ok(rows)
    }

    /// The rows that match the term of a DateRangeIndex, and the rows that
    /// the index has no entry for.
    fn date_range(&self, index: &str, term: Option<i64>) -> Result<(RoaringBitmap, RoaringBitmap), Fallback> {
        let (i, entry) = self.entry(index)?;
        let Index::DateRange(d) = &entry.index else {
            return fallback(format!("{index}: not a DateRangeIndex"));
        };
        let Some(term) = term else {
            return fallback("a date range term out of range");
        };
        // The rows that do not match: a since after the term, an until
        // before it.
        let mut not = Vec::new();
        let start = d.by_since.partition_point(|(s, _)| *s <= term);
        not.extend(d.by_since[start..].iter().map(|(_, r)| *r));
        let end = d.by_until.partition_point(|(u, _)| *u < term);
        not.extend(d.by_until[..end].iter().map(|(_, r)| *r));
        not.sort_unstable();
        not.dedup();
        let not = RoaringBitmap::from_sorted_iter(not).unwrap_or_default();
        let mut matching = &d.indexed - &not;
        let mut indexed = d.indexed.clone();
        for (row, value) in self.changed(Slot::Index(i)) {
            matching.remove(row);
            indexed.remove(row);
            let Some(value) = value else { continue };
            indexed.insert(row);
            let Val::Tuple(pair) = value else {
                return fallback("a date range value");
            };
            let bound = |v: &Val| v.int();
            let since_ok = bound(&pair[0]).is_none_or(|s| s <= term);
            let until_ok = bound(&pair[1]).is_none_or(|u| u >= term);
            if since_ok && until_ok {
                matching.insert(row);
            }
        }
        let missing = self.all_rows() - indexed;
        Ok((matching, missing))
    }

    /// The ranks of the values of the patch that the keys of the base do
    /// not hold, so that every value of the version has a rank in the order
    /// of the keys.
    fn rank_table(&self, i: usize, v: &ValueIndex) -> RankTable {
        let mut extra: Vec<(Val, u32)> = Vec::new();
        for (_, value) in self.changed(Slot::Index(i)) {
            if let Some(value) = value
                && let Some(Err(place)) = v.keys.search(value)
            {
                extra.push((value.clone(), place));
            }
        }
        extra.sort_by(|a, b| py_cmp(&a.0, &b.0).unwrap_or(Ordering::Equal));
        extra.dedup_by(|a, b| py_cmp(&a.0, &b.0) == Some(Ordering::Equal));
        let mut ranked = Vec::with_capacity(extra.len());
        let mut sub = 0u64;
        let mut last = None;
        for (value, place) in extra {
            sub = if last == Some(place) { sub + 1 } else { 1 };
            last = Some(place);
            ranked.push((value, (2 * u64::from(place)) << 20 | sub));
        }
        RankTable { extra: ranked }
    }

    /// The rank of a value among the keys of `v` and the values of the
    /// patch.
    fn rank_of(v: &ValueIndex, table: &RankTable, value: &Val) -> Option<u64> {
        match v.keys.search(value)? {
            Ok(s) => Some(slot_rank(s)),
            Err(_) => table
                .extra
                .binary_search_by(|(x, _)| py_cmp(x, value).unwrap_or(Ordering::Equal))
                .ok()
                .map(|k| table.extra[k].1),
        }
    }

    /// The rows of `rs` that the patch changed in the index at `i`, with
    /// the rank of their new value, and the changed rows.
    fn patched_ranks(&self, i: usize, v: &ValueIndex, rs: &RoaringBitmap) -> (Vec<(u64, i32)>, RoaringBitmap) {
        let mut items = Vec::new();
        let mut touched = RoaringBitmap::new();
        let table = self.rank_table(i, v);
        for (row, value) in self.changed(Slot::Index(i)) {
            touched.insert(row);
            if rs.contains(row)
                && let Some(rank) = value.and_then(|value| Self::rank_of(v, &table, value))
            {
                items.push((rank, self.rid_of(row)));
            }
        }
        (items, touched)
    }

    /// The rows of `rs` with a key in the index at `i`.
    fn keyed_count(&self, i: usize, v: &ValueIndex, rs: &RoaringBitmap) -> u64 {
        let (patched, touched) = self.patched_ranks(i, v, rs);
        let base = rs & &v.indexed;
        base.len() - (&base & &touched).len() + patched.len() as u64
    }

    /// The rows of `rs` with a key in the index at `i`, by key, with their
    /// ranks: by descending key when `desc`, and the rows of one key by
    /// descending rid when `tie_desc`. With `stop`, the walk ends with the
    /// key at which it holds `stop` rows.
    fn walk(
        &self,
        i: usize,
        v: &ValueIndex,
        rs: &RoaringBitmap,
        desc: bool,
        tie_desc: bool,
        stop: Option<usize>,
    ) -> Vec<(u64, i32)> {
        let (mut patched, touched) = self.patched_ranks(i, v, rs);
        let keys = v.keys.len() as u64;
        let order = |a: &(u64, i32), b: &(u64, i32)| {
            let by_rank = if desc { b.0.cmp(&a.0) } else { a.0.cmp(&b.0) };
            by_rank.then(if tie_desc { b.1.cmp(&a.1) } else { a.1.cmp(&b.1) })
        };
        // Few rows against many keys: the rank of each row, then a sort.
        if stop.is_none() && keys > 4 * rs.len() {
            let n = v.reverse.len() as u32;
            let mut items = patched;
            for row in rs.iter() {
                if row < n && !touched.contains(row) {
                    let s = v.reverse[row as usize];
                    if s != base::NONE {
                        items.push((slot_rank(s), self.rid_of(row)));
                    }
                }
            }
            items.sort_by(order);
            return items;
        }
        patched.sort_by(order);
        let mut out: Vec<(u64, i32)> = Vec::new();
        let mut p = 0;
        let before = |a: u64, b: u64| if desc { a > b } else { a < b };
        let mut group: Vec<(u64, i32)> = Vec::new();
        let slots: Box<dyn Iterator<Item = u32>> = if desc {
            Box::new((0..keys as u32).rev())
        } else {
            Box::new(0..keys as u32)
        };
        for s in slots {
            let rank = slot_rank(s);
            while p < patched.len() && before(patched[p].0, rank) {
                let r = patched[p].0;
                group.clear();
                while p < patched.len() && patched[p].0 == r {
                    group.push(patched[p]);
                    p += 1;
                }
                out.extend(group.iter().copied());
                if stop.is_some_and(|n| out.len() >= n) {
                    return out;
                }
            }
            group.clear();
            match v.postings.get(s) {
                Posting::One(row) => {
                    if rs.contains(row) && !touched.contains(row) {
                        group.push((rank, self.rid_of(row)));
                    }
                }
                Posting::Many(b) => {
                    let mut rows = b & rs;
                    rows -= &touched;
                    group.extend(rows.iter().map(|row| (rank, self.rid_of(row))));
                }
                Posting::Empty => {}
            }
            while p < patched.len() && patched[p].0 == rank {
                group.push(patched[p]);
                p += 1;
            }
            if group.is_empty() {
                continue;
            }
            if tie_desc {
                group.sort_by(|a, b| b.1.cmp(&a.1));
            } else {
                group.sort_by(|a, b| a.1.cmp(&b.1));
            }
            out.extend(group.iter().copied());
            if stop.is_some_and(|n| out.len() >= n) {
                return out;
            }
        }
        out.extend(patched[p..].iter().copied());
        out
    }

    /// The rank of the key of each row, for the rows in `rids` order.
    fn ranks(&self, i: usize, v: &ValueIndex, rows: &[u32]) -> Vec<Option<u64>> {
        let changed: std::collections::HashMap<u32, Option<&Val>> =
            self.changed(Slot::Index(i)).into_iter().collect();
        let table = self.rank_table(i, v);
        let n = v.reverse.len() as u32;
        rows.iter()
            .map(|row| {
                if let Some(value) = changed.get(row) {
                    value.and_then(|value| Self::rank_of(v, &table, value))
                } else if *row < n && v.reverse[*row as usize] != base::NONE {
                    Some(slot_rank(v.reverse[*row as usize]))
                } else {
                    None
                }
            })
            .collect()
    }

    fn sort(&self, plan: &Plan, rs: RoaringBitmap) -> Result<Reply, Fallback> {
        let rlen = rs.len() as i64;
        let mut b_start = plan.b_start;
        let mut b_size = plan.b_size;
        let mut limit = plan.limit;
        if let Some(size) = b_size {
            limit = Some(b_start + size);
        } else if limit.is_some_and(|l| l != 0) {
            b_size = limit;
        }
        let base_rows = self.base.rows.len() as u32;
        // The rows of the result in the order of their rids.
        let in_order = |rows: Vec<u32>| -> Vec<u32> {
            if rows.iter().all(|r| *r < base_rows) {
                return rows;
            }
            let mut rows = rows;
            rows.sort_by_key(|r| self.rid_of(*r));
            rows
        };
        if plan.sort.is_empty() {
            let (page, len) = match b_size {
                Some(size) => {
                    let skip = b_start.max(0) as usize;
                    let take = size.max(0) as usize;
                    let page: Vec<u32> = if rs.max().is_some_and(|m| m < base_rows) {
                        rs.iter().skip(skip).take(take).collect()
                    } else {
                        in_order(rs.iter().collect()).into_iter().skip(skip).take(take).collect()
                    };
                    let len = if rlen != 0 { page.len() as i64 } else { 0 };
                    (page, len)
                }
                None => (in_order(rs.iter().collect()), rlen),
            };
            return Ok(self.reply(plan, page, len, rlen));
        }
        let mut sorts = Vec::new();
        for (name, reverse) in &plan.sort {
            let (i, entry) = self.entry(name)?;
            let Index::Value(v) = &entry.index else {
                return fallback(format!("a sort on {name}, which is not a value index"));
            };
            let length = self.patch.lengths.get(&i).copied().unwrap_or(entry.length);
            sorts.push((i, v, *reverse, length));
        }
        let mut reverse: Vec<Option<bool>> = sorts.iter().map(|s| s.2).collect();
        if reverse[0].is_none() {
            return fallback("a sort without an order for its first index");
        }
        if let Some(l) = limit
            && l >= rlen
        {
            limit = Some(rlen);
        }
        if limit == Some(0) {
            return fallback("a sort limit of 0");
        }
        let mut switched = false;
        if let Some(size) = b_size
            && size != 0
            && b_start != 0
            && (b_start as f64) > rlen as f64 / 2.0
        {
            for r in reverse.iter_mut().flatten() {
                *r = !*r;
            }
            switched = true;
            let b_end = b_start + size;
            let mut size = size;
            if b_end >= rlen {
                if b_start >= rlen {
                    return Ok(Reply {
                        rids: Vec::new(),
                        len: 0,
                        actual: rlen,
                        rows: Vec::new(),
                    });
                }
                size += rlen - b_end;
                b_start = 0;
            } else {
                b_start = rlen - b_end;
            }
            b_size = Some(size);
            limit = Some(b_start + size);
        }
        let first_reverse = reverse[0].unwrap_or(false);
        let (i0, v0, _, first_length) = sorts[0];
        let iterate_index =
            limit.is_none() && (rlen as f64) > (first_length as f64) * (rlen as f64 / 100.0 + 1.0);
        let mut actual = rlen;
        let mut slen = 0i64;
        let mut result: Vec<i32>;
        if iterate_index && sorts.len() > 1 {
            // Groups by the whole key: the key of the first index, then the
            // key of each other index that the row has. Each group keeps
            // its rows by ascending rid; `multisort` orders the groups.
            let rows = in_order(rs.iter().collect());
            let ranks: Vec<Vec<Option<u64>>> =
                sorts.iter().map(|(i, v, _, _)| self.ranks(*i, v, &rows)).collect();
            let mut groups: std::collections::BTreeMap<Vec<u64>, Vec<i32>> = Default::default();
            for (k, row) in rows.iter().enumerate() {
                let Some(first) = ranks[0][k] else { continue };
                let mut key = vec![first];
                key.extend(ranks[1..].iter().filter_map(|r| r[k]));
                groups.entry(key).or_default().push(self.rid_of(*row));
            }
            let mut items: Vec<(Vec<u64>, Vec<i32>)> = groups.into_iter().collect();
            if first_reverse {
                items.sort_by(|a, b| b.0.cmp(&a.0));
            }
            let spec: Vec<bool> = reverse.iter().map_while(|r| *r).collect();
            if spec.iter().any(|r| *r != first_reverse) {
                if items.iter().any(|(key, _)| key.len() < spec.len()) {
                    return fallback("ZCatalog raises: a row without a key of a sort index");
                }
                items.sort_by(|a, b| {
                    for (k, r) in spec.iter().enumerate() {
                        let o = a.0[k].cmp(&b.0[k]);
                        let o = if *r { o.reverse() } else { o };
                        if o != Ordering::Equal {
                            return o;
                        }
                    }
                    Ordering::Equal
                });
            }
            result = items.into_iter().flat_map(|(_, rids)| rids).collect();
            slen = result.len() as i64;
        } else if iterate_index {
            // By key; the rows of a key by ascending rid. The rows without a
            // key are left out but stay in the count.
            let items = self.walk(i0, v0, &rs, first_reverse, false, None);
            slen = items.len() as i64;
            result = items.into_iter().map(|(_, rid)| rid).collect();
        } else if limit.is_none() || limit.is_some_and(|l| l * 4 > rlen) || sorts.len() > 1 {
            if sorts.len() == 1 {
                actual -= rlen - self.keyed_count(i0, v0, &rs) as i64;
                let items = self.walk(i0, v0, &rs, first_reverse, first_reverse, None);
                result = items.into_iter().map(|(_, rid)| rid).collect();
            } else {
                let rows = in_order(rs.iter().collect());
                let ranks: Vec<Vec<Option<u64>>> =
                    sorts.iter().map(|(i, v, _, _)| self.ranks(*i, v, &rows)).collect();
                let mut items: Vec<(Vec<u64>, i32)> = Vec::with_capacity(rows.len());
                for (k, row) in rows.iter().enumerate() {
                    if ranks.iter().all(|r| r[k].is_some()) {
                        items.push((ranks.iter().map(|r| r[k].unwrap_or(0)).collect(), self.rid_of(*row)));
                    } else {
                        actual -= 1;
                    }
                }
                // `multisort`: a stable sort on the whole key in the order
                // of the first index, then, when the orders differ, a
                // stable sort on the keys that have an order, each in its
                // own. Equal keys keep the order of rids.
                if first_reverse {
                    items.sort_by(|a, b| b.0.cmp(&a.0));
                } else {
                    items.sort_by(|a, b| a.0.cmp(&b.0));
                }
                let spec: Vec<bool> = reverse.iter().map_while(|r| *r).collect();
                if spec.iter().any(|r| *r != first_reverse) {
                    items.sort_by(|a, b| {
                        for (k, r) in spec.iter().enumerate() {
                            let o = a.0[k].cmp(&b.0[k]);
                            let o = if *r { o.reverse() } else { o };
                            if o != Ordering::Equal {
                                return o;
                            }
                        }
                        Ordering::Equal
                    });
                }
                result = items.into_iter().map(|(_, rid)| rid).collect();
            }
            if let Some(l) = limit {
                result.truncate(l.max(0) as usize);
            }
        } else {
            let limit = limit.unwrap_or(0).max(0) as usize;
            actual -= rlen - self.keyed_count(i0, v0, &rs) as i64;
            if first_reverse {
                // The rows at or above the threshold, then `_sort_nbest`.
                let mut items = self.walk(i0, v0, &rs, true, false, Some(limit));
                items.sort_by_key(|(_, rid)| *rid);
                result = nbest(&items, limit);
            } else {
                let items = self.walk(i0, v0, &rs, false, false, Some(limit));
                result = items.into_iter().take(limit).map(|(_, rid)| rid).collect();
            }
        }
        // `_limit_sequence`.
        if let Some(size) = b_size {
            result = result
                .into_iter()
                .skip(b_start.max(0) as usize)
                .take(size.max(0) as usize)
                .collect();
            if slen != 0 {
                slen = result.len() as i64;
            }
        }
        if switched {
            result.reverse();
        }
        let len = if iterate_index { slen } else { result.len() as i64 };
        let rows: Vec<u32> = result.iter().map(|rid| self.row_of(*rid)).collect();
        Ok(self.reply(plan, rows, len, actual))
    }

    fn reply(&self, plan: &Plan, rows: Vec<u32>, len: i64, actual: i64) -> Reply {
        let rids: Vec<i32> = rows.iter().map(|r| self.rid_of(*r)).collect();
        let mut out_rows = Vec::new();
        if plan.rows {
            for rid in &rids {
                let path = crate::version::slot_value(self.base, self.patch, Slot::Paths, *rid)
                    .and_then(|v| v.text().map(str::to_owned))
                    .unwrap_or_default();
                let row = match crate::version::slot_value(self.base, self.patch, Slot::Data, *rid) {
                    Some(Val::Bytes(b)) => b.into_vec(),
                    _ => Vec::new(),
                };
                out_rows.push((path, row));
            }
        }
        Reply {
            rids,
            len,
            actual,
            rows: out_rows,
        }
    }
}

/// The ranks of the values of a patch that the base has no key for.
struct RankTable {
    extra: Vec<(Val, u64)>,
}

/// The rank of the key at `slot` of a base: above the values of the patch
/// that sort before it.
fn slot_rank(slot: u32) -> u64 {
    (2 * u64::from(slot) + 1) << 20
}

/// `_sort_nbest` of ZCatalog: the `limit` rows of the highest keys, as its
/// insertion of each row of the result set in the order of rids keeps
/// them; then by descending key, and the rows of one key by descending
/// rid. `items` holds (rank, rid) in the order of rids.
pub fn nbest(items: &[(u64, i32)], limit: usize) -> Vec<i32> {
    let mut chosen: Vec<(u64, i32)> = if items.len() <= limit {
        items.to_vec()
    } else {
        let mut ranks: Vec<u64> = items.iter().map(|(k, _)| *k).collect();
        let place = items.len() - limit;
        let (_, threshold, _) = ranks.select_nth_unstable(place);
        let threshold = *threshold;
        let mut out: Vec<(u64, i32)> = items.iter().filter(|(k, _)| *k > threshold).copied().collect();
        let room = limit - out.len();
        // The first `limit` rows at or above the threshold, in the order of
        // rids, hold the rows of the threshold that stay: the last `room`.
        let ties: Vec<(u64, i32)> = items
            .iter()
            .filter(|(k, _)| *k >= threshold)
            .take(limit)
            .filter(|(k, _)| *k == threshold)
            .copied()
            .collect();
        out.extend(ties[ties.len().saturating_sub(room)..].iter().copied());
        out
    };
    chosen.sort_by(|a, b| b.cmp(a));
    chosen.into_iter().map(|(_, rid)| rid).collect()
}

fn min_of(keys: &[Val]) -> Option<Val> {
    let mut it = keys.iter();
    let mut best = it.next()?.clone();
    for k in it {
        if py_cmp(k, &best)? == Ordering::Less {
            best = k.clone();
        }
    }
    Some(best)
}

fn max_of(keys: &[Val]) -> Option<Val> {
    let mut it = keys.iter();
    let mut best = it.next()?.clone();
    for k in it {
        if py_cmp(k, &best)? == Ordering::Greater {
            best = k.clone();
        }
    }
    Some(best)
}

/// The keys and postings of a value index or a keyword index.
#[derive(Clone, Copy)]
enum Lists<'a> {
    Value(&'a ValueIndex),
    Keyword(&'a Keys, &'a Postings),
}

impl<'a> Lists<'a> {
    fn keys(&self) -> &'a Keys {
        match self {
            Lists::Value(v) => &v.keys,
            Lists::Keyword(k, _) => k,
        }
    }

    fn postings(&self) -> &'a Postings {
        match self {
            Lists::Value(v) => &v.postings,
            Lists::Keyword(_, p) => p,
        }
    }

    /// Tells if a row with the value `value` has the key `key`.
    fn has(&self, value: &Val, key: &Val) -> bool {
        match (self, value) {
            (Lists::Keyword(..), Val::List(items) | Val::Tuple(items)) => items
                .iter()
                .any(|v| py_cmp(v, key) == Some(Ordering::Equal)),
            _ => py_cmp(value, key) == Some(Ordering::Equal),
        }
    }
}

/// The structures of `search` of ExtendedPathIndex, in a version.
struct PathView<'v, 'a> {
    view: &'v View<'a>,
    index: &'a base::PathIndex,
    changed: Vec<(u32, Option<&'a str>)>,
    touched: RoaringBitmap,
}

impl<'v, 'a> PathView<'v, 'a> {
    fn new(view: &'v View<'a>, i: usize, index: &'a base::PathIndex) -> Self {
        let changed: Vec<(u32, Option<&'a str>)> = view
            .changed(Slot::Index(i))
            .into_iter()
            .map(|(row, v)| (row, v.and_then(Val::text)))
            .collect();
        let touched = view.touched(Slot::Index(i));
        let _ = view.patch.depths.get(&i);
        PathView {
            view,
            index,
            changed,
            touched,
        }
    }

    fn depth(&self) -> i64 {
        let i = self
            .view
            .base
            .indexes
            .iter()
            .position(|e| matches!(&e.index, Index::Path(p) if std::ptr::eq(p, self.index)))
            .unwrap_or(0);
        self.view
            .patch
            .depths
            .get(&i)
            .copied()
            .unwrap_or(self.index.depth)
    }

    fn patched(&self, base: Option<Posting<'_>>, test: impl Fn(&str) -> bool) -> Option<RoaringBitmap> {
        let mut rows = base.map(|p| p.to_bitmap()).unwrap_or_default();
        rows -= &self.touched;
        for (row, path) in &self.changed {
            if let Some(path) = path
                && test(path)
            {
                rows.insert(*row);
            }
        }
        (!rows.is_empty()).then_some(rows)
    }

    /// `_index[comp][level]`.
    fn at(&self, comp: &str, level: i64) -> Option<RoaringBitmap> {
        self.patched(self.index.at(comp, level), |path| {
            let cs = components(path);
            level >= 0 && cs.get(level as usize) == Some(&comp)
        })
    }

    /// `_index[None][level]`.
    fn ends_at(&self, level: i64) -> Option<RoaringBitmap> {
        let base = (level >= 0)
            .then(|| self.index.ends.get(level as usize))
            .flatten()
            .map(Posting::Many);
        self.patched(base, |path| components(path).len() as i64 - 1 == level)
    }

    /// `_index_parents[path]`.
    fn parents(&self, parent: &str) -> Option<RoaringBitmap> {
        self.patched(self.index.parents.get(parent).map(Posting::Many), |path| {
            let cs = components(path);
            format!("/{}", cs[..cs.len().saturating_sub(1)].join("/")) == parent
        })
    }

    /// `_index_items[path]`.
    fn item(&self, path: &str) -> Option<u32> {
        for (row, p) in &self.changed {
            if *p == Some(path) {
                return Some(*row);
            }
        }
        self.index
            .item(path)
            .filter(|row| !self.touched.contains(*row))
    }

    fn all(&self) -> RoaringBitmap {
        let mut rows = &self.index.indexed - &self.touched;
        for (row, p) in &self.changed {
            if p.is_some() {
                rows.insert(*row);
            }
        }
        rows
    }

    /// `ExtendedPathIndex.search`.
    fn search(
        &self,
        path: &str,
        level: i64,
        depth: i64,
        navtree: bool,
        navtree_start: i64,
        index_path: &[String],
    ) -> Result<RoaringBitmap, Fallback> {
        let mut level = level;
        let mut depth = depth;
        if level < 0 {
            let mut out = RoaringBitmap::new();
            for l in 0..=self.depth() {
                out |= self.search(path, l, depth, navtree, navtree_start, index_path)?;
            }
            return Ok(out);
        }
        let mut comps: Vec<&str> = components(path);
        if navtree_start < 0 {
            return fallback("a negative navtree_start");
        }
        if navtree && depth == -1 {
            depth = 1;
        }
        let pathlength = level + comps.len() as i64 - 1;
        if navtree && navtree_start > (pathlength + depth).min(self.depth()) {
            return Ok(RoaringBitmap::new());
        }
        if level == 0 && (depth == 0 || depth == 1) {
            if navtree {
                let mut result = RoaringBitmap::new();
                let mut i = comps.len() as i64;
                while i >= navtree_start {
                    let parent_path = format!("/{}", comps[..i.max(0) as usize].join("/"));
                    if depth == 1 {
                        if let Some(rows) = self.parents(&parent_path) {
                            result |= rows;
                        }
                    } else if let Some(row) = self.item(&parent_path) {
                        result.insert(row);
                    }
                    i -= 1;
                }
                return Ok(result);
            }
            let path = if path.starts_with('/') {
                path.to_owned()
            } else {
                format!("/{path}")
            };
            if depth == 0 {
                let mut out = RoaringBitmap::new();
                if let Some(row) = self.item(&path) {
                    out.insert(row);
                }
                return Ok(out);
            }
            return Ok(self.parents(&path).unwrap_or_default());
        }
        if level == 0 {
            let indexpath: Vec<&str> = index_path.iter().map(String::as_str).filter(|p| !p.is_empty()).collect();
            let minlength = indexpath.len().min(comps.len());
            for i in 0..minlength {
                if indexpath[i] != comps[i] {
                    break;
                }
                level += 1;
            }
            comps = comps[level as usize..].to_vec();
        }
        if comps.is_empty() && depth == -1 {
            return Ok(self.all());
        }
        let mut pathset: Option<RoaringBitmap> = None;
        let mut depthset: Option<RoaringBitmap> = None;
        if navtree && depth > 0 {
            let mut d = RoaringBitmap::new();
            for i in navtree_start.min(level)..=navtree_start.max(level) {
                if let Some(rows) = self.ends_at(i) {
                    d |= rows;
                }
            }
            depthset = Some(d);
        }
        let mut indexed: Vec<(usize, &str)> = comps.iter().copied().enumerate().collect();
        if !navtree {
            indexed.reverse();
        }
        for (i, comp) in indexed {
            let at = i as i64 + level;
            let res = self.at(comp, at);
            match res {
                None => {
                    pathset = Some(RoaringBitmap::new());
                    if !navtree {
                        return Ok(RoaringBitmap::new());
                    }
                }
                Some(res) => {
                    pathset = Some(match pathset {
                        None => res,
                        Some(p) => p & res,
                    });
                }
            }
            if navtree && at >= navtree_start {
                let ends = self.ends_at(at);
                let part = match (&pathset, ends) {
                    (Some(p), Some(e)) => p & e,
                    (Some(p), None) => p.clone(),
                    (None, Some(e)) => e,
                    (None, None) => RoaringBitmap::new(),
                };
                depthset = Some(match depthset {
                    None => part,
                    Some(d) => d | part,
                });
            }
        }
        if depth >= 0 {
            let mut start = comps.len() as i64 - 1;
            if navtree {
                start = start.max(navtree_start - level);
            }
            // BTrees' `intersection(a, None)` is `a`, and `(None, b)` is `b`.
            let mut out = depthset.unwrap_or_default();
            for i in start..=start + depth {
                match (&pathset, self.ends_at(i + level)) {
                    (Some(p), Some(e)) => out |= p & e,
                    (Some(p), None) => out |= p,
                    (None, Some(e)) => out |= e,
                    (None, None) => {}
                }
            }
            return Ok(out);
        }
        if navtree {
            return Ok(depthset.unwrap_or_default());
        }
        Ok(pathset.unwrap_or_default())
    }
}
