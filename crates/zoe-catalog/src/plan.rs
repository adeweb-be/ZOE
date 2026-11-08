//! The codec of a plan and of a reply, in the form of ADR 0059, section 8.
//!
//! The form is a tree of msgpack values: `Wire`. The extension module
//! makes it from a Python dict; the server and the native client convert
//! it from and to msgpack (`Wire::from_value`, `Wire::into_value`). This
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
//! reply  = {"tid": tid, "r": rids, "sc": scores | nil, "n": len,
//!           "a": actual_result_count,
//!           "rows": [[cell...] | nil...] | nil, "paths": [path | nil...] | nil}
//!        | {"tid": tid, "f": reason, "why": text}
//!        | {"tid": tid, "e": error, "why": text}
//! ```
//!
//! With the feature `text`, `split_text` takes the text clause of ADR 0060,
//! section 12 (`"k": "t"`, `text::clause`) out of a plan: at most one, kept
//! apart from the other clauses. The reply then gets `sc`, the score of each rid as a
//! signed 32-bit big-endian int, when the plan has no sort, and the error
//! form `e` when the text clause fails. Without the feature, a text clause
//! is an unknown kind.
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
//!
//! `decode_typed` also gives the kind of each clause. The mirror checks it
//! against the class of the index of the clause: a clause of another kind
//! gives the reason `plan`.

use crate::base::Kind;
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

impl Wire {
    /// The tree of the msgpack value `value`. An int past 64 signed bits,
    /// and a str that is not UTF-8, are errors: their text. The caller
    /// bounds the depth of `value`.
    pub fn from_value(value: &rmpv::Value) -> Result<Wire, String> {
        use rmpv::Value;
        Ok(match value {
            Value::Nil => Wire::Nil,
            Value::Boolean(b) => Wire::Bool(*b),
            Value::Integer(n) => Wire::Int(
                n.as_i64()
                    .ok_or_else(|| "an int of the plan beyond 64 bits".to_owned())?,
            ),
            Value::F32(f) => Wire::Float(f64::from(*f)),
            Value::F64(f) => Wire::Float(*f),
            Value::String(s) => Wire::Str(
                s.as_str()
                    .ok_or_else(|| "a str of the plan that is not UTF-8".to_owned())?
                    .to_owned(),
            ),
            Value::Binary(b) => Wire::Bin(b.clone()),
            Value::Array(items) => Wire::Array(
                items
                    .iter()
                    .map(Wire::from_value)
                    .collect::<Result<_, _>>()?,
            ),
            Value::Map(items) => Wire::Map(
                items
                    .iter()
                    .map(|(k, v)| Ok((Wire::from_value(k)?, Wire::from_value(v)?)))
                    .collect::<Result<_, String>>()?,
            ),
            Value::Ext(kind, data) => Wire::Ext(*kind, data.clone()),
        })
    }

    /// The msgpack value of the tree.
    pub fn into_value(self) -> rmpv::Value {
        use rmpv::Value;
        match self {
            Wire::Nil => Value::Nil,
            Wire::Bool(b) => Value::Boolean(b),
            Wire::Int(i) => Value::from(i),
            Wire::Float(f) => Value::F64(f),
            Wire::Str(s) => Value::from(s),
            Wire::Bin(b) => Value::Binary(b),
            Wire::Array(items) => Value::Array(items.into_iter().map(Wire::into_value).collect()),
            Wire::Map(items) => Value::Map(
                items
                    .into_iter()
                    .map(|(k, v)| (k.into_value(), v.into_value()))
                    .collect(),
            ),
            Wire::Ext(kind, data) => Value::Ext(kind, data),
        }
    }
}

/// The kind of a clause on the wire (`"k"`): the classes of index that it
/// reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClauseKind {
    /// `"v"`: a FieldIndex, a KeywordIndex, a DateIndex, a UUIDIndex or a
    /// `ZoeSecurityIndex`.
    Value,
    /// `"b"`: a BooleanIndex.
    Boolean,
    /// `"d"`: a DateRangeIndex.
    DateRange,
    /// `"p"`: an ExtendedPathIndex.
    Path,
}

impl ClauseKind {
    /// The name of the kind on the wire.
    pub fn name(self) -> &'static str {
        match self {
            ClauseKind::Value => "v",
            ClauseKind::Boolean => "b",
            ClauseKind::DateRange => "d",
            ClauseKind::Path => "p",
        }
    }

    /// Tells if a clause of this kind reads an index of the class `class`.
    pub fn reads(self, class: Kind) -> bool {
        match self {
            ClauseKind::Value => {
                matches!(
                    class,
                    Kind::Field | Kind::Keyword | Kind::Date | Kind::Uuid | Kind::Security
                )
            }
            ClauseKind::Boolean => class == Kind::Boolean,
            ClauseKind::DateRange => class == Kind::DateRange,
            ClauseKind::Path => class == Kind::Path,
        }
    }
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

fn clause(wire: &Wire) -> Result<(Clause, ClauseKind), PlanError> {
    let items = items(wire, "a clause")?;
    let kind = items.iter().find(|(k, _)| *k == "k").map(|(_, v)| *v);
    let kind = match kind {
        Some(Wire::Str(k)) => match k.as_str() {
            "v" => ClauseKind::Value,
            "b" => ClauseKind::Boolean,
            "d" => ClauseKind::DateRange,
            "p" => ClauseKind::Path,
            _ => return Err(PlanError::Unknown(format!("a clause of the kind {k:?}"))),
        },
        _ => return malformed("a clause without a kind"),
    };
    match kind {
        ClauseKind::Path => path_clause(&items).map(|c| (c, kind)),
        ClauseKind::Boolean | ClauseKind::DateRange => {
            one_key_clause(kind.name(), &items).map(|c| (c, kind))
        }
        ClauseKind::Value => value_clause(&items).map(|c| (c, kind)),
    }
}

/// Reads a clause of keys (`"v"`).
fn value_clause(items: &[(&str, &Wire)]) -> Result<Clause, PlanError> {
    let mut clause = Clause {
        index: String::new(),
        keys: Vec::new(),
        and: false,
        range: None,
        not: Vec::new(),
    };
    let mut named = false;
    for &(key, v) in items {
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
    decode_typed(wire).map(|(plan, columns, _)| (plan, columns))
}

/// Reads a plan as `decode_request` does, and gives the kind of each of
/// its clauses, in their order.
pub fn decode_typed(wire: &Wire) -> Result<(Plan, Columns, Vec<ClauseKind>), PlanError> {
    let mut plan = Plan::default();
    let mut columns = Columns::default();
    let mut kinds = Vec::new();
    let mut clauses = None;
    let mut unknown = None;
    for (key, v) in items(wire, "the plan")? {
        match key {
            "c" => {
                let mut out = Vec::new();
                for c in array(v, "c")? {
                    match clause(c) {
                        Ok((c, kind)) => {
                            out.push(c);
                            kinds.push(kind);
                        }
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
    Ok((plan, columns, kinds))
}

/// Tells if `wire` is a text clause: a map whose `k` is `t`.
#[cfg(feature = "text")]
fn is_text(wire: &Wire) -> bool {
    matches!(wire, Wire::Map(items) if items.iter().any(|(k, v)| {
        matches!(k, Wire::Str(k) if k == "k") && matches!(v, Wire::Str(v) if v == "t")
    }))
}

/// Takes the text clause out of the plan `wire` (ADR 0060, section 12):
/// the plan without it, and the clause. A plan holds at most one text
/// clause. A text clause of another form is `Malformed`. A plan of another
/// form comes back as it is, for `decode_typed` to refuse.
#[cfg(feature = "text")]
pub fn split_text(wire: &Wire) -> Result<(Wire, Option<crate::text::TextClause>), PlanError> {
    let Wire::Map(items) = wire else {
        return Ok((wire.clone(), None));
    };
    let mut text = None;
    let mut out = Vec::with_capacity(items.len());
    for (k, v) in items {
        match (k, v) {
            (Wire::Str(name), Wire::Array(clauses)) if name == "c" => {
                let mut kept = Vec::with_capacity(clauses.len());
                for c in clauses {
                    if is_text(c) {
                        if text.is_some() {
                            return malformed("a plan with two text clauses");
                        }
                        text = Some(
                            crate::text::clause::decode_clause(c, true)
                                .map_err(PlanError::Malformed)?,
                        );
                    } else {
                        kept.push(c.clone());
                    }
                }
                out.push((k.clone(), Wire::Array(kept)));
            }
            _ => out.push((k.clone(), v.clone())),
        }
    }
    Ok((Wire::Map(out), text))
}

/// The reply to a query at `tid`, as section 8 gives it on the wire, with
/// the scores `sc` and the error form `e` of ADR 0060, section 12.
pub fn encode_reply(tid: u64, answer: &Result<Reply, Fallback>) -> Wire {
    let tid = (
        Wire::Str("tid".into()),
        Wire::Bin(tid.to_be_bytes().to_vec()),
    );
    match answer {
        #[cfg(feature = "text")]
        Ok(Reply {
            error: Some(error), ..
        }) => Wire::Map(vec![
            tid,
            (Wire::Str("e".into()), Wire::Str(error.kind.name().into())),
            (Wire::Str("why".into()), Wire::Str(error.why.clone())),
        ]),
        Ok(reply) => {
            let mut items = vec![
                tid,
                (
                    Wire::Str("r".into()),
                    Wire::Bin(reply.rids.iter().flat_map(|r| r.to_be_bytes()).collect()),
                ),
            ];
            if let Some(scores) = &reply.scores {
                items.push((
                    Wire::Str("sc".into()),
                    Wire::Bin(scores.iter().flat_map(|s| s.to_be_bytes()).collect()),
                ));
            }
            items.push((Wire::Str("n".into()), Wire::Int(reply.len)));
            items.push((Wire::Str("a".into()), Wire::Int(reply.actual)));
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
