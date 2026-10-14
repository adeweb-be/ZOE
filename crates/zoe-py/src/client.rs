//! `zoe._zoe.Client`: the native client of `zoe.zeo.ClientStorage`, on
//! `zoe-client`.
//!
//! `zoe.zeo` wraps it in a ZODB storage when `native` is true. One client
//! holds one connection to one storage of a server, and one transaction at
//! a time. It connects over TCP or a unix domain socket, with TLS or
//! without, to the first of its addresses that opens. It has no cache, no
//! blob and no reconnect.
//!
//! A tokio runtime of the process runs the connections. It starts at the
//! first open of a client, never at the import. Each call waits for the
//! server inside `py.detach`: it releases the GIL, and on a free-threaded
//! build it holds nothing.
//!
//! When the server accepted `trace_context`, each call reads the W3C
//! traceparent of the current span on the calling thread, when it starts,
//! through the callable `traceparent` of the open. The call then carries
//! it to the server.
//!
//! A child of `os.fork()` has no thread of the runtime of its parent. A
//! client of the parent raises `RuntimeError` at each call in the child,
//! and its drop there frees nothing. A client that the child opens gets a
//! runtime of the child.

use std::mem;
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use pyo3::PyTraverseError;
use pyo3::exceptions::{PyException, PyRuntimeError, PyTypeError, PyValueError};
use pyo3::gc::PyVisit;
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyBytes, PyDict, PyFloat, PyInt, PyList, PyString, PyTuple, PyType};
use rmpv::Value;
use tokio::runtime::Runtime;
use zoe_client::{
    Address, Endpoint, Error, Invalidations, Options, Tls, TlsOptions, Tokio, Transaction, traced,
};
use zoe_core::{Oid, Tid};

use crate::errors::{self, p64, u64_of};

/// The number of worker threads of the runtime of the native clients.
const WORKERS: usize = 2;

/// The time between two attempts to open a client, until `wait_timeout`.
const RETRY: Duration = Duration::from_millis(100);

/// The deepest nesting of a value that the client converts.
const DEPTH: usize = 32;

/// The extension that lets a call carry the trace context of the client.
const TRACE_CONTEXT: &str = "trace_context";
/// The server name of TLS on a unix domain socket without
/// `server_hostname`. The client then does not check the name, so any
/// valid name does.
const UNIX_SERVER_NAME: &str = "localhost";

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
    /// The callable that returns the traceparent of the current span, when
    /// the server accepted `trace_context`. Else `None`.
    traceparent: Option<Py<PyAny>>,
    /// `true` when the client registered read-only.
    read_only: bool,
    /// The address of the server.
    address: Address,
}

#[pymethods]
impl NativeClient {
    /// Connect to the first server of `addresses` that opens, and register
    /// with the storage `storage`, read-only when `read_only` is true.
    /// Declare `extensions` in `register`, and call `get_info`.
    ///
    /// An address is a `(host, port)` tuple or the path of a unix domain
    /// socket as a `str`. The client connects to all of them and keeps one
    /// server: with `read_only_fallback`, a writable server before a
    /// read-only one.
    ///
    /// `tls` is `None`, or the tuple `(certificate, key, authenticate,
    /// check_hostname, server_hostname)` of the files and checks of TLS.
    /// `key` is `None` when `certificate` holds the key. The server name is
    /// `server_hostname`, else the host of the address. With TLS, the
    /// client never connects without TLS.
    ///
    /// When the server accepts `trace_context`, each call calls
    /// `traceparent()` on the calling thread when it starts. A str result
    /// is the W3C traceparent that the call carries. `None` gives a call
    /// without one.
    ///
    /// A connection that fails, or that the server closes before the
    /// register, is tried again until `wait_timeout` seconds pass. Then
    /// the open raises `ClientDisconnected`. Another error of the open
    /// raises at once. A file of `tls` that does not read raises
    /// `ValueError`.
    #[new]
    #[pyo3(signature = (
        addresses, storage, *, read_only, read_only_fallback, extensions, wait_timeout, tls=None,
        traceparent=None
    ))]
    #[expect(clippy::too_many_arguments)]
    fn new(
        py: Python<'_>,
        addresses: Vec<Bound<'_, PyAny>>,
        storage: String,
        read_only: bool,
        read_only_fallback: bool,
        extensions: Vec<String>,
        wait_timeout: f64,
        tls: Option<TlsArgs>,
        traceparent: Option<Py<PyAny>>,
    ) -> PyResult<Self> {
        let wait = Duration::try_from_secs_f64(wait_timeout)
            .map_err(|_| PyValueError::new_err("wait_timeout is not a duration"))?;
        let targets = addresses
            .iter()
            .map(Target::extract)
            .collect::<PyResult<Vec<_>>>()?;
        let options = Options {
            read_only,
            read_only_fallback,
            extensions,
        };
        let opened = py.detach(move || -> Result<_, Opening> {
            let secure = tls.as_ref().map(Secure::load).transpose()?;
            let runtime = runtime().map_err(Opening::Runtime)?;
            let open = open(&targets, secure.as_ref(), &storage, &options, wait);
            let client = runtime.block_on(open)?;
            // Subscribe first, then read the last tid: the client raises
            // its last tid before it sends an invalidation.
            let invalidations = client.invalidations();
            let wake = client.invalidations();
            let opened_at = client.last_transaction();
            Ok((runtime, client, invalidations, wake, opened_at))
        });
        let (runtime, client, invalidations, wake, opened_at) = match opened {
            Ok(opened) => opened,
            Err(Opening::Address(text) | Opening::Tls(text)) => {
                return Err(PyValueError::new_err(text));
            }
            Err(Opening::Runtime(error)) => return Err(error.into()),
            Err(Opening::Client(error)) => return Err(client_error(py, error)),
        };
        let traces = client.extensions().iter().any(|name| name == TRACE_CONTEXT);
        Ok(Self {
            pid: std::process::id(),
            runtime,
            read_only: client.read_only(),
            address: client.address().clone(),
            client: Mutex::new(Some(Arc::new(client))),
            transaction: Mutex::new(None),
            invalidations: Mutex::new(Some(invalidations)),
            wake: Mutex::new(Some(wake)),
            opened_at,
            traceparent: traceparent.filter(|_| traces),
        })
    }

    // The class is frozen, so it has no `__clear__`. The callable makes no
    // cycle that the collector cannot break at another object.
    fn __traverse__(&self, visit: PyVisit<'_>) -> Result<(), PyTraverseError> {
        if let Some(traceparent) = &self.traceparent {
            visit.call(traceparent)?;
        }
        Ok(())
    }

    /// Tell if the client registered read-only: as asked, or after a
    /// read-only fallback.
    fn read_only(&self) -> bool {
        self.read_only
    }

    /// Return the address of the server: `(host, port)` with the IP address
    /// of the server, or the path of a unix domain socket.
    fn address<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        Ok(match &self.address {
            Address::Tcp(addr) => (addr.ip().to_string(), addr.port())
                .into_pyobject(py)?
                .into_any(),
            Address::Unix(path) => PyString::new(py, &path.to_string_lossy()).into_any(),
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
        let trace = self.traceparent(py)?;
        let runtime = self.runtime;
        let oids = py
            .detach(move || runtime.block_on(traced(trace, client.new_oids(n))))
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
        let trace = self.traceparent(py)?;
        let runtime = self.runtime;
        let record = py
            .detach(move || runtime.block_on(traced(trace, client.load_before(oid, tid))))
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
        let trace = self.traceparent(py)?;
        let runtime = self.runtime;
        let data = py
            .detach(move || runtime.block_on(traced(trace, client.load_serial(oid, serial))))
            .map_err(|e| client_error(py, e))?;
        Ok(PyBytes::new(py, &data))
    }

    /// Return the next value of the sequence `name` of the store. Raise
    /// `Unsupported` when the server did not accept `sequences`.
    fn sequence_next(&self, py: Python<'_>, name: String) -> PyResult<u64> {
        let client = self.client(py)?;
        let trace = self.traceparent(py)?;
        let runtime = self.runtime;
        py.detach(move || runtime.block_on(traced(trace, client.sequence_next(&name))))
            .map_err(|e| client_error(py, e))
    }

    /// Begin a transaction. `extension` is the dict of the transaction
    /// extension. The caller holds the commit lock: a second transaction
    /// raises `StorageTransactionError`.
    fn begin(
        &self,
        py: Python<'_>,
        user: &[u8],
        description: &[u8],
        extension: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        let mut encoded = Vec::new();
        rmpv::encode::write_value(&mut encoded, &to_value(extension, 0)?)
            .map_err(|e| PyValueError::new_err(format!("the extension does not encode: {e}")))?;
        let client = self.client(py)?;
        let trace = self.traceparent(py)?;
        let runtime = self.runtime;
        let (user, description) = (user.to_vec(), description.to_vec());
        py.detach(|| {
            let mut slot = lock(&self.transaction);
            if slot.is_some() {
                return Err(Error::Invalid("a transaction is open".into()));
            }
            let begin = client.begin(&user, &description, &encoded);
            let txn = runtime.block_on(traced(trace, begin))?;
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
        let trace = self.traceparent(py)?;
        let runtime = self.runtime;
        py.detach(|| {
            let mut slot = lock(&self.transaction);
            let txn = slot.as_mut().ok_or_else(not_open)?;
            runtime.block_on(traced(trace, txn.store(oid, serial, data)))
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
        let trace = self.traceparent(py)?;
        let runtime = self.runtime;
        py.detach(|| {
            let mut slot = lock(&self.transaction);
            let txn = slot.as_mut().ok_or_else(not_open)?;
            runtime.block_on(traced(trace, txn.check_current_serial(oid, serial)))
        })
        .map_err(|e| client_error(py, e))
    }

    /// Vote, and return the oids that the server resolved. A failed vote
    /// ends the transaction: the server aborted it.
    fn vote<'py>(&self, py: Python<'py>) -> PyResult<Vec<Bound<'py, PyBytes>>> {
        self.check_process()?;
        let trace = self.traceparent(py)?;
        let runtime = self.runtime;
        let resolved = py
            .detach(|| {
                let mut slot = lock(&self.transaction);
                let txn = slot.as_mut().ok_or_else(not_open)?;
                let voted = runtime.block_on(traced(trace, txn.vote_resolved()));
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
        let trace = self.traceparent(py)?;
        let runtime = self.runtime;
        let tid = py
            .detach(|| {
                let txn = lock(&self.transaction).take().ok_or_else(not_open)?;
                runtime.block_on(traced(trace, txn.finish()))
            })
            .map_err(|e| client_error(py, e))?;
        Ok(p64(py, tid.0))
    }

    /// Abort the open transaction, and wait for the reply of the server.
    /// Without an open transaction, do nothing.
    fn abort(&self, py: Python<'_>) -> PyResult<()> {
        self.check_process()?;
        let trace = self.traceparent(py)?;
        let runtime = self.runtime;
        py.detach(|| {
            let txn = lock(&self.transaction).take();
            match txn {
                Some(txn) => runtime.block_on(traced(trace, txn.abort())),
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

    /// Return the W3C traceparent of the current span of the calling
    /// thread, or `None`. Without the callable, it calls no Python code.
    fn traceparent(&self, py: Python<'_>) -> PyResult<Option<String>> {
        match &self.traceparent {
            Some(traceparent) => Ok(traceparent.bind(py).call0()?.extract()?),
            None => Ok(None),
        }
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
    /// An address or a server name is not valid.
    Address(String),
    /// The files of TLS do not read, or the settings of TLS do not fit an
    /// address.
    Tls(String),
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

/// The files and the checks of TLS that `zoe.zeo` gives: the certificate,
/// the key or `None`, the CA file of `authenticate`, `check_hostname` and
/// `server_hostname` or `None`.
type TlsArgs = (PathBuf, Option<PathBuf>, PathBuf, bool, Option<String>);

/// The TLS of the endpoints: the loaded files and the name settings.
struct Secure {
    tls: Tls,
    check_hostname: bool,
    server_hostname: Option<String>,
}

impl Secure {
    /// Read the files of `args`. An error names the file.
    fn load(args: &TlsArgs) -> Result<Self, Opening> {
        let (certificate, key, authenticate, check_hostname, server_hostname) = args;
        let mut options = TlsOptions::new(certificate, authenticate);
        if let Some(key) = key {
            options = options.key(key);
        }
        let options = options.check_hostname(*check_hostname);
        Ok(Self {
            tls: Tls::load(&options).map_err(Opening::Tls)?,
            check_hostname: *check_hostname,
            server_hostname: server_hostname.clone(),
        })
    }
}

/// An address that `zoe.zeo` gives.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Target {
    /// A host and a port.
    Tcp(String, u16),
    /// The path of a unix domain socket.
    Unix(PathBuf),
}

impl std::fmt::Display for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Target::Tcp(host, port) => write!(f, "{host}:{port}"),
            Target::Unix(path) => write!(f, "{}", path.display()),
        }
    }
}

impl Target {
    /// Read a `(host, port)` tuple, or the path of a unix domain socket as
    /// a `str`. Another value raises `TypeError`.
    fn extract(address: &Bound<'_, PyAny>) -> PyResult<Self> {
        if let Ok(path) = address.cast::<PyString>() {
            return Ok(Target::Unix(PathBuf::from(path.to_str()?)));
        }
        if let Ok(pair) = address.cast::<PyTuple>()
            && let Ok((host, port)) = pair.extract::<(String, u16)>()
        {
            return Ok(Target::Tcp(host, port));
        }
        Err(PyTypeError::new_err(format!(
            "an address is (host, port) or the path of a unix socket, not {address}"
        )))
    }
}

/// The endpoints of one round of the open, and the error of each host that
/// did not resolve.
struct Round {
    endpoints: Vec<Endpoint>,
    unresolved: Vec<String>,
}

/// Return the endpoints of `targets`, each with TLS when `secure` is given.
/// `resolve` gives the IP address of a host and a port. A host that does
/// not resolve is left out of the round, and its error is kept. A setting
/// that does not fit an address fails with `Opening`.
fn endpoints(
    targets: &[Target],
    secure: Option<&Secure>,
    resolve: impl Fn(&str, u16) -> Result<SocketAddr, String>,
) -> Result<Round, Opening> {
    let mut round = Round {
        endpoints: Vec::new(),
        unresolved: Vec::new(),
    };
    for target in targets {
        let (endpoint, host) = match target {
            Target::Tcp(host, port) => match resolve(host, *port) {
                Ok(addr) => (Endpoint::new(addr), Some(host.as_str())),
                Err(error) => {
                    round.unresolved.push(error);
                    continue;
                }
            },
            Target::Unix(path) => (Endpoint::new(Address::Unix(path.clone())), None),
        };
        let Some(secure) = secure else {
            round.endpoints.push(endpoint);
            continue;
        };
        let name = match (secure.server_hostname.as_deref(), host) {
            (Some(name), _) | (None, Some(name)) => name,
            (None, None) if secure.check_hostname => {
                return Err(Opening::Tls(format!(
                    "TLS on the unix socket {target} checks the name of the server: \
                     give server_hostname"
                )));
            }
            (None, None) => UNIX_SERVER_NAME,
        };
        let endpoint = endpoint
            .tls(secure.tls.clone(), name)
            .map_err(|error| Opening::Address(error.to_string()))?;
        round.endpoints.push(endpoint);
    }
    Ok(round)
}

/// Return the first IP address of `host` and `port`.
fn resolve(host: &str, port: u16) -> Result<SocketAddr, String> {
    let mut addrs = (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("the address {host}:{port}: {e}"))?;
    addrs
        .next()
        .ok_or_else(|| format!("the address {host}:{port} has no IP address"))
}

/// Open a client to the first server of `targets` that opens, on the
/// runtime of the caller. Each round resolves the hosts again. Try again
/// after failed connections until `wait` passes.
async fn open(
    targets: &[Target],
    secure: Option<&Secure>,
    storage: &str,
    options: &Options,
    wait: Duration,
) -> Result<zoe_client::Client, Opening> {
    let deadline = Instant::now() + wait;
    loop {
        let round = endpoints(targets, secure, resolve)?;
        let left = deadline.saturating_duration_since(Instant::now());
        let opened = if round.endpoints.is_empty() {
            Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                round.unresolved.join("; "),
            )))
        } else {
            let runtime = Arc::new(Tokio);
            zoe_client::Client::open_first(runtime, &round.endpoints, storage, options, left).await
        };
        match opened {
            Err(Error::Io(_) | Error::Disconnected) if Instant::now() + RETRY < deadline => {
                tokio::time::sleep(RETRY).await;
            }
            Err(Error::Io(error)) => {
                let addresses: Vec<String> = targets.iter().map(Target::to_string).collect();
                let mut text = format!(
                    "no connection to {} within {wait:?}: {error}",
                    addresses.join(", ")
                );
                if !round.endpoints.is_empty() && !round.unresolved.is_empty() {
                    text = format!("{text}; {}", round.unresolved.join("; "));
                }
                return Err(Opening::Client(Error::Io(std::io::Error::new(
                    error.kind(),
                    text,
                ))));
            }
            opened => return Ok(opened?),
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
    fn a_unix_socket_has_no_resolution() {
        let targets = [
            Target::Unix(PathBuf::from("./zeo.sock")),
            Target::Tcp("127.0.0.1".into(), 8100),
        ];
        let round = endpoints(&targets, None, resolve).ok().unwrap();
        assert!(round.unresolved.is_empty());
        let endpoints = round.endpoints;
        let addresses: Vec<_> = endpoints.iter().map(Endpoint::address).collect();
        assert_eq!(
            addresses,
            [
                &Address::Unix(PathBuf::from("./zeo.sock")),
                &Address::Tcp("127.0.0.1:8100".parse().unwrap()),
            ]
        );
        assert!(endpoints.iter().all(|endpoint| !endpoint.is_tls()));
    }

    #[test]
    fn a_tls_file_that_does_not_read_fails_the_open_and_names_it() {
        let missing = std::env::temp_dir().join(format!("zoe-py-missing-{}", std::process::id()));
        let tls = (
            missing.join("client.pem"),
            None,
            missing.join("ca.pem"),
            true,
            None,
        );
        match Secure::load(&tls) {
            Err(Opening::Tls(text)) => assert!(text.contains("client.pem"), "{text}"),
            _ => panic!("the open did not fail on the missing file"),
        }
    }

    #[test]
    fn a_host_that_does_not_resolve_leaves_the_others_in_the_round() {
        let targets = [
            Target::Tcp("no-such-host".into(), 8100),
            Target::Tcp("127.0.0.1".into(), 8100),
        ];
        let fake = |host: &str, port: u16| match host {
            "no-such-host" => Err(format!("the address {host}:{port}: not found")),
            _ => resolve(host, port),
        };
        let round = endpoints(&targets, None, fake).ok().unwrap();
        assert_eq!(round.endpoints.len(), 1);
        assert_eq!(
            round.endpoints[0].address(),
            &Address::Tcp("127.0.0.1:8100".parse().unwrap())
        );
        assert_eq!(round.unresolved.len(), 1);
        assert!(round.unresolved[0].contains("no-such-host"));
    }

    #[test]
    fn an_address_shows_as_zeo_gives_it() {
        assert_eq!(Target::Tcp("db".into(), 8100).to_string(), "db:8100");
        assert_eq!(Target::Unix("./zeo.sock".into()).to_string(), "./zeo.sock");
    }

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
