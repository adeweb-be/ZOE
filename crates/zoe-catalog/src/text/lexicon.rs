//! The words of the lexicon `plone_lexicon` of Plone 6.1 (ADR 0060,
//! section 11): the compatibility mode indexes them.
//!
//! The lexicon has two elements:
//!
//! - The splitter "Unicode Whitespace splitter" of Products.CMFPlone. It
//!   puts the text in NFKC, takes each run of the characters of `\w` of
//!   Python's `re`, and cuts each run into parts of one class: Hangul, the
//!   CJK and kana block, Thai, and the other characters. A part of Hangul,
//!   CJK, kana or Thai gives its pairs of characters in a row, and its last
//!   character alone. Another part is one word.
//! - The normalizer "Unicode Ignoring Accents Case Normalizer". A word
//!   whose first code point is in U+0041 to U+024F goes through
//!   `baseNormalize` of plone.i18n: the ASCII of Unidecode, without the
//!   white space at its ends, and only its letters, digits, punctuation and
//!   white space. Then each word is in lower case.
//!
//! The tables of `plone_table.rs` give `\w` and the ASCII of Unidecode
//! 1.4.0, the pin of Plone 6.1. Its generator, `plone_table.py`, makes
//! them again.
//!
//! A glob of a query follows `Lexicon.globToWordIds`: the characters
//! before the first `*` or `?` are the prefix, which a word must start
//! with. The rest is a pattern of `*`, `?` and literal characters. The
//! prefix goes into the regular expression of ZCTextIndex as it is, so a
//! prefix with a character that a regular expression gives a meaning to
//! matches as that expression matches.

use std::cmp::Ordering;

use unicode_normalization::UnicodeNormalization;

use super::analysis::GlobChar;
use super::plone_table::{SECTIONS, WORD};

/// Tell if `c` matches `\w` of Python's `re` in a str pattern.
pub fn is_word(c: char) -> bool {
    if c.is_ascii() {
        return c.is_ascii_alphanumeric() || c == '_';
    }
    let cp = u32::from(c);
    WORD.binary_search_by(|&(lo, hi)| {
        if hi < cp {
            Ordering::Less
        } else if lo > cp {
            Ordering::Greater
        } else {
            Ordering::Equal
        }
    })
    .is_ok()
}

/// The class of `c` in the splitter: 0 for the other characters, 1 for
/// Hangul, 2 for CJK and kana, 3 for Thai.
fn class(c: char) -> u8 {
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

/// The ASCII of Unidecode for `c`: empty when it has none.
pub fn ascii_of(c: char) -> &'static str {
    let cp = u32::from(c);
    if cp > 0xEFFFF {
        return "";
    }
    match SECTIONS.binary_search_by_key(&(cp >> 8), |s| s.0) {
        Ok(i) => SECTIONS[i].1[(cp & 0xFF) as usize],
        Err(_) => "",
    }
}

/// Tell if Python's `str.strip()` removes the ASCII character `c`.
fn is_python_space(c: char) -> bool {
    matches!(
        c,
        ' ' | '\t' | '\n' | '\r' | '\x0b' | '\x0c' | '\x1c' | '\x1d' | '\x1e' | '\x1f'
    )
}

/// Tell if `baseNormalize` keeps the ASCII character `c`: a letter, a
/// digit, punctuation or white space.
fn is_allowed(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || c.is_ascii_punctuation()
        || matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0b' | '\x0c')
}

/// `baseNormalize` of plone.i18n: the ASCII of `word`, stripped, with the
/// characters that it keeps.
fn base_normalize(word: &str) -> String {
    let mut ascii = String::with_capacity(word.len());
    for c in word.chars() {
        if c.is_ascii() {
            ascii.push(c);
        } else {
            ascii.push_str(ascii_of(c));
        }
    }
    ascii
        .trim_matches(is_python_space)
        .chars()
        .filter(|c| is_allowed(*c))
        .collect()
}

/// The normalizer of the lexicon for one word: `baseNormalize` for a word
/// that starts with a code point in U+0041 to U+024F, then the lower case.
pub fn normalize(word: &str) -> String {
    if word.is_ascii() {
        // `baseNormalize` keeps an ASCII word of `\w` as it is.
        return word.to_ascii_lowercase();
    }
    let Some(first) = word.chars().next() else {
        return String::new();
    };
    if (0x41..=0x24F).contains(&u32::from(first)) {
        base_normalize(word).to_lowercase()
    } else {
        word.to_lowercase()
    }
}

/// The words of the splitter of the lexicon in `text`, before the
/// normalizer.
fn split(text: &str, out: &mut Vec<String>) {
    let normalized: std::borrow::Cow<'_, str> = if text.is_ascii() {
        std::borrow::Cow::Borrowed(text)
    } else {
        std::borrow::Cow::Owned(text.nfkc().collect())
    };
    let mut run: Vec<char> = Vec::new();
    let mut flush = |run: &mut Vec<char>| {
        let mut start = 0;
        while start < run.len() {
            let kind = class(run[start]);
            let mut end = start + 1;
            while end < run.len() && class(run[end]) == kind {
                end += 1;
            }
            let part = &run[start..end];
            if kind == 0 {
                out.push(part.iter().collect());
            } else {
                for i in 0..part.len() {
                    out.push(part[i..(i + 2).min(part.len())].iter().collect());
                }
            }
            start = end;
        }
        run.clear();
    };
    for c in normalized.chars() {
        if is_word(c) {
            run.push(c);
        } else if !run.is_empty() {
            flush(&mut run);
        }
    }
    if !run.is_empty() {
        flush(&mut run);
    }
}

/// The words of `text` as the lexicon `plone_lexicon` gives them to
/// ZCTextIndex when it indexes `text`.
pub fn words(text: &str) -> Vec<String> {
    let mut parts = Vec::new();
    split(text, &mut parts);
    parts.iter().map(|w| normalize(w)).collect()
}

/// The words of a glob of `Lexicon.globToWordIds`, for one pattern after
/// the lexicon.
#[derive(Clone, Debug)]
pub enum Glob {
    /// A pattern with no `*` and no `?`: this word.
    Word(String),
    /// The words that start with the prefix and whose rest matches.
    Match {
        /// The characters before the first `*` or `?`.
        prefix: String,
        /// The rest, when the prefix holds no character that a regular
        /// expression gives a meaning to.
        rest: Option<Vec<GlobChar>>,
        /// The expression of the pattern otherwise. `None` when it does
        /// not compile: no word matches.
        regex: Option<regex::Regex>,
    },
    /// A pattern that starts with `*` or `?`: ZCTextIndex refuses it, and
    /// no word matches.
    Refused,
}

/// The characters that a regular expression of Python gives a meaning to,
/// outside a class.
fn is_meta(c: char) -> bool {
    matches!(
        c,
        '.' | '^' | '$' | '*' | '+' | '?' | '{' | '}' | '[' | ']' | '\\' | '|' | '(' | ')'
    )
}

impl Glob {
    /// The glob of the pattern `p` of a text clause: `p` through the
    /// normalizer of the lexicon, then as `Lexicon.globToWordIds` reads it.
    pub fn of(p: &str) -> Glob {
        let pattern = normalize(p);
        let cut = pattern.find(['*', '?']);
        let Some(cut) = cut else {
            return Glob::Word(pattern);
        };
        if cut == 0 {
            return Glob::Refused;
        }
        let (prefix, rest) = pattern.split_at(cut);
        let chars: Vec<GlobChar> = rest
            .chars()
            .map(|c| match c {
                '*' => GlobChar::Any,
                '?' => GlobChar::One,
                c => GlobChar::Char(c),
            })
            .collect();
        if !prefix.chars().any(is_meta) {
            return Glob::Match {
                prefix: prefix.to_owned(),
                rest: Some(chars),
                regex: None,
            };
        }
        let mut expr = String::from("^(?:");
        expr.push_str(prefix);
        for c in &chars {
            match c {
                GlobChar::Any => expr.push_str(".*"),
                GlobChar::One => expr.push('.'),
                GlobChar::Char(c) => expr.push_str(&regex::escape(&c.to_string())),
            }
        }
        expr.push_str("$)");
        Glob::Match {
            prefix: prefix.to_owned(),
            rest: None,
            regex: regex::Regex::new(&expr).ok(),
        }
    }

    /// Tell if the word `word` of the index matches.
    pub fn matches(&self, word: &str) -> bool {
        match self {
            Glob::Word(w) => w == word,
            Glob::Refused => false,
            Glob::Match {
                prefix,
                rest,
                regex,
            } => {
                let Some(tail) = word.strip_prefix(prefix.as_str()) else {
                    return false;
                };
                match (rest, regex) {
                    (Some(rest), _) => super::analysis::glob_matches(rest, tail),
                    (None, Some(regex)) => regex.is_match(word),
                    (None, None) => false,
                }
            }
        }
    }

    /// The prefix that each word of the glob starts with.
    #[cfg(test)]
    pub fn prefix(&self) -> &str {
        match self {
            Glob::Word(w) => w,
            Glob::Match { prefix, .. } => prefix,
            Glob::Refused => "",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_splitter_takes_the_runs_of_word_characters() {
        assert_eq!(
            words("Hello, wörld! foo_bar 42 Ünïcode-Text"),
            ["hello", "world", "foo_bar", "42", "unicode", "text"]
        );
    }

    #[test]
    fn a_word_that_starts_with_a_digit_keeps_its_accents() {
        assert_eq!(words("2café"), ["2café"]);
    }

    #[test]
    fn cjk_hangul_and_thai_give_pairs_and_the_last_character() {
        assert_eq!(words("日本語"), ["日本", "本語", "語"]);
        assert_eq!(words("ab日本"), ["ab", "日本", "本"]);
        assert_eq!(words("한국"), ["한국", "국"]);
    }

    #[test]
    fn a_combining_mark_that_does_not_compose_cuts_a_word() {
        // U+0331 has no composed form with `a`, and is not `\w`.
        assert_eq!(words("xa\u{331}y"), ["xa", "y"]);
        // NFKC composes `e` and U+0301.
        assert_eq!(words("cafe\u{301}"), ["cafe"]);
    }

    #[test]
    fn compatibility_characters_go_through_nfkc() {
        assert_eq!(words("ﬁne ＡＢＣ ①"), ["fine", "abc", "1"]);
    }

    #[test]
    fn a_latin_word_goes_through_the_ascii_of_unidecode() {
        assert_eq!(words("Ærøskøbing"), ["aeroskobing"]);
        // Greek in a word that starts with a Latin letter.
        assert_eq!(words("aβγ"), ["abg"]);
        // A Greek word keeps its letters, in lower case.
        assert_eq!(words("ΑΒΓ"), ["αβγ"]);
    }

    #[test]
    fn a_glob_reads_its_prefix_and_its_rest() {
        let g = Glob::of("Café*");
        assert_eq!(g.prefix(), "cafe");
        assert!(g.matches("cafe"));
        assert!(g.matches("cafeteria"));
        assert!(!g.matches("caf"));
        let g = Glob::of("ca?e");
        assert!(g.matches("cafe"));
        assert!(!g.matches("cafes"));
        assert!(matches!(Glob::of("*a"), Glob::Refused));
        assert!(matches!(Glob::of("word"), Glob::Word(w) if w == "word"));
    }

    #[test]
    fn a_prefix_with_a_meta_character_matches_as_its_expression() {
        // U+01C0 is `|` in the ASCII of Unidecode: the expression of
        // ZCTextIndex is `a|b.$`, which matches each word that starts with
        // `a`.
        let g = Glob::of("a\u{1c0}b?");
        assert_eq!(g.prefix(), "a|b");
        assert!(g.matches("a|bxyz"));
        // U+028C is `^`: `a^` never matches.
        let g = Glob::of("a\u{28c}*");
        assert!(!g.matches("a^b"));
    }
}
