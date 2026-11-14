//! The search of the tantivy index of a catalog inside a filter, and the
//! integer scores (ADR 0060, sections 11 and 12).
//!
//! The score of a hit is the `f32` of tantivy times 1,024, rounded to the
//! nearest integer. The hits order by that integer, then by the rid
//! descending. The collector keeps the first hits by that order; its
//! pruning threshold is the highest `f32` that rounds below the integer of
//! the last hit that it keeps, so a hit that rounds to that integer still
//! comes in.
//!
//! The filter is the rids of the other clauses. The smaller side drives:
//!
//! - A filter that holds fewer than one document of the index in 32
//!   drives: its documents lead the intersection, with a score of 0.
//! - A query whose bound of hits is less than a quarter of the filter
//!   drives: each hit looks up its rid in the filter.
//! - Otherwise the filter becomes a bitset of the documents of each
//!   segment, which the collector checks in the callback of the pruning of
//!   tantivy (block-max WAND). A second pass on the same weight counts the
//!   hits in the filter with no score.
//!
//! A query with no bound has one pass that scores every hit. The search
//! leaves out the replaced documents of the text overlay.
//!
//! A filter is a sorted list of rids, or the rows of the mirror with their
//! rids. Each rid of a filter has a key: its place in the list, or its row.
//! A set of the hits for a sort gives the keys of its hits (`Want::Keys`):
//! one merge of the filter with the documents of the hits, in rid order.
//! With a bitset, that merge applies the filter, and the search reads the
//! hits with no filter.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::sync::Arc;

use tantivy::index::SegmentId;
use tantivy::query::{
    BooleanQuery, ConstScorer, EnableScoring, Explanation, Occur, Query, Scorer, Weight,
};
use tantivy::{DocId, DocSet, Score, Searcher, SegmentReader, TERMINATED};

use roaring::RoaringBitmap;

use super::index::Maps;

/// A filter drives the search when it holds fewer than one document of the
/// index in `DRIVE`.
pub const DRIVE: u64 = 32;

/// The hits drive when their bound is less than one in `HITS` of the
/// filter.
pub const HITS: u64 = 4;

/// The integer score of the `f32` score `score`.
pub fn to_int(score: Score) -> i64 {
    if !score.is_finite() {
        return 0;
    }
    (f64::from(score) * 1024.0).round() as i64
}

/// The highest `f32` whose integer score is below `k`: the pruning
/// threshold for a last kept hit of the integer `k`.
pub fn below(k: i64) -> Score {
    let mut x = ((k as f64 - 0.5) / 1024.0) as f32;
    while to_int(x) >= k {
        x = x.next_down();
    }
    while to_int(x.next_up()) < k {
        x = x.next_up();
    }
    x
}

/// How the search meets its filter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Strategy {
    /// No filter.
    None,
    /// The documents of the filter drive the search.
    FilterDrives,
    /// Each hit looks up its rid in the filter.
    HitsDrive,
    /// The filter is a bitset of each segment.
    Bitset,
}

/// The strategy of a search of an index of `num_docs` documents, with a
/// filter of `filter` rids, for a query whose bound of hits is `bound`.
pub fn strategy(num_docs: u64, filter: Option<u64>, bound: Option<u64>) -> Strategy {
    let Some(len) = filter else {
        return Strategy::None;
    };
    if len.saturating_mul(DRIVE) < num_docs {
        Strategy::FilterDrives
    } else if bound.is_some_and(|b| b.saturating_mul(HITS) < len) {
        Strategy::HitsDrive
    } else {
        Strategy::Bitset
    }
}

/// A set of the documents of one segment.
#[derive(Clone, Debug)]
pub struct DocBits {
    words: Vec<u64>,
}

impl DocBits {
    /// An empty set for a segment of `max_doc` documents.
    pub fn new(max_doc: u32) -> DocBits {
        DocBits {
            words: vec![0; (max_doc as usize).div_ceil(64)],
        }
    }

    /// Add `doc`.
    #[inline]
    pub fn insert(&mut self, doc: DocId) {
        self.words[(doc >> 6) as usize] |= 1 << (doc & 63);
    }

    /// Remove `doc`.
    #[inline]
    pub fn remove(&mut self, doc: DocId) {
        if let Some(w) = self.words.get_mut((doc >> 6) as usize) {
            *w &= !(1 << (doc & 63));
        }
    }

    /// Tell if the set holds `doc`.
    #[inline]
    pub fn contains(&self, doc: DocId) -> bool {
        self.words
            .get((doc >> 6) as usize)
            .is_some_and(|w| w & (1 << (doc & 63)) != 0)
    }
}

/// What the search wants.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Want {
    /// The count of the hits and the first hits by score, at most the
    /// limit, or every hit.
    Top(Option<usize>),
    /// The rids of the hits, with no score.
    Set,
    /// The keys of the hits in the filter, with no rid and no score. With
    /// no filter, as `Set`.
    Keys,
}

/// A hit: the integer score and the rid. The greater is the better.
pub type Hit = (i64, i32);

/// What a search found.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Found {
    /// The number of the hits in the filter.
    pub count: u64,
    /// The hits: by score, then rid descending, for `Want::Top`; in rid
    /// order, with the score 0, for `Want::Set`; none for `Want::Keys` with
    /// a filter.
    pub hits: Vec<Hit>,
    /// The keys of the hits, for `Want::Keys` with a filter.
    pub keys: Option<RoaringBitmap>,
}

/// The filter of a search: the rids of the other clauses. Each rid of the
/// filter has a key.
#[derive(Clone, Copy, Debug)]
pub enum Filter<'a> {
    /// The rids, sorted, each once. The key of a rid is its place.
    Rids(&'a [i32]),
    /// The rows `rows` of a table. The key of a rid is its row.
    Rows {
        /// The rows.
        rows: &'a RoaringBitmap,
        /// The rid of each first row of the table, in ascending order.
        rids: &'a [i32],
        /// The other rows of `rows`, each with its rid, in rid order.
        more: &'a [(i32, u32)],
    },
}

impl Filter<'_> {
    /// The number of the rids.
    pub fn len(&self) -> u64 {
        match self {
            Filter::Rids(rids) => rids.len() as u64,
            Filter::Rows { rows, .. } => rows.len(),
        }
    }

    /// Tell if the filter holds no rid.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The key of `rid`, when the filter holds it.
    #[inline]
    pub fn key_of(&self, rid: i32) -> Option<u32> {
        match self {
            Filter::Rids(rids) => rids.binary_search(&rid).ok().map(|k| k as u32),
            Filter::Rows { rows, rids, more } => match rids.binary_search(&rid) {
                Ok(row) => Some(row as u32).filter(|row| rows.contains(*row)),
                Err(_) => more
                    .binary_search_by_key(&rid, |m| m.0)
                    .ok()
                    .map(|i| more[i].1),
            },
        }
    }

    /// Tell if the filter holds `rid`.
    #[inline]
    pub fn contains(&self, rid: i32) -> bool {
        self.key_of(rid).is_some()
    }

    /// A bound of the keys: each key is below it.
    pub fn key_bound(&self) -> usize {
        match self {
            Filter::Rids(rids) => rids.len(),
            Filter::Rows { rows, .. } => rows.max().map_or(0, |m| m as usize + 1),
        }
    }

    /// Call `f` with each rid and its key, in rid order.
    #[inline]
    pub fn for_each(&self, mut f: impl FnMut(i32, u32)) {
        match self {
            Filter::Rids(rids) => {
                for (k, rid) in rids.iter().enumerate() {
                    f(*rid, k as u32);
                }
            }
            Filter::Rows { rows, rids, more } => {
                // The first rows ascend with their rids: a merge with the
                // other rows.
                let mut m = 0;
                let first = rids.len() as u32;
                let mut iter = rows.iter();
                let mut buf = [0u32; 256];
                'rows: loop {
                    let batch = iter.next_many(&mut buf);
                    if batch.is_empty() {
                        break;
                    }
                    for &row in batch.iter() {
                        if row >= first {
                            break 'rows;
                        }
                        let rid = rids[row as usize];
                        while m < more.len() && more[m].0 < rid {
                            f(more[m].0, more[m].1);
                            m += 1;
                        }
                        f(rid, row);
                    }
                }
                for &(rid, row) in &more[m..] {
                    f(rid, row);
                }
            }
        }
    }
}

/// Call `f` with the rid, the key, the segment and the document of each
/// rid of `filter` that is a live document of `maps`, in rid order.
pub fn each_doc(maps: &Maps, filter: Filter<'_>, mut f: impl FnMut(i32, u32, u32, DocId)) {
    // Both lists are in rid order: one merge.
    let docs = &maps.by_rid;
    let mut i = 0;
    filter.for_each(|rid, key| {
        while i < docs.len() && docs[i].0 < rid {
            i += 1;
        }
        if let Some(&(r, ord, doc)) = docs.get(i)
            && r == rid
        {
            f(rid, key, ord, doc);
        }
    });
}

/// The check of the filter in one segment.
enum Check {
    /// The live documents that are not replaced.
    Live(Option<DocBits>),
    /// The rid of each hit is in the filter and not replaced.
    Rids,
    /// The documents of the bitset.
    Bits(DocBits),
}

/// The filter of a search and the replaced rids.
pub struct Scope<'a> {
    /// The filter, or `None` for no filter.
    pub filter: Option<Filter<'a>>,
    /// The rids of the replaced documents.
    pub replaced: &'a HashSet<i32>,
}

/// Search `query` in `searcher`, inside `scope`, for `want`. `bound` is the
/// bound of the hits of the query, `None` when it has none. `strategy`
/// forces a strategy, for the tests; `None` picks it.
pub fn search(
    searcher: &Searcher,
    maps: &Maps,
    query: &dyn Query,
    bound: Option<u64>,
    scope: &Scope<'_>,
    want: Want,
    forced: Option<Strategy>,
) -> tantivy::Result<(Found, Strategy)> {
    let chosen =
        forced.unwrap_or_else(|| strategy(maps.num_docs(), scope.filter.map(|f| f.len()), bound));
    let chosen = match (scope.filter, chosen) {
        (None, _) => Strategy::None,
        (Some(_), Strategy::None) => Strategy::Bitset,
        (_, s) => s,
    };
    let segments = searcher.segment_readers();
    // The replaced documents of each segment.
    let mut excluded: Vec<Option<DocBits>> = vec![None; segments.len()];
    for rid in scope.replaced {
        if let Some((ord, doc)) = maps.doc(*rid) {
            excluded[ord as usize]
                .get_or_insert_with(|| DocBits::new(maps.max_docs[ord as usize]))
                .insert(doc);
        }
    }
    let mut checks: Vec<Check> = Vec::with_capacity(segments.len());
    let mut drive_docs: Vec<Vec<DocId>> = vec![Vec::new(); segments.len()];
    // The keys of a set come from a merge of the filter with the hits: with
    // a bitset, that merge applies the filter, and the search needs none.
    let unfiltered = want == Want::Keys && chosen == Strategy::Bitset;
    match (chosen, scope.filter) {
        (Strategy::None, _) | (_, None) => {
            checks.extend(excluded.into_iter().map(Check::Live));
        }
        _ if unfiltered => checks.extend(excluded.into_iter().map(Check::Live)),
        (Strategy::HitsDrive, Some(_)) => checks.extend((0..segments.len()).map(|_| Check::Rids)),
        (Strategy::FilterDrives | Strategy::Bitset, Some(filter)) => {
            let mut bits: Vec<DocBits> = maps.max_docs.iter().map(|m| DocBits::new(*m)).collect();
            each_doc(maps, filter, |rid, _, ord, doc| {
                if scope.replaced.is_empty() || !scope.replaced.contains(&rid) {
                    bits[ord as usize].insert(doc);
                    if chosen == Strategy::FilterDrives {
                        drive_docs[ord as usize].push(doc);
                    }
                }
            });
            for d in &mut drive_docs {
                d.sort_unstable();
            }
            checks.extend(bits.into_iter().map(Check::Bits));
        }
    }
    let driven: Box<dyn Query>;
    let query: &dyn Query = if chosen == Strategy::FilterDrives {
        let map = segments
            .iter()
            .zip(drive_docs)
            .map(|(seg, d)| (seg.segment_id(), d))
            .collect();
        driven = Box::new(BooleanQuery::new(vec![
            (Occur::Must, query.box_clone()),
            (
                Occur::Must,
                Box::new(DocsQuery {
                    docs: Arc::new(map),
                }),
            ),
        ]));
        driven.as_ref()
    } else {
        query
    };
    let keep = |ord: usize, seg: &SegmentReader, doc: DocId| -> bool {
        match &checks[ord] {
            Check::Bits(b) => b.contains(doc),
            Check::Live(ex) => {
                seg.alive_bitset().is_none_or(|a| a.is_alive(doc))
                    && ex.as_ref().is_none_or(|e| !e.contains(doc))
            }
            Check::Rids => {
                let rid = maps.doc_to_rid[ord][doc as usize];
                seg.alive_bitset().is_none_or(|a| a.is_alive(doc))
                    && scope.filter.is_some_and(|f| f.contains(rid))
                    && !scope.replaced.contains(&rid)
            }
        }
    };
    let rid_of = |ord: usize, doc: DocId| maps.doc_to_rid[ord][doc as usize];
    let found = match want {
        Want::Set | Want::Keys => {
            let weight = query.weight(EnableScoring::disabled_from_searcher(searcher))?;
            let mut docs = HitDocs::new(maps);
            for (ord, seg) in segments.iter().enumerate() {
                weight.for_each_no_score(seg, &mut |found| {
                    for &d in found {
                        if keep(ord, seg, d) {
                            docs.insert(ord, d);
                        }
                    }
                })?;
            }
            match (want, scope.filter) {
                (Want::Keys, Some(filter)) => docs.into_keys(maps, filter),
                _ => docs.into_found(maps),
            }
        }
        Want::Top(limit) => {
            let weight = query.weight(EnableScoring::enabled_from_searcher(searcher))?;
            let prune = bound.is_some() && limit.is_some();
            let mut top = Top::new(limit);
            let mut count = 0u64;
            for (ord, seg) in segments.iter().enumerate() {
                if prune {
                    weight.for_each_no_score(seg, &mut |docs| {
                        count += docs.iter().filter(|d| keep(ord, seg, **d)).count() as u64;
                    })?;
                    let mut threshold = top.threshold();
                    weight.for_each_pruning(threshold, seg, &mut |doc, score| {
                        if keep(ord, seg, doc) {
                            top.push((to_int(score), rid_of(ord, doc)));
                            threshold = top.threshold();
                        }
                        threshold
                    })?;
                } else {
                    weight.for_each(seg, &mut |doc, score| {
                        if keep(ord, seg, doc) {
                            count += 1;
                            top.push((to_int(score), rid_of(ord, doc)));
                        }
                    })?;
                }
            }
            Found {
                count,
                hits: top.into_sorted(),
                keys: None,
            }
        }
    };
    Ok((found, chosen))
}

/// The documents of a set of hits, a bitset for each segment.
pub struct HitDocs {
    bits: Vec<DocBits>,
    count: usize,
}

impl HitDocs {
    /// An empty set for the segments of `maps`.
    pub fn new(maps: &Maps) -> HitDocs {
        HitDocs {
            bits: maps.max_docs.iter().map(|m| DocBits::new(*m)).collect(),
            count: 0,
        }
    }

    /// Add the document `doc` of the segment `ord`, once.
    #[inline]
    pub fn insert(&mut self, ord: usize, doc: DocId) {
        self.bits[ord].insert(doc);
        self.count += 1;
    }

    /// The hits in rid order, with the score 0. Each document must be a
    /// live document of `maps`.
    pub fn into_found(self, maps: &Maps) -> Found {
        Found {
            count: self.count as u64,
            hits: self.by_rid(maps),
            keys: None,
        }
    }

    /// The keys of the hits in `filter`. Each document must be a live
    /// document of `maps` whose rid is in the filter.
    pub fn into_keys(self, maps: &Maps, filter: Filter<'_>) -> Found {
        if self.count.saturating_mul(16) < filter.len() as usize {
            // A small set: the key of each hit.
            let mut keys: Vec<u32> = self
                .by_rid(maps)
                .into_iter()
                .filter_map(|(_, rid)| filter.key_of(rid))
                .collect();
            keys.sort_unstable();
            return Found {
                count: keys.len() as u64,
                hits: Vec::new(),
                keys: Some(RoaringBitmap::from_sorted_iter(keys).unwrap_or_default()),
            };
        }
        // A large set: the merge of the filter with the documents, into a
        // bitset of the keys.
        let mut words = vec![0u64; filter.key_bound().div_ceil(64)];
        let mut count = 0u64;
        each_doc(maps, filter, |_, key, ord, doc| {
            if self.bits[ord as usize].contains(doc) {
                words[(key >> 6) as usize] |= 1 << (key & 63);
                count += 1;
            }
        });
        let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        Found {
            count,
            hits: Vec::new(),
            keys: Some(RoaringBitmap::from_lsb0_bytes(0, &bytes)),
        }
    }

    /// The hits in rid order, with the score 0. A large set walks the
    /// documents in rid order; a small one sorts its rids.
    fn by_rid(&self, maps: &Maps) -> Vec<Hit> {
        let mut hits: Vec<Hit> = Vec::with_capacity(self.count);
        if self.count.saturating_mul(16) >= maps.by_rid.len() {
            for &(rid, ord, doc) in &maps.by_rid {
                if self.bits[ord as usize].contains(doc) {
                    hits.push((0, rid));
                }
            }
        } else {
            for (ord, bits) in self.bits.iter().enumerate() {
                let rids = &maps.doc_to_rid[ord];
                for (w, word) in bits.words.iter().enumerate() {
                    let mut word = *word;
                    while word != 0 {
                        let doc = (w as u32) * 64 + word.trailing_zeros();
                        hits.push((0, rids[doc as usize]));
                        word &= word - 1;
                    }
                }
            }
            hits.sort_unstable_by_key(|h| h.1);
        }
        hits
    }
}

/// Add the hits `more` of the text overlay to the set `found` of
/// `want`. Each is in `filter` and not in `found`.
pub fn add_to_set(found: &mut Found, want: Want, more: Vec<i32>, filter: Option<Filter<'_>>) {
    found.count += more.len() as u64;
    match (want, filter) {
        (Want::Keys, Some(filter)) => found
            .keys
            .get_or_insert_with(RoaringBitmap::new)
            .extend(more.iter().filter_map(|rid| filter.key_of(*rid))),
        _ => merge_by_rid(
            &mut found.hits,
            more.into_iter().map(|rid| (0, rid)).collect(),
        ),
    }
}

/// Merge the hits `more`, in rid order, into the hits `hits`, in rid
/// order. No rid is in both.
pub fn merge_by_rid(hits: &mut Vec<Hit>, mut more: Vec<Hit>) {
    if more.is_empty() {
        return;
    }
    more.sort_unstable_by_key(|h| h.1);
    let mut out = Vec::with_capacity(hits.len() + more.len());
    let (mut i, mut j) = (0, 0);
    while i < hits.len() && j < more.len() {
        if hits[i].1 < more[j].1 {
            out.push(hits[i]);
            i += 1;
        } else {
            out.push(more[j]);
            j += 1;
        }
    }
    out.extend_from_slice(&hits[i..]);
    out.extend_from_slice(&more[j..]);
    *hits = out;
}

/// The first hits by score, then rid descending.
pub struct Top {
    limit: Option<usize>,
    heap: BinaryHeap<Reverse<Hit>>,
    all: Vec<Hit>,
}

impl Top {
    /// Keep at most `limit` hits, or every hit for `None`.
    pub fn new(limit: Option<usize>) -> Top {
        Top {
            limit,
            heap: BinaryHeap::new(),
            all: Vec::new(),
        }
    }

    /// Offer `hit`.
    pub fn push(&mut self, hit: Hit) {
        match self.limit {
            None => self.all.push(hit),
            Some(0) => {}
            Some(limit) => {
                if self.heap.len() < limit {
                    self.heap.push(Reverse(hit));
                } else if self.heap.peek().is_some_and(|m| hit > m.0) {
                    self.heap.pop();
                    self.heap.push(Reverse(hit));
                }
            }
        }
    }

    /// The pruning threshold: the highest `f32` that rounds below the last
    /// kept hit once the heap is full, else the lowest score.
    pub fn threshold(&self) -> Score {
        match (self.limit, self.heap.peek()) {
            (Some(limit), Some(m)) if self.heap.len() >= limit => below(m.0.0),
            _ => Score::MIN,
        }
    }

    /// The kept hits, the best first.
    pub fn into_sorted(self) -> Vec<Hit> {
        let mut out: Vec<Hit> = match self.limit {
            None => self.all,
            Some(_) => self.heap.into_iter().map(|Reverse(h)| h).collect(),
        };
        out.sort_unstable_by(|a, b| b.cmp(a));
        out
    }
}

/// A query that matches a sorted list of documents of each segment, with a
/// score of 0.
#[derive(Clone, Debug)]
struct DocsQuery {
    docs: Arc<HashMap<SegmentId, Vec<DocId>>>,
}

impl Query for DocsQuery {
    fn weight(&self, _: EnableScoring<'_>) -> tantivy::Result<Box<dyn Weight>> {
        Ok(Box::new(DocsWeight {
            docs: Arc::clone(&self.docs),
        }))
    }
}

struct DocsWeight {
    docs: Arc<HashMap<SegmentId, Vec<DocId>>>,
}

impl Weight for DocsWeight {
    fn scorer(&self, reader: &SegmentReader, _boost: Score) -> tantivy::Result<Box<dyn Scorer>> {
        let docs = self
            .docs
            .get(&reader.segment_id())
            .cloned()
            .unwrap_or_default();
        Ok(Box::new(ConstScorer::new(
            SortedDocs { docs, cursor: 0 },
            0.0,
        )))
    }

    fn explain(&self, _reader: &SegmentReader, _doc: DocId) -> tantivy::Result<Explanation> {
        Ok(Explanation::new("filter", 0.0))
    }
}

/// A sorted list of documents as a `DocSet`.
struct SortedDocs {
    docs: Vec<DocId>,
    cursor: usize,
}

impl DocSet for SortedDocs {
    fn advance(&mut self) -> DocId {
        self.cursor = (self.cursor + 1).min(self.docs.len());
        self.doc()
    }

    fn seek(&mut self, target: DocId) -> DocId {
        self.cursor += self.docs[self.cursor..].partition_point(|d| *d < target);
        self.doc()
    }

    fn doc(&self) -> DocId {
        self.docs.get(self.cursor).copied().unwrap_or(TERMINATED)
    }

    fn size_hint(&self) -> u32 {
        self.docs.len() as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn below_is_the_threshold_of_an_integer() {
        for k in [1, 2, 100, 1023, 1024, 4097, 1_000_000] {
            let x = below(k);
            assert!(to_int(x) < k, "{k}");
            assert!(to_int(x.next_up()) >= k, "{k}");
        }
    }

    #[test]
    fn the_strategy_follows_the_smaller_side() {
        assert_eq!(strategy(10_000, None, Some(5)), Strategy::None);
        assert_eq!(strategy(10_000, Some(100), None), Strategy::FilterDrives);
        assert_eq!(
            strategy(10_000, Some(5_000), Some(100)),
            Strategy::HitsDrive
        );
        assert_eq!(strategy(10_000, Some(5_000), Some(5_000)), Strategy::Bitset);
        assert_eq!(strategy(10_000, Some(5_000), None), Strategy::Bitset);
    }

    /// Maps of two segments of 40 documents, each with a distinct rid, the
    /// document 1 of the first segment deleted.
    fn two_segments() -> Maps {
        let mut maps = Maps::default();
        for ord in 0..2u32 {
            let rids: Vec<i32> = (0..40)
                .map(|doc| ((ord as i32 * 40 + doc) * 37) % 101 - 50)
                .collect();
            for (doc, rid) in rids.iter().enumerate() {
                if (ord, doc) != (0, 1) {
                    maps.by_rid.push((*rid, ord, doc as DocId));
                }
            }
            maps.doc_to_rid.push(Arc::new(rids));
            maps.max_docs.push(40);
        }
        maps.by_rid.sort_unstable_by_key(|e| e.0);
        maps
    }

    #[test]
    fn a_set_of_hits_comes_in_rid_order() {
        let maps = two_segments();
        let live = maps.by_rid.clone();
        // A small set sorts its rids; a large one walks the documents in
        // rid order.
        for step in [1, 2, 3, 7, 17, 40, 79, 100] {
            for first in [0, 1, 5] {
                let mut docs = HitDocs::new(&maps);
                let mut want = Vec::new();
                for (rid, ord, doc) in live.iter().skip(first).step_by(step) {
                    docs.insert(*ord as usize, *doc);
                    want.push((0, *rid));
                }
                let found = docs.into_found(&maps);
                assert_eq!(found.hits, want, "{step} {first}");
                assert_eq!(found.count, want.len() as u64);
                assert_eq!(found.keys, None);
            }
        }
    }

    #[test]
    fn a_set_in_a_filter_gives_the_keys_of_its_hits() {
        let maps = two_segments();
        let live = maps.by_rid.clone();
        // A filter of every other rid from -60 to 58: as a list; as the rows
        // of a table whose first rows ascend with their rids, without the
        // rows 3 and 5; and as such a table whose rids from -10 to 8 are
        // later rows, from the row 100 on, out of rid order.
        let rids: Vec<i32> = (-30..30).map(|k| 2 * k).collect();
        let mut rows: RoaringBitmap = (0..rids.len() as u32).collect();
        rows.remove(3);
        rows.remove(5);
        let listed: Vec<i32> = rows.iter().map(|r| rids[r as usize]).collect();
        let first: Vec<i32> = rids
            .iter()
            .copied()
            .filter(|r| !(-10..10).contains(r))
            .collect();
        let mut more: Vec<(i32, u32)> = (-5..5).map(|k| (2 * k, 109 - (k + 5) as u32)).collect();
        more.sort_unstable();
        let mut split: RoaringBitmap = (0..first.len() as u32).collect();
        split.extend(more.iter().map(|m| m.1));
        let filters = [
            Filter::Rids(&listed),
            Filter::Rows {
                rows: &rows,
                rids: &rids,
                more: &[],
            },
            Filter::Rows {
                rows: &split,
                rids: &first,
                more: &more,
            },
        ];
        for filter in filters {
            let mut seen = Vec::new();
            filter.for_each(|rid, key| seen.push((rid, key)));
            assert_eq!(seen.len() as u64, filter.len());
            assert!(
                seen.iter()
                    .all(|(rid, key)| filter.key_of(*rid) == Some(*key))
            );
            assert!(
                seen.iter()
                    .all(|(_, key)| (*key as usize) < filter.key_bound())
            );
            assert!(seen.windows(2).all(|w| w[0].0 < w[1].0));
            assert!(!filter.contains(1) && !filter.contains(-61) && !filter.contains(60));
            let rid_of: HashMap<u32, i32> = seen.iter().map(|(rid, key)| (*key, *rid)).collect();
            let rids_of = |found: &Found| -> Vec<i32> {
                let keys = found.keys.as_ref().unwrap();
                let mut out: Vec<i32> = keys.iter().map(|k| rid_of[&k]).collect();
                out.sort_unstable();
                out
            };
            // A small set looks up the key of each hit; a large one merges
            // the filter with the documents.
            for step in [1, 2, 9, 30, 79] {
                let mut docs = HitDocs::new(&maps);
                let mut want = Vec::new();
                for (rid, ord, doc) in live.iter().step_by(step) {
                    docs.insert(*ord as usize, *doc);
                    if filter.contains(*rid) {
                        want.push(*rid);
                    }
                }
                let found = docs.into_keys(&maps, filter);
                assert!(found.hits.is_empty());
                assert_eq!(rids_of(&found), want, "{filter:?} {step}");
                assert_eq!(found.count, want.len() as u64);
            }
            let mut found = HitDocs::new(&maps).into_keys(&maps, filter);
            add_to_set(&mut found, Want::Keys, vec![4, -8], Some(filter));
            assert_eq!(rids_of(&found), vec![-8, 4]);
            assert_eq!(found.count, 2);
        }
        let mut found = HitDocs::new(&maps).into_found(&maps);
        add_to_set(&mut found, Want::Set, vec![4, -8], Some(filters[0]));
        assert_eq!(found.hits, vec![(0, -8), (0, 4)]);
        assert_eq!(found.count, 2);
    }

    #[test]
    fn a_merge_keeps_the_rid_order() {
        let mut hits = vec![(0, -4), (0, 2), (0, 9)];
        merge_by_rid(&mut hits, vec![(0, 10), (0, -7), (0, 3)]);
        assert_eq!(
            hits,
            vec![(0, -7), (0, -4), (0, 2), (0, 3), (0, 9), (0, 10)]
        );
        merge_by_rid(&mut hits, Vec::new());
        assert_eq!(hits.len(), 6);
        let mut empty = Vec::new();
        merge_by_rid(&mut empty, vec![(0, 1)]);
        assert_eq!(empty, vec![(0, 1)]);
    }

    #[test]
    fn the_top_orders_by_score_then_rid_descending() {
        let mut top = Top::new(Some(3));
        for hit in [(5, 1), (7, -2), (5, 9), (7, 3), (1, 100)] {
            top.push(hit);
        }
        assert_eq!(top.into_sorted(), vec![(7, 3), (7, -2), (5, 9)]);
    }
}
