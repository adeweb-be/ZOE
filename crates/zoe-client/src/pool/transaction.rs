//! The transactions of a pool, each on its own connection, their finishes
//! and their turns, and the early write-intent locks of an owner.

use std::collections::HashSet;
use std::io::Read;
use std::sync::Arc;

use zoe_core::{Flavour, Oid, Tid};

use super::{Lease, Shared, Slot};
use crate::Error;
use crate::client::{Client, Transaction, Voted};

/// The early write-intent locks of one owner (ADR 0045), on a connection
/// of the pool that the owner leases from its first lock to the end of
/// its transaction. `begin_with` begins the transaction that adopts the
/// locks, on the same connection. The drop sends `unlock_intent` and gives
/// the connection back, so the next transaction of the connection adopts
/// none of them.
pub struct PoolIntents {
    lease: Option<Lease>,
    /// True after a lock that the server granted.
    held: bool,
}

impl PoolIntents {
    /// Hold the locks of one owner on the connection of `lease`. The owner
    /// holds no lock yet.
    pub(super) fn new(lease: Lease) -> PoolIntents {
        PoolIntents {
            lease: Some(lease),
            held: false,
        }
    }

    fn client(&self) -> &Client {
        self.lease.as_ref().expect("a lease").client()
    }

    /// Tell if the server granted a lock of the owner.
    pub fn holds(&self) -> bool {
        self.held
    }

    /// Tell if the connection of the owner is connected.
    pub fn is_connected(&self) -> bool {
        self.client().is_connected()
    }

    /// Take the intent locks of `oids`, as `Client::lock_intent` does, and
    /// return their current serials.
    pub async fn lock(&mut self, oids: &[Oid]) -> Result<Vec<Tid>, Error> {
        let serials = self.client().lock_intent(oids).await?;
        self.held = true;
        Ok(serials)
    }

    /// Release the locks, wait for the reply, and give the connection back.
    pub async fn release(mut self) -> Result<(), Error> {
        if std::mem::replace(&mut self.held, false) {
            self.client().unlock_intent().await?;
        }
        Ok(())
    }

    /// Begin the transaction that adopts the locks, on the connection of
    /// the locks, as `Pool::begin_with` does.
    pub async fn begin_with(
        mut self,
        user: &[u8],
        description: &[u8],
        extension: &[u8],
        tid: Option<Tid>,
        status: &str,
    ) -> Result<PoolTransaction, Error> {
        let txn = self
            .client()
            .begin_with(user, description, extension, tid, status)
            .await?;
        self.held = false;
        let lease = self.lease.take().expect("a lease");
        Ok(PoolTransaction::new(txn, lease))
    }
}

impl Drop for PoolIntents {
    fn drop(&mut self) {
        if !self.held {
            return;
        }
        // The lease goes back to the pool only after the release is in the
        // channel of the connection.
        if let Some(lease) = self.lease.take()
            && let Some(client) = lease.client.clone()
        {
            client.unlock_intent_later(Box::new(lease));
        }
    }
}

impl std::fmt::Debug for PoolIntents {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PoolIntents")
            .field("held", &self.held)
            .finish_non_exhaustive()
    }
}

/// A transaction of the pool, on its own connection: `store`, then `vote`,
/// then `finish`. A transaction that is dropped before its finish or its
/// abort sends `tpc_abort`, as `Transaction` does, and gives its
/// connection back.
pub struct PoolTransaction {
    // The transaction drops before the lease, so its abort goes out before
    // another transaction takes the connection.
    txn: Option<Transaction>,
    lease: Lease,
    /// The stores of the transaction, for the cache of the pool, in order.
    /// The data is `None` when the cache cannot hold it, or when the server
    /// resolved the oid. Empty when the pool has no cache.
    writes: Vec<(Oid, Option<Vec<u8>>)>,
    /// The oids that the transaction stored, deleted or restored.
    stored: HashSet<Oid>,
    /// True when a vote listed an oid as resolved.
    resolved: bool,
    /// True when a vote listed as resolved an oid that the transaction did
    /// not store: the server wrote it (ADR 0049), or the oid of an undo.
    unstored: bool,
}

impl PoolTransaction {
    /// Hold `txn`, which began on the connection of `lease`.
    pub(super) fn new(txn: Transaction, lease: Lease) -> PoolTransaction {
        PoolTransaction {
            txn: Some(txn),
            lease,
            writes: Vec::new(),
            stored: HashSet::new(),
            resolved: false,
            unstored: false,
        }
    }

    fn txn(&mut self) -> &mut Transaction {
        self.txn.as_mut().expect("an open transaction")
    }

    /// Keep the store of `data` for `oid`, for the cache of the pool.
    fn keep(&mut self, oid: Oid, data: &[u8]) {
        self.stored.insert(oid);
        if let Some(cache) = &self.lease.shared.cache {
            let data = cache.fits(data.len()).then(|| data.to_vec());
            self.writes.push((oid, data));
        }
    }

    /// Store `data` for `oid`, as `Transaction::store` does. The resolution
    /// of a conflict that a vote listed is a store over the serial
    /// `committed` that the vote gave. The server stores these bytes
    /// unchanged, so the turn adds them to the cache of the pool, as
    /// `ZEO.ClientStorage` does.
    pub async fn store(&mut self, oid: Oid, serial: Tid, data: &[u8]) -> Result<(), Error> {
        self.txn().store(oid, serial, data).await?;
        self.keep(oid, data);
        Ok(())
    }

    /// Delete `oid`, as `Transaction::delete_object` does. The turn ends
    /// the current entry of `oid` in the cache of the pool.
    pub async fn delete_object(&mut self, oid: Oid, serial: Tid) -> Result<(), Error> {
        self.txn().delete_object(oid, serial).await?;
        self.forget(oid);
        Ok(())
    }

    /// Copy a record of another storage, as `Transaction::restore` does.
    /// The turn ends the current entry of `oid` in the cache of the pool,
    /// and adds nothing.
    pub async fn restore(
        &mut self,
        oid: Oid,
        serial: Tid,
        data: Option<&[u8]>,
        prev_txn: Option<Tid>,
    ) -> Result<(), Error> {
        self.txn().restore(oid, serial, data, prev_txn).await?;
        self.forget(oid);
        Ok(())
    }

    /// Stage a write of a namespace, as `Transaction::kv_set` does.
    pub async fn kv_set(
        &mut self,
        flavour: Flavour,
        name: &str,
        key: &[u8],
        value: &[u8],
        ttl: Option<f64>,
    ) -> Result<(), Error> {
        self.txn().kv_set(flavour, name, key, value, ttl).await
    }

    /// Stage a delete of a key of a namespace, as `Transaction::kv_delete`
    /// does.
    pub async fn kv_delete(
        &mut self,
        flavour: Flavour,
        name: &str,
        key: &[u8],
    ) -> Result<(), Error> {
        self.txn().kv_delete(flavour, name, key).await
    }

    /// Undo the transaction `trans_id`, as `Transaction::undo` does. The
    /// vote lists the oids of the undo as resolved by the server, and the
    /// turn ends their current entries in the cache of the pool.
    pub async fn undo(&mut self, trans_id: &[u8]) -> Result<(), Error> {
        self.txn().undo(trans_id).await
    }

    /// Keep a write of `oid` without data, for the cache of the pool.
    fn forget(&mut self, oid: Oid) {
        self.stored.insert(oid);
        if self.lease.shared.cache.is_some() {
            self.writes.push((oid, None));
        }
    }

    /// Check `serial` of `oid` at the vote, as
    /// `Transaction::check_current_serial` does.
    pub async fn check_current_serial(&mut self, oid: Oid, serial: Tid) -> Result<(), Error> {
        self.txn().check_current_serial(oid, serial).await
    }

    /// Store `data` for `oid` with the blob `blob`, as
    /// `Transaction::store_blob` does.
    pub async fn store_blob(
        &mut self,
        oid: Oid,
        serial: Tid,
        data: &[u8],
        blob: &[u8],
    ) -> Result<(), Error> {
        self.txn().store_blob(oid, serial, data, blob).await?;
        self.keep(oid, data);
        Ok(())
    }

    /// Store `data` for `oid` with the blob that `blob` reads, as
    /// `Transaction::store_blob_from` does.
    pub async fn store_blob_from<R: Read>(
        &mut self,
        oid: Oid,
        serial: Tid,
        data: &[u8],
        blob: &mut R,
    ) -> Result<(), Error> {
        self.txn().store_blob_from(oid, serial, data, blob).await?;
        self.keep(oid, data);
        Ok(())
    }

    /// Store `data` for `oid` with the blob file `filename` of a shared
    /// blob directory, as `Transaction::store_blob_shared` does.
    pub async fn store_blob_shared(
        &mut self,
        oid: Oid,
        serial: Tid,
        data: &[u8],
        filename: &str,
    ) -> Result<(), Error> {
        self.txn()
            .store_blob_shared(oid, serial, data, filename)
            .await?;
        self.keep(oid, data);
        Ok(())
    }

    /// Vote, as `Transaction::vote_resolved` does. The cache of the pool
    /// will not hold the pickles of the oids that the server resolved. A
    /// conflict drops the current entry of its oid from the cache, as
    /// `tpc_vote` of `ZEO.ClientStorage` does.
    pub async fn vote_resolved(&mut self) -> Result<Vec<Oid>, Error> {
        let voted = self.txn().vote_resolved().await;
        match &voted {
            Ok(resolved) => self.server_resolved(resolved),
            Err(error) => self.failed(error),
        }
        voted
    }

    /// Vote, as `Transaction::vote_listed` does, with the cache rules of
    /// `vote_resolved`. A listed conflict changes nothing in the cache: the
    /// caller stores its resolution with `store`, or calls `unresolved`.
    pub async fn vote_listed(&mut self) -> Result<Voted, Error> {
        let voted = self.txn().vote_listed().await;
        match &voted {
            Ok(voted) => self.server_resolved(&voted.resolved),
            Err(error) => self.failed(error),
        }
        voted
    }

    /// Drop the current entry of `oid` from the cache: the client did not
    /// resolve the conflict that a vote listed for it, as `tpc_vote` of
    /// `ZEO.ClientStorage` does. The transaction stays open for its abort.
    pub fn unresolved(&self, oid: Oid) {
        if let Some(cache) = &self.lease.shared.cache {
            cache.drop_current(oid);
        }
    }

    /// Keep no pickle of the oids that the server resolved for the cache.
    /// An oid that the transaction did not store, as an oid of an undo,
    /// becomes a write without data, as `TransactionBuffer` of ZEO gives
    /// it: the turn ends its current entry.
    fn server_resolved(&mut self, resolved: &[Oid]) {
        self.resolved |= !resolved.is_empty();
        if resolved.iter().any(|oid| !self.stored.contains(oid)) {
            self.unstored = true;
        }
        if resolved.is_empty() || self.lease.shared.cache.is_none() {
            return;
        }
        let mut left: HashSet<Oid> = resolved.iter().copied().collect();
        for (oid, data) in &mut self.writes {
            if resolved.contains(oid) {
                *data = None;
                left.remove(oid);
            }
        }
        // The order of the reply, for a fixed order in the cache.
        for oid in resolved {
            if left.remove(oid) {
                self.writes.push((*oid, None));
            }
        }
    }

    /// Drop the current entry of the oid of a conflict of a failed vote.
    fn failed(&self, error: &Error) {
        if let Error::Conflict { oid, .. } | Error::ReadConflict { oid, .. } = error {
            self.unresolved(*oid);
        }
    }

    /// Vote, as `Transaction::vote` does, with the cache rules of
    /// `vote_resolved`.
    pub async fn vote(&mut self) -> Result<(), Error> {
        self.vote_resolved().await.map(drop)
    }

    /// Finish the transaction after its vote, and wait for the reply of the
    /// server. Then `Finished::turn` waits for the turn of its tid.
    ///
    /// The finish counts as in flight from before its frame goes out, until
    /// its reply or its failure, also when its future is dropped. After the
    /// close, after a lost listener, or on a connection of a server that
    /// the pool lost, it fails with `Error::Disconnected` and sends
    /// nothing. A finish that waited for its reply when the listener was
    /// lost fails with `Error::Disconnected` too: its outcome counts as
    /// unknown.
    pub async fn finish(mut self) -> Result<Finished, Error> {
        let shared = self.lease.shared.clone();
        let seq = {
            let mut state = shared.lock();
            if state.closed || state.lost || state.generation != self.lease.generation {
                return Err(Error::Disconnected);
            }
            let seq = state.next_seq;
            state.next_seq += 1;
            state.in_flight.insert(seq);
            seq
        };
        let mut flight = InFlight {
            shared: shared.clone(),
            seq,
            armed: true,
        };
        let txn = self.txn.take().expect("an open transaction");
        let writes = std::mem::take(&mut self.writes);
        let unstored = self.unstored;
        // The turn needs the oids of the commit for the consumer, or for the
        // cache when the server resolved an oid.
        let wants_oids = unstored || (self.resolved && shared.cache.is_some());
        let tid = txn.finish().await?;
        {
            let mut state = shared.lock();
            state.in_flight.remove(&seq);
            if state.doomed.remove(&seq) || state.closed {
                drop(state);
                drop(flight);
                return Err(Error::Disconnected);
            }
            if state.head.is_some_and(|head| head < tid.0) {
                state.counters.waited += 1;
            }
            state.replied.insert(
                tid.0,
                Slot {
                    wants_oids,
                    ..Slot::default()
                },
            );
        }
        flight.armed = false;
        drop(flight);
        shared.bump();
        Ok(Finished {
            shared,
            tid,
            armed: true,
            writes,
            unstored,
        })
    }

    /// Abort the transaction, and wait for the reply of the server.
    pub async fn abort(mut self) -> Result<(), Error> {
        let txn = self.txn.take().expect("an open transaction");
        txn.abort().await
    }
}

impl std::fmt::Debug for PoolTransaction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PoolTransaction")
            .field("txn", &self.txn)
            .finish_non_exhaustive()
    }
}

/// A finish that counts as in flight. The drop takes it out of the hold,
/// unless the finish got its reply.
struct InFlight {
    shared: Arc<Shared>,
    seq: u64,
    /// `true` until the finish got its reply.
    armed: bool,
}

impl Drop for InFlight {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        {
            let mut state = self.shared.lock();
            state.in_flight.remove(&self.seq);
            state.doomed.remove(&self.seq);
        }
        self.shared.bump();
    }
}

/// A finish that has the reply of the server: its commit is durable.
/// `turn` waits until the pool delivered every tid below it.
///
/// A `Finished` that is dropped before its turn leaves its tid to the
/// consumer of the pool, as the tid of another client.
pub struct Finished {
    shared: Arc<Shared>,
    tid: Tid,
    /// `true` until `turn` took the turn.
    armed: bool,
    /// The stores of the commit, for the cache of the pool.
    writes: Vec<(Oid, Option<Vec<u8>>)>,
    /// A vote listed as resolved an oid that the transaction did not store.
    unstored: bool,
}

impl Finished {
    /// Return the tid of the commit.
    pub fn tid(&self) -> Tid {
        self.tid
    }

    /// Wait for the turn of the tid: the pool delivered every tid below it.
    /// The caller runs the callback of its finish, then drops the `Turn`.
    /// After a lost listener, the turn comes after `Delivery::Stale`. After
    /// the close, it fails with `Error::Disconnected`.
    ///
    /// At the turn, before it returns, the commit goes into the cache of the
    /// pool: it ends the current entry of each oid that it wrote, and adds
    /// the data of each store that the server did not resolve. An oid that
    /// the transaction wrote and that the commit does not hold, as a node
    /// that the server resolved without a record (ADR 0049), only loses its
    /// current entry: its next load enters the cache again. A turn after
    /// a lost listener only drops the current entry of each oid that it
    /// wrote, and the cache skips the tid (`Cache::skip`): the cache did
    /// not get the commits before it.
    pub async fn turn(mut self) -> Result<Turn, Error> {
        let tid = self.tid;
        let (stale, oids) = self
            .shared
            .until(|state| match state.replied.get(&tid.0) {
                Some(slot) if slot.turn => {
                    let stale = slot.stale;
                    let oids = state.replied.remove(&tid.0).and_then(|slot| slot.oids);
                    state.running = Some(tid.0);
                    Some(Ok((stale, oids)))
                }
                _ if state.closed => Some(Err(Error::Disconnected)),
                _ => None,
            })
            .await?;
        self.armed = false;
        if let Some(cache) = &self.shared.cache {
            if stale {
                let oids: Vec<Oid> = self.writes.iter().map(|(oid, _)| *oid).collect();
                cache.skip(tid, &oids);
            } else {
                let mut writes = last_writes(&self.writes);
                // The server resolved some oids. It wrote no record for an
                // oid that the commit does not hold: drop its current entry,
                // and do not end it at the tid.
                if let Some(commit) = &oids {
                    let commit: HashSet<Oid> = commit.iter().copied().collect();
                    writes.retain(|(oid, _)| {
                        let held = commit.contains(oid);
                        if !held {
                            cache.drop_current(*oid);
                        }
                        held
                    });
                }
                cache.commit(tid, &writes);
            }
        }
        // The oids of the commit go to the consumer only when the server
        // wrote an oid that the transaction did not store.
        let oids = if self.unstored { oids } else { None };
        Ok(Turn {
            shared: self.shared.clone(),
            tid,
            stale,
            oids,
        })
    }
}

impl Drop for Finished {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        {
            let mut state = self.shared.lock();
            if let Some(slot) = state.replied.remove(&self.tid.0)
                && slot.turn
            {
                state.abandoned.insert(self.tid.0);
            }
        }
        self.shared.bump();
    }
}

impl std::fmt::Debug for Finished {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Finished").field("tid", &self.tid).finish()
    }
}

/// The turn of a commit of the pool: every tid below it was delivered. The
/// caller runs the callback of the finish while it holds the turn. The drop
/// of the turn delivers the tid, and the pool goes on.
pub struct Turn {
    shared: Arc<Shared>,
    tid: Tid,
    stale: bool,
    /// Every oid of the commit, when a vote of the transaction listed as
    /// resolved an oid that it did not store.
    oids: Option<Vec<Oid>>,
}

impl Turn {
    /// Return the tid of the commit.
    pub fn tid(&self) -> Tid {
        self.tid
    }

    /// Tell if the turn came after a lost listener, after
    /// `Delivery::Stale`.
    pub fn stale(&self) -> bool {
        self.stale
    }

    /// Return every oid of the commit, from its invalidation on the
    /// listener, when a vote of the transaction listed as resolved an oid
    /// that the transaction did not store (ADR 0049). The caller gives
    /// them to every Connection of its process, in one invalidation, before
    /// the callback of the finish. Else, and for a stale turn, `None`.
    pub fn unstored_commit(&self) -> Option<&[Oid]> {
        self.oids.as_deref()
    }
}

impl Drop for Turn {
    fn drop(&mut self) {
        {
            let mut state = self.shared.lock();
            state.running = None;
            state.last = state.last.max(self.tid.0);
            state.ended += 1;
        }
        self.shared.bump();
    }
}

impl std::fmt::Debug for Turn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Turn")
            .field("tid", &self.tid)
            .field("stale", &self.stale)
            .finish()
    }
}

/// Return the last write of each oid of `writes`, in the order of these
/// last writes.
fn last_writes(writes: &[(Oid, Option<Vec<u8>>)]) -> Vec<(Oid, Option<&[u8]>)> {
    let mut seen = HashSet::new();
    let mut last: Vec<_> = writes
        .iter()
        .rev()
        .filter(|(oid, _)| seen.insert(*oid))
        .map(|(oid, data)| (*oid, data.as_deref()))
        .collect();
    last.reverse();
    last
}
