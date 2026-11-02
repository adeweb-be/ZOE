//! The compatibility mode: the rids, the order, the counts and the scores
//! of ZCTextIndex with an `OkapiIndex`, over the postings of tantivy.
//!
//! The query is the parse tree of ZCTextIndex's `QueryParser`, with the
//! words that the lexicon gives (`Node`). tantivy gives the term
//! dictionary, the postings with their frequencies and positions, and the
//! deletes. ZOE keeps beside it the exact number of words of each document
//! (`n_<index>`), and counts the documents and their words. The score of a
//! word in a document is the integer of `OkapiIndex._search_wids`:
//! `int(tf * idf * 1024 + 0.5)`, with `idf = ln(1 + N / df)`. The scores of
//! the words add up, as `mass_weightedUnion` and `mass_weightedIntersection`
//! add them. The order is `IIBucket.byValue(0)`: the score descending, then
//! the rid descending.

use std::collections::{BTreeSet, HashMap, HashSet};

use serde::Deserialize;
use tantivy::postings::Postings;
use tantivy::schema::{Field, IndexRecordOption};
use tantivy::{DocId, DocSet, Searcher, SegmentReader, TERMINATED, Term};

/// A node of the parse tree of ZCTextIndex.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum Node {
    /// `AndNode`: the intersection of `items` minus the union of `nots`.
    And {
        /// The positive nodes.
        items: Vec<Node>,
        /// The nodes under `NOT`.
        #[serde(default)]
        nots: Vec<Node>,
    },
    /// `OrNode`: the union.
    Or {
        /// The nodes.
        items: Vec<Node>,
    },
    /// `AtomNode`: the union of the words of one term. No word matches
    /// every document: the node is then left out.
    Atom {
        /// The words of the term.
        words: Vec<String>,
    },
    /// `PhraseNode`: the words, in a row.
    Phrase {
        /// The words of the phrase.
        words: Vec<String>,
    },
    /// `GlobNode`: the words of the lexicon that match the pattern.
    Glob {
        /// The pattern, with `*` and `?`.
        pattern: String,
    },
}

impl Node {
    fn words(&self, out: &mut BTreeSet<String>) {
        match self {
            Node::And { items, nots } => {
                items.iter().chain(nots).for_each(|n| n.words(out));
            }
            Node::Or { items } => items.iter().for_each(|n| n.words(out)),
            Node::Atom { words } | Node::Phrase { words } => out.extend(words.iter().cloned()),
            Node::Glob { .. } => {}
        }
    }
}

/// The counts of one index that the Okapi score needs.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    /// The documents with a text in the index, an empty text included.
    pub n: u64,
    /// The words of those documents.
    pub total: u64,
}

impl Stats {
    /// Count the documents and the words of `len_field` in `searcher`.
    pub fn count(searcher: &Searcher, len_name: &str) -> tantivy::Result<Self> {
        let mut s = Stats::default();
        for seg in searcher.segment_readers() {
            let col = seg.fast_fields().u64(len_name)?;
            let alive = seg.alive_bitset();
            for doc in 0..seg.max_doc() {
                if alive.is_some_and(|a| !a.is_alive(doc)) {
                    continue;
                }
                let v = col.first(doc).unwrap_or(0);
                if v > 0 {
                    s.n += 1;
                    s.total += v - 1;
                }
            }
        }
        Ok(s)
    }
}

/// A change of the counts and of the document frequencies for an overlay:
/// the documents that it replaces leave, its documents come in.
#[derive(Clone, Debug, Default)]
pub struct Adjust {
    /// The change of `n`.
    pub n: i64,
    /// The change of `total`.
    pub total: i64,
    /// The change of the document frequency of each word.
    pub df: HashMap<String, i64>,
}

/// A sorted list of documents with their scores.
type Scores = Vec<(DocId, i64)>;

fn union(a: &Scores, b: &Scores) -> Scores {
    let mut out = Vec::with_capacity(a.len().max(b.len()));
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match a[i].0.cmp(&b[j].0) {
            std::cmp::Ordering::Less => {
                out.push(a[i]);
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                out.push(b[j]);
                j += 1;
            }
            std::cmp::Ordering::Equal => {
                out.push((a[i].0, a[i].1 + b[j].1));
                i += 1;
                j += 1;
            }
        }
    }
    out.extend_from_slice(&a[i..]);
    out.extend_from_slice(&b[j..]);
    out
}

fn intersection(a: &Scores, b: &Scores) -> Scores {
    let mut out = Vec::with_capacity(a.len().min(b.len()));
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match a[i].0.cmp(&b[j].0) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                out.push((a[i].0, a[i].1 + b[j].1));
                i += 1;
                j += 1;
            }
        }
    }
    out
}

fn difference(a: &Scores, b: &Scores) -> Scores {
    let mut out = Vec::with_capacity(a.len());
    let mut j = 0;
    for &(d, s) in a {
        while j < b.len() && b[j].0 < d {
            j += 1;
        }
        if j < b.len() && b[j].0 == d {
            continue;
        }
        out.push((d, s));
    }
    out
}

/// Tell if `word` matches the glob `pattern`: `*` is any run, `?` one
/// character, as `Lexicon.globToWordIds` does with `re.match` and `$`.
pub fn glob_match(pattern: &[char], word: &[char]) -> bool {
    match pattern.split_first() {
        None => word.is_empty(),
        Some(('*', rest)) => (0..=word.len()).any(|k| glob_match(rest, &word[k..])),
        Some(('?', rest)) => !word.is_empty() && glob_match(rest, &word[1..]),
        Some((c, rest)) => word.first() == Some(c) && glob_match(rest, &word[1..]),
    }
}

/// The executor of one query on one text index of a searcher.
pub struct Compat<'a> {
    searcher: &'a Searcher,
    field: Field,
    len_name: String,
    stats: Stats,
    adjust: Option<&'a Adjust>,
    /// The documents left out of every result, by segment.
    pub exclude: Option<&'a [HashSet<DocId>]>,
    df: HashMap<String, u64>,
    mean: f64,
    pairs: Option<Field>,
}

impl<'a> Compat<'a> {
    /// Make an executor on `field`, whose length field is `len_name`.
    pub fn new(searcher: &'a Searcher, field: Field, len_name: &str, stats: Stats) -> Self {
        Self {
            searcher,
            field,
            len_name: len_name.to_owned(),
            stats,
            adjust: None,
            exclude: None,
            df: HashMap::new(),
            mean: 0.0,
            pairs: None,
        }
    }

    /// Match a phrase through the field of the pairs of words.
    pub fn with_pairs(mut self, field: Field) -> Self {
        self.pairs = Some(field);
        self
    }

    /// Use these document frequencies in place of those of the searcher.
    pub fn with_df(mut self, df: HashMap<String, u64>) -> Self {
        self.df = df;
        self
    }

    /// The document frequencies that the last run used.
    pub fn df_used(&self) -> &HashMap<String, u64> {
        &self.df
    }

    /// Use the counts of `adjust` on top of the counts of the searcher.
    pub fn with_adjust(mut self, adjust: &'a Adjust) -> Self {
        self.adjust = Some(adjust);
        self
    }

    fn alive(&self, seg_ord: usize, seg: &SegmentReader, doc: DocId) -> bool {
        if seg.alive_bitset().is_some_and(|a| !a.is_alive(doc)) {
            return false;
        }
        !self.exclude.is_some_and(|ex| ex[seg_ord].contains(&doc))
    }

    /// The document frequency of `word` over the searcher, deletes and
    /// exclusions left out, plus the change of the overlay.
    pub fn doc_freq(&mut self, word: &str) -> tantivy::Result<u64> {
        if let Some(df) = self.df.get(word) {
            return Ok(*df);
        }
        let term = Term::from_field_text(self.field, word);
        let mut df: i64 = 0;
        for (ord, seg) in self.searcher.segment_readers().iter().enumerate() {
            let inv = seg.inverted_index(self.field)?;
            if seg.num_deleted_docs() == 0 {
                // No delete: the frequency of the term dictionary, minus
                // the excluded documents that hold the word.
                if let Some(info) = inv.get_term_info(&term)? {
                    df += i64::from(info.doc_freq);
                    if let Some(ex) = self.exclude.map(|ex| &ex[ord]).filter(|e| !e.is_empty()) {
                        let mut docs: Vec<DocId> = ex.iter().copied().collect();
                        docs.sort_unstable();
                        if let Some(mut p) = inv.read_postings(&term, IndexRecordOption::Basic)? {
                            for d in docs {
                                if p.doc() < d {
                                    p.seek(d);
                                }
                                if p.doc() == d {
                                    df -= 1;
                                }
                            }
                        }
                    }
                }
                continue;
            }
            if let Some(mut p) = inv.read_postings(&term, IndexRecordOption::Basic)? {
                let mut d = p.doc();
                while d != TERMINATED {
                    if self.alive(ord, seg, d) {
                        df += 1;
                    }
                    d = p.advance();
                }
            }
        }
        if let Some(a) = self.adjust {
            df += a.df.get(word).copied().unwrap_or(0);
        }
        let df = df.max(0) as u64;
        self.df.insert(word.to_owned(), df);
        Ok(df)
    }

    /// The scores of `word` in segment `ord`.
    fn term_scores(&mut self, ord: usize, word: &str) -> tantivy::Result<Scores> {
        let df = self.doc_freq(word)?;
        if df == 0 {
            return Ok(Vec::new());
        }
        let n = self.n() as f64;
        let idf = (1.0 + n / df as f64).ln() * 1024.0;
        let mean = self.mean;
        let seg = &self.searcher.segment_readers()[ord];
        let lens = seg.fast_fields().u64(&self.len_name)?;
        let term = Term::from_field_text(self.field, word);
        let mut out = Vec::new();
        let Some(mut p) = seg.inverted_index(self.field)?.read_postings(&term, IndexRecordOption::WithFreqs)?
        else {
            return Ok(out);
        };
        let (b, k1) = (0.75f64, 1.2f64);
        let (b_from1, k1_plus1) = (1.0 - b, k1 + 1.0);
        let mut d = p.doc();
        while d != TERMINATED {
            if self.alive(ord, seg, d) {
                let f = f64::from(p.term_freq());
                let len = lens.first(d).unwrap_or(1).saturating_sub(1) as f64;
                let lenweight = b_from1 + b * len / mean;
                let tf = f * k1_plus1 / (f + k1 * lenweight);
                out.push((d, (tf * idf + 0.5) as i64));
            }
            d = p.advance();
        }
        Ok(out)
    }

    /// The scores of `word` in segment `ord` for the documents of `docs`,
    /// which hold it, in their order.
    fn term_scores_for(&mut self, ord: usize, word: &str, docs: &[DocId]) -> tantivy::Result<Scores> {
        let df = self.doc_freq(word)?;
        let n = self.n() as f64;
        let idf = (1.0 + n / df.max(1) as f64).ln() * 1024.0;
        let mean = self.mean;
        let seg = &self.searcher.segment_readers()[ord];
        let lens = seg.fast_fields().u64(&self.len_name)?;
        let term = Term::from_field_text(self.field, word);
        let mut out = Vec::with_capacity(docs.len());
        let Some(mut p) = seg.inverted_index(self.field)?.read_postings(&term, IndexRecordOption::WithFreqs)?
        else {
            return Ok(docs.iter().map(|d| (*d, 0)).collect());
        };
        let (b, k1) = (0.75f64, 1.2f64);
        let (b_from1, k1_plus1) = (1.0 - b, k1 + 1.0);
        for &d in docs {
            if p.doc() < d {
                p.seek(d);
            }
            let mut score = 0;
            if p.doc() == d {
                let f = f64::from(p.term_freq());
                let len = lens.first(d).unwrap_or(1).saturating_sub(1) as f64;
                let lenweight = b_from1 + b * len / mean;
                let tf = f * k1_plus1 / (f + k1 * lenweight);
                score = (tf * idf + 0.5) as i64;
            }
            out.push((d, score));
        }
        Ok(out)
    }

    /// The document count of the run.
    pub fn n(&self) -> u64 {
        let adj = self.adjust.map_or(0, |a| a.n);
        (self.stats.n as i64 + adj).max(0) as u64
    }

    /// The word count of the run.
    pub fn total(&self) -> u64 {
        let adj = self.adjust.map_or(0, |a| a.total);
        (self.stats.total as i64 + adj).max(0) as u64
    }

    /// The words of the lexicon that match `pattern`, over all segments.
    pub fn expand_glob(&self, pattern: &str) -> tantivy::Result<Vec<String>> {
        let prefix: String = pattern.chars().take_while(|c| *c != '*' && *c != '?').collect();
        if prefix.len() == pattern.len() {
            return Ok(vec![prefix]);
        }
        let pat: Vec<char> = pattern.chars().collect();
        let mut words = BTreeSet::new();
        for seg in self.searcher.segment_readers() {
            let inv = seg.inverted_index(self.field)?;
            let mut stream = inv.terms().range().ge(prefix.as_bytes()).into_stream()?;
            while stream.advance() {
                let key = stream.key();
                if !key.starts_with(prefix.as_bytes()) {
                    break;
                }
                if let Ok(w) = std::str::from_utf8(key) {
                    let chars: Vec<char> = w.chars().collect();
                    if glob_match(&pat, &chars) {
                        words.insert(w.to_owned());
                    }
                }
            }
        }
        Ok(words.into_iter().collect())
    }

    fn phrase_filter(&self, ord: usize, words: &[String], cand: Scores) -> tantivy::Result<Scores> {
        let seg = &self.searcher.segment_readers()[ord];
        let inv = seg.inverted_index(self.field)?;
        let mut postings = Vec::new();
        for w in words {
            let term = Term::from_field_text(self.field, w);
            match inv.read_postings(&term, IndexRecordOption::WithFreqsAndPositions)? {
                Some(p) => postings.push(p),
                None => return Ok(Vec::new()),
            }
        }
        let mut out = Vec::new();
        let mut pos: Vec<Vec<u32>> = vec![Vec::new(); words.len()];
        for (doc, score) in cand {
            let mut ok = true;
            for (i, p) in postings.iter_mut().enumerate() {
                if p.doc() < doc {
                    p.seek(doc);
                }
                if p.doc() != doc {
                    ok = false;
                    break;
                }
                p.positions(&mut pos[i]);
            }
            if !ok {
                continue;
            }
            let found = pos[0].iter().any(|&start| {
                (1..words.len()).all(|i| pos[i].binary_search(&(start + i as u32)).is_ok())
            });
            if found {
                out.push((doc, score));
            }
        }
        Ok(out)
    }

    fn eval(&mut self, ord: usize, node: &Node) -> tantivy::Result<Option<Scores>> {
        Ok(match node {
            Node::Atom { words } => {
                if words.is_empty() {
                    return Ok(None);
                }
                let mut acc: Scores = Vec::new();
                for w in words {
                    let s = self.term_scores(ord, w)?;
                    acc = union(&acc, &s);
                }
                Some(acc)
            }
            Node::Glob { pattern } => {
                let words = self.expand_glob(pattern)?;
                let mut acc: Scores = Vec::new();
                for w in &words {
                    let s = self.term_scores(ord, w)?;
                    acc = union(&acc, &s);
                }
                Some(acc)
            }
            Node::Phrase { words } => {
                for w in words {
                    if self.doc_freq(w)? == 0 {
                        return Ok(Some(Vec::new()));
                    }
                }
                if let (Some(pf), true) = (self.pairs, words.len() > 1) {
                    // The documents that hold each pair in a row: for two
                    // words, the documents of the phrase.
                    let seg = &self.searcher.segment_readers()[ord];
                    let inv = seg.inverted_index(pf)?;
                    let mut cand: Option<Vec<DocId>> = None;
                    for w in words.windows(2) {
                        let term = Term::from_field_text(pf, &super::index::pair(&w[0], &w[1]));
                        let mut docs = Vec::new();
                        if let Some(mut p) = inv.read_postings(&term, IndexRecordOption::Basic)? {
                            let mut d = p.doc();
                            while d != TERMINATED {
                                if self.alive(ord, seg, d) {
                                    docs.push(d);
                                }
                                d = p.advance();
                            }
                        }
                        cand = Some(match cand {
                            None => docs,
                            Some(c) => {
                                let set: HashSet<DocId> = docs.into_iter().collect();
                                c.into_iter().filter(|d| set.contains(d)).collect()
                            }
                        });
                    }
                    let cand = cand.unwrap_or_default();
                    // The score of each word, read for the candidates only.
                    let mut acc: Scores = cand.iter().map(|d| (*d, 0)).collect();
                    for w in words {
                        let s = self.term_scores_for(ord, w, &cand)?;
                        for (a, b) in acc.iter_mut().zip(s) {
                            a.1 += b.1;
                        }
                    }
                    if words.len() > 2 {
                        Some(self.phrase_filter(ord, words, acc)?)
                    } else {
                        Some(acc)
                    }
                } else {
                    let mut lists = Vec::new();
                    for w in words {
                        lists.push(self.term_scores(ord, w)?);
                    }
                    lists.sort_by_key(Vec::len);
                    let mut acc = lists[0].clone();
                    for l in &lists[1..] {
                        acc = intersection(&acc, l);
                    }
                    Some(self.phrase_filter(ord, words, acc)?)
                }
            }
            Node::And { items, nots } => {
                let mut lists = Vec::new();
                for n in items {
                    if let Some(r) = self.eval(ord, n)? {
                        lists.push(r);
                    }
                }
                let mut set = if lists.is_empty() {
                    Vec::new()
                } else {
                    lists.sort_by_key(Vec::len);
                    let mut acc = lists[0].clone();
                    for l in &lists[1..] {
                        acc = intersection(&acc, l);
                    }
                    acc
                };
                let mut notset: Option<Scores> = None;
                for n in nots {
                    if let Some(r) = self.eval(ord, n)? {
                        notset = Some(match notset {
                            Some(a) => union(&a, &r),
                            None => r,
                        });
                    }
                }
                if let Some(ns) = notset {
                    set = difference(&set, &ns);
                }
                Some(set)
            }
            Node::Or { items } => {
                let mut acc: Option<Scores> = None;
                for n in items {
                    if let Some(r) = self.eval(ord, n)? {
                        acc = Some(match acc {
                            Some(a) => union(&a, &r),
                            None => r,
                        });
                    }
                }
                Some(acc.unwrap_or_default())
            }
        })
    }

    /// Run `node`. Return the hits of each segment, sorted by document,
    /// with their scores, or `None` when the tree matches every document.
    pub fn run(&mut self, node: &Node) -> tantivy::Result<Option<Vec<Scores>>> {
        let n = self.n();
        self.mean = if n == 0 { 0.0 } else { self.total() as f64 / n as f64 };
        let mut words = BTreeSet::new();
        node.words(&mut words);
        let mut out = Vec::new();
        let mut all = false;
        for ord in 0..self.searcher.segment_readers().len() {
            match self.eval(ord, node)? {
                Some(s) => out.push(s),
                None => {
                    all = true;
                    out.push(Vec::new());
                }
            }
        }
        Ok(if all { None } else { Some(out) })
    }
}

/// Map the hits of each segment to (rid, score), in the order of
/// `IIBucket.byValue(0)`: the score descending, then the rid descending.
/// Keep the first `limit` in that order, and return the count of all.
pub fn rank(
    searcher: &Searcher,
    hits: &[Scores],
    keep: impl Fn(u64) -> bool,
    limit: usize,
) -> tantivy::Result<(usize, Vec<(u64, i64)>)> {
    let mut all = Vec::new();
    for (ord, seg_hits) in hits.iter().enumerate() {
        let rids = searcher.segment_readers()[ord].fast_fields().u64("rid")?;
        for &(doc, score) in seg_hits {
            let rid = rids.first(doc).unwrap_or(0);
            if keep(rid) {
                all.push((rid, score));
            }
        }
    }
    let count = all.len();
    let cmp = |a: &(u64, i64), b: &(u64, i64)| b.1.cmp(&a.1).then(b.0.cmp(&a.0));
    if limit < all.len() {
        all.select_nth_unstable_by(limit, cmp);
        all.truncate(limit);
    }
    all.sort_by(cmp);
    Ok((count, all))
}
