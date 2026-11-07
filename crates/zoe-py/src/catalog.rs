//! The catalog mirrors of a `zoe.Storage` (ADR 0059): the plan of
//! `catalog_query` from a Python dict, its reply as a dict, the state and
//! the counts of the mirrors, and the log of their events.
//!
//! The mirrors of a store live in `zoe-catalog`. `storage` starts them at
//! the open of a store whose namespace `zoe.catalog` holds a registration
//! key, or at the first write of that namespace, never at import. It hands
//! each commit to them after its publication.

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyBytes, PyDict, PyFloat, PyInt, PyList, PyString, PyTuple};
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

/// The reply of a query at `tid`, as a dict: `r` as a list of ints, `n`
/// and `a`, or `f` and `why`.
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
