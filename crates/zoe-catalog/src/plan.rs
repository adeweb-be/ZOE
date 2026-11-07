//! The codec of a plan and of a reply, in the form of ADR 0059, section 8.
//!
//! The form is a tree of msgpack values: `Wire`. The extension module
//! makes it from a Python dict; the server makes it from msgpack. This
//! version reads the clauses of keys (`"k": "v"`), of a BooleanIndex
//! (`"k": "b"`), of a DateRangeIndex (`"k": "d"`) and of an
//! ExtendedPathIndex (`"k": "p"`).
//!
//! ```text
//! plan   = {"c": [clause...], "s": [[index, reverse]...], "l": sort_limit | nil,
//!           "bs": b_start, "bz": b_size | nil, "cols": [column...] | nil,
//!           "rn": count}
//! clause = {"k": "v", "i": index, "q": [value...], "and": bool,
//!           "r": [min | nil, max | nil] | nil, "not": [value...]}
//!        | {"k": "b", "i": index, "q": bool | nil}
//!        | {"k": "d", "i": index, "t": minutes | nil}
//!        | {"k": "p", "i": index, "q": [[path, level | nil]...], "lv": level,
//!           "and": bool, "d": depth, "nt": bool, "ns": navtree_start,
//!           "ip": [component...]}
//! value  = nil | bool | int | float | str | [value...] (a tuple)
//! reply  = {"tid": tid, "r": rids, "n": len, "a": actual_result_count,
//!           "rows": [[cell...] | nil...] | nil, "paths": [path | nil...] | nil}
//!        | {"tid": tid, "f": reason, "why": text}
//! ```
//!
//! `rows` and `paths` come when the plan names `cols`: one item for each of
//! the first `rn` rids of `r`. A cell is a msgpack value of `cell`. A row
//! is nil when a cell of the row has another type, when the record of the
//! rid does not have one value for each column of the schema, or when the
//! schema of the catalog has no column of `cols`.
//!
//! A plan that does not have this form is `Malformed`: the caller raises
//! `ValueError`. A key or a kind of clause that this version does not know
//! is `Unknown`: the reply is the reason `plan`.

use crate::cell;
use crate::query::{Clause, Columns, Plan, Reply};
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
    /// An ext: its type and its data.
    Ext(i8, Vec<u8>),
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
        Wire::Bin(_) | Wire::Map(_) | Wire::Ext(..) => {
            return malformed("a value that is a bin, a map or an ext");
        }
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
        Some(Wire::Str(k)) if matches!(k.as_str(), "v" | "b" | "d" | "p") => k.as_str(),
        Some(Wire::Str(k)) => {
            return Err(PlanError::Unknown(format!("a clause of the kind {k:?}")));
        }
        _ => return malformed("a clause without a kind"),
    };
    if kind == "p" {
        return path_clause(&items);
    }
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

/// Reads a clause on an ExtendedPathIndex (`"p"`). Its options go into
/// its first key, the tuple `(lv, d, nt, ns, (component...))`, and each
/// path of `q` into a key of its own, the tuple `(path, level or None)`.
/// Each key of the clause is required.
fn path_clause(items: &[(&str, &Wire)]) -> Result<Clause, PlanError> {
    let mut clause = Clause {
        index: String::new(),
        keys: Vec::new(),
        and: false,
        range: None,
        not: Vec::new(),
    };
    let mut paths = None;
    let (mut level, mut depth, mut navtree, mut start, mut ip) = (None, None, None, None, None);
    let mut named = false;
    let mut joined = false;
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
            "q" => {
                let mut out = Vec::new();
                for item in array(v, "q")? {
                    match array(item, "a path of q")? {
                        [Wire::Str(path), Wire::Nil] => out.push(Val::Tuple(
                            vec![Val::Str(path.as_str().into()), Val::None].into(),
                        )),
                        [Wire::Str(path), Wire::Int(level)] => out.push(Val::Tuple(
                            vec![Val::Str(path.as_str().into()), Val::Int(*level)].into(),
                        )),
                        _ => return malformed("a path of q is not [str, int | nil]"),
                    }
                }
                paths = Some(out);
            }
            "lv" => level = Some(int(v, "lv")?),
            "d" => depth = Some(int(v, "d")?),
            "ns" => start = Some(int(v, "ns")?),
            "nt" => match v {
                Wire::Bool(b) => navtree = Some(*b),
                _ => return malformed("nt is not a bool"),
            },
            "and" => match v {
                Wire::Bool(b) => {
                    clause.and = *b;
                    joined = true;
                }
                _ => return malformed("and is not a bool"),
            },
            "ip" => {
                let mut out = Vec::new();
                for c in array(v, "ip")? {
                    match c {
                        Wire::Str(c) => out.push(Val::Str(c.as_str().into())),
                        _ => return malformed("a component of ip is not a str"),
                    }
                }
                ip = Some(out);
            }
            other => return Err(PlanError::Unknown(format!("a clause key {other:?}"))),
        }
    }
    if !named {
        return malformed("a clause without an index");
    }
    let (Some(paths), Some(level), Some(depth), Some(navtree), Some(start), Some(ip), true) =
        (paths, level, depth, navtree, start, ip, joined)
    else {
        return malformed("a clause p without each of q, lv, and, d, nt, ns and ip");
    };
    clause.keys.push(Val::Tuple(
        vec![
            Val::Int(level),
            Val::Int(depth),
            Val::Bool(navtree),
            Val::Int(start),
            Val::Tuple(ip.into()),
        ]
        .into(),
    ));
    clause.keys.extend(paths);
    Ok(clause)
}

/// Reads a plan, without its rows.
pub fn decode_plan(wire: &Wire) -> Result<Plan, PlanError> {
    decode_request(wire).map(|(plan, _)| plan)
}

/// Reads a plan and the rows that its reply gives: `cols` and `rn`. A
/// negative `rn` gives no row.
pub fn decode_request(wire: &Wire) -> Result<(Plan, Columns), PlanError> {
    let mut plan = Plan::default();
    let mut columns = Columns::default();
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
            "rn" => columns.rn = usize::try_from(int(v, "rn")?).unwrap_or(0),
            "cols" => {
                columns.cols = match v {
                    Wire::Nil => None,
                    other => Some(
                        array(other, "cols")?
                            .iter()
                            .map(|c| match c {
                                Wire::Str(name) => Ok(name.clone()),
                                _ => malformed("a column of cols is not a str"),
                            })
                            .collect::<Result<_, _>>()?,
                    ),
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
    Ok((plan, columns))
}

/// The reply to a query at `tid`, as section 8 gives it on the wire.
pub fn encode_reply(tid: u64, answer: &Result<Reply, Fallback>) -> Wire {
    let tid = (
        Wire::Str("tid".into()),
        Wire::Bin(tid.to_be_bytes().to_vec()),
    );
    match answer {
        Ok(reply) => {
            let mut items = vec![
                tid,
                (
                    Wire::Str("r".into()),
                    Wire::Bin(reply.rids.iter().flat_map(|r| r.to_be_bytes()).collect()),
                ),
                (Wire::Str("n".into()), Wire::Int(reply.len)),
                (Wire::Str("a".into()), Wire::Int(reply.actual)),
            ];
            if let (Some(rows), Some(paths)) = (&reply.rows, &reply.paths) {
                let rows = rows
                    .iter()
                    .map(|row| row.as_deref().and_then(cell::decode).unwrap_or(Wire::Nil))
                    .collect();
                let paths = paths
                    .iter()
                    .map(|p| p.clone().map_or(Wire::Nil, Wire::Str))
                    .collect();
                items.push((Wire::Str("rows".into()), Wire::Array(rows)));
                items.push((Wire::Str("paths".into()), Wire::Array(paths)));
            }
            Wire::Map(items)
        }
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
