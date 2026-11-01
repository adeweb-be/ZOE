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
//! With a `cache_size`, the pool holds a client cache (ADR 0034): in
//! memory, or in the file `cache_path`, which survives the process. A hit
//! runs on the calling thread: it takes no lock, sends nothing and does not
//! release the GIL. A miss loads from the server and stores the reply.
//!
//! The pool delivers the tids of the listener in one order. A thread of
//! `zoe.zeo` takes each commit of another client, and each change of the
//! connection, with `next_delivery`, and confirms it with `delivered`.
//! `Transaction.finish` returns the tid of a commit at its turn, and
//! `Transaction.done` ends the turn after the callback of `tpc_finish`.
//!
//! `call` and `call_async` send the other calls of `zoe.zeo`, its `_call`
//! and `_async`, on the listener, and raise for an error reply what
//! `ZEO.ClientStorage` raises. `prefetch` starts loads that fill the cache
//! and does not wait for them. `sync` pings the listener.
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
//!
//! `open` holds the open of a client on its addresses and its TLS, and the
//! reconnect. `transaction` holds `zoe._zoe.Transaction`. `convert` holds
//! the conversions between the values of Python and of msgpack, and the
//! exceptions of the errors of the client.

mod convert;
mod open;
mod transaction;

use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::mem;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Duration;

use pyo3::PyTraverseError;
use pyo3::exceptions::{PyMemoryError, PyRuntimeError, PyValueError};
use pyo3::gc::PyVisit;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyList, PyString, PyTuple};
use tokio::runtime::Runtime;
use zoe_client::{
    Address, Cache, Delivery, Error, Event, FileCheck, Open, Options, Pool, PoolIntents, Reconnect,
    Tokio, Verification, lock, traced,
};
use zoe_core::{Oid, Tid};
use zoe_wire::extension::TRACE_CONTEXT;

use crate::errors::{self, p64, u64_of};
use crate::kv;
use convert::{args_of, blob_error, bytes, client_error, to_python, to_value, zeo_error};
pub(crate) use open::OtherProtocol;
use open::{Opening, Secure, Servers, Target, TlsArgs, dropping_of, endpoints};
pub(crate) use transaction::NativeTransaction;
use transaction::Slot;

/// The number of worker threads of the runtime of the native clients.
const WORKERS: usize = 2;

/// The seconds between two attempts to connect again, when the open gives
/// no `disconnect_poll`: the `connect_poll` of `ZEO.ClientStorage`.
const DISCONNECT_POLL: f64 = 1.0;

/// The most connections of a client that commit at once, when the open
/// gives no `pool_bound`. `zoe.zeo._POOL_BOUND` has the same value.
const POOL_BOUND: usize = 64;

/// The mode of a blob file of the cache: read-only for its owner, as
/// `ZEO.ClientStorage` sets it (`stat.S_IREAD`).
const BLOB_MODE: u32 = 0o400;
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
    /// With `cache_path`, the cache is that file, which the open creates
    /// when it does not exist (ADR 0034, stage 2). The open takes the lock
    /// of the file before it connects: a file that another open holds
    /// raises `zc.lockfile.LockError`, as the cache of ZEO does. A name of
    /// the storage longer than 256 bytes raises `ValueError`, and an error
    /// of the file raises `OSError`. A file with entries serves only after
    /// its verification against the server: `ready` waits for it.
    ///
    /// With `dropping`, a verification that drops every entry of the cache
    /// calls `dropping(reason)` before the drop, on a thread of its own,
    /// and waits for it: `reason` is empty when the server does not keep
    /// the commits after the cache, else the error of `getInvalidations`.
    ///
    /// An address is a `(host, port)` tuple or the path of a unix domain
    /// socket as a `str`. The client connects to all of them and keeps one
    /// server: with `read_only_fallback`, a writable server before a
    /// read-only one.
    ///
    /// `tls` is `None`, or the tuple `(certificate, key, authenticate,
    /// check_hostname, server_hostname)` of the files and checks of TLS.
    /// `certificate` is `None` for a client without a certificate: the
    /// client then ignores `key`. `key` is `None` when `certificate` holds
    /// the key. `authenticate` is a
    /// file or a directory of CA certificates, or `None` for the CA
    /// certificates of the system. The server name is
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
    ///
    /// `fall_back_on` lists protocols, as bytes. When the open with `wait`
    /// finds a server of one of these protocols, it stops at once and
    /// raises `OtherProtocol` with the protocol: it does not log and does
    /// not wait. The caller can then open another client. A reconnect
    /// never does this.
    ///
    /// With `prefetch_references` and a cache, each load that the server
    /// serves also prefetches the objects that its record refers to
    /// (`Pool::set_prefetch_references`).
    #[new]
    #[pyo3(signature = (
        addresses, storage, *, read_only, read_only_fallback, extensions, wait_timeout,
        pool_bound=POOL_BOUND, cache_size=0, tls=None, traceparent=None, wait=true,
        disconnect_poll=DISCONNECT_POLL, client_label=None, cache_path=None, dropping=None,
        fall_back_on=Vec::new(), prefetch_references=false
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
        cache_path: Option<PathBuf>,
        dropping: Option<Py<PyAny>>,
        fall_back_on: Vec<Vec<u8>>,
        prefetch_references: bool,
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
        let cache = match (cache_size, cache_path) {
            (0, _) => None,
            (size, None) => Some(Cache::new(size).map_err(|e| {
                let text = format!("cache_size {size}: {e}");
                // A size that the cache refuses is a bad value. Any other
                // error is a mapping that the kernel did not give.
                match e.kind() {
                    std::io::ErrorKind::InvalidInput => PyValueError::new_err(text),
                    _ => PyMemoryError::new_err(text),
                }
            })?),
            (size, Some(path)) => {
                let name = storage.as_bytes();
                let opened = py.detach(|| Cache::open(&path, size, name));
                Some(opened.map_err(|e| match e.kind() {
                    io::ErrorKind::WouldBlock => errors::lock_failure(py, &path),
                    io::ErrorKind::InvalidData => PyValueError::new_err(e.to_string()),
                    io::ErrorKind::InvalidInput => {
                        PyValueError::new_err(format!("cache_size {size}: {e}"))
                    }
                    _ => e.into(),
                })?)
            }
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
                true => {
                    Some(runtime.block_on(servers.open(&servers.options, true, &fall_back_on))?)
                }
                false => None,
            };
            let reopen = servers.clone();
            let open: Open = Arc::new(move |writable| {
                let servers = reopen.clone();
                Box::pin(async move { servers.reopen(writable).await })
            });
            let reconnect = Reconnect {
                upgrade,
                dropping: dropping.map(dropping_of),
                ..Reconnect::new(open, poll)
            };
            // The pool starts its tasks on the runtime.
            let _inside = runtime.enter();
            let pool = Pool::with_reconnect(Arc::new(Tokio), first, pool_bound, cache, reconnect);
            pool.set_prefetch_references(prefetch_references);
            Ok((runtime, pool, servers))
        });
        let (runtime, pool, servers) = match opened {
            Ok(opened) => opened,
            Err(Opening::Address(text) | Opening::Tls(text)) => {
                return Err(PyValueError::new_err(text));
            }
            Err(Opening::Runtime(error)) => return Err(error.into()),
            Err(Opening::Client(Error::OtherProtocol(theirs))) => {
                return Err(OtherProtocol::new_err(PyBytes::new(py, &theirs).unbind()));
            }
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
        let reply = self.run_zeo(py, async move |pool| pool.call(&method, args).await)?;
        to_python(py, &reply, 0)
    }

    /// Call `method` as `call` does, on the listener of `generation`, and
    /// return `(reply, generation)`: the generation of the listener that
    /// answered. `None` takes the listener of now. The generation changes
    /// at each loss of the listener. When the listener of `generation` is
    /// gone, the call sends nothing and raises the `KeyError` of an id
    /// that the server does not know, with the first arg.
    #[pyo3(signature = (method, args, generation=None))]
    fn call_on<'py>(
        &self,
        py: Python<'py>,
        method: String,
        args: &Bound<'py, PyAny>,
        generation: Option<u64>,
    ) -> PyResult<(Bound<'py, PyAny>, u64)> {
        let args = args_of(args)?;
        let (reply, generation) = self.run_zeo(py, async move |pool| {
            pool.call_on(generation, &method, args).await
        })?;
        Ok((to_python(py, &reply, 0)?, generation))
    }

    /// Send `method` with `args` on the listener as a call without a reply,
    /// as `_async` of `ZEO.ClientStorage` does. The rules of `call` apply.
    /// With `generation`, send it only on the listener of that generation
    /// (see `call_on`), else send nothing.
    #[pyo3(signature = (method, args, generation=None))]
    fn call_async(
        &self,
        py: Python<'_>,
        method: String,
        args: &Bound<'_, PyAny>,
        generation: Option<u64>,
    ) -> PyResult<()> {
        let args = args_of(args)?;
        self.run(py, async move |pool| {
            pool.notify_on(generation, &method, args).await
        })
    }

    /// Load each of `oids` before `tid` that the cache does not hold, all
    /// at once, and return the number of loads. It does not wait for the
    /// replies: they fill the cache. Without a cache, it loads nothing.
    /// Without a server, it raises `ClientDisconnected`.
    fn prefetch(
        &self,
        py: Python<'_>,
        oids: Vec<Bound<'_, PyAny>>,
        tid: &Bound<'_, PyAny>,
    ) -> PyResult<usize> {
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
        self.run(py, async |pool| pool.sync(wait).await)
    }

    /// Return the extensions that the server of the listener accepted.
    fn extensions(&self) -> PyResult<Vec<String>> {
        self.check_process()?;
        Ok(self
            .listener()
            .map(|listener| listener.extensions().to_vec())
            .unwrap_or_default())
    }

    /// Return how the verification of the cache file ended, once: `None`
    /// before its end, after the first call, and without a file with
    /// entries. Else a tuple `(kind, text)`: `kind` is `"kept"`, `"quick"`,
    /// `"dropped"`, `"failed"`, `"other"` or `"ahead"`, and `text` says more
    /// for the last three.
    fn take_cache_check(&self) -> Option<(&'static str, String)> {
        let check = lock(&self.pool).as_ref()?.take_file_check()?;
        Some(match check {
            FileCheck::Kept => ("kept", String::new()),
            FileCheck::Quick => ("quick", String::new()),
            FileCheck::Dropped => ("dropped", String::new()),
            FileCheck::Failed(text) => ("failed", text),
            FileCheck::Other { applied } => (
                "other",
                format!(
                    "the server holds no transaction {:016x}, the last transaction \
                     of the cache file: its history is another one",
                    applied.0
                ),
            ),
            FileCheck::Ahead { applied, server } => (
                "ahead",
                format!(
                    "the last transaction {:016x} of the server is before the last \
                     transaction {:016x} of the cache file",
                    server.0, applied.0
                ),
            ),
        })
    }

    /// Tell if the client is connected: not closed, and its listener is not
    /// lost.
    fn is_connected(&self) -> bool {
        self.pid == std::process::id()
            && lock(&self.pool)
                .as_ref()
                .is_some_and(|pool| pool.is_connected())
    }

    /// Wait at most `timeout` seconds until the cache serves: a cache file
    /// with entries serves after its verification against the server.
    /// The wait also ends without a cache, at the close and when the cache
    /// is disabled (`Pool::ready`). Return false when `timeout` passed
    /// first.
    fn ready(&self, py: Python<'_>, timeout: f64) -> PyResult<bool> {
        let timeout = Duration::try_from_secs_f64(timeout.max(0.0))
            .map_err(|_| PyValueError::new_err("timeout is not a duration"))?;
        let pool = self.pool(py)?;
        let runtime = self.runtime;
        // The timer of the timeout needs the runtime: make it inside.
        let waited = async move { tokio::time::timeout(timeout, pool.ready()).await };
        Ok(py.detach(move || runtime.block_on(waited).is_ok()))
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
        let oids = self.run(py, async |pool| pool.new_oids(n).await)?;
        Ok(oids.into_iter().map(|oid| p64(py, oid.0)).collect())
    }

    /// Return the record of `oid` that was current before `tid`: its
    /// pickle, its serial and the serial of the next record or `None`.
    /// Return `None` when `oid` has no record before `tid`. A hit of the
    /// cache sends nothing to the server. A miss stores the reply in the
    /// cache. An error of the server raises what ZEO raises for it, as for
    /// `call`.
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
        let (record, _position) =
            self.run_zeo(py, async |pool| pool.load_before(oid, tid).await)?;
        Ok(record.map(|record| {
            let data = PyBytes::new(py, &record.data);
            // The buffer goes back to the client for the next long reply.
            zoe_client::recycle(record.data);
            (
                data,
                p64(py, record.tid.0),
                record.end.map(|end| p64(py, end.0)),
            )
        }))
    }

    /// Return the current record of `oid`: its pickle and its serial, or
    /// `None` when `oid` has no record. A hit of the current entry of the
    /// cache sends nothing to the server, as `load` of `ZEO.ClientStorage`
    /// does. A miss stores the reply in the cache. An error of the server
    /// raises what ZEO raises for it, as for `call`.
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
        let (record, _position) = self.run_zeo(py, async |pool| {
            pool.load_before(oid, Tid(zoe_wire::MAX_TID)).await
        })?;
        Ok(record.map(|record| {
            let data = PyBytes::new(py, &record.data);
            zoe_client::recycle(record.data);
            (data, p64(py, record.tid.0))
        }))
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

    /// Return the entries of the cache as a list of `(oid, start, end)`,
    /// with `None` as the end of a current entry. Without a cache, return
    /// an empty list.
    fn cache_entries<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyList>> {
        self.check_process()?;
        let entries = match &self.cache {
            Some(cache) => py.detach(|| cache.entries()),
            None => Vec::new(),
        };
        let items = entries.into_iter().map(|(oid, start, end)| {
            let end = end.map(|end| p64(py, end.0));
            (p64(py, oid.0), p64(py, start.0), end)
        });
        PyList::new(py, items)
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
        let data = self.run_zeo(py, async |pool| pool.load_serial(oid, serial).await)?;
        let bytes = PyBytes::new(py, &data);
        zoe_client::recycle(data);
        Ok(bytes)
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
        let (pool, trace) = self.start(py)?;
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
        self.run(py, async move |pool| pool.sequence_next(&name).await)
    }

    /// Return the value of `key` in the namespace `name` of `flavour`, or
    /// `None`. Raise `Unsupported` when the server did not accept `kv`.
    fn kv_get<'py>(
        &self,
        py: Python<'py>,
        flavour: &str,
        name: String,
        key: Vec<u8>,
    ) -> PyResult<Option<Bound<'py, PyBytes>>> {
        let flavour = kv::flavour(flavour)?;
        let value = self.run(py, async move |pool| {
            pool.kv_get(flavour, &name, &key).await
        })?;
        Ok(value.map(|value| PyBytes::new(py, &value)))
    }

    /// Return the pairs of the namespace `name` of `flavour`, as
    /// `zoe.Storage.kv_scan` does. Raise `Unsupported` when the server did
    /// not accept `kv`.
    #[pyo3(signature = (flavour, name, start = None, stop = None, limit = 1000))]
    fn kv_scan<'py>(
        &self,
        py: Python<'py>,
        flavour: &str,
        name: String,
        start: Option<Vec<u8>>,
        stop: Option<Vec<u8>>,
        limit: i64,
    ) -> PyResult<Vec<(Bound<'py, PyBytes>, Bound<'py, PyBytes>)>> {
        let flavour = kv::flavour(flavour)?;
        let limit = u64::try_from(limit)
            .map_err(|_| PyValueError::new_err("the limit of a scan is less than 1"))?;
        let pairs = self.run(py, async move |pool| {
            let (start, stop) = (start.as_deref(), stop.as_deref());
            pool.kv_scan(flavour, &name, start, stop, limit).await
        })?;
        Ok(pairs
            .iter()
            .map(|(key, value)| (PyBytes::new(py, key), PyBytes::new(py, value)))
            .collect())
    }

    /// Return the generation of the durable namespace `name`, as 8 bytes.
    /// Raise `Unsupported` when the server did not accept `kv`.
    fn kv_generation<'py>(&self, py: Python<'py>, name: String) -> PyResult<Bound<'py, PyBytes>> {
        let tid = self.run(py, async move |pool| pool.kv_generation(&name).await)?;
        Ok(p64(py, tid.0))
    }

    /// Return a new owner of early write-intent locks (ADR 0045). It
    /// holds no lock and no connection.
    fn intents(&self) -> NativeIntents {
        NativeIntents {
            pid: self.pid,
            runtime: self.runtime,
            intents: tokio::sync::Mutex::new(None),
        }
    }

    /// Take the early write-intent lock of each oid of `oids`, in ascending
    /// order, for `owner`, and return the current serial of each oid. The
    /// first lock of an owner leases a connection of the pool, which the
    /// owner keeps until `unlock_intent` or the transaction that adopts
    /// its locks. An oid at or below an oid that the owner holds raises
    /// `StorageTransactionError`, and a wait past the lock timeout of the
    /// server raises `ConflictError`.
    fn lock_intent<'py>(
        &self,
        py: Python<'py>,
        owner: &NativeIntents,
        oids: Vec<Bound<'py, PyAny>>,
    ) -> PyResult<Vec<Bound<'py, PyBytes>>> {
        self.check_process()?;
        let oids = oids
            .iter()
            .map(|oid| u64_of(oid).map(Oid))
            .collect::<PyResult<Vec<Oid>>>()?;
        let serials = self.run(py, async |pool| {
            let mut intents = owner.intents.lock().await;
            if intents.is_none() {
                *intents = Some(pool.intents().await?);
            }
            let held = intents.as_mut().expect("an owner");
            let locked = held.lock(&oids).await;
            // An owner that holds nothing, or whose connection is lost,
            // gives its connection back to the pool.
            if locked.is_err() && (!held.holds() || !held.is_connected()) {
                *intents = None;
            }
            locked
        })?;
        Ok(serials.into_iter().map(|tid| p64(py, tid.0)).collect())
    }

    /// Release every intent lock of `owner`, and give its connection back
    /// to the pool. Without locks, do nothing.
    fn unlock_intent(&self, py: Python<'_>, owner: &NativeIntents) -> PyResult<()> {
        self.check_process()?;
        let runtime = self.runtime;
        py.detach(|| {
            runtime.block_on(async {
                let intents = owner.intents.lock().await.take();
                match intents {
                    Some(intents) => intents.release().await,
                    None => Ok(()),
                }
            })
        })
        .map_err(|e| client_error(py, e))
    }

    /// Begin a transaction on a free connection of the pool, and return
    /// it. `extension` is the dict of the transaction extension. With
    /// `tid`, the transaction commits under that tid, with the status
    /// `status`. When every connection is busy and the pool is at its
    /// bound, wait for a free one. With `intents`, an owner that holds
    /// early write-intent locks, the transaction begins on the connection
    /// of the owner and adopts its locks.
    #[pyo3(signature = (user, description, extension, tid = None, status = " ".to_owned(), intents = None))]
    #[allow(clippy::too_many_arguments)]
    fn begin(
        &self,
        py: Python<'_>,
        user: &[u8],
        description: &[u8],
        extension: &Bound<'_, PyAny>,
        tid: Option<&Bound<'_, PyAny>>,
        status: String,
        intents: Option<&NativeIntents>,
    ) -> PyResult<NativeTransaction> {
        let tid = tid.map(u64_of).transpose()?.map(Tid);
        let mut encoded = Vec::new();
        rmpv::encode::write_value(&mut encoded, &to_value(extension, 0)?)
            .map_err(|e| PyValueError::new_err(format!("the extension does not encode: {e}")))?;
        let (user, description) = (user.to_vec(), description.to_vec());
        let (txn, tracing) = self.run(py, async |pool| {
            // An owner whose lock waits on another thread has no locks to
            // give yet: the transaction begins without them.
            let held = intents
                .and_then(|owner| owner.intents.try_lock().ok())
                .and_then(|mut held| held.take());
            let txn = match held {
                Some(held) => {
                    held.begin_with(&user, &description, &encoded, tid, &status)
                        .await?
                }
                None => {
                    pool.begin_with(&user, &description, &encoded, tid, &status)
                        .await?
                }
            };
            Ok((txn, traces(pool)))
        })?;
        let slot = Arc::new(Mutex::new(Some(txn)));
        {
            let mut open = lock(&self.open);
            open.retain(|slot| slot.strong_count() > 0);
            open.push(Arc::downgrade(&slot));
        }
        Ok(NativeTransaction {
            pid: self.pid,
            runtime: self.runtime,
            txn: slot,
            turn: Mutex::new(None),
            traceparent: self
                .traceparent
                .as_ref()
                .filter(|_| tracing)
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

    /// Return the pool and the W3C traceparent of the calling thread, for a
    /// call to the server.
    fn start(&self, py: Python<'_>) -> PyResult<(Arc<Pool>, Option<String>)> {
        let pool = self.pool(py)?;
        let trace = self.traceparent(py, &pool)?;
        Ok((pool, trace))
    }

    /// Run `call` on the pool: on the runtime, inside `traced` with the
    /// traceparent of the calling thread, without the GIL. Return what
    /// `call` gives.
    fn wait<T: Send>(
        &self,
        py: Python<'_>,
        call: impl AsyncFnOnce(&Pool) -> Result<T, Error> + Send,
    ) -> PyResult<Result<T, Error>> {
        let (pool, trace) = self.start(py)?;
        let runtime = self.runtime;
        Ok(py.detach(move || runtime.block_on(traced(trace, call(pool.as_ref())))))
    }

    /// Run `call` as `wait` does. Raise its error as `client_error` gives
    /// it.
    fn run<T: Send>(
        &self,
        py: Python<'_>,
        call: impl AsyncFnOnce(&Pool) -> Result<T, Error> + Send,
    ) -> PyResult<T> {
        self.wait(py, call)?.map_err(|e| client_error(py, e))
    }

    /// Run `call` as `wait` does. Raise its error as `ZEO.ClientStorage`
    /// raises the error of a reply (`zeo_error`).
    fn run_zeo<T: Send>(
        &self,
        py: Python<'_>,
        call: impl AsyncFnOnce(&Pool) -> Result<T, Error> + Send,
    ) -> PyResult<T> {
        self.wait(py, call)?
            .map_err(|e| zeo_error(py, &self.peer_name(), e))
    }

    /// Return the address of the server as ZEO names its connection in
    /// its logs: `('host', port)`, or the path of a unix domain socket.
    fn peer_name(&self) -> String {
        match self.listener().map(|listener| listener.address().clone()) {
            Some(Address::Tcp(addr)) => format!("('{}', {})", addr.ip(), addr.port()),
            Some(Address::Unix(path)) => path.to_string_lossy().into_owned(),
            None => String::new(),
        }
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

/// An owner of early write-intent locks of a `Client` (ADR 0045): the locks
/// and the connection of the pool that holds them, from the first
/// `lock_intent` to `unlock_intent` or the `begin` that adopts them. The
/// drop releases the locks.
#[pyclass(frozen, module = "zoe._zoe", name = "Intents")]
pub(crate) struct NativeIntents {
    /// The process that opened the client.
    pid: u32,
    runtime: &'static Runtime,
    /// The locks, from the first `lock_intent`. An async mutex: a lock
    /// waits on the server with it held.
    intents: tokio::sync::Mutex<Option<PoolIntents>>,
}

impl Drop for NativeIntents {
    fn drop(&mut self) {
        let intents = self.intents.get_mut().take();
        if self.pid != std::process::id() {
            mem::forget(intents);
            return;
        }
        // The drop of the locks can start a task of the runtime.
        let _inside = self.runtime.enter();
        drop(intents);
    }
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
