//! The query of a typed plan over a version: the clauses as ZCatalog's
//! indexes apply them, and the sort as `Catalog.sortResults` does it.
//!
//! A version is a base and its overrides. Each clause applies to the base,
//! then to the rows of the overrides. A row of a rid that the base does
//! not hold gets a row number after the rows of the base.
//!
//! The order of a result follows the four sort paths of ZCatalog, each
//! with its rule of ties (finding, "The order of ZCatalog"): the path that
//! iterates the index, the path that sorts the result set, `_sort_nbest`
//! and `_sort_nbest_reverse`. A batch past the middle of the result takes
//! the other end, as `sortResults` does.

use std::cmp::Ordering;
use std::collections::HashMap;

use roaring::RoaringBitmap;

use crate::base::{
    Base, DateRangeIndex, FieldIndex, Index, Keys, KeywordIndex, Kind, Posting, Postings, covers,
    date_range_of, keywords_of,
};
use crate::change::{Overrides, Slot, same};
use crate::reason::{Fallback, Reason};
use crate::value::{Val, py_cmp};

/// A clause of a plan, typed by the client with ZCatalog's `IndexQuery`
/// and the conversion of the index. The class of the index tells how the
/// clause reads: a clause on a BooleanIndex has one key, its truth, or
/// none; a clause on a DateRangeIndex has one key, its date in minutes, or
/// `None` for a date that the index does not hold.
#[derive(Clone, Debug, PartialEq)]
pub struct Clause {
    /// The index.
    pub index: String,
    /// The keys, converted. A range clause has none.
    pub keys: Vec<Val>,
    /// True for the operator `and`.
    pub and: bool,
    /// The lowest and the highest key of a range, both included; `None`
    /// for an open end.
    pub range: Option<(Option<Val>, Option<Val>)>,
    /// The keys of `not`, converted.
    pub not: Vec<Val>,
}

/// A typed plan.
#[derive(Clone, Debug, Default, PartialEq)]
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
}

/// The answer of a plan.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Reply {
    /// The rids of the result, in order.
    pub rids: Vec<i32>,
    /// The length of the lazy result of ZCatalog.
    pub len: i64,
    /// `actual_result_count`.
    pub actual: i64,
    /// How the range clauses ran.
    pub trace: Trace,
}

/// How the range clauses of a query ran.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Trace {
    /// The range clauses that tested the key of each row of the other
    /// clauses.
    pub range_by_rows: usize,
    /// The range clauses that took the rows of each key of the range.
    pub range_by_keys: usize,
}

/// More keys than this in a range, with no smaller result set: the range
/// tests the key of each row of the index.
const MANY_KEYS: u32 = 4096;

fn fallback<T>(reason: Reason, why: impl Into<String>) -> Result<T, Fallback> {
    Err(Fallback::new(reason, why))
}

/// A version of a mirror seen by one query: a base and its overrides.
pub struct View<'a> {
    base: &'a Base,
    over: &'a Overrides,
    /// The rids of the overrides that the base has no row for, in order.
    /// Their rows follow the rows of the base.
    extra: Vec<i32>,
}

impl<'a> View<'a> {
    /// The view of `base` with `over`.
    pub fn new(base: &'a Base, over: &'a Overrides) -> View<'a> {
        let mut extra: Vec<i32> = over
            .trees
            .values()
            .flat_map(|rows| rows.keys().copied())
            .filter(|rid| base.rows.row(*rid).is_none())
            .collect();
        extra.sort_unstable();
        extra.dedup();
        View { base, over, extra }
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
            self.base.rows.rid(row)
        } else {
            self.extra[row as usize - n]
        }
    }

    /// Tells if the order of the rows of `rows` is the order of their rids.
    fn in_rid_order(&self, rows: &RoaringBitmap) -> bool {
        self.base.rows.in_rid_order()
            && rows
                .max()
                .is_none_or(|max| (max as usize) < self.base.rows.len())
    }

    /// The rows of `rows` in the order of their rids.
    fn by_rid(&self, rows: &RoaringBitmap) -> Vec<u32> {
        let mut out: Vec<u32> = rows.iter().collect();
        if !self.in_rid_order(rows) {
            out.sort_by_key(|row| self.rid_of(*row));
        }
        out
    }

    /// The changed rows of the tree at `slot`, with their values.
    fn changed(&self, slot: Slot) -> Vec<(u32, Option<&'a Val>)> {
        match self.over.trees.get(&slot) {
            Some(rows) => rows
                .iter()
                .map(|(rid, v)| (self.row_of(*rid), v.as_ref()))
                .collect(),
            None => Vec::new(),
        }
    }

    /// The rows of the catalog.
    fn all_rows(&self) -> RoaringBitmap {
        let mut rows = self.base.rows.live.clone();
        for (row, value) in self.changed(Slot::Paths) {
            if value.is_some() {
                rows.insert(row);
            } else {
                rows.remove(row);
            }
        }
        rows
    }

    /// The index `name`: its place, its kind and its structure.
    fn entry(&self, name: &str) -> Result<(usize, Kind, &'a Index), Fallback> {
        let Some(&i) = self.base.by_name.get(name) else {
            return fallback(
                Reason::IndexUnavailable,
                format!("the catalog has no index {name}"),
            );
        };
        let entry = &self.base.indexes[i];
        match (&entry.index, entry.kind) {
            (Index::Unavailable(reason), _) => fallback(
                Reason::IndexUnavailable,
                format!("the index {name}: {reason}"),
            ),
            (index, Some(kind)) => Ok((i, kind, index)),
            (_, None) => fallback(
                Reason::IndexUnavailable,
                format!("the index {name} is of a class that the mirror does not serve"),
            ),
        }
    }

    /// The index `name` of a sort: a FieldIndex, a DateIndex or a
    /// UUIDIndex.
    fn field(&self, name: &str) -> Result<(usize, &'a FieldIndex), Fallback> {
        let (i, kind, index) = self.entry(name)?;
        match index {
            Index::Field(f) if kind.sorts() => {
                if kind == Kind::Uuid {
                    self.unique(i, f)?;
                }
                Ok((i, f))
            }
            _ => fallback(
                Reason::Sort,
                format!("a sort on {name}, an index of a kind that the mirror does not sort"),
            ),
        }
    }

    /// Refuses a UUIDIndex that holds a UUID at more than one row in this
    /// version. ZCatalog keeps one of these rows in its `_index`, which
    /// the mirror does not read.
    fn unique(&self, i: usize, f: &FieldIndex) -> Result<(), Fallback> {
        let twice = || {
            fallback(
                Reason::IndexUnavailable,
                "the UUIDIndex holds a UUID at more than one row",
            )
        };
        let changed = self.changed(Slot::Index(i));
        let touched: RoaringBitmap = changed.iter().map(|(row, _)| *row).collect();
        for rows in f.postings.many() {
            if (rows - &touched).len() > 1 {
                return twice();
            }
        }
        let values: Vec<&Val> = changed.iter().filter_map(|(_, v)| *v).collect();
        for value in &values {
            if let Some(Ok(s)) = f.keys.search(value)
                && !(f.postings.get(s).to_bitmap() - &touched).is_empty()
            {
                return twice();
            }
        }
        let texts: Option<Vec<&str>> = values.iter().map(|v| v.text()).collect();
        let repeated = match texts {
            Some(mut texts) => {
                texts.sort_unstable();
                texts.windows(2).any(|w| w[0] == w[1])
            }
            None => values
                .iter()
                .enumerate()
                .any(|(k, a)| values[k + 1..].iter().any(|b| same(a, b))),
        };
        if repeated {
            return twice();
        }
        Ok(())
    }

    /// The `_length` of the index at `i` in this version.
    fn length(&self, i: usize) -> i64 {
        self.over
            .lengths
            .get(&i)
            .copied()
            .unwrap_or(self.base.indexes[i].length)
    }

    /// Answers `plan`.
    pub fn query(&self, plan: &Plan) -> Result<Reply, Fallback> {
        let mut trace = Trace::default();
        // The clauses on a DateRangeIndex apply last, so they see the
        // result of the others: ZCatalog's answer for a row without an
        // entry depends on the order of its plan.
        let mut ranges: Vec<(usize, &DateRangeIndex, Option<i64>)> = Vec::new();
        let mut order: Vec<&Clause> = Vec::new();
        for clause in &plan.clauses {
            let (i, _, index) = self.entry(&clause.index)?;
            match index {
                Index::DateRange(d) => {
                    let t = term(clause)?;
                    if t.is_none() && plan.clauses.len() > 1 {
                        // ZCatalog raises unless the index comes first in
                        // its plan.
                        return fallback(
                            Reason::DateRangeOrder,
                            "a date outside the range of a DateRangeIndex, with other clauses",
                        );
                    }
                    ranges.push((i, d, t));
                }
                _ => order.push(clause),
            }
        }
        // ZCatalog intersects the sets of the indexes in an order of its
        // own; the sets of the other indexes intersect in any order. The
        // ranges come last, so they see the smallest result set.
        order.sort_by_key(|c| c.range.is_some());
        let mut rs: Option<RoaringBitmap> = None;
        for clause in order {
            let set = self.clause(clause, rs.as_ref(), &mut trace)?;
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
        if !ranges.is_empty() && !rs.as_ref().is_some_and(RoaringBitmap::is_empty) {
            rs = Some(self.date_ranges(&ranges, rs, plan.clauses.len() == 1)?);
        }
        let rs = match rs {
            Some(rs) if !rs.is_empty() => rs,
            // An empty result, or a query that no index uses.
            _ => {
                return Ok(Reply {
                    trace,
                    ..Reply::default()
                });
            }
        };
        let mut reply = self.sort(plan, rs)?;
        reply.trace = trace;
        Ok(reply)
    }

    /// The rows of the clauses on DateRangeIndexes over `rs`, the result of
    /// the other clauses. ZCatalog drops a row without an entry when the
    /// index comes first in its plan, and keeps it when the index comes
    /// later: such a row in the result gives `date_range_order`. The only
    /// clause of a query always comes first.
    fn date_ranges(
        &self,
        ranges: &[(usize, &DateRangeIndex, Option<i64>)],
        rs: Option<RoaringBitmap>,
        alone: bool,
    ) -> Result<RoaringBitmap, Fallback> {
        if alone && let [(i, d, t)] = ranges {
            return Ok(self.date_range(*i, d, *t).0);
        }
        let mut r = rs.unwrap_or_else(|| self.all_rows());
        let mut entries = Vec::with_capacity(ranges.len());
        for (i, d, t) in ranges {
            let (matched, with_entry) = self.date_range(*i, d, *t);
            let missing = &r - &with_entry;
            r &= &matched;
            r |= missing;
            entries.push(with_entry);
        }
        if entries.iter().any(|e| !(&r - e).is_empty()) {
            return fallback(
                Reason::DateRangeOrder,
                "a row of the result has no entry in a DateRangeIndex of the query",
            );
        }
        Ok(r)
    }

    /// The rows whose range holds `t` in the DateRangeIndex at `i`, in this
    /// version, and the rows with an entry. For no date, each row with an
    /// entry: ZCatalog then takes every key of its trees.
    fn date_range(
        &self,
        i: usize,
        d: &DateRangeIndex,
        t: Option<i64>,
    ) -> (RoaringBitmap, RoaringBitmap) {
        let mut entries = d.indexed.clone();
        let mut matched = match t {
            Some(t) => d.matched(t),
            None => d.indexed.clone(),
        };
        for (row, value) in self.changed(Slot::Index(i)) {
            entries.remove(row);
            matched.remove(row);
            if let Some(bounds) = value.and_then(date_range_of) {
                entries.insert(row);
                if t.is_none_or(|t| covers(bounds, t)) {
                    matched.insert(row);
                }
            }
        }
        (matched, entries)
    }

    /// The rows of `key` in the index at `i`, in this version.
    fn posting(&self, i: usize, keyed: Keyed<'_>, key: &Val) -> RoaringBitmap {
        let mut rows = match keyed.keys().search(key) {
            Some(Ok(s)) => keyed.postings().get(s).to_bitmap(),
            _ => RoaringBitmap::new(),
        };
        for (row, value) in self.changed(Slot::Index(i)) {
            rows.remove(row);
            if value.is_some_and(|v| keyed.items(v).iter().any(|x| same(x, key))) {
                rows.insert(row);
            }
        }
        rows
    }

    /// The rows with a key in the index at `i`, in this version.
    fn indexed(&self, i: usize, keyed: Keyed<'_>) -> RoaringBitmap {
        let mut rows = keyed.indexed().clone();
        for (row, value) in self.changed(Slot::Index(i)) {
            if value.is_some_and(|v| !keyed.items(v).is_empty()) {
                rows.insert(row);
            } else {
                rows.remove(row);
            }
        }
        rows
    }

    /// The rows of the keys of `not`, which `_apply_not` gives.
    fn excluded(&self, i: usize, keyed: Keyed<'_>, not: &[Val]) -> RoaringBitmap {
        let mut out = RoaringBitmap::new();
        for key in not.iter().filter(|k| **k != Val::None) {
            out |= self.posting(i, keyed, key);
        }
        out
    }

    /// The rows of a clause, as the `query_index` of its index gives them
    /// for the result set `rs`. A clause on a DateRangeIndex applies in
    /// `date_ranges`.
    fn clause(
        &self,
        clause: &Clause,
        rs: Option<&RoaringBitmap>,
        trace: &mut Trace,
    ) -> Result<RoaringBitmap, Fallback> {
        let (i, kind, index) = self.entry(&clause.index)?;
        match (kind, index) {
            (Kind::Boolean, Index::Field(f)) => self.boolean(i, f, clause),
            (Kind::Keyword, Index::Keyword(k)) => self.values(i, Keyed::Many(k), clause, rs, trace),
            (Kind::Uuid, Index::Field(f)) => {
                self.unique(i, f)?;
                self.values(i, Keyed::One(f), clause, rs, trace)
            }
            (Kind::Field | Kind::Date, Index::Field(f)) => {
                self.values(i, Keyed::One(f), clause, rs, trace)
            }
            _ => fallback(
                Reason::IndexUnavailable,
                format!(
                    "the structure of the index {} is not of its class",
                    clause.index
                ),
            ),
        }
    }

    /// The rows of a clause on a BooleanIndex: the rows whose value has the
    /// truth of the first key, as `BooleanIndex.query_index` gives them. No
    /// key gives no row.
    fn boolean(
        &self,
        i: usize,
        f: &FieldIndex,
        clause: &Clause,
    ) -> Result<RoaringBitmap, Fallback> {
        if clause.range.is_some() || !clause.not.is_empty() || clause.and || clause.keys.len() > 1 {
            return fallback(
                Reason::Option,
                "an option that a BooleanIndex does not take",
            );
        }
        let Some(key) = clause.keys.first() else {
            return Ok(RoaringBitmap::new());
        };
        let wanted = truth(key);
        let mut rows = RoaringBitmap::new();
        for s in 0..f.keys.len() as u32 {
            if truth(&f.keys.get(s)) == wanted {
                f.postings.get(s).add_to(&mut rows);
            }
        }
        for (row, value) in self.changed(Slot::Index(i)) {
            rows.remove(row);
            if value.is_some_and(|v| truth(v) == wanted) {
                rows.insert(row);
            }
        }
        Ok(rows)
    }

    /// The rows of a clause of keys, as `UnIndex.query_index` gives them for
    /// the result set `rs`.
    fn values(
        &self,
        i: usize,
        keyed: Keyed<'_>,
        clause: &Clause,
        rs: Option<&RoaringBitmap>,
        trace: &mut Trace,
    ) -> Result<RoaringBitmap, Fallback> {
        let keys: Vec<&Val> = clause.keys.iter().filter(|k| **k != Val::None).collect();
        if let Some((lo, hi)) = &clause.range {
            if clause.and {
                return fallback(Reason::AndRange, "a range with the operator and");
            }
            let mut rows = self.range(i, keyed, lo.as_ref(), hi.as_ref(), rs, trace)?;
            if !clause.not.is_empty() {
                rows -= self.excluded(i, keyed, &clause.not);
            }
            return Ok(rows);
        }
        if clause.keys.is_empty() && !clause.not.is_empty() {
            // A query of `not` alone: the rows of the result set, or of
            // the catalog, without the rows of `not`. When no row has a
            // key of `not`, ZCatalog takes every key of the index instead,
            // so it gives the rows with a key.
            if clause.and {
                return fallback(Reason::Option, "a query of not alone with the operator and");
            }
            let excluded = self.excluded(i, keyed, &clause.not);
            if !excluded.is_empty() {
                let mut all = match rs {
                    Some(r) => r.clone(),
                    None => self.all_rows(),
                };
                all -= excluded;
                return Ok(all);
            }
            return Ok(self.indexed(i, keyed));
        }
        let mut sets = Vec::with_capacity(keys.len());
        for key in keys {
            let rows = self.posting(i, keyed, key);
            if rows.is_empty() && clause.and {
                return Ok(RoaringBitmap::new());
            }
            if !rows.is_empty() {
                sets.push(rows);
            }
        }
        let mut r = if clause.and {
            let mut it = sets.into_iter();
            let mut r = match (it.next(), rs) {
                (Some(first), _) => first,
                // `and` with no key gives the result set.
                (None, Some(rs)) => rs.clone(),
                (None, None) => RoaringBitmap::new(),
            };
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
        };
        if !clause.not.is_empty() {
            r -= self.excluded(i, keyed, &clause.not);
        }
        Ok(r)
    }

    /// The rows with a key from `lo` to `hi`, both included, as
    /// `index.values(lo, hi)` gives them.
    ///
    /// When the result set of the other clauses holds fewer rows than the
    /// range has keys, the range tests the keys of each row of that set:
    /// one read of the ranks of each row costs less than the union of the
    /// rows of each key.
    fn range(
        &self,
        i: usize,
        keyed: Keyed<'_>,
        lo: Option<&Val>,
        hi: Option<&Val>,
        rs: Option<&RoaringBitmap>,
        trace: &mut Trace,
    ) -> Result<RoaringBitmap, Fallback> {
        let unordered = || {
            Fallback::new(
                Reason::Option,
                "a bound of a range that the keys of the index do not order",
            )
        };
        let keys_of = keyed.keys();
        let first = match lo {
            None => 0,
            Some(lo) => match keys_of.search(lo) {
                Some(Ok(s) | Err(s)) => s,
                None => return Err(unordered()),
            },
        };
        let end = match hi {
            None => keys_of.len() as u32,
            Some(hi) => match keys_of.search(hi) {
                Some(Ok(s)) => s + 1,
                Some(Err(s)) => s,
                None => return Err(unordered()),
            },
        };
        let in_range = |v: &Val| -> Option<bool> {
            let above = match lo {
                None => true,
                Some(lo) => py_cmp(v, lo)? != Ordering::Less,
            };
            let below = match hi {
                None => true,
                Some(hi) => py_cmp(v, hi)? != Ordering::Greater,
            };
            Some(above && below)
        };
        let mut rows = RoaringBitmap::new();
        if end > first {
            let keys = end - first;
            let in_keys = |row: u32| keyed.row_in(row, first, end);
            match rs {
                Some(rs) if rs.len() < u64::from(keys) => {
                    trace.range_by_rows += 1;
                    rows.extend(rs.iter().filter(|row| in_keys(*row)));
                }
                _ if keys > MANY_KEYS => {
                    trace.range_by_rows += 1;
                    rows.extend(keyed.indexed().iter().filter(|row| in_keys(*row)));
                }
                _ => {
                    trace.range_by_keys += 1;
                    for s in first..end {
                        keyed.postings().get(s).add_to(&mut rows);
                    }
                }
            }
        } else {
            trace.range_by_keys += 1;
        }
        for (row, value) in self.changed(Slot::Index(i)) {
            rows.remove(row);
            if let Some(value) = value {
                let mut found = false;
                for item in keyed.items(value) {
                    found |= in_range(item).ok_or_else(unordered)?;
                }
                if found {
                    rows.insert(row);
                }
            }
        }
        Ok(rows)
    }

    /// The ranks of the values of the overrides that the keys of the base
    /// do not hold, so that each value of the version has a rank in the
    /// order of the keys.
    fn rank_table(&self, i: usize, f: &FieldIndex) -> Vec<(Val, u64)> {
        let mut extra: Vec<(Val, u32)> = Vec::new();
        for (_, value) in self.changed(Slot::Index(i)) {
            if let Some(value) = value
                && let Some(Err(place)) = f.keys.search(value)
            {
                extra.push((value.clone(), place));
            }
        }
        extra.sort_by(|a, b| py_cmp(&a.0, &b.0).unwrap_or(Ordering::Equal));
        extra.dedup_by(|a, b| same(&a.0, &b.0));
        let mut ranked = Vec::with_capacity(extra.len());
        let mut sub = 0u64;
        let mut last = None;
        for (value, place) in extra {
            sub = if last == Some(place) { sub + 1 } else { 1 };
            last = Some(place);
            ranked.push((value, ((2 * u64::from(place)) << 20) | sub));
        }
        ranked
    }

    /// The rank of a value among the keys of `f` and the values of the
    /// overrides.
    fn rank_of(f: &FieldIndex, table: &[(Val, u64)], value: &Val) -> Option<u64> {
        match f.keys.search(value)? {
            Ok(s) => Some(slot_rank(s)),
            Err(_) => table
                .binary_search_by(|(x, _)| py_cmp(x, value).unwrap_or(Ordering::Equal))
                .ok()
                .map(|k| table[k].1),
        }
    }

    /// The rows of `rs` that the overrides changed in the index at `i`,
    /// with the rank of their new value, and the changed rows.
    fn patched_ranks(
        &self,
        i: usize,
        f: &FieldIndex,
        rs: &RoaringBitmap,
    ) -> (Vec<(u64, i32)>, RoaringBitmap) {
        let mut items = Vec::new();
        let mut touched = RoaringBitmap::new();
        let table = self.rank_table(i, f);
        for (row, value) in self.changed(Slot::Index(i)) {
            touched.insert(row);
            if rs.contains(row)
                && let Some(rank) = value.and_then(|value| Self::rank_of(f, &table, value))
            {
                items.push((rank, self.rid_of(row)));
            }
        }
        (items, touched)
    }

    /// The number of rows of `rs` with a key in the index at `i`.
    fn keyed_count(&self, i: usize, f: &FieldIndex, rs: &RoaringBitmap) -> u64 {
        let (patched, touched) = self.patched_ranks(i, f, rs);
        let base = rs & &f.indexed;
        base.len() - (&base & &touched).len() + patched.len() as u64
    }

    /// The rows of `rs` with a key in the index at `i`, by key, with their
    /// ranks: by descending key when `desc`, and the rows of one key by
    /// descending rid when `tie_desc`. With `stop`, the walk ends with the
    /// key at which it holds `stop` rows.
    fn walk(
        &self,
        i: usize,
        f: &FieldIndex,
        rs: &RoaringBitmap,
        desc: bool,
        tie_desc: bool,
        stop: Option<usize>,
    ) -> Vec<(u64, i32)> {
        let (mut patched, touched) = self.patched_ranks(i, f, rs);
        let keys = f.keys.len() as u64;
        let order = |a: &(u64, i32), b: &(u64, i32)| {
            let by_rank = if desc { b.0.cmp(&a.0) } else { a.0.cmp(&b.0) };
            by_rank.then(if tie_desc {
                b.1.cmp(&a.1)
            } else {
                a.1.cmp(&b.1)
            })
        };
        // Few rows against many keys: the rank of each row, then a sort.
        if stop.is_none() && keys > 4 * rs.len() {
            let mut items = patched;
            for row in rs.iter() {
                if !touched.contains(row)
                    && let Some(s) = f.slot(row)
                {
                    items.push((slot_rank(s), self.rid_of(row)));
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
        let tie = |a: &(u64, i32), b: &(u64, i32)| {
            if tie_desc {
                b.1.cmp(&a.1)
            } else {
                a.1.cmp(&b.1)
            }
        };
        for s in slots {
            let rank = slot_rank(s);
            // The values of the overrides that sort before this key.
            while p < patched.len() && before(patched[p].0, rank) {
                let r = patched[p].0;
                while p < patched.len() && patched[p].0 == r {
                    out.push(patched[p]);
                    p += 1;
                }
                if stop.is_some_and(|n| out.len() >= n) {
                    return out;
                }
            }
            group.clear();
            match f.postings.get(s) {
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
            }
            while p < patched.len() && patched[p].0 == rank {
                group.push(patched[p]);
                p += 1;
            }
            if group.is_empty() {
                continue;
            }
            group.sort_by(tie);
            out.extend(group.iter().copied());
            if stop.is_some_and(|n| out.len() >= n) {
                return out;
            }
        }
        out.extend(patched[p..].iter().copied());
        out
    }

    /// The rank of the key of each row of `rows`.
    fn ranks(&self, i: usize, f: &FieldIndex, rows: &[u32]) -> Vec<Option<u64>> {
        let changed: HashMap<u32, Option<&Val>> =
            self.changed(Slot::Index(i)).into_iter().collect();
        let table = self.rank_table(i, f);
        rows.iter()
            .map(|row| match changed.get(row) {
                Some(value) => value.and_then(|value| Self::rank_of(f, &table, value)),
                None => f.slot(*row).map(slot_rank),
            })
            .collect()
    }

    fn sort(&self, plan: &Plan, rs: RoaringBitmap) -> Result<Reply, Fallback> {
        let rlen = rs.len() as i64;
        // `_sort_limit_arguments`.
        let mut b_start = plan.b_start;
        let mut b_size = plan.b_size;
        let mut limit = plan.limit;
        if let Some(size) = b_size {
            limit = Some(b_start + size);
        } else if limit.is_some_and(|l| l != 0) {
            b_size = limit;
        }
        if plan.sort.is_empty() {
            // `_limit_sequence` of the rids in ascending order.
            let rows = self.by_rid(&rs);
            let (page, len) = match b_size {
                Some(size) => {
                    let page: Vec<u32> = rows
                        .into_iter()
                        .skip(b_start.max(0) as usize)
                        .take(size.max(0) as usize)
                        .collect();
                    let len = page.len() as i64;
                    (page, len)
                }
                None => (rows, rlen),
            };
            return Ok(self.reply(page, len, rlen));
        }
        let mut sorts = Vec::with_capacity(plan.sort.len());
        for (name, reverse) in &plan.sort {
            let (i, f) = self.field(name)?;
            sorts.push((i, f, *reverse));
        }
        let mut reverse: Vec<Option<bool>> = sorts.iter().map(|s| s.2).collect();
        if reverse[0].is_none() {
            return fallback(Reason::Sort, "a sort without an order for its first index");
        }
        // `sortResults`.
        if let Some(l) = limit
            && l >= rlen
        {
            limit = Some(rlen);
        }
        if limit == Some(0) {
            return fallback(Reason::Sort, "a sort limit of 0");
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
                        actual: rlen,
                        ..Reply::default()
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
        let (i0, f0, _) = sorts[0];
        let first_length = self.length(i0);
        let iterate_index =
            limit.is_none() && (rlen as f64) > (first_length as f64) * (rlen as f64 / 100.0 + 1.0);
        let mut actual = rlen;
        let mut slen = 0i64;
        let mut result: Vec<i32>;
        if iterate_index && sorts.len() > 1 {
            return fallback(
                Reason::Sort,
                "several sort indexes on the path that iterates the index",
            );
        } else if iterate_index {
            // By key; the rows of a key by ascending rid. The rows without a
            // key leave the result and stay in the count.
            let items = self.walk(i0, f0, &rs, first_reverse, false, None);
            slen = items.len() as i64;
            result = items.into_iter().map(|(_, rid)| rid).collect();
        } else if limit.is_none() || limit.is_some_and(|l| l * 4 > rlen) || sorts.len() > 1 {
            if sorts.len() == 1 {
                actual -= rlen - self.keyed_count(i0, f0, &rs) as i64;
                let items = self.walk(i0, f0, &rs, first_reverse, first_reverse, None);
                result = items.into_iter().map(|(_, rid)| rid).collect();
            } else {
                let rows = self.by_rid(&rs);
                let ranks: Vec<Vec<Option<u64>>> = sorts
                    .iter()
                    .map(|(i, f, _)| self.ranks(*i, f, &rows))
                    .collect();
                let mut items: Vec<(Vec<u64>, i32)> = Vec::with_capacity(rows.len());
                for (k, row) in rows.iter().enumerate() {
                    if ranks.iter().all(|r| r[k].is_some()) {
                        items.push((
                            ranks.iter().map(|r| r[k].unwrap_or(0)).collect(),
                            self.rid_of(*row),
                        ));
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
            actual -= rlen - self.keyed_count(i0, f0, &rs) as i64;
            if first_reverse {
                // The rows at or above the threshold, then `_sort_nbest`.
                let mut items = self.walk(i0, f0, &rs, true, false, Some(limit));
                items.sort_by_key(|(_, rid)| *rid);
                result = nbest(&items, limit);
            } else {
                let items = self.walk(i0, f0, &rs, false, false, Some(limit));
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
        let len = if iterate_index {
            slen
        } else {
            result.len() as i64
        };
        Ok(Reply {
            rids: result,
            len,
            actual,
            trace: Trace::default(),
        })
    }

    fn reply(&self, rows: Vec<u32>, len: i64, actual: i64) -> Reply {
        Reply {
            rids: rows.iter().map(|r| self.rid_of(*r)).collect(),
            len,
            actual,
            trace: Trace::default(),
        }
    }
}

/// The term of a clause on a DateRangeIndex: its one key, a date in
/// minutes, or `None` for a date that the index does not hold.
fn term(clause: &Clause) -> Result<Option<i64>, Fallback> {
    if clause.range.is_some() || !clause.not.is_empty() || clause.and {
        return fallback(
            Reason::Option,
            "an option that a DateRangeIndex does not take",
        );
    }
    match &clause.keys[..] {
        [Val::Int(t)] => Ok(Some(*t)),
        [Val::None] => Ok(None),
        _ => fallback(
            Reason::Option,
            "a clause on a DateRangeIndex takes one date",
        ),
    }
}

/// The truth of a value, as Python's `bool` gives it.
fn truth(value: &Val) -> bool {
    match value {
        Val::None | Val::Missing => false,
        Val::Bool(b) => *b,
        Val::Int(i) => *i != 0,
        Val::Float(f) => *f != 0.0,
        Val::Str(s) => !s.is_empty(),
        Val::Bytes(b) => !b.is_empty(),
        Val::Tuple(v) | Val::List(v) => !v.is_empty(),
        Val::Message(_) => value.text().is_some_and(|t| !t.is_empty()),
        Val::DateTime(..) => true,
    }
}

/// The structure that a clause of keys reads: one key for each row, or
/// several.
#[derive(Clone, Copy)]
enum Keyed<'a> {
    /// A FieldIndex, a DateIndex or a UUIDIndex.
    One(&'a FieldIndex),
    /// A KeywordIndex.
    Many(&'a KeywordIndex),
}

impl<'a> Keyed<'a> {
    fn keys(self) -> &'a Keys {
        match self {
            Keyed::One(f) => &f.keys,
            Keyed::Many(k) => &k.keys,
        }
    }

    fn postings(self) -> &'a Postings {
        match self {
            Keyed::One(f) => &f.postings,
            Keyed::Many(k) => &k.postings,
        }
    }

    fn indexed(self) -> &'a RoaringBitmap {
        match self {
            Keyed::One(f) => &f.indexed,
            Keyed::Many(k) => &k.indexed,
        }
    }

    /// Tells if a key of `row` in the base has a slot from `first` to
    /// `end`, `end` excluded.
    fn row_in(self, row: u32, first: u32, end: u32) -> bool {
        match self {
            Keyed::One(f) => f.slot(row).is_some_and(|s| s >= first && s < end),
            Keyed::Many(k) => k.row_slots(row).iter().any(|s| *s >= first && *s < end),
        }
    }

    /// The keys of a value of the overrides.
    fn items(self, value: &Val) -> &[Val] {
        match self {
            Keyed::One(_) => std::slice::from_ref(value),
            Keyed::Many(_) => keywords_of(value),
        }
    }
}

/// The rank of the key at `slot` of a base: above the values of the
/// overrides that sort before it.
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
        let mut out: Vec<(u64, i32)> = items
            .iter()
            .filter(|(k, _)| *k > threshold)
            .copied()
            .collect();
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
