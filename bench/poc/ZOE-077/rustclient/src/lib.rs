//! A Python binding of `zoe-client` for the ZOE-077 POC. Not for merge.
//!
//! `python/zoeclientx` wraps it in a ZODB storage. Each call releases the
//! GIL while it waits. A tokio runtime of two threads runs the connections.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use pyo3::exceptions::{PyKeyError, PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use tokio::runtime::Runtime;
use zoe_client::{Client, Error, Invalidations, Transaction};
use zoe_core::{Oid, Tid};

fn runtime() -> &'static Runtime {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("a tokio runtime")
    })
}

/// A ZODB oid or tid: 8 bytes, big-endian.
fn u64_of(bytes: &[u8]) -> PyResult<u64> {
    let array: [u8; 8] = bytes
        .try_into()
        .map_err(|_| PyValueError::new_err("an oid or a tid has 8 bytes"))?;
    Ok(u64::from_be_bytes(array))
}

fn bytes8(py: Python<'_>, value: u64) -> Bound<'_, PyBytes> {
    PyBytes::new(py, &value.to_be_bytes())
}

// A conflict. The storage raises a `ConflictError` for it.
pyo3::create_exception!(_zoeclientx, Conflict, pyo3::exceptions::PyException);

fn to_py(error: Error) -> PyErr {
    match &error {
        Error::Server { class, message } if class.ends_with("POSKeyError") => {
            PyKeyError::new_err(message.clone())
        }
        Error::Conflict { .. } => Conflict::new_err(error.to_string()),
        _ => PyRuntimeError::new_err(error.to_string()),
    }
}

#[pyclass(frozen)]
struct ZClient {
    client: Arc<Client>,
    invalidations: Mutex<Invalidations>,
}

#[pymethods]
impl ZClient {
    #[new]
    fn new(py: Python<'_>, addr: &str) -> PyResult<Self> {
        let addr: SocketAddr = addr
            .parse()
            .map_err(|e| PyValueError::new_err(format!("{e}")))?;
        let (client, invalidations) = py
            .detach(|| {
                runtime().block_on(async {
                    let client = Client::connect(addr, "1").await?;
                    // Subscribe first, then read last_transaction.
                    let invalidations = client.invalidations();
                    Ok::<_, Error>((client, invalidations))
                })
            })
            .map_err(to_py)?;
        Ok(ZClient {
            client: Arc::new(client),
            invalidations: Mutex::new(invalidations),
        })
    }

    fn last_transaction<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        bytes8(py, self.client.last_transaction().0)
    }

    fn new_oids<'py>(&self, py: Python<'py>, n: usize) -> PyResult<Vec<Bound<'py, PyBytes>>> {
        let oids = py
            .detach(|| runtime().block_on(self.client.new_oids(n)))
            .map_err(to_py)?;
        Ok(oids.into_iter().map(|oid| bytes8(py, oid.0)).collect())
    }

    /// Return `(data, serial, next serial or None)`, or `None` when no
    /// record of the oid is older than `tid`.
    #[allow(clippy::type_complexity)]
    fn load_before<'py>(
        &self,
        py: Python<'py>,
        oid: &[u8],
        tid: &[u8],
    ) -> PyResult<
        Option<(
            Bound<'py, PyBytes>,
            Bound<'py, PyBytes>,
            Option<Bound<'py, PyBytes>>,
        )>,
    > {
        let (oid, tid) = (Oid(u64_of(oid)?), Tid(u64_of(tid)?));
        let record = py
            .detach(|| runtime().block_on(self.client.load_before(oid, tid)))
            .map_err(to_py)?;
        Ok(record.map(|r| {
            (
                PyBytes::new(py, &r.data),
                bytes8(py, r.tid.0),
                r.end.map(|end| bytes8(py, end.0)),
            )
        }))
    }

    /// Begin a transaction. It waits while another transaction of this
    /// client is open.
    fn begin(&self, py: Python<'_>) -> PyResult<ZTxn> {
        let txn = py
            .detach(|| runtime().block_on(self.client.begin(b"", b"", b"")))
            .map_err(to_py)?;
        Ok(ZTxn {
            txn: Mutex::new(Some(txn)),
        })
    }

    /// Wait at most `timeout` seconds for the next invalidation of another
    /// client. Return `(tid, oids)`, or `None` after the timeout.
    #[allow(clippy::type_complexity)]
    fn next_invalidation<'py>(
        &self,
        py: Python<'py>,
        timeout: f64,
    ) -> PyResult<Option<(Bound<'py, PyBytes>, Vec<Bound<'py, PyBytes>>)>> {
        let got = py.detach(|| {
            let mut invalidations = self.invalidations.lock().expect("the lock");
            runtime().block_on(async {
                tokio::time::timeout(Duration::from_secs_f64(timeout), invalidations.recv()).await
            })
        });
        match got {
            Err(_) => Ok(None),
            Ok(Ok(inv)) => Ok(Some((
                bytes8(py, inv.tid.0),
                inv.oids.iter().map(|oid| bytes8(py, oid.0)).collect(),
            ))),
            Ok(Err(e)) => Err(to_py(e)),
        }
    }
}

#[pyclass(frozen)]
struct ZTxn {
    txn: Mutex<Option<Transaction>>,
}

impl ZTxn {
    fn with<T>(
        &self,
        py: Python<'_>,
        f: impl AsyncFnOnce(&mut Transaction) -> Result<T, Error> + Send,
    ) -> PyResult<T>
    where
        T: Send,
    {
        py.detach(|| {
            let mut guard = self.txn.lock().expect("the lock");
            let txn = guard
                .as_mut()
                .ok_or_else(|| Error::Invalid("the transaction ended".into()))?;
            runtime().block_on(f(txn))
        })
        .map_err(to_py)
    }
}

#[pymethods]
impl ZTxn {
    fn store(&self, py: Python<'_>, oid: &[u8], serial: &[u8], data: &[u8]) -> PyResult<()> {
        let (oid, serial) = (Oid(u64_of(oid)?), Tid(u64_of(serial)?));
        self.with(py, async |txn| txn.store(oid, serial, data).await)
    }

    fn vote(&self, py: Python<'_>) -> PyResult<()> {
        self.with(py, async |txn| txn.vote().await)
    }

    fn finish<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyBytes>> {
        let txn = self.txn.lock().expect("the lock").take();
        let txn = txn.ok_or_else(|| to_py(Error::Invalid("the transaction ended".into())))?;
        let tid = py
            .detach(|| runtime().block_on(txn.finish()))
            .map_err(to_py)?;
        Ok(bytes8(py, tid.0))
    }

    fn abort(&self, py: Python<'_>) -> PyResult<()> {
        let txn = self.txn.lock().expect("the lock").take();
        match txn {
            Some(txn) => py.detach(|| runtime().block_on(txn.abort())).map_err(to_py),
            None => Ok(()),
        }
    }
}

#[pymodule]
fn _zoeclientx(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<ZClient>()?;
    m.add_class::<ZTxn>()?;
    m.add("Conflict", m.py().get_type::<Conflict>())?;
    Ok(())
}
