//! The arguments of the KV namespaces (ADR 0043), for `zoe.Storage` and
//! for the native client: the flavour and the TTL.

use std::time::Duration;

use pyo3::exceptions::{PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyBool;
use zoe_core::Flavour;

/// Return the flavour named `name`: `"ephemeral"` or `"durable"`. Another
/// name raises `ValueError`.
pub(crate) fn flavour(name: &str) -> PyResult<Flavour> {
    Flavour::from_name(name).ok_or_else(|| {
        PyValueError::new_err(format!(
            "the flavour of a namespace is \"ephemeral\" or \"durable\", not {name:?}"
        ))
    })
}

/// Return the TTL `ttl` in seconds: an int or a float, or `None` for no
/// TTL. A bool raises `TypeError`. A negative or not finite number raises
/// `ValueError`. The core checks the bounds of the TTL.
pub(crate) fn ttl_seconds(ttl: &Bound<'_, PyAny>) -> PyResult<Option<f64>> {
    if ttl.is_none() {
        return Ok(None);
    }
    if ttl.is_instance_of::<PyBool>() {
        return Err(PyTypeError::new_err(
            "a TTL is an int or a float, not a bool",
        ));
    }
    let seconds: f64 = ttl
        .extract()
        .map_err(|_| PyTypeError::new_err("a TTL is an int, a float or None"))?;
    if !seconds.is_finite() || seconds < 0.0 {
        return Err(PyValueError::new_err(
            "a TTL is a number of seconds from 0 to 2^32 - 1",
        ));
    }
    Ok(Some(seconds))
}

/// Return the TTL `ttl` as a duration, as `ttl_seconds` reads it.
pub(crate) fn ttl(ttl: &Bound<'_, PyAny>) -> PyResult<Option<Duration>> {
    ttl_seconds(ttl)?
        .map(|seconds| {
            Duration::try_from_secs_f64(seconds).map_err(|_| {
                PyValueError::new_err("a TTL is a number of seconds from 0 to 2^32 - 1")
            })
        })
        .transpose()
}
