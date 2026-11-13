//! The translation of a parse tree into a tantivy query in the enhanced
//! mode (ADR 0060, section 11), and the bound of its hits (section 12).
//!
//! A query reads the fields of its index and of the indexes of its boosts,
//! each with the weight of its index. A word matches in any of them; the
//! score is the sum of the BM25 scores of tantivy, each times its weight.
//!
//! - An atom matches any of its words. A word that the analyzer drops, a
//!   stop word, leaves the atom; an atom with no word left matches every
//!   document, so an `and` leaves it out.
//! - A phrase of two words reads the term of their pair. A longer phrase
//!   needs each of its pairs, and the positions of its words in a row.
//! - A glob matches the terms of its pattern with its literal parts
//!   folded. A pattern of one literal, alone or with a final `*`, also
//!   matches the analysed literal as an exact term. A match scores a
//!   constant, and the exact term adds its BM25 score. So `running*` finds
//!   the stem `run` and each term that starts with `running`, not
//!   `runway`. A document of the stem ranks above a document of the prefix
//!   alone.
//! - With fuzzy matching, a word also matches the terms at the edit
//!   distance of the settings, with a constant score.
//! - `and` needs each item and none of the nots; `or` any item.

use std::collections::HashMap;
use tantivy::query::{
    BooleanQuery, BoostQuery, EmptyQuery, FuzzyTermQuery, Occur, PhraseQuery, Query, RegexQuery,
    TermQuery,
};

use tantivy::schema::{Field, IndexRecordOption};
use tantivy::{Searcher, Term};

use super::analysis::{self, GlobChar, GlobPart};
use super::clause::Node;
use super::index::{Analyzers, LangFields, pairs_of};

/// One field that a query reads, with its weight.
#[derive(Clone, Debug)]
pub struct Read {
    /// The fields.
    pub field: LangFields,
    /// The weight of its index.
    pub weight: f32,
}

/// A glob in the language of one field: the positions of its folded form
/// and of its exact stem, and their regular expression.
#[derive(Clone, Debug)]
pub struct Glob {
    /// The positions of each form, each form once.
    pub forms: Vec<Vec<GlobChar>>,
    /// The regular expression of the forms.
    pub regex: String,
    /// The analysed literal of a pattern of one literal, alone or with a
    /// final `*`, when the analyzer gives one word: an exact term that
    /// adds its BM25 score.
    pub stem: Option<String>,
}

impl Glob {
    /// Tell if `word`, a term of the field, matches one of the forms.
    pub fn matches(&self, word: &str) -> bool {
        self.forms.iter().any(|f| analysis::glob_matches(f, word))
    }
}

/// The translator of one query.
pub struct Translator<'a> {
    reads: &'a [Read],
    analyzers: Analyzers,
    fuzzy: u8,
    globs: HashMap<(Field, String), Option<Glob>>,
}

impl<'a> Translator<'a> {
    /// A translator over `reads`, with the edit distance `fuzzy`.
    pub fn new(reads: &'a [Read], fuzzy: u8) -> Translator<'a> {
        Translator {
            reads,
            analyzers: Analyzers::default(),
            fuzzy,
            globs: HashMap::new(),
        }
    }

    /// The terms of `words` in the language of `read`.
    pub fn terms(&mut self, read: &Read, words: &[String]) -> Vec<String> {
        words
            .iter()
            .flat_map(|w| self.analyzers.words(&read.field.language, w))
            .collect()
    }

    /// The glob `pattern` in the language of `read`, or `None` when tantivy
    /// refuses its regular expression: then neither the index nor the text
    /// overlay matches it.
    pub fn glob(&mut self, read: &Read, pattern: &str) -> Option<Glob> {
        let key = (read.field.text, pattern.to_owned());
        if let Some(found) = self.globs.get(&key) {
            return found.clone();
        }
        let parts = analysis::glob_parts(pattern);
        let folded = analysis::glob_chars(&parts, analysis::fold);
        let stem = match parts.as_slice() {
            [GlobPart::Lit(literal)] | [GlobPart::Lit(literal), GlobPart::Any] => {
                match self
                    .analyzers
                    .words(&read.field.language, literal)
                    .as_slice()
                {
                    [one] => Some(one.clone()),
                    _ => None,
                }
            }
            _ => None,
        };
        let mut forms = vec![folded];
        if let Some(stem) = &stem
            && !analysis::glob_matches(&forms[0], stem)
        {
            forms.push(stem.chars().map(GlobChar::Char).collect());
        }
        let regex = match forms.as_slice() {
            [one] => analysis::glob_regex(one),
            many => many
                .iter()
                .map(|f| format!("({})", analysis::glob_regex(f)))
                .collect::<Vec<_>>()
                .join("|"),
        };
        let glob = RegexQuery::from_pattern(&regex, read.field.text)
            .is_ok()
            .then_some(Glob { forms, regex, stem });
        self.globs.insert(key, glob.clone());
        glob
    }

    fn term_query(&self, read: &Read, term: &str) -> Box<dyn Query> {
        let term = Term::from_field_text(read.field.text, term);
        let exact: Box<dyn Query> =
            Box::new(TermQuery::new(term.clone(), IndexRecordOption::WithFreqs));
        if self.fuzzy == 0 {
            return exact;
        }
        let fuzzy: Box<dyn Query> = Box::new(FuzzyTermQuery::new(term, self.fuzzy, true));
        Box::new(BooleanQuery::new(vec![
            (Occur::Should, exact),
            (Occur::Should, fuzzy),
        ]))
    }

    fn boosted(read: &Read, q: Box<dyn Query>) -> Box<dyn Query> {
        if read.weight == 1.0 {
            q
        } else {
            Box::new(BoostQuery::new(q, read.weight))
        }
    }

    /// The query of `glob` on the field of `read`: its forms with a
    /// constant score of 1, plus the BM25 score of its exact stem.
    fn glob_query(read: &Read, glob: Glob) -> Box<dyn Query> {
        let Ok(forms) = RegexQuery::from_pattern(&glob.regex, read.field.text) else {
            return Box::new(EmptyQuery);
        };
        let forms: Box<dyn Query> = Box::new(forms);
        let Some(stem) = glob.stem else {
            return forms;
        };
        let term = Term::from_field_text(read.field.text, &stem);
        let stem: Box<dyn Query> = Box::new(TermQuery::new(term, IndexRecordOption::WithFreqs));
        Self::any(vec![forms, stem])
    }

    fn any(clauses: Vec<Box<dyn Query>>) -> Box<dyn Query> {
        Box::new(BooleanQuery::new(
            clauses.into_iter().map(|q| (Occur::Should, q)).collect(),
        ))
    }

    /// The query of `node`, or `None` when it matches every document.
    pub fn translate(&mut self, node: &Node) -> Option<Box<dyn Query>> {
        match node {
            Node::Atom { words } => {
                let mut across = Vec::new();
                for read in self.reads {
                    let terms = self.terms(read, words);
                    if terms.is_empty() {
                        continue;
                    }
                    let clauses = terms.iter().map(|t| self.term_query(read, t)).collect();
                    across.push(Self::boosted(read, Self::any(clauses)));
                }
                if across.is_empty() {
                    return None;
                }
                Some(Self::any(across))
            }
            Node::Phrase { words } => {
                let mut across = Vec::new();
                let mut any_terms = false;
                for read in self.reads {
                    let terms = self.terms(read, words);
                    let q: Box<dyn Query> = match terms.len() {
                        0 => continue,
                        1 => self.term_query(read, &terms[0]),
                        _ => {
                            let mut must: Vec<(Occur, Box<dyn Query>)> = pairs_of(&terms)
                                .iter()
                                .map(|p| {
                                    let t = Term::from_field_text(read.field.pairs, p);
                                    let q: Box<dyn Query> =
                                        Box::new(TermQuery::new(t, IndexRecordOption::WithFreqs));
                                    (Occur::Must, q)
                                })
                                .collect();
                            if terms.len() > 2 {
                                let ts = terms
                                    .iter()
                                    .map(|t| Term::from_field_text(read.field.text, t))
                                    .collect();
                                must.push((Occur::Must, Box::new(PhraseQuery::new(ts))));
                            }
                            Box::new(BooleanQuery::new(must))
                        }
                    };
                    any_terms = true;
                    across.push(Self::boosted(read, q));
                }
                if !any_terms {
                    return None;
                }
                Some(Self::any(across))
            }
            Node::Glob { pattern } => {
                let mut across = Vec::new();
                for read in self.reads {
                    let q = match self.glob(read, pattern) {
                        Some(glob) => Self::glob_query(read, glob),
                        None => Box::new(EmptyQuery),
                    };
                    across.push(Self::boosted(read, q));
                }
                Some(Self::any(across))
            }
            Node::And { items, nots } => {
                let mut clauses: Vec<(Occur, Box<dyn Query>)> = items
                    .iter()
                    .filter_map(|n| self.translate(n).map(|q| (Occur::Must, q)))
                    .collect();
                if clauses.is_empty() {
                    return Some(Box::new(EmptyQuery));
                }
                let excluded: Vec<_> = nots
                    .iter()
                    .filter_map(|n| self.translate(n).map(|q| (Occur::MustNot, q)))
                    .collect();
                clauses.extend(excluded);
                Some(Box::new(BooleanQuery::new(clauses)))
            }
            Node::Or { items } => {
                let clauses: Vec<_> = items.iter().filter_map(|n| self.translate(n)).collect();
                if clauses.is_empty() {
                    return Some(Box::new(EmptyQuery));
                }
                Some(Self::any(clauses))
            }
        }
    }

    /// The bound of the hits of `node` on `searcher`, from the document
    /// frequencies of its words: a word gives its frequency, a phrase the
    /// least of its words, `and` the least of its items, `or` the sum. A
    /// glob and fuzzy matching have no bound: `None`. A node that matches
    /// every document has no bound either.
    pub fn bound(&mut self, searcher: &Searcher, node: &Node) -> Option<u64> {
        if self.fuzzy > 0 {
            return None;
        }
        match node {
            Node::Atom { words } => {
                let mut sum = 0u64;
                let mut any = false;
                for read in self.reads {
                    for t in self.terms(read, words) {
                        any = true;
                        let term = Term::from_field_text(read.field.text, &t);
                        sum = sum.saturating_add(searcher.doc_freq(&term).unwrap_or(u64::MAX / 4));
                    }
                }
                any.then_some(sum)
            }
            Node::Phrase { words } => {
                let mut sum = 0u64;
                let mut any = false;
                for read in self.reads {
                    let terms = self.terms(read, words);
                    let least = match terms.len() {
                        0 => continue,
                        1 => searcher.doc_freq(&Term::from_field_text(read.field.text, &terms[0])),
                        _ => pairs_of(&terms)
                            .iter()
                            .map(|p| searcher.doc_freq(&Term::from_field_text(read.field.pairs, p)))
                            .try_fold(u64::MAX, |m, d| d.map(|d| m.min(d))),
                    };
                    any = true;
                    sum = sum.saturating_add(least.unwrap_or(u64::MAX / 4));
                }
                any.then_some(sum)
            }
            Node::Glob { .. } => None,
            Node::And { items, .. } => {
                let bounds: Vec<Option<u64>> =
                    items.iter().map(|n| self.bound(searcher, n)).collect();
                bounds.into_iter().flatten().min()
            }
            Node::Or { items } => {
                let mut sum = 0u64;
                for n in items {
                    sum = sum.saturating_add(self.bound(searcher, n)?);
                }
                Some(sum)
            }
        }
    }
}
