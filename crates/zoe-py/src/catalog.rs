//! The catalog mirrors of a `zoe.Storage` (ADR 0059): the plan of
//! `catalog_query` from a Python dict, its transaction overlay from a list
//! of `(oid, pickle)` pairs, its reply as a dict with the rows of the
//! metadata table as Python values, the state and the counts of the
//! mirrors, and the log of their events.
//!
//! The mirrors of a store live in `zoe-catalog`. `storage` starts them at
//! the open of a store whose namespace `zoe.catalog` holds a registration
//! key, or at the first write of that namespace, never at import. It hands
//! each commit to them after its publication.

use pyo3::IntoPyObjectExt;
use std::sync::atomic::{AtomicUsize, Ordering};

use pyo3::exceptions::{PyIndexError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyBytes, PyDict, PyFloat, PyInt, PyList, PyString, PyTuple};
use zoe_catalog::cell::{self, EXT_DATE_TIME, EXT_MESSAGE, EXT_MISSING};
use zoe_catalog::counts::{Counts, LATENCY_BUCKETS};
use zoe_catalog::overlay::Records;
use zoe_catalog::{Fallback, Reply, Status, Wire};

/// The deepest plan that `wire` reads.
const DEPTH: usize = 64;

/// The plan `value`, a dict, as a tree of msgpack values. A value of
/// another type raises `ValueError`.
pub(crate) fn wire(value: &Bound<'_, PyAny>) -> PyResult<Wire> {
    wire_at(value, 0)
}

fn wire_at(value: &Bound<'_, PyAny>, depth: usize) -> PyResult<Wire> {
    if depth > DEPTH {
        return Err(PyValueError::new_err("the plan is nested too deep"));
    }
    if value.is_none() {
        return Ok(Wire::Nil);
    }
    if let Ok(b) = value.cast::<PyBool>() {
        return Ok(Wire::Bool(b.is_true()));
    }
    if value.is_instance_of::<PyInt>() {
        let i: i64 = value
            .extract()
            .map_err(|_| PyValueError::new_err("an int of the plan beyond 64 bits"))?;
        return Ok(Wire::Int(i));
    }
    if let Ok(f) = value.cast::<PyFloat>() {
        return Ok(Wire::Float(f.value()));
    }
    if let Ok(s) = value.cast::<PyString>() {
        return Ok(Wire::Str(s.to_str()?.to_owned()));
    }
    if let Ok(b) = value.cast::<PyBytes>() {
        return Ok(Wire::Bin(b.as_bytes().to_vec()));
    }
    if let Ok(items) = value.cast::<PyList>() {
        return items
            .iter()
            .map(|item| wire_at(&item, depth + 1))
            .collect::<PyResult<_>>()
            .map(Wire::Array);
    }
    if let Ok(items) = value.cast::<PyTuple>() {
        return items
            .iter()
            .map(|item| wire_at(&item, depth + 1))
            .collect::<PyResult<_>>()
            .map(Wire::Array);
    }
    if let Ok(items) = value.cast::<PyDict>() {
        return items
            .iter()
            .map(|(k, v)| Ok((wire_at(&k, depth + 1)?, wire_at(&v, depth + 1)?)))
            .collect::<PyResult<_>>()
            .map(Wire::Map);
    }
    Err(PyValueError::new_err(format!(
        "a value of the plan of the type {}",
        value.get_type().name()?
    )))
}

/// The classes of the ext types of the cells, imported at their first use.
#[derive(Default)]
struct Classes<'py> {
    missing: Option<Bound<'py, PyAny>>,
    date_time: Option<Bound<'py, PyAny>>,
    message: Option<Bound<'py, PyAny>>,
}

/// The object `name` of the module `module`, as the unpickler finds the
/// GLOBAL of a pickle.
fn global<'py>(
    py: Python<'py>,
    slot: &mut Option<Bound<'py, PyAny>>,
    module: &str,
    name: &str,
) -> PyResult<Bound<'py, PyAny>> {
    if let Some(found) = slot {
        return Ok(found.clone());
    }
    let found = py.import(module)?.getattr(name)?;
    *slot = Some(found.clone());
    Ok(found)
}

/// The Python value of the cell `value`, as the unpickler makes it from the
/// record of `data`: a tuple for an array, a dict for a map, `Missing.V`
/// for the ext 1, a `DateTime` given its state by `__setstate__` for the
/// ext 2, and the `Message` of its arguments for the ext 3.
fn cell_value<'py>(
    py: Python<'py>,
    classes: &mut Classes<'py>,
    value: &Wire,
) -> PyResult<Bound<'py, PyAny>> {
    let bad = || PyValueError::new_err("a cell of another form");
    Ok(match value {
        Wire::Nil => py.None().into_bound(py),
        Wire::Bool(b) => b.into_bound_py_any(py)?,
        Wire::Int(i) => i.into_bound_py_any(py)?,
        Wire::Float(f) => f.into_bound_py_any(py)?,
        Wire::Str(s) => PyString::new(py, s).into_any(),
        Wire::Bin(b) => PyBytes::new(py, b).into_any(),
        Wire::Array(items) => {
            let items = items
                .iter()
                .map(|item| cell_value(py, classes, item))
                .collect::<PyResult<Vec<_>>>()?;
            PyTuple::new(py, items)?.into_any()
        }
        Wire::Map(items) => {
            let out = PyDict::new(py);
            for (k, v) in items {
                out.set_item(cell_value(py, classes, k)?, cell_value(py, classes, v)?)?;
            }
            out.into_any()
        }
        Wire::Ext(EXT_MISSING, data) if data.is_empty() => {
            global(py, &mut classes.missing, "Missing", "V")?
        }
        Wire::Ext(EXT_DATE_TIME, data) => {
            let (micros, naive, tz) = cell::date_time(data).ok_or_else(bad)?;
            let class = global(py, &mut classes.date_time, "DateTime.DateTime", "DateTime")?;
            let made = class.call_method1("__new__", (&class,))?;
            made.call_method1("__setstate__", ((micros, naive, tz),))?;
            made
        }
        Wire::Ext(EXT_MESSAGE, data) => {
            let args = cell::message_args(data).ok_or_else(bad)?;
            let args = args
                .iter()
                .map(|arg| cell_value(py, classes, arg))
                .collect::<PyResult<Vec<_>>>()?;
            let class = global(
                py,
                &mut classes.message,
                "zope.i18nmessageid.message",
                "Message",
            )?;
            class.call1(PyTuple::new(py, args)?)?
        }
        Wire::Ext(..) => return Err(bad()),
    })
}

/// The tuple of the cells of the row `bytes`, a msgpack array. `None` when
/// a cell does not make a Python value: the brain then comes from `data`.
fn row<'py>(py: Python<'py>, classes: &mut Classes<'py>, bytes: &[u8]) -> Bound<'py, PyAny> {
    let made = match cell::decode(bytes) {
        Some(Wire::Array(cells)) => cells
            .iter()
            .map(|c| cell_value(py, classes, c))
            .collect::<PyResult<Vec<_>>>()
            .and_then(|cells| PyTuple::new(py, cells)),
        _ => Err(PyValueError::new_err("a row of another form")),
    };
    match made {
        Ok(cells) => cells.into_any(),
        Err(_) => py.None().into_bound(py),
    }
}

/// The rows of a reply: a sequence of the tuple of the cells of each of the
/// first rids, or `None`. It makes the Python values of a row only when the
/// row is read, so a caller that reads few brains pays for few rows.
#[pyclass(frozen, sequence, module = "zoe._zoe", name = "CatalogRows")]
pub(crate) struct Rows {
    rows: Vec<Option<Vec<u8>>>,
    /// The rows that a read decoded.
    decoded: AtomicUsize,
}

#[pymethods]
impl Rows {
    fn __len__(&self) -> usize {
        self.rows.len()
    }

    /// The tuple of the cells of the row at `index`, or `None` when the
    /// brain of its rid comes from `data`.
    fn __getitem__<'py>(&self, py: Python<'py>, index: isize) -> PyResult<Bound<'py, PyAny>> {
        let at = if index < 0 {
            index + self.rows.len() as isize
        } else {
            index
        };
        let Some(found) = usize::try_from(at).ok().and_then(|at| self.rows.get(at)) else {
            return Err(PyIndexError::new_err("the index of a row is out of range"));
        };
        Ok(match found {
            Some(bytes) => {
                self.decoded.fetch_add(1, Ordering::Relaxed);
                row(py, &mut Classes::default(), bytes)
            }
            None => py.None().into_bound(py),
        })
    }

    /// The number of rows that a read decoded, for the tests.
    #[getter]
    fn _decoded(&self) -> usize {
        self.decoded.load(Ordering::Relaxed)
    }
}

/// The reply of a query at `tid`, as a dict: `r` as a list of ints, `n`,
/// `a`, `rows` and `paths`, or `f` and `why`. `rows` is a sequence of the
/// tuple of the cells of each of the first rids, or `None` for a rid whose
/// brain comes from `data`, made when it is read; `paths` holds the path of
/// each. Both are `None` when the plan names no columns.
pub(crate) fn reply<'py>(
    py: Python<'py>,
    tid: &[u8],
    answer: &Result<Reply, Fallback>,
) -> PyResult<Bound<'py, PyDict>> {
    if let Ok(Reply {
        error: Some(error), ..
    }) = answer
    {
        return Err(crate::text::error(py, error));
    }
    let out = PyDict::new(py);
    out.set_item("tid", PyBytes::new(py, tid))?;
    match answer {
        Ok(reply) => {
            out.set_item("r", PyList::new(py, &reply.rids)?)?;
            if let Some(scores) = &reply.scores {
                out.set_item("sc", PyList::new(py, scores)?)?;
            }
            out.set_item("n", reply.len)?;
            out.set_item("a", reply.actual)?;
            match (&reply.rows, &reply.paths) {
                (Some(rows), Some(paths)) => {
                    let made = Rows {
                        rows: rows.clone(),
                        decoded: AtomicUsize::new(0),
                    };
                    out.set_item("rows", Bound::new(py, made)?)?;
                    out.set_item("paths", PyList::new(py, paths)?)?;
                }
                _ => {
                    out.set_item("rows", py.None())?;
                    out.set_item("paths", py.None())?;
                }
            }
        }
        Err(fallback) => {
            out.set_item("f", fallback.reason.name())?;
            out.set_item("why", &fallback.why)?;
        }
    }
    Ok(out)
}

/// The transaction overlay `value`: `None`, or a list or a tuple of
/// `(oid, pickle)` pairs of bytes, with each oid 8 bytes. Another form
/// raises `ValueError`.
pub(crate) fn overlay(value: Option<&Bound<'_, PyAny>>) -> PyResult<Option<Records>> {
    let Some(value) = value.filter(|v| !v.is_none()) else {
        return Ok(None);
    };
    let bad = || PyValueError::new_err("the overlay is a list of (oid, pickle) pairs of bytes");
    let items: Vec<Bound<'_, PyAny>> = if let Ok(list) = value.cast::<PyList>() {
        list.iter().collect()
    } else if let Ok(tuple) = value.cast::<PyTuple>() {
        tuple.iter().collect()
    } else {
        return Err(bad());
    };
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let pair: Vec<Bound<'_, PyAny>> = if let Ok(t) = item.cast::<PyTuple>() {
            t.iter().collect()
        } else if let Ok(l) = item.cast::<PyList>() {
            l.iter().collect()
        } else {
            return Err(bad());
        };
        let [oid, pickle] = pair.as_slice() else {
            return Err(bad());
        };
        let (Ok(oid), Ok(pickle)) = (oid.cast::<PyBytes>(), pickle.cast::<PyBytes>()) else {
            return Err(bad());
        };
        out.push((
            p64("an oid of the overlay", oid.as_bytes())?,
            pickle.as_bytes().to_vec(),
        ));
    }
    Ok(Some(out))
}

/// The transaction overlay `records` as the msgpack value of the wire, for
/// the native client: nil, or `[[oid, pickle]...]`.
pub(crate) fn value_of_overlay(records: Option<Records>) -> rmpv::Value {
    use rmpv::Value;
    match records {
        None => Value::Nil,
        Some(records) => Value::Array(
            records
                .into_iter()
                .map(|(oid, pickle)| {
                    Value::Array(vec![
                        Value::Binary(oid.to_be_bytes().to_vec()),
                        Value::Binary(pickle),
                    ])
                })
                .collect(),
        ),
    }
}

/// The reply `value` of `catalog_query` over the wire, as `reply` gives
/// the reply of the mirrors of the process: the same keys and the same
/// types. Each row keeps its msgpack bytes, and `CatalogRows` makes its
/// Python values only when the row is read. A key that this version does
/// not know is left out. A known key of another form raises `ValueError`.
pub(crate) fn reply_of_value<'py>(
    py: Python<'py>,
    value: rmpv::Value,
) -> PyResult<Bound<'py, PyDict>> {
    use rmpv::Value;
    let bad = |what: &str| PyValueError::new_err(format!("a catalog reply with {what}"));
    // The scores of a text clause, or the exception of its error form.
    let scores = crate::text::scores_of_value(py, &value)?;
    let Value::Map(items) = value else {
        return Err(bad("another form"));
    };
    let out = PyDict::new(py);
    let mut rows = py.None().into_bound(py);
    let mut paths = py.None().into_bound(py);
    let mut answered = false;
    for (key, item) in items {
        let Some(key) = key.as_str() else {
            return Err(bad("a key that is not a str"));
        };
        match (key, item) {
            ("tid", Value::Binary(tid)) => out.set_item("tid", PyBytes::new(py, &tid))?,
            ("r", Value::Binary(rids)) => {
                if rids.len() % 4 != 0 {
                    return Err(bad("rids that are not 4 bytes each"));
                }
                let rids: Vec<i32> = rids
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|c| i32::from_be_bytes(*c))
                    .collect();
                out.set_item("r", PyList::new(py, rids)?)?;
                answered = true;
            }
            ("n" | "a", Value::Integer(n)) => {
                out.set_item(key, n.as_i64().ok_or_else(|| bad("a count past 64 bits"))?)?;
            }
            ("rows", Value::Array(items)) => {
                let mut made = Vec::with_capacity(items.len());
                for item in items {
                    made.push(match item {
                        Value::Nil => None,
                        row @ Value::Array(_) => {
                            let mut bytes = Vec::new();
                            rmpv::encode::write_value(&mut bytes, &row)
                                .map_err(|_| bad("a row that does not encode"))?;
                            Some(bytes)
                        }
                        _ => return Err(bad("a row that is not an array or nil")),
                    });
                }
                rows = Bound::new(
                    py,
                    Rows {
                        rows: made,
                        decoded: AtomicUsize::new(0),
                    },
                )?
                .into_any();
            }
            ("paths", Value::Array(items)) => {
                let list = PyList::empty(py);
                for item in items {
                    match item {
                        Value::Nil => list.append(py.None())?,
                        Value::String(s) => {
                            list.append(s.as_str().ok_or_else(|| bad("a path that is not UTF-8"))?)?
                        }
                        _ => return Err(bad("a path that is not a str or nil")),
                    }
                }
                paths = list.into_any();
            }
            ("rows" | "paths", Value::Nil) => {}
            ("f" | "why", Value::String(s)) => {
                out.set_item(
                    key,
                    s.as_str().ok_or_else(|| bad("a text that is not UTF-8"))?,
                )?;
            }
            ("tid" | "r" | "n" | "a" | "rows" | "paths" | "f" | "why", _) => {
                return Err(bad(&format!("the key {key:?} of another form")));
            }
            // A newer server can add a key: an older client reads past it.
            _ => {}
        }
    }
    if answered {
        out.set_item("rows", rows)?;
        out.set_item("paths", paths)?;
        if let Some(scores) = scores {
            out.set_item("sc", PyList::new(py, scores)?)?;
        }
    }
    Ok(out)
}

/// The state of a mirror as a dict, or `None`.
pub(crate) fn status<'py>(py: Python<'py>, status: Option<Status>) -> PyResult<Bound<'py, PyAny>> {
    let Some(status) = status else {
        return Ok(py.None().into_bound(py));
    };
    let out = PyDict::new(py);
    out.set_item("state", status.state)?;
    out.set_item("reason", status.reason)?;
    out.set_item("base_tid", status.base_tid)?;
    out.set_item("applied", status.applied)?;
    out.set_item("floor", status.floor)?;
    out.set_item("sets", status.sets)?;
    out.set_item("stale_at", status.stale_at)?;
    out.set_item("rebases", status.rebases)?;
    out.set_item("bytes", status.bytes)?;
    Ok(out.into_any())
}

/// The counts of the mirrors as a dict, with the names of section 11.
pub(crate) fn counts<'py>(py: Python<'py>, counts: Option<Counts>) -> PyResult<Bound<'py, PyDict>> {
    let counts = counts.unwrap_or_default();
    let out = PyDict::new(py);
    let queries = PyDict::new(py);
    queries.set_item("native", counts.native)?;
    queries.set_item("python", counts.python)?;
    out.set_item("zoe_catalog_queries_total", queries)?;
    let fallbacks = PyDict::new(py);
    for (reason, n) in &counts.fallbacks {
        fallbacks.set_item(reason.name(), n)?;
    }
    out.set_item("zoe_catalog_fallbacks_total", fallbacks)?;
    let latency = PyDict::new(py);
    let mut buckets: Vec<(f64, u64)> = Vec::new();
    let mut cumulative = 0;
    for (bound, n) in LATENCY_BUCKETS.iter().zip(&counts.latency) {
        cumulative += n;
        buckets.push((*bound, cumulative));
    }
    latency.set_item("buckets", buckets)?;
    latency.set_item("count", counts.latency.iter().sum::<u64>())?;
    latency.set_item("sum", counts.latency_seconds)?;
    out.set_item("zoe_catalog_query_latency_seconds", latency)?;
    out.set_item("zoe_catalog_mirror_drops_total", counts.drops)?;
    out.set_item("zoe_catalog_overlay_records", counts.overlay_records)?;
    let mirrors = PyDict::new(py);
    for m in &counts.mirrors {
        let one = PyDict::new(py);
        one.set_item("zoe_catalog_mirror_ready", m.ready)?;
        one.set_item("zoe_catalog_mirror_build_seconds", m.build_seconds)?;
        one.set_item("zoe_catalog_mirror_lag_tids", m.lag_tids)?;
        one.set_item("zoe_catalog_mirror_bytes", m.bytes)?;
        mirrors.set_item(PyBytes::new(py, &m.catalog.to_be_bytes()), one)?;
    }
    out.set_item("mirrors", mirrors)?;
    let texts = PyDict::new(py);
    for t in &counts.texts {
        let one = PyDict::new(py);
        one.set_item("zoe_text_commit_lag_seconds", t.lag_seconds)?;
        one.set_item("ready", t.ready)?;
        one.set_item("docs", t.docs)?;
        texts.set_item(PyBytes::new(py, &t.catalog.to_be_bytes()), one)?;
    }
    out.set_item("texts", texts)?;
    Ok(out)
}

/// Logs `events` of the mirrors as warnings of the logger `zoe.catalog`.
/// A failed log is ignored: the events are for the operator only.
pub(crate) fn log(py: Python<'_>, events: Vec<String>) {
    if events.is_empty() {
        return;
    }
    let Ok(logger) = py
        .import("logging")
        .and_then(|logging| logging.call_method1("getLogger", ("zoe.catalog",)))
    else {
        return;
    };
    for event in events {
        let _ = logger.call_method1("warning", ("%s", event));
    }
}

/// The 8 bytes of an oid or of a tid, as an int. Other bytes raise
/// `ValueError`.
pub(crate) fn p64(what: &str, bytes: &[u8]) -> PyResult<u64> {
    <[u8; 8]>::try_from(bytes)
        .map(u64::from_be_bytes)
        .map_err(|_| PyValueError::new_err(format!("{what} is 8 bytes")))
}

/// What a throwaway mirror reads: the records that the Python callable
/// `load` gives, `load(oid)` as 8 bytes to a pickle or `None`. The callable
/// reads the test storage at the snapshot of the Connection, so the tid of
/// each read is that of the build. The build reads no commit and no
/// registration.
struct Loads {
    load: Py<PyAny>,
    tid: u64,
}

impl zoe_catalog::Reads for Loads {
    fn last_tid(&self) -> u64 {
        self.tid
    }

    fn load_before(&self, oid: u64, _tid: u64) -> Result<Option<Vec<u8>>, String> {
        Python::attach(|py| {
            let found = self
                .load
                .call1(py, (PyBytes::new(py, &oid.to_be_bytes()),))
                .map_err(|e| format!("load of {oid:#x}: {e}"))?;
            let found = found.bind(py);
            if found.is_none() {
                return Ok(None);
            }
            found
                .cast::<PyBytes>()
                .map(|b| Some(b.as_bytes().to_vec()))
                .map_err(|_| format!("load of {oid:#x} gave no bytes"))
        })
    }

    fn commits(
        &self,
        _from: u64,
        _to: u64,
        _each: &mut dyn FnMut(zoe_catalog::read::Commit) -> bool,
    ) -> Result<(), String> {
        Ok(())
    }

    fn registrations(&self) -> Result<Vec<Vec<u8>>, String> {
        Ok(Vec::new())
    }

    fn registration_generation(&self) -> Result<u64, String> {
        Ok(0)
    }
}

/// Answer `plan` from a throwaway mirror of the catalog `catalog`, 8
/// bytes, as of the snapshot that ends before `tid`, 8 bytes (ADR 0059,
/// section 9). This is a private function of `zoe.plone.catalog`, for the
/// mode `compare` on a test storage.
///
/// `load(oid)` gives the pickle of an oid of 8 bytes as the Connection
/// sees it, or `None`. The build reads the catalog through it, then the
/// query reads the transaction overlay `overlay` over the base, as a query
/// of the mirror of a store reads it. The mirror answers this query only,
/// and is dropped: it never enters the mirrors of a store. The reply is
/// that of `Storage.catalog_query`. A plan of another form raises
/// `ValueError`.
#[pyfunction]
#[pyo3(signature = (load, catalog, tid, plan, overlay=None))]
pub(crate) fn _catalog_throwaway<'py>(
    py: Python<'py>,
    load: Py<PyAny>,
    catalog: &[u8],
    tid: &[u8],
    plan: &Bound<'py, PyAny>,
    overlay: Option<&Bound<'py, PyAny>>,
) -> PyResult<Bound<'py, PyDict>> {
    use std::collections::HashMap;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use zoe_catalog::build::{BuildError, build};
    use zoe_catalog::change::Overrides;
    use zoe_catalog::query::View;
    use zoe_catalog::{PlanError, Reason, decode_ordered};

    let oid = p64("catalog", catalog)?;
    let before = p64("tid", tid)?;
    let wire = wire(plan)?;
    let records = self::overlay(overlay)?;
    let (plan, columns, kinds, order) = match decode_ordered(&wire) {
        Ok(decoded) => decoded,
        Err(PlanError::Malformed(why)) => return Err(PyValueError::new_err(why)),
        Err(PlanError::Unknown(why)) => {
            return reply(py, tid, &Err(Fallback::new(Reason::Plan, why)));
        }
    };
    // The version holds each commit before `tid`.
    let at = before.saturating_sub(1);
    let reads = Loads { load, tid: at };
    let answer = py.detach(|| {
        catch_unwind(AssertUnwindSafe(|| {
            // The loads take the GIL in turn, so one thread walks the
            // trees.
            let built = match build(&reads, oid, at, 1, &|_| true) {
                Ok(built) => built,
                Err(BuildError::Refused(why)) => {
                    return Err(Fallback::new(Reason::Refused, why));
                }
                Err(BuildError::Dropped) => {
                    return Err(Fallback::new(Reason::Dropped, "no room for the build"));
                }
            };
            let empty = Overrides::default();
            View::new(&built.base, &empty).check_kinds(&plan, &kinds)?;
            let over = match records.as_deref().filter(|r| !r.is_empty()) {
                None => empty,
                Some(records) => zoe_catalog::overlay::read(
                    &built.base,
                    &empty,
                    &built.shapes,
                    &built.objects,
                    &HashMap::new(),
                    at,
                    records,
                )?,
            };
            // The order of an order clause, through `load` as the trees.
            let list = match &order {
                Some(order) => Some(zoe_catalog::order::read(
                    &reads,
                    order,
                    before,
                    records.as_deref(),
                )?),
                None => None,
            };
            let mut view = View::new(&built.base, &over);
            if let Some(list) = &list {
                view = view.ordered(list);
            }
            view.query_rows(&plan, &columns)
        }))
        .unwrap_or_else(|_| Err(Fallback::new(Reason::Refused, "the query panicked")))
    });
    reply(py, tid, &answer)
}
