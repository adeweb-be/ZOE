//! The catalog mirrors of a `zoe.Storage` (ADR 0059): the plan of
//! `catalog_query` from a Python dict, its reply as a dict with the rows of
//! the metadata table as Python values, the state and the counts of the
//! mirrors, and the log of their events.
//!
//! The mirrors of a store live in `zoe-catalog`. `storage` starts them at
//! the open of a store whose namespace `zoe.catalog` holds a registration
//! key, or at the first write of that namespace, never at import. It hands
//! each commit to them after its publication.

use pyo3::IntoPyObjectExt;
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyBytes, PyDict, PyFloat, PyInt, PyList, PyString, PyTuple};
use zoe_catalog::cell::{self, EXT_DATE_TIME, EXT_MESSAGE, EXT_MISSING};
use zoe_catalog::counts::{Counts, LATENCY_BUCKETS};
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

/// The reply of a query at `tid`, as a dict: `r` as a list of ints, `n`,
/// `a`, `rows` and `paths`, or `f` and `why`. `rows` holds a tuple of the
/// cells of each of the first rids, or `None` for a rid whose brain comes
/// from `data`; `paths` holds the path of each. Both are `None` when the
/// plan names no columns.
pub(crate) fn reply<'py>(
    py: Python<'py>,
    tid: &[u8],
    answer: &Result<Reply, Fallback>,
) -> PyResult<Bound<'py, PyDict>> {
    let out = PyDict::new(py);
    out.set_item("tid", PyBytes::new(py, tid))?;
    match answer {
        Ok(reply) => {
            out.set_item("r", PyList::new(py, &reply.rids)?)?;
            out.set_item("n", reply.len)?;
            out.set_item("a", reply.actual)?;
            match (&reply.rows, &reply.paths) {
                (Some(rows), Some(paths)) => {
                    let mut classes = Classes::default();
                    let made = PyList::empty(py);
                    for each in rows {
                        made.append(match each {
                            Some(bytes) => row(py, &mut classes, bytes),
                            None => py.None().into_bound(py),
                        })?;
                    }
                    out.set_item("rows", made)?;
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
