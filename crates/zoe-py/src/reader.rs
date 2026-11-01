//! `zoe._zoe.Reader`: a reader of ZODB records in Rust. It is the base
//! class of `zoe.reader.Reader`, which also derives from ZODB's
//! `ObjectReader`. `Connection.setstate` calls its `setGhostState`.
//!
//! The reader decodes the class pickle and the state pickle of a record
//! with the opcodes that ZODB writes at protocol 3, and builds the objects
//! as the C unpickler of zodbpickle builds them. It finds a global through
//! the class factory of the connection once, then in a table. It resolves
//! a persistent reference in the pickle cache, as
//! `ObjectReader._persistent_load` does. A record with another construct
//! takes the Python path of `ObjectReader` whole: another opcode, another
//! protocol, a class that the factory gives as `Broken`, a weak reference,
//! a reference into another database, a Python 2 str that is not ASCII,
//! an int of more than 16 bytes, and malformed bytes.
//!
//! The reader adds no lock to the pickle cache. One thread uses a
//! connection and its reader at a time. The table of globals has a mutex,
//! and no Python code runs while a call holds it.

// NEWOBJ calls `tp_new` of the class, and BINUNICODE and BUILD call the C
// API of CPython, as the C unpickler does.
#![allow(unsafe_code)]

use std::cell::Cell;
use std::collections::HashMap;
use std::ffi::c_char;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use pyo3::exceptions::{PyAttributeError, PyTypeError};
use pyo3::ffi;
use pyo3::intern;
use pyo3::prelude::*;
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
}

const REASONS: [&str; 10] = [
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
];

static DECODED: AtomicU64 = AtomicU64::new(0);
static FALLBACKS: [AtomicU64; 10] = [const { AtomicU64::new(0) }; 10];
static OPCODES: [AtomicU64; 256] = [const { AtomicU64::new(0) }; 256];

/// The end of a decode that does not give a state.
enum Stop {
    /// The record takes the Python path.
    Fallback(Reason),
    /// A call into Python raised. The Python path raises the same.
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
    broken: Py<PyType>,
    /// `ObjectReader.getState`, the Python path.
    python_get_state: Py<PyAny>,
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
        let broken = py
            .import("ZODB.broken")?
            .getattr("Broken")?
            .cast_into::<PyType>()?;
        let reader = py.import("ZODB.serialize")?.getattr("ObjectReader")?;
        Ok(Self {
            globals: Mutex::new(HashMap::new()),
            broken: broken.unbind(),
            python_get_state: reader.getattr("getState")?.unbind(),
        })
    }

    /// Decode the state of `pickle` and give it to `obj.__setstate__`.
    #[pyo3(name = "setGhostState")]
    fn set_ghost_state(
        slf: &Bound<'_, Self>,
        obj: &Bound<'_, PyAny>,
        pickle: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        let py = slf.py();
        // The body of `ObjectReader.setGhostState`, with the state of the
        // Rust path or of `ObjectReader.getState`.
        let state = match decode(slf, pickle) {
            Ok(state) => {
                DECODED.fetch_add(1, Ordering::Relaxed);
                state
            }
            Err(Stop::Fallback(reason)) => {
                count(reason);
                slf.get().python_get_state.bind(py).call1((slf, pickle))?
            }
            Err(Stop::Error(err)) => return Err(err),
        };
        obj.call_method1(intern!(py, "__setstate__"), (state,))?;
        Ok(())
    }

    /// Return the state of `pickle`.
    #[pyo3(name = "getState")]
    fn get_state<'py>(
        slf: &Bound<'py, Self>,
        pickle: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        match decode(slf, pickle) {
            Ok(state) => {
                DECODED.fetch_add(1, Ordering::Relaxed);
                Ok(state)
            }
            Err(Stop::Fallback(reason)) => {
                count(reason);
                slf.get()
                    .python_get_state
                    .bind(slf.py())
                    .call1((slf, pickle))
            }
            Err(Stop::Error(err)) => Err(err),
        }
    }

    /// Return `(state, None)` when the Rust path decodes `pickle`, else
    /// `(None, reason)`. It counts nothing. The oracle of the spike calls
    /// it.
    fn _decode<'py>(
        slf: &Bound<'py, Self>,
        pickle: &Bound<'py, PyAny>,
    ) -> PyResult<(Option<Bound<'py, PyAny>>, Option<String>)> {
        match decode(slf, pickle) {
            Ok(state) => Ok((Some(state), None)),
            Err(Stop::Fallback(Reason::Opcode)) => {
                let opcode = LAST_OPCODE.with(Cell::get);
                Ok((None, Some(format!("opcode 0x{opcode:02x}"))))
            }
            Err(Stop::Fallback(reason)) => Ok((None, Some(REASONS[reason as usize].into()))),
            Err(Stop::Error(err)) => Err(err),
        }
    }
}

thread_local! {
    /// The last opcode that sent a record to the Python path in this
    /// thread.
    static LAST_OPCODE: Cell<u8> = const { Cell::new(0) };
}

fn count(reason: Reason) {
    FALLBACKS[reason as usize].fetch_add(1, Ordering::Relaxed);
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

/// Set the counts of `_reader_counts` to zero.
#[pyfunction]
pub(crate) fn _reader_reset() {
    DECODED.store(0, Ordering::Relaxed);
    for n in FALLBACKS.iter().chain(OPCODES.iter()) {
        n.store(0, Ordering::Relaxed);
    }
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
    // The class pickle fills the memo that the state pickle reads.
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
                    LAST_OPCODE.with(|last| last.set(opcode));
                    return fallback(Reason::Opcode);
                }
            }
        }
    }

    /// APPEND and APPENDS, as `do_append` of the C unpickler.
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
        let split = state.cast::<PyTuple>().is_ok_and(|pair| pair.len() == 2);
        let (state, slots) = if split {
            let pair = state.cast::<PyTuple>().map_err(PyErr::from)?;
            (pair.get_item(0)?, Some(pair.get_item(1)?))
        } else {
            (state, None)
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
                // SAFETY: the three objects are live. The call returns -1
                // with an exception set on failure.
                let status =
                    unsafe { ffi::PyObject_SetAttr(inst.as_ptr(), key.as_ptr(), value.as_ptr()) };
                if status < 0 {
                    return Err(PyErr::fetch(py).into());
                }
            }
        }
        Ok(())
    }

    /// The global of `key`, the bytes of a module line and a name line,
    /// from the table, else from the class factory of the connection.
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

    /// BINPERSID, as `ObjectReader._persistent_load`.
    fn persistent_load(&mut self, pid: Bound<'py, PyAny>) -> Result<Bound<'py, PyAny>, Stop> {
        let py = self.py;
        if let Ok(pair) = pid.cast::<PyTuple>() {
            // `load_persistent(oid, klass)`.
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
            // `load_oid(oid)`.
            let oid = oid_of(pid)?;
            if let Some(obj) = self.cached(&oid)? {
                return Ok(obj);
            }
            return self.conn_get(&oid);
        }
        if let Ok(list) = pid.cast::<PyList>() {
            if list.len() == 1 {
                return fallback(Reason::Weak);
            }
            let kind = list.get_item(0)?;
            if let Ok(kind) = kind.cast::<PyString>() {
                match kind.to_str()? {
                    "w" => return fallback(Reason::Weak),
                    "m" | "n" => return fallback(Reason::OtherDatabase),
                    _ => {}
                }
            }
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
        let mut key = Vec::new();
        key.extend_from_slice(module.to_str()?.as_bytes());
        key.push(b'\n');
        key.extend_from_slice(name.to_str()?.as_bytes());
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
    let text = text.to_str()?;
    if !text.is_ascii() {
        return fallback(Reason::Malformed);
    }
    Ok(PyBytes::new(oid.py(), text.as_bytes()).into_any())
}

/// A str from UTF-8 with the error handler `surrogatepass`, as BINUNICODE
/// of the C unpickler.
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

/// The interned form of a str, as `PyUnicode_InternInPlace` gives it.
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
