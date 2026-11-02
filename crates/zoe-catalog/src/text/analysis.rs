//! The analyzers of the text index.
//!
//! `plone_words` gives the words of the lexicon `plone_lexicon` of a Plone
//! 6.1 site: the splitter "Unicode Whitespace splitter" and the normalizer
//! "Unicode Ignoring Accents Case Normalizer" of `Products.CMFPlone`. The
//! compatibility mode indexes these words. The enhanced mode uses a tantivy
//! analyzer with folding, stop words and a Snowball stemmer.

use tantivy::tokenizer::{
    AsciiFoldingFilter, Language, LowerCaser, RemoveLongFilter, SimpleTokenizer, Stemmer,
    StopWordFilter, TextAnalyzer, Token, TokenStream, Tokenizer,
};
use unicode_normalization::UnicodeNormalization;
use unicode_properties::{GeneralCategoryGroup, UnicodeGeneralCategory};

/// Tell if `c` is in the CJK, Hangul or Thai ranges of the splitter. The
/// splitter cuts these runs into bigrams.
fn is_bigram_script(c: char) -> bool {
    matches!(c,
        '\u{AC00}'..='\u{D7AF}'
        | '\u{3040}'..='\u{30FF}' | '\u{4E00}'..='\u{9FFF}' | '\u{3400}'..='\u{4DBF}'
        | '\u{F900}'..='\u{FAFF}'
        | '\u{0E00}'..='\u{0E7F}')
}

/// The script class of a bigram character: the splitter keeps runs of one
/// class together.
fn script_class(c: char) -> u8 {
    match c {
        '\u{AC00}'..='\u{D7AF}' => 1,
        '\u{3040}'..='\u{30FF}'
        | '\u{4E00}'..='\u{9FFF}'
        | '\u{3400}'..='\u{4DBF}'
        | '\u{F900}'..='\u{FAFF}' => 2,
        '\u{0E00}'..='\u{0E7F}' => 3,
        _ => 0,
    }
}

/// Tell if `c` matches `\w` of the `re` module of Python for a str
/// pattern: a letter, a number, or the underscore.
pub fn is_python_word(c: char) -> bool {
    if c == '_' {
        return true;
    }
    matches!(
        c.general_category_group(),
        GeneralCategoryGroup::Letter | GeneralCategoryGroup::Number
    )
}

/// The printable ASCII that `baseNormalize` keeps: letters, digits,
/// punctuation and whitespace.
fn is_allowed(c: char) -> bool {
    c.is_ascii_alphanumeric() || c.is_ascii_punctuation() || matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0b' | '\x0c')
}

/// The table of the transliteration, one string for each character of
/// the basic plane, as Python's Unidecode gives it. Read from the JSON file
/// that `ZOE_UNIDECODE_TABLE` names; without it, the `unidecode` crate.
fn table() -> Option<&'static Vec<String>> {
    static TABLE: std::sync::OnceLock<Option<Vec<String>>> = std::sync::OnceLock::new();
    TABLE
        .get_or_init(|| {
            let path = std::env::var("ZOE_UNIDECODE_TABLE").ok()?;
            serde_json::from_slice(&std::fs::read(path).ok()?).ok()
        })
        .as_ref()
}

/// Transliterate `word` to ASCII.
fn transliterate(word: &str) -> String {
    match table() {
        Some(t) => word
            .chars()
            .map(|c| t.get(c as usize).map_or("", String::as_str))
            .collect(),
        None => unidecode::unidecode(word),
    }
}

/// The normalizer of Plone: a word that starts with a Latin character goes
/// through `unidecode` and keeps only printable ASCII; every word is then
/// in lower case.
fn normalize(word: &str) -> String {
    let first = word.chars().next().map_or(0, |c| c as u32);
    if (0x41..=0x24F).contains(&first) {
        let ascii = transliterate(word);
        let kept: String = ascii.trim().chars().filter(|c| is_allowed(*c)).collect();
        kept.to_lowercase()
    } else {
        word.to_lowercase()
    }
}

/// Return the words of `text` as the lexicon `plone_lexicon` gives them.
pub fn plone_words(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let normalized: String = text.nfkc().collect();
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut Vec<String>| {
        if run.is_empty() {
            return;
        }
        // Cut the run into segments of one script class.
        let chars: Vec<char> = run.chars().collect();
        let mut start = 0;
        while start < chars.len() {
            let class = script_class(chars[start]);
            let mut end = start + 1;
            while end < chars.len() && script_class(chars[end]) == class {
                end += 1;
            }
            let seg = &chars[start..end];
            if is_bigram_script(seg[0]) {
                for i in 0..seg.len() {
                    let pair: String = seg[i..(i + 2).min(seg.len())].iter().collect();
                    out.push(normalize(&pair));
                }
            } else {
                out.push(normalize(&seg.iter().collect::<String>()));
            }
            start = end;
        }
        run.clear();
    };
    for c in normalized.chars() {
        if is_python_word(c) {
            run.push(c);
        } else {
            flush(&mut run, &mut out);
        }
    }
    flush(&mut run, &mut out);
    out
}

/// A token stream over words that were split in advance.
pub struct WordStream {
    tokens: Vec<Token>,
    next: usize,
}

impl WordStream {
    /// Make a stream of `words`, at positions 0, 1, 2 and on.
    pub fn new(words: &[String]) -> Self {
        let tokens = words
            .iter()
            .enumerate()
            .map(|(position, w)| Token {
                offset_from: 0,
                offset_to: 0,
                position,
                text: w.clone(),
                position_length: 1,
            })
            .collect();
        Self { tokens, next: 0 }
    }

    /// Return the tokens.
    pub fn into_tokens(self) -> Vec<Token> {
        self.tokens
    }
}

impl TokenStream for WordStream {
    fn advance(&mut self) -> bool {
        if self.next < self.tokens.len() {
            self.next += 1;
            true
        } else {
            false
        }
    }
    fn token(&self) -> &Token {
        &self.tokens[self.next - 1]
    }
    fn token_mut(&mut self) -> &mut Token {
        &mut self.tokens[self.next - 1]
    }
}

/// The tokenizer of the compatibility mode: the words of `plone_words`.
#[derive(Clone, Default)]
pub struct PloneTokenizer;

impl Tokenizer for PloneTokenizer {
    type TokenStream<'a> = WordStream;
    fn token_stream<'a>(&'a mut self, text: &'a str) -> WordStream {
        WordStream::new(&plone_words(text))
    }
}

/// The analyzer of the enhanced mode for `language`.
pub fn enhanced(language: Language) -> TextAnalyzer {
    let builder = TextAnalyzer::builder(SimpleTokenizer::default())
        .filter(RemoveLongFilter::limit(40))
        .filter(LowerCaser)
        .filter(AsciiFoldingFilter);
    match StopWordFilter::new(language) {
        Some(stop) => builder.filter(stop).filter(Stemmer::new(language)).build(),
        None => builder.filter(Stemmer::new(language)).build(),
    }
}

/// The name of the tokenizer of the compatibility mode.
pub const PLONE: &str = "plone";
/// The name of the tokenizer of the enhanced mode.
pub const ENHANCED: &str = "enhanced";
