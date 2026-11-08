//! The text clause and its codec (ADR 0060, section 12), and the replies of
//! `text_query` and `text_info`.
//!
//! ```text
//! clause   = {"k": "t", "i": index, "q": node, "lg": [language...] | nil,
//!             "cl": [index...], "st": [staged...], "set": settings | nil}
//! settings = {"mode": "enhanced" | "compat", "language": [language...],
//!             "boosts": {index: float}}
//! node     = {"n": "and", "items": [node...], "nots": [node...]}
//!          | {"n": "or", "items": [node...]}
//!          | {"n": "atom", "w": [word...]}
//!          | {"n": "phrase", "w": [word...]}
//!          | {"n": "glob", "p": pattern}
//! staged   = [index, rid, language, text | nil]
//! ```
//!
//! A clause of `catalog_query` has `"k": "t"`; the clause of `text_query`
//! has no `k`. A clause that does not have this form is an error: the
//! caller raises `ValueError`. Each rid is a signed 32-bit int
//! (`zoe_core::check_rid`).

use crate::plan::Wire;

use super::settings::{IndexSettings, Mode, check_boost, check_index, check_language};

/// The deepest tree of nodes that a clause holds.
pub const MAX_DEPTH: usize = 64;

/// The most msgpack values of one call (ADR 0059, section 8).
pub const MAX_VALUES: usize = 50_000;

/// The most bytes of one call, and of one reply.
pub const MAX_BYTES: usize = 16 << 20;

/// A node of the parse tree of ZCTextIndex.
#[derive(Clone, Debug, PartialEq)]
pub enum Node {
    /// Every item, and none of the nots.
    And {
        /// The items.
        items: Vec<Node>,
        /// The items of `NOT`.
        nots: Vec<Node>,
    },
    /// Any item.
    Or {
        /// The items.
        items: Vec<Node>,
    },
    /// Any of the words: the words of one term after the lexicon.
    Atom {
        /// The words.
        words: Vec<String>,
    },
    /// The words in a row.
    Phrase {
        /// The words.
        words: Vec<String>,
    },
    /// The words that match a pattern of `*` and `?`, as the user wrote
    /// it.
    Glob {
        /// The pattern.
        pattern: String,
    },
}

/// A text that the transaction of the query staged: it replaces the
/// document of its key for this query only.
#[derive(Clone, Debug, PartialEq)]
pub struct Staged {
    /// The index id.
    pub index: String,
    /// The rid.
    pub rid: i32,
    /// The language.
    pub language: String,
    /// The text, `None` for an unindex.
    pub text: Option<String>,
}

/// A text clause.
#[derive(Clone, Debug, PartialEq)]
pub struct TextClause {
    /// The text index of the clause.
    pub index: String,
    /// The parse tree.
    pub node: Node,
    /// The languages whose fields the query reads, `None` for every field.
    pub languages: Option<Vec<String>>,
    /// The indexes that the transaction cleared: the answer for them comes
    /// from the staged texts alone.
    pub cleared: Vec<String>,
    /// The staged texts.
    pub staged: Vec<Staged>,
    /// The settings of the index, on a test storage; `None` reads
    /// `zoe.text`.
    pub settings: Option<IndexSettings>,
}

/// The errors of a text clause on the wire (`e`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TextErrorKind {
    /// The catalog has no index, or its open or a rebuild runs past
    /// `catalog-wait`.
    Unavailable,
    /// The snapshot is at or before `pack_watermark`.
    Packed,
    /// The snapshot is before a clear delta that the index holds.
    Cleared,
    /// A delta fails its checksum.
    Corrupt,
    /// The reply passes the cap of a reply.
    TooLarge,
}

impl TextErrorKind {
    /// The name on the wire.
    pub fn name(self) -> &'static str {
        match self {
            TextErrorKind::Unavailable => "unavailable",
            TextErrorKind::Packed => "packed",
            TextErrorKind::Cleared => "cleared",
            TextErrorKind::Corrupt => "corrupt",
            TextErrorKind::TooLarge => "too_large",
        }
    }

    /// The kind of `name`.
    pub fn from_name(name: &str) -> Option<TextErrorKind> {
        [
            TextErrorKind::Unavailable,
            TextErrorKind::Packed,
            TextErrorKind::Cleared,
            TextErrorKind::Corrupt,
            TextErrorKind::TooLarge,
        ]
        .into_iter()
        .find(|k| k.name() == name)
    }
}

/// An error of a text clause, with a text that says why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextError {
    /// The kind.
    pub kind: TextErrorKind,
    /// The text.
    pub why: String,
}

impl TextError {
    /// An error of `kind` with `why`.
    pub fn new(kind: TextErrorKind, why: impl Into<String>) -> TextError {
        TextError {
            kind,
            why: why.into(),
        }
    }
}

/// Why a text call fails as a call: the caller raises.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CallError {
    /// The arguments do not have the form of section 12: `ValueError`.
    Malformed(String),
    /// The mode of the index is not served: `Unsupported`.
    Unsupported(String),
}

fn malformed<T>(why: impl Into<String>) -> Result<T, String> {
    Err(why.into())
}

fn key_of(key: &Wire) -> Result<&str, String> {
    match key {
        Wire::Str(s) => Ok(s),
        _ => malformed("a key of a text clause is not a str"),
    }
}

fn str_of<'w>(v: &'w Wire, what: &str) -> Result<&'w str, String> {
    match v {
        Wire::Str(s) => Ok(s),
        _ => malformed(format!("{what} is not a str")),
    }
}

fn array_of<'w>(v: &'w Wire, what: &str) -> Result<&'w [Wire], String> {
    match v {
        Wire::Array(items) => Ok(items),
        _ => malformed(format!("{what} is not a list")),
    }
}

fn strs_of(v: &Wire, what: &str) -> Result<Vec<String>, String> {
    array_of(v, what)?
        .iter()
        .map(|w| str_of(w, what).map(str::to_owned))
        .collect()
}

/// Read a rid: a signed 32-bit int.
pub fn rid_of(v: &Wire) -> Result<i32, String> {
    match v {
        Wire::Int(n) => zoe_core::check_rid(*n).map_err(|e| e.to_string()),
        _ => malformed("a rid is not an int"),
    }
}

/// Read the text clause `wire`. `with_kind` tells if the clause has `k`,
/// as in a plan of `catalog_query`.
pub fn decode_clause(wire: &Wire, with_kind: bool) -> Result<TextClause, String> {
    let Wire::Map(items) = wire else {
        return malformed("a text clause is not a map");
    };
    let (mut index, mut node, mut languages, mut cleared, mut staged, mut settings) =
        (None, None, None, None, None, None);
    let mut kind = false;
    for (key, v) in items {
        match key_of(key)? {
            "k" if with_kind => {
                if str_of(v, "k")? != "t" {
                    return malformed("the kind of a text clause is not t");
                }
                kind = true;
            }
            "i" => {
                let name = str_of(v, "i")?;
                check_index(name)?;
                index = Some(name.to_owned());
            }
            "q" => node = Some(decode_node(v, 0)?),
            "lg" => {
                languages = Some(match v {
                    Wire::Nil => None,
                    other => {
                        let list = strs_of(other, "lg")?;
                        for l in &list {
                            check_language(l)?;
                        }
                        Some(list)
                    }
                })
            }
            "cl" => {
                let list = strs_of(v, "cl")?;
                for name in &list {
                    check_index(name)?;
                }
                cleared = Some(list);
            }
            "st" => {
                let mut out = Vec::new();
                for item in array_of(v, "st")? {
                    out.push(decode_staged(item)?);
                }
                staged = Some(out);
            }
            "set" => {
                settings = Some(match v {
                    Wire::Nil => None,
                    other => Some(decode_settings(other)?),
                })
            }
            other => return malformed(format!("a key {other:?} of a text clause")),
        }
    }
    if with_kind && !kind {
        return malformed("a text clause without k");
    }
    match (index, node, languages, cleared, staged, settings) {
        (Some(index), Some(node), Some(languages), Some(cleared), Some(staged), Some(settings)) => {
            Ok(TextClause {
                index,
                node,
                languages,
                cleared,
                staged,
                settings,
            })
        }
        _ => malformed("a text clause without one of i, q, lg, cl, st and set"),
    }
}

fn decode_node(wire: &Wire, depth: usize) -> Result<Node, String> {
    if depth >= MAX_DEPTH {
        return malformed("a text clause deeper than 64 nodes");
    }
    let Wire::Map(items) = wire else {
        return malformed("a node is not a map");
    };
    let get = |name: &str| {
        items
            .iter()
            .find(|(k, _)| matches!(k, Wire::Str(s) if s == name))
            .map(|(_, v)| v)
    };
    for (k, _) in items {
        let k = key_of(k)?;
        if !matches!(k, "n" | "items" | "nots" | "w" | "p") {
            return malformed(format!("a key {k:?} of a node"));
        }
    }
    let nodes = |name: &str| -> Result<Vec<Node>, String> {
        let list = get(name).ok_or_else(|| format!("a node without {name}"))?;
        array_of(list, name)?
            .iter()
            .map(|n| decode_node(n, depth + 1))
            .collect()
    };
    let words =
        || -> Result<Vec<String>, String> { strs_of(get("w").ok_or("a node without w")?, "w") };
    let kind = str_of(get("n").ok_or("a node without n")?, "n")?;
    let expect = |keys: &[&str]| -> Result<(), String> {
        if items.len() == keys.len() + 1 && keys.iter().all(|k| get(k).is_some()) {
            Ok(())
        } else {
            malformed(format!("a node {kind} without the keys {keys:?}"))
        }
    };
    match kind {
        "and" => {
            expect(&["items", "nots"])?;
            Ok(Node::And {
                items: nodes("items")?,
                nots: nodes("nots")?,
            })
        }
        "or" => {
            expect(&["items"])?;
            Ok(Node::Or {
                items: nodes("items")?,
            })
        }
        "atom" => {
            expect(&["w"])?;
            Ok(Node::Atom { words: words()? })
        }
        "phrase" => {
            expect(&["w"])?;
            Ok(Node::Phrase { words: words()? })
        }
        "glob" => {
            expect(&["p"])?;
            Ok(Node::Glob {
                pattern: str_of(get("p").expect("p"), "p")?.to_owned(),
            })
        }
        other => malformed(format!("a node of the kind {other:?}")),
    }
}

fn decode_staged(wire: &Wire) -> Result<Staged, String> {
    match array_of(wire, "a staged text")? {
        [index, rid, language, text] => {
            let index = str_of(index, "the index of a staged text")?;
            check_index(index)?;
            let language = str_of(language, "the language of a staged text")?;
            check_language(language)?;
            let text = match text {
                Wire::Nil => None,
                other => Some(str_of(other, "a staged text")?.to_owned()),
            };
            Ok(Staged {
                index: index.to_owned(),
                rid: rid_of(rid)?,
                language: language.to_owned(),
                text,
            })
        }
        _ => malformed("a staged text is not [index, rid, language, text]"),
    }
}

fn decode_settings(wire: &Wire) -> Result<IndexSettings, String> {
    let Wire::Map(items) = wire else {
        return malformed("the settings of a text clause are not a map");
    };
    let (mut mode, mut language, mut boosts) = (None, None, None);
    for (k, v) in items {
        match key_of(k)? {
            "mode" => {
                let name = str_of(v, "mode")?;
                mode = Some(
                    Mode::from_name(name)
                        .ok_or_else(|| format!("the mode {name:?} is not known"))?,
                );
            }
            "language" => {
                let list = strs_of(v, "language")?;
                for l in &list {
                    check_language(l)?;
                }
                language = Some(list);
            }
            "boosts" => {
                let Wire::Map(pairs) = v else {
                    return malformed("the boosts are not a map");
                };
                let mut out = Vec::new();
                for (name, weight) in pairs {
                    let name = str_of(name, "the index of a boost")?;
                    check_index(name)?;
                    let weight = match weight {
                        Wire::Float(f) => *f,
                        Wire::Int(i) => *i as f64,
                        _ => return malformed("a boost is not a number"),
                    };
                    out.push((name.to_owned(), check_boost(weight)?));
                }
                boosts = Some(out);
            }
            other => return malformed(format!("a key {other:?} of the settings")),
        }
    }
    match (mode, language, boosts) {
        (Some(mode), Some(language), Some(boosts)) => Ok(IndexSettings {
            indexed_attrs: Vec::new(),
            mode,
            language,
            boosts,
            fuzzy: 0,
        }),
        _ => malformed("settings without one of mode, language and boosts"),
    }
}

/// Count the msgpack values and the bytes of `wire`, for the cap of a
/// call. The count stops past the caps.
pub fn measure(wire: &Wire) -> (usize, usize) {
    let mut values = 0;
    let mut bytes = 0;
    let mut stack = vec![wire];
    while let Some(w) = stack.pop() {
        values += 1;
        if values > MAX_VALUES || bytes > MAX_BYTES {
            break;
        }
        match w {
            Wire::Str(s) => bytes += s.len() + 1,
            Wire::Bin(b) | Wire::Ext(_, b) => bytes += b.len() + 1,
            Wire::Array(items) => {
                bytes += 1;
                stack.extend(items.iter());
            }
            Wire::Map(items) => {
                bytes += 1;
                for (k, v) in items {
                    stack.push(k);
                    stack.push(v);
                }
            }
            _ => bytes += 9,
        }
    }
    (values, bytes)
}

/// Tell if `wire` passes the cap of a call: 50,000 msgpack values or 16
/// MiB.
pub fn too_large(wire: &Wire) -> bool {
    let (values, bytes) = measure(wire);
    values > MAX_VALUES || bytes > MAX_BYTES
}

/// Read a filter of `text_query`: nil, or bin of signed 32-bit big-endian
/// ints. The filter comes back sorted, each rid once.
pub fn decode_filter(wire: &Wire) -> Result<Option<Vec<i32>>, String> {
    match wire {
        Wire::Nil => Ok(None),
        Wire::Bin(bytes) if bytes.len().is_multiple_of(4) => {
            let mut rids: Vec<i32> = bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|c| i32::from_be_bytes(*c))
                .collect();
            rids.sort_unstable();
            rids.dedup();
            Ok(Some(rids))
        }
        _ => malformed("a filter is not nil or bin of 4-byte rids"),
    }
}

/// The bin of `rids`, each a signed 32-bit big-endian int.
pub fn rids_bin(rids: impl IntoIterator<Item = i32>) -> Wire {
    Wire::Bin(rids.into_iter().flat_map(i32::to_be_bytes).collect())
}

fn tid_item(tid: u64) -> (Wire, Wire) {
    (
        Wire::Str("tid".into()),
        Wire::Bin(tid.to_be_bytes().to_vec()),
    )
}

/// The error form of a reply: `{"tid", "e", "why"}`.
pub fn encode_error(tid: u64, error: &TextError) -> Wire {
    Wire::Map(vec![
        tid_item(tid),
        (Wire::Str("e".into()), Wire::Str(error.kind.name().into())),
        (Wire::Str("why".into()), Wire::Str(error.why.clone())),
    ])
}

/// The hits of `text_query`: every hit in the filter, in rid order, with
/// its integer score.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TextHits {
    /// The rids, in ascending order.
    pub rids: Vec<i32>,
    /// The score of each rid.
    pub scores: Vec<i32>,
}

/// The reply of `text_query`.
pub fn encode_hits(tid: u64, answer: &Result<TextHits, TextError>) -> Wire {
    match answer {
        Ok(hits) => Wire::Map(vec![
            tid_item(tid),
            (Wire::Str("r".into()), rids_bin(hits.rids.iter().copied())),
            (
                Wire::Str("sc".into()),
                rids_bin(hits.scores.iter().copied()),
            ),
        ]),
        Err(error) => encode_error(tid, error),
    }
}

/// What `text_info` gives.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TextInfo {
    /// The documents of the index at the snapshot.
    pub docs: u64,
    /// The distinct words of the index at its tid.
    pub words: u64,
    /// The language and the live text of the rid at the snapshot.
    pub text: Option<(String, String)>,
}

/// The reply of `text_info`.
pub fn encode_info(tid: u64, answer: &Result<TextInfo, TextError>) -> Wire {
    match answer {
        Ok(info) => {
            let (lang, text) = match &info.text {
                Some((l, t)) => (Wire::Str(l.clone()), Wire::Str(t.clone())),
                None => (Wire::Nil, Wire::Nil),
            };
            Wire::Map(vec![
                tid_item(tid),
                (
                    Wire::Str("docs".into()),
                    Wire::Int(i64::try_from(info.docs).unwrap_or(i64::MAX)),
                ),
                (
                    Wire::Str("words".into()),
                    Wire::Int(i64::try_from(info.words).unwrap_or(i64::MAX)),
                ),
                (Wire::Str("lang".into()), lang),
                (Wire::Str("text".into()), text),
            ])
        }
        Err(error) => encode_error(tid, error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str) -> Wire {
        Wire::Str(v.into())
    }

    fn map(items: Vec<(&str, Wire)>) -> Wire {
        Wire::Map(items.into_iter().map(|(k, v)| (s(k), v)).collect())
    }

    fn atom(w: &str) -> Wire {
        map(vec![("n", s("atom")), ("w", Wire::Array(vec![s(w)]))])
    }

    fn clause(q: Wire) -> Wire {
        map(vec![
            ("i", s("SearchableText")),
            ("q", q),
            ("lg", Wire::Nil),
            ("cl", Wire::Array(vec![])),
            (
                "st",
                Wire::Array(vec![Wire::Array(vec![
                    s("SearchableText"),
                    Wire::Int(-5),
                    s("en"),
                    Wire::Nil,
                ])]),
            ),
            ("set", Wire::Nil),
        ])
    }

    #[test]
    fn a_clause_reads() {
        let c = decode_clause(&clause(atom("word")), false).unwrap();
        assert_eq!(c.index, "SearchableText");
        assert_eq!(
            c.node,
            Node::Atom {
                words: vec!["word".into()]
            }
        );
        assert_eq!(c.staged[0].rid, -5);
        assert_eq!(c.staged[0].text, None);
    }

    #[test]
    fn a_clause_of_another_form_is_an_error() {
        let rid = Wire::Array(vec![s("T"), Wire::Int(1 << 40), s("en"), Wire::Nil]);
        let mut bad_rid = clause(atom("w"));
        if let Wire::Map(items) = &mut bad_rid {
            items[4].1 = Wire::Array(vec![rid]);
        }
        let mut deep = atom("w");
        for _ in 0..MAX_DEPTH + 1 {
            deep = map(vec![("n", s("or")), ("items", Wire::Array(vec![deep]))]);
        }
        for bad in [
            clause(map(vec![("n", s("near")), ("w", Wire::Array(vec![]))])),
            clause(map(vec![("n", s("atom"))])),
            clause(s("word")),
            bad_rid,
            clause(deep),
            Wire::Array(vec![]),
        ] {
            assert!(decode_clause(&bad, false).is_err(), "{bad:?}");
        }
        assert!(decode_clause(&clause(atom("w")), true).is_err());
    }
}
