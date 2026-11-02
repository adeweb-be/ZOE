//! The tantivy index of one catalog: its schema, its writer and its reader.
//!
//! One document holds the rid and, for each text index, a field. In the
//! compatibility mode the field holds the words of `plone_words`, and a
//! fast field `n_<index>` holds the number of words plus one, or 0 when the
//! document has no text in that index. The Okapi score of ZCTextIndex needs
//! that exact length: the norms of tantivy round it. In the enhanced mode
//! the field holds the text through the enhanced analyzer.

use std::path::Path;

use tantivy::schema::{
    FAST, Field, INDEXED, IndexRecordOption, Schema, TextFieldIndexing, TextOptions,
};
use tantivy::tokenizer::{Language, PreTokenizedString, TokenStream};
use tantivy::{Index, IndexReader, IndexWriter, ReloadPolicy, TantivyDocument, Term};

use super::analysis::{self, ENHANCED, PLONE, PloneTokenizer, plone_words};

/// The text indexes of the stock catalog of Plone.
pub const INDEXES: [&str; 3] = ["SearchableText", "Title", "Description"];

/// The mode of an index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// The words and the Okapi score of ZCTextIndex.
    Compat,
    /// Stemming, folding, stop words and the BM25 of tantivy.
    Enhanced,
}

/// The fields of the schema.
#[derive(Clone, Debug)]
pub struct Fields {
    /// The rid.
    pub rid: Field,
    /// The text field of each index of `INDEXES`.
    pub text: Vec<Field>,
    /// The length field of each index (compatibility mode).
    pub len: Vec<Field>,
    /// The effective date in minutes, for the sort of the measures.
    pub effective: Field,
    /// The field of the pairs of words in a row of each index: a phrase of
    /// two words is one term.
    pub pairs: Vec<Field>,
}

/// The separator of the two words of a pair.
pub const PAIR: char = '\u{1}';

/// Return the term of the pair `a`, `b`.
pub fn pair(a: &str, b: &str) -> String {
    format!("{a}{PAIR}{b}")
}

/// Return the schema of `mode` and its fields.
pub fn schema(mode: Mode) -> (Schema, Fields) {
    let mut b = Schema::builder();
    let rid = b.add_u64_field("rid", INDEXED | FAST);
    let tokenizer = match mode {
        Mode::Compat => PLONE,
        Mode::Enhanced => ENHANCED,
    };
    let opts = TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer(tokenizer)
            .set_index_option(IndexRecordOption::WithFreqsAndPositions),
    );
    let text = INDEXES.iter().map(|n| b.add_text_field(n, opts.clone())).collect();
    let len = INDEXES.iter().map(|n| b.add_u64_field(&format!("n_{n}"), FAST)).collect();
    let effective = b.add_i64_field("effective", FAST);
    let popts = TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer(tokenizer)
            .set_index_option(IndexRecordOption::WithFreqs),
    );
    let pairs = INDEXES.iter().map(|n| b.add_text_field(&format!("p_{n}"), popts.clone())).collect();
    (b.build(), Fields { rid, text, len, effective, pairs })
}

/// Register the tokenizers on `index`.
pub fn register(index: &Index) {
    register_on(index.tokenizers());
}

/// Register the tokenizers on `manager`.
pub fn register_on(manager: &tantivy::tokenizer::TokenizerManager) {
    manager.register(PLONE, analysis_plone());
    manager.register(ENHANCED, analysis::enhanced(Language::English));
}

fn analysis_plone() -> tantivy::tokenizer::TextAnalyzer {
    tantivy::tokenizer::TextAnalyzer::builder(PloneTokenizer).build()
}

/// Create an index of `mode` in `dir`, or in memory when `dir` is `None`.
pub fn create(mode: Mode, dir: Option<&Path>) -> tantivy::Result<(Index, Fields)> {
    let (schema, fields) = schema(mode);
    let index = match dir {
        Some(d) => Index::create_in_dir(d, schema)?,
        None => Index::create_in_ram(schema),
    };
    register(&index);
    Ok((index, fields))
}

/// Open the index of `mode` in `dir`.
pub fn open(mode: Mode, dir: &Path) -> tantivy::Result<(Index, Fields)> {
    let index = Index::open_in_dir(dir)?;
    register(&index);
    let (_, fields) = schema(mode);
    Ok((index, fields))
}

/// A reader that reloads only on request: it starts no thread.
pub fn reader(index: &Index) -> tantivy::Result<IndexReader> {
    index.reader_builder().reload_policy(ReloadPolicy::Manual).try_into()
}

/// One document of the corpus.
#[derive(Clone, Debug, Default)]
pub struct Doc {
    /// The rid.
    pub rid: u64,
    /// The text of each index of `INDEXES`, `None` when absent.
    pub texts: [Option<String>; 3],
    /// The effective date in minutes.
    pub effective: i64,
}

/// Return the tantivy document of `doc` in `mode`.
pub fn document(mode: Mode, fields: &Fields, doc: &Doc) -> TantivyDocument {
    let mut d = TantivyDocument::default();
    d.add_u64(fields.rid, doc.rid);
    d.add_i64(fields.effective, doc.effective);
    for (i, text) in doc.texts.iter().enumerate() {
        let Some(text) = text else {
            d.add_u64(fields.len[i], 0);
            continue;
        };
        match mode {
            Mode::Compat => {
                let words = plone_words(text);
                d.add_u64(fields.len[i], words.len() as u64 + 1);
                add_pairs(&mut d, fields.pairs[i], words.iter().map(|w| (w.as_str(), 0)).enumerate().map(|(p, (w, _))| (w, p)));
                let tokens = analysis::WordStream::new(&words).into_tokens();
                d.add_pre_tokenized_text(
                    fields.text[i],
                    PreTokenizedString { text: String::new(), tokens },
                );
            }
            Mode::Enhanced => {
                d.add_u64(fields.len[i], 1);
                let tokens = ENHANCED_ANALYZER.with(|a| {
                    let mut a = a.borrow_mut();
                    let mut stream = a.token_stream(text);
                    let mut out = Vec::new();
                    while stream.advance() {
                        out.push(stream.token().clone());
                    }
                    out
                });
                add_pairs(&mut d, fields.pairs[i], tokens.iter().map(|t| (t.text.as_str(), t.position)));
                d.add_pre_tokenized_text(
                    fields.text[i],
                    PreTokenizedString { text: String::new(), tokens },
                );
            }
        }
    }
    d
}

thread_local! {
    static ENHANCED_ANALYZER: std::cell::RefCell<tantivy::tokenizer::TextAnalyzer> =
        std::cell::RefCell::new(analysis::enhanced(Language::English));
}

/// Add the pairs of the words in a row of `words`, (word, position), to
/// `field`. Two words with a gap between them, a stop word, make no pair.
fn add_pairs<'a>(d: &mut TantivyDocument, field: Field, words: impl Iterator<Item = (&'a str, usize)>) {
    let mut tokens = Vec::new();
    let mut prev: Option<(&str, usize)> = None;
    for (w, p) in words {
        if let Some((a, q)) = prev
            && q + 1 == p
        {
            tokens.push(tantivy::tokenizer::Token {
                offset_from: 0,
                offset_to: 0,
                position: tokens.len(),
                text: pair(a, w),
                position_length: 1,
            });
        }
        prev = Some((w, p));
    }
    d.add_pre_tokenized_text(field, PreTokenizedString { text: String::new(), tokens });
}

/// Replace the document of `doc.rid` in the writer: a delete of the rid,
/// then an add. `None` texts in every index is an unindex.
pub fn upsert(writer: &IndexWriter, mode: Mode, fields: &Fields, doc: &Doc) -> tantivy::Result<()> {
    writer.delete_term(Term::from_field_u64(fields.rid, doc.rid));
    if doc.texts.iter().any(Option::is_some) {
        writer.add_document(document(mode, fields, doc))?;
    }
    Ok(())
}

/// Read one line of the corpus.
pub fn parse_line(line: &str) -> Option<(Doc, serde_json::Value)> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let rid = v.get("rid")?.as_u64()?;
    let text = |k: &str| v.get(k).and_then(|t| t.as_str()).map(str::to_owned);
    let doc = Doc {
        rid,
        texts: [text(INDEXES[0]), text(INDEXES[1]), text(INDEXES[2])],
        effective: v.get("effective").and_then(serde_json::Value::as_i64).unwrap_or(0),
    };
    Some((doc, v))
}

/// The size of the files of `dir`, in bytes.
pub fn dir_size(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .map(|it| it.filter_map(|e| e.ok()?.metadata().ok()).map(|m| m.len()).sum())
        .unwrap_or(0)
}
