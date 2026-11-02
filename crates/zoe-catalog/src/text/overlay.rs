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
    if counts == Counts::Exact {
        c.exclude = Some(&ex);
    }
    let main_hits = c.run(node)?;
    // The overlay scores with the same counts and frequencies.
    let mut df_main = c.df_used().clone();
    if counts == Counts::Current {
        // The frequencies of the main index as it is, without exclusion.
        let mut plain = Compat::new(main, fields.text[i], &len_name, stats);
        df_main = HashMap::new();
        for w in query_words(&plain, node)? {
            let df = plain.doc_freq(&w)?;
            df_main.insert(w, df);
        }
    }
    let mut oc = Compat::new(&o.searcher, o.fields.text[i], &len_name, main_stats).with_df(df_main);
    let o_hits = oc.run(node)?;
    let (Some(mh), Some(oh)) = (main_hits, o_hits) else {
        return Ok((0, Vec::new()));
    };
    let rids = &o.rids;
    let (n1, mut t1) = compat::rank(main, &mh, |r| keep(r) && !rids.contains(&r), limit)?;
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

/// The words of `node`, with the words of the lexicon of `c` that its globs
/// match.
fn query_words(c: &Compat<'_>, node: &Node) -> tantivy::Result<Vec<String>> {
    let mut words = std::collections::BTreeSet::new();
    collect_words(node, &mut words);
    let mut globs = Vec::new();
    collect_globs(node, &mut globs);
    for g in globs {
        words.extend(c.expand_glob(&g)?);
    }
    Ok(words.into_iter().collect())
}

fn collect_globs(node: &Node, out: &mut Vec<String>) {
    match node {
        Node::And { items, nots } => items.iter().chain(nots).for_each(|n| collect_globs(n, out)),
        Node::Or { items } => items.iter().for_each(|n| collect_globs(n, out)),
        Node::Glob { pattern } => out.push(pattern.clone()),
        _ => {}
    }
}

/// An overlay of the compatibility mode with no tantivy segment: the words
/// of each document, matched and scored directly. It has no fixed cost.
pub struct Direct {
    /// The replaced rids.
    pub rids: HashSet<u64>,
    /// Each document: its rid, and the words of each index, `None` when
    /// absent.
    pub docs: Vec<(u64, [Option<Vec<String>>; 3])>,
}

impl Direct {
    /// Split the texts of index `i` of `docs`: a query reads one index.
    pub fn build(docs: &[Doc], i: usize) -> Self {
        let rids = docs.iter().map(|d| d.rid).collect();
        let docs = docs
            .iter()
            .map(|d| {
                let mut ws: [Option<Vec<String>>; 3] = [None, None, None];
                ws[i] = d.texts[i].as_deref().map(super::analysis::plone_words);
                (d.rid, ws)
            })
            .collect();
        Self { rids, docs }
    }

    fn exclusions(&self, main: &Searcher, fields: &Fields) -> tantivy::Result<Vec<HashSet<DocId>>> {
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

/// The score of one document for one node: `None` when the node matches
/// every document, `Some(None)` when the document does not match.
fn eval_doc(
    node: &Node,
    words: &[String],
    score: &dyn Fn(&str) -> Option<i64>,
    glob: &dyn Fn(&str, &[String]) -> Vec<String>,
) -> Option<Option<i64>> {
    match node {
        Node::Atom { words: ws } => {
            if ws.is_empty() {
                return None;
            }
            let mut acc = None;
            for w in ws {
                if let Some(s) = score(w) {
                    acc = Some(acc.unwrap_or(0) + s);
                }
            }
            Some(acc)
        }
        Node::Glob { pattern } => {
            let mut acc = None;
            for w in glob(pattern, words) {
                if let Some(s) = score(&w) {
                    acc = Some(acc.unwrap_or(0) + s);
                }
            }
            Some(acc)
        }
        Node::Phrase { words: ws } => {
            let mut total = 0;
            for w in ws {
                match score(w) {
                    Some(s) => total += s,
                    None => return Some(None),
                }
            }
            let found = words.windows(ws.len().max(1)).any(|win| win == ws.as_slice());
            Some(found.then_some(total))
        }
        Node::And { items, nots } => {
            let mut acc: Option<i64> = None;
            let mut any = false;
            for n in items {
                match eval_doc(n, words, score, glob) {
                    None => {}
                    Some(None) => return Some(None),
                    Some(Some(s)) => {
                        any = true;
                        acc = Some(acc.unwrap_or(0) + s);
                    }
                }
            }
            if !any {
                return Some(None);
            }
            for n in nots {
                if let Some(Some(_)) = eval_doc(n, words, score, glob) {
                    return Some(None);
                }
            }
            Some(acc)
        }
        Node::Or { items } => {
            let mut acc = None;
            for n in items {
                if let Some(Some(s)) = eval_doc(n, words, score, glob) {
                    acc = Some(acc.unwrap_or(0) + s);
                }
            }
            Some(acc)
        }
    }
}

/// Run `node` of the compatibility mode on text index `i` of `main` with a
/// direct overlay, as `compat` does with a segment.
#[allow(clippy::too_many_arguments)]
pub fn compat_direct(
    main: &Searcher,
    fields: &Fields,
    i: usize,
    stats: Stats,
    o: &Direct,
    node: &Node,
    counts: Counts,
    keep: impl Fn(u64) -> bool + Copy,
    limit: usize,
) -> tantivy::Result<Ranked> {
    let len_name = format!("n_{}", INDEXES[i]);
    let ex = o.exclusions(main, fields)?;
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
    let present: Vec<(u64, &Vec<String>)> =
        o.docs.iter().filter_map(|(rid, ws)| ws[i].as_ref().map(|w| (*rid, w))).collect();
    let (on, ototal) = (present.len() as i64, present.iter().map(|p| p.1.len() as i64).sum::<i64>());
    // The overlay documents that hold each word.
    let mut odf: HashMap<&str, i64> = HashMap::new();
    for (_, ws) in &present {
        let distinct: HashSet<&str> = ws.iter().map(String::as_str).collect();
        for w in distinct {
            *odf.entry(w).or_default() += 1;
        }
    }
    let (main_stats, adjust) = match counts {
        Counts::Current => (stats, Adjust::default()),
        Counts::Exact => (
            Stats {
                n: (stats.n as i64 - gone_n + on) as u64,
                total: (stats.total as i64 - gone_total + ototal) as u64,
            },
            Adjust {
                n: 0,
                total: 0,
                df: odf.iter().map(|(w, n)| ((*w).to_owned(), *n)).collect(),
            },
        ),
    };
    let mut c = Compat::new(main, fields.text[i], &len_name, main_stats).with_adjust(&adjust);
    if counts == Counts::Exact {
        c.exclude = Some(&ex);
    }
    let main_hits = c.run(node)?;
    let mut c2 = Compat::new(main, fields.text[i], &len_name, main_stats).with_adjust(&adjust);
    if counts == Counts::Exact {
        c2.exclude = Some(&ex);
    }
    let n = c2.n() as f64;
    let mean = if n == 0.0 { 0.0 } else { c2.total() as f64 / n };
    let df_cache: std::cell::RefCell<HashMap<String, u64>> = Default::default();
    let c2 = std::cell::RefCell::new(c2);
    // The document frequency of a word, read once for the query.
    let df_of = |w: &str| -> u64 {
        if let Some(d) = df_cache.borrow().get(w) {
            return *d;
        }
        let d = c2.borrow_mut().doc_freq(w).unwrap_or(0);
        // A word that only the overlay holds, outside the window.
        let d = if d == 0 { odf.get(w).copied().unwrap_or(0) as u64 } else { d };
        df_cache.borrow_mut().insert(w.to_owned(), d);
        d
    };
    let mut hits = Vec::new();
    for (rid, ws) in &present {
        let len = ws.len() as f64;
        let mut tf: HashMap<&str, u32> = HashMap::new();
        for w in ws.iter() {
            *tf.entry(w.as_str()).or_default() += 1;
        }
        let score = |w: &str| -> Option<i64> {
            let f = f64::from(*tf.get(w)?);
            let df = df_of(w);
            if df == 0 {
                return None;
            }
            let idf = (1.0 + n / df as f64).ln() * 1024.0;
            let lenweight = 0.25 + 0.75 * len / mean;
            let t = f * 2.2 / (f + 1.2 * lenweight);
            Some((t * idf + 0.5) as i64)
        };
        let glob = |pattern: &str, words: &[String]| -> Vec<String> {
            let pat: Vec<char> = pattern.chars().collect();
            let mut out: Vec<String> = words
                .iter()
                .filter(|w| super::compat::glob_match(&pat, &w.chars().collect::<Vec<_>>()))
                .cloned()
                .collect();
            out.sort();
            out.dedup();
            out
        };
        if let Some(Some(s)) = eval_doc(node, ws, &score, &glob)
            && keep(*rid)
        {
            hits.push((*rid, s));
        }
    }
    let Some(mh) = main_hits else {
        return Ok((0, Vec::new()));
    };
    // Outside the window the main index still holds the old versions:
    // leave them out of its result.
    let rids = &o.rids;
    let (n1, mut t1) = compat::rank(main, &mh, |r| keep(r) && !rids.contains(&r), limit)?;
    let n2 = hits.len();
    t1.extend(hits);
    t1.sort_by(|a, b| b.1.cmp(&a.1).then(b.0.cmp(&a.0)));
    t1.truncate(limit);
    Ok((n1 + n2, t1.into_iter().map(|(r, s)| (r, s as f64)).collect()))
}
