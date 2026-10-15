//! `zoe._zoe.Client`: the native client of `zoe.zeo.ClientStorage`, on
//! `zoe-client`.
//!
//! `zoe.zeo` wraps it in a ZODB storage when `native` is true. One client
//! holds one connection to one storage of a server, and one transaction at
//! a time. It has no cache, no blob and no reconnect.
//!
//! A tokio runtime of the process runs the connections. It starts at the
//! first open of a client, never at the import. Each call waits for the
//! server inside `py.detach`: it releases the GIL, and on a free-threaded
//! build it holds nothing.
//!
//! A child of `os.fork()` has no thread of the runtime of its parent. A
//! client of the parent raises `RuntimeError` at each call in the child,
//! and its drop there frees nothing. A client that the child opens gets a
//! runtime of the child.

use std::mem;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use pyo3::exceptions::{PyException, PyRuntimeError, PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyBytes, PyDict, PyFloat, PyInt, PyList, PyString, PyTuple, PyType};
use rmpv::Value;
use tokio::runtime::Runtime;
use zoe_client::{Error, Invalidations, Options, Tokio, Transaction};
use zoe_core::{Oid, Tid};

use crate::errors::{self, p64, u64_of};

/// The number of worker threads of the runtime of the native clients.
const WORKERS: usize = 2;

/// The time between two attempts to open a client, until `wait_timeout`.
const RETRY: Duration = Duration::from_millis(100);

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

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Return the runtime of the native clients of this process. The first
/// call starts it. A child of `os.fork()` gets a runtime of its own: the
/// threads of the runtime of the parent do not exist in the child. The
/// runtime of the parent stays allocated there and is never dropped.
///
/// A fork while another thread of the parent holds the mutex of this
/// function leaves it locked in the child: the first open of the child
/// then waits for ever. The mutex is held only while the runtime builds,
/// at the first open of a process, so the window is short.
fn runtime() -> std::io::Result<&'static Runtime> {
    static RUNTIME: Mutex<Option<(u32, &'static Runtime)>> = Mutex::new(None);
    let pid = std::process::id();
    let mut slot = lock(&RUNTIME);
    if let Some((owner, runtime)) = *slot
        && owner == pid
    {
        return Ok(runtime);
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(WORKERS)
        .thread_name("zoe-client")
        .enable_all()
        .build()?;
    let runtime: &'static Runtime = Box::leak(Box::new(runtime));
    *slot = Some((pid, runtime));
    Ok(runtime)
}

/// A client of one storage of a ZEO 6 server, with the ZOE extensions
/// that the server accepts.
#[pyclass(frozen, module = "zoe._zoe", name = "Client")]
pub(crate) struct NativeClient {
    /// The process that opened the client.
    pid: u32,
    runtime: &'static Runtime,
    /// The client, until `close`.
    client: Mutex<Option<Arc<zoe_client::Client>>>,
    /// The open transaction, from `begin` to its end.
    transaction: Mutex<Option<Transaction>>,
    /// The receiver whose invalidations `drain` gives. The drop in a child
    /// of `os.fork()` takes it out.
    invalidations: Mutex<Option<Invalidations>>,
    /// A second receiver. It only wakes the thread that delivers the
    /// invalidations: `wait_invalidation` reads it.
    wake: Mutex<Option<Invalidations>>,
    /// The last tid that the client knew after the open.
    opened_at: Tid,
}

#[pymethods]
impl NativeClient {
    /// Connect to the server at `(host, port)` and register with the
    /// storage `storage`, read-only when `read_only` is true. Declare
    /// `extensions` in `register`, and call `get_info`.
    ///
    /// A connection that fails, or that the server closes before the
    /// register, is tried again until `wait_timeout` seconds pass. Then
    /// the open raises `ClientDisconnected`. Another error of the open
    /// raises at once.
    #[new]
    #[pyo3(signature = (host, port, storage, *, read_only, extensions, wait_timeout))]
    fn new(
        py: Python<'_>,
        host: String,
        port: u16,
        storage: String,
        read_only: bool,
        extensions: Vec<String>,
        wait_timeout: f64,
    ) -> PyResult<Self> {
        let wait = Duration::try_from_secs_f64(wait_timeout)
            .map_err(|_| PyValueError::new_err("wait_timeout is not a duration"))?;
        let options = Options {
            read_only,
            extensions,
        };
        let opened = py.detach(move || -> Result<_, Opening> {
            let addr = resolve(&host, port)?;
            let runtime = runtime().map_err(Opening::Runtime)?;
            let client = runtime.block_on(open(addr, &storage, &options, wait))?;
            // Subscribe first, then read the last tid: the client raises
            // its last tid before it sends an invalidation.
            let invalidations = client.invalidations();
            let wake = client.invalidations();
            let opened_at = client.last_transaction();
            Ok((runtime, client, invalidations, wake, opened_at))
        });
        let (runtime, client, invalidations, wake, opened_at) = match opened {
            Ok(opened) => opened,
            Err(Opening::Address(text)) => return Err(PyValueError::new_err(text)),
            Err(Opening::Runtime(error)) => return Err(error.into()),
            Err(Opening::Client(error)) => return Err(client_error(py, error)),
        };
        Ok(Self {
            pid: std::process::id(),
            runtime,
            client: Mutex::new(Some(Arc::new(client))),
            transaction: Mutex::new(None),
            invalidations: Mutex::new(Some(invalidations)),
            wake: Mutex::new(Some(wake)),
            opened_at,
        })
    }

    /// Return the last tid that the client knew after the open: the tid
    /// of `register`.
    fn opened_at<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        p64(py, self.opened_at.0)
    }

    /// Return the reply of `get_info` at the open, as a dict.
    fn info<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let client = self.client(py)?;
        to_python(py, client.info(), 0)
    }

    /// Return the extensions that the server accepted at the open.
    fn extensions(&self, py: Python<'_>) -> PyResult<Vec<String>> {
        Ok(self.client(py)?.extensions().to_vec())
    }

    /// Tell if the connection is open.
    fn is_connected(&self) -> bool {
        self.pid == std::process::id()
            && lock(&self.client)
                .as_ref()
                .is_some_and(|client| client.is_connected())
    }

    /// Return `n` new oids.
    fn new_oids<'py>(&self, py: Python<'py>, n: usize) -> PyResult<Vec<Bound<'py, PyBytes>>> {
        let client = self.client(py)?;
        let runtime = self.runtime;
        let oids = py
            .detach(move || runtime.block_on(client.new_oids(n)))
            .map_err(|e| client_error(py, e))?;
        Ok(oids.into_iter().map(|oid| p64(py, oid.0)).collect())
    }

    /// Return the record of `oid` that was current before `tid`: its
    /// pickle, its serial and the serial of the next record or `None`.
    /// Return `None` when `oid` has no record before `tid`.
    #[allow(clippy::type_complexity)]
    fn load_before<'py>(
        &self,
        py: Python<'py>,
        oid: &Bound<'py, PyAny>,
        tid: &Bound<'py, PyAny>,
    ) -> PyResult<
        Option<(
            Bound<'py, PyBytes>,
            Bound<'py, PyBytes>,
            Option<Bound<'py, PyBytes>>,
        )>,
    > {
        let (oid, tid) = (Oid(u64_of(oid)?), Tid(u64_of(tid)?));
        let client = self.client(py)?;
        let runtime = self.runtime;
        let record = py
            .detach(move || runtime.block_on(client.load_before(oid, tid)))
            .map_err(|e| client_error(py, e))?;
        Ok(record.map(|record| {
            (
                PyBytes::new(py, &record.data),
                p64(py, record.tid.0),
                record.end.map(|end| p64(py, end.0)),
            )
        }))
    }

    /// Return the pickle of `oid` that the transaction `serial` wrote.
    fn load_serial<'py>(
        &self,
        py: Python<'py>,
        oid: &Bound<'py, PyAny>,
        serial: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyBytes>> {
        let (oid, serial) = (Oid(u64_of(oid)?), Tid(u64_of(serial)?));
        let client = self.client(py)?;
        let runtime = self.runtime;
        let data = py
            .detach(move || runtime.block_on(client.load_serial(oid, serial)))
            .map_err(|e| client_error(py, e))?;
        Ok(PyBytes::new(py, &data))
    }

    /// Return the next value of the sequence `name` of the store. Raise
    /// `Unsupported` when the server did not accept `sequences`.
    fn sequence_next(&self, py: Python<'_>, name: String) -> PyResult<u64> {
        let client = self.client(py)?;
        let runtime = self.runtime;
        py.detach(move || runtime.block_on(client.sequence_next(&name)))
            .map_err(|e| client_error(py, e))
    }

    /// Begin a transaction. `extension` is the dict of the transaction
    /// extension. With `tid`, the transaction commits under that tid, with
    /// the status `status`. The caller holds the commit lock: a second
    /// transaction raises `StorageTransactionError`.
    #[pyo3(signature = (user, description, extension, tid = None, status = " ".to_owned()))]
    fn begin(
        &self,
        py: Python<'_>,
        user: &[u8],
        description: &[u8],
        extension: &Bound<'_, PyAny>,
        tid: Option<&Bound<'_, PyAny>>,
        status: String,
    ) -> PyResult<()> {
        let tid = tid.map(u64_of).transpose()?.map(Tid);
        let mut encoded = Vec::new();
        rmpv::encode::write_value(&mut encoded, &to_value(extension, 0)?)
            .map_err(|e| PyValueError::new_err(format!("the extension does not encode: {e}")))?;
        let client = self.client(py)?;
        let runtime = self.runtime;
        let (user, description) = (user.to_vec(), description.to_vec());
        py.detach(|| {
            let mut slot = lock(&self.transaction);
            if slot.is_some() {
                return Err(Error::Invalid("a transaction is open".into()));
            }
            let txn =
                runtime.block_on(client.begin_with(&user, &description, &encoded, tid, &status))?;
            *slot = Some(txn);
            Ok(())
        })
        .map_err(|e| client_error(py, e))
    }

    /// Store `data` for `oid` in the open transaction. `serial` is the
    /// serial that the transaction read.
    fn store(
        &self,
        py: Python<'_>,
        oid: &Bound<'_, PyAny>,
        serial: &Bound<'_, PyAny>,
        data: &[u8],
    ) -> PyResult<()> {
        let (oid, serial) = (Oid(u64_of(oid)?), Tid(u64_of(serial)?));
        self.check_process()?;
        let runtime = self.runtime;
        py.detach(|| {
            let mut slot = lock(&self.transaction);
            let txn = slot.as_mut().ok_or_else(not_open)?;
            runtime.block_on(txn.store(oid, serial, data))
        })
        .map_err(|e| client_error(py, e))
    }

    /// Ask the server to check at the vote that `serial` is still the
    /// current serial of `oid`.
    fn check_current_serial(
        &self,
        py: Python<'_>,
        oid: &Bound<'_, PyAny>,
        serial: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        let (oid, serial) = (Oid(u64_of(oid)?), Tid(u64_of(serial)?));
        self.check_process()?;
        let runtime = self.runtime;
        py.detach(|| {
            let mut slot = lock(&self.transaction);
            let txn = slot.as_mut().ok_or_else(not_open)?;
            runtime.block_on(txn.check_current_serial(oid, serial))
        })
        .map_err(|e| client_error(py, e))
    }

    /// Vote, and return the oids that the server resolved. A failed vote
    /// ends the transaction: the server aborted it.
    fn vote<'py>(&self, py: Python<'py>) -> PyResult<Vec<Bound<'py, PyBytes>>> {
        self.check_process()?;
        let runtime = self.runtime;
        let resolved = py
            .detach(|| {
                let mut slot = lock(&self.transaction);
                let txn = slot.as_mut().ok_or_else(not_open)?;
                let voted = runtime.block_on(txn.vote_resolved());
                if voted.is_err() {
                    let ended = slot.take();
                    let _inside = runtime.enter();
                    drop(ended);
                }
                voted
            })
            .map_err(|e| client_error(py, e))?;
        Ok(resolved.into_iter().map(|oid| p64(py, oid.0)).collect())
    }

    /// Finish the open transaction after its vote, and return its tid.
    fn finish<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyBytes>> {
        self.check_process()?;
        let runtime = self.runtime;
        let tid = py
            .detach(|| {
                let txn = lock(&self.transaction).take().ok_or_else(not_open)?;
                runtime.block_on(txn.finish())
            })
            .map_err(|e| client_error(py, e))?;
        Ok(p64(py, tid.0))
    }

    /// Abort the open transaction, and wait for the reply of the server.
    /// Without an open transaction, do nothing.
    fn abort(&self, py: Python<'_>) -> PyResult<()> {
        self.check_process()?;
        let runtime = self.runtime;
        py.detach(|| {
            let txn = lock(&self.transaction).take();
            match txn {
                Some(txn) => runtime.block_on(txn.abort()),
                None => Ok(()),
            }
        })
        .map_err(|e| client_error(py, e))
    }

    /// Return the invalidations that wait, in tid order, without a wait:
    /// a tuple of a flag and a list of `(tid, oids)`. The flag is true when
    /// the client missed invalidations: the caller must then treat its
    /// cache as stale. After the close of the connection, the list holds
    /// the invalidations that came before it.
    #[allow(clippy::type_complexity)]
    fn drain<'py>(
        &self,
        py: Python<'py>,
    ) -> PyResult<(bool, Vec<(Bound<'py, PyBytes>, Vec<Bound<'py, PyBytes>>)>)> {
        self.check_process()?;
        let (lagged, items) = py.detach(|| {
            let mut receiver = lock(&self.invalidations);
            let mut lagged = false;
            let mut items = Vec::new();
            let Some(receiver) = receiver.as_mut() else {
                return (lagged, items);
            };
            loop {
                match receiver.try_recv() {
                    Ok(Some(invalidation)) => items.push(invalidation),
                    Err(Error::Lagged(_)) => lagged = true,
                    Ok(None) | Err(_) => break,
                }
            }
            (lagged, items)
        });
        let items: Vec<_> = items
            .into_iter()
            .map(|invalidation| {
                let oids = invalidation.oids.iter().map(|oid| p64(py, oid.0));
                (p64(py, invalidation.tid.0), oids.collect::<Vec<_>>())
            })
            .collect();
        Ok((lagged, items))
    }

    /// Wait until the server sends an invalidation, and return true. Return
    /// false when the connection closes. It takes no invalidation: `drain`
    /// gives them.
    fn wait_invalidation(&self, py: Python<'_>) -> PyResult<bool> {
        self.check_process()?;
        let runtime = self.runtime;
        Ok(py.detach(|| {
            let mut wake = lock(&self.wake);
            let Some(wake) = wake.as_mut() else {
                return false;
            };
            match runtime.block_on(wake.recv()) {
                Ok(_) | Err(Error::Lagged(_)) => true,
                Err(_) => false,
            }
        }))
    }

    /// Close the connection. A call that waits, and each later call, raises
    /// `ClientDisconnected`. A second close does nothing.
    fn close(&self, py: Python<'_>) -> PyResult<()> {
        self.check_process()?;
        let runtime = self.runtime;
        let client = lock(&self.client).take();
        let Some(client) = client else {
            return Ok(());
        };
        py.detach(move || {
            client.disconnect();
            let txn = lock(&self.transaction).take();
            {
                let _inside = runtime.enter();
                drop(txn);
            }
            // The tasks stop at once. Wait for them when no call holds
            // the client.
            if let Ok(client) = Arc::try_unwrap(client) {
                runtime.block_on(client.close());
            }
        });
        Ok(())
    }
}

impl NativeClient {
    /// Return the client, or raise when it is closed or when the process
    /// is a child of the process that opened it.
    fn client(&self, py: Python<'_>) -> PyResult<Arc<zoe_client::Client>> {
        self.check_process()?;
        let client = lock(&self.client).clone();
        client.ok_or_else(|| errors::client_disconnected(py, "the client is closed".into()))
    }

    /// Raise `RuntimeError` in a child of `os.fork()`: the threads of the
    /// client are in the parent only.
    fn check_process(&self) -> PyResult<()> {
        if self.pid != std::process::id() {
            return Err(PyRuntimeError::new_err(
                "the client was opened in another process, before a fork",
            ));
        }
        Ok(())
    }
}

impl Drop for NativeClient {
    fn drop(&mut self) {
        let client = self
            .client
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let txn = self
            .transaction
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if self.pid != std::process::id() {
            // The threads and the locks of the parent are not here. Free
            // nothing of the connection.
            let receivers = [&mut self.invalidations, &mut self.wake].map(|receiver| {
                receiver
                    .get_mut()
                    .unwrap_or_else(PoisonError::into_inner)
                    .take()
            });
            mem::forget((client, txn, receivers));
            return;
        }
        // The drop of a transaction can start a task of the runtime.
        let _inside = self.runtime.enter();
        drop(txn);
        drop(client);
    }
}

/// An error of the open of a client.
enum Opening {
    /// The address does not resolve.
    Address(String),
    /// The runtime does not start.
    Runtime(std::io::Error),
    /// The client does not open.
    Client(Error),
}

impl From<Error> for Opening {
    fn from(error: Error) -> Self {
        Self::Client(error)
    }
}

/// Return the first address of `host` and `port`.
fn resolve(host: &str, port: u16) -> Result<SocketAddr, Opening> {
    let mut addrs = (host, port)
        .to_socket_addrs()
        .map_err(|e| Opening::Address(format!("the address {host}:{port}: {e}")))?;
    addrs
        .next()
        .ok_or_else(|| Opening::Address(format!("the address {host}:{port} has no IP address")))
}

/// Open a client on the runtime of the caller. Try again after a failed
/// connection until `wait` passes.
async fn open(
    addr: SocketAddr,
    storage: &str,
    options: &Options,
    wait: Duration,
) -> Result<zoe_client::Client, Error> {
    let deadline = Instant::now() + wait;
    loop {
        match zoe_client::Client::open(Arc::new(Tokio), addr, storage, options).await {
            Err(Error::Io(_) | Error::Disconnected) if Instant::now() + RETRY < deadline => {
                tokio::time::sleep(RETRY).await;
            }
            Err(Error::Io(error)) => {
                return Err(Error::Io(std::io::Error::new(
                    error.kind(),
                    format!("no connection to {addr} within {wait:?}: {error}"),
                )));
            }
            opened => return opened,
        }
    }
}

/// The error of a call without an open transaction.
fn not_open() -> Error {
    Error::Invalid("no transaction is open".into())
}

/// Return the exception of Python for an error of the client.
fn client_error(py: Python<'_>, error: Error) -> PyErr {
    match error {
        Error::Conflict { oid, serials } => {
            errors::conflict(py, errors::conflict_class(py), oid, serials, None)
        }
        Error::ReadConflict { oid, serials } => {
            errors::conflict(py, errors::read_conflict_class(py), oid, serials, None)
        }
        Error::Disconnected => {
            errors::client_disconnected(py, "the connection to the server is lost".into())
        }
        Error::Io(error) => errors::client_disconnected(py, error.to_string()),
        Error::Server { class, message } => server_error(py, &class, message),
        Error::Unsupported(text) => errors::unsupported_failure(py, text),
        Error::Invalid(text) => errors::transaction_failure(py, text),
        error => errors::storage_failure(py, error.to_string()),
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
fn to_value(object: &Bound<'_, PyAny>, depth: usize) -> PyResult<Value> {
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
fn to_python<'py>(py: Python<'py>, value: &Value, depth: usize) -> PyResult<Bound<'py, PyAny>> {
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
    fn an_ip_address_resolves_to_itself() {
        assert_eq!(
            resolve("127.0.0.1", 8100).ok(),
            Some("127.0.0.1:8100".parse().unwrap())
        );
    }

    #[test]
    fn only_the_exceptions_of_known_modules_are_raised() {
        for module in ["ZODB.POSException", "ZEO.Exceptions", "builtins"] {
            assert!(MODULES.contains(&module));
        }
        assert!(!MODULES.contains(&"os"));
    }
}
