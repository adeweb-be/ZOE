//! The conversions of a native client: the values of Python to msgpack and
//! back, the copy of a cache entry into `bytes`, and the exceptions of
//! Python for the errors of the client.

use pyo3::exceptions::{PyException, PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{
    PyBool, PyBytes, PyDict, PyFloat, PyInt, PyList, PyModule, PyString, PyTuple, PyType,
};
use rmpv::Value;
use zoe_client::{Copier, Error};

use crate::errors;

/// The deepest nesting of a value that the client converts.
const DEPTH: usize = 32;

/// The modules whose exception classes a server error can name. Another
/// class raises `StorageError`.
const MODULES: [&str; 4] = [
    "ZODB.POSException",
    "ZEO.Exceptions",
    "transaction.interfaces",
    "builtins",
];

/// The `out` of a lookup of the cache: a `bytes` of `len` bytes that
/// `copy` writes and checks, or `None` when the check fails. The writer
/// does not fill the bytes with zeros first, so the data takes one pass.
pub(super) fn bytes<'py>(
    py: Python<'py>,
    len: usize,
    copy: &mut Copier<'_>,
) -> Option<Bound<'py, PyBytes>> {
    PyBytes::new_with_writer(py, len, |writer| {
        let mut written = true;
        let good = copy(&mut |block| written &= writer.write_bytes(block).is_ok());
        if good && written {
            Ok(())
        } else {
            Err(PyValueError::new_err("the entry changed during the copy"))
        }
    })
    .ok()
}

/// Return the exception of Python for an error of a blob call. An error of
/// a file there (`Error::Io`) raises `OSError`. Another error raises as
/// `client_error` gives it.
pub(super) fn blob_error(py: Python<'_>, error: Error) -> PyErr {
    match error {
        Error::Io(error) => error.into(),
        error => client_error(py, error),
    }
}

/// Return the exception of Python for an error of the client.
pub(super) fn client_error(py: Python<'_>, error: Error) -> PyErr {
    match error {
        Error::Conflict {
            oid,
            serials,
            class_name,
        } => {
            let error = errors::conflict(py, errors::conflict_class(py), oid, serials, None);
            // ZEO gives the class that the server names to the error, so
            // its text names the class of the object.
            if let Some(class_name) = class_name {
                let _ = error.value(py).setattr("class_name", class_name);
            }
            error
        }
        Error::ReadConflict { oid, serials } => {
            errors::conflict(py, errors::read_conflict_class(py), oid, serials, None)
        }
        Error::Disconnected => {
            errors::client_disconnected(py, "the connection to the server is lost".into())
        }
        Error::Io(error) => errors::client_disconnected(py, error.to_string()),
        // A class of the factories of ZEO's client gets what its factory
        // makes of the value, as from `ZEO.ClientStorage`: for example a
        // `MultipleUndoErrors` with its `_errs`.
        Error::Server {
            class,
            value,
            message,
        } => factory_error(py, &class, &value).unwrap_or_else(|| server_error(py, &class, message)),
        Error::Unsupported(text) => errors::unsupported_failure(py, text),
        Error::Invalid(text) => errors::transaction_failure(py, text),
        error => errors::storage_failure(py, error.to_string()),
    }
}

/// Return the exception of Python for an error of the client, as
/// `ZEO.ClientStorage` raises it for the reply of a call: an error of the
/// server with a class of the `exc_factories` of `ZEO.asyncio.client`
/// raises what its factory makes of the value, and another class raises
/// `ZEO.Exceptions.ServerException(class, value)`. Each other error raises
/// as `client_error` gives it.
pub(super) fn zeo_error(py: Python<'_>, name: &str, error: Error) -> PyErr {
    let Error::Server {
        class,
        value,
        message,
    } = error
    else {
        return client_error(py, error);
    };
    let made = (|| -> PyResult<PyErr> {
        let value = to_python(py, &value, 0)?;
        let (client, factory) = factory(py, &class)?;
        let exception = match factory {
            Some(factory) => {
                let exception = factory.call1((&class, &value))?;
                // ZEO logs an error of the server that its factories make,
                // but a POSKeyError and a ConflictError.
                if !exception.is_instance(&client.getattr("unlogged_exceptions")?)? {
                    client
                        .getattr("logger")?
                        .call_method1("error", ("%s from server: %s:%s", name, &class, &value))?;
                }
                exception
            }
            None => py
                .import("ZEO.Exceptions")?
                .getattr("ServerException")?
                .call1((&class, value))?,
        };
        Ok(PyErr::from_value(exception))
    })();
    made.unwrap_or_else(|_| server_error(py, &class, message))
}

/// Return the exception that the factory of `ZEO.asyncio.client` of the
/// class `class` makes of the msgpack value `value`, or `None` when no
/// factory has the class or the factory fails.
fn factory_error(py: Python<'_>, class: &str, value: &Value) -> Option<PyErr> {
    let made = (|| -> PyResult<Option<PyErr>> {
        let Some(factory) = factory(py, class)?.1 else {
            return Ok(None);
        };
        let value = to_python(py, value, 0)?;
        Ok(Some(PyErr::from_value(factory.call1((class, value))?)))
    })();
    made.ok().flatten()
}

/// Return the module `ZEO.asyncio.client` and the factory of its
/// `exc_factories` for the class `class`, or `None` when no factory has the
/// class.
fn factory<'py>(
    py: Python<'py>,
    class: &str,
) -> PyResult<(Bound<'py, PyModule>, Option<Bound<'py, PyAny>>)> {
    let client = py.import("ZEO.asyncio.client")?;
    let factory = client.getattr("exc_factories")?.get_item(class).ok();
    Ok((client, factory))
}

/// Return the msgpack values of the args of a call: a list or a tuple.
pub(super) fn args_of(args: &Bound<'_, PyAny>) -> PyResult<Vec<Value>> {
    match to_value(args, 0)? {
        Value::Array(items) => Ok(items),
        _ => Err(PyTypeError::new_err(
            "the args of a call are a list or a tuple",
        )),
    }
}

/// Return the exception of the server error of the class `class`. The
/// class must be an exception of `MODULES`; another class raises
/// `StorageError` with the class and the message.
fn server_error(py: Python<'_>, class: &str, message: String) -> PyErr {
    let found = class
        .rsplit_once('.')
        .filter(|(module, _)| MODULES.contains(module))
        .and_then(|(module, name)| {
            let class = py.import(module).ok()?.getattr(name).ok()?;
            let class = class.cast_into::<PyType>().ok()?;
            let exception = class.is_subclass_of::<PyException>().ok()?;
            exception.then_some(class)
        });
    match found {
        Some(found) => errors::raise(Ok(&found), (message,), None),
        None => errors::storage_failure(py, format!("{class}: {message}")),
    }
}

/// Return the msgpack value of a Python value: `None`, a bool, an int, a
/// float, a str, bytes, a list or a tuple, or a dict.
pub(super) fn to_value(object: &Bound<'_, PyAny>, depth: usize) -> PyResult<Value> {
    if depth > DEPTH {
        return Err(PyValueError::new_err("the value is nested too deep"));
    }
    if object.is_none() {
        return Ok(Value::Nil);
    }
    if let Ok(flag) = object.cast::<PyBool>() {
        return Ok(Value::Boolean(flag.is_true()));
    }
    if object.is_instance_of::<PyInt>() {
        if let Ok(n) = object.extract::<i64>() {
            return Ok(Value::from(n));
        }
        return Ok(Value::from(object.extract::<u64>()?));
    }
    if let Ok(float) = object.cast::<PyFloat>() {
        return Ok(Value::F64(float.value()));
    }
    if let Ok(text) = object.cast::<PyString>() {
        return Ok(Value::from(text.to_str()?));
    }
    if let Ok(bytes) = object.cast::<PyBytes>() {
        return Ok(Value::Binary(bytes.as_bytes().to_vec()));
    }
    if object.is_instance_of::<PyList>() || object.is_instance_of::<PyTuple>() {
        let items = object.try_iter()?;
        return items
            .map(|item| to_value(&item?, depth + 1))
            .collect::<PyResult<_>>()
            .map(Value::Array);
    }
    if let Ok(map) = object.cast::<PyDict>() {
        return map
            .iter()
            .map(|(key, value)| Ok((to_value(&key, depth + 1)?, to_value(&value, depth + 1)?)))
            .collect::<PyResult<_>>()
            .map(Value::Map);
    }
    let name = object.get_type().name()?;
    Err(PyTypeError::new_err(format!(
        "a value of the type {name} does not go to the server"
    )))
}

/// Return the Python value of a msgpack value. An array becomes a tuple,
/// and a map a dict. A str that is not UTF-8 becomes bytes.
pub(super) fn to_python<'py>(
    py: Python<'py>,
    value: &Value,
    depth: usize,
) -> PyResult<Bound<'py, PyAny>> {
    if depth > DEPTH {
        return Err(PyValueError::new_err("the value is nested too deep"));
    }
    Ok(match value {
        Value::Nil => py.None().into_bound(py),
        Value::Boolean(flag) => PyBool::new(py, *flag).to_owned().into_any(),
        Value::Integer(n) => match (n.as_i64(), n.as_u64()) {
            (Some(n), _) => n.into_pyobject(py)?.into_any(),
            (None, Some(n)) => n.into_pyobject(py)?.into_any(),
            (None, None) => py.None().into_bound(py),
        },
        Value::F32(x) => PyFloat::new(py, f64::from(*x)).into_any(),
        Value::F64(x) => PyFloat::new(py, *x).into_any(),
        Value::String(text) => match text.as_str() {
            Some(text) => PyString::new(py, text).into_any(),
            None => PyBytes::new(py, text.as_bytes()).into_any(),
        },
        Value::Binary(bytes) | Value::Ext(_, bytes) => PyBytes::new(py, bytes).into_any(),
        Value::Array(items) => {
            let items = items
                .iter()
                .map(|item| to_python(py, item, depth + 1))
                .collect::<PyResult<Vec<_>>>()?;
            PyTuple::new(py, items)?.into_any()
        }
        Value::Map(entries) => {
            let map = PyDict::new(py);
            for (key, value) in entries {
                map.set_item(
                    to_python(py, key, depth + 1)?,
                    to_python(py, value, depth + 1)?,
                )?;
            }
            map.into_any()
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_exceptions_of_known_modules_are_raised() {
        for module in ["ZODB.POSException", "ZEO.Exceptions", "builtins"] {
            assert!(MODULES.contains(&module));
        }
        assert!(!MODULES.contains(&"os"));
    }
}
