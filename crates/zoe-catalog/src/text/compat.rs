//! The compatibility mode (ADR 0060, sections 11 and 13): the rids, the
//! order, the counts and the integer scores of ZCTextIndex with an
//! `OkapiIndex` and the lexicon `plone_lexicon`.
//!
//! tantivy gives the term dictionary, the postings with their frequencies
//! and positions, and the deletes. The maps give the number of words of
//! each document. The score of a word in a document is the integer of
//! `OkapiIndex._search_wids`: `int(tf * idf + 0.5)`, with
//! `idf = log(1 + N / df) * 1024`,
//! `tf = f * (k1 + 1) / (f + k1 * (1 - b + b * len / mean))`, `k1` 1.2 and
//! `b` 0.75. The nodes follow `ParseTree`:
//!
//! - An atom adds up the scores of its words, as `mass_weightedUnion`
//!   does. An atom with no word matches every document: an `and` and an
//!   `or` leave it out, and alone it gives no hit, as `Catalog.search`
//!   gives none for it.
//! - A phrase needs each word in the index, and the words in a row. Its
//!   score adds up the scores of its words.
//! - A glob reads the words of the index that `Lexicon.globToWordIds`
//!   gives for the pattern after the lexicon, and adds up their scores.
//! - An `and` adds up the scores of its items in each document of all of
//!   them, less the documents of its nots. An `or` adds up the scores of
//!   its items in each document of any of them.
//!
//! The statistics: N, the total of the words and the document frequency of
//! each word are those of the index less the replaced documents, plus the
//! documents of the text overlay. So a query scores as ZCTextIndex scores
//! on the documents of the snapshot with the staged texts. The order is
//! that of `IIBucket.byValue(0)`: the score descending, then the rid
//! descending.

use std::collections::{BTreeSet, HashMap};

use tantivy::postings::Postings;
use tantivy::schema::{Field, IndexRecordOption};
use tantivy::{DocId, DocSet, Searcher, SegmentReader, TERMINATED, Term};

use super::analysis::pair;
use super::clause::Node;
use super::index::{Lengths, Maps};
use super::lexicon::Glob;
use super::search::{self, DocBits, Filter, Found, HitDocs, Strategy, Top, Want};

/// `k1` of `OkapiIndex`.
const K1: f64 = 1.2;
/// `b` of `OkapiIndex`.
const B: f64 = 0.75;

/// The integer score of a word `f` times in a document of `len` words.
/// `idf` is the inverse document frequency times 1,024.
#[inline]
fn okapi(f: u32, len: u32, idf: f64, mean: f64) -> i64 {
    let f = f64::from(f);
    let lenweight = (1.0 - B) + B * f64::from(len) / mean;
    let tf = f * (K1 + 1.0) / (f + K1 * lenweight);
    (tf * idf + 0.5) as i64
}

/// A document of the text overlay with a text in the index: its words.
#[derive(Debug)]
pub struct Doc {
    /// The rid.
    pub rid: i32,
    words: Vec<String>,
    tf: HashMap<String, u32>,
}

impl Doc {
    /// The document of `rid` with the words `words` of the lexicon.
    pub fn new(rid: i32, words: Vec<String>) -> Doc {
        let mut tf = HashMap::new();
        for w in &words {
            *tf.entry(w.clone()).or_insert(0) += 1;
        }
        Doc { rid, words, tf }
    }

    fn len(&self) -> u32 {
        u32::try_from(self.words.len()).unwrap_or(u32::MAX)
    }
}

/// The tantivy index of a query in the compatibility mode.
pub struct Indexed<'a> {
    /// The searcher.
    pub searcher: &'a Searcher,
    /// The maps of its documents.
    pub maps: &'a Maps,
    /// The field of the words of the index.
    pub text: Field,
    /// The field of the pairs of words.
    pub pairs: Field,
    /// The lengths of the documents of the index.
    pub lengths: &'a Lengths,
}

/// A sorted list of documents of one segment, with their scores.
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

/// The check of one segment: the documents that a hit may be.
enum Check {
    /// The live documents that are not replaced.
    Live,
    /// The documents of the bitset: the filter, without the replaced ones.
    Bits(DocBits),
    /// The live documents that are not replaced, whose rid is in the
    /// filter.
    Rids,
}

/// The result of a node for a document of the overlay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Eval {
    /// The node matches every document.
    Every,
    /// The document does not match.
    No,
    /// The document matches, with this score.
    Yes(i64),
}

/// One query in the compatibility mode.
pub struct Run<'a> {
    index: Option<Indexed<'a>>,
    /// The replaced documents of each segment that have a text in the
    /// index, sorted.
    replaced: Vec<Vec<DocId>>,
    /// The replaced documents of each segment.
    excluded: Vec<Option<DocBits>>,
    overlay: &'a [Doc],
    filter: Option<Filter<'a>>,
    checks: Vec<Check>,
    /// The documents of the filter of each segment, when they drive.
    drive: Vec<Option<Vec<DocId>>>,
    n: u64,
    mean: f64,
    df: HashMap<String, u64>,
    globs: HashMap<String, Vec<String>>,
    /// False when the search wants a set: the hits get no score.
    scored: bool,
}

impl<'a> Run<'a> {
    /// A query on `index`, or on the overlay alone for `None`, with the
    /// replaced rids `replaced`, the documents of the text overlay that
    /// have a text in the index, and the filter `filter`.
    pub fn new(
        index: Option<Indexed<'a>>,
        replaced: &BTreeSet<i32>,
        overlay: &'a [Doc],
        filter: Option<Filter<'a>>,
    ) -> Run<'a> {
        let segments = index
            .as_ref()
            .map_or(0, |i| i.searcher.segment_readers().len());
        let mut run = Run {
            index,
            replaced: vec![Vec::new(); segments],
            excluded: vec![None; segments],
            overlay,
            filter,
            checks: Vec::new(),
            drive: vec![None; segments],
            n: 0,
            mean: 0.0,
            df: HashMap::new(),
            globs: HashMap::new(),
            scored: true,
        };
        let (mut n, mut total) = (0i64, 0i64);
        if let Some(ix) = &run.index {
            n = ix.lengths.docs as i64;
            total = ix.lengths.total as i64;
            for rid in replaced {
                let Some((ord, doc)) = ix.maps.doc(*rid) else {
                    continue;
                };
                let ord = ord as usize;
                run.excluded[ord]
                    .get_or_insert_with(|| DocBits::new(ix.maps.max_docs[ord]))
                    .insert(doc);
                let len = ix.lengths.of(ord, doc);
                if len > 0 {
                    n -= 1;
                    total -= i64::from(len - 1);
                    run.replaced[ord].push(doc);
                }
            }
            for docs in &mut run.replaced {
                docs.sort_unstable();
            }
        }
        for doc in overlay {
            n += 1;
            total += i64::from(doc.len());
        }
        run.n = n.max(0) as u64;
        run.mean = if run.n == 0 {
            0.0
        } else {
            total.max(0) as f64 / run.n as f64
        };
        run
    }

    /// Choose how the search meets its filter, from the bound of the hits
    /// of `node`, and build the checks of each segment.
    fn prepare(
        &mut self,
        node: &Node,
        want: Want,
        forced: Option<Strategy>,
    ) -> tantivy::Result<Strategy> {
        let Some(ix) = &self.index else {
            return Ok(Strategy::None);
        };
        let num_docs = ix.maps.num_docs();
        let chosen = match forced {
            Some(s) => s,
            None => {
                let bound = self.bound(node)?;
                search::strategy(num_docs, self.filter.map(|f| f.len()), bound)
            }
        };
        let Some(ix) = &self.index else {
            return Ok(Strategy::None);
        };
        let chosen = match (self.filter, chosen) {
            (None, _) => Strategy::None,
            (Some(_), Strategy::None) => Strategy::Bitset,
            (_, s) => s,
        };
        let segments = ix.searcher.segment_readers().len();
        self.checks.clear();
        // The keys of a set come from a merge of the filter with the hits:
        // with a bitset, that merge applies the filter, and the search needs
        // none.
        let unfiltered = want == Want::Keys && chosen == Strategy::Bitset;
        match (chosen, self.filter) {
            (Strategy::None, _) | (_, None) => {
                self.checks.extend((0..segments).map(|_| Check::Live));
            }
            _ if unfiltered => self.checks.extend((0..segments).map(|_| Check::Live)),
            (Strategy::HitsDrive, Some(_)) => {
                self.checks.extend((0..segments).map(|_| Check::Rids));
            }
            (Strategy::FilterDrives | Strategy::Bitset, Some(filter)) => {
                let mut bits: Vec<DocBits> =
                    ix.maps.max_docs.iter().map(|m| DocBits::new(*m)).collect();
                let mut drive: Vec<Vec<DocId>> = vec![Vec::new(); segments];
                let excluded = &self.excluded;
                search::each_doc(ix.maps, filter, |_, _, ord, doc| {
                    if excluded[ord as usize]
                        .as_ref()
                        .is_none_or(|e| !e.contains(doc))
                    {
                        bits[ord as usize].insert(doc);
                        if chosen == Strategy::FilterDrives {
                            drive[ord as usize].push(doc);
                        }
                    }
                });
                if chosen == Strategy::FilterDrives {
                    for (ord, mut d) in drive.into_iter().enumerate() {
                        d.sort_unstable();
                        self.drive[ord] = Some(d);
                    }
                }
                self.checks.extend(bits.into_iter().map(Check::Bits));
            }
        }
        Ok(chosen)
    }

    /// Tell if the document `doc` of the segment `ord` may be a hit.
    #[inline]
    fn keep(&self, ord: usize, seg: &SegmentReader, doc: DocId) -> bool {
        match &self.checks[ord] {
            Check::Bits(b) => b.contains(doc),
            Check::Live => {
                seg.alive_bitset().is_none_or(|a| a.is_alive(doc))
                    && self.excluded[ord].as_ref().is_none_or(|e| !e.contains(doc))
            }
            Check::Rids => {
                let Some(ix) = &self.index else {
                    return false;
                };
                let rid = ix.maps.doc_to_rid[ord][doc as usize];
                seg.alive_bitset().is_none_or(|a| a.is_alive(doc))
                    && self.excluded[ord].as_ref().is_none_or(|e| !e.contains(doc))
                    && self.filter.is_some_and(|f| f.contains(rid))
            }
        }
    }

    /// The document frequency of `word`: the live documents of the index
    /// that hold it, less the replaced ones, plus the documents of the
    /// overlay that hold it.
    fn doc_freq(&mut self, word: &str) -> tantivy::Result<u64> {
        if let Some(df) = self.df.get(word) {
            return Ok(*df);
        }
        let mut df: i64 = self
            .overlay
            .iter()
            .filter(|d| d.tf.contains_key(word))
            .count() as i64;
        if let Some(ix) = &self.index {
            let term = Term::from_field_text(ix.text, word);
            for (ord, seg) in ix.searcher.segment_readers().iter().enumerate() {
                let inv = seg.inverted_index(ix.text)?;
                let Some(info) = inv.get_term_info(&term)? else {
                    continue;
                };
                let replaced = &self.replaced[ord];
                if seg.num_deleted_docs() == 0 && replaced.is_empty() {
                    df += i64::from(info.doc_freq);
                    continue;
                }
                let mut postings =
                    inv.read_postings_from_terminfo(&info, IndexRecordOption::Basic)?;
                if seg.num_deleted_docs() == 0 {
                    df += i64::from(info.doc_freq);
                    for &d in replaced {
                        if postings.doc() < d {
                            postings.seek(d);
                        }
                        if postings.doc() == d {
                            df -= 1;
                        }
                    }
                    continue;
                }
                let mut d = postings.doc();
                let mut r = 0;
                while d != TERMINATED {
                    while r < replaced.len() && replaced[r] < d {
                        r += 1;
                    }
                    let is_replaced = r < replaced.len() && replaced[r] == d;
                    if !is_replaced && seg.alive_bitset().is_none_or(|a| a.is_alive(d)) {
                        df += 1;
                    }
                    d = postings.advance();
                }
            }
        }
        let df = df.max(0) as u64;
        self.df.insert(word.to_owned(), df);
        Ok(df)
    }

    /// The inverse document frequency of `word` times 1,024, or `None`
    /// when no document holds it.
    fn idf(&mut self, word: &str) -> tantivy::Result<Option<f64>> {
        let df = self.doc_freq(word)?;
        if df == 0 {
            return Ok(None);
        }
        Ok(Some((1.0 + self.n as f64 / df as f64).ln() * 1024.0))
    }

    /// The words of the glob `pattern`: the words of the index and of the
    /// overlay that it matches and that a document holds.
    fn expand(&mut self, pattern: &str) -> tantivy::Result<Vec<String>> {
        if let Some(found) = self.globs.get(pattern) {
            return Ok(found.clone());
        }
        let glob = Glob::of(pattern);
        let mut candidates: BTreeSet<String> = BTreeSet::new();
        match &glob {
            Glob::Refused => {}
            Glob::Word(w) => {
                candidates.insert(w.clone());
            }
            Glob::Match { prefix, .. } => {
                if let Some(ix) = &self.index {
                    for seg in ix.searcher.segment_readers() {
                        let inv = seg.inverted_index(ix.text)?;
                        let mut stream = inv.terms().range().ge(prefix.as_bytes()).into_stream()?;
                        while stream.advance() {
                            let key = stream.key();
                            if !key.starts_with(prefix.as_bytes()) {
                                break;
                            }
                            if let Ok(w) = std::str::from_utf8(key)
                                && glob.matches(w)
                            {
                                candidates.insert(w.to_owned());
                            }
                        }
                    }
                }
                for doc in self.overlay {
                    for w in doc.tf.keys() {
                        if glob.matches(w) {
                            candidates.insert(w.clone());
                        }
                    }
                }
            }
        }
        let mut words = Vec::new();
        for w in candidates {
            if self.doc_freq(&w)? > 0 {
                words.push(w);
            }
        }
        self.globs.insert(pattern.to_owned(), words.clone());
        Ok(words)
    }

    /// A bound of the hits of `node`: a word gives its document
    /// frequency, a phrase the least of its words, `and` the least of its
    /// items, `or` and a glob the sum. `None` when it matches every
    /// document.
    fn bound(&mut self, node: &Node) -> tantivy::Result<Option<u64>> {
        Ok(match node {
            Node::Atom { words } if words.is_empty() => None,
            Node::Atom { words } => {
                let mut sum = 0u64;
                for w in words {
                    sum = sum.saturating_add(self.doc_freq(w)?);
                }
                Some(sum)
            }
            Node::Glob { pattern } => {
                let mut sum = 0u64;
                for w in self.expand(pattern)? {
                    sum = sum.saturating_add(self.doc_freq(&w)?);
                }
                Some(sum)
            }
            Node::Phrase { words } => {
                let mut least = if words.is_empty() { 0 } else { u64::MAX };
                for w in words {
                    least = least.min(self.doc_freq(w)?);
                }
                Some(least)
            }
            Node::And { items, .. } => {
                let mut least: Option<u64> = None;
                for n in items {
                    if let Some(b) = self.bound(n)? {
                        least = Some(least.map_or(b, |l| l.min(b)));
                    }
                }
                Some(least.unwrap_or(0))
            }
            Node::Or { items } => {
                let mut sum = 0u64;
                for n in items {
                    sum = sum.saturating_add(self.bound(n)?.unwrap_or(0));
                }
                Some(sum)
            }
        })
    }

    /// The scores of `word` in the segment `ord`, for the documents that
    /// may be hits.
    fn term_scores(&mut self, ord: usize, word: &str) -> tantivy::Result<Scores> {
        if !self.scored {
            return self.term_docs(ord, word);
        }
        let Some(idf) = self.idf(word)? else {
            return Ok(Vec::new());
        };
        let Some(ix) = &self.index else {
            return Ok(Vec::new());
        };
        let seg = &ix.searcher.segment_readers()[ord];
        let term = Term::from_field_text(ix.text, word);
        let Some(mut postings) = seg
            .inverted_index(ix.text)?
            .read_postings(&term, IndexRecordOption::WithFreqs)?
        else {
            return Ok(Vec::new());
        };
        let mean = self.mean;
        let lengths = &ix.lengths.segments[ord];
        let mut out = Vec::new();
        if let Some(docs) = &self.drive[ord] {
            // A small filter: its documents seek the postings.
            for &d in docs {
                if postings.doc() < d {
                    postings.seek(d);
                }
                if postings.doc() == TERMINATED {
                    break;
                }
                if postings.doc() == d {
                    let len = lengths[d as usize].saturating_sub(1);
                    out.push((d, okapi(postings.term_freq(), len, idf, mean)));
                }
            }
            return Ok(out);
        }
        let mut d = postings.doc();
        while d != TERMINATED {
            if self.keep(ord, seg, d) {
                let len = lengths[d as usize].saturating_sub(1);
                out.push((d, okapi(postings.term_freq(), len, idf, mean)));
            }
            d = postings.advance();
        }
        Ok(out)
    }

    /// The documents of the segment `ord` that hold `word` and that may be
    /// hits, with the score 0.
    fn term_docs(&self, ord: usize, word: &str) -> tantivy::Result<Scores> {
        let Some(ix) = &self.index else {
            return Ok(Vec::new());
        };
        let seg = &ix.searcher.segment_readers()[ord];
        let term = Term::from_field_text(ix.text, word);
        let Some(mut postings) = seg
            .inverted_index(ix.text)?
            .read_postings(&term, IndexRecordOption::Basic)?
        else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        if let Some(docs) = &self.drive[ord] {
            for &d in docs {
                if postings.doc() < d {
                    postings.seek(d);
                }
                if postings.doc() == TERMINATED {
                    break;
                }
                if postings.doc() == d {
                    out.push((d, 0));
                }
            }
            return Ok(out);
        }
        let mut d = postings.doc();
        while d != TERMINATED {
            if self.keep(ord, seg, d) {
                out.push((d, 0));
            }
            d = postings.advance();
        }
        Ok(out)
    }

    /// The scores of `word` in the segment `ord` for the documents `docs`,
    /// which hold it, in their order.
    fn term_scores_for(
        &mut self,
        ord: usize,
        word: &str,
        docs: &[DocId],
    ) -> tantivy::Result<Scores> {
        let idf = self.idf(word)?.unwrap_or(0.0);
        let Some(ix) = &self.index else {
            return Ok(Vec::new());
        };
        let seg = &ix.searcher.segment_readers()[ord];
        let term = Term::from_field_text(ix.text, word);
        let lengths = &ix.lengths.segments[ord];
        let Some(mut postings) = seg
            .inverted_index(ix.text)?
            .read_postings(&term, IndexRecordOption::WithFreqs)?
        else {
            return Ok(docs.iter().map(|d| (*d, 0)).collect());
        };
        let mut out = Vec::with_capacity(docs.len());
        for &d in docs {
            if postings.doc() < d {
                postings.seek(d);
            }
            let mut score = 0;
            if postings.doc() == d {
                let len = lengths[d as usize].saturating_sub(1);
                score = okapi(postings.term_freq(), len, idf, self.mean);
            }
            out.push((d, score));
        }
        Ok(out)
    }

    /// The documents of the segment `ord` that may be hits and that hold
    /// each pair of `words` in a row.
    fn pair_docs(&self, ord: usize, words: &[String]) -> tantivy::Result<Vec<DocId>> {
        let Some(ix) = &self.index else {
            return Ok(Vec::new());
        };
        let seg = &ix.searcher.segment_readers()[ord];
        let inv = seg.inverted_index(ix.pairs)?;
        let mut cand: Option<Vec<DocId>> = None;
        for w in words.windows(2) {
            let term = Term::from_field_text(ix.pairs, &pair(&w[0], &w[1]));
            let mut docs = Vec::new();
            if let Some(mut p) = inv.read_postings(&term, IndexRecordOption::Basic)? {
                match (&self.drive[ord], &cand) {
                    (_, Some(c)) => {
                        for &d in c {
                            if p.doc() < d {
                                p.seek(d);
                            }
                            if p.doc() == d {
                                docs.push(d);
                            }
                        }
                    }
                    (Some(drive), None) => {
                        for &d in drive {
                            if p.doc() < d {
                                p.seek(d);
                            }
                            if p.doc() == TERMINATED {
                                break;
                            }
                            if p.doc() == d {
                                docs.push(d);
                            }
                        }
                    }
                    (None, None) => {
                        let mut d = p.doc();
                        while d != TERMINATED {
                            if self.keep(ord, seg, d) {
                                docs.push(d);
                            }
                            d = p.advance();
                        }
                    }
                }
            }
            if docs.is_empty() {
                return Ok(docs);
            }
            cand = Some(docs);
        }
        Ok(cand.unwrap_or_default())
    }

    /// The documents of `cand` whose words hold `words` in a row.
    fn in_a_row(
        &self,
        ord: usize,
        words: &[String],
        cand: Vec<DocId>,
    ) -> tantivy::Result<Vec<DocId>> {
        let Some(ix) = &self.index else {
            return Ok(Vec::new());
        };
        let seg = &ix.searcher.segment_readers()[ord];
        let inv = seg.inverted_index(ix.text)?;
        let mut postings = Vec::new();
        for w in words {
            let term = Term::from_field_text(ix.text, w);
            match inv.read_postings(&term, IndexRecordOption::WithFreqsAndPositions)? {
                Some(p) => postings.push(p),
                None => return Ok(Vec::new()),
            }
        }
        let mut out = Vec::new();
        let mut pos: Vec<Vec<u32>> = vec![Vec::new(); words.len()];
        'docs: for doc in cand {
            for (i, p) in postings.iter_mut().enumerate() {
                if p.doc() < doc {
                    p.seek(doc);
                }
                if p.doc() != doc {
                    continue 'docs;
                }
                p.positions(&mut pos[i]);
            }
            let found = pos[0].iter().any(|&start| {
                (1..words.len()).all(|i| pos[i].binary_search(&(start + i as u32)).is_ok())
            });
            if found {
                out.push(doc);
            }
        }
        Ok(out)
    }

    /// The hits of `node` in the segment `ord`, or `None` when it matches
    /// every document.
    fn eval(&mut self, ord: usize, node: &Node) -> tantivy::Result<Option<Scores>> {
        Ok(match node {
            Node::Atom { words } => {
                if words.is_empty() {
                    return Ok(None);
                }
                let mut acc = Vec::new();
                for w in words {
                    let s = self.term_scores(ord, w)?;
                    acc = if acc.is_empty() { s } else { union(&acc, &s) };
                }
                Some(acc)
            }
            Node::Glob { pattern } => {
                let mut acc = Vec::new();
                for w in self.expand(pattern)? {
                    let s = self.term_scores(ord, &w)?;
                    acc = if acc.is_empty() { s } else { union(&acc, &s) };
                }
                Some(acc)
            }
            Node::Phrase { words } => {
                if words.is_empty() {
                    return Ok(Some(Vec::new()));
                }
                for w in words {
                    if self.doc_freq(w)? == 0 {
                        return Ok(Some(Vec::new()));
                    }
                }
                if words.len() == 1 {
                    return Ok(Some(self.term_scores(ord, &words[0])?));
                }
                let mut cand = self.pair_docs(ord, words)?;
                if words.len() > 2 && !cand.is_empty() {
                    cand = self.in_a_row(ord, words, cand)?;
                }
                let mut acc: Scores = cand.iter().map(|d| (*d, 0)).collect();
                if !self.scored {
                    return Ok(Some(acc));
                }
                for w in words {
                    let s = self.term_scores_for(ord, w, &cand)?;
                    for (a, b) in acc.iter_mut().zip(s) {
                        a.1 += b.1;
                    }
                }
                Some(acc)
            }
            Node::And { items, nots } => {
                let mut lists = Vec::new();
                for n in items {
                    if let Some(r) = self.eval(ord, n)? {
                        lists.push(r);
                    }
                }
                let mut set = match lists.len() {
                    0 => Vec::new(),
                    _ => {
                        lists.sort_by_key(Vec::len);
                        let mut acc = lists[0].clone();
                        for l in &lists[1..] {
                            acc = intersection(&acc, l);
                        }
                        acc
                    }
                };
                for n in nots {
                    if set.is_empty() {
                        break;
                    }
                    if let Some(r) = self.eval(ord, n)? {
                        set = difference(&set, &r);
                    }
                }
                Some(set)
            }
            Node::Or { items } => {
                let mut acc = Vec::new();
                for n in items {
                    if let Some(r) = self.eval(ord, n)? {
                        acc = if acc.is_empty() { r } else { union(&acc, &r) };
                    }
                }
                Some(acc)
            }
        })
    }

    /// The score of a word of the overlay in `doc`.
    fn doc_score(&mut self, doc: &Doc, word: &str) -> tantivy::Result<Option<i64>> {
        let Some(&f) = doc.tf.get(word) else {
            return Ok(None);
        };
        let Some(idf) = self.idf(word)? else {
            return Ok(None);
        };
        Ok(Some(okapi(f, doc.len(), idf, self.mean)))
    }

    /// The result of `node` for the overlay document `doc`.
    fn eval_doc(&mut self, doc: &Doc, node: &Node) -> tantivy::Result<Eval> {
        Ok(match node {
            Node::Atom { words } => {
                if words.is_empty() {
                    return Ok(Eval::Every);
                }
                let mut total: Option<i64> = None;
                for w in words {
                    if let Some(s) = self.doc_score(doc, w)? {
                        total = Some(total.unwrap_or(0) + s);
                    }
                }
                total.map_or(Eval::No, Eval::Yes)
            }
            Node::Glob { pattern } => {
                let mut total: Option<i64> = None;
                for w in self.expand(pattern)? {
                    if let Some(s) = self.doc_score(doc, &w)? {
                        total = Some(total.unwrap_or(0) + s);
                    }
                }
                total.map_or(Eval::No, Eval::Yes)
            }
            Node::Phrase { words } => {
                if words.is_empty() {
                    return Ok(Eval::No);
                }
                for w in words {
                    if self.doc_freq(w)? == 0 {
                        return Ok(Eval::No);
                    }
                }
                if !doc
                    .words
                    .windows(words.len())
                    .any(|w| w == words.as_slice())
                {
                    return Ok(Eval::No);
                }
                let mut total = 0;
                for w in words {
                    total += self.doc_score(doc, w)?.unwrap_or(0);
                }
                Eval::Yes(total)
            }
            Node::And { items, nots } => {
                let mut total: Option<i64> = None;
                for n in items {
                    match self.eval_doc(doc, n)? {
                        Eval::Every => {}
                        Eval::No => return Ok(Eval::No),
                        Eval::Yes(s) => total = Some(total.unwrap_or(0) + s),
                    }
                }
                let Some(total) = total else {
                    return Ok(Eval::No);
                };
                for n in nots {
                    if let Eval::Yes(_) = self.eval_doc(doc, n)? {
                        return Ok(Eval::No);
                    }
                }
                Eval::Yes(total)
            }
            Node::Or { items } => {
                let mut total: Option<i64> = None;
                for n in items {
                    if let Eval::Yes(s) = self.eval_doc(doc, n)? {
                        total = Some(total.unwrap_or(0) + s);
                    }
                }
                total.map_or(Eval::No, Eval::Yes)
            }
        })
    }

    /// Run `node` for `want`. `forced` forces a strategy, for the tests.
    /// Return the hits and the strategy of the search of the index, `None`
    /// with no index.
    pub fn search(
        &mut self,
        node: &Node,
        want: Want,
        forced: Option<Strategy>,
    ) -> tantivy::Result<(Found, Option<Strategy>)> {
        let strategy = if self.index.is_some() {
            Some(self.prepare(node, want, forced)?)
        } else {
            None
        };
        let mut count = 0u64;
        if matches!(node, Node::Atom { words } if words.is_empty()) {
            // `Catalog.search` gives no hit for a tree that matches every
            // document.
            return Ok((Found::default(), strategy));
        }
        let segments = self
            .index
            .as_ref()
            .map_or(0, |i| i.searcher.segment_readers().len());
        if !matches!(want, Want::Top(_)) {
            // A set has no score: the words read no frequency.
            self.scored = false;
            let mut found = Found::default();
            if let Some(maps) = self.index.as_ref().map(|ix| ix.maps) {
                let mut docs = HitDocs::new(maps);
                for ord in 0..segments {
                    if let Some(hits) = self.eval(ord, node)? {
                        for (doc, _) in hits {
                            docs.insert(ord, doc);
                        }
                    }
                }
                found = match (want, self.filter) {
                    (Want::Keys, Some(filter)) => docs.into_keys(maps, filter),
                    _ => docs.into_found(maps),
                };
            }
            let mut more = Vec::new();
            for doc in self.overlay {
                if self.in_filter(doc.rid) && matches!(self.eval_doc(doc, node)?, Eval::Yes(_)) {
                    more.push(doc.rid);
                }
            }
            search::add_to_set(&mut found, want, more, self.filter);
            return Ok((found, strategy));
        }
        let mut top = Top::new(match want {
            Want::Top(limit) => limit,
            Want::Set | Want::Keys => None,
        });
        for ord in 0..segments {
            let Some(hits) = self.eval(ord, node)? else {
                continue;
            };
            let Some(ix) = &self.index else {
                break;
            };
            let rids = &ix.maps.doc_to_rid[ord];
            for (doc, score) in hits {
                count += 1;
                top.push((score, rids[doc as usize]));
            }
        }
        for doc in self.overlay {
            if !self.in_filter(doc.rid) {
                continue;
            }
            if let Eval::Yes(score) = self.eval_doc(doc, node)? {
                count += 1;
                top.push((score, doc.rid));
            }
        }
        Ok((
            Found {
                count,
                hits: top.into_sorted(),
                keys: None,
            },
            strategy,
        ))
    }

    /// Tell if the filter holds `rid`.
    fn in_filter(&self, rid: i32) -> bool {
        self.filter.is_none_or(|f| f.contains(rid))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_score_is_the_integer_of_okapi_index() {
        // `int(tf * idf + 0.5)` of `OkapiIndex._search_wids`, for a word
        // once in a document of 3 words, in 1 document of 4, with a mean of
        // 2.5 words: 1523.
        let idf = (1.0f64 + 4.0 / 1.0).ln() * 1024.0;
        assert_eq!(okapi(1, 3, idf, 2.5), 1523);
    }

    #[test]
    fn the_set_operations_add_up_the_scores() {
        let a = vec![(1, 10), (3, 5)];
        let b = vec![(3, 1), (4, 2)];
        assert_eq!(union(&a, &b), vec![(1, 10), (3, 6), (4, 2)]);
        assert_eq!(intersection(&a, &b), vec![(3, 6)]);
        assert_eq!(difference(&a, &b), vec![(1, 10)]);
    }
}
