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

/// The most characters of a glob. A word of the index holds at most 39
/// bytes, so a longer pattern finds nothing that this one misses.
pub const MAX_GLOB: usize = 255;

/// One position of a glob: a character, `?` or `*`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GlobChar {
    /// A literal character.
    Char(char),
    /// `?`: one character.
    One,
    /// `*`: any run of characters.
    Any,
}

/// A part of a pattern before its analysis: a run of literal characters,
/// `?` or `*`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GlobPart {
    /// `*`.
    Any,
    /// `?`.
    One,
    /// Literal characters, as the user wrote them.
    Lit(String),
}

/// The parts of the glob `pattern`. A run of `*` is one part.
pub fn glob_parts(pattern: &str) -> Vec<GlobPart> {
    let mut out = Vec::new();
    let mut literal = String::new();
    for c in pattern.chars() {
        match c {
            '*' | '?' => {
                if !literal.is_empty() {
                    out.push(GlobPart::Lit(std::mem::take(&mut literal)));
                }
                if c == '?' {
                    out.push(GlobPart::One);
                } else if out.last() != Some(&GlobPart::Any) {
                    out.push(GlobPart::Any);
                }
            }
            c => literal.push(c),
        }
    }
    if !literal.is_empty() {
        out.push(GlobPart::Lit(literal));
    }
    out
}

/// The positions of `parts`, each literal part through `literal`.
pub fn glob_chars(parts: &[GlobPart], mut literal: impl FnMut(&str) -> String) -> Vec<GlobChar> {
    let mut out = Vec::new();
    for part in parts {
        match part {
            GlobPart::Any => {
                if out.last() != Some(&GlobChar::Any) {
                    out.push(GlobChar::Any);
                }
            }
            GlobPart::One => out.push(GlobChar::One),
            GlobPart::Lit(text) => out.extend(literal(text).chars().map(GlobChar::Char)),
        }
    }
    out
}

/// The regular expression of the positions `pattern` over the terms of a
/// field.
pub fn glob_regex(pattern: &[GlobChar]) -> String {
    let mut out = String::new();
    for p in pattern {
        match p {
            GlobChar::Any => out.push_str(".*"),
            GlobChar::One => out.push('.'),
            GlobChar::Char(c) => {
                if "\\.+*?()|[]{}^$#&-~\"<>@".contains(*c) {
                    out.push('\\');
                }
                out.push(*c);
            }
        }
    }
    out
}

/// Tell if `word` matches the positions `pattern`. The match is
/// iterative: two cursors and the place of the last `*`, so its time is at
/// most the product of the two lengths, and it uses no recursion.
pub fn glob_matches(pattern: &[GlobChar], word: &str) -> bool {
    let w: Vec<char> = word.chars().collect();
    let (mut p, mut i) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while i < w.len() {
        match pattern.get(p) {
            Some(GlobChar::One) => {
                p += 1;
                i += 1;
            }
            Some(GlobChar::Char(c)) if *c == w[i] => {
                p += 1;
                i += 1;
            }
            Some(GlobChar::Any) => {
                star = Some((p, i));
                p += 1;
            }
            _ => match star {
                Some((s, mark)) => {
                    star = Some((s, mark + 1));
                    p = s + 1;
                    i = mark + 1;
                }
                None => return false,
            },
        }
    }
    pattern[p..].iter().all(|c| *c == GlobChar::Any)
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

    fn folded(pattern: &str) -> Vec<GlobChar> {
        glob_chars(&glob_parts(pattern), fold)
    }

    #[test]
    fn a_glob_folds_its_literal_parts() {
        assert_eq!(glob_regex(&folded("Caf*")), "caf.*");
        assert_eq!(glob_regex(&folded("c?fé**")), "c.fe.*");
        let caf = folded("Caf*");
        assert!(glob_matches(&caf, "cafe"));
        assert!(!glob_matches(&caf, "coffee"));
        assert!(glob_matches(&folded("*a?e"), "cafe"));
        assert!(!glob_matches(&folded("*a?e"), "cafes"));
    }

    #[test]
    fn a_run_of_stars_is_one_star() {
        let pattern = format!("a{}z", "*".repeat(200_000));
        let parts = glob_parts(&pattern);
        assert_eq!(parts.len(), 3);
        let started = std::time::Instant::now();
        assert!(glob_matches(&folded(&pattern), "abcz"));
        assert!(!glob_matches(&folded(&pattern), "abcy"));
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn a_pattern_of_many_stars_matches_in_bounded_time() {
        // `*e` ten and more times, then `z`, against a long word of `e`:
        // a backtracking match takes exponential time on it.
        let pattern = format!("{}z", "*e".repeat(120));
        let word = "e".repeat(39);
        let started = std::time::Instant::now();
        for _ in 0..100 {
            assert!(!glob_matches(&folded(&pattern), &word));
        }
        assert!(glob_matches(
            &folded(&pattern),
            &format!("{}z", "e".repeat(200))
        ));
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn a_transposition_is_one_edit() {
        assert_eq!(edit_distance("form", "from"), 1);
        assert_eq!(edit_distance("kitten", "sitting"), 3);
    }
}
