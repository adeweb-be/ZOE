//! `zoe._zoe.Reader`: the reader of ZOE, which decodes a ZODB record in
//! Rust (ADR 0057). It is the base class of `zoe.reader.Reader`, which also
//! derives from ZODB's `ObjectReader`. `Connection.setstate` calls its
//! `setGhostState`.
//!
//! The reader decodes the class pickle and the state pickle of a record
//! with the opcodes that ZODB writes at protocol 3, and builds the objects
//! as the C unpickler of zodbpickle builds them. It finds a global through
//! the class factory of the connection once, then in a table. It resolves
//! a persistent reference in the pickle cache, as
//! `ObjectReader._persistent_load` does. A record that the Rust path does
//! not decode goes to the Python path of `ObjectReader` whole: another
//! opcode, a protocol above 3, a class that the factory gives as `Broken`,
//! a weak reference, a reference into another database, another persistent
//! id, a Python 2 str that is not ASCII, an int of more than 16 bytes,
//! malformed bytes, and an exception of a call into Python.
//!
//! `setstate`, the `setstate` of `zoe.reader.Connection`, does the lines of
//! `Connection.setstate` of ZODB for an ordinary record in one call: the
//! load through the storage, the bookkeeping, and the decode of the reader
//! without a call through Python. A closed connection and a `Blob` go to
//! the method of ZODB.
//!
//! The reader adds no lock to the pickle cache. One thread uses a
//! connection and its reader at a time. The table of globals has a mutex,
//! held for a lookup or an insert only, and no Python code runs while a
//! call holds it.

use std::collections::HashMap;
use std::ffi::c_char;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use pyo3::exceptions::{PyAttributeError, PyException, PyTypeError, PyValueError};
use pyo3::ffi;
use pyo3::intern;
use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::{PyBool, PyBytes, PyDict, PyFloat, PyList, PyString, PyTuple, PyType};
use zoe_pickle::opcode as op;

/// The highest memo index that the reader keeps. A record with a higher
/// index takes the Python path.
const MEMO_LIMIT: usize = 1 << 20;

/// Why a record takes the Python path.
#[derive(Clone, Copy)]
enum Reason {
    Opcode,
    Protocol,
    Malformed,
    Broken,
    Weak,
    OtherDatabase,
    Reference,
    BigInt,
    Py2Str,
    NotBytes,
    Error,
}

/// The names of the reasons, in the order of `Reason`.
const REASONS: [&str; 11] = [
    "opcode",
    "protocol",
    "malformed",
    "broken",
    "weak reference",
    "other database",
    "reference",
    "big int",
    "python 2 str",
    "not bytes",
    "error",
];

static DECODED: AtomicU64 = AtomicU64::new(0);
static FALLBACKS: [AtomicU64; REASONS.len()] = [const { AtomicU64::new(0) }; REASONS.len()];
static OPCODES: [AtomicU64; 256] = [const { AtomicU64::new(0) }; 256];

/// The end of a decode that does not give a state.
enum Stop {
    /// The record takes the Python path.
    Fallback(Reason),
    /// A call into Python raised.
    Error(PyErr),
}

impl From<PyErr> for Stop {
    fn from(err: PyErr) -> Self {
        Stop::Error(err)
    }
}

fn fallback<T>(reason: Reason) -> Result<T, Stop> {
    Err(Stop::Fallback(reason))
}

/// The base of `zoe.reader.Reader`. The Python subclass gives the
/// `__dict__` that holds `_conn`, `_cache` and `_factory`, as
/// `ObjectReader.__init__` sets them.
#[pyclass(frozen, subclass, module = "zoe._zoe", name = "Reader")]
pub(crate) struct Reader {
    /// The globals that the class factory gave, by the bytes of their
    /// module line and name line.
    globals: Mutex<HashMap<Box<[u8]>, Py<PyAny>>>,
    /// `ZODB.broken.Broken`.
    broken: Py<PyType>,
    /// `ObjectReader.getState`, the Python path.
    python_get_state: Py<PyAny>,
    /// `ZODB.POSException.StateLoadError`, when ZODB has it.
    state_load_error: Option<Py<PyType>>,
    /// `ZODB.POSException.POSError`.
    pos_error: Py<PyType>,
    /// `transaction.interfaces.TransientError`.
    transient_error: Py<PyType>,
}

#[pymethods]
impl Reader {
    #[new]
    #[pyo3(signature = (*_args, **_kwargs))]
    fn new(
        py: Python<'_>,
        _args: &Bound<'_, PyTuple>,
        _kwargs: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<Self> {
        let class = |module: &str, name: &str| -> PyResult<Py<PyType>> {
            Ok(py
                .import(module)?
                .getattr(name)?
                .cast_into::<PyType>()?
                .unbind())
        };
        let exceptions = py.import("ZODB.POSException")?;
        let state_load_error = match exceptions.getattr(intern!(py, "StateLoadError")) {
            Ok(value) => Some(value.cast_into::<PyType>()?.unbind()),
            Err(err) if err.is_instance_of::<PyAttributeError>(py) => None,
            Err(err) => return Err(err),
        };
        let reader = py.import("ZODB.serialize")?.getattr("ObjectReader")?;
        Ok(Self {
            globals: Mutex::new(HashMap::new()),
            broken: class("ZODB.broken", "Broken")?,
            python_get_state: reader.getattr("getState")?.unbind(),
            state_load_error,
            pos_error: class("ZODB.POSException", "POSError")?,
            transient_error: class("transaction.interfaces", "TransientError")?,
        })
    }

    /// Decode the state of `pickle` and give it to `obj.__setstate__`, as
    /// `ObjectReader.setGhostState` does.
    #[pyo3(name = "setGhostState")]
    fn set_ghost_state(
        slf: &Bound<'_, Self>,
        obj: &Bound<'_, PyAny>,
        pickle: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        ghost_state(slf, obj, pickle)
    }

    /// Return the state of `pickle`, as `ObjectReader.getState` does.
    #[pyo3(name = "getState")]
    fn get_state<'py>(
        slf: &Bound<'py, Self>,
        pickle: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        state_of(slf, pickle)
    }
}

impl Reader {
    /// The error of a `__setstate__` that raised `err`, as
    /// `ObjectReader.setGhostState` gives it. With a ZODB that has
    /// `StateLoadError`, an `Exception` that is not a `POSError` or a
    /// `TransientError` becomes a `StateLoadError` whose cause is `err`.
    fn state_error(&self, py: Python<'_>, err: PyErr) -> PyErr {
        let Some(wrapper) = &self.state_load_error else {
            return err;
        };
        if !err.is_instance_of::<PyException>(py)
            || err.is_instance(py, self.pos_error.bind(py).as_any())
            || err.is_instance(py, self.transient_error.bind(py).as_any())
        {
            return err;
        }
        match wrapper.bind(py).call0() {
            Ok(value) => {
                let wrapped = PyErr::from_value(value);
                wrapped.set_cause(py, Some(err));
                wrapped
            }
            Err(other) => other,
        }
    }
}

/// Decode the state of `pickle` and give it to `obj.__setstate__`, as
/// `ObjectReader.setGhostState` does.
fn ghost_state(
    reader: &Bound<'_, Reader>,
    obj: &Bound<'_, PyAny>,
    pickle: &Bound<'_, PyAny>,
) -> PyResult<()> {
    let py = reader.py();
    let state = state_of(reader, pickle)?;
    match obj.call_method1(intern!(py, "__setstate__"), (state,)) {
        Ok(_) => Ok(()),
        Err(err) => Err(reader.get().state_error(py, err)),
    }
}

/// The objects that `setstate` uses, imported at its first call.
struct ConnectionNames {
    /// `ZODB.Connection.Connection`.
    zodb_connection: Py<PyType>,
    /// The function `setstate` of ZODB, when `ZODB.Connection.Connection`
    /// held it at the first call.
    zodb_setstate: Option<Py<PyAny>>,
    /// `zoe.zodb.LockingConnection.setstate`, the `setstate` of the early
    /// locks.
    locking_setstate: Py<PyAny>,
    /// `ZODB.blob.Blob`.
    blob: Py<PyType>,
    /// `ZODB.POSException.ConflictError`.
    conflict_error: Py<PyType>,
    /// `ZODB.Connection.className`.
    class_name: Py<PyAny>,
    /// `ZODB.utils.oid_repr`.
    oid_repr: Py<PyAny>,
    /// `zoe.reader.Reader`.
    reader: Py<PyType>,
}

impl ConnectionNames {
    fn import(py: Python<'_>) -> PyResult<Self> {
        let connection = py.import("ZODB.Connection")?;
        let class = |module: &str, name: &str| -> PyResult<Py<PyType>> {
            Ok(py
                .import(module)?
                .getattr(name)?
                .cast_into::<PyType>()?
                .unbind())
        };
        let zodb_connection = connection.getattr("Connection")?.cast_into::<PyType>()?;
        let setstate = zodb_connection.getattr("setstate")?;
        // The function of ZODB, and not a function that replaced it.
        let name = |attribute: &str| -> PyResult<String> {
            setstate.getattr(attribute)?.extract::<String>()
        };
        let zodb_setstate = (name("__module__").is_ok_and(|n| n == "ZODB.Connection")
            && name("__qualname__").is_ok_and(|n| n == "Connection.setstate"))
        .then(|| setstate.clone().unbind());
        Ok(Self {
            zodb_setstate,
            locking_setstate: py
                .import("zoe.zodb")?
                .getattr("LockingConnection")?
                .getattr("setstate")?
                .unbind(),
            zodb_connection: zodb_connection.unbind(),
            blob: class("ZODB.blob", "Blob")?,
            conflict_error: class("ZODB.POSException", "ConflictError")?,
            class_name: connection.getattr("className")?.unbind(),
            oid_repr: py.import("ZODB.utils")?.getattr("oid_repr")?.unbind(),
            reader: class("zoe.reader", "Reader")?,
        })
    }

    /// Whether `conn` loads through the lines of `setstate` in Rust, when
    /// `setstate` is the method of ZODB's `Connection` now. It does with the
    /// function of ZODB, and with the function of the early locks of
    /// `zoe.zodb` for a storage without hot classes, which then calls the
    /// function of ZODB. Another function keeps its lines.
    fn in_rust(&self, setstate: &Bound<'_, PyAny>, conn: &Bound<'_, PyAny>) -> PyResult<bool> {
        if self
            .zodb_setstate
            .as_ref()
            .is_some_and(|own| setstate.is(own))
        {
            return Ok(true);
        }
        if setstate.is(&self.locking_setstate) {
            return Ok(!has_hot_classes(conn)?);
        }
        Ok(false)
    }
}

/// Whether the storage of the database of `conn` has the hot classes of
/// early locks, as `zoe.zodb` reads them.
fn has_hot_classes(conn: &Bound<'_, PyAny>) -> PyResult<bool> {
    let py = conn.py();
    let db = conn.getattr(intern!(py, "_db"))?;
    if db.is_none() {
        return Ok(false);
    }
    let Some(storage) = db.getattr_opt(intern!(py, "storage"))? else {
        return Ok(false);
    };
    match storage.getattr_opt(intern!(py, "early_lock_classes"))? {
        Some(classes) => classes.is_truthy(),
        None => Ok(false),
    }
}

static CONNECTION_NAMES: PyOnceLock<ConnectionNames> = PyOnceLock::new();

/// The `setstate` of `zoe.reader.Connection`: the lines of
/// `Connection.setstate` of ZODB for an ordinary record, in one call. A
/// closed connection, a `Blob`, and a connection whose method of ZODB is
/// another function (`ConnectionNames::in_rust`) go to the method of
/// ZODB's `Connection`. A `ConflictError` leaves as it is. Another
/// exception is logged, as the method of ZODB logs it, then raised.
#[pyfunction]
#[pyo3(name = "setstate", text_signature = "(self, obj)")]
fn connection_setstate(conn: &Bound<'_, PyAny>, obj: &Bound<'_, PyAny>) -> PyResult<()> {
    let py = conn.py();
    let names = CONNECTION_NAMES.get_or_try_init(py, || ConnectionNames::import(py))?;
    let oid = obj.getattr(intern!(py, "_p_oid"))?;
    let setstate = names
        .zodb_connection
        .bind(py)
        .getattr(intern!(py, "setstate"))?;
    if conn.getattr(intern!(py, "opened"))?.is_none()
        || obj.is_instance(names.blob.bind(py))?
        || !names.in_rust(&setstate, conn)?
    {
        setstate.call1((conn, obj))?;
        return Ok(());
    }
    match load_state(names, conn, obj, &oid) {
        Ok(()) => Ok(()),
        Err(err) if err.is_instance(py, names.conflict_error.bind(py)) => Err(err),
        Err(err) => Err(log_load_error(names, conn, obj, &oid, err)),
    }
}

/// The lines of the `try` of `Connection.setstate` of ZODB, but the blob.
fn load_state(
    names: &ConnectionNames,
    conn: &Bound<'_, PyAny>,
    obj: &Bound<'_, PyAny>,
    oid: &Bound<'_, PyAny>,
) -> PyResult<()> {
    let py = conn.py();
    let loaded = conn
        .getattr(intern!(py, "_storage"))?
        .call_method1(intern!(py, "load"), (oid,))?;
    let (pickle, serial) = pair_of(&loaded)?;
    let count = conn.getattr(intern!(py, "_load_count"))?.add(1)?;
    conn.setattr(intern!(py, "_load_count"), count)?;
    let reader = conn.getattr(intern!(py, "_reader"))?;
    if reader.get_type().is(names.reader.bind(py)) {
        // The reader of ZOE: its decode, without a call through Python.
        ghost_state(reader.cast::<Reader>()?, obj, &pickle)?;
    } else {
        reader.call_method1(intern!(py, "setGhostState"), (obj, &pickle))?;
    }
    obj.setattr(intern!(py, "_p_serial"), serial)?;
    let size = pickle.len()?;
    conn.getattr(intern!(py, "_cache"))?
        .call_method1(intern!(py, "update_object_size_estimation"), (oid, size))?;
    obj.setattr(intern!(py, "_p_estimated_size"), size)?;
    Ok(())
}

/// `pickle, serial = loaded`. A value that is not a tuple of two items is
/// unpacked by iteration: it raises `ValueError`, or the error of the
/// iteration.
fn pair_of<'py>(loaded: &Bound<'py, PyAny>) -> PyResult<(Bound<'py, PyAny>, Bound<'py, PyAny>)> {
    if let Ok(pair) = loaded.cast::<PyTuple>()
        && pair.len() == 2
    {
        return Ok((pair.get_item(0)?, pair.get_item(1)?));
    }
    let mut values = Vec::with_capacity(2);
    for value in loaded.try_iter()? {
        if values.len() == 2 {
            return Err(PyValueError::new_err(
                "too many values to unpack (expected 2)",
            ));
        }
        values.push(value?);
    }
    let n = values.len();
    let mut values = values.into_iter();
    match (values.next(), values.next()) {
        (Some(pickle), Some(serial)) => Ok((pickle, serial)),
        _ => Err(PyValueError::new_err(format!(
            "not enough values to unpack (expected 2, got {n})"
        ))),
    }
}

/// Log `err` as the bare `except` of `Connection.setstate` of ZODB logs
/// it, and return it. An error of the log takes its place, with `err` as
/// its context.
fn log_load_error(
    names: &ConnectionNames,
    conn: &Bound<'_, PyAny>,
    obj: &Bound<'_, PyAny>,
    oid: &Bound<'_, PyAny>,
    err: PyErr,
) -> PyErr {
    let py = conn.py();
    let logged = (|| -> PyResult<()> {
        let exception = conn
            .getattr(intern!(py, "_log"))?
            .getattr(intern!(py, "exception"))?;
        let class_name = names.class_name.bind(py).call1((obj,))?;
        let oid_text = names.oid_repr.bind(py).call1((oid,))?;
        let kwargs = PyDict::new(py);
        kwargs.set_item(intern!(py, "exc_info"), err.value(py))?;
        exception.call(
            ("Couldn't load state for %s %s", class_name, oid_text),
            Some(&kwargs),
        )?;
        Ok(())
    })();
    let Err(other) = logged else {
        return err;
    };
    let value = other.value(py);
    if value
        .getattr(intern!(py, "__context__"))
        .is_ok_and(|context| context.is_none())
    {
        // The error drops when the assignment fails: `other` is raised.
        let _ = value.setattr(intern!(py, "__context__"), err.value(py));
    }
    other
}

/// The `setstate` of `zoe.reader.Connection`: `setstate(conn, obj)` as an
/// `instancemethod` of CPython. A class attribute of that type binds to
/// the instance, as a function of Python does.
pub(crate) fn setstate_method<'py>(m: &Bound<'py, PyModule>) -> PyResult<Bound<'py, PyAny>> {
    let function = wrap_pyfunction!(connection_setstate, m)?;
    instance_method(function.as_any())
}

/// `PyInstanceMethod_New(function)`. pyo3-ffi does not declare it.
#[allow(unsafe_code)]
fn instance_method<'py>(function: &Bound<'py, PyAny>) -> PyResult<Bound<'py, PyAny>> {
    unsafe extern "C" {
        fn PyInstanceMethod_New(function: *mut ffi::PyObject) -> *mut ffi::PyObject;
    }
    // SAFETY: `function` is a live object. The call takes its own reference
    // to it, and returns a new reference or NULL with an exception set.
    unsafe { Bound::from_owned_ptr_or_err(function.py(), PyInstanceMethod_New(function.as_ptr())) }
}

/// The state of `pickle` from the Rust path, else from the Python path of
/// `ObjectReader.getState`, which then raises what it raises. An exception
/// that is not an `Exception`, such as `KeyboardInterrupt`, leaves at once,
/// as it leaves `ObjectReader.getState`.
fn state_of<'py>(
    reader: &Bound<'py, Reader>,
    pickle: &Bound<'py, PyAny>,
) -> PyResult<Bound<'py, PyAny>> {
    let py = reader.py();
    let reason = match decode(reader, pickle) {
        Ok(state) => {
            DECODED.fetch_add(1, Ordering::Relaxed);
            return Ok(state);
        }
        Err(Stop::Fallback(reason)) => reason,
        Err(Stop::Error(err)) if err.is_instance_of::<PyException>(py) => Reason::Error,
        Err(Stop::Error(err)) => return Err(err),
    };
    FALLBACKS[reason as usize].fetch_add(1, Ordering::Relaxed);
    reader
        .get()
        .python_get_state
        .bind(py)
        .call1((reader, pickle))
}

/// Return the counts of the readers of the process: the records that the
/// Rust path decoded, the records that took the Python path by reason,
/// and the opcodes that sent a record there.
#[pyfunction]
pub(crate) fn _reader_counts(py: Python<'_>) -> PyResult<Bound<'_, PyDict>> {
    let out = PyDict::new(py);
    out.set_item("decoded", DECODED.load(Ordering::Relaxed))?;
    let reasons = PyDict::new(py);
    for (name, n) in REASONS.iter().zip(FALLBACKS.iter()) {
        let n = n.load(Ordering::Relaxed);
        if n > 0 {
            reasons.set_item(*name, n)?;
        }
    }
    out.set_item("fallback", reasons)?;
    let opcodes = PyDict::new(py);
    for (byte, n) in OPCODES.iter().enumerate() {
        let n = n.load(Ordering::Relaxed);
        if n > 0 {
            opcodes.set_item(format!("0x{byte:02x}"), n)?;
        }
    }
    out.set_item("opcodes", opcodes)?;
    Ok(out)
}

/// Decode the class pickle and the state pickle of a record. Return the
/// state.
fn decode<'py>(
    reader: &Bound<'py, Reader>,
    pickle: &Bound<'py, PyAny>,
) -> Result<Bound<'py, PyAny>, Stop> {
    let Ok(bytes) = pickle.cast::<PyBytes>() else {
        return fallback(Reason::NotBytes);
    };
    let mut decoder = Decoder {
        py: reader.py(),
        reader,
        data: bytes.as_bytes(),
        pos: 0,
        stack: Vec::with_capacity(16),
        marks: Vec::with_capacity(4),
        memo: Vec::with_capacity(8),
        cache: None,
    };
    // The class pickle fills the memo that the state pickle reads, as one
    // unpickler loads both.
    decoder.load()?;
    decoder.load()
}

struct Decoder<'a, 'py> {
    py: Python<'py>,
    reader: &'a Bound<'py, Reader>,
    data: &'a [u8],
    pos: usize,
    stack: Vec<Bound<'py, PyAny>>,
    marks: Vec<usize>,
    memo: Vec<Option<Bound<'py, PyAny>>>,
    /// `_cache` of the reader, read once for each decode.
    cache: Option<Bound<'py, PyAny>>,
}

impl<'a, 'py> Decoder<'a, 'py> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], Stop> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|end| *end <= self.data.len());
        let Some(end) = end else {
            return fallback(Reason::Malformed);
        };
        let bytes = &self.data[self.pos..end];
        self.pos = end;
        Ok(bytes)
    }

    fn byte(&mut self) -> Result<u8, Stop> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, Stop> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    /// Read up to a newline, the newline included.
    fn line(&mut self) -> Result<&'a [u8], Stop> {
        let rest = &self.data[self.pos..];
        let Some(n) = rest.iter().position(|byte| *byte == b'\n') else {
            return fallback(Reason::Malformed);
        };
        self.take(n + 1)
    }

    fn push(&mut self, value: Bound<'py, PyAny>) {
        self.stack.push(value);
    }

    fn pop(&mut self) -> Result<Bound<'py, PyAny>, Stop> {
        match self.stack.pop() {
            Some(value) => Ok(value),
            None => fallback(Reason::Malformed),
        }
    }

    fn top(&self) -> Result<&Bound<'py, PyAny>, Stop> {
        match self.stack.last() {
            Some(value) => Ok(value),
            None => fallback(Reason::Malformed),
        }
    }

    fn marker(&mut self) -> Result<usize, Stop> {
        match self.marks.pop() {
            Some(mark) if mark <= self.stack.len() => Ok(mark),
            _ => fallback(Reason::Malformed),
        }
    }

    /// Decode one pickle, as `Unpickler.load` does. The memo stays.
    fn load(&mut self) -> Result<Bound<'py, PyAny>, Stop> {
        let py = self.py;
        self.stack.clear();
        self.marks.clear();
        loop {
            let opcode = self.byte()?;
            match opcode {
                op::PROTO => {
                    if self.byte()? > 3 {
                        return fallback(Reason::Protocol);
                    }
                }
                op::STOP => return self.pop(),
                op::MARK => self.marks.push(self.stack.len()),
                op::NONE => self.push(py.None().into_bound(py)),
                op::NEWTRUE => self.push(PyBool::new(py, true).to_owned().into_any()),
                op::NEWFALSE => self.push(PyBool::new(py, false).to_owned().into_any()),
                op::BININT => {
                    let Ok(value) = (self.u32()? as i32).into_pyobject(py);
                    self.push(value.into_any());
                }
                op::BININT1 => {
                    let Ok(value) = self.byte()?.into_pyobject(py);
                    self.push(value.into_any());
                }
                op::BININT2 => {
                    let bytes = self.take(2)?;
                    let Ok(value) = u16::from_le_bytes([bytes[0], bytes[1]]).into_pyobject(py);
                    self.push(value.into_any());
                }
                op::LONG1 => {
                    let n = usize::from(self.byte()?);
                    let bytes = self.take(n)?;
                    if n > 16 {
                        return fallback(Reason::BigInt);
                    }
                    // Two's complement, little-endian, as
                    // `_PyLong_FromByteArray` reads it.
                    let fill = if bytes.last().is_some_and(|b| b & 0x80 != 0) {
                        0xff
                    } else {
                        0
                    };
                    let mut full = [fill; 16];
                    full[..n].copy_from_slice(bytes);
                    let Ok(value) = i128::from_le_bytes(full).into_pyobject(py);
                    self.push(value.into_any());
                }
                op::BINFLOAT => {
                    let bytes = self.take(8)?;
                    let mut raw = [0; 8];
                    raw.copy_from_slice(bytes);
                    self.push(PyFloat::new(py, f64::from_be_bytes(raw)).into_any());
                }
                op::BINUNICODE => {
                    let n = self.u32()? as usize;
                    let bytes = self.take(n)?;
                    let text = unicode(py, bytes)?;
                    self.push(text);
                }
                op::BINBYTES => {
                    let n = self.u32()? as usize;
                    let bytes = self.take(n)?;
                    self.push(PyBytes::new(py, bytes).into_any());
                }
                op::SHORT_BINBYTES => {
                    let n = usize::from(self.byte()?);
                    let bytes = self.take(n)?;
                    self.push(PyBytes::new(py, bytes).into_any());
                }
                op::SHORT_BINSTRING | op::BINSTRING => {
                    let n = if opcode == op::BINSTRING {
                        let n = self.u32()? as i32;
                        if n < 0 {
                            return fallback(Reason::Malformed);
                        }
                        n as usize
                    } else {
                        usize::from(self.byte()?)
                    };
                    let bytes = self.take(n)?;
                    // The unpickler of ZODB decodes a Python 2 str as
                    // ASCII, strict.
                    let text = match std::str::from_utf8(bytes) {
                        Ok(text) if bytes.is_ascii() => text,
                        _ => return fallback(Reason::Py2Str),
                    };
                    self.push(PyString::new(py, text).into_any());
                }
                op::EMPTY_TUPLE => self.push(PyTuple::empty(py).into_any()),
                op::TUPLE => {
                    let mark = self.marker()?;
                    let tuple = PyTuple::new(py, self.stack.drain(mark..))?;
                    self.push(tuple.into_any());
                }
                op::TUPLE1 | op::TUPLE2 | op::TUPLE3 => {
                    let n = usize::from(opcode - op::TUPLE1 + 1);
                    let Some(start) = self.stack.len().checked_sub(n) else {
                        return fallback(Reason::Malformed);
                    };
                    let tuple = PyTuple::new(py, self.stack.drain(start..))?;
                    self.push(tuple.into_any());
                }
                op::EMPTY_LIST => self.push(PyList::empty(py).into_any()),
                op::EMPTY_DICT => self.push(PyDict::new(py).into_any()),
                op::APPEND => {
                    let Some(x) = self.stack.len().checked_sub(1) else {
                        return fallback(Reason::Malformed);
                    };
                    self.append(x)?;
                }
                op::APPENDS => {
                    let x = self.marker()?;
                    self.append(x)?;
                }
                op::SETITEM => {
                    let Some(x) = self.stack.len().checked_sub(2) else {
                        return fallback(Reason::Malformed);
                    };
                    self.set_items(x)?;
                }
                op::SETITEMS => {
                    let x = self.marker()?;
                    self.set_items(x)?;
                }
                op::GLOBAL => {
                    let start = self.pos;
                    let module = self.line()?;
                    let name = self.line()?;
                    let data = self.data;
                    let key = &data[start..self.pos];
                    let value = self.global(key, || {
                        let module = std::str::from_utf8(&module[..module.len() - 1]);
                        let name = std::str::from_utf8(&name[..name.len() - 1]);
                        match (module, name) {
                            (Ok(module), Ok(name)) if !module.is_empty() && !name.is_empty() => {
                                Ok((PyString::new(py, module), PyString::new(py, name)))
                            }
                            _ => fallback(Reason::Malformed),
                        }
                    })?;
                    self.push(value);
                }
                op::REDUCE => {
                    let args = self.pop()?;
                    let callable = self.pop()?;
                    let Ok(args) = args.cast_into::<PyTuple>() else {
                        return fallback(Reason::Malformed);
                    };
                    let value = callable.call1(args)?;
                    self.push(value);
                }
                op::NEWOBJ => {
                    let value = self.new_object()?;
                    self.push(value);
                }
                op::BUILD => self.build()?,
                op::BINPERSID => {
                    let pid = self.pop()?;
                    let value = self.persistent_load(pid)?;
                    self.push(value);
                }
                op::BINGET | op::LONG_BINGET => {
                    let index = if opcode == op::BINGET {
                        usize::from(self.byte()?)
                    } else {
                        self.u32()? as usize
                    };
                    let Some(Some(value)) = self.memo.get(index) else {
                        return fallback(Reason::Malformed);
                    };
                    let value = value.clone();
                    self.push(value);
                }
                op::BINPUT | op::LONG_BINPUT => {
                    let index = if opcode == op::BINPUT {
                        usize::from(self.byte()?)
                    } else {
                        self.u32()? as usize
                    };
                    if index >= MEMO_LIMIT {
                        return fallback(Reason::Malformed);
                    }
                    let value = self.top()?.clone();
                    if self.memo.len() <= index {
                        self.memo.resize(index + 1, None);
                    }
                    self.memo[index] = Some(value);
                }
                op::DUP => {
                    let value = self.top()?.clone();
                    self.push(value);
                }
                op::POP => {
                    // As the C unpickler: a mark at the top of the stack
                    // goes first.
                    if self.marks.last() == Some(&self.stack.len()) {
                        self.marks.pop();
                    } else {
                        self.pop()?;
                    }
                }
                op::POP_MARK => {
                    let mark = self.marker()?;
                    self.stack.truncate(mark);
                }
                _ => {
                    OPCODES[usize::from(opcode)].fetch_add(1, Ordering::Relaxed);
                    return fallback(Reason::Opcode);
                }
            }
        }
    }

    /// APPEND and APPENDS, as `do_append` of the C unpickler. A list, or a
    /// subclass of list, takes the items directly.
    fn append(&mut self, x: usize) -> Result<(), Stop> {
        let len = self.stack.len();
        if x > len || x == 0 {
            return fallback(Reason::Malformed);
        }
        if x == len {
            return Ok(());
        }
        let target = self.stack[x - 1].clone();
        let items: Vec<_> = self.stack.drain(x..).collect();
        if let Ok(list) = target.cast::<PyList>() {
            for item in items {
                list.append(item)?;
            }
        } else {
            let append = target.getattr(intern!(self.py, "append"))?;
            for item in items {
                append.call1((item,))?;
            }
        }
        Ok(())
    }

    /// SETITEM and SETITEMS, as `do_setitems` of the C unpickler.
    fn set_items(&mut self, x: usize) -> Result<(), Stop> {
        let len = self.stack.len();
        if x > len || x == 0 {
            return fallback(Reason::Malformed);
        }
        if x == len {
            return Ok(());
        }
        if !(len - x).is_multiple_of(2) {
            return fallback(Reason::Malformed);
        }
        let target = self.stack[x - 1].clone();
        let mut items = self.stack.drain(x..);
        while let (Some(key), Some(value)) = (items.next(), items.next()) {
            target.set_item(key, value)?;
        }
        Ok(())
    }

    /// NEWOBJ, as `load_newobj` of the C unpickler: `tp_new(cls, args)`.
    #[allow(unsafe_code)]
    fn new_object(&mut self) -> Result<Bound<'py, PyAny>, Stop> {
        let args = self.pop()?;
        let cls = self.pop()?;
        let Ok(args) = args.cast_into::<PyTuple>() else {
            return fallback(Reason::Malformed);
        };
        let Ok(cls) = cls.cast_into::<PyType>() else {
            return fallback(Reason::Malformed);
        };
        let ty = cls.as_type_ptr();
        // SAFETY: `ty` is a live type object, and `args` a live tuple. The
        // call returns a new reference or NULL with an exception set.
        unsafe {
            let Some(new) = (*ty).tp_new else {
                return fallback(Reason::Malformed);
            };
            let obj = new(ty, args.as_ptr(), std::ptr::null_mut());
            Ok(Bound::from_owned_ptr_or_err(self.py, obj)?)
        }
    }

    /// BUILD, as `load_build` of the C unpickler.
    fn build(&mut self) -> Result<(), Stop> {
        let py = self.py;
        if self.stack.len() < 2 {
            return fallback(Reason::Malformed);
        }
        let state = self.pop()?;
        let inst = self.top()?.clone();
        match inst.getattr(intern!(py, "__setstate__")) {
            Ok(setstate) => {
                setstate.call1((state,))?;
                return Ok(());
            }
            Err(err) if err.is_instance_of::<PyAttributeError>(py) => {}
            Err(err) => return Err(err.into()),
        }
        // A state of two items is the instance dict and the slot state.
        let (state, slots) = match state.cast::<PyTuple>() {
            Ok(pair) if pair.len() == 2 => (pair.get_item(0)?, Some(pair.get_item(1)?)),
            _ => (state.clone(), None),
        };
        if !state.is_none() {
            let Ok(state) = state.cast::<PyDict>() else {
                return fallback(Reason::Malformed);
            };
            let dict = inst.getattr(intern!(py, "__dict__"))?;
            for (key, value) in state.iter() {
                let key = if key.is_exact_instance_of::<PyString>() {
                    intern_in_place(key)
                } else {
                    key
                };
                dict.set_item(key, value)?;
            }
        }
        if let Some(slots) = slots {
            let Ok(slots) = slots.cast::<PyDict>() else {
                return fallback(Reason::Malformed);
            };
            for (key, value) in slots.iter() {
                // The C unpickler raises for a name that is not a str: the
                // Python path raises the same.
                let Ok(key) = key.cast_into::<PyString>() else {
                    return fallback(Reason::Malformed);
                };
                inst.setattr(key, value)?;
            }
        }
        Ok(())
    }

    /// The global of `key`, the bytes of a module line and a name line,
    /// from the table, else from the class factory of the connection. The
    /// table keeps no subclass of `Broken`: the record takes the Python
    /// path.
    fn global(
        &mut self,
        key: &[u8],
        names: impl FnOnce() -> Result<(Bound<'py, PyString>, Bound<'py, PyString>), Stop>,
    ) -> Result<Bound<'py, PyAny>, Stop> {
        let py = self.py;
        let reader = self.reader.get();
        let known = lock(&reader.globals)
            .get(key)
            .map(|value| value.clone_ref(py));
        if let Some(value) = known {
            return Ok(value.into_bound(py));
        }
        let (module, name) = names()?;
        let factory = self.reader.getattr(intern!(py, "_factory"))?;
        let conn = self.reader.getattr(intern!(py, "_conn"))?;
        let value = factory.call1((conn, module, name))?;
        if let Ok(class) = value.cast::<PyType>()
            && class.is_subclass(reader.broken.bind(py).as_any())?
        {
            return fallback(Reason::Broken);
        }
        let unbound = value.clone().unbind();
        let replaced = lock(&reader.globals).insert(key.into(), unbound);
        // A value that the table held drops outside the mutex.
        drop(replaced);
        Ok(value)
    }

    fn cache(&mut self) -> Result<Bound<'py, PyAny>, Stop> {
        if let Some(cache) = &self.cache {
            return Ok(cache.clone());
        }
        let cache = self.reader.getattr(intern!(self.py, "_cache"))?;
        self.cache = Some(cache.clone());
        Ok(cache)
    }

    /// `self._cache.get(oid, None)`.
    fn cached(&mut self, oid: &Bound<'py, PyAny>) -> Result<Option<Bound<'py, PyAny>>, Stop> {
        let py = self.py;
        let value = self
            .cache()?
            .call_method1(intern!(py, "get"), (oid, py.None()))?;
        Ok((!value.is_none()).then_some(value))
    }

    /// `self._conn.get(oid)`.
    fn conn_get(&mut self, oid: &Bound<'py, PyAny>) -> Result<Bound<'py, PyAny>, Stop> {
        let py = self.py;
        let conn = self.reader.getattr(intern!(py, "_conn"))?;
        Ok(conn.call_method1(intern!(py, "get"), (oid,))?)
    }

    /// BINPERSID, as `ObjectReader._persistent_load`: a tuple is
    /// `load_persistent(oid, klass)` and a bytes or a str is `load_oid(oid)`.
    /// A list is a weak reference or a reference into another database.
    fn persistent_load(&mut self, pid: Bound<'py, PyAny>) -> Result<Bound<'py, PyAny>, Stop> {
        let py = self.py;
        if let Ok(pair) = pid.cast::<PyTuple>() {
            if pair.len() != 2 {
                return fallback(Reason::Reference);
            }
            let oid = oid_of(pair.get_item(0)?)?;
            if let Some(obj) = self.cached(&oid)? {
                return Ok(obj);
            }
            let mut klass = pair.get_item(1)?;
            if let Ok(names) = klass.cast::<PyTuple>() {
                klass = self.class_of_names(names)?;
            }
            let Ok(class) = klass.cast::<PyType>() else {
                return fallback(Reason::Malformed);
            };
            if class.is_subclass(self.reader.get().broken.bind(py).as_any())? {
                return fallback(Reason::Broken);
            }
            let made = klass
                .getattr(intern!(py, "__new__"))
                .and_then(|new| new.call1((&klass,)));
            let obj = match made {
                Ok(obj) => obj,
                Err(err) if err.is_instance_of::<PyTypeError>(py) => return self.conn_get(&oid),
                Err(err) => return Err(err.into()),
            };
            self.cache()?
                .call_method1(intern!(py, "new_ghost"), (&oid, &obj))?;
            return Ok(obj);
        }
        if pid.is_instance_of::<PyBytes>() || pid.is_instance_of::<PyString>() {
            let oid = oid_of(pid)?;
            if let Some(obj) = self.cached(&oid)? {
                return Ok(obj);
            }
            return self.conn_get(&oid);
        }
        if let Ok(list) = pid.cast::<PyList>() {
            // `[oid]` and `["w", (oid, ...)]` are weak references,
            // `["m", ...]` and `["n", ...]` references into another
            // database.
            let kind = match list.len() {
                1 => return fallback(Reason::Weak),
                2 => list.get_item(0)?,
                _ => return fallback(Reason::Reference),
            };
            let kind = kind
                .cast::<PyString>()
                .ok()
                .and_then(|kind| kind.to_str().ok().map(str::to_owned));
            return match kind.as_deref() {
                Some("w") => fallback(Reason::Weak),
                Some("m" | "n") => fallback(Reason::OtherDatabase),
                _ => fallback(Reason::Reference),
            };
        }
        fallback(Reason::Reference)
    }

    /// `self._get_class(module, name)`, through the table of globals.
    fn class_of_names(&mut self, names: &Bound<'py, PyTuple>) -> Result<Bound<'py, PyAny>, Stop> {
        if names.len() != 2 {
            return fallback(Reason::Malformed);
        }
        let (Ok(module), Ok(name)) = (
            names.get_item(0)?.cast_into::<PyString>(),
            names.get_item(1)?.cast_into::<PyString>(),
        ) else {
            return fallback(Reason::Malformed);
        };
        // A str that is not valid UTF-8, such as a lone surrogate, has no
        // key in the table.
        let (Ok(module_text), Ok(name_text)) = (module.to_str(), name.to_str()) else {
            return fallback(Reason::Malformed);
        };
        let mut key = Vec::with_capacity(module_text.len() + name_text.len() + 2);
        key.extend_from_slice(module_text.as_bytes());
        key.push(b'\n');
        key.extend_from_slice(name_text.as_bytes());
        key.push(b'\n');
        self.global(&key, || Ok((module, name)))
    }
}

/// The oid of a persistent id: the bytes, or a str that is ASCII as bytes.
fn oid_of(oid: Bound<'_, PyAny>) -> Result<Bound<'_, PyAny>, Stop> {
    if oid.is_instance_of::<PyBytes>() {
        return Ok(oid);
    }
    let Ok(text) = oid.cast::<PyString>() else {
        return fallback(Reason::Malformed);
    };
    match text.to_str() {
        Ok(text) if text.is_ascii() => Ok(PyBytes::new(oid.py(), text.as_bytes()).into_any()),
        _ => fallback(Reason::Malformed),
    }
}

/// A str from UTF-8 with the error handler `surrogatepass`, as BINUNICODE
/// of the C unpickler.
#[allow(unsafe_code)]
fn unicode<'py>(py: Python<'py>, bytes: &[u8]) -> Result<Bound<'py, PyAny>, Stop> {
    let Ok(len) = ffi::Py_ssize_t::try_from(bytes.len()) else {
        return fallback(Reason::Malformed);
    };
    // SAFETY: the pointer and the length describe `bytes`. The call
    // returns a new reference or NULL with an exception set.
    let text = unsafe {
        ffi::PyUnicode_DecodeUTF8(
            bytes.as_ptr().cast::<c_char>(),
            len,
            c"surrogatepass".as_ptr(),
        )
    };
    // SAFETY: `text` is a new reference or NULL.
    match unsafe { Bound::from_owned_ptr_or_err(py, text) } {
        Ok(text) => Ok(text),
        Err(_) => fallback(Reason::Malformed),
    }
}

/// The interned form of an exact str, as `PyUnicode_InternInPlace` gives
/// it.
#[allow(unsafe_code)]
fn intern_in_place(key: Bound<'_, PyAny>) -> Bound<'_, PyAny> {
    let py = key.py();
    let mut ptr = key.into_ptr();
    // SAFETY: `ptr` is an owned reference to an exact str. The call
    // replaces it with an owned reference to the interned str.
    unsafe {
        ffi::PyUnicode_InternInPlace(&mut ptr);
        Bound::from_owned_ptr(py, ptr)
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
