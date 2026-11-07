//! The filter and the sort of a text clause, inside the search.
//!
//! The other clauses of a catalog query give a bitmap of rids. The text
//! index works on its own document ids. `RidMap` maps one to the other.
//! The smaller side drives:
//!
//! - A small filter drives the search: its documents seek the postings.
//! - A query with few hits checks the rid of each hit in the bitmap of the
//!   filter.
//! - Otherwise the filter becomes a bitset of documents for each segment
//!   before the search. The search checks a hit against that bitset before
//!   it keeps the hit, and the pruning of tantivy keeps working.
//!
//! With `sort_on`, the text clause gives its hits as a bitset, with no
//! score. `SortArray` stands for the sort array of the mirror: it gives the
//! first rows in the order of the sort.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::sync::Arc;

use roaring::RoaringBitmap;
use tantivy::collector::{Collector, SegmentCollector};
use tantivy::index::SegmentId;
use tantivy::query::{ConstScorer, EnableScoring, Explanation, Query, Scorer, Weight};
use tantivy::{DocAddress, DocId, DocSet, Score, Searcher, SegmentOrdinal, SegmentReader, TERMINATED};

/// A filter drives the search when it holds fewer than one document in
/// `DRIVE` of the index.
pub const DRIVE: u64 = 32;

/// The hits drive when the estimate of their number is less than one in
/// `HITS` of the filter: a check of a rid in the bitmap costs about `HITS`
/// times the copy of one rid of the filter into the bitset.
pub const HITS: u64 = 4;

/// A set of the documents of one segment, one bit for each document.
#[derive(Clone, Debug)]
pub struct DocBits {
    words: Vec<u64>,
}

impl DocBits {
    /// Make an empty set for a segment of `max_doc` documents.
    pub fn new(max_doc: u32) -> Self {
        Self { words: vec![0; (max_doc as usize).div_ceil(64)] }
    }

    /// Add `doc`.
    #[inline]
    pub fn insert(&mut self, doc: DocId) {
        self.words[(doc >> 6) as usize] |= 1 << (doc & 63);
    }

    /// Tell if the set holds `doc`.
    #[inline]
    pub fn contains(&self, doc: DocId) -> bool {
        self.words[(doc >> 6) as usize] & (1 << (doc & 63)) != 0
    }

    /// Keep the documents that `other` also holds.
    pub fn and_with(&mut self, other: &DocBits) {
        self.words.iter_mut().zip(&other.words).for_each(|(a, b)| *a &= b);
    }

    /// Add the documents of `other`.
    pub fn or_with(&mut self, other: &DocBits) {
        self.words.iter_mut().zip(&other.words).for_each(|(a, b)| *a |= b);
    }

    /// Remove the documents of `other`.
    pub fn andnot_with(&mut self, other: &DocBits) {
        self.words.iter_mut().zip(&other.words).for_each(|(a, b)| *a &= !b);
    }

    /// The number of documents in the set.
    pub fn len(&self) -> u64 {
        self.words.iter().map(|w| u64::from(w.count_ones())).sum()
    }

    /// Tell if the set is empty.
    pub fn is_empty(&self) -> bool {
        self.words.iter().all(|w| *w == 0)
    }

    /// The documents of the set, in ascending order.
    pub fn iter(&self) -> impl Iterator<Item = DocId> + '_ {
        self.words.iter().enumerate().flat_map(|(i, &w)| {
            let mut w = w;
            std::iter::from_fn(move || {
                if w == 0 {
                    return None;
                }
                let bit = w.trailing_zeros();
                w &= w - 1;
                Some((i as u32) << 6 | bit)
            })
        })
    }
}

/// The map between the rids of the catalog and the documents of the text
/// index. The mirror keeps it up to date with the index.
pub struct RidMap {
    /// The rid of each document of each segment.
    pub doc_to_rid: Vec<Arc<Vec<u32>>>,
    /// The segment and the document of each rid, `u64::MAX` when none.
    rid_to_doc: Vec<u64>,
    /// The number of documents of each segment.
    pub max_docs: Vec<u32>,
    /// The number of live documents of the index.
    pub num_docs: u64,
}

impl RidMap {
    /// Build the map from the fast field `rid` of `searcher`.
    pub fn build(searcher: &Searcher) -> tantivy::Result<Self> {
        let mut doc_to_rid = Vec::new();
        let mut rid_to_doc: Vec<u64> = Vec::new();
        let mut max_docs = Vec::new();
        let mut num_docs = 0;
        for (ord, seg) in searcher.segment_readers().iter().enumerate() {
            let rids = seg.fast_fields().u64("rid")?;
            let mut map = vec![u32::MAX; seg.max_doc() as usize];
            for doc in 0..seg.max_doc() {
                if seg.alive_bitset().is_some_and(|a| !a.is_alive(doc)) {
                    continue;
                }
                let rid = rids.first(doc).unwrap_or(0) as u32;
                map[doc as usize] = rid;
                if rid_to_doc.len() <= rid as usize {
                    rid_to_doc.resize(rid as usize + 1, u64::MAX);
                }
                rid_to_doc[rid as usize] = (ord as u64) << 32 | u64::from(doc);
                num_docs += 1;
            }
            doc_to_rid.push(Arc::new(map));
            max_docs.push(seg.max_doc());
        }
        Ok(Self { doc_to_rid, rid_to_doc, max_docs, num_docs })
    }

    /// The segment and the document of `rid`.
    #[inline]
    pub fn doc(&self, rid: u32) -> Option<(usize, DocId)> {
        let v = *self.rid_to_doc.get(rid as usize)?;
        (v != u64::MAX).then_some(((v >> 32) as usize, v as u32))
    }

    /// The filter of `rids`. `estimate` bounds the number of hits of the
    /// query, `None` when it is not known.
    pub fn filter(&self, rids: &Arc<RoaringBitmap>, estimate: Option<u64>) -> Filter {
        let drive = rids.len() * DRIVE < self.num_docs;
        if !drive && estimate.is_some_and(|e| e * HITS < rids.len()) {
            return Filter { rids: rids.clone(), bits: None, docs: None, len: rids.len() };
        }
        let mut bits: Vec<DocBits> = self.max_docs.iter().map(|m| DocBits::new(*m)).collect();
        let mut docs: Vec<Vec<DocId>> = vec![Vec::new(); self.max_docs.len()];
        let mut len = 0;
        let mut it = rids.iter();
        let mut buf = [0u32; 1024];
        loop {
            let batch = it.next_many(&mut buf);
            if batch.is_empty() {
                break;
            }
            for &rid in batch.iter() {
                if let Some((ord, doc)) = self.doc(rid) {
                    bits[ord].insert(doc);
                    if drive {
                        docs[ord].push(doc);
                    }
                    len += 1;
                }
            }
        }
        let docs = drive.then(|| {
            docs.iter_mut().for_each(|d| d.sort_unstable());
            docs
        });
        Filter { rids: rids.clone(), bits: Some(bits.into_iter().map(Arc::new).collect()), docs, len }
    }
}

/// The filter of a text clause.
pub struct Filter {
    /// The rids of the filter.
    pub rids: Arc<RoaringBitmap>,
    /// The bitset of each segment, `None` when the hits drive.
    pub bits: Option<Vec<Arc<DocBits>>>,
    /// The sorted documents of each segment, when the filter drives the
    /// search.
    pub docs: Option<Vec<Vec<DocId>>>,
    /// The number of documents.
    pub len: u64,
}

impl Filter {
    /// The filter as a query that matches its documents with a score of 0.
    /// In a `BooleanQuery` with `Must`, it leads the intersection when it is
    /// the smallest clause.
    pub fn query(&self, searcher: &Searcher) -> DocsQuery {
        let docs = self.docs.as_ref().expect("a filter that drives");
        let map = searcher
            .segment_readers()
            .iter()
            .zip(docs)
            .map(|(seg, d)| (seg.segment_id(), d.clone()))
            .collect();
        DocsQuery { docs: Arc::new(map) }
    }

    /// The check of the filter in each segment.
    pub fn sets(&self, map: &RidMap) -> Vec<SegSet> {
        match &self.bits {
            Some(b) => b.iter().map(|b| SegSet::Docs(b.clone())).collect(),
            None => map.doc_to_rid.iter().map(|r| SegSet::Rids(self.rids.clone(), r.clone())).collect(),
        }
    }
}

/// The check of a filter in one segment.
#[derive(Clone)]
pub enum SegSet {
    /// The bitset of the documents.
    Docs(Arc<DocBits>),
    /// The bitmap of the rids, and the rid of each document.
    Rids(Arc<RoaringBitmap>, Arc<Vec<u32>>),
}

impl SegSet {
    /// Tell if the filter holds `doc`.
    #[inline]
    pub fn contains(&self, doc: DocId) -> bool {
        match self {
            SegSet::Docs(b) => b.contains(doc),
            SegSet::Rids(r, map) => r.contains(map[doc as usize]),
        }
    }
}

/// A query that matches a sorted list of documents in each segment.
#[derive(Clone, Debug)]
pub struct DocsQuery {
    docs: Arc<HashMap<SegmentId, Vec<DocId>>>,
}

impl Query for DocsQuery {
    fn weight(&self, _: EnableScoring<'_>) -> tantivy::Result<Box<dyn Weight>> {
        Ok(Box::new(DocsWeight { docs: self.docs.clone() }))
    }
}

struct DocsWeight {
    docs: Arc<HashMap<SegmentId, Vec<DocId>>>,
}

impl Weight for DocsWeight {
    fn scorer(&self, reader: &SegmentReader, _boost: Score) -> tantivy::Result<Box<dyn Scorer>> {
        let docs = self.docs.get(&reader.segment_id()).cloned().unwrap_or_default();
        Ok(Box::new(ConstScorer::new(SortedDocs { docs, cursor: 0 }, 0.0)))
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

/// Count the hits of a query in a filter, with no score, and keep them as
/// bits when `keep` is set.
pub struct InFilter {
    /// The check of each segment, or `None` for no filter.
    pub sets: Option<Vec<SegSet>>,
    /// The number of documents of each segment.
    pub max_docs: Vec<u32>,
    /// Keep the hits.
    pub keep: bool,
}

/// The child of `InFilter`.
pub struct InFilterChild {
    ord: SegmentOrdinal,
    set: Option<SegSet>,
    out: Option<DocBits>,
    count: u64,
}

impl SegmentCollector for InFilterChild {
    type Fruit = (SegmentOrdinal, u64, Option<DocBits>);

    fn collect(&mut self, doc: DocId, _score: Score) {
        self.collect_block(&[doc]);
    }

    fn collect_block(&mut self, docs: &[DocId]) {
        match (&self.set, self.out.as_mut()) {
            // A bitset: no branch on the filter, which keeps one hit in two
            // at random. The kept hits meet the filter in `harvest`.
            (Some(SegSet::Docs(_)) | None, Some(o)) => docs.iter().for_each(|d| o.insert(*d)),
            (Some(SegSet::Docs(b)), None) => {
                self.count += docs.iter().map(|d| u64::from(b.contains(*d))).sum::<u64>();
            }
            (None, None) => self.count += docs.len() as u64,
            (Some(set), mut out) => {
                for &d in docs {
                    if set.contains(d) {
                        self.count += 1;
                        if let Some(o) = out.as_deref_mut() {
                            o.insert(d);
                        }
                    }
                }
            }
        }
    }

    fn harvest(mut self) -> Self::Fruit {
        if let Some(o) = self.out.as_mut() {
            if let Some(SegSet::Docs(b)) = &self.set {
                o.and_with(b);
            }
            self.count = o.len();
        }
        (self.ord, self.count, self.out)
    }
}

impl Collector for InFilter {
    type Fruit = (u64, Vec<Option<DocBits>>);
    type Child = InFilterChild;

    fn for_segment(&self, ord: SegmentOrdinal, _seg: &SegmentReader) -> tantivy::Result<InFilterChild> {
        Ok(InFilterChild {
            ord,
            set: self.sets.as_ref().map(|s| s[ord as usize].clone()),
            out: self.keep.then(|| DocBits::new(self.max_docs[ord as usize])),
            count: 0,
        })
    }

    fn requires_scoring(&self) -> bool {
        false
    }

    fn merge_fruits(
        &self,
        fruits: Vec<(SegmentOrdinal, u64, Option<DocBits>)>,
    ) -> tantivy::Result<Self::Fruit> {
        let mut sets = vec![None; self.max_docs.len()];
        let mut count = 0;
        for (ord, c, s) in fruits {
            count += c;
            sets[ord as usize] = s;
        }
        Ok((count, sets))
    }
}

/// A hit, ordered so that the greater is the better: the higher score, then
/// the lower document, as `TopDocs` orders them.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Hit {
    score: Score,
    doc: DocAddress,
}

impl Eq for Hit {}

impl PartialOrd for Hit {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Hit {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.score.total_cmp(&other.score).then(other.doc.cmp(&self.doc))
    }
}

/// The first `limit` hits by score of a query in a filter. The filter is
/// checked in the callback of the pruning of tantivy (block-max WAND), so
/// only the hits that can enter the first `limit` are checked, and the
/// threshold rises with the hits of the filter.
pub struct TopInFilter {
    /// The check of each segment.
    pub sets: Vec<SegSet>,
    /// The number of hits to keep.
    pub limit: usize,
}

/// The child of `TopInFilter`. `collect_segment` does the work.
pub struct TopChild;

impl SegmentCollector for TopChild {
    type Fruit = Vec<(Score, DocAddress)>;

    fn collect(&mut self, _doc: DocId, _score: Score) {}

    fn harvest(self) -> Self::Fruit {
        Vec::new()
    }
}

impl Collector for TopInFilter {
    type Fruit = Vec<(Score, DocAddress)>;
    type Child = TopChild;

    fn for_segment(&self, _ord: SegmentOrdinal, _seg: &SegmentReader) -> tantivy::Result<TopChild> {
        Ok(TopChild)
    }

    fn requires_scoring(&self) -> bool {
        true
    }

    fn merge_fruits(&self, fruits: Vec<Vec<(Score, DocAddress)>>) -> tantivy::Result<Self::Fruit> {
        let mut all: Vec<Hit> =
            fruits.into_iter().flatten().map(|(score, doc)| Hit { score, doc }).collect();
        all.sort_by(|a, b| b.cmp(a));
        all.truncate(self.limit);
        Ok(all.into_iter().map(|h| (h.score, h.doc)).collect())
    }

    fn collect_segment(
        &self,
        weight: &dyn Weight,
        ord: SegmentOrdinal,
        reader: &SegmentReader,
    ) -> tantivy::Result<Vec<(Score, DocAddress)>> {
        let bits = &self.sets[ord as usize];
        let alive = reader.alive_bitset();
        let limit = self.limit;
        let mut heap: BinaryHeap<Reverse<Hit>> = BinaryHeap::with_capacity(limit + 1);
        weight.for_each_pruning(Score::MIN, reader, &mut |doc, score| {
            if bits.contains(doc) && alive.is_none_or(|a| a.is_alive(doc)) {
                let hit = Hit { score, doc: DocAddress::new(ord, doc) };
                if heap.len() < limit {
                    heap.push(Reverse(hit));
                } else if heap.peek().is_some_and(|m| hit > m.0) {
                    heap.pop();
                    heap.push(Reverse(hit));
                }
            }
            if heap.len() == limit { heap.peek().map_or(Score::MIN, |m| m.0.score) } else { Score::MIN }
        })?;
        Ok(heap.into_iter().map(|Reverse(h)| (h.score, h.doc)).collect())
    }
}

/// The sort array of the mirror, for one sort index: the key of each rid,
/// and the rids in the order of the sort, descending.
pub struct SortArray {
    key: Vec<i64>,
    order: Vec<u32>,
}

impl SortArray {
    /// Build the sort array of the fast field `name` of `searcher`.
    pub fn build(searcher: &Searcher, map: &RidMap, name: &str) -> tantivy::Result<Self> {
        let mut key = vec![i64::MIN; map.rid_to_doc.len()];
        for (ord, seg) in searcher.segment_readers().iter().enumerate() {
            let col = seg.fast_fields().i64(name)?;
            for doc in 0..seg.max_doc() {
                let rid = map.doc_to_rid[ord][doc as usize];
                if rid != u32::MAX {
                    key[rid as usize] = col.first(doc).unwrap_or(0);
                }
            }
        }
        let mut order: Vec<u32> = (0..key.len() as u32).filter(|r| key[*r as usize] != i64::MIN).collect();
        order.sort_unstable_by(|a, b| (key[*b as usize], *b).cmp(&(key[*a as usize], *a)));
        Ok(Self { key, order })
    }

    /// The first `limit` rids of `sets` (one set of documents for each
    /// segment, `count` documents in all), by the key descending, then the
    /// rid descending. A large set walks the order of the sort and stops at
    /// `limit`; a small one keeps the best `limit` of its rows.
    pub fn top(&self, map: &RidMap, sets: &[DocBits], count: u64, limit: usize) -> Vec<u32> {
        let n = self.order.len() as u64;
        if count > 0 && (limit as u64) * n < count * count {
            let mut out = Vec::with_capacity(limit);
            // The walk stops after `count` rows: past that, the best rows
            // of the set cost less.
            for &rid in self.order.iter().take(count as usize) {
                if let Some((ord, doc)) = map.doc(rid)
                    && sets[ord].contains(doc)
                {
                    out.push(rid);
                    if out.len() == limit {
                        return out;
                    }
                }
            }
        }
        self.nbest(map, sets, limit)
    }

    /// The best `limit` rows of `sets`, in order.
    pub fn nbest(&self, map: &RidMap, sets: &[DocBits], limit: usize) -> Vec<u32> {
        let mut heap: BinaryHeap<Reverse<(i64, u32)>> = BinaryHeap::with_capacity(limit + 1);
        for (ord, set) in sets.iter().enumerate() {
            let rids = &map.doc_to_rid[ord];
            for doc in set.iter() {
                let rid = rids[doc as usize];
                let k = (self.key[rid as usize], rid);
                if heap.len() < limit {
                    heap.push(Reverse(k));
                } else if heap.peek().is_some_and(|m| k > m.0) {
                    heap.pop();
                    heap.push(Reverse(k));
                }
            }
        }
        let mut out: Vec<(i64, u32)> = heap.into_iter().map(|Reverse(k)| k).collect();
        out.sort_unstable_by(|a, b| b.cmp(a));
        out.into_iter().map(|k| k.1).collect()
    }

    /// The key of `rid`.
    pub fn key(&self, rid: u32) -> i64 {
        self.key[rid as usize]
    }
}

/// The count of the hits of `query` in `sets` and the first `limit` of
/// them by score. One weight serves both passes, so a glob or a fuzzy term
/// expands once.
pub fn count_and_top(
    searcher: &Searcher,
    query: &dyn Query,
    sets: Vec<SegSet>,
    max_docs: Vec<u32>,
    limit: usize,
) -> tantivy::Result<(u64, Vec<(Score, DocAddress)>)> {
    let weight = query.weight(EnableScoring::enabled_from_searcher(searcher))?;
    let count = InFilter { sets: Some(sets.clone()), max_docs, keep: false };
    let top = TopInFilter { sets, limit };
    let (mut counts, mut tops) = (Vec::new(), Vec::new());
    for (ord, seg) in searcher.segment_readers().iter().enumerate() {
        counts.push(count.collect_segment(weight.as_ref(), ord as SegmentOrdinal, seg)?);
        tops.push(top.collect_segment(weight.as_ref(), ord as SegmentOrdinal, seg)?);
    }
    Ok((count.merge_fruits(counts)?.0, top.merge_fruits(tops)?))
}
