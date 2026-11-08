//! The text calls of the storages (ADR 0060, sections 8 and 12): the
//! arguments from Python, the replies as dicts, and the exceptions.
//!
//! - `unavailable` and `too_large` raise `zoe.storage.Unavailable`, a
//!   `StorageError`; `packed` and `cleared` raise `ReadConflictError`;
//!   `corrupt` raises `StorageSystemError`.
//! - A clause or a filter of another form raises `ValueError`, and the
//!   compatibility mode raises `Unsupported`.
//! - Each rid from Python goes through `zoe_core::check_rid`: a rid outside
//!   the signed 32-bit range raises `ValueError`.

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyList};
use zoe_catalog::Wire;
use zoe_catalog::text::clause::{TextErrorKind, TextHits, TextInfo};
use zoe_catalog::text::{CallError, TextError};

use crate::errors;

/// The rid `value`, a Python int, as the rid of a delta. Another int, or a
/// value that is not an int, raises `ValueError`.
pub(crate) fn rid(value: &Bound<'_, PyAny>) -> PyResult<i32> {
    let wide = value.extract::<i64>().unwrap_or(i64::MAX);
    zoe_core::check_rid(wide).map_err(|e| PyValueError::new_err(e.to_string()))
}

/// The filter of `text_query`: `None`, or the rids of an iterable of ints,
/// as the bin of the wire, sorted.
pub(crate) fn filter(value: Option<&Bound<'_, PyAny>>) -> PyResult<Wire> {
    let Some(value) = value.filter(|v| !v.is_none()) else {
        return Ok(Wire::Nil);
    };
    let mut rids = Vec::new();
    for item in value.try_iter()? {
        rids.push(rid(&item?)?);
    }
    rids.sort_unstable();
    rids.dedup();
    Ok(zoe_catalog::text::clause::rids_bin(rids))
}

/// The exception of the error `error` of a text clause.
pub(crate) fn error(py: Python<'_>, error: &TextError) -> PyErr {
    match error.kind {
        TextErrorKind::Unavailable | TextErrorKind::TooLarge => {
            errors::unavailable_failure(py, error.why.clone())
        }
        TextErrorKind::Packed | TextErrorKind::Cleared => {
            errors::read_conflict_failure(py, error.why.clone())
        }
        TextErrorKind::Corrupt => errors::storage_system_failure(py, error.why.clone()),
    }
}

/// The exception of a text call that fails as a call.
pub(crate) fn call_error(py: Python<'_>, error: CallError) -> PyErr {
    match error {
        CallError::Malformed(why) => PyValueError::new_err(why),
        CallError::Unsupported(why) => errors::unsupported_failure(py, why),
    }
}

/// The reply of `text_query` as a dict: `tid`, `r`, the rids as a list of
/// ints in ascending order, and `sc`, the integer score of each.
pub(crate) fn hits<'py>(py: Python<'py>, tid: &[u8], hits: &TextHits) -> PyResult<Bound<'py, PyDict>> {
    let out = PyDict::new(py);
    out.set_item("tid", PyBytes::new(py, tid))?;
    out.set_item("r", PyList::new(py, &hits.rids)?)?;
    out.set_item("sc", PyList::new(py, &hits.scores)?)?;
    Ok(out)
}

/// The reply of `text_info` as a dict: `tid`, `docs`, `words`, and `lang`
/// and `text`, each a str or `None`.
pub(crate) fn info<'py>(py: Python<'py>, tid: &[u8], info: &TextInfo) -> PyResult<Bound<'py, PyDict>> {
    let out = PyDict::new(py);
    out.set_item("tid", PyBytes::new(py, tid))?;
    out.set_item("docs", info.docs)?;
    out.set_item("words", info.words)?;
    match &info.text {
        Some((lang, text)) => {
            out.set_item("lang", lang)?;
            out.set_item("text", text)?;
        }
        None => {
            out.set_item("lang", py.None())?;
            out.set_item("text", py.None())?;
        }
    }
    Ok(out)
}

/// The error form `{"tid", "e", "why"}` of a reply of the server, or
/// `None` for another form.
pub(crate) fn error_of_value(value: &rmpv::Value) -> Option<TextError> {
    let rmpv::Value::Map(items) = value else {
        return None;
    };
    let get = |name: &str| {
        items
            .iter()
            .find(|(k, _)| k.as_str() == Some(name))
            .map(|(_, v)| v)
    };
    let kind = TextErrorKind::from_name(get("e")?.as_str()?)?;
    let why = get("why").and_then(|w| w.as_str()).unwrap_or("").to_owned();
    Some(TextError::new(kind, why))
}

/// The ints of `bytes`, each a signed 32-bit big-endian int.
fn ints(bytes: &[u8]) -> PyResult<Vec<i32>> {
    if !bytes.len().is_multiple_of(4) {
        return Err(PyValueError::new_err("a text reply with ints that are not 4 bytes each"));
    }
    Ok(bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| i32::from_be_bytes(*c))
        .collect())
}

/// The reply `value` of `text_query` over the wire, as `hits` gives the
/// reply of the process. The error form raises.
pub(crate) fn hits_of_value<'py>(
    py: Python<'py>,
    tid: &[u8],
    value: &rmpv::Value,
) -> PyResult<Bound<'py, PyDict>> {
    if let Some(e) = error_of_value(value) {
        return Err(error(py, &e));
    }
    let bad = || PyValueError::new_err("a text reply of another form");
    let rmpv::Value::Map(items) = value else {
        return Err(bad());
    };
    let get = |name: &str| {
        items
            .iter()
            .find(|(k, _)| k.as_str() == Some(name))
            .map(|(_, v)| v)
    };
    let (Some(rmpv::Value::Binary(r)), Some(rmpv::Value::Binary(sc))) = (get("r"), get("sc")) else {
        return Err(bad());
    };
    let found = TextHits {
        rids: ints(r)?,
        scores: ints(sc)?,
    };
    hits(py, tid, &found)
}

/// The reply `value` of `text_info` over the wire, as `info` gives the
/// reply of the process. The error form raises.
pub(crate) fn info_of_value<'py>(
    py: Python<'py>,
    tid: &[u8],
    value: &rmpv::Value,
) -> PyResult<Bound<'py, PyDict>> {
    if let Some(e) = error_of_value(value) {
        return Err(error(py, &e));
    }
    let bad = || PyValueError::new_err("a text reply of another form");
    let rmpv::Value::Map(items) = value else {
        return Err(bad());
    };
    let get = |name: &str| {
        items
            .iter()
            .find(|(k, _)| k.as_str() == Some(name))
            .map(|(_, v)| v)
    };
    let count = |name: &str| get(name).and_then(rmpv::Value::as_u64).ok_or_else(bad);
    let text = match (get("lang"), get("text")) {
        (Some(rmpv::Value::String(l)), Some(rmpv::Value::String(t))) => Some((
            l.as_str().ok_or_else(bad)?.to_owned(),
            t.as_str().ok_or_else(bad)?.to_owned(),
        )),
        _ => None,
    };
    let found = TextInfo {
        docs: count("docs")?,
        words: count("words")?,
        text,
    };
    info(py, tid, &found)
}

/// The scores `sc` of a reply of `catalog_query` over the wire, or the
/// exception of its error form.
pub(crate) fn scores_of_value(py: Python<'_>, value: &rmpv::Value) -> PyResult<Option<Vec<i32>>> {
    if let Some(e) = error_of_value(value) {
        return Err(error(py, &e));
    }
    let rmpv::Value::Map(items) = value else {
        return Ok(None);
    };
    match items.iter().find(|(k, _)| k.as_str() == Some("sc")) {
        Some((_, rmpv::Value::Binary(sc))) => ints(sc).map(Some),
        Some((_, rmpv::Value::Nil)) | None => Ok(None),
        Some(_) => Err(PyValueError::new_err("a catalog reply with sc of another form")),
    }
}
