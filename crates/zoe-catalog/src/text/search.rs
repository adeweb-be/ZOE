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

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::sync::Arc;

use tantivy::index::SegmentId;
use tantivy::query::{
    BooleanQuery, ConstScorer, EnableScoring, Explanation, Occur, Query, Scorer, Weight,
};
use tantivy::{DocId, DocSet, Score, Searcher, SegmentReader, TERMINATED};

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
}

/// A hit: the integer score and the rid. The greater is the better.
pub type Hit = (i64, i32);

/// What a search found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Found {
    /// The number of the hits in the filter.
    pub count: u64,
    /// The hits: by score, then rid descending, for `Want::Top`; in no
    /// order, with the score 0, for `Want::Set`.
    pub hits: Vec<Hit>,
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
    /// The rids of the filter, sorted, or `None` for no filter.
    pub filter: Option<&'a [i32]>,
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
    let chosen = forced
        .unwrap_or_else(|| strategy(maps.num_docs(), scope.filter.map(|f| f.len() as u64), bound));
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
    match (chosen, scope.filter) {
        (Strategy::None, _) | (_, None) => {
            checks.extend(excluded.into_iter().map(Check::Live));
        }
        (Strategy::HitsDrive, Some(_)) => checks.extend((0..segments.len()).map(|_| Check::Rids)),
        (Strategy::FilterDrives | Strategy::Bitset, Some(filter)) => {
            let mut bits: Vec<DocBits> = maps.max_docs.iter().map(|m| DocBits::new(*m)).collect();
            // Both lists are in rid order: one merge.
            let mut docs = maps.by_rid.iter().peekable();
            for rid in filter {
                while docs.peek().is_some_and(|d| d.0 < *rid) {
                    docs.next();
                }
                if let Some(&&(r, ord, doc)) = docs.peek()
                    && r == *rid
                    && !scope.replaced.contains(rid)
                {
                    bits[ord as usize].insert(doc);
                    if chosen == Strategy::FilterDrives {
                        drive_docs[ord as usize].push(doc);
                    }
                }
            }
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
                    && scope.filter.is_some_and(|f| f.binary_search(&rid).is_ok())
                    && !scope.replaced.contains(&rid)
            }
        }
    };
    let rid_of = |ord: usize, doc: DocId| maps.doc_to_rid[ord][doc as usize];
    let found = match want {
        Want::Set => {
            let weight = query.weight(EnableScoring::disabled_from_searcher(searcher))?;
            let mut hits = Vec::new();
            for (ord, seg) in segments.iter().enumerate() {
                weight.for_each_no_score(seg, &mut |docs| {
                    for &d in docs {
                        if keep(ord, seg, d) {
                            hits.push((0, rid_of(ord, d)));
                        }
                    }
                })?;
            }
            Found {
                count: hits.len() as u64,
                hits,
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
            }
        }
    };
    Ok((found, chosen))
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

    #[test]
    fn the_top_orders_by_score_then_rid_descending() {
        let mut top = Top::new(Some(3));
        for hit in [(5, 1), (7, -2), (5, 9), (7, 3), (1, 100)] {
            top.push(hit);
        }
        assert_eq!(top.into_sorted(), vec![(7, 3), (7, -2), (5, 9)]);
    }
}
