//! The enhanced mode: the parse tree of ZCTextIndex as a tantivy query,
//! with the analyzer of the enhanced mode, the BM25 of tantivy and the
//! boosts of each field.

use tantivy::Index;
use tantivy::query::{
    BooleanQuery, BoostQuery, FuzzyTermQuery, Occur, PhraseQuery, Query, RegexQuery, TermQuery,
};
use tantivy::schema::{Field, IndexRecordOption};
use tantivy::tokenizer::TextAnalyzer;
use tantivy::{Term, tokenizer::TokenStream};

use super::analysis::ENHANCED;
use super::compat::Node;

/// The options of the translation.
#[derive(Clone, Debug)]
pub struct Options {
    /// The fields and their boosts: a word matches in any of them.
    pub fields: Vec<(Field, f32)>,
    /// A word also matches the words at this edit distance (0: off).
    pub fuzzy: u8,
}

/// The translator of the enhanced mode.
pub struct Translator {
    analyzer: TextAnalyzer,
    opts: Options,
}

impl Translator {
    /// Make a translator for `index`.
    pub fn new(index: &Index, opts: Options) -> Self {
        let analyzer = index.tokenizers().get(ENHANCED).expect("the enhanced analyzer");
        Self { analyzer, opts }
    }

    fn analyze(&mut self, word: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut stream = self.analyzer.token_stream(word);
        while stream.advance() {
            out.push(stream.token().text.clone());
        }
        out
    }

    fn across(&self, make: impl Fn(Field) -> Box<dyn Query>) -> Box<dyn Query> {
        if self.opts.fields.len() == 1 {
            return make(self.opts.fields[0].0);
        }
        let clauses = self
            .opts
            .fields
            .iter()
            .map(|&(f, boost)| {
                let q: Box<dyn Query> = Box::new(BoostQuery::new(make(f), boost));
                (Occur::Should, q)
            })
            .collect();
        Box::new(BooleanQuery::new(clauses))
    }

    fn word(&self, w: &str) -> Box<dyn Query> {
        let fuzzy = self.opts.fuzzy;
        self.across(|f| {
            let term = Term::from_field_text(f, w);
            if fuzzy > 0 {
                Box::new(FuzzyTermQuery::new(term, fuzzy, true))
            } else {
                Box::new(TermQuery::new(term, IndexRecordOption::WithFreqs))
            }
        })
    }

    /// Translate `node`, or `None` when it matches every document.
    pub fn translate(&mut self, node: &Node) -> Option<Box<dyn Query>> {
        match node {
            Node::Atom { words } => {
                let terms: Vec<String> = words.iter().flat_map(|w| self.analyze(w)).collect();
                if terms.is_empty() {
                    return None;
                }
                let clauses = terms.iter().map(|t| (Occur::Should, self.word(t))).collect();
                Some(Box::new(BooleanQuery::new(clauses)))
            }
            Node::Glob { pattern } => {
                let prefix: String =
                    pattern.chars().take_while(|c| *c != '*' && *c != '?').collect();
                let folded = self.analyze_prefix(&prefix);
                let regex = format!("{}.*", regex_escape(&folded));
                Some(self.across(|f| {
                    Box::new(RegexQuery::from_pattern(&regex, f).expect("a valid regex"))
                }))
            }
            Node::Phrase { words } => {
                let terms: Vec<String> = words.iter().flat_map(|w| self.analyze(w)).collect();
                match terms.len() {
                    0 => None,
                    1 => Some(self.word(&terms[0])),
                    _ => Some(self.across(|f| {
                        let ts = terms.iter().map(|t| Term::from_field_text(f, t)).collect();
                        Box::new(PhraseQuery::new(ts))
                    })),
                }
            }
            Node::And { items, nots } => {
                let mut clauses: Vec<(Occur, Box<dyn Query>)> = items
                    .iter()
                    .filter_map(|n| self.translate(n).map(|q| (Occur::Must, q)))
                    .collect();
                if clauses.is_empty() {
                    return Some(Box::new(BooleanQuery::new(Vec::new())));
                }
                clauses.extend(
                    nots.iter().filter_map(|n| self.translate(n).map(|q| (Occur::MustNot, q))),
                );
                Some(Box::new(BooleanQuery::new(clauses)))
            }
            Node::Or { items } => {
                let clauses: Vec<_> = items
                    .iter()
                    .filter_map(|n| self.translate(n).map(|q| (Occur::Should, q)))
                    .collect();
                Some(Box::new(BooleanQuery::new(clauses)))
            }
        }
    }

    /// The prefix in the form of the index: lower case and folded, not
    /// stemmed, so that a prefix of a stem still matches.
    fn analyze_prefix(&self, prefix: &str) -> String {
        let mut a = TextAnalyzer::builder(tantivy::tokenizer::RawTokenizer::default())
            .filter(tantivy::tokenizer::LowerCaser)
            .filter(tantivy::tokenizer::AsciiFoldingFilter)
            .build();
        let mut s = a.token_stream(prefix);
        let mut out = String::new();
        while s.advance() {
            out.push_str(&s.token().text);
        }
        out
    }
}

fn regex_escape(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if "\\.+*?()|[]{}^$#&-~".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}
