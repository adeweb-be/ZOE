//! The text overlay (ADR 0060, section 13): the texts that one query scores
//! beside the tantivy index, in place of the replaced documents.
//!
//! The overlay builds no tantivy segment. It splits the texts of the fields
//! that the query reads, and scores the words of the query in each
//! document as tantivy scores them in the index: the BM25 of each term with
//! the statistics of the current index, times the weight of its field; a
//! glob and a fuzzy match with the constant score of their weight, and the
//! exact stem of a glob with its BM25 too. So a document scores the same in
//! the overlay and in the index.

use std::collections::{HashMap, HashSet};

use tantivy::fieldnorm::FieldNormReader;
use tantivy::query::{Bm25StatisticsProvider, Bm25Weight};
use tantivy::schema::Field;
use tantivy::{Score, Term};

use super::analysis;
use super::clause::Node;
use super::index::{Fields, field_of, pairs_of};
use super::translate::{Read, Translator};

/// The words of one field of one document.
#[derive(Debug, Default)]
struct FieldWords {
    words: Vec<String>,
    tf: HashMap<String, u32>,
    pairs: HashMap<String, u32>,
    n_pairs: u32,
}

/// One document of the overlay.
#[derive(Debug)]
pub struct Doc {
    /// The rid.
    pub rid: i32,
    fields: HashMap<Field, FieldWords>,
}

impl Doc {
    /// The document of `rid` with `texts`: for each text index, its
    /// language and its text. Only the fields of `reads` count.
    pub fn new(
        rid: i32,
        texts: &[(String, String, String)],
        fields: &Fields,
        reads: &[Read],
        translator: &mut Translator<'_>,
    ) -> Doc {
        let mut out = HashMap::new();
        for (index, language, text) in texts {
            let Some(langs) = fields.indexes.get(index) else {
                continue;
            };
            let names: Vec<String> = langs.iter().map(|f| f.language.clone()).collect();
            let field = &langs[field_of(&names, language)];
            let Some(read) = reads.iter().find(|r| r.field.text == field.text) else {
                continue;
            };
            let words = translator.terms(read, std::slice::from_ref(text));
            let mut tf = HashMap::new();
            for w in &words {
                *tf.entry(w.clone()).or_insert(0u32) += 1;
            }
            let pair_list = pairs_of(&words);
            let mut pairs = HashMap::new();
            for p in &pair_list {
                *pairs.entry(p.clone()).or_insert(0u32) += 1;
            }
            out.insert(
                field.text,
                FieldWords {
                    words,
                    tf,
                    pairs,
                    n_pairs: pair_list.len() as u32,
                },
            );
        }
        Doc { rid, fields: out }
    }
}

/// The statistics of the overlay alone: for a query with no index, or on
/// an index with no document.
#[derive(Debug, Default)]
pub struct OwnStats {
    docs: u64,
    tokens: HashMap<Field, u64>,
    freq: HashMap<Term, u64>,
}

impl OwnStats {
    /// The statistics of `docs` for the fields of `reads`.
    pub fn of(docs: &[Doc], reads: &[Read]) -> OwnStats {
        let mut s = OwnStats {
            docs: docs.len() as u64,
            ..OwnStats::default()
        };
        for doc in docs {
            for read in reads {
                let Some(fw) = doc.fields.get(&read.field.text) else {
                    continue;
                };
                *s.tokens.entry(read.field.text).or_default() += fw.words.len() as u64;
                *s.tokens.entry(read.field.pairs).or_default() += u64::from(fw.n_pairs);
                for w in fw.tf.keys() {
                    *s.freq
                        .entry(Term::from_field_text(read.field.text, w))
                        .or_default() += 1;
                }
                for p in fw.pairs.keys() {
                    *s.freq
                        .entry(Term::from_field_text(read.field.pairs, p))
                        .or_default() += 1;
                }
            }
        }
        s
    }
}

impl Bm25StatisticsProvider for OwnStats {
    fn total_num_tokens(&self, field: Field) -> tantivy::Result<u64> {
        Ok(self.tokens.get(&field).copied().unwrap_or(0))
    }

    fn total_num_docs(&self) -> tantivy::Result<u64> {
        Ok(self.docs)
    }

    fn doc_freq(&self, term: &Term) -> tantivy::Result<u64> {
        Ok(self.freq.get(term).copied().unwrap_or(0))
    }
}

/// The result of a node for one document.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Eval {
    /// The node matches every document: an `and` leaves it out.
    Every,
    /// The document does not match.
    No,
    /// The document matches, with this score.
    Yes(Score),
}

/// The scorer of the overlay of one query.
pub struct Scorer<'s> {
    stats: &'s dyn Bm25StatisticsProvider,
    weights: HashMap<(Term, u32), Option<Bm25Weight>>,
    fuzzy: u8,
}

impl<'s> Scorer<'s> {
    /// A scorer with the statistics `stats` and the edit distance `fuzzy`.
    pub fn new(stats: &'s dyn Bm25StatisticsProvider, fuzzy: u8) -> Scorer<'s> {
        Scorer {
            stats,
            weights: HashMap::new(),
            fuzzy,
        }
    }

    /// The weight of `terms`, one field, boosted by `boost`. `None` when
    /// the statistics do not give a finite weight.
    fn weight(&mut self, terms: &[Term], boost: f32) -> Option<Bm25Weight> {
        let key = (
            terms[0].clone(),
            boost.to_bits() ^ (terms.len() as u32) << 24,
        );
        if terms.len() == 1
            && let Some(w) = self.weights.get(&key)
        {
            return w.clone();
        }
        let docs = self.stats.total_num_docs().ok()?;
        let mut ok = docs > 0;
        for t in terms {
            ok &= self.stats.doc_freq(t).is_ok_and(|f| f <= docs);
        }
        let w = ok
            .then(|| Bm25Weight::for_terms(self.stats, terms).ok())
            .flatten()
            .map(|w| w.boost_by(boost));
        if terms.len() == 1 {
            self.weights.insert(key, w.clone());
        }
        w
    }

    fn bm25(&mut self, terms: &[Term], boost: f32, len: u32, freq: u32) -> Score {
        let id = FieldNormReader::fieldnorm_to_id(len);
        match self.weight(terms, boost) {
            Some(w) => {
                let s = w.score(id, freq);
                if s.is_finite() { s } else { 0.0 }
            }
            None => 0.0,
        }
    }

    /// The score of the term `t` in the words `fw` of the field of `read`:
    /// its BM25, and the weight of the field for a fuzzy match. `None` when
    /// it does not match.
    fn word(&mut self, fw: &FieldWords, read: &Read, t: &str) -> Option<Score> {
        let mut s: Option<Score> = None;
        if let Some(&freq) = fw.tf.get(t) {
            let term = Term::from_field_text(read.field.text, t);
            s = Some(self.bm25(&[term], read.weight, fw.words.len() as u32, freq));
        }
        if self.fuzzy > 0
            && fw
                .tf
                .keys()
                .any(|w| analysis::edit_distance(w, t) <= self.fuzzy as usize)
        {
            s = Some(s.unwrap_or(0.0) + read.weight);
        }
        s
    }

    /// The score of `doc` for `node`, or `None` when it does not match.
    pub fn score(
        &mut self,
        doc: &Doc,
        node: &Node,
        translator: &mut Translator<'_>,
        reads: &[Read],
    ) -> Option<Score> {
        match self.eval(doc, node, translator, reads) {
            Eval::Yes(s) => Some(s),
            Eval::Every | Eval::No => None,
        }
    }

    fn eval(
        &mut self,
        doc: &Doc,
        node: &Node,
        translator: &mut Translator<'_>,
        reads: &[Read],
    ) -> Eval {
        match node {
            Node::Atom { words } => {
                let mut any_terms = false;
                let mut total: Option<Score> = None;
                for read in reads {
                    let terms = translator.terms(read, words);
                    if terms.is_empty() {
                        continue;
                    }
                    any_terms = true;
                    let Some(fw) = doc.fields.get(&read.field.text) else {
                        continue;
                    };
                    for t in &terms {
                        if let Some(s) = self.word(fw, read, t) {
                            total = Some(total.unwrap_or(0.0) + s);
                        }
                    }
                }
                match (any_terms, total) {
                    (false, _) => Eval::Every,
                    (true, None) => Eval::No,
                    (true, Some(s)) => Eval::Yes(s),
                }
            }
            Node::Phrase { words } => {
                let mut any_terms = false;
                let mut total: Option<Score> = None;
                for read in reads {
                    let terms = translator.terms(read, words);
                    if terms.is_empty() {
                        continue;
                    }
                    any_terms = true;
                    let Some(fw) = doc.fields.get(&read.field.text) else {
                        continue;
                    };
                    if terms.len() == 1 {
                        if let Some(s) = self.word(fw, read, &terms[0]) {
                            total = Some(total.unwrap_or(0.0) + s);
                        }
                        continue;
                    }
                    let pairs = pairs_of(&terms);
                    let mut s = 0.0;
                    let mut all = true;
                    for p in &pairs {
                        match fw.pairs.get(p) {
                            Some(&freq) => {
                                let term = Term::from_field_text(read.field.pairs, p);
                                s += self.bm25(&[term], read.weight, fw.n_pairs, freq);
                            }
                            None => {
                                all = false;
                                break;
                            }
                        }
                    }
                    if !all {
                        continue;
                    }
                    if terms.len() > 2 {
                        let count = fw
                            .words
                            .windows(terms.len())
                            .filter(|w| *w == terms)
                            .count();
                        if count == 0 {
                            continue;
                        }
                        let ts: Vec<Term> = terms
                            .iter()
                            .map(|t| Term::from_field_text(read.field.text, t))
                            .collect();
                        s += self.bm25(&ts, read.weight, fw.words.len() as u32, count as u32);
                    }
                    total = Some(total.unwrap_or(0.0) + s);
                }
                match (any_terms, total) {
                    (false, _) => Eval::Every,
                    (true, None) => Eval::No,
                    (true, Some(s)) => Eval::Yes(s),
                }
            }
            Node::Glob { pattern } => {
                let mut total: Option<Score> = None;
                for read in reads {
                    let Some(fw) = doc.fields.get(&read.field.text) else {
                        continue;
                    };
                    let Some(glob) = translator.glob(read, pattern) else {
                        continue;
                    };
                    if !fw.tf.keys().any(|w| glob.matches(w)) {
                        continue;
                    }
                    let mut s = read.weight;
                    if let Some(stem) = &glob.stem
                        && let Some(&freq) = fw.tf.get(stem)
                    {
                        let term = Term::from_field_text(read.field.text, stem);
                        s += self.bm25(&[term], read.weight, fw.words.len() as u32, freq);
                    }
                    total = Some(total.unwrap_or(0.0) + s);
                }
                total.map_or(Eval::No, Eval::Yes)
            }
            Node::And { items, nots } => {
                let mut total: Option<Score> = None;
                for n in items {
                    match self.eval(doc, n, translator, reads) {
                        Eval::Every => {}
                        Eval::No => return Eval::No,
                        Eval::Yes(s) => total = Some(total.unwrap_or(0.0) + s),
                    }
                }
                let Some(total) = total else {
                    return Eval::No;
                };
                for n in nots {
                    if let Eval::Yes(_) = self.eval(doc, n, translator, reads) {
                        return Eval::No;
                    }
                }
                Eval::Yes(total)
            }
            Node::Or { items } => {
                let mut total: Option<Score> = None;
                for n in items {
                    if let Eval::Yes(s) = self.eval(doc, n, translator, reads) {
                        total = Some(total.unwrap_or(0.0) + s);
                    }
                }
                total.map_or(Eval::No, Eval::Yes)
            }
        }
    }
}

/// The rids of `docs`.
pub fn rids(docs: &[Doc]) -> HashSet<i32> {
    docs.iter().map(|d| d.rid).collect()
}
