//! The codec of a plan and of a reply, in the form of ADR 0059, section 8.
//!
//! The form is a tree of msgpack values: `Wire`. The extension module
//! makes it from a Python dict; the server makes it from msgpack. This
//! version reads the clauses of keys (`"k": "v"`), of a BooleanIndex
//! (`"k": "b"`) and of a DateRangeIndex (`"k": "d"`).
//!
//! ```text
//! plan   = {"c": [clause...], "s": [[index, reverse]...], "l": sort_limit | nil,
//!           "bs": b_start, "bz": b_size | nil, "cols": nil, "rn": count}
//! clause = {"k": "v", "i": index, "q": [value...], "and": bool,
//!           "r": [min | nil, max | nil] | nil, "not": [value...]}
//!        | {"k": "b", "i": index, "q": bool | nil}
//!        | {"k": "d", "i": index, "t": minutes | nil}
//! value  = nil | bool | int | float | str | [value...] (a tuple)
//! reply  = {"tid": tid, "r": rids, "n": len, "a": actual_result_count}
//!        | {"tid": tid, "f": reason, "why": text}
//! ```
//!
//! A plan that does not have this form is `Malformed`: the caller raises
//! `ValueError`. A key or a kind of clause that this version does not know
//! is `Unknown`: the reply is the reason `plan`.

use crate::query::{Clause, Plan, Reply};
use crate::reason::{Fallback, Reason};
use crate::value::Val;

/// A msgpack value.
#[derive(Clone, Debug, PartialEq)]
pub enum Wire {
    /// nil.
    Nil,
    /// A bool.
    Bool(bool),
    /// An int.
    Int(i64),
    /// A float.
    Float(f64),
    /// A str.
    Str(String),
    /// A bin.
    Bin(Vec<u8>),
    /// An array.
    Array(Vec<Wire>),
    /// A map, in the order of its items.
    Map(Vec<(Wire, Wire)>),
}

/// Why a plan does not read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlanError {
    /// The plan does not have the form of section 8.
    Malformed(String),
    /// The plan has a key or a kind of clause that this version does not
    /// know.
    Unknown(String),
}

impl PlanError {
    /// The fallback of an unknown key or kind, `None` for a malformed plan.
    pub fn fallback(&self) -> Option<Fallback> {
        match self {
            PlanError::Unknown(why) => Some(Fallback::new(Reason::Plan, why.clone())),
            PlanError::Malformed(_) => None,
        }
    }
}

fn malformed<T>(why: impl Into<String>) -> Result<T, PlanError> {
    Err(PlanError::Malformed(why.into()))
}

/// The items of a map with str keys.
fn items<'a>(wire: &'a Wire, what: &str) -> Result<Vec<(&'a str, &'a Wire)>, PlanError> {
    let Wire::Map(items) = wire else {
        return malformed(format!("{what} is not a map"));
    };
    items
        .iter()
        .map(|(k, v)| match k {
            Wire::Str(k) => Ok((k.as_str(), v)),
            _ => malformed(format!("{what} has a key that is not a str")),
        })
        .collect()
}

fn int(wire: &Wire, what: &str) -> Result<i64, PlanError> {
    match wire {
        Wire::Int(i) => Ok(*i),
        _ => malformed(format!("{what} is not an int")),
    }
}

fn opt_int(wire: &Wire, what: &str) -> Result<Option<i64>, PlanError> {
    match wire {
        Wire::Nil => Ok(None),
        other => int(other, what).map(Some),
    }
}

fn array<'a>(wire: &'a Wire, what: &str) -> Result<&'a [Wire], PlanError> {
    match wire {
        Wire::Array(items) => Ok(items),
        _ => malformed(format!("{what} is not an array")),
    }
}

/// Reads a value of a clause.
fn value(wire: &Wire) -> Result<Val, PlanError> {
    let mut depth = 0;
    value_at(wire, &mut depth)
}

fn value_at(wire: &Wire, depth: &mut usize) -> Result<Val, PlanError> {
    *depth += 1;
    if *depth > 32 {
        return malformed("a value nested too deep");
    }
    let out = match wire {
        Wire::Nil => Val::None,
        Wire::Bool(b) => Val::Bool(*b),
        Wire::Int(i) => Val::Int(*i),
        Wire::Float(f) => Val::Float(*f),
        Wire::Str(s) => Val::Str(s.as_str().into()),
        Wire::Array(items) => Val::Tuple(
            items
                .iter()
                .map(|item| value_at(item, depth))
                .collect::<Result<_, _>>()?,
        ),
        Wire::Bin(_) | Wire::Map(_) => return malformed("a value that is a bin or a map"),
    };
    *depth -= 1;
    Ok(out)
}

fn values(wire: &Wire, what: &str) -> Result<Vec<Val>, PlanError> {
    array(wire, what)?.iter().map(value).collect()
}

fn clause(wire: &Wire) -> Result<Clause, PlanError> {
    let items = items(wire, "a clause")?;
    let kind = items.iter().find(|(k, _)| *k == "k").map(|(_, v)| *v);
    let kind = match kind {
        Some(Wire::Str(k)) if matches!(k.as_str(), "v" | "b" | "d") => k.as_str(),
        Some(Wire::Str(k)) => {
            return Err(PlanError::Unknown(format!("a clause of the kind {k:?}")));
        }
        _ => return malformed("a clause without a kind"),
    };
    if kind != "v" {
        return one_key_clause(kind, &items);
    }
    let mut clause = Clause {
        index: String::new(),
        keys: Vec::new(),
        and: false,
        range: None,
        not: Vec::new(),
    };
    let mut named = false;
    for (key, v) in items {
        match key {
            "k" => {}
            "i" => match v {
                Wire::Str(name) => {
                    clause.index = name.clone();
                    named = true;
                }
                _ => return malformed("the index of a clause is not a str"),
            },
            "q" => clause.keys = values(v, "q")?,
            "and" => match v {
                Wire::Bool(b) => clause.and = *b,
                _ => return malformed("and is not a bool"),
            },
            "r" => {
                clause.range = match v {
                    Wire::Nil => None,
                    Wire::Array(bounds) if bounds.len() == 2 => {
                        let bound = |w: &Wire| -> Result<Option<Val>, PlanError> {
                            match w {
                                Wire::Nil => Ok(None),
                                other => value(other).map(Some),
                            }
                        };
                        Some((bound(&bounds[0])?, bound(&bounds[1])?))
                    }
                    _ => return malformed("r is not nil or an array of two bounds"),
                }
            }
            "not" => clause.not = values(v, "not")?,
            other => return Err(PlanError::Unknown(format!("a clause key {other:?}"))),
        }
    }
    if !named {
        return malformed("a clause without an index");
    }
    Ok(clause)
}

/// Reads a clause on a BooleanIndex (`"b"`, the truth `q`) or on a
/// DateRangeIndex (`"d"`, the term `t`) as a clause of one key: `q` and
/// `t` give it; a nil `q` gives no key, and a nil `t` the key `None`.
fn one_key_clause(kind: &str, items: &[(&str, &Wire)]) -> Result<Clause, PlanError> {
    let mut clause = Clause {
        index: String::new(),
        keys: Vec::new(),
        and: false,
        range: None,
        not: Vec::new(),
    };
    let (mut named, mut keyed) = (false, false);
    let what = if kind == "b" { "q" } else { "t" };
    for (key, v) in items {
        match *key {
            "k" => {}
            "i" => match v {
                Wire::Str(name) => {
                    clause.index = name.clone();
                    named = true;
                }
                _ => return malformed("the index of a clause is not a str"),
            },
            k if k == what => {
                keyed = true;
                clause.keys = match (kind, v) {
                    ("b", Wire::Bool(b)) => vec![Val::Bool(*b)],
                    ("b", Wire::Nil) => Vec::new(),
                    ("d", Wire::Int(t)) => vec![Val::Int(*t)],
                    ("d", Wire::Nil) => vec![Val::None],
                    ("b", _) => return malformed("q of a clause b is not a bool or nil"),
                    _ => return malformed("t of a clause d is not an int or nil"),
                };
            }
            other => return Err(PlanError::Unknown(format!("a clause key {other:?}"))),
        }
    }
    if !named {
        return malformed("a clause without an index");
    }
    if !keyed {
        return malformed(format!("a clause {kind} without {what}"));
    }
    Ok(clause)
}

/// Reads a plan.
pub fn decode_plan(wire: &Wire) -> Result<Plan, PlanError> {
    let mut plan = Plan::default();
    let mut clauses = None;
    let mut unknown = None;
    for (key, v) in items(wire, "the plan")? {
        match key {
            "c" => {
                let mut out = Vec::new();
                for c in array(v, "c")? {
                    match clause(c) {
                        Ok(c) => out.push(c),
                        Err(PlanError::Unknown(why)) => {
                            unknown.get_or_insert(why);
                        }
                        Err(e) => return Err(e),
                    }
                }
                clauses = Some(out);
            }
            "s" => {
                for s in array(v, "s")? {
                    match array(s, "a sort")? {
                        [Wire::Str(name), reverse] => plan.sort.push((
                            name.clone(),
                            match reverse {
                                Wire::Nil => None,
                                Wire::Bool(b) => Some(*b),
                                _ => return malformed("the order of a sort is not a bool"),
                            },
                        )),
                        _ => return malformed("a sort is not [index, reverse]"),
                    }
                }
            }
            "l" => plan.limit = opt_int(v, "l")?,
            "bs" => plan.b_start = int(v, "bs")?,
            "bz" => plan.b_size = opt_int(v, "bz")?,
            "rn" => {
                int(v, "rn")?;
            }
            "cols" => {
                if *v != Wire::Nil {
                    unknown.get_or_insert_with(|| "the columns of the metadata table".into());
                }
            }
            other => {
                unknown.get_or_insert_with(|| format!("a plan key {other:?}"));
            }
        }
    }
    let Some(clauses) = clauses else {
        return malformed("a plan without c");
    };
    if let Some(why) = unknown {
        return Err(PlanError::Unknown(why));
    }
    plan.clauses = clauses;
    Ok(plan)
}

/// The reply to a query at `tid`, as section 8 gives it on the wire.
pub fn encode_reply(tid: u64, answer: &Result<Reply, Fallback>) -> Wire {
    let tid = (
        Wire::Str("tid".into()),
        Wire::Bin(tid.to_be_bytes().to_vec()),
    );
    match answer {
        Ok(reply) => Wire::Map(vec![
            tid,
            (
                Wire::Str("r".into()),
                Wire::Bin(reply.rids.iter().flat_map(|r| r.to_be_bytes()).collect()),
            ),
            (Wire::Str("n".into()), Wire::Int(reply.len)),
            (Wire::Str("a".into()), Wire::Int(reply.actual)),
        ]),
        Err(fallback) => Wire::Map(vec![
            tid,
            (
                Wire::Str("f".into()),
                Wire::Str(fallback.reason.name().into()),
            ),
            (Wire::Str("why".into()), Wire::Str(fallback.why.clone())),
        ]),
    }
}
