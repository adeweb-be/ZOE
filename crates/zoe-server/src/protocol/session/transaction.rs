//! `Transaction`: the open transaction of a connection, and the calls of
//! the transaction. One thread holds it and runs these calls in order.

use std::sync::Arc;
use std::time::{Duration, Instant};

use rmpv::Value;
use zoe_core::{BlobFile, Compression, Finished, IntentLocks, Oid, Store, Tid, Txn, ZoeError};
use zoe_wire::p64;

use super::args::{
    Args, ArgsMut, bin_of, flavour_of, serial_of, store_serial, str_of, text_bytes, ttl_of, u64_of,
};
use super::{Fault, Outcome, Session};
use crate::protocol::compress::{Compressor, Job};
use crate::protocol::extension;
use crate::protocol::message::{self, Call};
use crate::protocol::method::{self, Op};
use crate::protocol::status::Stats;
use crate::runtime::Runtime;

/// The key of the transaction extension that names the method of the
/// request of the transaction. `zoe.plone` sets it.
pub(super) const REQUEST_METHOD: &str = "zoe.request_method";

/// Tell if `extension`, the map of the extension of `tpc_begin`, marks the
/// transaction of a GET or a HEAD request.
pub(super) fn of_get(extension: &Value) -> bool {
    let Value::Map(items) = extension else {
        return false;
    };
    items.iter().any(|(key, value)| {
        key.as_str() == Some(REQUEST_METHOD)
            && value
                .as_str()
                .is_some_and(|m| m.eq_ignore_ascii_case("GET") || m.eq_ignore_ascii_case("HEAD"))
    })
}

/// Return the records that `txn` staged, by class. A deletion and a
/// pickle whose class does not decode have no class.
pub(super) fn classes_of(txn: &Txn<Arc<Store>>) -> std::collections::BTreeMap<String, u64> {
    let mut classes = std::collections::BTreeMap::new();
    for oid in txn.staged_oids() {
        if let Some(class) = txn.staged_class_name(oid) {
            *classes.entry(class).or_default() += 1;
        }
    }
    classes
}

/// The longest time that a session holds intent locks without a
/// `tpc_begin`, by default. Then the server releases them (ADR 0045).
pub(crate) const INTENT_HOLD: Duration = Duration::from_secs(60);

/// The open transaction of a connection.
pub(super) struct Open {
    /// The id that the client gave to the transaction.
    pub(super) id: Value,
    /// The core transaction, or the first error of the transaction: an
    /// error of `tpc_begin` or of a call without a reply. The vote fails
    /// with that error, as ZEO reports these errors at the vote. The error
    /// takes the place of the core transaction, so the core transaction
    /// drops, and that aborts it.
    pub(super) txn: Result<Txn<Arc<Store>>, Fault>,
    /// The number of conflicts that the last vote listed for the client to
    /// resolve. When it is not 0, the next vote is the second vote.
    pub(super) listed: u64,
    /// The number of oids that tier 1 merged in the first vote when that
    /// vote listed conflicts too. The second vote that passes counts them.
    pub(super) native: u64,
    /// The blob file that the client uploads, from `storeBlobStart` to
    /// `storeBlobEnd`. The end of the transaction drops it, and that
    /// removes its file.
    pub(super) upload: Option<BlobFile>,
    /// The jobs of the compression pool that encode the staged pickles,
    /// until the first vote. The end of the transaction drops them, and
    /// that cancels them.
    pub(super) jobs: Vec<Job>,
    /// True for the transaction of a GET or a HEAD request: its commit
    /// counts its records by class in the writes-on-GET report.
    pub(super) of_get: bool,
    /// The undo ids that `undoa` queued, in order. The first vote stages
    /// their undos.
    pub(super) undos: Vec<Value>,
    /// True after a vote that passed: the transaction holds its locks
    /// until it ends. The transaction timeout counts from then.
    pub(super) voted: bool,
}

impl Open {
    /// Run `step` on the core transaction. An error of `step` ends the core
    /// transaction and fails the vote. After an error, `step` does not run.
    pub(super) fn stage(&mut self, step: impl FnOnce(&mut Txn<Arc<Store>>) -> Result<(), Fault>) {
        if let Ok(txn) = &mut self.txn
            && let Err(fault) = step(txn)
        {
            self.txn = Err(fault);
        }
    }
}

/// The open transaction of one connection. One thread holds it and runs
/// the calls of the transaction in order. That thread begins the core
/// transaction and makes every later call of it, so the core never refuses
/// a call with `WrongThread`.
pub(crate) struct Transaction {
    pub(super) session: Arc<Session>,
    pub(super) open: Option<Open>,
    /// The commit of the `tpc_finish` that runs, with its reply. `call`
    /// publishes it after it counts the call.
    pub(super) committed: Option<(Finished, Option<Vec<u8>>)>,
    /// The early write-intent locks of the session (ADR 0045), from its
    /// first `lock_intent`. The next `tpc_begin` adopts them.
    pub(super) intents: Option<IntentLocks<Arc<Store>>>,
    /// The compression pool and the runtime of its jobs. Without them, the
    /// finish encodes each staged pickle.
    pub(super) compress: Option<(Compressor, Arc<dyn Runtime>)>,
}

impl Transaction {
    /// Return the transaction state of a connection of `session`, with no
    /// open transaction.
    pub(crate) fn new(session: Arc<Session>) -> Self {
        Self {
            session,
            open: None,
            committed: None,
            intents: None,
            compress: None,
        }
    }

    /// Encode the staged pickles on `compressor`, with jobs on `runtime`.
    pub(crate) fn compressing(mut self, compressor: Compressor, runtime: Arc<dyn Runtime>) -> Self {
        self.compress = Some((compressor, runtime));
        self
    }

    /// Tell if a transaction is open, or if the session holds intent
    /// locks for its next transaction.
    pub(crate) fn is_open(&self) -> bool {
        self.open.is_some() || self.holds_intents()
    }

    /// Return the transaction timeout of the storage when the open
    /// transaction passed a vote, or `None`.
    pub(crate) fn voted_timeout(&self) -> Option<Duration> {
        if self.open.as_ref().is_some_and(|open| open.voted) {
            self.session.storage.transaction_timeout
        } else {
            None
        }
    }

    /// Tell if the session holds intent locks.
    pub(super) fn holds_intents(&self) -> bool {
        self.intents
            .as_ref()
            .is_some_and(|intents| !intents.is_empty())
    }

    /// Return the time that the session can still hold its intent locks
    /// without a `tpc_begin`, or `None` when it holds none or a transaction
    /// is open.
    pub(crate) fn intent_hold_left(&self) -> Option<Duration> {
        if self.open.is_some() {
            return None;
        }
        let held_for = self.intents.as_ref()?.held_for()?;
        Some(self.session.storage.intent_hold.saturating_sub(held_for))
    }

    /// Release the intent locks that the session held past the bound
    /// without a `tpc_begin`, and log one WARN event. The locks are
    /// advisory, so the drop changes no outcome but the chance of a
    /// conflict.
    pub(crate) fn drop_intents(&mut self) {
        let Some(intents) = self.intents.as_mut() else {
            return;
        };
        let oids = intents.oids();
        intents.release();
        if let Some(&first) = oids.first() {
            tracing::warn!(
                oids = oids.len(),
                first = first.hex(),
                held_ms = self.session.storage.intent_hold.as_millis() as u64,
                "released the intent locks that a client held without tpc_begin"
            );
        }
    }

    /// Run one call of the transaction, and count it in the status and the
    /// metrics of the storage. `storea` and `restorea` take the pickle out
    /// of the args of `call` for the compression pool.
    ///
    /// The caller puts the reply after this count. The reply of a commit
    /// goes out with its publication, so the publication comes after the
    /// count too. So a client that has a reply reads metrics that count its
    /// call.
    pub(crate) fn call(&mut self, call: &mut Call) -> Outcome {
        let was_open = self.open.is_some();
        let listed = self.open.as_ref().map_or(0, |open| open.listed);
        let started = Instant::now();
        let op = method::find(&call.method).map(|method| method.op);
        let outcome = self.dispatch(call, op);
        // The publication exists before the count. So a panic of the count
        // still publishes the commit.
        let publication = self
            .committed
            .take()
            .map(|(finished, reply)| self.session.publication(finished, reply));
        let elapsed = started.elapsed();
        self.count(op, &outcome, was_open);
        self.measure(op, &outcome, was_open, listed, elapsed);
        drop(publication);
        outcome
    }

    /// Add a call that ran for `elapsed` to the metrics of the storage.
    /// `was_open` tells if a transaction was open before the call, and
    /// `listed` gives the conflicts that its last vote listed. Only the
    /// thread of the transaction changes `open`, so the call sees the
    /// state that it left.
    fn measure(
        &self,
        op: Option<Op>,
        outcome: &Outcome,
        was_open: bool,
        listed: u64,
        elapsed: Duration,
    ) {
        let metrics = &self.session.storage.metrics;
        let ended = was_open && self.open.is_none();
        let conflict = matches!(outcome, Outcome::Failed(fault) if fault.is_conflict());
        let vote = op == Some(Op::Vote);
        match op {
            Some(Op::Vote) => metrics.vote_latency.observe_duration(elapsed),
            Some(Op::TpcFinish) if ended => {
                metrics.finish_latency.observe_duration(elapsed);
                if matches!(outcome, Outcome::Published) {
                    Stats::add(&metrics.commits);
                }
            }
            _ => {}
        }
        // A vote that fails with a conflict raises it in the client. So
        // does a transaction that ends between the two votes: the client
        // did not resolve a conflict that the vote listed.
        if (vote && conflict) || (ended && listed > 0 && !vote) {
            Stats::add(&metrics.conflicts_raised);
        }
    }

    /// Count a call that ran, as ZEO counts it. `was_open` tells if a
    /// transaction was open before the call.
    fn count(&self, op: Option<Op>, outcome: &Outcome, was_open: bool) {
        let stats = &self.session.storage.stats;
        let is_open = self.open.is_some();
        match (was_open, is_open) {
            (false, true) => Stats::add(&stats.active_txns),
            (true, false) => Stats::sub(&stats.active_txns),
            _ => {}
        }
        // An end of the transaction that the call named.
        let ended = was_open && !is_open;
        // ZEO counts `storea`, `deleteObject` and `restorea` in `stores`.
        let stores = matches!(op, Some(Op::Storea | Op::DeleteObject | Op::Restorea));
        let counter = match outcome {
            Outcome::Close(_) | Outcome::Gone => None,
            Outcome::Done(_) if stores => Some(&stats.stores),
            Outcome::Failed(fault) if op == Some(Op::Vote) && fault.is_conflict() => {
                Some(&stats.conflicts)
            }
            _ if op == Some(Op::TpcFinish) && ended => Some(&stats.commits),
            _ if op == Some(Op::TpcAbort) && ended => Some(&stats.aborts),
            _ => None,
        };
        if let Some(counter) = counter {
            Stats::add(counter);
        }
    }

    /// Run one call of the transaction, whose method has the arm `op` of
    /// the dispatch.
    fn dispatch(&mut self, call: &mut Call, op: Option<Op>) -> Outcome {
        if !self.session.is_registered() {
            return Outcome::Close(format!("{} before register", call.method));
        }
        if let Some(fault) = self.session.undeclared(&call.method) {
            return Outcome::Failed(fault);
        }
        let Call { method, args, .. } = call;
        let changed = ArgsMut {
            method,
            values: args,
        };
        let args = Args {
            method,
            values: changed.values,
        };
        let result = match op {
            Some(Op::LockIntent) => self.lock_intent(args),
            Some(Op::UnlockIntent) => self.unlock_intent(args),
            Some(Op::TpcBegin) => self.tpc_begin(args),
            Some(Op::Storea) => self.storea(changed),
            Some(Op::Restorea) => self.restorea(changed),
            Some(Op::CheckCurrentSerial) => self.check_current_serial(args),
            Some(Op::DeleteObject) => self.delete_object(args),
            Some(Op::KvSet) => self.kv_set(args),
            Some(Op::KvDelete) => self.kv_delete(args),
            Some(Op::Undoa) => self.undoa(args),
            Some(Op::Vote) => self.vote(args),
            Some(Op::TpcFinish) => return self.tpc_finish(&*call),
            Some(Op::TpcAbort) => self.tpc_abort(args),
            Some(Op::StoreBlobStart) => self.store_blob_start(args),
            Some(Op::StoreBlobChunk) => self.store_blob_chunk(args),
            Some(Op::StoreBlobEnd) => self.store_blob_end(args),
            Some(Op::StoreBlobShared) => self.store_blob_shared(args),
            // The other calls run in `Session::dispatch`.
            _ => return Outcome::Close(format!("unknown method {}", call.method)),
        };
        result.into()
    }

    /// Begin the transaction of the client. `tpc_begin` has no reply, so
    /// an error in it fails the vote of the transaction, and the
    /// connection stays open.
    ///
    /// A tid in the args begins a restore transaction, which commits under
    /// that tid. As in ZEO, a second `tpc_begin` of the open transaction
    /// does nothing. A `tpc_begin` of another transaction leaves the open
    /// one as it is: a later call of the other transaction fails, because
    /// it is not the open one.
    fn tpc_begin(&mut self, args: Args<'_>) -> Result<Value, Fault> {
        let Some(id) = args.values.first() else {
            return Err(args.takes("4 to 6 arguments"));
        };
        if self.open.is_none() {
            let mut txn = self.begin(args);
            // The transaction adopts the intent locks of the session.
            if let (Ok(begun), Some(intents)) = (&mut txn, self.intents.as_mut())
                && let Err(error) = begun.adopt_intents(intents)
            {
                txn = Err(error.into());
            }
            // A transaction that does not begin adopts nothing: the intent
            // locks of the session go.
            if txn.is_err()
                && let Some(intents) = self.intents.as_mut()
            {
                intents.release();
            }
            self.open = Some(Open {
                id: id.clone(),
                txn,
                listed: 0,
                native: 0,
                upload: None,
                jobs: Vec::new(),
                of_get: args.values.get(3).is_some_and(of_get),
                undos: Vec::new(),
                voted: false,
            });
        }
        Ok(Value::Nil)
    }

    /// Take the early write-intent lock of each oid of `args`, a list of
    /// oids in ascending order, for the next transaction of the session.
    /// Return the current serial of each oid, in the same order, when the
    /// session holds them all.
    ///
    /// An oid at or below an oid that the session holds fails with
    /// `StorageTransactionError` (`LockOrder`), and the call takes nothing.
    /// A wait past the lock timeout, or the end of the connection, fails
    /// with `ConflictError`. A call in an open transaction, or of a client
    /// that registered read-only, fails.
    fn lock_intent(&mut self, args: Args<'_>) -> Result<Value, Fault> {
        let [oids] = args.exactly::<1>()?;
        if self.session.read_only() {
            return Err(Fault::read_only());
        }
        if self.open.is_some() {
            return Err(Fault::transaction(format!(
                "{} in an open transaction",
                args.method
            )));
        }
        let Value::Array(items) = oids else {
            return Err(Fault::type_error(format!(
                "the oids of {} are not a list",
                args.method
            )));
        };
        let oids = items
            .iter()
            .map(|oid| u64_of(oid).map(Oid))
            .collect::<Result<Vec<Oid>, Fault>>()?;
        let session = &self.session;
        let intents = self.intents.get_or_insert_with(|| {
            let mut intents = IntentLocks::new(Arc::clone(&session.store));
            intents.set_lock_cancel(session.lock_cancel.clone());
            intents
        });
        let serials = intents.lock(&oids)?;
        Ok(Value::Array(
            serials.into_iter().map(|serial| p64(serial.0)).collect(),
        ))
    }

    /// Release every intent lock of the session: the client ended its
    /// transaction before `tpc_begin`.
    fn unlock_intent(&mut self, args: Args<'_>) -> Result<Value, Fault> {
        args.exactly::<0>()?;
        if let Some(intents) = self.intents.as_mut() {
            intents.release();
        }
        Ok(Value::Nil)
    }

    /// Return the core transaction that `args` of `tpc_begin` describe.
    fn begin(&self, args: Args<'_>) -> Result<Txn<Arc<Store>>, Fault> {
        if args.values.len() < 4 || args.values.len() > 6 {
            return Err(args.takes("4 to 6 arguments"));
        }
        if self.session.read_only() {
            return Err(Fault::read_only());
        }
        // A copy gives the status of the copied transaction: " " for a
        // normal one, "p" for one that a pack of FileStorage changed. The
        // store keeps no status, as in `zoe.Storage`.
        if args
            .values
            .get(5)
            .is_some_and(|status| !matches!(status.as_str(), Some(" " | "p")))
        {
            return Err(Fault::transaction("transaction status is not supported"));
        }
        let user = text_bytes(&args.values[1])?;
        let description = text_bytes(&args.values[2])?;
        let ext = extension::pickle(&args.values[3]).map_err(Fault::type_error)?;
        let store = Arc::clone(&self.session.store);
        let mut txn = match args.values.get(4).filter(|tid| !tid.is_nil()) {
            None => Txn::begin(store, &user, &description, &ext),
            Some(tid) => {
                let tid = Tid(u64_of(tid)?);
                Txn::begin_restore(store, tid, &user, &description, &ext)?
            }
        };
        txn.set_lock_cancel(self.session.lock_cancel.clone());
        txn.set_server_writes(self.session.server_writes());
        Ok(txn)
    }

    /// Return the open transaction with the id `id`. Another id fails with
    /// `StorageTransactionError`, whose args are the id of the open
    /// transaction, or None, and `id`.
    pub(super) fn current(&mut self, id: &Value) -> Result<&mut Open, Fault> {
        let current = self.open.as_ref().map(|open| &open.id);
        if current != Some(id) {
            return Err(Fault::not_current(current, id));
        }
        Ok(self.open.as_mut().expect("the open transaction"))
    }

    /// Stage a new version of an object, and queue the encode of its
    /// pickle. The call does not wait for the encode.
    fn storea(&mut self, args: ArgsMut<'_>) -> Result<Value, Fault> {
        let [oid, serial, data, id] = args.exactly::<4>()?;
        let mut staged = None;
        self.current(id)?.stage(|txn| {
            let (oid, serial) = (Oid(u64_of(oid)?), store_serial(serial)?);
            let Value::Binary(data) = data else {
                return Err(Fault::type_error("data is not bytes"));
            };
            txn.store(oid, serial, data)?;
            staged = Some((oid, std::mem::take(data)));
            Ok(())
        });
        if let Some((oid, data)) = staged {
            self.queue(oid, data);
        }
        Ok(Value::Nil)
    }

    /// Queue on the compression pool the encode of `data`, the pickle that
    /// the open transaction staged for `oid`.
    ///
    /// No job is queued without a pool, for a store without compression,
    /// for a deletion, for a pickle in the spill file, and after a vote
    /// that listed conflicts: the finish encodes these pickles. A pickle in
    /// the spill file stays out of memory, and the second vote takes its
    /// locks already.
    fn queue(&mut self, oid: Oid, data: Vec<u8>) {
        let (Some((compressor, runtime)), Some(open)) = (&self.compress, &mut self.open) else {
            return;
        };
        let Ok(txn) = &open.txn else { return };
        let compression = self.session.store.compression();
        if open.listed > 0
            || data.is_empty()
            || compression == Compression::None
            || !txn.stages_in_memory(oid)
        {
            return;
        }
        let job = compressor.queue(runtime, oid, data, compression);
        open.jobs.push(job);
    }

    /// Stage a copy of a record in the restore transaction `id`, a
    /// transaction that `tpc_begin` began with a tid. Nil data copies a
    /// deletion. As in `zoe.Storage`, the record gets the tid of the
    /// transaction, not `serial`, and `prev_txn` is not used. In a
    /// transaction without a tid, the vote fails with
    /// `StorageTransactionError`.
    fn restorea(&mut self, args: ArgsMut<'_>) -> Result<Value, Fault> {
        let [oid, _serial, data, _prev_txn, id] = args.exactly::<5>()?;
        let mut staged = None;
        self.current(id)?.stage(|txn| {
            let oid = Oid(u64_of(oid)?);
            match data {
                Value::Binary(data) => {
                    txn.restore(oid, Some(data))?;
                    staged = Some((oid, std::mem::take(data)));
                }
                Value::Nil => txn.restore(oid, None)?,
                _ => return Err(Fault::type_error("data is not bytes or None")),
            }
            Ok(())
        });
        if let Some((oid, data)) = staged {
            self.queue(oid, data);
        }
        Ok(Value::Nil)
    }

    /// Record that the transaction read `oid` at `serial`. The vote takes
    /// a shared lock on `oid`, and a stale serial fails the vote with
    /// `ReadConflictError`.
    fn check_current_serial(&mut self, args: Args<'_>) -> Result<Value, Fault> {
        let [oid, serial, id] = args.exactly::<3>()?;
        self.current(id)?.stage(|txn| {
            let (oid, serial) = (Oid(u64_of(oid)?), serial_of(serial)?);
            Ok(txn.check_current_serial(oid, serial)?)
        });
        Ok(Value::Nil)
    }

    /// Stage a deletion of `oid`. The vote checks `serial` as it does for
    /// `storea`.
    fn delete_object(&mut self, args: Args<'_>) -> Result<Value, Fault> {
        let [oid, serial, id] = args.exactly::<3>()?;
        self.current(id)?.stage(|txn| {
            let (oid, serial) = (Oid(u64_of(oid)?), serial_of(serial)?);
            Ok(txn.delete(oid, serial)?)
        });
        Ok(Value::Nil)
    }

    /// Return the open transaction, for a call of the transaction that
    /// names none. Without an open transaction, the call fails with
    /// `StorageTransactionError`.
    fn opened(&mut self, method: &str) -> Result<&mut Open, Fault> {
        self.open
            .as_mut()
            .ok_or_else(|| Fault::transaction(format!("{method} outside a transaction")))
    }

    /// Stage a write of a namespace in the open transaction:
    /// `kv_set(flavour, name, key, value, ttl)`. `ttl` is an int or a float
    /// of seconds for `"ephemeral"`, and nil for `"durable"`. The call has
    /// no reply: an error fails the vote, as an error of `storea` does.
    fn kv_set(&mut self, args: Args<'_>) -> Result<Value, Fault> {
        let [flavour, name, key, value, ttl] = args.exactly::<5>()?;
        self.opened(args.method)?.stage(|txn| {
            let flavour = flavour_of(flavour)?;
            let (name, key) = (str_of(name, "name of a namespace")?, bin_of(key, "key")?);
            let value = bin_of(value, "value")?;
            let ttl = ttl_of(ttl)?;
            Ok(txn.kv_set(flavour, name, key, value, ttl)?)
        });
        Ok(Value::Nil)
    }

    /// Stage a delete of a key of a namespace in the open transaction:
    /// `kv_delete(flavour, name, key)`, with the rules of `kv_set`.
    fn kv_delete(&mut self, args: Args<'_>) -> Result<Value, Fault> {
        let [flavour, name, key] = args.exactly::<3>()?;
        self.opened(args.method)?.stage(|txn| {
            let flavour = flavour_of(flavour)?;
            let (name, key) = (str_of(name, "name of a namespace")?, bin_of(key, "key")?);
            Ok(txn.kv_delete(flavour, name, key)?)
        });
        Ok(Value::Nil)
    }

    /// Queue the undo of the transaction of the undo id `trans_id` into the
    /// open transaction `id`. `undoa` has no reply: the first vote stages
    /// the undo, as ZEO's server does, and an error there fails the vote.
    fn undoa(&mut self, args: Args<'_>) -> Result<Value, Fault> {
        let [trans_id, id] = args.exactly::<2>()?;
        self.current(id)?.undos.push(trans_id.clone());
        Ok(Value::Nil)
    }

    /// Commit the transaction. Its reply, the tid, goes out with the
    /// invalidations of the commit: `call` publishes the commit. A finish
    /// that fails publishes nothing. As in ZEO, a finish of a transaction
    /// that is not the open one does nothing and gives None.
    fn tpc_finish(&mut self, call: &Call) -> Outcome {
        let finished = match self.finish(Args::of(call)) {
            Ok(Some(finished)) => finished,
            Ok(None) => return Outcome::Done(Value::Nil),
            Err(fault) => return Outcome::Failed(fault),
        };
        let reply = (!call.oneway).then(|| message::reply(call.id.clone(), p64(finished.tid.0)));
        self.committed = Some((finished, reply));
        Outcome::Published
    }

    /// Commit the open transaction with the id of `args`. Return `None`
    /// when no such transaction is open.
    fn finish(&mut self, args: Args<'_>) -> Result<Option<Finished>, Fault> {
        let [id] = args.exactly::<1>()?;
        if !self.open.as_ref().is_some_and(|open| &open.id == id) {
            return Ok(None);
        }
        let open = self.open.take().expect("the open transaction");
        let Ok(txn) = open.txn else {
            return Err(Fault::transaction(format!("{} before vote", args.method)));
        };
        let classes = open.of_get.then(|| classes_of(&txn));
        // The core returns only after the commit is durable.
        let finished = txn.finish_after()?;
        if let Some(classes) = classes {
            self.session.storage.metrics.add_writes_on_get(classes);
        }
        Ok(Some(finished))
    }

    fn tpc_abort(&mut self, args: Args<'_>) -> Result<Value, Fault> {
        let [id] = args.exactly::<1>()?;
        // As in ZEO, an abort of another transaction does nothing.
        if self.open.as_ref().is_some_and(|open| &open.id == id)
            && let Some(Open { txn: Ok(txn), .. }) = self.open.take()
        {
            txn.abort();
        }
        Ok(Value::Nil)
    }

    /// Open the upload of a blob in the current transaction. A stock client
    /// sends `storea` for the oid first, so a transaction is open.
    ///
    /// After an error of the transaction, the blob calls do nothing, and
    /// the upload writes no file. The vote fails with the first error.
    fn store_blob_start(&mut self, args: Args<'_>) -> Result<Value, Fault> {
        args.exactly::<0>()?;
        let open = self.open.as_mut().ok_or_else(|| {
            Fault::transaction(format!("{}: no current transaction", args.method))
        })?;
        if open.txn.is_err() {
            return Ok(Value::Nil);
        }
        if open.upload.is_some() {
            return Err(Fault::value_error(format!(
                "{}: an upload is open",
                args.method
            )));
        }
        // The blob file of the core goes through the `Vfs` of the store.
        let upload = self
            .session
            .store
            .new_blob_file()
            .map_err(|error| match error {
                ZoeError::Io(error) => {
                    Fault::storage(format!("cannot create an upload file: {error}"))
                }
                error => Fault::from(error),
            })?;
        open.upload = Some(upload);
        Ok(Value::Nil)
    }

    /// Append a chunk to the upload of the current transaction.
    fn store_blob_chunk(&mut self, args: Args<'_>) -> Result<Value, Fault> {
        let [chunk] = args.exactly::<1>()?;
        let Value::Binary(chunk) = chunk else {
            return Err(Fault::type_error("a chunk is not bytes"));
        };
        let Some(open) = self.open.as_mut() else {
            return Err(Fault::value_error(format!(
                "{}: no upload is open",
                args.method
            )));
        };
        if open.txn.is_err() {
            return Ok(Value::Nil);
        }
        let upload = open
            .upload
            .as_mut()
            .ok_or_else(|| Fault::value_error(format!("{}: no upload is open", args.method)))?;
        std::io::Write::write_all(upload, chunk)
            .map_err(|error| Fault::storage(format!("cannot write an upload file: {error}")))?;
        Ok(Value::Nil)
    }

    /// Stage the uploaded blob with `data` for `oid` with `Txn::store_blob`,
    /// which syncs the file. A stock client sent `storea` for the oid
    /// before, and this store keeps the pickle and adds the blob. An error
    /// of the stage fails the vote, as an error of `storea` does.
    fn store_blob_end(&mut self, args: Args<'_>) -> Result<Value, Fault> {
        let [oid, serial, data, id] = args.exactly::<4>()?;
        let open = self.current(id)?;
        // Dropping the upload removes a file that the core did not take.
        let upload = open.upload.take();
        if open.txn.is_err() {
            return Ok(Value::Nil);
        }
        let upload = upload
            .ok_or_else(|| Fault::value_error(format!("{}: no upload is open", args.method)))?;
        open.stage(|txn| {
            let (oid, serial) = (Oid(u64_of(oid)?), store_serial(serial)?);
            let Value::Binary(data) = data else {
                return Err(Fault::type_error("data is not bytes"));
            };
            Ok(txn.store_blob(oid, serial, data, upload)?)
        });
        Ok(Value::Nil)
    }

    /// Stage the blob in the file `filename` of the directory of `oid` with
    /// `data`, for a client that shares the blob directory of the store.
    /// `Txn::store_blob_shared` takes the file by rename. `filename` must
    /// be a name that ends in `.tmp` or `.tmp-`, never a path.
    ///
    /// The call has no reply. Without `--shared-blob-dir`, a name that the
    /// core refuses, and each other error of the stage fail the vote, as an
    /// error of `storea` does. Then the file stays in its place.
    fn store_blob_shared(&mut self, args: Args<'_>) -> Result<Value, Fault> {
        let [oid, serial, data, filename, id] = args.exactly::<5>()?;
        let shared = self.session.storage.shared_blob_dir;
        self.current(id)?.stage(|txn| {
            if !shared {
                return Err(Fault::storage(format!(
                    "{} needs zoe serve --shared-blob-dir \
                     (ZOE_SHARED_BLOB_DIR): this server does not share its blob \
                     directory with its clients",
                    args.method
                )));
            }
            let (oid, serial) = (Oid(u64_of(oid)?), store_serial(serial)?);
            let Value::Binary(data) = data else {
                return Err(Fault::type_error("data is not bytes"));
            };
            let name = match filename {
                Value::String(text) => text.as_str(),
                Value::Binary(bytes) => std::str::from_utf8(bytes).ok(),
                _ => None,
            }
            .ok_or_else(|| Fault::type_error("the blob file name is not a str"))?;
            Ok(txn.store_blob_shared(oid, serial, data, name)?)
        });
        Ok(Value::Nil)
    }
}

impl Drop for Transaction {
    /// A transaction that is still open at the end of a connection counts
    /// as an abort, as in ZEO. The drop of the core transaction aborts it.
    fn drop(&mut self) {
        if let Some(open) = &self.open {
            if open.listed > 0 {
                Stats::add(&self.session.storage.metrics.conflicts_raised);
            }
            let stats = &self.session.storage.stats;
            Stats::sub(&stats.active_txns);
            Stats::add(&stats.aborts);
        }
    }
}
