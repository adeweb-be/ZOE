//! `zoe._zoe.Transaction`: one transaction of a native client, on its own
//! connection of the pool, from `begin` to `finish` and `done`, or to
//! `abort`.

use std::fs::File;
use std::mem;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use pyo3::PyTraverseError;
use pyo3::gc::PyVisit;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyString};
use tokio::runtime::Runtime;
use zoe_client::{Error, PoolTransaction, Turn, lock, traced};
use zoe_core::{Oid, Tid};

use super::convert::{blob_error, client_error};
use super::{check_process, this_process, traceparent};
use crate::errors::{self, p64, u64_of};
use crate::kv;

/// The most rounds of conflict resolution in one vote, as the vote of
/// `ZEO.ClientStorage` has.
const ROUNDS: usize = 9;

/// The open transaction of a `Transaction`.
pub(super) type Slot = Mutex<Option<PoolTransaction>>;

/// One transaction of a `Client`, on its own connection of the pool, from
/// `begin` to `finish` and `done`, or to `abort`.
#[pyclass(frozen, module = "zoe._zoe", name = "Transaction")]
pub(crate) struct NativeTransaction {
    /// The process that opened the client.
    pub(super) process: u64,
    pub(super) runtime: &'static Runtime,
    /// The transaction, until its finish, its abort or a failed vote. The
    /// client keeps a weak reference to it for `Client.abort`.
    pub(super) txn: Arc<Slot>,
    /// The turn of the commit, from the return of `finish` to `done`.
    pub(super) turn: Mutex<Option<Turn>>,
    /// The callable of the traceparent of the client, or `None`.
    pub(super) traceparent: Option<Py<PyAny>>,
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
        self.run(py, async |txn| txn.store(oid, serial, data).await)
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
        let trace = self.trace(py)?;
        py.detach(|| {
            // The file opens before the lock of the transaction.
            let mut file = File::open(&filename)?;
            self.block(trace, async |txn| {
                txn.store_blob_from(oid, serial, data, &mut file).await
            })
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
        self.run(py, async |txn| {
            txn.store_blob_shared(oid, serial, data, filename).await
        })
    }

    /// Stage a write of `value` for `key` in the namespace `name` of
    /// `flavour`. `ttl` is an int or a float of seconds, or `None`. The
    /// server reports an error of the write at the vote. Raise
    /// `Unsupported` when the server did not accept `kv`.
    fn kv_set(
        &self,
        py: Python<'_>,
        flavour: &str,
        name: &str,
        key: &[u8],
        value: &[u8],
        ttl: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        let flavour = kv::flavour(flavour)?;
        let ttl = kv::ttl_seconds(ttl)?;
        self.run(py, async |txn| {
            txn.kv_set(flavour, name, key, value, ttl).await
        })
    }

    /// Stage a delete of `key` in the namespace `name` of `flavour`, with
    /// the rules of `kv_set`.
    fn kv_delete(&self, py: Python<'_>, flavour: &str, name: &str, key: &[u8]) -> PyResult<()> {
        let flavour = kv::flavour(flavour)?;
        self.run(py, async |txn| txn.kv_delete(flavour, name, key).await)
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
        self.run(py, async |txn| txn.delete_object(oid, serial).await)
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
        self.run(py, async |txn| {
            txn.restore(oid, serial, data, prev_txn).await
        })
    }

    /// Undo the transaction `trans_id` in this transaction: `undoa`, sent to
    /// the server, which answers as it does for `ZEO.ClientStorage`. The id
    /// is bytes, as `undoLog` gives it, or a `str` of its text.
    fn undo(&self, py: Python<'_>, trans_id: &Bound<'_, PyAny>) -> PyResult<()> {
        check_process(self.process)?;
        let trans_id: Vec<u8> = match trans_id.cast::<PyString>() {
            Ok(text) => text.to_str()?.as_bytes().to_vec(),
            Err(_) => trans_id.extract()?,
        };
        self.run(py, async |txn| txn.undo(&trans_id).await)
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
        self.run(py, async |txn| txn.check_current_serial(oid, serial).await)
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
        check_process(self.process)?;
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
                    self.block(trace, async |txn| {
                        txn.store(listed.oid, listed.committed, bytes).await
                    })
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
        check_process(self.process)?;
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

    /// Return every oid of the commit that holds the turn, when its vote
    /// listed as resolved an oid that the transaction did not store: the
    /// server wrote that oid (ADR 0049), or it is an oid of an undo. The
    /// caller gives them to every Connection of the process in one
    /// invalidation before the callback of `tpc_finish`. Else `None`.
    fn unstored_commit<'py>(&self, py: Python<'py>) -> Option<Vec<Bound<'py, PyBytes>>> {
        let oids = py.detach(|| {
            lock(&self.turn)
                .as_ref()
                .and_then(|turn| turn.unstored_commit().map(<[Oid]>::to_vec))
        })?;
        Some(oids.into_iter().map(|oid| p64(py, oid.0)).collect())
    }

    /// End the turn of the commit: the callback of `tpc_finish` ran. The
    /// pool then delivers the tid and goes on. Without a turn, do nothing.
    fn done(&self, py: Python<'_>) -> PyResult<()> {
        check_process(self.process)?;
        py.detach(|| drop(lock(&self.turn).take()));
        Ok(())
    }

    /// Abort the transaction, and wait for the reply of the server. After
    /// its end, do nothing.
    fn abort(&self, py: Python<'_>) -> PyResult<()> {
        check_process(self.process)?;
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
    /// Return the W3C traceparent of the calling thread for a call of the
    /// transaction. In a child of `os.fork()`, raise `RuntimeError`.
    fn trace(&self, py: Python<'_>) -> PyResult<Option<String>> {
        check_process(self.process)?;
        traceparent(py, self.traceparent.as_ref())
    }

    /// Run `call` on the open transaction, on the runtime, inside `traced`
    /// with `trace`. The caller holds no GIL. Without an open transaction,
    /// fail with `Error::Invalid`.
    fn block<T>(
        &self,
        trace: Option<String>,
        call: impl AsyncFnOnce(&mut PoolTransaction) -> Result<T, Error>,
    ) -> Result<T, Error> {
        let mut slot = lock(&self.txn);
        let txn = slot.as_mut().ok_or_else(not_open)?;
        self.runtime.block_on(traced(trace, call(txn)))
    }

    /// Run `call` on the open transaction as `block` does, with the
    /// traceparent of the calling thread, without the GIL. Raise its error
    /// as `client_error` gives it.
    fn run<T: Send>(
        &self,
        py: Python<'_>,
        call: impl AsyncFnOnce(&mut PoolTransaction) -> Result<T, Error> + Send,
    ) -> PyResult<T> {
        let trace = self.trace(py)?;
        py.detach(|| self.block(trace, call))
            .map_err(|e| client_error(py, e))
    }

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
        if self.process != this_process() {
            mem::forget((txn, turn));
            return;
        }
        // The drop of a transaction can start a task of the runtime.
        let _inside = self.runtime.enter();
        drop(txn);
        drop(turn);
    }
}

/// The error of a call without an open transaction.
fn not_open() -> Error {
    Error::Invalid("no transaction is open".into())
}
