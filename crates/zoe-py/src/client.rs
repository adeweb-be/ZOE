//! `zoe._zoe.Client`: the native client of `zoe.zeo.ClientStorage`, on
//! `zoe-client`.
//!
//! `zoe.zeo` wraps it in a ZODB storage when `native` is true. One client
//! holds a pool of connections to one storage of a server (ADR 0036): the
//! listener, and one connection for each transaction that runs. It
//! connects over TCP or a unix domain socket, with TLS or without, to the
//! first of its addresses that opens. When it loses its server, it
//! connects again in the background, and verifies its cache, as
//! `ZEO.ClientStorage` does. With the read-only fallback, it takes a
//! writable server when one opens.
//!
//! `send_blob` downloads a blob into a file of the blob cache of
//! `zoe.zeo`, and `Transaction.store_blob` uploads a blob file. With a blob
//! directory that the server shares, `Transaction.store_blob_shared` sends
//! only the name of the file. `zoe.zeo` keeps the blob cache: its layout,
//! its locks and its pruning.
//!
//! With a `cache_size`, the pool holds a client cache in memory (ADR 0034).
//! A hit runs on the calling thread: it takes no lock, sends nothing and
//! does not release the GIL. A miss loads from the server and stores the
//! reply.
//!
//! The pool delivers the tids of the listener in one order. A thread of
//! `zoe.zeo` takes each commit of another client, and each change of the
//! connection, with `next_delivery`, and confirms it with `delivered`.
//! `Transaction.finish` returns the tid of a commit at its turn, and
//! `Transaction.done` ends the turn after the callback of `tpc_finish`.
//!
//! `Transaction.vote` resolves the conflicts that the vote lists with the
//! callable that `zoe.zeo` gives, ZODB's `tryToResolveConflict`. The
//! callable runs on the thread of the transaction, with no lock held.
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

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::mem;
use std::net::{SocketAddr, ToSocketAddrs};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant};

use pyo3::PyTraverseError;
use pyo3::exceptions::{PyException, PyMemoryError, PyRuntimeError, PyTypeError, PyValueError};
use pyo3::gc::PyVisit;
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyBytes, PyDict, PyFloat, PyInt, PyList, PyString, PyTuple, PyType};
use rmpv::Value;
use tokio::runtime::Runtime;
use zoe_client::{
    Address, Cache, Connect, Delivery, Endpoint, Error, Event, Open, Options, Pool,
    PoolTransaction, Reconnect, Tls, TlsOptions, Tokio, Turn, Verification, traced,
};
use zoe_core::{Oid, Tid};

use crate::errors::{self, p64, u64_of};

/// The number of worker threads of the runtime of the native clients.
const WORKERS: usize = 2;

/// The time between two attempts to open a client, until `wait_timeout`.
const RETRY: Duration = Duration::from_millis(100);

/// The seconds between two attempts to connect again, when the open gives
/// no `disconnect_poll`: the `connect_poll` of `ZEO.ClientStorage`.
const DISCONNECT_POLL: f64 = 1.0;

/// The most connections of a client that commit at once, when the open
/// gives no `pool_bound`. `zoe.zeo._POOL_BOUND` has the same value.
const POOL_BOUND: usize = 64;

/// The deepest nesting of a value that the client converts.
const DEPTH: usize = 32;

/// The most rounds of conflict resolution in one vote, as the vote of
/// `ZEO.ClientStorage` has.
const ROUNDS: usize = 9;

/// The `before` of a load of the current record: ZODB's `maxtid`.
const MAX_TID: Tid = Tid(i64::MAX as u64);

/// The extension that lets a call carry the trace context of the client.
const TRACE_CONTEXT: &str = "trace_context";

/// The mode of a blob file of the cache: read-only for its owner, as
/// `ZEO.ClientStorage` sets it (`stat.S_IREAD`).
const BLOB_MODE: u32 = 0o400;
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
/// that the server accepts: a pool of connections with one listener.
#[pyclass(frozen, module = "zoe._zoe", name = "Client")]
pub(crate) struct NativeClient {
    /// The process that opened the client.
    pid: u32,
    runtime: &'static Runtime,
    /// The pool, until `close`.
    pool: Mutex<Option<Arc<Pool>>>,
    /// The client cache of the pool, if any. The hits read it without the
    /// mutex of the pool. The close empties it.
    cache: Option<Arc<Cache>>,
    /// The transactions that `begin` gave, while they live.
    open: Mutex<Vec<Weak<Slot>>>,
    /// The last tid of the listener after the open.
    opened_at: Tid,
    /// The last tid that the pool delivered before `close`.
    closed_at: AtomicU64,
    /// The callable that returns the traceparent of the current span. A
    /// call calls it only when its server accepted `trace_context`.
    traceparent: Option<Py<PyAny>>,
    /// `true` when the open asked for a read-only client.
    read_only: bool,
    /// What the client connects to. `new_addr` changes its addresses.
    servers: Arc<Servers>,
}

#[pymethods]
impl NativeClient {
    /// Connect to the first server of `addresses` that opens, and register
    /// with the storage `storage`, read-only when `read_only` is true.
    /// Declare `extensions` in `register`, and call `get_info`. That
    /// connection is the listener of the pool. The pool opens at most
    /// `pool_bound` more connections to the same server, each for one
    /// transaction at a time, when a transaction finds none free. A
    /// `pool_bound` of 0 raises `ValueError`.
    ///
    /// A `cache_size` above 0 gives the pool a client cache whose ring
    /// holds that many bytes. A `cache_size` from 1 to 7 raises
    /// `ValueError`. A mapping that the kernel does not give raises
    /// `MemoryError`.
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
    /// With `wait`, a connection that fails, or that the server closes
    /// before the register, is tried again until `wait_timeout` seconds
    /// pass. Then the open raises `ClientDisconnected`. Another error of
    /// the open raises at once. Without `wait`, the open does not connect:
    /// the client connects in the background, and each call raises
    /// `ClientDisconnected` until then. A file of `tls` that does not read
    /// raises `ValueError`.
    ///
    /// When the client loses its server, it connects again in the
    /// background: at once, then every `disconnect_poll` seconds, each
    /// attempt for at most `wait_timeout` seconds. It verifies its cache
    /// against the new server first. With `read_only_fallback`, a client on
    /// a read-only server tries a writable server every `disconnect_poll`
    /// seconds, and takes it.
    ///
    /// With `client_label`, each connection sends it in `set_client_label`
    /// after `get_info`, and the server puts it in its log lines.
    #[new]
    #[pyo3(signature = (
        addresses, storage, *, read_only, read_only_fallback, extensions, wait_timeout,
        pool_bound=POOL_BOUND, cache_size=0, tls=None, traceparent=None, wait=true,
        disconnect_poll=DISCONNECT_POLL, client_label=None
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
        pool_bound: usize,
        cache_size: u64,
        tls: Option<TlsArgs>,
        traceparent: Option<Py<PyAny>>,
        wait: bool,
        disconnect_poll: f64,
        client_label: Option<String>,
    ) -> PyResult<Self> {
        let timeout = Duration::try_from_secs_f64(wait_timeout)
            .map_err(|_| PyValueError::new_err("wait_timeout is not a duration"))?;
        let poll = Duration::try_from_secs_f64(disconnect_poll)
            .ok()
            .filter(|poll| !poll.is_zero())
            .ok_or_else(|| PyValueError::new_err("disconnect_poll is not a duration"))?;
        if pool_bound == 0 {
            return Err(PyValueError::new_err("pool_bound is at least 1"));
        }
        let cache = match cache_size {
            0 => None,
            size => Some(Cache::new(size).map_err(|e| {
                let text = format!("cache_size {size}: {e}");
                // A size that the cache refuses is a bad value. Any other
                // error is a mapping that the kernel did not give.
                match e.kind() {
                    std::io::ErrorKind::InvalidInput => PyValueError::new_err(text),
                    _ => PyMemoryError::new_err(text),
                }
            })?),
        };
        let targets = addresses
            .iter()
            .map(Target::extract)
            .collect::<PyResult<Vec<_>>>()?;
        let options = Options {
            read_only,
            read_only_fallback,
            extensions,
            client_label,
        };
        let upgrade = read_only_fallback && !read_only;
        let opened = py.detach(move || -> Result<_, Opening> {
            let secure = tls.as_ref().map(Secure::load).transpose()?.map(Arc::new);
            let runtime = runtime().map_err(Opening::Runtime)?;
            // A setting that does not fit an address fails the open, also
            // without `wait`.
            endpoints(&targets, secure.as_deref(), |_, port| {
                Ok(SocketAddr::from(([127, 0, 0, 1], port)))
            })?;
            let servers = Arc::new(Servers {
                targets: Mutex::new(targets),
                secure,
                storage,
                options,
                wait: timeout,
            });
            let first = match wait {
                true => Some(runtime.block_on(servers.open(&servers.options, true))?),
                false => None,
            };
            let reopen = servers.clone();
            let open: Open = Arc::new(move |writable| {
                let servers = reopen.clone();
                Box::pin(async move { servers.reopen(writable).await })
            });
            let reconnect = Reconnect {
                upgrade,
                ..Reconnect::new(open, poll)
            };
            // The pool starts its tasks on the runtime.
            let _inside = runtime.enter();
            let pool = Pool::with_reconnect(Arc::new(Tokio), first, pool_bound, cache, reconnect);
            Ok((runtime, pool, servers))
        });
        let (runtime, pool, servers) = match opened {
            Ok(opened) => opened,
            Err(Opening::Address(text) | Opening::Tls(text)) => {
                return Err(PyValueError::new_err(text));
            }
            Err(Opening::Runtime(error)) => return Err(error.into()),
            Err(Opening::Client(error)) => return Err(client_error(py, error)),
        };
        Ok(Self {
            pid: std::process::id(),
            runtime,
            read_only,
            opened_at: pool.last_transaction(),
            cache: pool.cache().cloned(),
            closed_at: AtomicU64::new(pool.last_transaction().0),
            pool: Mutex::new(Some(Arc::new(pool))),
            open: Mutex::new(Vec::new()),
            traceparent,
            servers,
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

    /// Tell if the listener registered read-only: as asked, or after a
    /// read-only fallback. Without a listener, tell if the open asked for
    /// a read-only client.
    fn read_only(&self) -> PyResult<bool> {
        self.check_process()?;
        Ok(self
            .listener()
            .map_or(self.read_only, |listener| listener.read_only()))
    }

    /// Return the address of the server: `(host, port)` with the IP address
    /// of the server, or the path of a unix domain socket. Return `None`
    /// without a listener.
    fn address<'py>(&self, py: Python<'py>) -> PyResult<Option<Bound<'py, PyAny>>> {
        self.check_process()?;
        let Some(listener) = self.listener() else {
            return Ok(None);
        };
        Ok(Some(match listener.address() {
            Address::Tcp(addr) => (addr.ip().to_string(), addr.port())
                .into_pyobject(py)?
                .into_any(),
            Address::Unix(path) => PyString::new(py, &path.to_string_lossy()).into_any(),
        }))
    }

    /// Return the last tid that the client knew after the open: the tid
    /// of `register` of the listener.
    fn opened_at<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        p64(py, self.opened_at.0)
    }

    /// Return the reply of `get_info` of the listener, as a dict, with
    /// the last `info` call of the server over it, as `_info` of
    /// `ZEO.ClientStorage` holds them. Without a listener, return an empty
    /// dict.
    fn info<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        self.check_process()?;
        let info = lock(&self.pool).as_ref().and_then(|pool| pool.info());
        match info {
            Some(info) => to_python(py, &info, 0),
            None => Ok(PyDict::new(py).into_any()),
        }
    }

    /// Take `addresses` in place of the addresses of the open, as
    /// `new_addr` of `ZEO.ClientStorage` does. The client keeps its server.
    /// Each later attempt to connect, after the loss of the server, tries
    /// the new addresses. An address takes the form of the open. A setting
    /// of TLS that does not fit an address raises `ValueError`, and the
    /// addresses stay.
    fn new_addr(&self, addresses: Vec<Bound<'_, PyAny>>) -> PyResult<()> {
        self.check_process()?;
        let targets = addresses
            .iter()
            .map(Target::extract)
            .collect::<PyResult<Vec<_>>>()?;
        endpoints(&targets, self.servers.secure.as_deref(), |_, port| {
            Ok(SocketAddr::from(([127, 0, 0, 1], port)))
        })
        .map_err(|error| match error {
            Opening::Address(text) | Opening::Tls(text) => PyValueError::new_err(text),
            error => PyValueError::new_err(error.into_error().to_string()),
        })?;
        *lock(&self.servers.targets) = targets;
        Ok(())
    }

    /// Call `method` of the server with `args` on the listener, and return
    /// its reply, as `_call` of `ZEO.ClientStorage` does. An array of the
    /// reply becomes a tuple, and a map a dict. An error reply raises the
    /// exception that `ZEO.ClientStorage` raises for it. A call of a
    /// transaction raises `StorageTransactionError` and sends nothing.
    fn call<'py>(
        &self,
        py: Python<'py>,
        method: String,
        args: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let args = args_of(args)?;
        let pool = self.pool(py)?;
        let trace = self.traceparent(py, &pool)?;
        let runtime = self.runtime;
        let reply = py
            .detach(move || runtime.block_on(traced(trace, pool.call(&method, args))))
            .map_err(|e| zeo_error(py, e))?;
        to_python(py, &reply, 0)
    }

    /// Send `method` with `args` on the listener as a call without a reply,
    /// as `_async` of `ZEO.ClientStorage` does. The rules of `call` apply.
    fn call_async(&self, py: Python<'_>, method: String, args: &Bound<'_, PyAny>) -> PyResult<()> {
        let args = args_of(args)?;
        let pool = self.pool(py)?;
        let trace = self.traceparent(py, &pool)?;
        let runtime = self.runtime;
        py.detach(move || runtime.block_on(traced(trace, pool.notify(&method, args))))
            .map_err(|e| client_error(py, e))
    }

    /// Load each of `oids` before `tid` that the cache does not hold, all
    /// at once, and return the number of loads. It does not wait for the
    /// replies: they fill the cache. Without a cache, it loads nothing.
    /// Without a server, it raises `ClientDisconnected`.
    fn prefetch(&self, py: Python<'_>, oids: Vec<Bound<'_, PyAny>>, tid: &Bound<'_, PyAny>) -> PyResult<usize> {
        let tid = Tid(u64_of(tid)?);
        let oids = oids
            .iter()
            .map(|oid| u64_of(oid).map(Oid))
            .collect::<PyResult<Vec<_>>>()?;
        let pool = self.pool(py)?;
        let runtime = self.runtime;
        py.detach(move || {
            // The tasks of the loads start on the runtime.
            let _inside = runtime.enter();
            pool.prefetch(&oids, tid)
        })
        .map_err(|e| client_error(py, e))
    }

    /// Call `ping` on the listener and wait for its reply. With `wait`,
    /// wait then until the client delivered each tid that the listener read
    /// before the reply. The thread that runs a delivery must not wait.
    fn sync(&self, py: Python<'_>, wait: bool) -> PyResult<()> {
        let pool = self.pool(py)?;
        let trace = self.traceparent(py, &pool)?;
        let runtime = self.runtime;
        py.detach(move || runtime.block_on(traced(trace, pool.sync(wait))))
            .map_err(|e| client_error(py, e))
    }

    /// Return the extensions that the server of the listener accepted.
    fn extensions(&self) -> PyResult<Vec<String>> {
        self.check_process()?;
        Ok(self
            .listener()
            .map(|listener| listener.extensions().to_vec())
            .unwrap_or_default())
    }

    /// Tell if the client is connected: not closed, and its listener is not
    /// lost.
    fn is_connected(&self) -> bool {
        self.pid == std::process::id()
            && lock(&self.pool)
                .as_ref()
                .is_some_and(|pool| pool.is_connected())
    }

    /// Return the most connections that commit at once.
    fn pool_bound(&self, py: Python<'_>) -> PyResult<usize> {
        Ok(self.pool(py)?.bound())
    }

    /// Return the last tid that the pool delivered. With `wait`, wait first
    /// for the delivery that runs now, if one runs: the thread that runs it
    /// must not wait. After `close`, return the last tid before the close.
    fn last_transaction<'py>(&self, py: Python<'py>, wait: bool) -> PyResult<Bound<'py, PyBytes>> {
        self.check_process()?;
        let pool = {
            let slot = lock(&self.pool);
            match slot.as_ref() {
                Some(pool) => pool.clone(),
                None => return Ok(p64(py, self.closed_at.load(AtomicOrdering::SeqCst))),
            }
        };
        let tid = if wait {
            let runtime = self.runtime;
            py.detach(move || runtime.block_on(pool.settled_transaction()))
        } else {
            pool.last_transaction()
        };
        Ok(p64(py, tid.0))
    }

    /// Return `n` new oids.
    fn new_oids<'py>(&self, py: Python<'py>, n: usize) -> PyResult<Vec<Bound<'py, PyBytes>>> {
        let pool = self.pool(py)?;
        let trace = self.traceparent(py, &pool)?;
        let runtime = self.runtime;
        let oids = py
            .detach(move || runtime.block_on(traced(trace, pool.new_oids(n))))
            .map_err(|e| client_error(py, e))?;
        Ok(oids.into_iter().map(|oid| p64(py, oid.0)).collect())
    }

    /// Return the record of `oid` that was current before `tid`: its
    /// pickle, its serial and the serial of the next record or `None`.
    /// Return `None` when `oid` has no record before `tid`. A hit of the
    /// cache sends nothing to the server. A miss stores the reply in the
    /// cache.
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
        self.check_process()?;
        if let Some(cache) = &self.cache
            && let Some((data, start, end)) =
                cache.load_before_with(oid, tid, |len, fill| bytes(py, len, fill))
        {
            return Ok(Some((
                data,
                p64(py, start.0),
                end.map(|end| p64(py, end.0)),
            )));
        }
        let pool = self.pool(py)?;
        let trace = self.traceparent(py, &pool)?;
        let runtime = self.runtime;
        let (record, _position) = py
            .detach(move || runtime.block_on(traced(trace, pool.load_before(oid, tid))))
            .map_err(|e| client_error(py, e))?;
        Ok(record.map(|record| {
            (
                PyBytes::new(py, &record.data),
                p64(py, record.tid.0),
                record.end.map(|end| p64(py, end.0)),
            )
        }))
    }

    /// Return the current record of `oid`: its pickle and its serial, or
    /// `None` when `oid` has no record. A hit of the current entry of the
    /// cache sends nothing to the server, as `load` of `ZEO.ClientStorage`
    /// does. A miss stores the reply in the cache.
    #[allow(clippy::type_complexity)]
    fn load<'py>(
        &self,
        py: Python<'py>,
        oid: &Bound<'py, PyAny>,
    ) -> PyResult<Option<(Bound<'py, PyBytes>, Bound<'py, PyBytes>)>> {
        let oid = Oid(u64_of(oid)?);
        self.check_process()?;
        if let Some(cache) = &self.cache
            && let Some((data, start)) = cache.load_with(oid, |len, fill| bytes(py, len, fill))
        {
            return Ok(Some((data, p64(py, start.0))));
        }
        let pool = self.pool(py)?;
        let trace = self.traceparent(py, &pool)?;
        let runtime = self.runtime;
        let (record, _position) = py
            .detach(move || runtime.block_on(traced(trace, pool.load_before(oid, MAX_TID))))
            .map_err(|e| client_error(py, e))?;
        Ok(record.map(|record| (PyBytes::new(py, &record.data), p64(py, record.tid.0))))
    }

    /// Empty the cache, and keep the tid up to which it applied the
    /// commits. Without a cache, do nothing.
    fn cache_clear(&self, py: Python<'_>) -> PyResult<()> {
        self.check_process()?;
        if let Some(cache) = &self.cache {
            py.detach(|| cache.clear());
        }
        Ok(())
    }

    /// Return the pickle of `oid` that the transaction `serial` wrote. An
    /// error of the server raises as for `call`.
    fn load_serial<'py>(
        &self,
        py: Python<'py>,
        oid: &Bound<'py, PyAny>,
        serial: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyBytes>> {
        let (oid, serial) = (Oid(u64_of(oid)?), Tid(u64_of(serial)?));
        let pool = self.pool(py)?;
        let trace = self.traceparent(py, &pool)?;
        let runtime = self.runtime;
        let data = py
            .detach(move || runtime.block_on(traced(trace, pool.load_serial(oid, serial))))
            .map_err(|e| zeo_error(py, e))?;
        Ok(PyBytes::new(py, &data))
    }

    /// Download the blob of `oid` that the transaction `serial` wrote into
    /// the file `filename`, and return its size. The caller holds the lock
    /// of the blob in the cache.
    ///
    /// The bytes go to `<filename>.dl` first. Only a whole blob, whose file
    /// has the size of the bytes that came, becomes `filename`, by rename,
    /// read-only. A failed download removes `<filename>.dl`. A blob that
    /// the server does not have raises `POSKeyError`. An error of the file
    /// raises `OSError`.
    fn send_blob(
        &self,
        py: Python<'_>,
        oid: &Bound<'_, PyAny>,
        serial: &Bound<'_, PyAny>,
        filename: PathBuf,
    ) -> PyResult<u64> {
        let (oid, serial) = (Oid(u64_of(oid)?), Tid(u64_of(serial)?));
        let pool = self.pool(py)?;
        let trace = self.traceparent(py, &pool)?;
        let runtime = self.runtime;
        py.detach(move || {
            let partial = partial_name(&filename);
            let downloaded = download(&partial, |sink| {
                runtime.block_on(traced(trace, pool.send_blob(oid, serial, sink)))
            })
            .and_then(|size| {
                fs::rename(&partial, &filename)?;
                Ok(size)
            });
            if downloaded.is_err() {
                let _ = fs::remove_file(&partial);
            }
            downloaded
        })
        .map_err(|e| blob_error(py, e))
    }

    /// Return the next value of the sequence `name` of the store. Raise
    /// `Unsupported` when the server did not accept `sequences`.
    fn sequence_next(&self, py: Python<'_>, name: String) -> PyResult<u64> {
        let pool = self.pool(py)?;
        let trace = self.traceparent(py, &pool)?;
        let runtime = self.runtime;
        py.detach(move || runtime.block_on(traced(trace, pool.sequence_next(&name))))
            .map_err(|e| client_error(py, e))
    }

    /// Begin a transaction on a free connection of the pool, and return
    /// it. `extension` is the dict of the transaction extension. With
    /// `tid`, the transaction commits under that tid, with the status
    /// `status`. When every connection is busy and the pool is at its
    /// bound, wait for a free one.
    #[pyo3(signature = (user, description, extension, tid = None, status = " ".to_owned()))]
    fn begin(
        &self,
        py: Python<'_>,
        user: &[u8],
        description: &[u8],
        extension: &Bound<'_, PyAny>,
        tid: Option<&Bound<'_, PyAny>>,
        status: String,
    ) -> PyResult<NativeTransaction> {
        let tid = tid.map(u64_of).transpose()?.map(Tid);
        let mut encoded = Vec::new();
        rmpv::encode::write_value(&mut encoded, &to_value(extension, 0)?)
            .map_err(|e| PyValueError::new_err(format!("the extension does not encode: {e}")))?;
        let pool = self.pool(py)?;
        let trace = self.traceparent(py, &pool)?;
        let runtime = self.runtime;
        let (user, description) = (user.to_vec(), description.to_vec());
        let txn = py
            .detach(|| {
                let begin = pool.begin_with(&user, &description, &encoded, tid, &status);
                runtime.block_on(traced(trace, begin))
            })
            .map_err(|e| client_error(py, e))?;
        let slot = Arc::new(Mutex::new(Some(txn)));
        {
            let mut open = lock(&self.open);
            open.retain(|slot| slot.strong_count() > 0);
            open.push(Arc::downgrade(&slot));
        }
        Ok(NativeTransaction {
            pid: self.pid,
            runtime,
            txn: slot,
            turn: Mutex::new(None),
            traceparent: self
                .traceparent
                .as_ref()
                .filter(|_| traces(&pool))
                .map(|t| t.clone_ref(py)),
        })
    }

    /// Abort each transaction of `begin` that is still open, and wait for
    /// the replies of the server. Without one, do nothing.
    fn abort(&self, py: Python<'_>) -> PyResult<()> {
        self.check_process()?;
        let pool = lock(&self.pool).clone();
        let trace = match &pool {
            Some(pool) => self.traceparent(py, pool)?,
            None => None,
        };
        let runtime = self.runtime;
        let open: Vec<Arc<Slot>> = lock(&self.open)
            .drain(..)
            .filter_map(|slot| slot.upgrade())
            .collect();
        py.detach(|| {
            let mut result = Ok(());
            for slot in open {
                let txn = lock(&slot).take();
                if let Some(txn) = txn
                    && let Err(error) = runtime.block_on(traced(trace.clone(), txn.abort()))
                {
                    result = Err(error);
                }
            }
            result
        })
        .map_err(|e| client_error(py, e))
    }

    /// Wait for the next delivery of the pool, and return it: `("invalidate",
    /// tid, oids)` for a commit of another client, `("stale",)` when the
    /// client lost its listener, `("connected", verification)` when it
    /// connected again (`"kept"`, `"quick"` or `"dropped"`), `("refused",
    /// text)` when it refused a server whose last tid is below its own, or
    /// `None` after the close. The caller gives it to the DB, then calls
    /// `delivered`. The pool delivers nothing more until then.
    fn next_delivery<'py>(&self, py: Python<'py>) -> PyResult<Option<Bound<'py, PyTuple>>> {
        self.check_process()?;
        let Some(pool) = lock(&self.pool).clone() else {
            return Ok(None);
        };
        let runtime = self.runtime;
        let event = py.detach(move || runtime.block_on(pool.next_event()));
        let Some(event) = event else {
            return Ok(None);
        };
        let word = |text: &str| PyString::new(py, text).into_any();
        let tuple = match event {
            Event::Delivery(Delivery::Invalidate(invalidation)) => {
                let oids: Vec<_> = invalidation.oids.iter().map(|oid| p64(py, oid.0)).collect();
                let items = [
                    PyString::new(py, "invalidate").into_any(),
                    p64(py, invalidation.tid.0).into_any(),
                    PyList::new(py, oids)?.into_any(),
                ];
                PyTuple::new(py, items)?
            }
            Event::Delivery(Delivery::Stale) => PyTuple::new(py, [word("stale")])?,
            Event::Connected(verification) => {
                let verification = match verification {
                    Verification::Kept => "kept",
                    Verification::Quick => "quick",
                    Verification::Dropped => "dropped",
                };
                PyTuple::new(py, [word("connected"), word(verification)])?
            }
            Event::Refused(text) => PyTuple::new(py, [word("refused"), word(&text)])?,
        };
        Ok(Some(tuple))
    }

    /// Confirm the delivery that `next_delivery` gave: the DB got it.
    fn delivered(&self, py: Python<'_>) -> PyResult<()> {
        self.check_process()?;
        let pool = lock(&self.pool).clone();
        if let Some(pool) = pool {
            py.detach(move || pool.delivered());
        }
        Ok(())
    }

    /// Close the connections. A call that waits, and each later call,
    /// raises `ClientDisconnected`. `next_delivery` gives `None`. A second
    /// close does nothing.
    fn close(&self, py: Python<'_>) -> PyResult<()> {
        self.check_process()?;
        let runtime = self.runtime;
        let pool = {
            let mut slot = lock(&self.pool);
            if let Some(pool) = slot.as_ref() {
                let last = pool.last_transaction().0;
                self.closed_at.store(last, AtomicOrdering::SeqCst);
            }
            slot.take()
        };
        let Some(pool) = pool else {
            return Ok(());
        };
        py.detach(move || {
            pool.disconnect();
            let _inside = runtime.enter();
            drop(pool);
        });
        Ok(())
    }
}

impl NativeClient {
    /// Return the pool, or raise when it is closed or when the process is
    /// a child of the process that opened it.
    fn pool(&self, py: Python<'_>) -> PyResult<Arc<Pool>> {
        self.check_process()?;
        let pool = lock(&self.pool).clone();
        pool.ok_or_else(|| errors::client_disconnected(py, "the client is closed".into()))
    }

    /// Return the W3C traceparent of the current span of the calling
    /// thread, or `None`. Without the callable, or when the server of the
    /// listener did not accept `trace_context`, it calls no Python code.
    fn traceparent(&self, py: Python<'_>, pool: &Pool) -> PyResult<Option<String>> {
        let callable = self.traceparent.as_ref().filter(|_| traces(pool));
        traceparent(py, callable)
    }

    /// Return the listener of the pool, if the client is open and has one.
    fn listener(&self) -> Option<Arc<zoe_client::Client>> {
        if self.pid != std::process::id() {
            return None;
        }
        lock(&self.pool).as_ref().and_then(|pool| pool.listener())
    }

    /// Raise `RuntimeError` in a child of `os.fork()`: the threads of the
    /// client are in the parent only.
    fn check_process(&self) -> PyResult<()> {
        check_process(self.pid)
    }
}

impl Drop for NativeClient {
    fn drop(&mut self) {
        let pool = self
            .pool
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if self.pid != std::process::id() {
            // The threads and the locks of the parent are not here. Free
            // nothing of the connections.
            mem::forget(pool);
            return;
        }
        // The drop of a connection can start a task of the runtime.
        let _inside = self.runtime.enter();
        drop(pool);
    }
}

/// The open transaction of a `Transaction`.
type Slot = Mutex<Option<PoolTransaction>>;

/// One transaction of a `Client`, on its own connection of the pool, from
/// `begin` to `finish` and `done`, or to `abort`.
#[pyclass(frozen, module = "zoe._zoe", name = "Transaction")]
pub(crate) struct NativeTransaction {
    /// The process that opened the client.
    pid: u32,
    runtime: &'static Runtime,
    /// The transaction, until its finish, its abort or a failed vote. The
    /// client keeps a weak reference to it for `Client.abort`.
    txn: Arc<Slot>,
    /// The turn of the commit, from the return of `finish` to `done`.
    turn: Mutex<Option<Turn>>,
    /// The callable of the traceparent of the client, or `None`.
    traceparent: Option<Py<PyAny>>,
}

#[pymethods]
impl NativeTransaction {
    fn __traverse__(&self, visit: PyVisit<'_>) -> Result<(), PyTraverseError> {
        if let Some(traceparent) = &self.traceparent {
            visit.call(traceparent)?;
        }
        Ok(())
    }

    /// Store `data` for `oid`. `serial` is the serial that the transaction
    /// read.
    fn store(
        &self,
        py: Python<'_>,
        oid: &Bound<'_, PyAny>,
        serial: &Bound<'_, PyAny>,
        data: &[u8],
    ) -> PyResult<()> {
        let (oid, serial) = (Oid(u64_of(oid)?), Tid(u64_of(serial)?));
        check_process(self.pid)?;
        let trace = traceparent(py, self.traceparent.as_ref())?;
        let runtime = self.runtime;
        py.detach(|| {
            let mut slot = lock(&self.txn);
            let txn = slot.as_mut().ok_or_else(not_open)?;
            runtime.block_on(traced(trace, txn.store(oid, serial, data)))
        })
        .map_err(|e| client_error(py, e))
    }

    /// Store `data` for `oid` with the blob in the file `filename`, and
    /// upload the file in chunks. The file stays in its place. An error of
    /// the file raises `OSError`; the caller then aborts the transaction.
    fn store_blob(
        &self,
        py: Python<'_>,
        oid: &Bound<'_, PyAny>,
        serial: &Bound<'_, PyAny>,
        data: &[u8],
        filename: PathBuf,
    ) -> PyResult<()> {
        let (oid, serial) = (Oid(u64_of(oid)?), Tid(u64_of(serial)?));
        check_process(self.pid)?;
        let trace = traceparent(py, self.traceparent.as_ref())?;
        let runtime = self.runtime;
        py.detach(|| {
            let mut file = File::open(&filename)?;
            let mut slot = lock(&self.txn);
            let txn = slot.as_mut().ok_or_else(not_open)?;
            let store = txn.store_blob_from(oid, serial, data, &mut file);
            runtime.block_on(traced(trace, store))
        })
        .map_err(|e| blob_error(py, e))
    }

    /// Store `data` for `oid` with the blob in the file `filename` of the
    /// directory of `oid` in the blob directory that the server shares. The
    /// server takes the file by rename.
    fn store_blob_shared(
        &self,
        py: Python<'_>,
        oid: &Bound<'_, PyAny>,
        serial: &Bound<'_, PyAny>,
        data: &[u8],
        filename: &str,
    ) -> PyResult<()> {
        let (oid, serial) = (Oid(u64_of(oid)?), Tid(u64_of(serial)?));
        check_process(self.pid)?;
        let trace = traceparent(py, self.traceparent.as_ref())?;
        let runtime = self.runtime;
        py.detach(|| {
            let mut slot = lock(&self.txn);
            let txn = slot.as_mut().ok_or_else(not_open)?;
            let store = txn.store_blob_shared(oid, serial, data, filename);
            runtime.block_on(traced(trace, store))
        })
        .map_err(|e| client_error(py, e))
    }

    /// Delete `oid`. `serial` is the serial that the transaction read: the
    /// vote checks it as for a store.
    fn delete_object(
        &self,
        py: Python<'_>,
        oid: &Bound<'_, PyAny>,
        serial: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        let (oid, serial) = (Oid(u64_of(oid)?), Tid(u64_of(serial)?));
        check_process(self.pid)?;
        let trace = traceparent(py, self.traceparent.as_ref())?;
        let runtime = self.runtime;
        py.detach(|| {
            let mut slot = lock(&self.txn);
            let txn = slot.as_mut().ok_or_else(not_open)?;
            runtime.block_on(traced(trace, txn.delete_object(oid, serial)))
        })
        .map_err(|e| client_error(py, e))
    }

    /// Copy a record of another storage into this transaction, which began
    /// with a tid. `None` as `data` copies a deletion. `prev_txn` is a tid
    /// or `None`.
    fn restore(
        &self,
        py: Python<'_>,
        oid: &Bound<'_, PyAny>,
        serial: &Bound<'_, PyAny>,
        data: Option<&[u8]>,
        prev_txn: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<()> {
        let (oid, serial) = (Oid(u64_of(oid)?), Tid(u64_of(serial)?));
        let prev_txn = prev_txn.map(u64_of).transpose()?.map(Tid);
        check_process(self.pid)?;
        let trace = traceparent(py, self.traceparent.as_ref())?;
        let runtime = self.runtime;
        py.detach(|| {
            let mut slot = lock(&self.txn);
            let txn = slot.as_mut().ok_or_else(not_open)?;
            runtime.block_on(traced(trace, txn.restore(oid, serial, data, prev_txn)))
        })
        .map_err(|e| client_error(py, e))
    }

    /// Undo the transaction `trans_id` in this transaction: `undoa`, sent to
    /// the server, which answers as it does for `ZEO.ClientStorage`.
    fn undo(&self, py: Python<'_>, trans_id: &[u8]) -> PyResult<()> {
        check_process(self.pid)?;
        let trace = traceparent(py, self.traceparent.as_ref())?;
        let runtime = self.runtime;
        py.detach(|| {
            let mut slot = lock(&self.txn);
            let txn = slot.as_mut().ok_or_else(not_open)?;
            runtime.block_on(traced(trace, txn.undo(trans_id)))
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
        check_process(self.pid)?;
        let trace = traceparent(py, self.traceparent.as_ref())?;
        let runtime = self.runtime;
        py.detach(|| {
            let mut slot = lock(&self.txn);
            let txn = slot.as_mut().ok_or_else(not_open)?;
            runtime.block_on(traced(trace, txn.check_current_serial(oid, serial)))
        })
        .map_err(|e| client_error(py, e))
    }

    /// Vote, resolve the conflicts that the vote lists, and return the oids
    /// that the server or the client resolved, as `tpc_vote` of
    /// `ZEO.ClientStorage` does. For each listed conflict, it calls
    /// `resolve(oid, committed, read, data)` on this thread, with no lock
    /// held, stores the result over `committed` and votes again, for at
    /// most `ROUNDS` rounds. A vote that fails ends the transaction: the
    /// server aborted it. An error of `resolve` or of the store of its
    /// result, or a conflict still listed after the last round, drops the
    /// cache entry of the oid, raises, and leaves the transaction open for
    /// its abort.
    fn vote<'py>(
        &self,
        py: Python<'py>,
        resolve: &Bound<'py, PyAny>,
    ) -> PyResult<Vec<Bound<'py, PyBytes>>> {
        check_process(self.pid)?;
        let runtime = self.runtime;
        let mut resolved: Vec<Oid> = Vec::new();
        for round in 0..=ROUNDS {
            let trace = traceparent(py, self.traceparent.as_ref())?;
            let voted = py
                .detach(|| {
                    let mut slot = lock(&self.txn);
                    let txn = slot.as_mut().ok_or_else(not_open)?;
                    let voted = runtime.block_on(traced(trace, txn.vote_listed()));
                    if voted.is_err() {
                        let ended = slot.take();
                        let _inside = runtime.enter();
                        drop(ended);
                    }
                    voted
                })
                .map_err(|e| client_error(py, e))?;
            for oid in voted.resolved {
                if !resolved.contains(&oid) {
                    resolved.push(oid);
                }
            }
            if voted.conflicts.is_empty() {
                return Ok(resolved.into_iter().map(|oid| p64(py, oid.0)).collect());
            }
            for listed in voted.conflicts {
                let data = if round < ROUNDS {
                    let args = (
                        p64(py, listed.oid.0),
                        p64(py, listed.committed.0),
                        p64(py, listed.read.0),
                        PyBytes::new(py, &listed.data),
                    );
                    resolve
                        .call1(args)
                        .and_then(|data| Ok(data.cast_into::<PyBytes>()?))
                } else {
                    let serials = Some((listed.committed, listed.read));
                    let class = errors::conflict_class(py);
                    let data = Some(listed.data.as_slice());
                    Err(errors::conflict(py, class, listed.oid, serials, data))
                };
                let data = match data {
                    Ok(data) => data,
                    Err(error) => {
                        self.unresolved(py, listed.oid);
                        return Err(error);
                    }
                };
                let trace = traceparent(py, self.traceparent.as_ref())?;
                let bytes = data.as_bytes();
                let stored = py.detach(|| {
                    let mut slot = lock(&self.txn);
                    let txn = slot.as_mut().ok_or_else(not_open)?;
                    let store = txn.store_resolved(listed.oid, listed.committed, bytes);
                    runtime.block_on(traced(trace, store))
                });
                if let Err(error) = stored {
                    self.unresolved(py, listed.oid);
                    return Err(client_error(py, error));
                }
                if !resolved.contains(&listed.oid) {
                    resolved.push(listed.oid);
                }
            }
        }
        unreachable!("the last round returns or raises")
    }

    /// Finish the transaction after its vote, and return its tid. It
    /// returns when the pool delivered every tid below the tid. The caller
    /// then runs the callback of `tpc_finish` and calls `done`: the pool
    /// delivers no later tid until then.
    fn finish<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyBytes>> {
        check_process(self.pid)?;
        let trace = traceparent(py, self.traceparent.as_ref())?;
        let runtime = self.runtime;
        let tid = py
            .detach(|| {
                let txn = lock(&self.txn).take().ok_or_else(not_open)?;
                let finish = async move { txn.finish().await?.turn().await };
                let turn = runtime.block_on(traced(trace, finish))?;
                let tid = turn.tid();
                *lock(&self.turn) = Some(turn);
                Ok(tid)
            })
            .map_err(|e| client_error(py, e))?;
        Ok(p64(py, tid.0))
    }

    /// End the turn of the commit: the callback of `tpc_finish` ran. The
    /// pool then delivers the tid and goes on. Without a turn, do nothing.
    fn done(&self, py: Python<'_>) -> PyResult<()> {
        check_process(self.pid)?;
        py.detach(|| drop(lock(&self.turn).take()));
        Ok(())
    }

    /// Abort the transaction, and wait for the reply of the server. After
    /// its end, do nothing.
    fn abort(&self, py: Python<'_>) -> PyResult<()> {
        check_process(self.pid)?;
        let trace = traceparent(py, self.traceparent.as_ref())?;
        let runtime = self.runtime;
        py.detach(|| {
            let txn = lock(&self.txn).take();
            match txn {
                Some(txn) => runtime.block_on(traced(trace, txn.abort())),
                None => Ok(()),
            }
        })
        .map_err(|e| client_error(py, e))
    }
}

impl NativeTransaction {
    /// Drop the current cache entry of `oid`, whose listed conflict the
    /// client did not resolve. After the end of the transaction, do
    /// nothing.
    fn unresolved(&self, py: Python<'_>, oid: Oid) {
        py.detach(|| {
            if let Some(txn) = lock(&self.txn).as_ref() {
                txn.unresolved(oid);
            }
        });
    }
}

impl Drop for NativeTransaction {
    fn drop(&mut self) {
        let txn = lock(&self.txn).take();
        let turn = self
            .turn
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if self.pid != std::process::id() {
            mem::forget((txn, turn));
            return;
        }
        // The drop of a transaction can start a task of the runtime.
        let _inside = self.runtime.enter();
        drop(txn);
        drop(turn);
    }
}

/// The `out` of a lookup of the cache: a `bytes` of `len` bytes that `fill`
/// fills and checks, or `None` when the check fails.
fn bytes<'py>(
    py: Python<'py>,
    len: usize,
    fill: &mut dyn FnMut(&mut [u8]) -> bool,
) -> Option<Bound<'py, PyBytes>> {
    PyBytes::new_with(py, len, |buf| {
        if fill(buf) {
            Ok(())
        } else {
            Err(PyValueError::new_err("the entry changed during the copy"))
        }
    })
    .ok()
}

/// Tell if the server of the listener of `pool` accepted `trace_context`.
fn traces(pool: &Pool) -> bool {
    pool.listener().is_some_and(|listener| {
        listener
            .extensions()
            .iter()
            .any(|name| name == TRACE_CONTEXT)
    })
}

/// Return the W3C traceparent of `traceparent()`, or `None` without the
/// callable. Without the callable, it calls no Python code.
fn traceparent(py: Python<'_>, traceparent: Option<&Py<PyAny>>) -> PyResult<Option<String>> {
    match traceparent {
        Some(traceparent) => Ok(traceparent.bind(py).call0()?.extract()?),
        None => Ok(None),
    }
}

/// Raise `RuntimeError` in a child of `os.fork()` of the process `pid`.
fn check_process(pid: u32) -> PyResult<()> {
    if pid != std::process::id() {
        return Err(PyRuntimeError::new_err(
            "the client was opened in another process, before a fork",
        ));
    }
    Ok(())
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

impl Opening {
    /// Return the error of the client for an attempt to connect again.
    fn into_error(self) -> Error {
        match self {
            Opening::Address(text) | Opening::Tls(text) => Error::Invalid(text),
            Opening::Runtime(error) => Error::Io(error),
            Opening::Client(error) => error,
        }
    }
}

/// What a client connects to: its addresses, its TLS, its storage, the
/// options of its `register` and the longest wait of one open. Each open
/// reads the addresses when it starts.
struct Servers {
    targets: Mutex<Vec<Target>>,
    secure: Option<Arc<Secure>>,
    storage: String,
    options: Options,
    wait: Duration,
}

impl Servers {
    /// Open a listener as `options` asks, and return it with the `Connect`
    /// of its server. With `retry`, try again after failed connections
    /// until the wait passes. Else make one round.
    async fn open(
        &self,
        options: &Options,
        retry: bool,
    ) -> Result<(zoe_client::Client, Connect), Opening> {
        let targets = lock(&self.targets).clone();
        let (listener, endpoint) = open(
            &targets,
            self.secure.as_deref(),
            &self.storage,
            options,
            self.wait,
            retry,
        )
        .await?;
        // The other connections register as the listener did, on its
        // server, without a fallback.
        let options = Options {
            read_only: listener.read_only(),
            read_only_fallback: false,
            ..options.clone()
        };
        let storage = self.storage.clone();
        let connect: Connect = Arc::new(move || {
            let (endpoint, storage, options) = (endpoint.clone(), storage.clone(), options.clone());
            Box::pin(async move {
                zoe_client::Client::open_endpoint(Arc::new(Tokio), &endpoint, &storage, &options)
                    .await
            })
        });
        Ok((listener, connect))
    }

    /// Open a new listener for the pool, in one round. With `writable`, open
    /// only a writable one.
    async fn reopen(&self, writable: bool) -> Result<(zoe_client::Client, Connect), Error> {
        let options = match writable {
            true => Options {
                read_only: false,
                read_only_fallback: false,
                ..self.options.clone()
            },
            false => self.options.clone(),
        };
        self.open(&options, false)
            .await
            .map_err(Opening::into_error)
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

/// Resolve the host of each TCP address of `targets`, as `resolve` does,
/// on the blocking threads of the runtime. A lookup of the system can wait
/// for a long time: on a worker thread it would stop the connections of
/// every client of the process.
async fn resolve_hosts(targets: &[Target]) -> HashMap<(String, u16), Result<SocketAddr, String>> {
    let mut resolved = HashMap::new();
    for target in targets {
        let Target::Tcp(host, port) = target else {
            continue;
        };
        let key = (host.clone(), *port);
        if resolved.contains_key(&key) {
            continue;
        }
        let (name, port) = key.clone();
        let lookup = tokio::task::spawn_blocking(move || resolve(&name, port)).await;
        let addr = lookup.unwrap_or_else(|e| Err(format!("the address {host}:{port}: {e}")));
        resolved.insert(key, addr);
    }
    resolved
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
/// runtime of the caller, and return it with its endpoint. Each round
/// resolves the hosts again. With `retry`, try again after failed
/// connections until `wait` passes.
async fn open(
    targets: &[Target],
    secure: Option<&Secure>,
    storage: &str,
    options: &Options,
    wait: Duration,
    retry: bool,
) -> Result<(zoe_client::Client, Endpoint), Opening> {
    let deadline = Instant::now() + wait;
    loop {
        let resolved = resolve_hosts(targets).await;
        let round = endpoints(targets, secure, |host, port| {
            resolved
                .get(&(host.to_owned(), port))
                .cloned()
                .unwrap_or_else(|| Err(format!("the address {host}:{port} was not resolved")))
        })?;
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
            Err(Error::Io(_) | Error::Disconnected)
                if retry && Instant::now() + RETRY < deadline =>
            {
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
            opened => {
                let client = opened?;
                let endpoint = round
                    .endpoints
                    .iter()
                    .find(|endpoint| endpoint.address() == client.address())
                    .cloned()
                    .ok_or_else(|| Error::Protocol("the server has no endpoint".into()))?;
                return Ok((client, endpoint));
            }
        }
    }
}

/// Return the name of the partial file of a download: `<filename>.dl`, as
/// `ZEO.ClientStorage` names it.
fn partial_name(filename: &Path) -> PathBuf {
    let mut partial = filename.as_os_str().to_owned();
    partial.push(".dl");
    PathBuf::from(partial)
}

/// Write the bytes of `fetch` into a new file at `partial`, and return
/// their number. The file must hold that many bytes after the write. Then
/// make it read-only. A file at `partial` that a stopped download left is
/// removed first.
fn download(
    partial: &Path,
    fetch: impl FnOnce(&mut BufWriter<File>) -> Result<u64, Error>,
) -> Result<u64, Error> {
    // A crash can leave a read-only partial file, which `File::create` does
    // not open.
    match fs::remove_file(partial) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error.into()),
        _ => {}
    }
    let mut sink = BufWriter::new(File::create(partial)?);
    let size = fetch(&mut sink)?;
    sink.flush()?;
    let file = sink.into_inner().map_err(|e| e.into_error())?;
    let written = file.metadata()?.len();
    if written != size {
        return Err(Error::Io(io::Error::other(format!(
            "the download wrote {written} bytes of {size} into {}",
            partial.display()
        ))));
    }
    file.set_permissions(fs::Permissions::from_mode(BLOB_MODE))?;
    Ok(size)
}

/// Return the exception of Python for an error of a blob call. An error of
/// a file there (`Error::Io`) raises `OSError`. Another error raises as
/// `client_error` gives it.
fn blob_error(py: Python<'_>, error: Error) -> PyErr {
    match error {
        Error::Io(error) => error.into(),
        error => client_error(py, error),
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
        Error::Server { class, message, .. } => server_error(py, &class, message),
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
fn zeo_error(py: Python<'_>, error: Error) -> PyErr {
    let Error::Server { class, value, message } = error else {
        return client_error(py, error);
    };
    let made = (|| -> PyResult<PyErr> {
        let value = to_python(py, &value, 0)?;
        let factories = py.import("ZEO.asyncio.client")?.getattr("exc_factories")?;
        let exception = match factories.get_item(&class) {
            Ok(factory) => factory.call1((&class, value))?,
            Err(_) => py
                .import("ZEO.Exceptions")?
                .getattr("ServerException")?
                .call1((&class, value))?,
        };
        Ok(PyErr::from_value(exception))
    })();
    made.unwrap_or_else(|_| server_error(py, &class, message))
}

/// Return the msgpack values of the args of a call: a list or a tuple.
fn args_of(args: &Bound<'_, PyAny>) -> PyResult<Vec<Value>> {
    match to_value(args, 0)? {
        Value::Array(items) => Ok(items),
        _ => Err(PyTypeError::new_err("the args of a call are a list or a tuple")),
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
    fn a_host_name_resolves_on_a_blocking_thread() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let targets = [
            Target::Tcp("localhost".into(), 8100),
            Target::Unix(PathBuf::from("./zeo.sock")),
        ];
        let resolved = runtime.block_on(resolve_hosts(&targets));
        assert_eq!(resolved.len(), 1);
        let addr = resolved[&("localhost".to_owned(), 8100)].clone().unwrap();
        assert!(addr.ip().is_loopback(), "{addr}");
        assert_eq!(addr.port(), 8100);
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
