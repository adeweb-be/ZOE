//! The plans file of `zoe-bench catalog`: the plans that a step of the
//! harness makes once in a Zope process and stores with the fixture.
//!
//! The file is one msgpack map, version 1:
//!
//! ```text
//! {"version": 1, "catalog": bin8, "fixture": {"name": str, "preset": str, "counts": map},
//!  "modes": {mode: bin} | nil,
//!  "queries": [query...], "writes": [write...] | nil, "texts": [staged...] | nil}
//! query  = {"name": str, "scenario": scenario, "plan": plan | nil, "reason": str | nil,
//!           "band": str | nil, "variant": str | nil, "weight": int | nil,
//!           "expect": {"r": bin, "n": int, "a": int} | nil}
//! write  = {"rows": int, "records": [[bin8, bin]...]}
//! staged = [index, rid, language, text]
//! ```
//!
//! - `catalog` is the oid of the `Catalog` object.
//! - `modes` maps each mode of the text indexes, `enhanced` and `compat`,
//!   to the value of the namespace `zoe.text` for the catalog in that mode.
//! - `scenario` is `intersect`, `text`, `filter`, `mix` or `path`. A query of
//!   `text` has a `band` and a `variant`. A query of `filter` gives the
//!   filter of a text variant: its `name` is the variant. A query of `mix`
//!   without a plan is a line of the log that the client sends to the
//!   Python path, with its `reason`. `weight` counts the lines of the log
//!   that the query stands for.
//! - `plan` has the form of ADR 0059, section 8, with nil `overlay`.
//! - `expect` holds the rids, `len` and `actual_result_count` of the
//!   Python path at the last tid of the store.
//! - `writes` are transactions of the catalog that a run commits in order,
//!   on a copy of the store at the tid of the plans: each record with the
//!   current serial of its oid. `rows` is the number of catalog rows that
//!   the transaction changes.
//! - `texts` are live texts of the store, which a run commits again on
//!   other rids and stages in a clause.

use std::collections::BTreeMap;
use std::path::Path;

use rmpv::Value;
use zoe_core::Oid;

/// The plans file, read.
#[derive(Clone, Debug)]
pub struct Plans {
    /// The oid of the `Catalog` object.
    pub catalog: Oid,
    /// The name of the fixture.
    pub name: String,
    /// The preset of the fixture: `catalog-1m`, `catalog-mini` or `custom`.
    pub preset: String,
    /// The counts of the fixture.
    pub counts: serde_json::Value,
    /// The value of `zoe.text` of each mode.
    pub modes: BTreeMap<String, Vec<u8>>,
    /// The queries, in the order of the file.
    pub queries: Vec<Query>,
    /// The transactions that a run commits.
    pub writes: Vec<Write>,
    /// The live texts of the sample.
    pub texts: Vec<Staged>,
}

/// One query of the plans file.
#[derive(Clone, Debug)]
pub struct Query {
    /// The name of the query.
    pub name: String,
    /// The scenario of the query.
    pub scenario: String,
    /// The plan, or `None` for a line of the log that takes the Python
    /// path in the client.
    pub plan: Option<Value>,
    /// The reason of the client when there is no plan.
    pub reason: Option<String>,
    /// The band of a text term.
    pub band: Option<String>,
    /// The variant of a text query, or the variant of a filter.
    pub variant: Option<String>,
    /// The lines of the log that the query stands for.
    pub weight: u64,
    /// The answer of the Python path.
    pub expect: Option<Expect>,
}

/// The answer of the Python path to a query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Expect {
    /// The rids of the result, in order.
    pub rids: Vec<i32>,
    /// `len()` of the result.
    pub n: i64,
    /// `actual_result_count` of the result.
    pub a: i64,
}

/// One transaction of the catalog.
#[derive(Clone, Debug)]
pub struct Write {
    /// The catalog rows that the transaction changes.
    pub rows: u64,
    /// The records.
    pub records: Vec<(Oid, Vec<u8>)>,
}

/// One live text.
#[derive(Clone, Debug)]
pub struct Staged {
    /// The index id.
    pub index: String,
    /// The rid.
    pub rid: i32,
    /// The language.
    pub language: String,
    /// The text.
    pub text: String,
}

/// An error of the plans file: its text.
pub type PlanError = String;

impl Plans {
    /// Read the plans file at `path`.
    pub fn read(path: &Path) -> Result<Plans, PlanError> {
        let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Plans::parse(&bytes)
    }

    /// Read the bytes of a plans file.
    pub fn parse(bytes: &[u8]) -> Result<Plans, PlanError> {
        let mut reader = bytes;
        let top = rmpv::decode::read_value(&mut reader)
            .map_err(|e| format!("the plans file is not msgpack: {e}"))?;
        if get(&top, "version").and_then(Value::as_u64) != Some(1) {
            return Err("the plans file is not of the version 1".into());
        }
        let catalog = match get(&top, "catalog") {
            Some(Value::Binary(b)) if b.len() == 8 => Oid(u64::from_be_bytes(
                b.as_slice().try_into().expect("8 bytes"),
            )),
            _ => return Err("the plans file has no catalog oid".into()),
        };
        let fixture = get(&top, "fixture").ok_or("the plans file has no fixture")?;
        let name = text(fixture, "name").ok_or("the fixture has no name")?;
        let preset = text(fixture, "preset").unwrap_or_else(|| "custom".into());
        let counts = get(fixture, "counts").map_or(serde_json::Value::Null, json_of);
        let mut modes = BTreeMap::new();
        if let Some(Value::Map(items)) = get(&top, "modes") {
            for (k, v) in items {
                let (Some(mode), Value::Binary(value)) = (k.as_str(), v) else {
                    return Err("a mode is not a str with a bin".into());
                };
                modes.insert(mode.to_owned(), value.clone());
            }
        }
        let mut queries = Vec::new();
        for item in array(&top, "queries")? {
            queries.push(query(item)?);
        }
        let mut writes = Vec::new();
        for item in array(&top, "writes")? {
            let rows = get(item, "rows").and_then(Value::as_u64).unwrap_or(0);
            let mut records = Vec::new();
            for record in array(item, "records")? {
                match record.as_array().map(Vec::as_slice) {
                    Some([Value::Binary(oid), Value::Binary(data)]) if oid.len() == 8 => {
                        let oid = u64::from_be_bytes(oid.as_slice().try_into().expect("8 bytes"));
                        records.push((Oid(oid), data.clone()));
                    }
                    _ => return Err("a record of a write is not [bin8, bin]".into()),
                }
            }
            writes.push(Write { rows, records });
        }
        let mut texts = Vec::new();
        for item in array(&top, "texts")? {
            match item.as_array().map(Vec::as_slice) {
                Some([index, rid, language, text]) => {
                    let (Some(index), Some(rid), Some(language), Some(text)) = (
                        index.as_str(),
                        rid.as_i64(),
                        language.as_str(),
                        text.as_str(),
                    ) else {
                        return Err("a text is not [str, int, str, str]".into());
                    };
                    let rid = i32::try_from(rid).map_err(|_| "a rid passes 32 bits")?;
                    texts.push(Staged {
                        index: index.to_owned(),
                        rid,
                        language: language.to_owned(),
                        text: text.to_owned(),
                    });
                }
                _ => return Err("a text is not [index, rid, language, text]".into()),
            }
        }
        Ok(Plans {
            catalog,
            name,
            preset,
            counts,
            modes,
            queries,
            writes,
            texts,
        })
    }

    /// The queries of `scenario`.
    pub fn of(&self, scenario: &str) -> Vec<&Query> {
        self.queries
            .iter()
            .filter(|q| q.scenario == scenario)
            .collect()
    }
}

fn query(item: &Value) -> Result<Query, PlanError> {
    let name = text(item, "name").ok_or("a query has no name")?;
    let scenario = text(item, "scenario").ok_or("a query has no scenario")?;
    let plan = match get(item, "plan") {
        None | Some(Value::Nil) => None,
        Some(plan @ Value::Map(_)) => Some(plan.clone()),
        Some(_) => return Err(format!("the plan of {name} is not a map")),
    };
    let expect = match get(item, "expect") {
        None | Some(Value::Nil) => None,
        Some(e) => Some(Expect {
            rids: rids(get(e, "r")).ok_or("an expect has no rids")?,
            n: get(e, "n")
                .and_then(Value::as_i64)
                .ok_or("an expect has no n")?,
            a: get(e, "a")
                .and_then(Value::as_i64)
                .ok_or("an expect has no a")?,
        }),
    };
    Ok(Query {
        name,
        scenario,
        plan,
        reason: text(item, "reason"),
        band: text(item, "band"),
        variant: text(item, "variant"),
        weight: get(item, "weight").and_then(Value::as_u64).unwrap_or(1),
        expect,
    })
}

/// The value of `key` in the map `value`.
pub fn get<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    value
        .as_map()?
        .iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .map(|(_, v)| v)
}

/// The str of `key` in the map `value`.
fn text(value: &Value, key: &str) -> Option<String> {
    get(value, key).and_then(Value::as_str).map(str::to_owned)
}

/// The array of `key` in the map `value`, empty when nil or missing.
fn array<'a>(value: &'a Value, key: &str) -> Result<&'a [Value], PlanError> {
    match get(value, key) {
        None | Some(Value::Nil) => Ok(&[]),
        Some(Value::Array(items)) => Ok(items),
        Some(_) => Err(format!("{key} is not an array")),
    }
}

/// The rids of a bin of signed 32-bit big-endian ints.
pub fn rids(value: Option<&Value>) -> Option<Vec<i32>> {
    match value {
        Some(Value::Binary(bytes)) if bytes.len() % 4 == 0 => Some(
            bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|c| i32::from_be_bytes(*c))
                .collect(),
        ),
        _ => None,
    }
}

/// The JSON of a msgpack value: a bin becomes its length.
pub fn json_of(value: &Value) -> serde_json::Value {
    use serde_json::Value as J;
    match value {
        Value::Nil => J::Null,
        Value::Boolean(b) => J::Bool(*b),
        Value::Integer(i) => i
            .as_i64()
            .map(J::from)
            .or_else(|| i.as_u64().map(J::from))
            .unwrap_or(J::Null),
        Value::F32(f) => J::from(f64::from(*f)),
        Value::F64(f) => J::from(*f),
        Value::String(s) => J::String(s.as_str().unwrap_or_default().to_owned()),
        Value::Binary(b) => J::from(b.len()),
        Value::Array(items) => J::Array(items.iter().map(json_of).collect()),
        Value::Map(items) => J::Object(
            items
                .iter()
                .filter_map(|(k, v)| Some((k.as_str()?.to_owned(), json_of(v))))
                .collect(),
        ),
        Value::Ext(..) => J::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(text: &str) -> Value {
        Value::from(text)
    }

    fn map(items: Vec<(&str, Value)>) -> Value {
        Value::Map(items.into_iter().map(|(k, v)| (s(k), v)).collect())
    }

    #[test]
    fn a_plans_file_reads_back() {
        let file = map(vec![
            ("version", Value::from(1)),
            ("catalog", Value::Binary(7u64.to_be_bytes().to_vec())),
            (
                "fixture",
                map(vec![
                    ("name", s("mini")),
                    ("preset", s("catalog-mini")),
                    ("counts", map(vec![("size", Value::from(3))])),
                ]),
            ),
            (
                "modes",
                map(vec![("compat", Value::Binary(b"{}".to_vec()))]),
            ),
            (
                "queries",
                Value::Array(vec![
                    map(vec![
                        ("name", s("q")),
                        ("scenario", s("intersect")),
                        ("plan", map(vec![("c", Value::Array(vec![]))])),
                        (
                            "expect",
                            map(vec![
                                ("r", Value::Binary(vec![0, 0, 0, 5, 255, 255, 255, 255])),
                                ("n", Value::from(2)),
                                ("a", Value::from(2)),
                            ]),
                        ),
                    ]),
                    map(vec![
                        ("name", s("l")),
                        ("scenario", s("mix")),
                        ("plan", Value::Nil),
                        ("reason", s("index")),
                        ("weight", Value::from(4)),
                    ]),
                ]),
            ),
            (
                "writes",
                Value::Array(vec![map(vec![
                    ("rows", Value::from(2)),
                    (
                        "records",
                        Value::Array(vec![Value::Array(vec![
                            Value::Binary(9u64.to_be_bytes().to_vec()),
                            Value::Binary(b"p".to_vec()),
                        ])]),
                    ),
                ])]),
            ),
            (
                "texts",
                Value::Array(vec![Value::Array(vec![
                    s("SearchableText"),
                    Value::from(5),
                    s("en"),
                    s("kalo"),
                ])]),
            ),
        ]);
        let mut bytes = Vec::new();
        rmpv::encode::write_value(&mut bytes, &file).unwrap();
        let plans = Plans::parse(&bytes).unwrap();
        assert_eq!(plans.catalog, Oid(7));
        assert_eq!(plans.preset, "catalog-mini");
        assert_eq!(plans.counts["size"], 3);
        assert_eq!(plans.modes["compat"], b"{}");
        assert_eq!(plans.queries[0].expect.as_ref().unwrap().rids, vec![5, -1]);
        assert_eq!(plans.queries[1].reason.as_deref(), Some("index"));
        assert_eq!(plans.queries[1].weight, 4);
        assert_eq!(plans.writes[0].records[0].0, Oid(9));
        assert_eq!(plans.texts[0].rid, 5);
        assert!(Plans::parse(b"\x80").is_err());
    }
}
