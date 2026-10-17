//! The exceptions that `zoe.Storage` raises: the exceptions of ZODB, and
//! `zoe.WrongThread`. The native client also raises `ClientDisconnected`
//! of ZEO, and `LockError` of `zc.lockfile` for a cache file that another
//! open holds.
//!
//! The module imports ZODB and ZEO only when it raises an exception. So
//! loading the extension does not load them.

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::{PyBytes, PyDict, PyType};
use zoe_core::{Oid, Tid, ZoeError};

/// Define a function that returns an exception class of Python. The
/// function imports the class once.
macro_rules! exception {
    ($function:ident, $module:literal, $name:literal) => {
        fn $function(py: Python<'_>) -> PyResult<&Bound<'_, PyType>> {
            static CLASS: PyOnceLock<Py<PyType>> = PyOnceLock::new();
            CLASS.import(py, $module, $name)
        }
    };
}

exception!(pos_key_error, "ZODB.POSException", "POSKeyError");
exception!(conflict_error, "ZODB.POSException", "ConflictError");
exception!(
    read_conflict_error,
    "ZODB.POSException",
    "ReadConflictError"
);
exception!(storage_error, "ZODB.POSException", "StorageError");
exception!(
    storage_system_error,
    "ZODB.POSException",
    "StorageSystemError"
);
exception!(
    storage_transaction_error,
    "ZODB.POSException",
    "StorageTransactionError"
);
exception!(read_only_error, "ZODB.POSException", "ReadOnlyError");
exception!(unsupported, "ZODB.POSException", "Unsupported");
exception!(wrong_thread, "zoe.storage", "WrongThread");
exception!(
    client_disconnected_error,
    "ZEO.Exceptions",
    "ClientDisconnected"
);

/// Return the class `ConflictError` of ZODB.
pub(crate) fn conflict_class(py: Python<'_>) -> PyResult<&Bound<'_, PyType>> {
    conflict_error(py)
}

/// Return the class `ReadConflictError` of ZODB.
pub(crate) fn read_conflict_class(py: Python<'_>) -> PyResult<&Bound<'_, PyType>> {
    read_conflict_error(py)
}

exception!(lock_error, "zc.lockfile", "LockError");

/// Return a `LockError` of `zc.lockfile` for the cache file `path`, which
/// another open holds, with the text of `zc.lockfile`.
pub(crate) fn lock_failure(py: Python<'_>, path: &std::path::Path) -> PyErr {
    let text = format!("Couldn't lock '{}'", path.display());
    raise(lock_error(py), (text,), None)
}

/// Return a `ClientDisconnected` of ZEO with the text `text`.
pub(crate) fn client_disconnected(py: Python<'_>, text: String) -> PyErr {
    raise(client_disconnected_error(py), (text,), None)
}

/// Return a `StorageError` of ZODB with the text `text`.
pub(crate) fn storage_failure(py: Python<'_>, text: String) -> PyErr {
    raise(storage_error(py), (text,), None)
}

/// Return a `StorageTransactionError` of ZODB with the text `text`.
pub(crate) fn transaction_failure(py: Python<'_>, text: String) -> PyErr {
    raise(storage_transaction_error(py), (text,), None)
}

/// Return an `Unsupported` of ZODB with the text `text`.
pub(crate) fn unsupported_failure(py: Python<'_>, text: String) -> PyErr {
    raise(unsupported(py), (text,), None)
}

/// A core error, with the details of a vote that failed on a conflict.
pub(crate) struct Failure {
    pub(crate) error: ZoeError,
    /// The current serial, then the serial that the transaction read.
    pub(crate) serials: Option<(Tid, Tid)>,
    /// The pickle that the transaction stored for the oid in conflict.
    pub(crate) data: Option<Vec<u8>>,
}

impl From<ZoeError> for Failure {
    fn from(error: ZoeError) -> Self {
        Self {
            error,
            serials: None,
            data: None,
        }
    }
}

impl From<std::io::Error> for Failure {
    fn from(error: std::io::Error) -> Self {
        ZoeError::from(error).into()
    }
}

impl Failure {
    /// Return the exception of ZODB for this failure.
    pub(crate) fn into_py(self, py: Python<'_>) -> PyErr {
        match self.error {
            ZoeError::NotFound { oid } => raise(pos_key_error(py), (p64(py, oid.0),), None),
            ZoeError::Conflict { oid } => conflict(
                py,
                conflict_error(py),
                oid,
                self.serials,
                self.data.as_deref(),
            ),
            ZoeError::ReadConflict { oid } => {
                conflict(py, read_conflict_error(py), oid, self.serials, None)
            }
            ZoeError::WrongThread => message(wrong_thread(py), &self.error),
            ZoeError::Protocol(_) | ZoeError::LockOrder { .. } => {
                message(storage_transaction_error(py), &self.error)
            }
            ZoeError::Io(error) => error.into(),
            ZoeError::InvalidKv(_) => PyValueError::new_err(self.error.to_string()),
            ZoeError::Locked
            | ZoeError::Closed
            | ZoeError::Format(_)
            | ZoeError::ReadOnly
            | ZoeError::NotClosedCleanly
            | ZoeError::RepairFailed(_)
            | ZoeError::TooLarge { .. }
            | ZoeError::KvTooLarge { .. }
            | ZoeError::MissingBlobDir { .. }
            | ZoeError::NotBlobDir { .. } => message(storage_error(py), &self.error),
            ZoeError::Halted
            | ZoeError::CorruptRecord { .. }
            | ZoeError::CorruptBlob { .. }
            | ZoeError::CorruptTxn { .. }
            | ZoeError::CorruptSequence { .. }
            | ZoeError::CorruptKv { .. }
            | ZoeError::Backend(_) => message(storage_system_error(py), &self.error),
        }
    }
}

/// Return the exception for a pack that failed with `error`.
///
/// A record that fails its checksum stops the pack with `CorruptRecord`.
/// A record or a row that the pack cannot read stops it with `Format`.
/// Both raise `StorageSystemError`. The text starts with "pack stopped"
/// and gives the text of the core, which names the record or the row.
/// Other errors map as for every call.
pub(crate) fn pack_failure(py: Python<'_>, error: ZoeError) -> PyErr {
    let text = match &error {
        ZoeError::CorruptRecord { .. } => format!("pack stopped: {error}"),
        ZoeError::Format(text) => format!("pack stopped: {text}"),
        _ => return Failure::from(error).into_py(py),
    };
    raise(storage_system_error(py), (text,), None)
}

/// Return the exception for a call on a storage that is closed.
pub(crate) fn closed(py: Python<'_>) -> PyErr {
    raise(storage_error(py), ("the storage is closed",), None)
}

/// Return the exception for a transaction status that the storage does
/// not take.
pub(crate) fn status(py: Python<'_>, status: &Bound<'_, PyAny>) -> PyErr {
    let message = match status.repr() {
        Ok(repr) => format!("transaction status {repr} is not supported"),
        Err(_) => "transaction status is not supported".to_owned(),
    };
    raise(storage_transaction_error(py), (message,), None)
}

/// Return the exception for a transaction that the storage does not
/// know.
pub(crate) fn unknown_transaction(py: Python<'_>) -> PyErr {
    raise(
        storage_transaction_error(py),
        ("the storage did not begin this transaction",),
        None,
    )
}

/// Return the exception for a second `tpc_begin` of a transaction.
pub(crate) fn duplicate_transaction(py: Python<'_>) -> PyErr {
    raise(
        storage_transaction_error(py),
        ("duplicate tpc_begin calls for the same transaction",),
        None,
    )
}

/// Return the exception for a `tpc_finish` in a `tpc_finish` callback of
/// the same storage.
pub(crate) fn commit_in_callback(py: Python<'_>) -> PyErr {
    raise(
        storage_transaction_error(py),
        ("a tpc_finish callback cannot commit in the same storage",),
        None,
    )
}

/// Return the exception for a write to a read-only storage.
pub(crate) fn read_only(py: Python<'_>) -> PyErr {
    raise(read_only_error(py), (), None)
}

/// Return the exception for a store with a version.
pub(crate) fn version(py: Python<'_>) -> PyErr {
    raise(unsupported(py), ("versions are not supported",), None)
}

/// Return the 8 big-endian bytes of `value`, as ZODB writes an oid or a
/// tid.
pub(crate) fn p64(py: Python<'_>, value: u64) -> Bound<'_, PyBytes> {
    PyBytes::new(py, &value.to_be_bytes())
}

/// Read an oid or a tid from 8 big-endian bytes.
pub(crate) fn u64_of(value: &Bound<'_, PyAny>) -> PyResult<u64> {
    let bytes = value.cast::<PyBytes>()?.as_bytes();
    let array: [u8; 8] = bytes
        .try_into()
        .map_err(|_| PyValueError::new_err("an oid or a tid has 8 bytes"))?;
    Ok(u64::from_be_bytes(array))
}

/// Return a `ConflictError` of the class `class` on `oid`, with the
/// serials when the vote gave them. ZODB reads the class name of the
/// object from `data`, the pickle that the transaction stored. If it cannot
/// read it, the exception has no `data`.
pub(crate) fn conflict(
    py: Python<'_>,
    class: PyResult<&Bound<'_, PyType>>,
    oid: Oid,
    serials: Option<(Tid, Tid)>,
    data: Option<&[u8]>,
) -> PyErr {
    let class = match class {
        Ok(class) => class,
        Err(error) => return error,
    };
    let make = |data: Option<&[u8]>| -> PyResult<PyErr> {
        let kwargs = PyDict::new(py);
        kwargs.set_item("oid", p64(py, oid.0))?;
        if let Some((current, read)) = serials {
            kwargs.set_item("serials", (p64(py, current.0), p64(py, read.0)))?;
        }
        if let Some(data) = data {
            kwargs.set_item("data", PyBytes::new(py, data))?;
        }
        Ok(PyErr::from_value(class.call((), Some(&kwargs))?))
    };
    let made = match make(data) {
        Err(_) if data.is_some() => make(None),
        made => made,
    };
    made.unwrap_or_else(|error| error)
}

/// Return an exception of the class `class` with the text of `error`.
fn message(class: PyResult<&Bound<'_, PyType>>, error: &ZoeError) -> PyErr {
    raise(class, (error.to_string(),), None)
}

/// Return an exception of the class `class`, made from `args` and
/// `kwargs`. An error in the import or in the call comes back instead.
pub(crate) fn raise<'py, A>(
    class: PyResult<&Bound<'py, PyType>>,
    args: A,
    kwargs: Option<&Bound<'py, PyDict>>,
) -> PyErr
where
    A: pyo3::call::PyCallArgs<'py>,
{
    match class.and_then(|class| class.call(args, kwargs)) {
        Ok(value) => PyErr::from_value(value),
        Err(error) => error,
    }
}
