//! An open transaction of a client: `store`, then `vote`, then `finish`.

use std::io::{self, Read};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use rmpv::Value;
use tokio::sync::OwnedMutexGuard;
use zoe_core::{Flavour, Oid, Tid};
use zoe_wire::extension::KV;
use zoe_wire::p64;

use super::connection::Inner;
use super::{bad, oid, tid, unsupported};
use crate::Error;
use crate::protocol::get;

/// The largest chunk of a blob that `Transaction::store_blob` sends, as
/// `ZEO.ClientStorage` sends it.
pub const BLOB_CHUNK: usize = 59_000;

/// The reply of `Transaction::vote_listed`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Voted {
    /// The oids that the server resolved, in the order of the reply.
    pub resolved: Vec<Oid>,
    /// The conflicts that the client must resolve, in the order of the
    /// reply.
    pub conflicts: Vec<Listed>,
}

/// A conflict that a vote lists for the client to resolve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    /// The oid of the conflict.
    pub oid: Oid,
    /// The serial that committed after the read of the transaction.
    pub committed: Tid,
    /// The serial that the transaction read.
    pub read: Tid,
    /// The pickle that the transaction stored.
    pub data: Vec<u8>,
}

/// An open transaction of a client: `store`, then `vote`, then `finish`.
/// A transaction that is dropped before `finish` or `abort` sends
/// `tpc_abort` and does not wait for its reply.
pub struct Transaction {
    inner: Arc<Inner>,
    id: u64,
    /// `true` until the server confirmed the finish or the abort. While it
    /// is `true`, the drop sends `tpc_abort`.
    open: bool,
    /// `true` after a failed vote or an abort. The server ended the
    /// transaction, so the client sends no more calls for it.
    ended: bool,
    /// The commit lock of the client. The drop gives it to the `tpc_abort`
    /// that it sends.
    guard: Option<OwnedMutexGuard<()>>,
    /// True when the server accepted the extension `kv`.
    kv: bool,
}

impl Transaction {
    /// Hold the transaction `id` of the connection `inner`, which began
    /// under the commit lock `guard`. `kv` tells if the server accepted the
    /// extension `kv`.
    pub(super) fn new(
        inner: Arc<Inner>,
        id: u64,
        guard: OwnedMutexGuard<()>,
        kv: bool,
    ) -> Transaction {
        Transaction {
            inner,
            id,
            open: true,
            ended: false,
            guard: Some(guard),
            kv,
        }
    }

    /// Store `data` for `oid`. `serial` is the serial that the client read,
    /// or `Tid(0)` for a new object. The server reports an error of the
    /// store at the vote. After a failed vote, it fails with
    /// `Error::Invalid` and sends nothing.
    pub async fn store(&mut self, oid: Oid, serial: Tid, data: &[u8]) -> Result<(), Error> {
        self.check()?;
        self.inner
            .notify(
                "storea",
                vec![p64(oid.0), p64(serial.0), data.into(), self.id.into()],
            )
            .await
    }

    /// Ask the server to check at the vote that `serial` is still the
    /// current serial of `oid`. A stale serial fails the vote with
    /// `Error::ReadConflict`. After a failed vote, it fails with
    /// `Error::Invalid` and sends nothing.
    pub async fn check_current_serial(&mut self, oid: Oid, serial: Tid) -> Result<(), Error> {
        self.check()?;
        self.inner
            .notify(
                "checkCurrentSerialInTransaction",
                vec![p64(oid.0), p64(serial.0), self.id.into()],
            )
            .await
    }

    /// Delete `oid` in the transaction, as `deleteObject` of
    /// `ZEO.ClientStorage` does. `serial` is the serial that the client
    /// read: the vote checks it as for a store. After a failed vote, it
    /// fails with `Error::Invalid` and sends nothing.
    pub async fn delete_object(&mut self, oid: Oid, serial: Tid) -> Result<(), Error> {
        self.check()?;
        self.inner
            .notify(
                "deleteObject",
                vec![p64(oid.0), p64(serial.0), self.id.into()],
            )
            .await
    }

    /// Stage a write of `value` for `key` in the namespace `name` of
    /// `flavour`: `kv_set`, without a reply. An ephemeral key needs `ttl`,
    /// in seconds; a durable key takes none. The write commits with the
    /// transaction. The server reports an error of the write at the vote.
    /// It needs the extension `kv`: without it, it fails with
    /// `Error::Unsupported` and sends nothing. After a failed vote, it fails
    /// with `Error::Invalid` and sends nothing.
    pub async fn kv_set(
        &mut self,
        flavour: Flavour,
        name: &str,
        key: &[u8],
        value: &[u8],
        ttl: Option<f64>,
    ) -> Result<(), Error> {
        self.check()?;
        self.check_kv()?;
        let ttl = ttl.map_or(Value::Nil, Value::F64);
        let args = vec![
            flavour.name().into(),
            name.into(),
            key.into(),
            value.into(),
            ttl,
        ];
        self.inner.notify("kv_set", args).await
    }

    /// Stage a delete of `key` in the namespace `name` of `flavour`:
    /// `kv_delete`, with the rules of `kv_set`.
    pub async fn kv_delete(
        &mut self,
        flavour: Flavour,
        name: &str,
        key: &[u8],
    ) -> Result<(), Error> {
        self.check()?;
        self.check_kv()?;
        let args = vec![flavour.name().into(), name.into(), key.into()];
        self.inner.notify("kv_delete", args).await
    }

    /// Fail with `Error::Unsupported` when the server did not accept the
    /// extension `kv`.
    fn check_kv(&self) -> Result<(), Error> {
        if self.kv {
            return Ok(());
        }
        Err(unsupported(KV))
    }

    /// Copy a record of another storage into a transaction that began with
    /// a tid, as `restore` of `ZEO.ClientStorage` does: `restorea`. `None`
    /// as `data` copies a deletion. After a failed vote, it fails with
    /// `Error::Invalid` and sends nothing.
    pub async fn restore(
        &mut self,
        oid: Oid,
        serial: Tid,
        data: Option<&[u8]>,
        prev_txn: Option<Tid>,
    ) -> Result<(), Error> {
        self.check()?;
        let data = data.map_or(Value::Nil, Value::from);
        let prev_txn = prev_txn.map_or(Value::Nil, |tid| p64(tid.0));
        self.inner
            .notify(
                "restorea",
                vec![p64(oid.0), p64(serial.0), data, prev_txn, self.id.into()],
            )
            .await
    }

    /// Undo the transaction `trans_id` in this transaction, as `undo` of
    /// `ZEO.ClientStorage` does: `undoa`, without a reply. After a failed
    /// vote, it fails with `Error::Invalid` and sends nothing.
    pub async fn undo(&mut self, trans_id: &[u8]) -> Result<(), Error> {
        self.check()?;
        self.inner
            .notify("undoa", vec![trans_id.into(), self.id.into()])
            .await
    }

    /// Store `data` for `oid` with the blob `blob`, as `ZEO.ClientStorage`
    /// does without a shared blob directory: `storea`, then
    /// `storeBlobStart`, one `storeBlobChunk` per chunk of `BLOB_CHUNK`
    /// bytes, and `storeBlobEnd`. No call has a reply. The server reports
    /// an error of the stage at the vote. After a failed vote, it fails
    /// with `Error::Invalid` and sends nothing.
    pub async fn store_blob(
        &mut self,
        oid: Oid,
        serial: Tid,
        data: &[u8],
        blob: &[u8],
    ) -> Result<(), Error> {
        self.store_blob_from(oid, serial, data, &mut &blob[..])
            .await
    }

    /// Store `data` for `oid` with the blob that `blob` reads to its end, as
    /// `store_blob` does. It reads one chunk at a time, so the blob is
    /// never in memory as a whole. A read that fails gives `Error::Io`, and
    /// `storeBlobEnd` does not go out. The transaction then counts as
    /// failed, as after a failed vote: a later `store`, `vote` or `finish`
    /// fails with `Error::Invalid` and sends nothing, and the abort or the
    /// drop sends `tpc_abort`.
    pub async fn store_blob_from<R: Read>(
        &mut self,
        oid: Oid,
        serial: Tid,
        data: &[u8],
        blob: &mut R,
    ) -> Result<(), Error> {
        self.check()?;
        let args = vec![p64(oid.0), p64(serial.0), data.into(), self.id.into()];
        self.inner.notify("storea", args.clone()).await?;
        self.inner.notify("storeBlobStart", vec![]).await?;
        let mut chunk = vec![0; BLOB_CHUNK];
        loop {
            let n = match fill(blob, &mut chunk) {
                Ok(n) => n,
                Err(error) => {
                    // The server holds an upload without its end. Only the
                    // abort may follow.
                    self.ended = true;
                    return Err(error.into());
                }
            };
            if n > 0 {
                self.inner
                    .notify("storeBlobChunk", vec![chunk[..n].into()])
                    .await?;
            }
            if n < BLOB_CHUNK {
                break;
            }
        }
        self.inner.notify("storeBlobEnd", args).await
    }

    /// Store `data` for `oid` with the blob in the file `filename` of the
    /// directory of `oid` in a blob directory that the client shares with
    /// the server, as `ZEO.ClientStorage` does with a shared blob
    /// directory: `storea`, then `storeBlobShared`. The server takes the
    /// file by rename. No call has a reply. The server reports an error of
    /// the stage at the vote. After a failed vote, it fails with
    /// `Error::Invalid` and sends nothing.
    pub async fn store_blob_shared(
        &mut self,
        oid: Oid,
        serial: Tid,
        data: &[u8],
        filename: &str,
    ) -> Result<(), Error> {
        self.check()?;
        let (oid, serial) = (p64(oid.0), p64(serial.0));
        let args = vec![oid.clone(), serial.clone(), data.into(), self.id.into()];
        self.inner.notify("storea", args).await?;
        let args = vec![oid, serial, data.into(), filename.into(), self.id.into()];
        self.inner.notify("storeBlobShared", args).await
    }

    /// Vote. A stale serial fails with `Error::Conflict`, both when the
    /// server fails the vote and when it lists the conflict for the client
    /// to resolve: this method resolves no conflict. A failed vote aborts
    /// the transaction, and a later `store`, `vote` or `finish` fails with
    /// `Error::Invalid`. A stock server can list the oids that it resolved:
    /// the vote passes, and the client ignores these oids.
    pub async fn vote(&mut self) -> Result<(), Error> {
        self.vote_resolved().await.map(drop)
    }

    /// Vote as `vote` does, and return the oids that the server resolved.
    /// The server stored another pickle than the client for each of them,
    /// so the client must load them again.
    pub async fn vote_resolved(&mut self) -> Result<Vec<Oid>, Error> {
        let voted = self.vote_listed().await?;
        let Some(listed) = voted.conflicts.first() else {
            return Ok(voted.resolved);
        };
        let error = Error::Conflict {
            oid: listed.oid,
            serials: Some((listed.committed, listed.read)),
            class_name: None,
        };
        self.end().await;
        Err(error)
    }

    /// Vote, and return the oids that the server resolved and the conflicts
    /// that it lists for the client to resolve. With listed conflicts, the
    /// transaction stays open and the server keeps its locks. The caller
    /// then stores the resolved pickle of each conflict over its committed
    /// serial and votes again, or aborts. A vote that fails aborts the
    /// transaction, as for `vote`.
    pub async fn vote_listed(&mut self) -> Result<Voted, Error> {
        self.check()?;
        let result = match self.inner.call("vote", vec![self.id.into()]).await {
            // A stock server lists the oids that it resolved, and a server
            // with conflict resolution in the client lists the conflicts.
            Ok(Value::Array(items)) => vote_reply(&items),
            Ok(other) => Err(bad("vote", &other)),
            Err(e) => Err(e),
        };
        if result.is_err() {
            self.end().await;
        }
        result
    }

    /// End the transaction after a failed vote: send `tpc_abort` and wait
    /// for its reply. The server keeps the locks of a listed conflict until
    /// the abort. The abort of a failed vote gets nil. When this future is
    /// dropped before the abort returns, the drop of the transaction sends
    /// it.
    async fn end(&mut self) {
        self.ended = true;
        if self
            .inner
            .call("tpc_abort", vec![self.id.into()])
            .await
            .is_ok()
        {
            self.open = false;
        }
    }

    /// Finish the transaction after a vote, and return its tid. The server
    /// replies when the commit is durable. `Error::Disconnected` here means
    /// that the outcome is unknown: the commit can be durable.
    pub async fn finish(mut self) -> Result<Tid, Error> {
        self.check()?;
        let (reply, _) = self
            .inner
            .call_at("tpc_finish", vec![self.id.into()], true)
            .await;
        let tid = tid(&reply?, "tpc_finish")?;
        self.open = false;
        self.inner.last_tid.fetch_max(tid.0, Ordering::SeqCst);
        Ok(tid)
    }

    /// Abort the transaction, and wait for the reply of the server.
    pub async fn abort(mut self) -> Result<(), Error> {
        self.ended = true;
        self.inner.call("tpc_abort", vec![self.id.into()]).await?;
        self.open = false;
        Ok(())
    }

    /// Fail with `Error::Invalid` when the server ended the transaction.
    fn check(&self) -> Result<(), Error> {
        if self.ended {
            return Err(Error::Invalid(
                "a call for a transaction that failed or that the server ended".into(),
            ));
        }
        Ok(())
    }
}

impl Drop for Transaction {
    fn drop(&mut self) {
        // The commit lock goes only after the frame is in the channel, so
        // the `tpc_begin` of the next transaction comes after this abort.
        if self.open {
            let guard = Box::new(self.guard.take());
            self.inner
                .send_later("tpc_abort", vec![self.id.into()], guard);
        }
    }
}

impl std::fmt::Debug for Transaction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Transaction")
            .field("id", &self.id)
            .field("open", &self.open)
            .field("ended", &self.ended)
            .finish_non_exhaustive()
    }
}

/// Read the items of a vote reply: an oid that the server resolved, or a
/// map `{oid, serials: [committed, read], data}` that lists a conflict.
pub(super) fn vote_reply(items: &[Value]) -> Result<Voted, Error> {
    let mut voted = Voted::default();
    for item in items {
        if item.is_map() {
            voted
                .conflicts
                .push(listed(item).ok_or_else(|| bad("vote", item))?);
        } else {
            voted.resolved.push(oid(item, "vote")?);
        }
    }
    Ok(voted)
}

/// Read a conflict that a vote lists. A map without the serials or the
/// data does not read.
fn listed(value: &Value) -> Option<Listed> {
    let oid = get(value, "oid").and_then(u64_of).map(Oid)?;
    let Some(Value::Array(serials)) = get(value, "serials") else {
        return None;
    };
    let [committed, read] = serials.as_slice() else {
        return None;
    };
    let data = get(value, "data")?.as_slice()?.to_vec();
    Some(Listed {
        oid,
        committed: Tid(u64_of(committed)?),
        read: Tid(u64_of(read)?),
        data,
    })
}

/// Read from `reader` until `buf` is full or the reader is at its end.
/// Return the number of bytes read.
fn fill(reader: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(filled)
}
