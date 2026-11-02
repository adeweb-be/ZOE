//! The overlay of a query: some documents of the index are replaced, for
//! this query only, by other versions of their texts.
//!
//! Both mechanisms of the consistency of the text index use it. Outside the
//! window, the documents that changed after the snapshot get their text at
//! the snapshot, read from the delta history. In the transaction that
//! indexes, the documents that the transaction staged get their staged
//! text. A replaced document leaves the result of the main index, and the
//! overlay, a small index in memory, answers for its new version. A
//! document with no text in the overlay is absent at that version.

use std::collections::{HashMap, HashSet};

use tantivy::collector::{Count, FilterCollector, TopDocs};
use tantivy::query::Query;
use tantivy::schema::IndexRecordOption;
use tantivy::{DocAddress, DocId, DocSet, Index, Searcher, TERMINATED, Term};

use super::compat::{self, Adjust, Compat, Node, Stats};
use super::index::{self, Doc, Fields, INDEXES, Mode};

/// An overlay: the replaced rids, and an index in memory of the documents
/// that have a text at the new version.
pub struct Overlay {
    /// The rids that the overlay replaces.
    pub rids: HashSet<u64>,
    /// The index of the new versions.
    pub index: Index,
    /// Its fields.
    pub fields: Fields,
    /// Its searcher.
    pub searcher: Searcher,
}

impl Overlay {
    /// Build the overlay of `docs`: each replaces the document of its rid.
    /// A doc with no text replaces it with nothing.
    pub fn build(mode: Mode, docs: &[Doc]) -> tantivy::Result<Self> {
        let (schema, fields) = index::schema(mode);
        let tokenizers = tantivy::tokenizer::TokenizerManager::default();
        index::register_on(&tokenizers);
        // One segment, written in this thread: no thread starts.
        let mut writer = Index::builder()
            .schema(schema)
            .tokenizers(tokenizers)
            .single_segment_index_writer(tantivy::directory::RamDirectory::create(), 15_000_000)?;
        for d in docs {
            if d.texts.iter().any(Option::is_some) {
                writer.add_document(index::document(mode, &fields, d))?;
            }
        }
        let index = writer.finalize()?;
        index::register(&index);
        let searcher = index::reader(&index)?.searcher();
        let rids = docs.iter().map(|d| d.rid).collect();
        Ok(Self { rids, index, fields, searcher })
    }

    /// The documents of the replaced rids in `main`, by segment.
    pub fn exclusions(&self, main: &Searcher, fields: &Fields) -> tantivy::Result<Vec<HashSet<DocId>>> {
        let mut out = Vec::new();
        for seg in main.segment_readers() {
            let inv = seg.inverted_index(fields.rid)?;
            let mut set = HashSet::new();
            for &rid in &self.rids {
                let term = Term::from_field_u64(fields.rid, rid);
                if let Some(mut p) = inv.read_postings(&term, IndexRecordOption::Basic)? {
                    let mut d = p.doc();
                    while d != TERMINATED {
                        if seg.alive_bitset().is_none_or(|a| a.is_alive(d)) {
                            set.insert(d);
                        }
                        d = p.advance();
                    }
                }
            }
            out.push(set);
        }
        Ok(out)
    }
}

/// The result of a query: the count, and the first hits as (rid, score).
pub type Ranked = (usize, Vec<(u64, f64)>);

/// Run `query` of the enhanced mode on `main` with `overlay`: the main
/// index without the replaced rids, then the overlay with the statistics of
/// the main index. `keep` is the filter of the mirror.
pub fn enhanced(
    main: &Searcher,
    overlay: Option<&Overlay>,
    query: &dyn Query,
    keep: impl Fn(u64) -> bool + Clone + Send + Sync + 'static,
    limit: usize,
) -> tantivy::Result<Ranked> {
    let replaced: HashSet<u64> = overlay.map(|o| o.rids.clone()).unwrap_or_default();
    let k1 = keep.clone();
    let pred = move |rid: u64| k1(rid) && !replaced.contains(&rid);
    let collector = FilterCollector::new(
        "rid".to_string(),
        pred,
        (Count, TopDocs::with_limit(limit).order_by_score()),
    );
    let (mut count, top) = main.search(query, &collector)?;
    let mut hits = to_rids(main, &top)?;
    if let Some(o) = overlay {
        let collector = FilterCollector::new(
            "rid".to_string(),
            keep,
            (Count, TopDocs::with_limit(limit).order_by_score()),
        );
        let (c, top) = o.searcher.search_with_statistics_provider(query, &collector, main)?;
        count += c;
        hits.extend(to_rids(&o.searcher, &top)?);
        hits.sort_by(|a, b| b.1.total_cmp(&a.1).then(b.0.cmp(&a.0)));
        hits.truncate(limit);
    }
    Ok((count, hits))
}

fn to_rids(searcher: &Searcher, top: &[(f32, DocAddress)]) -> tantivy::Result<Vec<(u64, f64)>> {
    let mut out = Vec::with_capacity(top.len());
    for (score, addr) in top {
        let rids = searcher.segment_reader(addr.segment_ord).fast_fields().u64("rid")?;
        out.push((rids.first(addr.doc_id).unwrap_or(0), f64::from(*score)));
    }
    Ok(out)
}

/// How the compatibility mode counts with an overlay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Counts {
    /// The counts of the main index as it is: outside the window.
    Current,
    /// The counts of the main index with the overlay in place of the
    /// replaced documents: the scores of ZCTextIndex in the transaction.
    Exact,
}

/// Run `node` of the compatibility mode on text index `i` of `main` with
/// `overlay`. `stats` are the counts of the main index, which the store
/// keeps.
#[allow(clippy::too_many_arguments)]
pub fn compat(
    main: &Searcher,
    fields: &Fields,
    i: usize,
    stats: Stats,
    overlay: Option<&Overlay>,
    node: &Node,
    counts: Counts,
    keep: impl Fn(u64) -> bool + Copy,
    limit: usize,
) -> tantivy::Result<Ranked> {
    let len_name = format!("n_{}", INDEXES[i]);
    let Some(o) = overlay else {
        let mut c = Compat::new(main, fields.text[i], &len_name, stats);
        let Some(hits) = c.run(node)? else {
            return Ok((0, Vec::new()));
        };
        let (n, top) = compat::rank(main, &hits, keep, limit)?;
        return Ok((n, top.into_iter().map(|(r, s)| (r, s as f64)).collect()));
    };
    let ex = o.exclusions(main, fields)?;
    let ostats = Stats::count(&o.searcher, &len_name)?;
    // The documents that leave the main index, and their words.
    let (mut gone_n, mut gone_total) = (0i64, 0i64);
    for (ord, set) in ex.iter().enumerate() {
        let lens = main.segment_readers()[ord].fast_fields().u64(&len_name)?;
        for &d in set {
            let v = lens.first(d).unwrap_or(0);
            if v > 0 {
                gone_n += 1;
                gone_total += v as i64 - 1;
            }
        }
    }
    let (main_stats, adjust) = match counts {
        Counts::Current => (stats, Adjust::default()),
        Counts::Exact => {
            let s = Stats {
                n: (stats.n as i64 - gone_n + ostats.n as i64) as u64,
                total: (stats.total as i64 - gone_total + ostats.total as i64) as u64,
            };
            // The overlay documents that hold each word of the query.
            let mut words = std::collections::BTreeSet::new();
            collect_words(node, &mut words);
            let mut df = HashMap::new();
            for w in &words {
                let mut tmp = Compat::new(&o.searcher, o.fields.text[i], &len_name, ostats);
                let _ = tmp.run(&Node::Atom { words: vec![w.clone()] })?;
                df.insert(w.clone(), tmp.df_used().get(w).copied().unwrap_or(0) as i64);
            }
            (s, Adjust { n: 0, total: 0, df })
        }
    };
    let mut c = Compat::new(main, fields.text[i], &len_name, main_stats).with_adjust(&adjust);
    c.exclude = Some(&ex);
    let main_hits = c.run(node)?;
    // The overlay scores with the same counts and frequencies.
    let mut df_main = c.df_used().clone();
    if counts == Counts::Current {
        // The frequencies of the main index as it is, without exclusion.
        let mut plain = Compat::new(main, fields.text[i], &len_name, stats);
        let _ = plain.run(node)?;
        df_main = plain.df_used().clone();
    }
    let mut oc = Compat::new(&o.searcher, o.fields.text[i], &len_name, main_stats).with_df(df_main);
    let o_hits = oc.run(node)?;
    let (Some(mh), Some(oh)) = (main_hits, o_hits) else {
        return Ok((0, Vec::new()));
    };
    let (n1, mut t1) = compat::rank(main, &mh, keep, limit)?;
    let (n2, t2) = compat::rank(&o.searcher, &oh, keep, limit)?;
    t1.extend(t2);
    t1.sort_by(|a, b| b.1.cmp(&a.1).then(b.0.cmp(&a.0)));
    t1.truncate(limit);
    Ok((n1 + n2, t1.into_iter().map(|(r, s)| (r, s as f64)).collect()))
}

fn collect_words(node: &Node, out: &mut std::collections::BTreeSet<String>) {
    match node {
        Node::And { items, nots } => items.iter().chain(nots).for_each(|n| collect_words(n, out)),
        Node::Or { items } => items.iter().for_each(|n| collect_words(n, out)),
        Node::Atom { words } | Node::Phrase { words } => out.extend(words.iter().cloned()),
        Node::Glob { .. } => {}
    }
}
