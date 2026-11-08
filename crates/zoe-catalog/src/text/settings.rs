//! The settings of the text indexes of a catalog: the durable namespace
//! `zoe.text` of the store (ADR 0060, section 14).
//!
//! The key is the 8 bytes of the oid of the `Catalog` object, as the
//! registration key of `zoe.catalog`. The value is a JSON object:
//!
//! ```text
//! {"indexes": {index: {"indexed_attrs": [attribute...],
//!                      "mode": "enhanced" | "compat",
//!                      "language": [language...],
//!                      "boosts": {index: float},
//!                      "fuzzy": 0 | 1 | 2}}}
//! ```
//!
//! - `language` lists the languages of the site. The first one is the
//!   language of a text whose language is not in the list. An empty list
//!   gives one field with no stemmer and no stop words.
//! - `boosts` gives the indexes that a query on the index also reads, and
//!   the weight of each. The index itself has the weight 1 unless `boosts`
//!   names it.
//! - `fuzzy` is the edit distance of fuzzy matching: 0, the default, turns
//!   it off.
//! - `indexed_attrs` is for `ZoeTextIndex`: the text index does not read it.
//!
//! A change of `mode` or `language`, or of the set of the indexes, starts a
//! rebuild of the index of the catalog. A change of the boosts applies to
//! the next query.

use std::collections::BTreeMap;

use serde_json::{Map, Value as Json};

/// The durable namespace of the settings.
pub const NAMESPACE: &str = "zoe.text";

/// The mode of a text index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// The analyzer of the language, the BM25 of tantivy and the boosts.
    Enhanced,
    /// The words and the score of ZCTextIndex. This build does not serve
    /// it: a query in this mode is `Unsupported`.
    Compat,
}

impl Mode {
    /// The name of the mode.
    pub fn name(self) -> &'static str {
        match self {
            Mode::Enhanced => "enhanced",
            Mode::Compat => "compat",
        }
    }

    /// The mode of `name`.
    pub fn from_name(name: &str) -> Option<Mode> {
        match name {
            "enhanced" => Some(Mode::Enhanced),
            "compat" => Some(Mode::Compat),
            _ => None,
        }
    }
}

/// The settings of one text index.
#[derive(Clone, Debug, PartialEq)]
pub struct IndexSettings {
    /// The attributes that `ZoeTextIndex` indexes.
    pub indexed_attrs: Vec<String>,
    /// The mode.
    pub mode: Mode,
    /// The languages of the fields of the index, the default first.
    pub language: Vec<String>,
    /// The indexes that a query on this index also reads, with their
    /// weights.
    pub boosts: Vec<(String, f32)>,
    /// The edit distance of fuzzy matching, 0 for none.
    pub fuzzy: u8,
}

impl IndexSettings {
    /// The settings of an index in the enhanced mode with the languages
    /// `language` and no boost.
    pub fn enhanced(language: &[&str]) -> IndexSettings {
        IndexSettings {
            indexed_attrs: Vec::new(),
            mode: Mode::Enhanced,
            language: language.iter().map(|l| (*l).to_owned()).collect(),
            boosts: Vec::new(),
            fuzzy: 0,
        }
    }

    /// The indexes that a query on `index` reads, with their weights: the
    /// index, then the indexes of `boosts`, each once.
    pub fn read(&self, index: &str) -> Vec<(String, f32)> {
        let own = self
            .boosts
            .iter()
            .find(|(name, _)| name == index)
            .map_or(1.0, |(_, w)| *w);
        let mut out = vec![(index.to_owned(), own)];
        for (name, weight) in &self.boosts {
            if name != index && !out.iter().any(|(n, _)| n == name) {
                out.push((name.clone(), *weight));
            }
        }
        out
    }
}

/// The settings of the text indexes of one catalog, by index id.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CatalogSettings {
    /// The settings of each text index.
    pub indexes: BTreeMap<String, IndexSettings>,
}

/// The layout of the tantivy index of a catalog: each text index, its mode
/// and the languages of its fields, in the order of the index ids. The
/// index records it in the payload of each commit. A change of the layout
/// starts a rebuild.
pub type Layout = Vec<(String, Mode, Vec<String>)>;

impl CatalogSettings {
    /// Read the value `bytes` of the namespace. A value of another form is
    /// an error: its text.
    pub fn parse(bytes: &[u8]) -> Result<CatalogSettings, String> {
        let json: Json =
            serde_json::from_slice(bytes).map_err(|e| format!("the settings are not JSON: {e}"))?;
        let Some(top) = json.as_object() else {
            return Err("the settings are not an object".into());
        };
        let Some(Json::Object(indexes)) = top.get("indexes") else {
            return Err("the settings have no object indexes".into());
        };
        let mut out = CatalogSettings::default();
        for (name, value) in indexes {
            check_index(name)?;
            out.indexes.insert(name.clone(), parse_index(value)?);
        }
        Ok(out)
    }

    /// The value of the namespace for these settings.
    pub fn to_json(&self) -> Vec<u8> {
        let mut indexes = Map::new();
        for (name, s) in &self.indexes {
            let mut item = Map::new();
            item.insert(
                "indexed_attrs".into(),
                Json::Array(s.indexed_attrs.iter().cloned().map(Json::String).collect()),
            );
            item.insert("mode".into(), Json::String(s.mode.name().into()));
            item.insert(
                "language".into(),
                Json::Array(s.language.iter().cloned().map(Json::String).collect()),
            );
            let boosts = s
                .boosts
                .iter()
                .map(|(n, w)| (n.clone(), Json::from(f64::from(*w))))
                .collect();
            item.insert("boosts".into(), Json::Object(boosts));
            item.insert("fuzzy".into(), Json::from(s.fuzzy));
            indexes.insert(name.clone(), Json::Object(item));
        }
        let mut top = Map::new();
        top.insert("indexes".into(), Json::Object(indexes));
        serde_json::to_vec(&Json::Object(top)).expect("JSON")
    }

    /// The layout of the tantivy index of these settings.
    pub fn layout(&self) -> Layout {
        self.indexes
            .iter()
            .map(|(name, s)| (name.clone(), s.mode, s.language.clone()))
            .collect()
    }
}

/// Check an index id: 1 to 255 bytes.
pub(crate) fn check_index(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > 255 {
        return Err(format!("the index id {name:?} does not have 1 to 255 bytes"));
    }
    Ok(())
}

/// Check a language: at most 35 bytes of ASCII.
pub(crate) fn check_language(language: &str) -> Result<(), String> {
    if language.len() > 35 || !language.is_ascii() {
        return Err(format!(
            "the language {language:?} is not at most 35 bytes of ASCII"
        ));
    }
    Ok(())
}

/// Check a boost: a finite weight above 0.
pub(crate) fn check_boost(weight: f64) -> Result<f32, String> {
    if weight.is_finite() && weight > 0.0 && weight < 1e6 {
        Ok(weight as f32)
    } else {
        Err(format!("the boost {weight} is not a finite weight above 0"))
    }
}

fn parse_index(value: &Json) -> Result<IndexSettings, String> {
    let Some(item) = value.as_object() else {
        return Err("the settings of an index are not an object".into());
    };
    let strs = |key: &str| -> Result<Vec<String>, String> {
        match item.get(key) {
            None => Ok(Vec::new()),
            Some(Json::Array(items)) => items
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| format!("an item of {key} is not a str"))
                })
                .collect(),
            Some(_) => Err(format!("{key} is not a list")),
        }
    };
    let mode = match item.get("mode") {
        None => Mode::Enhanced,
        Some(Json::String(name)) => {
            Mode::from_name(name).ok_or_else(|| format!("the mode {name:?} is not known"))?
        }
        Some(_) => return Err("the mode is not a str".into()),
    };
    let language = strs("language")?;
    for l in &language {
        check_language(l)?;
    }
    let mut boosts = Vec::new();
    match item.get("boosts") {
        None => {}
        Some(Json::Object(items)) => {
            for (name, weight) in items {
                check_index(name)?;
                let weight = weight
                    .as_f64()
                    .ok_or_else(|| format!("the boost of {name} is not a number"))?;
                boosts.push((name.clone(), check_boost(weight)?));
            }
        }
        Some(_) => return Err("the boosts are not an object".into()),
    }
    let fuzzy = match item.get("fuzzy") {
        None => 0,
        Some(value) => match value.as_u64() {
            Some(n @ 0..=2) => n as u8,
            _ => return Err("fuzzy is not 0, 1 or 2".into()),
        },
    };
    Ok(IndexSettings {
        indexed_attrs: strs("indexed_attrs")?,
        mode,
        language,
        boosts,
        fuzzy,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_settings_read_back_what_they_write() {
        let mut settings = CatalogSettings::default();
        let mut s = IndexSettings::enhanced(&["en", "fr"]);
        s.boosts = vec![("Title".into(), 3.0), ("Description".into(), 1.5)];
        s.indexed_attrs = vec!["SearchableText".into()];
        s.fuzzy = 1;
        settings.indexes.insert("SearchableText".into(), s);
        let back = CatalogSettings::parse(&settings.to_json()).unwrap();
        assert_eq!(back, settings);
    }

    #[test]
    fn a_value_of_another_form_is_an_error() {
        for bad in [
            &b"[]"[..],
            b"{}",
            b"{\"indexes\": {\"\": {}}}",
            b"{\"indexes\": {\"T\": {\"mode\": \"other\"}}}",
            b"{\"indexes\": {\"T\": {\"fuzzy\": 3}}}",
            b"{\"indexes\": {\"T\": {\"boosts\": {\"D\": -1}}}}",
            b"not json",
        ] {
            assert!(CatalogSettings::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_query_reads_the_index_then_its_boosts() {
        let mut s = IndexSettings::enhanced(&["en"]);
        s.boosts = vec![
            ("Title".into(), 3.0),
            ("Description".into(), 1.5),
            ("SearchableText".into(), 1.0),
        ];
        assert_eq!(
            s.read("SearchableText"),
            vec![
                ("SearchableText".into(), 1.0),
                ("Title".into(), 3.0),
                ("Description".into(), 1.5),
            ]
        );
    }
}
