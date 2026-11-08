//! The analysis of the enhanced mode (ADR 0060, section 11): the words of a
//! text in the language of its field.
//!
//! The analyzer of a language splits the text on the characters that are
//! not letters or digits, drops the words longer than 40 bytes, puts each
//! word in lower case, drops the stop words of the language, stems each
//! word with the Snowball stemmer of the language, then folds it to ASCII.
//! A language without a stemmer keeps the words as they are after the
//! lower case, and folds them.
//!
//! The words of a text take the positions 0, 1, 2 and on, after the stop
//! words leave: a phrase matches across a stop word, as in ZCTextIndex.
//! The field of the pairs holds each two words in a row as one term.

use tantivy::tokenizer::{
    AsciiFoldingFilter, Language, LowerCaser, RawTokenizer, RemoveLongFilter, SimpleTokenizer,
    Stemmer, StopWordFilter, TextAnalyzer, TokenStream,
};

/// The separator of the two words of a pair.
pub const PAIR: char = '\u{1}';

/// The term of the pair `a`, `b`.
pub fn pair(a: &str, b: &str) -> String {
    let mut out = String::with_capacity(a.len() + b.len() + 1);
    out.push_str(a);
    out.push(PAIR);
    out.push_str(b);
    out
}

/// The tantivy language of a BCP 47 tag: its primary subtag. `None` when
/// tantivy has no stemmer for it.
pub fn language_of(tag: &str) -> Option<Language> {
    let primary = tag.split(['-', '_']).next().unwrap_or("");
    Some(match primary.to_ascii_lowercase().as_str() {
        "ar" => Language::Arabic,
        "da" => Language::Danish,
        "nl" => Language::Dutch,
        "en" => Language::English,
        "fi" => Language::Finnish,
        "fr" => Language::French,
        "de" => Language::German,
        "el" => Language::Greek,
        "hu" => Language::Hungarian,
        "it" => Language::Italian,
        "no" | "nb" | "nn" => Language::Norwegian,
        "pt" => Language::Portuguese,
        "ro" => Language::Romanian,
        "ru" => Language::Russian,
        "es" => Language::Spanish,
        "sv" => Language::Swedish,
        "ta" => Language::Tamil,
        "tr" => Language::Turkish,
        _ => return None,
    })
}

/// The analyzer of the language `tag`.
pub fn analyzer(tag: &str) -> TextAnalyzer {
    let builder = TextAnalyzer::builder(SimpleTokenizer::default())
        .filter(RemoveLongFilter::limit(40))
        .filter(LowerCaser);
    match language_of(tag) {
        Some(language) => match StopWordFilter::new(language) {
            Some(stop) => builder
                .filter(stop)
                .filter(Stemmer::new(language))
                .filter(AsciiFoldingFilter)
                .build(),
            None => builder
                .filter(Stemmer::new(language))
                .filter(AsciiFoldingFilter)
                .build(),
        },
        None => builder.filter(AsciiFoldingFilter).build(),
    }
}

/// The words of `text` through `analyzer`, in order.
pub fn words(analyzer: &mut TextAnalyzer, text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut stream = analyzer.token_stream(text);
    while stream.advance() {
        out.push(stream.token().text.clone());
    }
    out
}

/// `text` in lower case and folded to ASCII, as one word: the form of the
/// literal parts of a glob.
pub fn fold(text: &str) -> String {
    let mut analyzer = TextAnalyzer::builder(RawTokenizer::default())
        .filter(LowerCaser)
        .filter(AsciiFoldingFilter)
        .build();
    words(&mut analyzer, text).concat()
}

/// The regular expression of the glob `pattern` over the terms of a field:
/// `*` is any run of characters, `?` one character, and the literal parts
/// are folded.
pub fn glob_regex(pattern: &str) -> String {
    let mut out = String::new();
    let mut literal = String::new();
    let flush = |literal: &mut String, out: &mut String| {
        if !literal.is_empty() {
            out.push_str(&regex_escape(&fold(literal)));
            literal.clear();
        }
    };
    for c in pattern.chars() {
        match c {
            '*' => {
                flush(&mut literal, &mut out);
                out.push_str(".*");
            }
            '?' => {
                flush(&mut literal, &mut out);
                out.push('.');
            }
            c => literal.push(c),
        }
    }
    flush(&mut literal, &mut out);
    out
}

fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if "\\.+*?()|[]{}^$#&-~\"<>@".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Tell if `word` matches the glob `pattern` after the fold of its
/// literal parts: the check of the text overlay.
pub fn glob_matches(pattern: &[GlobPart], word: &str) -> bool {
    let w: Vec<char> = word.chars().collect();
    fn go(p: &[GlobPart], w: &[char]) -> bool {
        match p.split_first() {
            None => w.is_empty(),
            Some((GlobPart::Any, rest)) => (0..=w.len()).any(|i| go(rest, &w[i..])),
            Some((GlobPart::One, rest)) => !w.is_empty() && go(rest, &w[1..]),
            Some((GlobPart::Lit(l), rest)) => {
                w.len() >= l.len() && w[..l.len()] == l[..] && go(rest, &w[l.len()..])
            }
        }
    }
    go(pattern, &w)
}

/// A part of a glob.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GlobPart {
    /// `*`.
    Any,
    /// `?`.
    One,
    /// Folded literal characters.
    Lit(Vec<char>),
}

/// The parts of the glob `pattern`.
pub fn glob_parts(pattern: &str) -> Vec<GlobPart> {
    let mut out = Vec::new();
    let mut literal = String::new();
    let flush = |literal: &mut String, out: &mut Vec<GlobPart>| {
        if !literal.is_empty() {
            out.push(GlobPart::Lit(fold(literal).chars().collect()));
            literal.clear();
        }
    };
    for c in pattern.chars() {
        match c {
            '*' => {
                flush(&mut literal, &mut out);
                out.push(GlobPart::Any);
            }
            '?' => {
                flush(&mut literal, &mut out);
                out.push(GlobPart::One);
            }
            c => literal.push(c),
        }
    }
    flush(&mut literal, &mut out);
    out
}

/// The edit distance of `a` and `b`, with a transposition of two
/// neighbours as one edit: the distance of the fuzzy matching of tantivy.
pub fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (n, m) = (a.len(), b.len());
    let mut d = vec![vec![0usize; m + 1]; n + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in d[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=n {
        for j in 1..=m {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut v = (d[i - 1][j] + 1)
                .min(d[i][j - 1] + 1)
                .min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                v = v.min(d[i - 2][j - 2] + 1);
            }
            d[i][j] = v;
        }
    }
    d[n][m]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_english_analyzer_stems_folds_and_drops_stop_words() {
        let mut a = analyzer("en");
        assert_eq!(
            words(&mut a, "The Running cafés of the city"),
            ["run", "cafe", "citi"]
        );
    }

    #[test]
    fn a_glob_folds_its_literal_parts() {
        assert_eq!(glob_regex("Caf*"), "caf.*");
        assert_eq!(glob_regex("c?fé*"), "c.fe.*");
        let parts = glob_parts("Caf*");
        assert!(glob_matches(&parts, "cafe"));
        assert!(!glob_matches(&parts, "coffee"));
    }

    #[test]
    fn a_transposition_is_one_edit() {
        assert_eq!(edit_distance("form", "from"), 1);
        assert_eq!(edit_distance("kitten", "sitting"), 3);
    }
}
