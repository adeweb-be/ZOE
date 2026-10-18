//! The calls of one connection.
//!
//! A `Session` holds the state of a connection that all its calls share.
//! The calls that are not calls of the transaction run on many threads at
//! once. A `Transaction` holds the open transaction of the connection. One
//! thread holds it and runs the calls of the transaction in order. That
//! thread begins the core transaction and makes every later call of it, so
//! the core never refuses a call with `WrongThread`.
//!
//! `register` adds the connection to the invalidations of the storage. A
//! commit goes to the invalidations with the reply of its `tpc_finish`, so
//! the reply and the invalidations reach each client in commit order.
//!
//! Each call runs in a `connection` span of its own (`Session::span`): its
//! fields `storage` and `client` go into each log line of the call, the
//! log lines of the core too. The client is the label of
//! `set_client_label` when the call starts, or the address of the peer.
//! Inside it, the call runs in the span of the traces (`Session::request`).
//!
//! A transaction of a connection encodes its staged pickles on the
//! compression pool (`compress`): `storea` and `restorea` queue a job, and
//! the first vote waits for the jobs before it takes a lock.
//!
//! The extension `kv` gives the KV namespaces of the store (ADR 0043).
//! `kv_get`, `kv_scan` and `kv_generation` read on the read pool, outside
//! the transaction. `kv_set` and `kv_delete` stage a write in the open
//! transaction of the connection, as `storea` stages a record.
//!
//! `undoa` queues an undo into the open transaction, and the first vote
//! stages it before it takes a lock, as ZEO's server does. An undo that no
//! tier of the core merges fails the vote with `UndoError`: no Python runs
//! on the server.

use std::collections::HashSet;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use rmpv::Value;
use zoe_core::{
    BlobFile, Compression, Finished, Flavour, IntentLocks, LockCancel, Oid, Store, Tid, Txn,
    TxnMeta, ZoeError, undo_id, undo_tid,
};

use super::blob::Download;
use super::clock;
use super::compress::{Compressor, Failed, Job};
use super::extension;
use super::invalidation::{ClientId, Commit, Others};
use super::iteration::{self, Iterators, Refused};
use super::message::{self, Call};
use super::negotiation;
use super::outbox::{Outbox, Parts, Room};
use super::status::Stats;
use super::{NamedStorage, Stopping, Storages};
use crate::runtime::Runtime;

/// The longest client label that a log line takes, in characters.
const MAX_LABEL: usize = 200;

/// The most oids that one `new_oids` call gives, as in ZEO.
const MAX_NEW_OIDS: u64 = 100;

/// The reads of a `loadBefore` that can each meet a publication before
/// the call reads under the mutex of the hub.
const LOAD_TRIES: usize = 3;

/// The address and the length of each record that `Store::load_before`
/// gave to a `loadBefore` of the tests. A test finds there the buffer of
/// the store, and then looks for it at the socket.
#[cfg(test)]
pub(crate) static LOADED: Mutex<Vec<(usize, usize)>> = Mutex::new(Vec::new());

/// The class of a conflict error of ZODB.
const CONFLICT_ERROR: &str = "ZODB.POSException.ConflictError";

/// The class of a read conflict error of ZODB.
const READ_CONFLICT_ERROR: &str = "ZODB.POSException.ReadConflictError";

/// The class of a transaction error of ZODB.
const TRANSACTION_ERROR: &str = "ZODB.POSException.StorageTransactionError";

/// The class of the undo errors of ZODB that a stock client rebuilds: a
/// `MultipleUndoErrors` is an `UndoError`, and the factories of
/// `ZEO.asyncio.client` know only it.
const UNDO_ERRORS: &str = "ZODB.POSException.MultipleUndoErrors";

/// The reason of an undo that a later transaction stops, as in FileStorage.
const CHANGED_LATER: &str = "Some data were modified by a later transaction";

/// The most transactions that one read of the undo log takes.
const UNDO_LOG_CHUNK: usize = 256;

/// The text of a conflict error of ZODB.
const CONFLICT: &str = "database conflict error";

/// The text of a read conflict error of ZODB.
const READ_CONFLICT: &str = "database read conflict error";

/// The interfaces that the store provides, as `(module, name)`, in the
/// order of `providedBy(zoe.Storage(path)).__iro__`. A stock client takes
/// the storage interfaces of this list as its own. The store provides
/// `IExternalGC`, because `deleteObject` writes a deletion, and
/// `IStorageUndoable`.
const INTERFACES: [(&str, &str); 9] = [
    ("ZODB.interfaces", "IBlobStorageRestoreable"),
    ("ZODB.interfaces", "IBlobStorage"),
    ("ZODB.interfaces", "IStorageRestoreable"),
    ("ZODB.interfaces", "IStorageCurrentRecordIteration"),
    ("ZODB.interfaces", "IStorageIteration"),
    ("ZODB.interfaces", "IStorageUndoable"),
    ("ZODB.interfaces", "IExternalGC"),
    ("ZODB.interfaces", "IStorage"),
    ("zope.interface", "Interface"),
];

/// The methods that ZEO counts in `stores`.
const STORES: [&str; 3] = ["storea", "deleteObject", "restorea"];

/// The key of the transaction extension that names the method of the
/// request of the transaction. `zoe.plone` sets it.
const REQUEST_METHOD: &str = "zoe.request_method";

/// Tell if `extension`, the map of the extension of `tpc_begin`, marks the
/// transaction of a GET or a HEAD request.
fn of_get(extension: &Value) -> bool {
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
fn classes_of(txn: &Txn<Arc<Store>>) -> std::collections::BTreeMap<String, u64> {
    let mut classes = std::collections::BTreeMap::new();
    for oid in txn.staged_oids() {
        if let Some(class) = txn.staged_class_name(oid) {
            *classes.entry(class).or_default() += 1;
        }
    }
    classes
}

/// An exception for the client: its class and its arguments.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Fault {
    class: &'static str,
    args: Value,
}

impl Fault {
    fn new(class: &'static str, text: impl Into<String>) -> Self {
        Self {
            class,
            args: Value::Array(vec![Value::from(text.into())]),
        }
    }

    fn value_error(text: impl Into<String>) -> Self {
        Self::new("builtins.ValueError", text)
    }

    fn type_error(text: impl Into<String>) -> Self {
        Self::new("builtins.TypeError", text)
    }

    fn transaction(text: impl Into<String>) -> Self {
        Self::new(TRANSACTION_ERROR, text)
    }

    /// The error of a call for the transaction `given` when `current` is
    /// the open transaction. The args are `(current or None, given)`, as
    /// ZEO gives them.
    fn not_current(current: Option<&Value>, given: &Value) -> Self {
        Self {
            class: TRANSACTION_ERROR,
            args: Value::Array(vec![current.cloned().unwrap_or(Value::Nil), given.clone()]),
        }
    }

    fn read_only() -> Self {
        Self {
            class: "ZODB.POSException.ReadOnlyError",
            args: Value::Array(vec![]),
        }
    }

    fn storage(text: impl Into<String>) -> Self {
        Self::new("ZODB.POSException.StorageError", text)
    }

    /// An undo that `reason` refuses, on `oid` when an object stops it. A
    /// stock client rebuilds it as a `MultipleUndoErrors` of one error,
    /// which is an `UndoError`. Each error is `(oid, reason)`, as in the
    /// `MultipleUndoErrors` of FileStorage.
    fn undo(reason: &str, oid: Option<Oid>) -> Self {
        let oid = oid.map_or(Value::Nil, |oid| p64(oid.0));
        let error = Value::Array(vec![oid, Value::from(reason)]);
        Self {
            class: UNDO_ERRORS,
            args: Value::Map(vec![(Value::from("_errs"), Value::Array(vec![error]))]),
        }
    }

    /// The error of a call that names no object `oid`: a `POSKeyError` of
    /// the oid as the client sent it.
    fn no_object(oid: Value) -> Self {
        Self {
            class: "ZODB.POSException.POSKeyError",
            args: Value::Array(vec![oid]),
        }
    }

    /// The error of a call that names no iterator `id`: a `KeyError` of the
    /// id, as ZEO gives it.
    fn no_iterator(id: &Value) -> Self {
        Self {
            class: "builtins.KeyError",
            args: Value::Array(vec![id.clone()]),
        }
    }

    /// The error of a call on the iterator `id` that the iterators refused.
    fn of_iterator(refused: Refused, id: &Value) -> Self {
        match refused {
            Refused::Unknown => Self::no_iterator(id),
            Refused::OutOfOrder(tid) => Self::new(
                "builtins.Exception",
                format!(
                    "Out-of-order request for record iterator for transaction {:016x}",
                    tid.0
                ),
            ),
            Refused::Full => Self::storage(format!(
                "a connection keeps at most {} open iterators; iterator_gc or a new connection frees them",
                iteration::MAX_OPEN
            )),
        }
    }

    /// Return the exception of ZODB for an error of a pack, as
    /// `zoe.Storage.pack` raises it. A record that fails its checksum or
    /// does not decode stops the pack with `StorageSystemError`. A pack
    /// that the stop of the server stopped fails with `StorageError`.
    fn of_pack(error: &ZoeError) -> Self {
        match error {
            ZoeError::CorruptRecord { .. } => Self::new(
                "ZODB.POSException.StorageSystemError",
                format!("pack stopped: {error}"),
            ),
            ZoeError::Format(text) => Self::new(
                "ZODB.POSException.StorageSystemError",
                format!("pack stopped: {text}"),
            ),
            ZoeError::Closed => Self::storage("pack stopped: the server stops"),
            _ => Self::of(error),
        }
    }

    /// Tell if the fault is a conflict. The names of the conflict
    /// exceptions of ZODB end in `ConflictError`, and they all derive from
    /// `ConflictError`.
    fn is_conflict(&self) -> bool {
        self.class.ends_with("ConflictError")
    }

    /// A conflict on `oid`, as ZEO sends it: the `__dict__` of the
    /// exception, with the current serial and the serial that the
    /// transaction read, and the class of the object.
    fn conflict(
        class: &'static str,
        message: &str,
        oid: Oid,
        serials: Option<(Tid, Tid)>,
        class_name: Option<String>,
    ) -> Self {
        let serials = serials.map_or(Value::Nil, |(current, read)| {
            Value::Array(vec![p64(current.0), p64(read.0)])
        });
        Self {
            class,
            args: Value::Map(vec![
                (Value::from("message"), Value::from(message)),
                (Value::from("oid"), p64(oid.0)),
                (
                    Value::from("class_name"),
                    class_name.map_or(Value::Nil, Value::from),
                ),
                (Value::from("serials"), serials),
            ]),
        }
    }

    /// Return the exception of ZODB for an error of the vote of `txn`. A
    /// conflict carries the serials of the stale oid. A `ConflictError`
    /// also carries the class of the object that `txn` stored, and a
    /// `ReadConflictError` has none, as in ZEO.
    fn of_vote(error: &ZoeError, txn: &Txn<Arc<Store>>) -> Self {
        match *error {
            ZoeError::Conflict { oid } => Self::conflict(
                CONFLICT_ERROR,
                CONFLICT,
                oid,
                txn.conflict_serials(),
                txn.staged_class_name(oid),
            ),
            ZoeError::ReadConflict { oid } => Self::conflict(
                READ_CONFLICT_ERROR,
                READ_CONFLICT,
                oid,
                txn.conflict_serials(),
                None,
            ),
            _ => Self::of(error),
        }
    }

    /// Return the exception of ZODB for `error`.
    fn of(error: &ZoeError) -> Self {
        let text = error.to_string();
        match *error {
            ZoeError::NotFound { oid } => Self::no_object(p64(oid.0)),
            ZoeError::Conflict { oid } => Self::conflict(CONFLICT_ERROR, CONFLICT, oid, None, None),
            ZoeError::ReadConflict { oid } => {
                Self::conflict(READ_CONFLICT_ERROR, READ_CONFLICT, oid, None, None)
            }
            ZoeError::ReadOnly => Self::read_only(),
            ZoeError::Undo { oid, reason } => Self::undo(reason, oid),
            ZoeError::Protocol(_) | ZoeError::LockOrder { .. } | ZoeError::WrongThread => {
                Self::transaction(text)
            }
            ZoeError::InvalidKv(_) => Self::value_error(text),
            ZoeError::Halted
            | ZoeError::CorruptRecord { .. }
            | ZoeError::CorruptBlob { .. }
            | ZoeError::CorruptTxn { .. }
            | ZoeError::CorruptSequence { .. }
            | ZoeError::CorruptKv { .. }
            | ZoeError::Backend(_) => Self::new("ZODB.POSException.StorageSystemError", text),
            _ => Self::new("ZODB.POSException.StorageError", text),
        }
    }
}

impl From<ZoeError> for Fault {
    fn from(error: ZoeError) -> Self {
        Self::of(&error)
    }
}

/// What a call gives back.
#[derive(Debug)]
pub(crate) enum Outcome {
    /// The result of the call.
    Done(Value),
    /// A `tpc_finish` that committed. Its reply went out with the
    /// invalidations of the commit.
    Published,
    /// A load that found a record. The call put its reply in the session
    /// queue itself, with the buffer of the record in it, so the outcome
    /// holds no copy of the record.
    Sent,
    /// The client is gone. The connection ends.
    Gone,
    /// The exception of the call.
    Failed(Fault),
    /// The server closes the connection, as ZEO does for a call it does not
    /// know.
    Close(String),
}

impl From<Result<Value, Fault>> for Outcome {
    fn from(result: Result<Value, Fault>) -> Self {
        match result {
            Ok(value) => Self::Done(value),
            Err(fault) => Self::Failed(fault),
        }
    }
}

/// The way of the reply of one call to its client: the room that the call
/// took in the session queue before it ran. A call can also put its reply
/// in the queue itself, while it runs.
pub(crate) struct Reply {
    room: Option<Room>,
    /// The call put its reply itself. False when the connection was closed.
    sent: Option<bool>,
    /// The bytes of the reply that the call put itself, for the tests.
    #[cfg(test)]
    frame: Option<Vec<u8>>,
}

impl Reply {
    /// The reply of a call that took `room`.
    pub(crate) fn new(room: Room) -> Self {
        Self {
            room: Some(room),
            sent: None,
            #[cfg(test)]
            frame: None,
        }
    }

    /// Keep a copy of the bytes of the frame `parts` that the call puts
    /// itself, and give `parts` back.
    #[cfg(test)]
    fn keep(&mut self, parts: Option<Parts>) -> Option<Parts> {
        self.frame = parts.as_ref().map(|parts| parts.concat());
        parts
    }

    /// Put the reply of `call`, whose outcome is `outcome`, in the queue of
    /// `outbox`. Return false when the connection ends: the client is gone,
    /// the call closes the connection, or a call without a reply failed.
    ///
    /// A call without a reply has no way to report its error, so the server
    /// closes the connection, as ZEO does.
    pub(crate) fn send(self, call: &Call, outcome: Outcome, outbox: &Outbox) -> bool {
        let frame = match outcome {
            Outcome::Published | Outcome::Sent => None,
            Outcome::Gone => return false,
            Outcome::Done(_) if call.oneway => None,
            // The text that a client sends goes only into fields, never
            // into the message. The text format quotes and escapes a field
            // of text, so a line feed in it cannot add a log line.
            Outcome::Failed(fault) if call.oneway => {
                let args = fault.args.to_string();
                tracing::warn!(
                    method = call.method.as_str(),
                    class = fault.class,
                    args = args.as_str(),
                    "a call without a reply failed: closing the connection"
                );
                return false;
            }
            Outcome::Close(why) => {
                tracing::warn!(reason = why.as_str(), "closing the connection");
                return false;
            }
            // The call put its reply itself.
            Outcome::Done(_) | Outcome::Failed(_) if self.sent.is_some() => None,
            Outcome::Done(value) => Some(message::reply(call.id.clone(), value)),
            Outcome::Failed(fault) => {
                Some(message::error(call.id.clone(), fault.class, fault.args))
            }
        };
        match (self.sent, frame) {
            (Some(sent), _) => sent,
            (None, Some(frame)) => match self.room {
                Some(room) => room.put(Some(frame)),
                // A download used the room of its call for its first frame.
                None => outbox.send(frame),
            },
            (None, None) => true,
        }
    }
}

/// The calls of the open transaction of a connection. One thread runs
/// them in order, from `tpc_begin`, or from the first `lock_intent` before
/// it, to the end of the transaction.
const TRANSACTION_CALLS: [&str; 17] = [
    "lock_intent",
    "unlock_intent",
    "tpc_begin",
    "storea",
    "restorea",
    "checkCurrentSerialInTransaction",
    "deleteObject",
    "kv_set",
    "kv_delete",
    "undoa",
    "vote",
    "tpc_finish",
    "tpc_abort",
    "storeBlobStart",
    "storeBlobChunk",
    "storeBlobEnd",
    "storeBlobShared",
];

/// The longest time that a session holds intent locks without a
/// `tpc_begin`, by default. Then the server releases them (ADR 0045).
pub(crate) const INTENT_HOLD: Duration = Duration::from_secs(60);

/// Tell if `method` is a call of the transaction of the connection.
pub(crate) fn in_transaction(method: &str) -> bool {
    TRANSACTION_CALLS.contains(&method)
}

/// Tell if `method` can run for a long time or wait for the client: a
/// download waits while the client reads, and a pack reads the whole
/// store. Such a call runs on a thread of its own, not on the read pool.
pub(crate) fn runs_long(method: &str) -> bool {
    matches!(method, "sendBlob" | "pack")
}

/// Tell if `method` runs on the read pool, once the connection is
/// registered: a call that is not `set_client_label`, not a call of the
/// transaction and does not run long.
pub(crate) fn on_read_pool(method: &str) -> bool {
    method != "set_client_label" && !in_transaction(method) && !runs_long(method)
}

/// Tell if the reader waits for the end of `method` before it starts the
/// next call of the connection. Each call that starts after
/// `set_client_label` logs with the label. A stock client sends
/// `iterator_gc` with no reply, and a later call for an iterator that it
/// listed must fail with `KeyError`.
pub(crate) fn ends_before_next(method: &str) -> bool {
    matches!(method, "set_client_label" | "iterator_gc")
}

/// The open transaction of a connection.
struct Open {
    /// The id that the client gave to the transaction.
    id: Value,
    /// The core transaction, or the first error of the transaction: an
    /// error of `tpc_begin` or of a call without a reply. The vote fails
    /// with that error, as ZEO reports these errors at the vote. The error
    /// takes the place of the core transaction, so the core transaction
    /// drops, and that aborts it.
    txn: Result<Txn<Arc<Store>>, Fault>,
    /// The number of conflicts that the last vote listed for the client to
    /// resolve. When it is not 0, the next vote is the second vote.
    listed: u64,
    /// The number of oids that tier 1 merged in the first vote when that
    /// vote listed conflicts too. The second vote that passes counts them.
    native: u64,
    /// The blob file that the client uploads, from `storeBlobStart` to
    /// `storeBlobEnd`. The end of the transaction drops it, and that
    /// removes its file.
    upload: Option<BlobFile>,
    /// The jobs of the compression pool that encode the staged pickles,
    /// until the first vote. The end of the transaction drops them, and
    /// that cancels them.
    jobs: Vec<Job>,
    /// True for the transaction of a GET or a HEAD request: its commit
    /// counts its records by class in the writes-on-GET report.
    of_get: bool,
    /// The undo ids that `undoa` queued, in order. The first vote stages
    /// their undos.
    undos: Vec<Value>,
    /// True after a vote that passed: the transaction holds its locks
    /// until it ends. The transaction timeout counts from then.
    voted: bool,
}

impl Open {
    /// Run `step` on the core transaction. An error of `step` ends the core
    /// transaction and fails the vote. After an error, `step` does not run.
    fn stage(&mut self, step: impl FnOnce(&mut Txn<Arc<Store>>) -> Result<(), Fault>) {
        if let Ok(txn) = &mut self.txn
            && let Err(fault) = step(txn)
        {
            self.txn = Err(fault);
        }
    }
}

/// What `register` gave to a connection.
struct Registration {
    /// The id of the connection in the invalidations of the storage.
    client: ClientId,
    read_only: bool,
    /// The extensions that the server accepted from the declaration of
    /// the client, or none when the client declared nothing.
    extensions: Option<Vec<&'static str>>,
}

/// The state of one connection that all its calls share. The calls that
/// are not calls of the transaction run on many threads at once.
pub(crate) struct Session {
    storage: Arc<NamedStorage>,
    /// The store of `storage`.
    store: Arc<Store>,
    /// The frames that wait for the client.
    outbox: Outbox,
    /// The registration of the connection, from `register` on.
    registration: OnceLock<Registration>,
    /// The iterators of the connection. One call at a time uses them.
    iterators: Mutex<Iterators>,
    /// The label that the client gave with `set_client_label`, for the log
    /// lines of the calls that start after it.
    label: Mutex<Option<String>>,
    /// The stop of the server. A pack stops between two batches when the
    /// server stops.
    stopping: Stopping,
    /// The end of the connection ends the lock wait of each transaction of
    /// the session, also a wait that starts after the end.
    lock_cancel: LockCancel,
    /// True when the client may register read-only only, as the holder of
    /// a certificate of the read-only client CA.
    read_only_client: bool,
}

/// The open transaction of one connection. One thread holds it and runs
/// the calls of the transaction in order. That thread begins the core
/// transaction and makes every later call of it, so the core never refuses
/// a call with `WrongThread`.
pub(crate) struct Transaction {
    session: Arc<Session>,
    open: Option<Open>,
    /// The early write-intent locks of the session (ADR 0045), from its
    /// first `lock_intent`. The next `tpc_begin` adopts them.
    intents: Option<IntentLocks<Arc<Store>>>,
    /// The compression pool and the runtime of its jobs. Without them, the
    /// finish encodes each staged pickle.
    compress: Option<(Compressor, Arc<dyn Runtime>)>,
}

/// Return the storage that `call` registers with, before the connection
/// has one. Only `register` can come first, and it must name one of
/// `storages`. Otherwise, return the outcome of the call: a call before
/// `register` closes the connection, and an unknown name fails the call,
/// as in ZEO.
pub(crate) fn select(storages: &Storages, call: &Call) -> Result<Arc<NamedStorage>, Outcome> {
    if call.method != "register" {
        return Err(Outcome::Close(format!("{} before register", call.method)));
    }
    let (name, _, _) = register_args(&call.args).map_err(Outcome::Failed)?;
    let name = storage_name(name);
    storages
        .get(&name)
        .cloned()
        .ok_or_else(|| Outcome::Failed(Fault::value_error(format!("unknown storage: {name}"))))
}

/// Return the arguments of `register`: the storage name, the read-only
/// flag and the declaration of the extensions. A stock client gives no
/// declaration.
fn register_args(args: &[Value]) -> Result<(&Value, &Value, Option<&Value>), Fault> {
    match args {
        [storage, read_only] => Ok((storage, read_only, None)),
        [storage, read_only, declaration] => Ok((storage, read_only, Some(declaration))),
        _ => Err(Fault::type_error("register takes 2 or 3 arguments")),
    }
}

/// Return the storage name that a client gives to `register`.
fn storage_name(value: &Value) -> String {
    match value {
        Value::String(text) => text.as_str().unwrap_or_default().to_owned(),
        Value::Binary(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        other => other.to_string(),
    }
}

impl Session {
    /// Return the session of a connection to `storage`, whose frames go to
    /// `outbox`. `stopping` is the stop of the server.
    pub(crate) fn new(storage: Arc<NamedStorage>, outbox: Outbox, stopping: Stopping) -> Self {
        Self {
            store: Arc::clone(&storage.store),
            storage,
            outbox,
            registration: OnceLock::new(),
            iterators: Mutex::new(Iterators::default()),
            label: Mutex::new(None),
            stopping,
            lock_cancel: LockCancel::new(),
            read_only_client: false,
        }
    }

    /// End the lock wait of the vote of the session, and each later one.
    /// The vote then fails, and its transaction releases the locks that it
    /// took. The end of the connection calls it.
    pub(crate) fn end_lock_wait(&self) {
        self.lock_cancel.cancel();
    }

    /// Let the client register read-only only, when `read_only` is true.
    pub(crate) fn read_only_client(mut self, read_only: bool) -> Self {
        self.read_only_client = read_only;
        self
    }

    /// Return the label that the client gave, if any.
    pub(crate) fn label(&self) -> Option<String> {
        self.label
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Return the span of a call of this session: the client, the label or
    /// else `peer`, and the storage.
    pub(crate) fn span(&self, peer: &str) -> tracing::Span {
        let label = self.label();
        let client = label.as_deref().unwrap_or(peer);
        tracing::info_span!("connection", client, storage = self.storage.name.as_str())
    }

    /// Return the span of `call` in the traces: a span named after its
    /// method, with the storage of the session, in the trace of the
    /// traceparent of the call, if it has one. Enter it inside the span of
    /// the connection.
    pub(crate) fn request(&self, call: &Call) -> tracing::Span {
        crate::telemetry::request(&call.method, &self.storage.name, call.traceparent())
    }

    /// Tell if `register` selected the storage of the session.
    pub(crate) fn is_registered(&self) -> bool {
        self.registration.get().is_some()
    }

    /// Tell if the server accepted `trace_context` from the declaration of
    /// the client. Only then can a call of the connection have a trace
    /// context.
    pub(crate) fn traces(&self) -> bool {
        self.registration
            .get()
            .and_then(|r| r.extensions.as_deref())
            .is_some_and(|accepted| accepted.contains(&negotiation::TRACE_CONTEXT))
    }

    /// Tell if the client registered read-only.
    fn read_only(&self) -> bool {
        self.registration.get().is_some_and(|r| r.read_only)
    }

    /// Return the error of a call to a method of an extension that the
    /// connection did not declare. The connection stays open.
    fn undeclared(&self, method: &str) -> Option<Fault> {
        let accepted = self
            .registration
            .get()
            .and_then(|r| r.extensions.as_deref());
        negotiation::refused(method, accepted, negotiation::SERVED).map(Fault::value_error)
    }

    /// The id of the connection in the invalidations of the storage.
    fn client(&self) -> Option<ClientId> {
        self.registration.get().map(|r| r.client)
    }

    /// The iterators of the connection.
    fn iterators(&self) -> MutexGuard<'_, Iterators> {
        self.iterators
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// The frames that wait for the client.
    pub(crate) fn outbox(&self) -> &Outbox {
        &self.outbox
    }

    /// Run one call that is not a call of the transaction, and count it in
    /// the status and the metrics of the storage. The call puts its reply
    /// with `reply`, or takes a place there.
    pub(crate) fn call(&self, call: &Call, reply: &mut Reply) -> Outcome {
        let started = Instant::now();
        let outcome = self.dispatch(call, reply);
        if call.method == "loadBefore" {
            let metrics = &self.storage.metrics;
            metrics.load_latency.observe_duration(started.elapsed());
        }
        self.count(&call.method, &outcome);
        outcome
    }

    /// Count a call that ran, as ZEO counts it.
    fn count(&self, method: &str, outcome: &Outcome) {
        let stats = &self.storage.stats;
        let counter = match outcome {
            Outcome::Close(_) | Outcome::Gone => None,
            Outcome::Done(_) if method == "register" => Some(&stats.connections),
            _ if method == "loadBefore" => Some(&stats.loads),
            _ => None,
        };
        if let Some(counter) = counter {
            Stats::add(counter);
        }
    }

    /// Run one call.
    fn dispatch(&self, call: &Call, reply: &mut Reply) -> Outcome {
        let args = call.args.as_slice();
        if !self.is_registered() {
            return match call.method.as_str() {
                "register" => self.register(args).into(),
                method => Outcome::Close(format!("{method} before register")),
            };
        }
        if let Some(fault) = self.undeclared(&call.method) {
            return Outcome::Failed(fault);
        }
        let result = match call.method.as_str() {
            "register" => Err(Fault::value_error("duplicate register() call")),
            "lastTransaction" => Ok(p64(self.storage.invalidations.published().0)),
            "get_info" => arity::<0>(args, "get_info").and_then(|_| self.info()),
            "get_size_info" => arity::<0>(args, "get_size_info")
                .and_then(|_| self.sizes())
                .map(|items| Value::Map(items.into())),
            "server_status" => {
                arity::<0>(args, "server_status").map(|_| self.storage.status().to_value())
            }
            "ping" => arity::<0>(args, "ping").map(|_| Value::Nil),
            // ZEO puts the label in its log lines, and so does the server.
            "set_client_label" => arity::<1>(args, "set_client_label").map(|[label]| {
                // An empty label gives the address of the peer back.
                *self.label.lock().unwrap_or_else(PoisonError::into_inner) =
                    Some(client_label(label)).filter(|label| !label.trim().is_empty());
                Value::Nil
            }),
            "getTid" => self.get_tid(args),
            "new_oids" => self.new_oids(args),
            "loadBefore" => return self.load_before(call, reply),
            "loadSerial" => self.load_serial(args),
            "getInvalidations" => self.get_invalidations(args),
            "history" => self.history(args),
            "undoLog" => self.undo_log(args),
            "undoInfo" => self.undo_info(args),
            "record_iternext" => self.record_iternext(args),
            "iterator_start" => self.iterator_start(args),
            "iterator_next" => self.iterator_next(args),
            "iterator_record_start" => self.iterator_record_start(args),
            "iterator_record_next" => self.iterator_record_next(args),
            "iterator_gc" => self.iterator_gc(args),
            "pack" => self.pack(args),
            "sendBlob" => return self.send_blob(args, reply),
            "sequence_next" => self.sequence_next(args),
            "kv_get" => self.kv_get(args),
            "kv_scan" => self.kv_scan(args),
            "kv_generation" => self.kv_generation(args),
            method => return Outcome::Close(format!("unknown method {method}")),
        };
        result.into()
    }

    /// Return the value of a key of a namespace, or nil: `kv_get(flavour,
    /// name, key)`. The read gives the last committed value, and an expired
    /// key reads as nil. It runs outside the transaction of the connection.
    fn kv_get(&self, args: &[Value]) -> Result<Value, Fault> {
        let [flavour, name, key] = arity::<3>(args, "kv_get")?;
        let (flavour, name, key) = (flavour_of(flavour)?, str_of(name)?, bin_of(key, "key")?);
        let value = self.store.kv_get(flavour, name, key)?;
        Ok(value.map_or(Value::Nil, Value::Binary))
    }

    /// Return the pairs of a namespace in the byte order of the keys:
    /// `kv_scan(flavour, name, start, stop, limit)`. `start` is included and
    /// `stop` excluded, and nil is an open end. The reply holds at most
    /// `limit` pairs, and at most 1,000, as `[[key, value], ...]`.
    fn kv_scan(&self, args: &[Value]) -> Result<Value, Fault> {
        let [flavour, name, start, stop, limit] = arity::<5>(args, "kv_scan")?;
        let flavour = flavour_of(flavour)?;
        let name = str_of(name)?;
        let start = optional_bin(start, "start")?;
        let stop = optional_bin(stop, "stop")?;
        // A negative limit is a value error, as in `zoe.Storage`.
        let limit = match limit {
            Value::Integer(n) => match n.as_u64() {
                Some(n) => usize::try_from(n).unwrap_or(usize::MAX),
                None => return Err(Fault::value_error("the limit of a scan is less than 1")),
            },
            _ => return Err(Fault::type_error("the limit of a scan is not an int")),
        };
        let pairs = self.store.kv_scan(flavour, name, start, stop, limit)?;
        Ok(Value::Array(
            pairs
                .into_iter()
                .map(|(key, value)| Value::Array(vec![Value::Binary(key), Value::Binary(value)]))
                .collect(),
        ))
    }

    /// Return the generation of a durable namespace, the tid of the last
    /// commit that wrote it, as 8 bytes: `kv_generation(name)`. A namespace
    /// that no commit wrote has the zero tid.
    fn kv_generation(&self, args: &[Value]) -> Result<Value, Fault> {
        let [name] = arity::<1>(args, "kv_generation")?;
        Ok(p64(self.store.kv_generation(str_of(name)?)?.0))
    }

    /// Return the next value of the sequence `name` of the store (F29),
    /// for a connection that declared the extension `sequences`. The value
    /// is not part of a transaction: an abort does not give it back. A
    /// read-only connection fails with `ReadOnlyError`, as `zoe.Storage`
    /// does. Every 100 values of a name, the call waits for one write of
    /// the backend.
    fn sequence_next(&self, args: &[Value]) -> Result<Value, Fault> {
        let [name] = arity::<1>(args, "sequence_next")?;
        let name = name
            .as_str()
            .ok_or_else(|| Fault::type_error("the name of a sequence is not a str"))?;
        if self.read_only() {
            return Err(Fault::read_only());
        }
        Ok(Value::from(self.store.sequence(name).next()?))
    }

    /// Register the connection with its storage. A ZOE client declares the
    /// extensions that it wants in a third argument. The server keeps the
    /// names that it serves and leaves out the other names.
    fn register(&self, args: &[Value]) -> Result<Value, Fault> {
        let (storage, read_only, declaration) = register_args(args)?;
        let storage = storage_name(storage);
        if storage != self.storage.name {
            return Err(Fault::value_error(format!("unknown storage: {storage}")));
        }
        let extensions = declaration
            .map(|value| {
                let names = negotiation::declared(value).map_err(Fault::type_error)?;
                Ok::<_, Fault>(negotiation::accept(&names, negotiation::SERVED))
            })
            .transpose()?;
        // A client that asks to write a read-only storage gets
        // `ReadOnlyError`, as from ZEO. So does a read-only client. The
        // connection stays open, so a client with `read_only_fallback`
        // registers again read-only.
        let read_only = truth(read_only);
        if !read_only && (self.storage.read_only || self.read_only_client) {
            return Err(Fault::read_only());
        }
        // The client gets the invalidation of each commit after this tid.
        let (client, published) = self.storage.invalidations.subscribe(self.outbox.clone());
        let registration = Registration {
            client,
            read_only,
            extensions,
        };
        if self.registration.set(registration).is_err() {
            self.storage.invalidations.unsubscribe(client);
            return Err(Fault::value_error("duplicate register() call"));
        }
        Ok(p64(published.0))
    }

    /// Load the record of an oid before a tid: `[data, serial, next serial
    /// or nil]`, or nil.
    ///
    /// The reply shows the commits that the hub published, and no other:
    /// the read takes the record before the last published tid plus 1 at
    /// most, and it gives a next serial only when the hub published it. The
    /// call makes the frame of its reply, then puts it in the session queue
    /// under the mutex of the hub, if the hub published no commit during
    /// the read. So the reply comes after the invalidation of each commit
    /// that it shows, and before the invalidation of each later commit. A
    /// stock client keeps a record without a next serial as the current
    /// one, and needs this order. The writer gets the whole frame at once.
    ///
    /// A publication during the read makes the call read again. After
    /// `LOAD_TRIES` reads, it reads under the mutex of the hub.
    fn load_before(&self, call: &Call, reply: &mut Reply) -> Outcome {
        let parsed = arity::<2>(&call.args, "loadBefore")
            .and_then(|[oid, tid]| Ok((Oid(u64_of(oid)?), Tid(u64_of(tid)?))));
        let (oid, tid) = match parsed {
            Ok(parsed) => parsed,
            Err(fault) => return Outcome::Failed(fault),
        };
        let store = &self.store;
        let read = |published: Tid| -> (Outcome, Option<Parts>) {
            let before = tid.min(Tid(published.0.saturating_add(1)));
            let outcome = match store.load_before(oid, before) {
                Ok(Some(record)) => {
                    #[cfg(test)]
                    LOADED
                        .lock()
                        .unwrap()
                        .push((record.data.as_ptr() as usize, record.data.len()));
                    // The buffer of the record goes into the frame as it
                    // is, with no copy. A call without a reply puts an
                    // empty place.
                    let next = record
                        .next
                        .filter(|next| *next <= published)
                        .map_or(Value::Nil, |next| p64(next.0));
                    let parts = (!call.oneway).then(|| {
                        message::load_reply(&call.id, record.data, &p64(record.serial.0), &next)
                    });
                    return (Outcome::Sent, parts);
                }
                Ok(None) => Outcome::Done(Value::Nil),
                Err(error) => Outcome::Failed(Fault::from(error)),
            };
            let frame = match &outcome {
                _ if call.oneway => None,
                Outcome::Failed(fault) => Some(message::error(
                    call.id.clone(),
                    fault.class,
                    fault.args.clone(),
                )),
                _ => Some(message::reply_of(&call.id, &Value::Nil)),
            };
            (outcome, frame.map(|frame| vec![frame]))
        };
        let invalidations = &self.storage.invalidations;
        let mut room = reply
            .room
            .take()
            .unwrap_or_else(|| self.outbox.blocking_room());
        for _ in 0..LOAD_TRIES {
            let seen = invalidations.published();
            // An error goes into the queue too, so it goes out in the room
            // of the call and never waits for the client.
            let (outcome, frame) = read(seen);
            #[cfg(test)]
            let frame = reply.keep(frame);
            match invalidations.put_since(seen, room, frame) {
                Ok(sent) => {
                    reply.sent = Some(sent);
                    return outcome;
                }
                Err(back) => room = back,
            }
        }
        #[cfg(test)]
        let mut kept = None;
        let (outcome, sent) = invalidations.read_put(room, |published| {
            let (outcome, frame) = read(published);
            #[cfg(test)]
            {
                kept = frame.as_ref().map(|parts| parts.concat());
            }
            (outcome, frame)
        });
        #[cfg(test)]
        {
            reply.frame = kept;
        }
        reply.sent = Some(sent);
        outcome
    }

    /// Send the blob of `oid` at `serial` to the client: `receiveBlobStart`,
    /// the chunks and `receiveBlobStop`. The reply nil comes after them, so
    /// the client has the file when the reply arrives.
    ///
    /// The call first reads the whole blob and checks its size and its
    /// hash. So a blob that does not match its record goes out as an error
    /// reply, never as a chunk.
    ///
    /// The first frame takes the room of the call. Each later frame waits
    /// while more than `outbox::WAITING` bytes of frames of the session
    /// wait, then for room in the session queue, without holding room. So
    /// many downloads of one connection never wait for each other in a
    /// circle.
    fn send_blob(&self, args: &[Value], reply: &mut Reply) -> Outcome {
        let download = arity::<2>(args, "sendBlob").and_then(|[oid, serial]| {
            let (oid, serial) = (Oid(u64_of(oid)?), Tid(u64_of(serial)?));
            self.store.verify_blob(oid, serial)?;
            let reader = self.store.open_blob(oid, serial)?;
            Ok(Download::new(oid, serial, reader))
        });
        let download = match download {
            Ok(download) => download,
            Err(fault) => return Outcome::Failed(fault),
        };
        let send = |frame| match reply.room.take() {
            Some(room) => room.put(Some(frame)),
            None => self.outbox.send(frame),
        };
        match download.send(send) {
            Ok(true) => Outcome::Done(Value::Nil),
            Ok(false) => Outcome::Gone,
            Err(error) => Outcome::Failed(error.into()),
        }
    }

    /// The information that a stock client asks for after `register`. The
    /// name is the directory of the store, as `zoe.Storage.getName` gives
    /// it. The key `extensions` holds the extensions that the server
    /// accepted from the declaration of the client. A client that declared
    /// nothing gets no such key.
    fn info(&self) -> Result<Value, Fault> {
        let interfaces = INTERFACES
            .iter()
            .map(|(module, name)| Value::Array(vec![Value::from(*module), Value::from(*name)]))
            .collect();
        let mut items = Vec::from(self.sizes()?);
        items.extend([
            (
                Value::from("name"),
                Value::from(self.storage.path.display().to_string()),
            ),
            (Value::from("supportsUndo"), Value::Boolean(true)),
            (
                Value::from("supports_record_iternext"),
                Value::Boolean(true),
            ),
            (Value::from("interfaces"), Value::Array(interfaces)),
        ]);
        let accepted = self.registration.get().and_then(|r| r.extensions.as_ref());
        if let Some(accepted) = accepted {
            let names = accepted.iter().map(|name| Value::from(*name)).collect();
            items.push((Value::from("extensions"), Value::Array(names)));
        }
        Ok(Value::Map(items))
    }

    /// The items `length` and `size` of the store. The length is the object
    /// count of the store, as `len(zoe.Storage)` gives it. A read-only
    /// store of format 1 has no count, and its length is 0. The size is
    /// the size of the files in the store directory.
    fn sizes(&self) -> Result<[(Value, Value); 2], Fault> {
        let size = self
            .storage
            .size()
            .map_err(|error| Fault::storage(format!("the size of the store: {error}")))?;
        let length = self.store.object_count().unwrap_or(0);
        Ok([
            (Value::from("length"), Value::from(length)),
            (Value::from("size"), Value::from(size)),
        ])
    }

    fn new_oids(&self, args: &[Value]) -> Result<Value, Fault> {
        let n = match args {
            [] => MAX_NEW_OIDS,
            [n] => n
                .as_i64()
                .ok_or_else(|| Fault::type_error("n is not an int"))?
                .clamp(1, 100) as u64,
            _ => return Err(Fault::type_error("new_oids takes at most 1 argument")),
        };
        if self.read_only() {
            return Err(Fault::read_only());
        }
        let oids = (0..n)
            .map(|_| self.store.new_oid().map(|oid| p64(oid.0)))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Value::Array(oids))
    }

    fn load_serial(&self, args: &[Value]) -> Result<Value, Fault> {
        let [oid, serial] = arity::<2>(args, "loadSerial")?;
        let (oid, serial) = (Oid(u64_of(oid)?), Tid(u64_of(serial)?));
        Ok(Value::Binary(self.store.load_serial(oid, serial)?))
    }

    /// Return at most `size` records of an oid, newest first, as the maps
    /// of FileStorage's `history`. A map has no keys of the transaction
    /// extension.
    ///
    /// An oid that is not 8 bytes names no object: the call fails with
    /// `POSKeyError` before it checks the size, as ZEO's server does.
    fn history(&self, args: &[Value]) -> Result<Value, Fault> {
        let (oid, size) = match args {
            [oid] => (oid, None),
            [oid, size] => (oid, Some(size)),
            _ => return Err(Fault::type_error("history takes 1 or 2 arguments")),
        };
        let oid = Oid(u64_of(oid).map_err(|_| Fault::no_object(oid.clone()))?);
        let size = match size {
            None => 1,
            // A negative size gives no entry, as in FileStorage.
            Some(Value::Integer(size)) => size.as_u64().unwrap_or(0),
            Some(_) => return Err(Fault::type_error("size is not an int")),
        };
        let size = usize::try_from(size).unwrap_or(usize::MAX);
        let entries = self.store.history(oid, size)?;
        Ok(Value::Array(
            entries
                .into_iter()
                .map(|entry| {
                    Value::Map(vec![
                        (Value::from("time"), Value::F64(clock::tid_time(entry.tid))),
                        (Value::from("user_name"), Value::Binary(entry.user)),
                        (Value::from("description"), Value::Binary(entry.description)),
                        (Value::from("tid"), p64(entry.tid.0)),
                        (Value::from("size"), Value::from(entry.size)),
                    ])
                })
                .collect(),
        ))
    }

    /// Return the transactions that an undo can undo, newest first, from
    /// the index `first` up to `last`, `last` excluded, as the maps of
    /// FileStorage's `undoLog`. A negative `last` gives at most `-last`
    /// transactions. The transactions at or before the last pack are not
    /// in the log.
    fn undo_log(&self, args: &[Value]) -> Result<Value, Fault> {
        let (first, last) = match args {
            [] => (0, -20),
            [first] => (int_of(first)?, -20),
            [first, last] => (int_of(first)?, int_of(last)?),
            _ => return Err(Fault::type_error("undoLog takes 0 to 2 arguments")),
        };
        self.undo_entries(first, last, &[])
    }

    /// Return the transactions of `undoLog(first, last)` whose map holds
    /// each key of the map `specification` with its value, as `undoInfo`
    /// of ZODB does.
    fn undo_info(&self, args: &[Value]) -> Result<Value, Fault> {
        let (first, last, specification) = match args {
            [] => (0, -20, None),
            [first] => (int_of(first)?, -20, None),
            [first, last] => (int_of(first)?, int_of(last)?, None),
            [first, last, specification] => (int_of(first)?, int_of(last)?, Some(specification)),
            _ => return Err(Fault::type_error("undoInfo takes 0 to 3 arguments")),
        };
        let specification = match specification {
            None | Some(Value::Nil) => &[][..],
            Some(Value::Map(items)) => &items[..],
            Some(_) => return Err(Fault::type_error("the specification is not a dict")),
        };
        self.undo_entries(first, last, specification)
    }

    /// Return the maps of the undo log from the index `first` up to `last`,
    /// with only the maps that hold each item of `specification`.
    fn undo_entries(
        &self,
        first: i64,
        last: i64,
        specification: &[(Value, Value)],
    ) -> Result<Value, Fault> {
        let last = if last < 0 {
            first.saturating_sub(last)
        } else {
            last
        };
        let first = usize::try_from(first).unwrap_or(0);
        let last = usize::try_from(last).unwrap_or(0);
        let mut found = Vec::new();
        let mut index = 0;
        let mut before = Tid::MAX;
        'log: while index < last {
            let rows = self.store.undo_log(before, UNDO_LOG_CHUNK)?;
            let Some(oldest) = rows.last() else { break };
            before = oldest.tid;
            for row in &rows {
                let entry = undo_entry(row);
                let Value::Map(items) = &entry else {
                    unreachable!("an entry of the undo log is a map")
                };
                // A key that the map does not hold matches None, as in
                // ZODB: `desc.get(k, None) != v`.
                let matches = specification.iter().all(|(key, value)| {
                    let found = items.iter().find(|item| item.0 == *key);
                    python_eq(found.map_or(&Value::Nil, |item| &item.1), value)
                });
                if !matches {
                    continue;
                }
                if index >= first {
                    found.push(entry);
                }
                index += 1;
                if index >= last {
                    break 'log;
                }
            }
        }
        Ok(Value::Array(found))
    }

    /// Return the current record of the first oid at or after the oid of
    /// the call, nil for the first oid: `[oid, tid, data, next oid or
    /// nil]`. With no such oid, fail with `ValueError`, as FileStorage
    /// does.
    fn record_iternext(&self, args: &[Value]) -> Result<Value, Fault> {
        let next = match args {
            [] | [Value::Nil] => None,
            [next] => Some(Oid(u64_of(next)?)),
            _ => return Err(Fault::type_error("record_iternext takes 1 argument")),
        };
        let record = self
            .store
            .record_iternext(next)?
            .ok_or_else(|| Fault::value_error("no object at or after the oid"))?;
        Ok(Value::Array(vec![
            p64(record.oid.0),
            p64(record.tid.0),
            Value::Binary(record.data),
            record.next.map_or(Value::Nil, |oid| p64(oid.0)),
        ]))
    }

    /// Publish a commit of this connection. Every other client gets its
    /// invalidation, with an empty list for a commit that wrote no oid, as
    /// from ZEO. This client gets `info`, then `reply`.
    ///
    /// The oids come from the transaction index of the store. When they do
    /// not read, the hub closes the other connections, so each client
    /// verifies its cache when it connects again. The hub publishes the
    /// commit also when a read panics, so later commits never wait for
    /// this one.
    fn publish(&self, finished: Finished, reply: Option<Vec<u8>>) {
        let mut publication = self.storage.invalidations.prepare(Commit {
            previous: finished.previous,
            tid: finished.tid,
            origin: self.client(),
            outbox: self.outbox.clone(),
            own: reply.into_iter().collect(),
            others: Others::Close,
        });
        let tid = finished.tid;
        let others = match self.store.written_oids(tid, tid, usize::MAX) {
            Ok(Some(oids)) => Others::Send(message::notify(
                "invalidateTransaction",
                vec![
                    p64(tid.0),
                    Value::Array(oids.into_iter().map(|oid| p64(oid.0)).collect()),
                ],
            )),
            Ok(None) => Others::Close,
            Err(error) => {
                tracing::warn!(
                    tid = format_args!("{:016x}", tid.0),
                    "the oids of the commit do not read: {error}"
                );
                Others::Close
            }
        };
        // `info` gives the sizes of the store, as ZEO sends them before the
        // reply. Sizes that do not read leave it out.
        let info = self
            .sizes()
            .ok()
            .map(|sizes| message::notify("info", vec![Value::Map(sizes.into())]));
        let commit = publication.commit();
        commit.others = others;
        if let Some(info) = info {
            commit.own.insert(0, info);
        }
    }

    /// Return the serial of the current record of an oid, as the reads of
    /// the connection see it: the record before the last published tid
    /// plus 1, as `loadBefore` reads. An oid without a record there, and an
    /// oid whose record there is a deletion, fail with `POSKeyError`, as in
    /// FileStorage.
    ///
    /// The reply takes its place after the read, so it comes after the
    /// invalidation of the commit whose tid it gives. The client keeps no
    /// state of the reply, so the reply can come after the invalidation of
    /// a later commit.
    fn get_tid(&self, args: &[Value]) -> Result<Value, Fault> {
        let [oid] = arity::<1>(args, "getTid")?;
        let oid = Oid(u64_of(oid)?);
        let published = self.storage.invalidations.published();
        match self
            .store
            .load_before(oid, Tid(published.0.saturating_add(1)))?
        {
            Some(record) => Ok(p64(record.serial.0)),
            // The oid has only records that the hub did not publish yet.
            None => Err(ZoeError::NotFound { oid }.into()),
        }
    }

    /// Return the last published tid and every oid that a commit after the
    /// tid of the call wrote: `[tid, oids]`, each oid once. The oids come
    /// from the transaction index of the store, so the reply does not
    /// depend on what this process saw.
    ///
    /// Return nil when more than `invalidation-queue-size` transactions
    /// that wrote records come after the tid, or when the index does not
    /// read. The client then verifies its cache. A tid younger than
    /// `invalidation-age` gets every transaction after it, whatever their
    /// number. The index reads in slices, each a snapshot, so a commit
    /// never waits for the read.
    fn get_invalidations(&self, args: &[Value]) -> Result<Value, Fault> {
        let [tid] = arity::<1>(args, "getInvalidations")?;
        let tid = u64_of(tid)?;
        // The connection gets the invalidation of each commit after `last`.
        let last = self.storage.invalidations.published();
        let queues = self.storage.queues;
        // The session reads the clock only when the age is on.
        let limit = if !queues.invalidation_age.is_zero()
            && clock::younger_than(Tid(tid), queues.invalidation_age, SystemTime::now())
        {
            usize::MAX
        } else {
            queues.invalidations
        };
        let oids = match tid.checked_add(1) {
            Some(from) => self.store.written_oids(Tid(from), last, limit),
            None => Ok(Some(Vec::new())),
        };
        Ok(match oids {
            Ok(Some(oids)) => Value::Array(vec![
                p64(last.0),
                Value::Array(oids.into_iter().map(|oid| p64(oid.0)).collect()),
            ]),
            Ok(None) => Value::Nil,
            Err(error) => {
                tracing::warn!("getInvalidations: the transaction index does not read: {error}");
                Value::Nil
            }
        })
    }

    /// Start an iterator over the transactions from the tid `start` to the
    /// tid `stop`, both included. Nil is no bound. Return the id of the
    /// iterator.
    fn iterator_start(&self, args: &[Value]) -> Result<Value, Fault> {
        let [start, stop] = arity::<2>(args, "iterator_start")?;
        let (start, stop) = (optional_tid(start)?, optional_tid(stop)?);
        let transactions = self.store.iterator(start, stop)?;
        let id = self
            .iterators()
            .start(transactions)
            .map_err(|refused| Fault::of_iterator(refused, &Value::Nil))?;
        Ok(Value::from(id))
    }

    /// Return the next transaction of the iterator `id`, as `[tid, status,
    /// user, description, extension]`, or nil at its end. The status is
    /// always `" "`. The extension is the map of the extension bytes that
    /// the store keeps, because a stock client takes the value of the wire
    /// as the extension.
    fn iterator_next(&self, args: &[Value]) -> Result<Value, Fault> {
        let [id] = arity::<1>(args, "iterator_next")?;
        let next = id
            .as_u64()
            .ok_or(Refused::Unknown)
            .and_then(|iid| self.iterators().next_transaction(iid))
            .map_err(|refused| Fault::of_iterator(refused, id))?;
        let txn = match next {
            None => return Ok(Value::Nil),
            Some(Err(error)) => return Err(Fault::of(&error)),
            Some(Ok(txn)) => txn,
        };
        let extension = extension::unpickle(&txn.extension).map_err(|why| {
            Fault::value_error(format!(
                "the extension of the transaction {:016x}: {why}",
                txn.tid.0
            ))
        })?;
        Ok(Value::Array(vec![
            p64(txn.tid.0),
            Value::from(" "),
            Value::Binary(txn.user.clone()),
            Value::Binary(txn.description.clone()),
            extension,
        ]))
    }

    /// Start an iterator over the records of `tid`, the last transaction
    /// that the transaction iterator `id` gave. Return the id of the record
    /// iterator.
    fn iterator_record_start(&self, args: &[Value]) -> Result<Value, Fault> {
        let [id, tid] = arity::<2>(args, "iterator_record_start")?;
        let tid = Tid(u64_of(tid)?);
        let records = id
            .as_u64()
            .ok_or(Refused::Unknown)
            .and_then(|iid| self.iterators().start_records(&self.store, iid, tid))
            .map_err(|refused| Fault::of_iterator(refused, id))?
            .map_err(|error| Fault::of(&error))?;
        Ok(Value::from(records))
    }

    /// Return the next record of the record iterator `id`, as `[oid, tid,
    /// data, data_txn]`, or nil at its end. The data of a deletion is nil.
    /// The store keeps every pickle whole, so `data_txn` is always nil. The
    /// iterator reads the record now, and leaves out a record that a pack
    /// removed after the iterator started.
    fn iterator_record_next(&self, args: &[Value]) -> Result<Value, Fault> {
        let [id] = arity::<1>(args, "iterator_record_next")?;
        let next = id
            .as_u64()
            .ok_or(Refused::Unknown)
            .and_then(|iid| self.iterators().next_record(&self.store, iid))
            .map_err(|refused| Fault::of_iterator(refused, id))?;
        let record = match next {
            None => return Ok(Value::Nil),
            Some(Err(error)) => return Err(Fault::of(&error)),
            Some(Ok(record)) => record,
        };
        Ok(Value::Array(vec![
            p64(record.oid.0),
            p64(record.tid.0),
            record.data.map_or(Value::Nil, Value::Binary),
            Value::Nil,
        ]))
    }

    /// Remove the iterators that the client dropped. The client lists each
    /// record iterator that it dropped, also one at its end, which is gone.
    /// So an id that names no open iterator does nothing, as in ZEO.
    fn iterator_gc(&self, args: &[Value]) -> Result<Value, Fault> {
        let [ids] = arity::<1>(args, "iterator_gc")?;
        let Value::Array(ids) = ids else {
            return Err(Fault::type_error("the iterator ids are not a list"));
        };
        self.iterators().gc(ids.iter().filter_map(Value::as_u64));
        Ok(Value::Nil)
    }

    /// Pack the store at the tid of the time `time`, as FileStorage
    /// computes it, or at the last published tid when that is lower. The
    /// other connections read and commit while the pack runs. The pack
    /// runs on a thread of its own, so the other calls of this connection
    /// run too.
    ///
    /// After the pack, the hub sends `info` with the new size to every
    /// client of the storage. The reply goes out after the `info` of this
    /// client. It also goes out after the pack when `wait` is false. When
    /// the sizes do not read, no client gets `info`, and the reply is nil,
    /// because the pack is done.
    ///
    /// A read-only connection packs, as in ZEO: `zeopack` connects
    /// read-only. A store that the server opened read-only refuses the pack
    /// with `ReadOnlyError`.
    ///
    /// When the server stops, the pack stops between two batches and fails
    /// with `StorageError`. It does not stop during the mark. The store
    /// keeps what the pack did not remove, and the next pack removes it.
    fn pack(&self, args: &[Value]) -> Result<Value, Fault> {
        // A pack deletes data. A read-only client may not pack. A
        // connection that registered read-only by its own choice packs, as
        // `zeopack` needs.
        if self.read_only_client {
            return Err(Fault::read_only());
        }
        let time = match args {
            [time] | [time, _] => time,
            _ => return Err(Fault::type_error("pack takes 1 or 2 arguments")),
        };
        let time = match time {
            Value::F64(time) => *time,
            Value::F32(time) => f64::from(*time),
            Value::Integer(time) => time.as_f64().unwrap_or(f64::NAN),
            _ => return Err(Fault::type_error("the pack time is not a number")),
        };
        let tid = clock::pack_tid(time)
            .ok_or_else(|| Fault::value_error(format!("the pack time {time} has no tid")))?;
        // The store makes a commit durable before the hub publishes it, and
        // a client reads only the published commits. A pack after the last
        // published tid can remove a record that such a read needs.
        let invalidations = &self.storage.invalidations;
        let tid = tid.min(invalidations.published());
        let stopping = &self.stopping;
        self.store
            .pack_until(tid, || stopping.is_set())
            .map_err(|error| Fault::of_pack(&error))?;
        // `info` gives the sizes of the store. Sizes that do not read leave it
        // out, as for a commit.
        if let Ok(sizes) = self.sizes() {
            let info = message::notify("info", vec![Value::Map(sizes.into())]);
            invalidations.send_to_all(info);
        }
        Ok(Value::Nil)
    }
}

impl Drop for Session {
    /// The end of a connection leaves the status. The connection leaves the
    /// invalidations of the storage, and its iterators go with the session.
    fn drop(&mut self) {
        if let Some(registration) = self.registration.take() {
            Stats::sub(&self.storage.stats.connections);
            self.storage.invalidations.unsubscribe(registration.client);
        }
    }
}

impl Transaction {
    /// Return the transaction state of a connection of `session`, with no
    /// open transaction.
    pub(crate) fn new(session: Arc<Session>) -> Self {
        Self {
            session,
            open: None,
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
    fn holds_intents(&self) -> bool {
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
                first = format!("0x{:016x}", first.0),
                held_ms = self.session.storage.intent_hold.as_millis() as u64,
                "released the intent locks that a client held without tpc_begin"
            );
        }
    }

    /// Run one call of the transaction, and count it in the status of the
    /// storage. `storea` and `restorea` take the pickle out of the args of
    /// `call` for the compression pool.
    pub(crate) fn call(&mut self, call: &mut Call) -> Outcome {
        let was_open = self.open.is_some();
        let listed = self.open.as_ref().map_or(0, |open| open.listed);
        let started = Instant::now();
        let outcome = self.dispatch(call);
        let elapsed = started.elapsed();
        self.count(&call.method, &outcome, was_open);
        self.measure(&call.method, &outcome, was_open, listed, elapsed);
        outcome
    }

    /// Add a call that ran for `elapsed` to the metrics of the storage.
    /// `was_open` tells if a transaction was open before the call, and
    /// `listed` gives the conflicts that its last vote listed. Only the
    /// thread of the transaction changes `open`, so the call sees the
    /// state that it left.
    fn measure(
        &self,
        method: &str,
        outcome: &Outcome,
        was_open: bool,
        listed: u64,
        elapsed: std::time::Duration,
    ) {
        let metrics = &self.session.storage.metrics;
        let ended = was_open && self.open.is_none();
        let conflict = matches!(outcome, Outcome::Failed(fault) if fault.is_conflict());
        match method {
            "vote" => metrics.vote_latency.observe_duration(elapsed),
            "tpc_finish" if ended => {
                metrics.finish_latency.observe_duration(elapsed);
                if matches!(outcome, Outcome::Published) {
                    add(&metrics.commits);
                }
            }
            _ => {}
        }
        // A vote that fails with a conflict raises it in the client. So
        // does a transaction that ends between the two votes: the client
        // did not resolve a conflict that the vote listed.
        if (method == "vote" && conflict) || (ended && listed > 0 && method != "vote") {
            add(&metrics.conflicts_raised);
        }
    }

    /// Count a call that ran, as ZEO counts it. `was_open` tells if a
    /// transaction was open before the call.
    fn count(&self, method: &str, outcome: &Outcome, was_open: bool) {
        let stats = &self.session.storage.stats;
        let is_open = self.open.is_some();
        match (was_open, is_open) {
            (false, true) => Stats::add(&stats.active_txns),
            (true, false) => Stats::sub(&stats.active_txns),
            _ => {}
        }
        // An end of the transaction that the call named.
        let ended = was_open && !is_open;
        let counter = match outcome {
            Outcome::Close(_) | Outcome::Gone => None,
            Outcome::Done(_) if STORES.contains(&method) => Some(&stats.stores),
            Outcome::Failed(fault) if method == "vote" && fault.is_conflict() => {
                Some(&stats.conflicts)
            }
            _ if method == "tpc_finish" && ended => Some(&stats.commits),
            _ if method == "tpc_abort" && ended => Some(&stats.aborts),
            _ => None,
        };
        if let Some(counter) = counter {
            Stats::add(counter);
        }
    }

    /// Run one call of the transaction.
    fn dispatch(&mut self, call: &mut Call) -> Outcome {
        let args = call.args.as_mut_slice();
        if !self.session.is_registered() {
            return Outcome::Close(format!("{} before register", call.method));
        }
        if let Some(fault) = self.session.undeclared(&call.method) {
            return Outcome::Failed(fault);
        }
        let result = match call.method.as_str() {
            "lock_intent" => self.lock_intent(args),
            "unlock_intent" => self.unlock_intent(args),
            "tpc_begin" => self.tpc_begin(args),
            "storea" => self.storea(args),
            "restorea" => self.restorea(args),
            "checkCurrentSerialInTransaction" => self.check_current_serial(args),
            "deleteObject" => self.delete_object(args),
            "kv_set" => self.kv_set(args),
            "kv_delete" => self.kv_delete(args),
            "undoa" => self.undoa(args),
            "vote" => self.vote(args),
            "tpc_finish" => return self.tpc_finish(&*call),
            "tpc_abort" => self.tpc_abort(args),
            "storeBlobStart" => self.store_blob_start(args),
            "storeBlobChunk" => self.store_blob_chunk(args),
            "storeBlobEnd" => self.store_blob_end(args),
            "storeBlobShared" => self.store_blob_shared(args),
            method => return Outcome::Close(format!("unknown method {method}")),
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
    fn tpc_begin(&mut self, args: &[Value]) -> Result<Value, Fault> {
        let Some(id) = args.first() else {
            return Err(Fault::type_error("tpc_begin takes 4 to 6 arguments"));
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
                of_get: args.get(3).is_some_and(of_get),
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
    fn lock_intent(&mut self, args: &[Value]) -> Result<Value, Fault> {
        let [oids] = arity::<1>(args, "lock_intent")?;
        if self.session.read_only() {
            return Err(Fault::read_only());
        }
        if self.open.is_some() {
            return Err(Fault::transaction("lock_intent in an open transaction"));
        }
        let Value::Array(items) = oids else {
            return Err(Fault::type_error("the oids of lock_intent are not a list"));
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
    fn unlock_intent(&mut self, args: &[Value]) -> Result<Value, Fault> {
        arity::<0>(args, "unlock_intent")?;
        if let Some(intents) = self.intents.as_mut() {
            intents.release();
        }
        Ok(Value::Nil)
    }

    /// Return the core transaction that `args` of `tpc_begin` describe.
    fn begin(&self, args: &[Value]) -> Result<Txn<Arc<Store>>, Fault> {
        if args.len() < 4 || args.len() > 6 {
            return Err(Fault::type_error("tpc_begin takes 4 to 6 arguments"));
        }
        if self.session.read_only() {
            return Err(Fault::read_only());
        }
        // A copy gives the status of the copied transaction: " " for a
        // normal one, "p" for one that a pack of FileStorage changed. The
        // store keeps no status, as in `zoe.Storage`.
        if args
            .get(5)
            .is_some_and(|status| !matches!(status.as_str(), Some(" " | "p")))
        {
            return Err(Fault::transaction("transaction status is not supported"));
        }
        let user = text_bytes(&args[1])?;
        let description = text_bytes(&args[2])?;
        let ext = extension::pickle(&args[3]).map_err(Fault::type_error)?;
        let store = Arc::clone(&self.session.store);
        let mut txn = match args.get(4).filter(|tid| !tid.is_nil()) {
            None => Txn::begin(store, &user, &description, &ext),
            Some(tid) => {
                let tid = Tid(u64_of(tid)?);
                Txn::begin_restore(store, tid, &user, &description, &ext)?
            }
        };
        txn.set_lock_cancel(self.session.lock_cancel.clone());
        Ok(txn)
    }

    /// Return the open transaction with the id `id`. Another id fails with
    /// `StorageTransactionError`, whose args are the id of the open
    /// transaction, or None, and `id`.
    fn current(&mut self, id: &Value) -> Result<&mut Open, Fault> {
        let current = self.open.as_ref().map(|open| &open.id);
        if current != Some(id) {
            return Err(Fault::not_current(current, id));
        }
        Ok(self.open.as_mut().expect("the open transaction"))
    }

    /// Stage a new version of an object, and queue the encode of its
    /// pickle. The call does not wait for the encode.
    fn storea(&mut self, args: &mut [Value]) -> Result<Value, Fault> {
        let [oid, serial, data, id] = arity_mut::<4>(args, "storea")?;
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
    fn restorea(&mut self, args: &mut [Value]) -> Result<Value, Fault> {
        let [oid, _serial, data, _prev_txn, id] = arity_mut::<5>(args, "restorea")?;
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
    fn check_current_serial(&mut self, args: &[Value]) -> Result<Value, Fault> {
        let [oid, serial, id] = arity::<3>(args, "checkCurrentSerialInTransaction")?;
        self.current(id)?.stage(|txn| {
            let (oid, serial) = (Oid(u64_of(oid)?), serial_of(serial)?);
            Ok(txn.check_current_serial(oid, serial)?)
        });
        Ok(Value::Nil)
    }

    /// Stage a deletion of `oid`. The vote checks `serial` as it does for
    /// `storea`.
    fn delete_object(&mut self, args: &[Value]) -> Result<Value, Fault> {
        let [oid, serial, id] = arity::<3>(args, "deleteObject")?;
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
    fn kv_set(&mut self, args: &[Value]) -> Result<Value, Fault> {
        let [flavour, name, key, value, ttl] = arity::<5>(args, "kv_set")?;
        self.opened("kv_set")?.stage(|txn| {
            let flavour = flavour_of(flavour)?;
            let (name, key) = (str_of(name)?, bin_of(key, "key")?);
            let value = bin_of(value, "value")?;
            let ttl = ttl_of(ttl)?;
            Ok(txn.kv_set(flavour, name, key, value, ttl)?)
        });
        Ok(Value::Nil)
    }

    /// Stage a delete of a key of a namespace in the open transaction:
    /// `kv_delete(flavour, name, key)`, with the rules of `kv_set`.
    fn kv_delete(&mut self, args: &[Value]) -> Result<Value, Fault> {
        let [flavour, name, key] = arity::<3>(args, "kv_delete")?;
        self.opened("kv_delete")?.stage(|txn| {
            let flavour = flavour_of(flavour)?;
            let (name, key) = (str_of(name)?, bin_of(key, "key")?);
            Ok(txn.kv_delete(flavour, name, key)?)
        });
        Ok(Value::Nil)
    }

    /// Queue the undo of the transaction of the undo id `trans_id` into the
    /// open transaction `id`. `undoa` has no reply: the first vote stages
    /// the undo, as ZEO's server does, and an error there fails the vote.
    fn undoa(&mut self, args: &[Value]) -> Result<Value, Fault> {
        let [trans_id, id] = arity::<2>(args, "undoa")?;
        self.current(id)?.undos.push(trans_id.clone());
        Ok(Value::Nil)
    }

    /// Vote, and return the conflicts that the client resolves, as ZEO 6
    /// does with client conflict resolution. An empty list means that the
    /// vote passed.
    ///
    /// The first vote is `vote_resolving` of the core. It takes the locks
    /// once, in oid order, and runs tier 1 on each stale oid of a class of
    /// BTrees or `Length`: the reply gives each oid that tier 1 merged as a
    /// bare oid, as a ZEO 6 server gives an oid that it resolved, and a
    /// refusal of tier 1 fails the vote with `ConflictError`. Another stale
    /// written oid that the client can resolve does not fail it: the reply
    /// lists the oid, and the transaction keeps its locks. The client
    /// resolves each conflict, stores the resolved pickle over the
    /// committed serial and votes again. The second vote takes no lock and
    /// checks every serial again. No Python runs on the server.
    ///
    /// Before it takes a lock, the first vote stages the undos that `undoa`
    /// queued, in order. The reply gives each oid of an undone transaction
    /// as a bare oid, as ZEO's server does. An undo that no tier merges
    /// fails the vote with `UndoError`. Then the vote waits for each job of
    /// the compression pool of the transaction, and attaches its payload to
    /// the staged record. A job that failed fails the vote with
    /// `StorageError`.
    ///
    /// A failed vote ends the transaction, as in ZEO: the core transaction
    /// drops, which releases its locks. So the `tpc_abort` that follows
    /// finds no transaction.
    fn vote(&mut self, args: &[Value]) -> Result<Value, Fault> {
        let [id] = arity::<1>(args, "vote")?;
        let storage = Arc::clone(&self.session.storage);
        let open = self.current(id)?;
        let listed = std::mem::take(&mut open.listed);
        let native = std::mem::take(&mut open.native);
        let jobs = std::mem::take(&mut open.jobs);
        let undos = std::mem::take(&mut open.undos);
        let voted = match &mut open.txn {
            Ok(txn) if listed > 0 => {
                let voted = txn
                    .vote()
                    .map(|()| Voted::default())
                    .map_err(|error| Fault::of_vote(&error, txn));
                // The second vote compares the merges of shadow mode with
                // the records of the client: log and count what it found.
                storage.metrics.count_verifications(txn.verifications());
                if let Some(log) = &storage.resolution_log
                    && !txn.verifications().is_empty()
                {
                    log.write(&txn.verification_log_lines(SystemTime::now(), Some(&storage.name)));
                }
                voted
            }
            Ok(txn) => {
                let voted = undo(txn, &undos).and_then(|(undone, seen)| {
                    attach(txn, jobs)?;
                    let mut voted = vote_resolving(txn)?;
                    voted
                        .native
                        .retain(|oid| u64_of(oid).map_or(true, |oid| !seen.contains(&Oid(oid))));
                    voted.undone = undone.into_iter().map(|oid| p64(oid.0)).collect();
                    Ok(voted)
                });
                // The vote that takes the locks runs tier 1: log and count
                // its results, also when the vote failed.
                storage.metrics.count_resolutions(txn.resolutions());
                if let Some(log) = &storage.resolution_log
                    && !txn.resolutions().is_empty()
                {
                    log.write(&txn.resolution_log_lines(SystemTime::now(), Some(&storage.name)));
                }
                voted
            }
            Err(fault) => Err(fault.clone()),
        };
        match voted {
            Ok(Voted {
                undone,
                native: merged,
                conflicts,
            }) => {
                open.voted = true;
                open.listed = conflicts.len() as u64;
                // A vote that passes counts the conflicts that the client
                // resolved and the oids that tier 1 merged. A first vote
                // that lists conflicts keeps its merged oids for the
                // second vote.
                if conflicts.is_empty() {
                    Stats::add_n(&storage.stats.conflicts_resolved, listed);
                    let merges = native + merged.len() as u64;
                    Stats::add_n(&storage.metrics.conflicts_native, merges);
                } else {
                    open.native = merged.len() as u64;
                }
                Ok(Value::Array(
                    undone.into_iter().chain(merged).chain(conflicts).collect(),
                ))
            }
            Err(fault) => {
                self.open = None;
                Err(fault)
            }
        }
    }

    /// Commit the transaction. Its reply, the tid, goes out with the
    /// invalidations of the commit. A finish that fails publishes nothing.
    /// As in ZEO, a finish of a transaction that is not the open one does
    /// nothing and gives None.
    fn tpc_finish(&mut self, call: &Call) -> Outcome {
        let finished = match self.finish(&call.args) {
            Ok(Some(finished)) => finished,
            Ok(None) => return Outcome::Done(Value::Nil),
            Err(fault) => return Outcome::Failed(fault),
        };
        let reply = (!call.oneway).then(|| message::reply(call.id.clone(), p64(finished.tid.0)));
        self.session.publish(finished, reply);
        Outcome::Published
    }

    /// Commit the open transaction with the id of `args`. Return `None`
    /// when no such transaction is open.
    fn finish(&mut self, args: &[Value]) -> Result<Option<Finished>, Fault> {
        let [id] = arity::<1>(args, "tpc_finish")?;
        if !self.open.as_ref().is_some_and(|open| &open.id == id) {
            return Ok(None);
        }
        let open = self.open.take().expect("the open transaction");
        let Ok(txn) = open.txn else {
            return Err(Fault::transaction("tpc_finish before vote"));
        };
        let classes = open.of_get.then(|| classes_of(&txn));
        // The core returns only after the commit is durable.
        let finished = txn.finish_after()?;
        if let Some(classes) = classes {
            self.session.storage.metrics.add_writes_on_get(classes);
        }
        Ok(Some(finished))
    }

    fn tpc_abort(&mut self, args: &[Value]) -> Result<Value, Fault> {
        let [id] = arity::<1>(args, "tpc_abort")?;
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
    fn store_blob_start(&mut self, args: &[Value]) -> Result<Value, Fault> {
        arity::<0>(args, "storeBlobStart")?;
        let open = self
            .open
            .as_mut()
            .ok_or_else(|| Fault::transaction("storeBlobStart: no current transaction"))?;
        if open.txn.is_err() {
            return Ok(Value::Nil);
        }
        if open.upload.is_some() {
            return Err(Fault::value_error("storeBlobStart: an upload is open"));
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
    fn store_blob_chunk(&mut self, args: &[Value]) -> Result<Value, Fault> {
        let [chunk] = arity::<1>(args, "storeBlobChunk")?;
        let Value::Binary(chunk) = chunk else {
            return Err(Fault::type_error("a chunk is not bytes"));
        };
        let Some(open) = self.open.as_mut() else {
            return Err(Fault::value_error("storeBlobChunk: no upload is open"));
        };
        if open.txn.is_err() {
            return Ok(Value::Nil);
        }
        let upload = open
            .upload
            .as_mut()
            .ok_or_else(|| Fault::value_error("storeBlobChunk: no upload is open"))?;
        std::io::Write::write_all(upload, chunk)
            .map_err(|error| Fault::storage(format!("cannot write an upload file: {error}")))?;
        Ok(Value::Nil)
    }

    /// Stage the uploaded blob with `data` for `oid` with `Txn::store_blob`,
    /// which syncs the file. A stock client sent `storea` for the oid
    /// before, and this store keeps the pickle and adds the blob. An error
    /// of the stage fails the vote, as an error of `storea` does.
    fn store_blob_end(&mut self, args: &[Value]) -> Result<Value, Fault> {
        let [oid, serial, data, id] = arity::<4>(args, "storeBlobEnd")?;
        let open = self.current(id)?;
        // Dropping the upload removes a file that the core did not take.
        let upload = open.upload.take();
        if open.txn.is_err() {
            return Ok(Value::Nil);
        }
        let upload = upload.ok_or_else(|| Fault::value_error("storeBlobEnd: no upload is open"))?;
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
    fn store_blob_shared(&mut self, args: &[Value]) -> Result<Value, Fault> {
        let [oid, serial, data, filename, id] = arity::<5>(args, "storeBlobShared")?;
        let shared = self.session.storage.shared_blob_dir;
        self.current(id)?.stage(|txn| {
            if !shared {
                return Err(Fault::storage(
                    "storeBlobShared needs zoe serve --shared-blob-dir \
                     (ZOE_SHARED_BLOB_DIR): this server does not share its blob \
                     directory with its clients",
                ));
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
                add(&self.session.storage.metrics.conflicts_raised);
            }
            let stats = &self.session.storage.stats;
            Stats::sub(&stats.active_txns);
            Stats::add(&stats.aborts);
        }
    }
}

/// Wait for each job in `jobs`, and attach its payload to the staged
/// record of its oid in `txn`. A payload that does not match the staged
/// pickle stays out, and the finish encodes the pickle. The first job that
/// failed fails the call, and the drop of the other jobs cancels them.
fn attach(txn: &mut Txn<Arc<Store>>, jobs: Vec<Job>) -> Result<(), Fault> {
    for job in jobs {
        let (oid, encoded) = job.wait().map_err(|Failed(oid)| {
            Fault::storage(format!(
                "the encode of the pickle of oid {:#x} failed",
                oid.0
            ))
        })?;
        txn.set_encoded(oid, encoded)?;
    }
    Ok(())
}

/// The items of the reply of a vote.
#[derive(Default)]
struct Voted {
    /// The oids of the undone transactions, each as a bare oid.
    undone: Vec<Value>,
    /// The oids that tier 1 merged, each as a bare oid.
    native: Vec<Value>,
    /// The conflicts that the client resolves.
    conflicts: Vec<Value>,
}

/// Stage in `txn` the undo of each undo id of `ids`, in order. Return the
/// oids of the undone transactions, each once, in the order of the undos,
/// and their set.
///
/// An undo id that names no transaction, and an undo that the core
/// refuses, fail with `UndoError`. So does an undo that no tier of the
/// core merges: the in-process storage resolves such an undo in Python,
/// and no Python runs on the server.
fn undo(txn: &mut Txn<Arc<Store>>, ids: &[Value]) -> Result<(Vec<Oid>, HashSet<Oid>), Fault> {
    let mut oids = Vec::new();
    let mut seen = HashSet::new();
    for id in ids {
        let tid = match id {
            Value::Binary(id) => undo_tid(id),
            Value::String(id) => undo_tid(id.as_bytes()),
            _ => None,
        }
        .ok_or_else(|| Fault::undo("the undo id names no transaction", None))?;
        let undone = txn.undo(tid)?;
        if let Some(conflict) = undone.conflicts.first() {
            return Err(Fault::undo(CHANGED_LATER, Some(conflict.oid)));
        }
        for oid in undone.oids {
            if seen.insert(oid) {
                oids.push(oid);
            }
        }
    }
    Ok((oids, seen))
}

/// Return the map of the undo log of the transaction row `row`: the keys
/// `id`, `time`, `user_name` and `description` of FileStorage, then the
/// keys of the extension, which replace a key of the same name, as in
/// FileStorage. An extension that gives no msgpack map adds no key.
fn undo_entry(row: &TxnMeta) -> Value {
    let mut items = vec![
        (Value::from("id"), Value::Binary(undo_id(row.tid))),
        (Value::from("time"), Value::F64(clock::tid_time(row.tid))),
        (Value::from("user_name"), Value::Binary(row.user.clone())),
        (
            Value::from("description"),
            Value::Binary(row.description.clone()),
        ),
    ];
    if let Ok(Value::Map(extension)) = extension::unpickle(&row.extension) {
        for (key, value) in extension {
            match items.iter_mut().find(|item| item.0 == key) {
                Some(item) => item.1 = value,
                None => items.push((key, value)),
            }
        }
    }
    Value::Map(items)
}

/// Tell if Python finds `a` and `b` equal, where the msgpack values allow
/// it to tell: an int, a float and a bool compare by their numbers, as
/// `1 == 1.0` and `True == 1` are true. Other values compare as msgpack
/// values, so `b"x"` and `"x"` differ, as in Python.
fn python_eq(a: &Value, b: &Value) -> bool {
    fn number(value: &Value) -> Option<f64> {
        match value {
            Value::Integer(n) => n
                .as_i64()
                .map(|n| n as f64)
                .or(n.as_u64().map(|n| n as f64)),
            Value::F64(x) => Some(*x),
            Value::F32(x) => Some(f64::from(*x)),
            Value::Boolean(b) => Some(f64::from(u8::from(*b))),
            _ => None,
        }
    }
    match (a, b) {
        (Value::Integer(x), Value::Integer(y)) => x == y,
        _ => match (number(a), number(b)) {
            (Some(x), Some(y)) => x == y,
            _ => a == b,
        },
    }
}

/// Return the int `value` of an argument.
fn int_of(value: &Value) -> Result<i64, Fault> {
    value
        .as_i64()
        .ok_or_else(|| Fault::type_error("an index of the undo log is not an int"))
}

/// Vote with `vote_resolving` of the core. Return each oid that tier 1
/// merged, and each stale written oid that the client can resolve, as ZEO 6
/// lists a conflict in the reply of `vote`: `{oid, serials: [committed,
/// read], data}`. `data` is the pickle that the transaction stored. The
/// client loads the two other states with `loadSerial`.
fn vote_resolving(txn: &mut Txn<Arc<Store>>) -> Result<Voted, Fault> {
    let conflicts = txn
        .vote_resolving()
        .map_err(|error| Fault::of_vote(&error, txn))?;
    let native = txn
        .natively_resolved()
        .into_iter()
        .map(|oid| p64(oid.0))
        .collect();
    let conflicts = conflicts
        .into_iter()
        .map(|(oid, committed)| {
            // A staged pickle in the spill file is read from the file, and
            // that read can fail.
            let (read, data) = txn
                .staged(oid)?
                .ok_or_else(|| Fault::storage("a conflict has no staged pickle"))?;
            Ok(Value::Map(vec![
                (Value::from("oid"), p64(oid.0)),
                (
                    Value::from("serials"),
                    Value::Array(vec![p64(committed.tid.0), p64(read.0)]),
                ),
                (Value::from("data"), Value::Binary(data.into_owned())),
            ]))
        })
        .collect::<Result<Vec<_>, Fault>>()?;
    Ok(Voted {
        undone: Vec::new(),
        native,
        conflicts,
    })
}

/// Add 1 to the metric `counter`.
fn add(counter: &AtomicU64) {
    Stats::add(counter);
}

/// Return the label of a client for the log lines: its text, with each
/// control character replaced by a space, and at most `MAX_LABEL`
/// characters.
fn client_label(value: &Value) -> String {
    let text = storage_name(value);
    text.chars()
        .take(MAX_LABEL)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// Return the `N` arguments of `method`.
/// Return the args of `method` as an array of `N` values to change.
fn arity_mut<'a, const N: usize>(
    args: &'a mut [Value],
    method: &str,
) -> Result<&'a mut [Value; N], Fault> {
    args.try_into()
        .map_err(|_| Fault::type_error(format!("{method} takes {N} arguments")))
}

fn arity<'a, const N: usize>(args: &'a [Value], method: &str) -> Result<&'a [Value; N], Fault> {
    args.try_into()
        .map_err(|_| Fault::type_error(format!("{method} takes {N} arguments")))
}

/// The flavour of a namespace: the str `"ephemeral"` or `"durable"`.
fn flavour_of(value: &Value) -> Result<Flavour, Fault> {
    let name = value
        .as_str()
        .ok_or_else(|| Fault::type_error("the flavour of a namespace is not a str"))?;
    Flavour::from_name(name).ok_or_else(|| {
        Fault::value_error("the flavour of a namespace is not \"ephemeral\" or \"durable\"")
    })
}

/// The name of a namespace: a str.
fn str_of(value: &Value) -> Result<&str, Fault> {
    value
        .as_str()
        .ok_or_else(|| Fault::type_error("the name of a namespace is not a str"))
}

/// A key, a bound or a value of a namespace: bin. `what` names it.
fn bin_of<'a>(value: &'a Value, what: &str) -> Result<&'a [u8], Fault> {
    match value {
        Value::Binary(bytes) => Ok(bytes),
        _ => Err(Fault::type_error(format!("the {what} is not bytes"))),
    }
}

/// A bound of a scan: bin, or nil for an open end.
fn optional_bin<'a>(value: &'a Value, what: &str) -> Result<Option<&'a [u8]>, Fault> {
    match value {
        Value::Nil => Ok(None),
        value => bin_of(value, what).map(Some),
    }
}

/// The TTL of a write: an int or a float of seconds, or nil for none. A
/// negative, not finite or too long TTL fails with `ValueError`. The core
/// checks its bounds.
fn ttl_of(value: &Value) -> Result<Option<Duration>, Fault> {
    let refused = || Fault::value_error("a TTL is not a number of seconds from 0 to 2^32 - 1");
    match value {
        Value::Nil => Ok(None),
        Value::Integer(n) => match n.as_u64() {
            Some(seconds) => Ok(Some(Duration::from_secs(seconds))),
            None => Err(refused()),
        },
        Value::F32(seconds) => Duration::try_from_secs_f32(*seconds)
            .map(Some)
            .map_err(|_| refused()),
        Value::F64(seconds) => Duration::try_from_secs_f64(*seconds)
            .map(Some)
            .map_err(|_| refused()),
        _ => Err(Fault::type_error("a TTL is not an int, a float or None")),
    }
}

/// An oid or a tid as the protocol sends it: 8 bytes, big-endian.
pub(super) fn p64(n: u64) -> Value {
    Value::Binary(n.to_be_bytes().to_vec())
}

/// A tid, or nil for none.
fn optional_tid(value: &Value) -> Result<Option<Tid>, Fault> {
    match value {
        Value::Nil => Ok(None),
        tid => Ok(Some(Tid(u64_of(tid)?))),
    }
}

/// The serial of a store, `storea`, `storeBlobEnd` or `storeBlobShared`.
/// A nil serial is the zero tid, the serial of a new object, as in
/// FileStorage. Another serial reads as `serial_of` reads it.
fn store_serial(value: &Value) -> Result<Tid, Fault> {
    match value {
        Value::Nil => Ok(Tid::ZERO),
        serial => serial_of(serial),
    }
}

/// The serial argument of a call of the transaction: 8 bytes, or a str
/// whose UTF-8 bytes are 8 bytes. ZEO's tests send the serial of a new
/// object as the str `'\0' * 8`, and ZEO's server takes it. A str of
/// another length fails as a str oid or tid does.
fn serial_of(value: &Value) -> Result<Tid, Fault> {
    match value {
        Value::String(text) => text
            .as_str()
            .and_then(|text| <[u8; 8]>::try_from(text.as_bytes()).ok())
            .map(|bytes| Tid(u64::from_be_bytes(bytes)))
            .ok_or_else(|| Fault::type_error("an oid or a tid is not bytes")),
        serial => Ok(Tid(u64_of(serial)?)),
    }
}

/// The number in the 8 big-endian bytes of `value`.
fn u64_of(value: &Value) -> Result<u64, Fault> {
    match value {
        Value::Binary(bytes) => <[u8; 8]>::try_from(bytes.as_slice())
            .map(u64::from_be_bytes)
            .map_err(|_| Fault::type_error("an oid or a tid is not 8 bytes")),
        _ => Err(Fault::type_error("an oid or a tid is not bytes")),
    }
}

/// The bytes of a user name or a description: bytes, or a text in UTF-8.
fn text_bytes(value: &Value) -> Result<Vec<u8>, Fault> {
    match value {
        Value::Binary(bytes) => Ok(bytes.clone()),
        Value::String(text) => Ok(text.as_bytes().to_vec()),
        Value::Nil => Ok(Vec::new()),
        _ => Err(Fault::type_error("a user or a description is not bytes")),
    }
}

/// The truth of a msgpack value, as Python gives it.
fn truth(value: &Value) -> bool {
    match value {
        Value::Nil => false,
        Value::Boolean(flag) => *flag,
        Value::Integer(n) => n.as_i64() != Some(0),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use zoe_core::Options;

    use tokio::sync::mpsc::Receiver;

    use super::super::outbox::Inbox;
    use super::*;
    use crate::protocol::{Queues, ResolutionLog};

    /// A pickle of a `persistent.mapping.PersistentMapping` record, as ZODB
    /// writes it: the class pickle, then the state pickle.
    const MAPPING: &[u8] = b"\x80\x03X\x12\x00\x00\x00persistent.mappingq\x00\
        X\x11\x00\x00\x00PersistentMappingq\x01\x86q\x02N\x86q\x03.\x80\x03}q\x00.";

    /// A connection whose calls run in order on the thread of the test.
    struct Client {
        session: Arc<Session>,
        txn: Transaction,
        /// The last call put its reply in the queue itself.
        sent: Option<bool>,
    }

    impl Client {
        fn new(storage: Arc<NamedStorage>, outbox: Outbox) -> Self {
            Self::stopping(storage, outbox, never_stops())
        }

        /// A connection whose pack stops when `stopping` tells it.
        fn stopping(storage: Arc<NamedStorage>, outbox: Outbox, stopping: Stopping) -> Self {
            Self::of(Session::new(storage, outbox, stopping))
        }

        /// The connection of `session`.
        fn of(session: Session) -> Self {
            let session = Arc::new(session);
            Self {
                txn: Transaction::new(Arc::clone(&session)),
                session,
                sent: None,
            }
        }

        /// Run one call. Keep whether it put its reply itself.
        fn call(&mut self, call: &Call) -> Outcome {
            if self.session.is_registered() && in_transaction(&call.method) {
                return self.txn.call(&mut call.clone());
            }
            let mut reply = Reply::new(self.session.outbox.blocking_room());
            let outcome = self.session.call(call, &mut reply);
            self.sent = reply.sent.take();
            match outcome {
                // The result of a load that put its reply is in the frame.
                Outcome::Sent => {
                    let frame = reply.frame.expect("the call put a frame");
                    let message = rmpv::decode::read_value(&mut frame.as_slice()).unwrap();
                    let Value::Array(mut parts) = message else {
                        panic!("a reply is not an array: {message:?}");
                    };
                    assert_eq!(parts[1], Value::from(0), "{parts:?}");
                    Outcome::Done(parts.pop().unwrap())
                }
                outcome => outcome,
            }
        }
    }

    impl std::ops::Deref for Client {
        type Target = Session;

        fn deref(&self) -> &Session {
            &self.session
        }
    }

    /// Run the calls that arrive on `calls` as a connection runs them, and
    /// put their replies in `outbox`. Return when `calls` closes and each
    /// call ended, or when a call ends the connection. The threads of the
    /// calls log to the subscriber of the thread of the test.
    fn run(storages: Arc<Storages>, mut calls: Receiver<Call>, outbox: Outbox) {
        use super::super::pipeline::Calls;
        use crate::runtime::Tokio;

        let dispatch = tracing::dispatcher::get_default(Clone::clone);
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .on_thread_start(move || {
                std::mem::forget(tracing::dispatcher::set_default(&dispatch));
            })
            .build()
            .unwrap();
        runtime.block_on(async move {
            let mut connection = Calls::new(
                storages,
                outbox.clone(),
                Arc::new(Tokio),
                "test".into(),
                never_stops(),
            );
            let ended = connection.ended();
            loop {
                let call = tokio::select! {
                    call = calls.recv() => call,
                    () = ended.wait() => None,
                };
                let Some(call) = call else { break };
                let ticket = connection.ticket(&call).await;
                if ticket.is_closed() {
                    break;
                }
                connection.start(call, ticket).await;
            }
            drop(outbox);
            connection.finish().await;
        });
    }

    /// Return a signal that never tells a stop: its sender is gone.
    fn never_stops() -> Stopping {
        Stopping::new().1
    }

    /// Return an outbox whose frames go nowhere.
    fn outbox() -> Outbox {
        Outbox::new(1_000, 1_000).0
    }

    /// Return a storage on a new store for the test `name`.
    fn storage(name: &str, queues: Queues) -> Arc<NamedStorage> {
        let dir = std::env::temp_dir().join(format!("zoe-session-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Arc::new(Store::open(&dir).unwrap());
        Arc::new(NamedStorage::new("1", &dir, store, queues))
    }

    fn session(name: &str) -> Client {
        Client::new(storage(name, Queues::DEFAULT), outbox())
    }

    /// Return the storages of a server that serves only `storage`.
    fn serving(storage: Arc<NamedStorage>) -> Arc<Storages> {
        Arc::new(Storages::new(vec![storage], Queues::DEFAULT.client))
    }

    fn call(session: &mut Client, method: &str, args: Vec<Value>) -> Outcome {
        session.call(&Call {
            id: Value::from(1),
            oneway: false,
            method: method.into(),
            args,
            context: None,
        })
    }

    fn done(outcome: Outcome) -> Value {
        match outcome {
            Outcome::Done(value) => value,
            other => panic!("not done: {other:?}"),
        }
    }

    fn failed(outcome: Outcome) -> Fault {
        match outcome {
            Outcome::Failed(fault) => fault,
            other => panic!("not failed: {other:?}"),
        }
    }

    /// Finish the transaction `id`, and return its tid. With no other
    /// session, the commit is the last published one.
    fn finish(s: &mut Client, id: Value) -> u64 {
        match call(s, "tpc_finish", vec![id]) {
            Outcome::Published => {}
            other => panic!("not published: {other:?}"),
        }
        u64_of(&done(call(s, "lastTransaction", vec![]))).unwrap()
    }

    /// Begin the transaction `id`, with the tid `tid` for a restore
    /// transaction.
    fn begin(s: &mut Client, id: i64, tid: Option<u64>) {
        let tid = tid.map_or(Value::Nil, p64);
        let args = vec![
            Value::from(id),
            Value::Nil,
            Value::Nil,
            Value::Map(vec![]),
            tid,
            Value::from(" "),
        ];
        assert_eq!(done(call(s, "tpc_begin", args)), Value::Nil);
    }

    fn store(s: &mut Client, id: i64, oid: u64, serial: u64, data: &[u8]) {
        let args = vec![
            p64(oid),
            p64(serial),
            Value::Binary(data.to_vec()),
            Value::from(id),
        ];
        assert_eq!(done(call(s, "storea", args)), Value::Nil);
    }

    /// Vote and finish the transaction `id`. Return the committed tid, or
    /// the exception of the vote.
    fn vote_and_finish(s: &mut Client, id: i64) -> Result<u64, Fault> {
        match call(s, "vote", vec![Value::from(id)]) {
            Outcome::Done(_) => Ok(finish(s, Value::from(id))),
            outcome => Err(failed(outcome)),
        }
    }

    /// The value of `key` in the args of a conflict.
    fn item(fault: &Fault, key: &str) -> Value {
        let Value::Map(items) = &fault.args else {
            panic!("the args of a conflict are a map: {fault:?}");
        };
        items
            .iter()
            .find(|(name, _)| name.as_str() == Some(key))
            .map(|(_, value)| value.clone())
            .unwrap_or_else(|| panic!("no {key} in {fault:?}"))
    }

    #[test]
    fn a_call_before_register_closes_the_connection() {
        let mut s = session("before-register");
        assert!(matches!(
            call(&mut s, "lastTransaction", vec![]),
            Outcome::Close(_)
        ));
    }

    #[test]
    fn register_takes_only_the_storage_1_and_only_once() {
        let mut s = session("register");
        let fault = failed(call(
            &mut s,
            "register",
            vec![Value::from("2"), Value::Boolean(false)],
        ));
        assert_eq!(fault.class, "builtins.ValueError");
        assert_eq!(
            done(call(
                &mut s,
                "register",
                vec![Value::from("1"), Value::Boolean(false)]
            )),
            p64(0)
        );
        let fault = failed(call(
            &mut s,
            "register",
            vec![Value::from("1"), Value::Boolean(false)],
        ));
        assert_eq!(fault.class, "builtins.ValueError");
        assert!(matches!(
            call(&mut s, "no_such_method", vec![]),
            Outcome::Close(_)
        ));
    }

    #[test]
    fn register_selects_the_storage_of_the_connection_by_its_name() {
        let named = |name: &str| {
            let dir = std::env::temp_dir()
                .join(format!("zoe-session-select-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            let store = Arc::new(Store::open(&dir).unwrap());
            Arc::new(NamedStorage::new(name, &dir, store, Queues::DEFAULT))
        };
        let storages = Storages::new(vec![named("a"), named("b")], Queues::DEFAULT.client);
        let request = |method: &str, args: Vec<Value>| Call {
            id: Value::from(1),
            oneway: false,
            method: method.into(),
            args,
            context: None,
        };
        let register =
            |name: &str| request("register", vec![Value::from(name), Value::Boolean(false)]);
        for name in ["a", "b"] {
            let storage = select(&storages, &register(name)).ok().unwrap();
            assert_eq!(storage.name, name);
        }
        let by_bytes = request(
            "register",
            vec![Value::Binary(b"b".to_vec()), Value::Boolean(false)],
        );
        assert_eq!(select(&storages, &by_bytes).ok().unwrap().name, "b");
        let declaring = request(
            "register",
            vec![
                Value::from("a"),
                Value::Boolean(false),
                Value::Array(vec![]),
            ],
        );
        assert_eq!(select(&storages, &declaring).ok().unwrap().name, "a");
        let fault = failed(select(&storages, &register("1")).err().unwrap());
        assert_eq!(fault.class, "builtins.ValueError");
        let fault = failed(
            select(&storages, &request("register", vec![]))
                .err()
                .unwrap(),
        );
        assert_eq!(fault.class, "builtins.TypeError");
        assert!(matches!(
            select(&storages, &request("lastTransaction", vec![])),
            Err(Outcome::Close(_))
        ));
    }

    #[test]
    fn a_read_only_client_gets_no_oid_and_its_vote_fails() {
        let mut s = session("read-only");
        done(call(
            &mut s,
            "register",
            vec![Value::from("1"), Value::Boolean(true)],
        ));
        let fault = failed(call(&mut s, "new_oids", vec![]));
        assert_eq!(fault, Fault::read_only());
        begin(&mut s, 1, None);
        store(&mut s, 1, 1, 0, b"a");
        let fault = failed(call(&mut s, "vote", vec![Value::from(1)]));
        assert_eq!(fault, Fault::read_only());
        assert!(s.txn.open.is_none());
    }

    #[test]
    fn new_oids_gives_at_most_100_and_at_least_1() {
        let mut s = registered("new-oids");
        let count = |v: Value| v.as_array().unwrap().len();
        assert_eq!(count(done(call(&mut s, "new_oids", vec![]))), 100);
        assert_eq!(
            count(done(call(&mut s, "new_oids", vec![Value::from(500)]))),
            100
        );
        assert_eq!(
            count(done(call(&mut s, "new_oids", vec![Value::from(0)]))),
            1
        );
    }

    #[test]
    fn a_failed_store_fails_the_vote_and_ends_the_transaction() {
        let mut s = registered("failed-store");
        begin(&mut s, 9, None);
        // The core refuses an empty pickle. The calls after it do nothing.
        store(&mut s, 9, 1, 0, b"");
        store(&mut s, 9, 2, 0, b"a");
        let fault = failed(call(&mut s, "vote", vec![Value::from(9)]));
        assert_eq!(fault.class, TRANSACTION_ERROR);
        assert!(s.txn.open.is_none());
        // As in ZEO, a finish or an abort of no transaction gives None.
        assert_eq!(
            done(call(&mut s, "tpc_finish", vec![Value::from(9)])),
            Value::Nil
        );
        assert_eq!(
            done(call(&mut s, "tpc_abort", vec![Value::from(9)])),
            Value::Nil
        );
        let load = vec![p64(2), p64(u64::MAX)];
        assert_eq!(
            failed(call(&mut s, "loadBefore", load)).class,
            "ZODB.POSException.POSKeyError"
        );
    }

    #[test]
    fn an_error_of_tpc_begin_fails_the_vote_and_keeps_the_connection() {
        let mut s = registered("begin-errors");
        let first = commit(&mut s, 1, 1, 0, b"a").unwrap();
        let bad_status = vec![
            Value::from(2),
            Value::Nil,
            Value::Nil,
            Value::Map(vec![]),
            Value::Nil,
            Value::from("u"),
        ];
        let bad_extension = vec![Value::from(2), Value::Nil, Value::Nil, Value::from(3)];
        let old_tid = vec![
            Value::from(2),
            Value::Nil,
            Value::Nil,
            Value::Map(vec![]),
            p64(first),
        ];
        let short_tid = vec![
            Value::from(2),
            Value::Nil,
            Value::Nil,
            Value::Map(vec![]),
            Value::Binary(vec![1]),
        ];
        for args in [bad_status, bad_extension, old_tid, short_tid] {
            assert_eq!(done(call(&mut s, "tpc_begin", args.clone())), Value::Nil);
            store(&mut s, 2, 1, first, b"b");
            let fault = failed(call(&mut s, "vote", vec![Value::from(2)]));
            assert!(
                [TRANSACTION_ERROR, "builtins.TypeError"].contains(&fault.class),
                "{args:?}: {fault:?}"
            );
            assert_eq!(
                done(call(&mut s, "tpc_abort", vec![Value::from(2)])),
                Value::Nil
            );
        }
        // Nothing changed, and the session takes the next transaction.
        assert_eq!(s.store.last_transaction(), Tid(first));
        commit(&mut s, 3, 1, first, b"c").unwrap();
    }

    #[test]
    fn a_call_of_another_transaction_names_both_ids() {
        let mut s = registered("other-transaction");
        let fault = failed(call(&mut s, "vote", vec![Value::from(7)]));
        assert_eq!(fault, Fault::not_current(None, &Value::from(7)));
        assert_eq!(fault.args, Value::Array(vec![Value::Nil, Value::from(7)]));

        begin(&mut s, 1, None);
        // A second tpc_begin, of this transaction or of another one, does
        // nothing.
        begin(&mut s, 1, None);
        begin(&mut s, 2, None);
        let fault = failed(call(&mut s, "vote", vec![Value::from(2)]));
        assert_eq!(
            fault.args,
            Value::Array(vec![Value::from(1), Value::from(2)])
        );
        for method in ["storea", "checkCurrentSerialInTransaction", "deleteObject"] {
            let mut args = vec![p64(1), p64(0), Value::Binary(b"a".to_vec())];
            if method != "storea" {
                args.pop();
            }
            args.push(Value::from(2));
            let fault = failed(call(&mut s, method, args));
            assert_eq!(fault.class, TRANSACTION_ERROR, "{method}");
        }
        assert_eq!(
            done(call(&mut s, "tpc_finish", vec![Value::from(2)])),
            Value::Nil
        );
        assert_eq!(
            done(call(&mut s, "tpc_abort", vec![Value::from(2)])),
            Value::Nil
        );
        // The transaction 1 is still open.
        store(&mut s, 1, 1, 0, b"a");
        vote_and_finish(&mut s, 1).unwrap();
    }

    #[test]
    fn a_stale_serial_is_a_conflict_error_with_the_serials_and_the_class() {
        let mut s = registered("conflict");
        let first = commit(&mut s, 1, 0, 0, MAPPING).unwrap();
        let fault = commit(&mut s, 2, 0, 0, MAPPING).unwrap_err();
        assert_eq!(
            fault,
            Fault::conflict(
                CONFLICT_ERROR,
                CONFLICT,
                Oid(0),
                Some((Tid(first), Tid(0))),
                Some("persistent.mapping.PersistentMapping".into()),
            )
        );
        assert_eq!(item(&fault, "message"), Value::from(CONFLICT));
        assert_eq!(
            item(&fault, "class_name"),
            Value::from("persistent.mapping.PersistentMapping")
        );
        assert_eq!(
            item(&fault, "serials"),
            Value::Array(vec![p64(first), p64(0)])
        );
        // A pickle whose class does not decode gives no class.
        let fault = commit(&mut s, 3, 0, 0, b"x").unwrap_err();
        assert_eq!(item(&fault, "class_name"), Value::Nil);
        assert_eq!(item(&fault, "oid"), p64(0));
    }

    #[test]
    fn a_stale_read_current_serial_is_a_read_conflict_error() {
        let mut s = registered("read-conflict");
        let first = commit(&mut s, 1, 1, 0, MAPPING).unwrap();
        let second = commit(&mut s, 2, 1, first, MAPPING).unwrap();
        begin(&mut s, 3, None);
        let check = vec![p64(1), p64(first), Value::from(3)];
        assert_eq!(
            done(call(&mut s, "checkCurrentSerialInTransaction", check)),
            Value::Nil
        );
        store(&mut s, 3, 2, 0, MAPPING);
        let fault = vote_and_finish(&mut s, 3).unwrap_err();
        assert_eq!(
            fault,
            Fault::conflict(
                READ_CONFLICT_ERROR,
                READ_CONFLICT,
                Oid(1),
                Some((Tid(second), Tid(first))),
                None,
            )
        );
        assert_eq!(item(&fault, "class_name"), Value::Nil);
        assert!(s.txn.open.is_none());
        assert_eq!(
            done(call(&mut s, "tpc_abort", vec![Value::from(3)])),
            Value::Nil
        );

        // A current serial commits.
        begin(&mut s, 4, None);
        let check = vec![p64(1), p64(second), Value::from(4)];
        done(call(&mut s, "checkCurrentSerialInTransaction", check));
        store(&mut s, 4, 2, 0, MAPPING);
        vote_and_finish(&mut s, 4).unwrap();
    }

    #[test]
    fn delete_object_writes_a_deletion_with_the_serial_check_of_storea() {
        let mut s = registered("delete");
        let first = commit(&mut s, 1, 1, 0, MAPPING).unwrap();
        // A stale serial is a conflict. A deletion has no class.
        begin(&mut s, 2, None);
        let delete = vec![p64(1), p64(0), Value::from(2)];
        assert_eq!(done(call(&mut s, "deleteObject", delete)), Value::Nil);
        let fault = vote_and_finish(&mut s, 2).unwrap_err();
        assert_eq!(
            fault,
            Fault::conflict(
                CONFLICT_ERROR,
                CONFLICT,
                Oid(1),
                Some((Tid(first), Tid(0))),
                None
            )
        );

        begin(&mut s, 3, None);
        let delete = vec![p64(1), p64(first), Value::from(3)];
        done(call(&mut s, "deleteObject", delete));
        let deleted = vote_and_finish(&mut s, 3).unwrap();
        assert!(deleted > first);
        let load = vec![p64(1), p64(u64::MAX)];
        let fault = failed(call(&mut s, "loadBefore", load));
        assert_eq!(fault, Fault::from(ZoeError::NotFound { oid: Oid(1) }));
        // The revision before the deletion still loads.
        let load = vec![p64(1), p64(deleted)];
        let loaded = done(call(&mut s, "loadBefore", load));
        assert_eq!(
            loaded,
            Value::Array(vec![
                Value::Binary(MAPPING.to_vec()),
                p64(first),
                p64(deleted)
            ])
        );
        let load = vec![p64(1), p64(first)];
        let loaded = done(call(&mut s, "loadSerial", load));
        assert_eq!(loaded, Value::Binary(MAPPING.to_vec()));
        let load = vec![p64(1), p64(deleted)];
        let fault = failed(call(&mut s, "loadSerial", load));
        assert_eq!(fault, Fault::from(ZoeError::NotFound { oid: Oid(1) }));
        // `record_iternext` skips the deleted oid.
        let fault = failed(call(&mut s, "record_iternext", vec![Value::Nil]));
        assert_eq!(fault.class, "builtins.ValueError");
        let kept = commit(&mut s, 4, 2, 0, MAPPING).unwrap();
        let current = done(call(&mut s, "record_iternext", vec![Value::Nil]));
        assert_eq!(
            current,
            Value::Array(vec![
                p64(2),
                p64(kept),
                Value::Binary(MAPPING.to_vec()),
                Value::Nil
            ])
        );
    }

    #[test]
    fn tpc_begin_with_a_tid_commits_under_that_tid() {
        let mut s = registered("restore");
        let first = commit(&mut s, 1, 1, 0, b"a").unwrap();
        let wanted = first + 2;
        begin(&mut s, 2, Some(wanted));
        store(&mut s, 2, 1, first, b"b");
        assert_eq!(vote_and_finish(&mut s, 2), Ok(wanted));
        assert_eq!(s.store.last_transaction(), Tid(wanted));
        // The serial check of storea holds in a restore transaction: the
        // vote lists the conflict, and a second vote without a resolved
        // pickle fails.
        begin(&mut s, 3, Some(wanted + 1));
        store(&mut s, 3, 1, first, b"c");
        let listed = done(call(&mut s, "vote", vec![Value::from(3)]));
        assert_eq!(listed.as_array().map(Vec::len), Some(1));
        let fault = failed(call(&mut s, "vote", vec![Value::from(3)]));
        assert_eq!(fault.class, CONFLICT_ERROR);
    }

    /// The pickle of a `PCounter` of `ZODB.tests.ConflictResolution` with
    /// the value `value`, as the client of the trace stored it.
    fn pcounter(value: u8) -> Vec<u8> {
        hex(&format!(
            "8003581d0000005a4f44422e74657374732e436f6e666c6963745265736f6c75\
             74696f6e7100580800000050436f756e74657271018671024e8671032e80037d\
             710058060000005f76616c756571014b{value:02x}732e"
        ))
    }

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
            .collect()
    }

    /// Run the call of the frame `raw`, in hex. Return the frame of its
    /// reply, or `None` for a call without a reply.
    fn replay(s: &mut Client, raw: &str) -> Option<Vec<u8>> {
        let call = message::decode(&hex(raw)).unwrap();
        match s.call(&call) {
            Outcome::Done(Value::Nil) if call.oneway => None,
            Outcome::Done(value) => Some(message::reply(call.id, value)),
            Outcome::Failed(fault) => Some(message::error(call.id, fault.class, fault.args)),
            Outcome::Close(why) => panic!("the session closed: {why}"),
            Outcome::Published | Outcome::Sent => panic!("the reply went to the outbox"),
            Outcome::Gone => panic!("the client is gone"),
        }
    }

    #[test]
    fn a_conflict_goes_to_the_client_with_the_bytes_of_zeo() {
        // The serials of docs/spikes/zeo-wire/client-resolution.jsonl. The
        // oid 1 holds a PCounter at 1 under BASE, then at 2 under
        // COMMITTED. Restore transactions commit under these tids.
        const BASE: u64 = 0x040c_458e_f171_4222;
        const COMMITTED: u64 = 0x040c_458e_f1f7_6388;
        let mut s = registered("resolve-trace");
        begin(&mut s, 1, Some(BASE));
        store(&mut s, 1, 1, 0, &pcounter(1));
        assert_eq!(vote_and_finish(&mut s, 1), Ok(BASE));
        begin(&mut s, 2, Some(COMMITTED));
        store(&mut s, 2, 1, BASE, &pcounter(2));
        assert_eq!(vote_and_finish(&mut s, 2), Ok(COMMITTED));

        // Lines 24 to 34 of the trace: the client stores 3 over BASE. The
        // vote lists the conflict with the pickle of the client. The client
        // loads the two other states, stores 4 over COMMITTED and votes
        // again.
        let txn = "cf000072bf8b95d090";
        let begin = format!(
            "9400c3a97470635f626567696e96{txn}c40475736572c40b6465736372697074\
             696f6e81a46e6f7465a57370696b65c0a120"
        );
        assert_eq!(replay(&mut s, &begin), None);
        let three = format!(
            "9400c3a673746f72656194c4080000000000000001c408040c458ef1714222c453{}{txn}",
            hex_of(&pcounter(3))
        );
        assert_eq!(replay(&mut s, &three), None);
        let vote = replay(&mut s, &format!("9408c2a4766f746591{txn}"));
        let listed = format!(
            "940800a62e7265706c799183a36f6964c4080000000000000001a773657269616c\
             7392c408040c458ef1f76388c408040c458ef1714222a464617461c453{}",
            hex_of(&pcounter(3))
        );
        assert_eq!(vote, Some(hex(&listed)));
        let read = replay(
            &mut s,
            "9409c2aa6c6f616453657269616c92c4080000000000000001c408040c458ef1714222",
        );
        let one = format!("940900a62e7265706c79c453{}", hex_of(&pcounter(1)));
        assert_eq!(read, Some(hex(&one)));
        let committed = replay(
            &mut s,
            "940ac2aa6c6f616453657269616c92c4080000000000000001c408040c458ef1f76388",
        );
        let two = format!("940a00a62e7265706c79c453{}", hex_of(&pcounter(2)));
        assert_eq!(committed, Some(hex(&two)));
        let resolved = "8003581d0000005a4f44422e74657374732e436f6e666c6963745265736f6c\
             7574696f6e7100580800000050436f756e74657271018671024e8671032e80037d71\
             0458060000005f76616c756571054b04732e";
        let four = format!(
            "9400c3a673746f72656194c4080000000000000001c408040c458ef1f76388c453{resolved}{txn}"
        );
        assert_eq!(replay(&mut s, &four), None);
        let vote = replay(&mut s, &format!("940bc2a4766f746591{txn}"));
        assert_eq!(vote, Some(hex("940b00a62e7265706c7990")));

        let finish = message::decode(&hex(&format!("940cc2aa7470635f66696e69736891{txn}")));
        assert!(matches!(s.call(&finish.unwrap()), Outcome::Published));
        let tid = u64_of(&done(call(&mut s, "lastTransaction", vec![]))).unwrap();
        assert!(tid > COMMITTED);
        let load = done(call(&mut s, "loadBefore", vec![p64(1), p64(u64::MAX)]));
        assert_eq!(
            load,
            Value::Array(vec![Value::Binary(hex(resolved)), p64(tid), Value::Nil])
        );
        let status = s.storage.status().to_value();
        assert_eq!(count(&status, "conflicts_resolved"), 1);
        assert_eq!(count(&status, "conflicts"), 0);
    }

    fn hex_of(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    /// Begin the transaction `id` that stores `data` over `serial` for each
    /// `(oid, serial)` of `stores`, and vote. Return the outcome of the
    /// vote.
    fn store_and_vote(s: &mut Client, id: i64, stores: &[(u64, u64)], data: &[u8]) -> Outcome {
        begin(s, id, None);
        for &(oid, serial) in stores {
            store(s, id, oid, serial, data);
        }
        call(s, "vote", vec![Value::from(id)])
    }

    #[test]
    fn a_stale_read_current_serial_is_never_resolved() {
        let mut s = registered("resolve-read-current");
        let one = commit(&mut s, 1, 1, 0, MAPPING).unwrap();
        let two = commit(&mut s, 2, 2, 0, MAPPING).unwrap();
        let one_later = commit(&mut s, 3, 1, one, MAPPING).unwrap();
        let two_later = commit(&mut s, 4, 2, two, MAPPING).unwrap();
        // The transaction read the oid 1 at a stale serial, and writes the
        // oid 2 over a stale serial that the client could resolve.
        begin(&mut s, 5, None);
        let check = vec![p64(1), p64(one), Value::from(5)];
        done(call(&mut s, "checkCurrentSerialInTransaction", check));
        store(&mut s, 5, 2, two, MAPPING);
        let fault = failed(call(&mut s, "vote", vec![Value::from(5)]));
        assert_eq!(
            fault,
            Fault::conflict(
                READ_CONFLICT_ERROR,
                READ_CONFLICT,
                Oid(1),
                Some((Tid(one_later), Tid(one))),
                None,
            )
        );
        // The failed vote ends the transaction and releases its locks.
        assert!(s.txn.open.is_none());
        commit(&mut s, 6, 2, two_later, MAPPING).unwrap();
    }

    #[test]
    fn the_second_vote_checks_every_serial_again() {
        let mut s = registered("resolve-second-vote");
        let first = commit(&mut s, 1, 1, 0, MAPPING).unwrap();
        let second = commit(&mut s, 2, 1, first, MAPPING).unwrap();
        let listed = done(store_and_vote(&mut s, 3, &[(1, first)], b"mine"));
        let conflicts = listed.as_array().unwrap();
        assert_eq!(conflicts.len(), 1);
        assert_eq!(keys(&conflicts[0]), ["oid", "serials", "data"]);
        assert_eq!(get(&conflicts[0], "oid"), &p64(1));
        assert_eq!(
            get(&conflicts[0], "serials"),
            &Value::Array(vec![p64(second), p64(first)])
        );
        // The data is the pickle of the client, not the committed one.
        assert_eq!(get(&conflicts[0], "data"), &Value::Binary(b"mine".to_vec()));
        // The transaction keeps its locks: it is still open.
        assert!(s.txn.open.is_some());

        // A second vote without a resolved pickle fails on the stale
        // serial, and the transaction ends.
        let fault = failed(call(&mut s, "vote", vec![Value::from(3)]));
        assert_eq!(fault.class, CONFLICT_ERROR);
        assert_eq!(item(&fault, "oid"), p64(1));
        assert_eq!(
            item(&fault, "serials"),
            Value::Array(vec![p64(second), p64(first)])
        );
        assert!(s.txn.open.is_none());
        let status = s.storage.status().to_value();
        assert_eq!(count(&status, "conflicts"), 1);
        assert_eq!(count(&status, "conflicts_resolved"), 0);
        commit(&mut s, 4, 1, second, MAPPING).unwrap();
    }

    #[test]
    fn a_conflict_that_is_not_resolvable_fails_the_first_vote() {
        let mut s = registered("resolve-not-resolvable");
        let first = commit(&mut s, 1, 1, 0, MAPPING).unwrap();
        commit(&mut s, 2, 2, 0, MAPPING).unwrap();
        commit(&mut s, 3, 1, first, MAPPING).unwrap();
        // The oid 1 can be resolved. The oid 2 is a new object for the
        // transaction, so its conflict cannot be resolved.
        begin(&mut s, 4, None);
        store(&mut s, 4, 1, first, MAPPING);
        store(&mut s, 4, 2, 0, MAPPING);
        let fault = failed(call(&mut s, "vote", vec![Value::from(4)]));
        assert_eq!(fault.class, CONFLICT_ERROR);
        assert_eq!(item(&fault, "oid"), p64(2));
        assert!(s.txn.open.is_none());
    }

    #[test]
    fn a_client_that_disconnects_between_the_votes_releases_the_locks() {
        let dir =
            std::env::temp_dir().join(format!("zoe-session-disconnect-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let options = Options {
            lock_timeout: std::time::Duration::from_millis(200),
            ..Options::default()
        };
        let store = Arc::new(Store::open_with(&dir, options).unwrap());
        let storage = Arc::new(NamedStorage::new("1", &dir, store, Queues::DEFAULT));
        let mut other = Client::new(Arc::clone(&storage), outbox());
        let register = vec![Value::from("1"), Value::Boolean(false)];
        done(call(&mut other, "register", register));
        let first = commit(&mut other, 1, 1, 0, MAPPING).unwrap();
        let second = commit(&mut other, 2, 1, first, MAPPING).unwrap();

        // A client votes on a stale serial on its own connection, gets the
        // conflict and closes the connection.
        let (calls, received) = tokio::sync::mpsc::channel(crate::protocol::QUEUE);
        let (replies, mut sent, _closing) = Outbox::new(1_000, 1_000);
        let session = {
            let storage = Arc::clone(&storage);
            std::thread::spawn(move || run(serving(storage), received, replies))
        };
        let send = |id: i64, oneway: bool, method: &str, args: Vec<Value>| {
            let call = Call {
                id: Value::from(id),
                oneway,
                method: method.into(),
                args,
                context: None,
            };
            calls.blocking_send(call).unwrap();
        };
        send(
            1,
            false,
            "register",
            vec![Value::from("1"), Value::Boolean(false)],
        );
        let begin = vec![Value::from(7), Value::Nil, Value::Nil, Value::Map(vec![])];
        send(0, true, "tpc_begin", begin);
        let data = Value::Binary(MAPPING.to_vec());
        send(
            0,
            true,
            "storea",
            vec![p64(1), p64(first), data, Value::from(7)],
        );
        send(2, false, "vote", vec![Value::from(7)]);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        runtime.block_on(sent.recv()).unwrap();
        let vote = runtime.block_on(sent.recv()).unwrap();
        let reply = rmpv::decode::read_value(&mut vote.to_vec().as_slice()).unwrap();
        assert_eq!(reply.as_array().unwrap()[0], Value::from(2));
        let conflicts = &reply.as_array().unwrap()[3];
        assert_eq!(conflicts.as_array().map(Vec::len), Some(1));
        assert_eq!(count(&storage.status().to_value(), "active_txns"), 1);
        // Between the two votes, the transaction holds the lock of the oid:
        // another vote on the oid waits past the lock timeout.
        let fault = commit(&mut other, 3, 1, second, MAPPING).unwrap_err();
        assert_eq!(fault.class, CONFLICT_ERROR);
        assert_eq!(item(&fault, "serials"), Value::Nil);
        drop(calls);
        session.join().unwrap();

        // The locks are free: a commit on the oid does not wait for them.
        commit(&mut other, 4, 1, second, MAPPING).unwrap();
        let status = storage.status().to_value();
        assert_eq!(count(&status, "active_txns"), 0);
        assert_eq!(count(&status, "aborts"), 1);
        assert_eq!(count(&status, "conflicts_resolved"), 0);
    }

    #[test]
    fn the_calls_that_wait_at_the_end_of_a_connection_still_run() {
        let storage = storage("calls-at-end", Queues::DEFAULT);
        let (calls, received) = tokio::sync::mpsc::channel(16);
        let (replies, _inbox, _closing) = Outbox::new(16, 16);
        let send = |oneway: bool, method: &str, args: Vec<Value>| {
            let call = Call {
                id: Value::from(1),
                oneway,
                method: method.into(),
                args,
                context: None,
            };
            calls.blocking_send(call).unwrap();
        };
        let oids = [1, 2, 3];
        send(
            false,
            "register",
            vec![Value::from("1"), Value::Boolean(false)],
        );
        let begin = vec![Value::from(7), Value::Nil, Value::Nil, Value::Map(vec![])];
        send(true, "tpc_begin", begin);
        for oid in oids {
            let data = Value::Binary(MAPPING.to_vec());
            send(true, "storea", vec![p64(oid), p64(0), data, Value::from(7)]);
        }
        send(false, "vote", vec![Value::from(7)]);
        send(false, "tpc_finish", vec![Value::from(7)]);
        // The connection ends before the session runs the first call.
        drop(calls);
        run(serving(Arc::clone(&storage)), received, replies);

        // The session ran each call, so the finish committed the three oids
        // together, and no lock stays.
        let tid = storage.invalidations.published();
        assert_ne!(tid, Tid(0));
        for oid in oids {
            assert_eq!(storage.store.load(Oid(oid)).unwrap().1, tid);
        }
        assert!(storage.store.lock_table_is_empty());
        let status = storage.status().to_value();
        assert_eq!(count(&status, "commits"), 1);
        assert_eq!(count(&status, "active_txns"), 0);
    }

    #[test]
    fn server_status_counts_the_conflicts_that_the_client_resolved() {
        let mut s = registered("resolve-count");
        let one = commit(&mut s, 1, 1, 0, MAPPING).unwrap();
        let two = commit(&mut s, 2, 2, 0, MAPPING).unwrap();
        let one_later = commit(&mut s, 3, 1, one, MAPPING).unwrap();
        let two_later = commit(&mut s, 4, 2, two, MAPPING).unwrap();
        let listed = done(store_and_vote(&mut s, 5, &[(1, one), (2, two)], MAPPING));
        assert_eq!(listed.as_array().map(Vec::len), Some(2));
        // The client stores each resolved pickle over the committed serial,
        // and the second vote passes.
        store(&mut s, 5, 1, one_later, b"resolved 1");
        store(&mut s, 5, 2, two_later, b"resolved 2");
        assert_eq!(
            done(call(&mut s, "vote", vec![Value::from(5)])),
            Value::Array(vec![])
        );
        let tid = finish(&mut s, Value::from(5));
        let status = s.storage.status().to_value();
        assert_eq!(count(&status, "conflicts_resolved"), 2);
        assert_eq!(count(&status, "conflicts"), 0);
        assert_eq!(count(&status, "commits"), 5);
        let load = done(call(&mut s, "loadBefore", vec![p64(2), p64(u64::MAX)]));
        assert_eq!(
            load,
            Value::Array(vec![
                Value::Binary(b"resolved 2".to_vec()),
                p64(tid),
                Value::Nil
            ])
        );

        // A conflict that the client aborts is not resolved.
        done(store_and_vote(&mut s, 6, &[(1, one)], MAPPING));
        done(call(&mut s, "tpc_abort", vec![Value::from(6)]));
        let status = s.storage.status().to_value();
        assert_eq!(count(&status, "conflicts_resolved"), 2);
        assert_eq!(count(&status, "aborts"), 1);
        commit(&mut s, 7, 1, tid, MAPPING).unwrap();
    }

    /// Returns the record of a `BTrees.Length.Length` of `value`, as ZODB
    /// writes it.
    fn length_record(value: u8) -> Vec<u8> {
        [
            &b"\x80\x03cBTrees.Length\nLength\nq\x00.\x80\x03K"[..],
            &[value],
            b".",
        ]
        .concat()
    }

    #[test]
    fn the_vote_gives_an_oid_that_tier_1_merged_as_a_bare_oid() {
        let name = "native-merge";
        let dir = std::env::temp_dir().join(format!("zoe-session-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Arc::new(Store::open(&dir).unwrap());
        let log_path = dir.join("resolutions.log");
        let log = Arc::new(ResolutionLog::open(&log_path).unwrap());
        let storage = NamedStorage::new("1", &dir, store, Queues::DEFAULT);
        let mut s = Client::new(Arc::new(storage.resolution_log(Some(log))), outbox());
        done(call(
            &mut s,
            "register",
            vec![Value::from("1"), Value::Boolean(false)],
        ));
        let read = commit(&mut s, 1, 1, 0, &length_record(10)).unwrap();
        commit(&mut s, 2, 1, read, &length_record(12)).unwrap();
        let reply = done(store_and_vote(&mut s, 3, &[(1, read)], &length_record(7)));
        assert_eq!(reply, Value::Array(vec![p64(1)]));
        let tid = finish(&mut s, Value::from(3));
        let load = done(call(&mut s, "loadBefore", vec![p64(1), p64(u64::MAX)]));
        assert_eq!(
            load,
            Value::Array(vec![Value::Binary(length_record(9)), p64(tid), Value::Nil])
        );
        let metrics = &s.storage.metrics;
        assert_eq!(get_counter(&metrics.conflicts_native), 1);
        assert_eq!(get_counter(&metrics.structural_merges[0][0][0]), 1);
        // The client resolved nothing.
        let status = s.storage.status().to_value();
        assert_eq!(count(&status, "conflicts_resolved"), 0);
        let text = std::fs::read_to_string(&log_path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 1, "{text}");
        assert!(lines[0].contains("\"storage\":\"1\""), "{text}");
        assert!(
            lines[0].contains("\"class\":\"BTrees.Length.Length\""),
            "{text}"
        );
        assert!(lines[0].contains("\"outcome\":\"merged\""), "{text}");
        assert!(lines[0].contains("\"user\":\"\""), "{text}");
        assert!(lines[0].contains("\"committed_user\":\"user-2\""), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_merge_of_tier_1_counts_at_the_vote_that_passes() {
        let mut s = registered("native-two-votes");
        let storage = Arc::clone(&s.storage);
        let native = || get_counter(&storage.metrics.conflicts_native);
        // The oid 1 is a Length, which tier 1 merges. The oid 2 is a
        // mapping, which only the client resolves.
        let length_read = commit(&mut s, 1, 1, 0, &length_record(10)).unwrap();
        commit(&mut s, 2, 1, length_read, &length_record(12)).unwrap();
        let mapping_read = commit(&mut s, 3, 2, 0, MAPPING).unwrap();
        let mapping_later = commit(&mut s, 4, 2, mapping_read, MAPPING).unwrap();
        for (id, abort) in [(5, true), (6, false)] {
            begin(&mut s, id, None);
            store(&mut s, id, 1, length_read, &length_record(7));
            store(&mut s, id, 2, mapping_read, MAPPING);
            let listed = done(call(&mut s, "vote", vec![Value::from(id)]));
            let listed = listed.as_array().unwrap();
            assert_eq!(listed.len(), 2, "{listed:?}");
            assert_eq!(listed[0], p64(1));
            // The first vote listed a conflict for the client, so nothing
            // counts yet.
            assert_eq!(native(), 0);
            if abort {
                done(call(&mut s, "tpc_abort", vec![Value::from(id)]));
                assert_eq!(native(), 0);
                continue;
            }
            store(&mut s, id, 2, mapping_later, b"resolved");
            assert_eq!(
                done(call(&mut s, "vote", vec![Value::from(id)])),
                Value::Array(vec![])
            );
            finish(&mut s, Value::from(id));
            assert_eq!(native(), 1);
            let status = s.storage.status().to_value();
            assert_eq!(count(&status, "conflicts_resolved"), 1);
        }
    }

    fn get_counter(counter: &AtomicU64) -> u64 {
        counter.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn registered(name: &str) -> Client {
        let mut s = session(name);
        done(call(
            &mut s,
            "register",
            vec![Value::from("1"), Value::Boolean(false)],
        ));
        s
    }

    /// Commit the transaction `id` that stores `data` for `oid` over
    /// `serial`. Return its tid, or the fault of its vote.
    fn commit(s: &mut Client, id: i64, oid: u64, serial: u64, data: &[u8]) -> Result<u64, Fault> {
        let user = Value::Binary(format!("user-{id}").into_bytes());
        let description = Value::Binary(b"description".to_vec());
        let id = Value::from(id);
        let begin = vec![id.clone(), user, description, Value::Map(vec![])];
        done(call(s, "tpc_begin", begin));
        let data = Value::Binary(data.to_vec());
        done(call(
            s,
            "storea",
            vec![p64(oid), p64(serial), data, id.clone()],
        ));
        match call(s, "vote", vec![id.clone()]) {
            Outcome::Done(_) => Ok(finish(s, id)),
            outcome => Err(failed(outcome)),
        }
    }

    /// The value of `key` in the map `map`.
    fn get<'a>(map: &'a Value, key: &str) -> &'a Value {
        map.as_map()
            .unwrap()
            .iter()
            .find(|(k, _)| k.as_str() == Some(key))
            .map(|(_, value)| value)
            .unwrap_or_else(|| panic!("no key {key}"))
    }

    /// The keys of the map `map`, in order.
    fn keys(map: &Value) -> Vec<&str> {
        map.as_map()
            .unwrap()
            .iter()
            .map(|(key, _)| key.as_str().unwrap())
            .collect()
    }

    fn count(status: &Value, key: &str) -> u64 {
        get(status, key).as_u64().unwrap()
    }

    /// Undo the transaction of the undo id `undo_id` in the transaction
    /// `id`. Return the reply of the vote, or its fault.
    fn undo_in(s: &mut Client, id: i64, undo_id: Value) -> Result<Value, Fault> {
        begin(s, id, None);
        done(call(s, "undoa", vec![undo_id, Value::from(id)]));
        match call(s, "vote", vec![Value::from(id)]) {
            Outcome::Done(reply) => {
                finish(s, Value::from(id));
                Ok(reply)
            }
            outcome => Err(failed(outcome)),
        }
    }

    #[test]
    fn an_undo_stages_at_the_vote_and_the_reply_gives_its_oids() {
        let mut s = registered("undo");
        let first = commit(&mut s, 1, 7, 0, b"one").unwrap();
        let second = commit(&mut s, 2, 7, first, b"two").unwrap();
        let log = done(call(
            &mut s,
            "undoLog",
            vec![Value::from(0), Value::from(1)],
        ));
        let entry = &log.as_array().unwrap()[0];
        assert_eq!(get(entry, "id"), &Value::Binary(undo_id(Tid(second))));
        assert_eq!(get(entry, "user_name"), &Value::Binary(b"user-2".to_vec()));
        let reply = undo_in(&mut s, 3, get(entry, "id").clone()).unwrap();
        assert_eq!(reply, Value::Array(vec![p64(7)]));
        let loaded = done(call(&mut s, "loadBefore", vec![p64(7), p64(u64::MAX >> 1)]));
        assert_eq!(
            loaded.as_array().unwrap()[0],
            Value::Binary(b"one".to_vec())
        );
        // The undo is in the log, newest first.
        let log = done(call(
            &mut s,
            "undoLog",
            vec![Value::from(0), Value::from(-20)],
        ));
        assert_eq!(log.as_array().unwrap().len(), 3);
    }

    #[test]
    fn an_undo_that_no_tier_merges_fails_the_vote_with_an_undo_error() {
        let mut s = registered("undo-error");
        let first = commit(&mut s, 1, 7, 0, b"one").unwrap();
        let second = commit(&mut s, 2, 7, first, b"two").unwrap();
        commit(&mut s, 3, 7, second, b"three").unwrap();
        let fault = undo_in(&mut s, 4, Value::Binary(undo_id(Tid(second)))).unwrap_err();
        assert_eq!(fault.class, "ZODB.POSException.MultipleUndoErrors");
        let errors = item(&fault, "_errs");
        let [error] = errors.as_array().unwrap().as_slice() else {
            panic!("one error: {fault:?}");
        };
        assert_eq!(error.as_array().unwrap()[0], p64(7));
        // An id that names no transaction fails the vote too.
        let fault = undo_in(&mut s, 5, Value::Binary(b"not an id".to_vec())).unwrap_err();
        assert_eq!(fault.class, "ZODB.POSException.MultipleUndoErrors");
    }

    #[test]
    fn undo_info_keeps_the_entries_of_its_specification() {
        let mut s = registered("undo-info");
        let first = commit(&mut s, 1, 7, 0, b"one").unwrap();
        commit(&mut s, 2, 7, first, b"two").unwrap();
        let specification = Value::Map(vec![(
            Value::from("user_name"),
            Value::Binary(b"user-1".to_vec()),
        )]);
        let args = vec![Value::from(0), Value::from(-20), specification];
        let info = done(call(&mut s, "undoInfo", args));
        let entries = info.as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(get(&entries[0], "id"), &Value::Binary(undo_id(Tid(first))));
        let all = done(call(&mut s, "undoInfo", vec![]));
        assert_eq!(all.as_array().unwrap().len(), 2);
    }

    #[test]
    fn undo_info_matches_a_missing_key_with_none_and_numbers_as_python() {
        let mut s = registered("undo-info-python");
        let first = commit(&mut s, 1, 7, 0, b"one").unwrap();
        commit(&mut s, 2, 7, first, b"two").unwrap();
        let info = |s: &mut Client, items: Vec<(Value, Value)>| {
            let args = vec![Value::from(0), Value::from(-20), Value::Map(items)];
            done(call(s, "undoInfo", args)).as_array().unwrap().len()
        };
        assert_eq!(info(&mut s, vec![(Value::from("missing"), Value::Nil)]), 2);
        assert_eq!(
            info(&mut s, vec![(Value::from("missing"), Value::from(1))]),
            0
        );
        assert!(python_eq(&Value::from(1), &Value::F64(1.0)));
        assert!(python_eq(&Value::Boolean(true), &Value::from(1)));
        assert!(!python_eq(&Value::from("x"), &Value::Binary(b"x".to_vec())));
        assert!(!python_eq(&Value::from(2), &Value::F64(2.5)));
    }

    #[test]
    fn the_vote_of_an_undo_of_many_oids_gives_each_oid_once() {
        const OIDS: u64 = 20_000;
        let mut s = registered("undo-many");
        begin(&mut s, 1, None);
        for oid in 1..=OIDS {
            store(&mut s, 1, oid, 0, b"one");
        }
        let tid = vote_and_finish(&mut s, 1).unwrap();
        let started = Instant::now();
        let reply = undo_in(&mut s, 2, Value::Binary(undo_id(Tid(tid)))).unwrap();
        let oids = reply.as_array().unwrap();
        assert_eq!(oids.len() as u64, OIDS);
        let distinct: HashSet<u64> = oids.iter().map(|oid| u64_of(oid).unwrap()).collect();
        assert_eq!(distinct.len() as u64, OIDS);
        // The undo is linear in its oids: a list scan for each oid would
        // take minutes in a debug build.
        assert!(started.elapsed() < std::time::Duration::from_secs(60));
    }

    #[test]
    fn get_info_describes_the_store() {
        let mut s = registered("get-info");
        let info = done(call(&mut s, "get_info", vec![]));
        assert_eq!(
            keys(&info),
            [
                "length",
                "size",
                "name",
                "supportsUndo",
                "supports_record_iternext",
                "interfaces"
            ]
        );
        assert_eq!(get(&info, "length"), &Value::from(0));
        assert!(count(&info, "size") > 0);
        assert_eq!(
            get(&info, "name").as_str(),
            s.storage.path.to_str(),
            "the name is the directory of the store"
        );
        assert_eq!(get(&info, "supportsUndo"), &Value::Boolean(true));
        assert_eq!(
            get(&info, "supports_record_iternext"),
            &Value::Boolean(true)
        );
        let interfaces: Vec<_> = get(&info, "interfaces")
            .as_array()
            .unwrap()
            .iter()
            .map(|pair| match pair.as_array().unwrap().as_slice() {
                [module, name] => (module.as_str().unwrap(), name.as_str().unwrap()),
                other => panic!("not a pair: {other:?}"),
            })
            .collect();
        assert_eq!(interfaces, INTERFACES);
        assert!(interfaces.contains(&("ZODB.interfaces", "IExternalGC")));
        assert!(interfaces.contains(&("ZODB.interfaces", "IStorageUndoable")));

        let sizes = done(call(&mut s, "get_size_info", vec![]));
        assert_eq!(keys(&sizes), ["length", "size"]);
        assert_eq!(get(&sizes, "size"), get(&info, "size"));
        let fault = failed(call(&mut s, "get_info", vec![Value::Nil]));
        assert_eq!(fault.class, "builtins.TypeError");
    }

    /// The `length` of `get_size_info`.
    fn length(s: &mut Client) -> u64 {
        count(&done(call(s, "get_size_info", vec![])), "length")
    }

    #[test]
    fn the_length_is_the_object_count_of_the_store() {
        let mut s = registered("length");
        assert_eq!(length(&mut s), 0);
        let first = commit(&mut s, 1, 1, 0, MAPPING).unwrap();
        commit(&mut s, 2, 2, 0, MAPPING).unwrap();
        assert_eq!(length(&mut s), 2);
        // A new revision is no new object.
        commit(&mut s, 3, 1, first, MAPPING).unwrap();
        assert_eq!(length(&mut s), 2);
        let info = done(call(&mut s, "get_info", vec![]));
        assert_eq!(get(&info, "length"), &Value::from(2));
    }

    #[test]
    fn get_info_gives_back_the_accepted_extensions_of_a_declaration() {
        let mut s = session("declared");
        let register =
            |declaration: Value| vec![Value::from("1"), Value::Boolean(false), declaration];
        // A declaration that is not a list of names fails, and the client
        // can register again.
        let fault = failed(call(&mut s, "register", register(Value::from("a"))));
        assert_eq!(fault.class, "builtins.TypeError");
        assert!(!s.session.is_registered());
        let declaration = Value::Array(vec![Value::from("unknown"), Value::from("unknown")]);
        assert_eq!(
            done(call(&mut s, "register", register(declaration))),
            p64(0)
        );
        let info = done(call(&mut s, "get_info", vec![]));
        assert_eq!(keys(&info).last(), Some(&"extensions"));
        assert_eq!(get(&info, "extensions"), &Value::Array(vec![]));
        let again = vec![Value::from("1"), Value::Boolean(false)];
        assert_eq!(
            failed(call(&mut s, "register", again)).class,
            "builtins.ValueError"
        );
    }

    /// Return a session registered with the declaration `declaration`.
    fn declaring(name: &str, read_only: bool, declaration: &[&str]) -> Client {
        let mut s = session(name);
        let names = declaration.iter().map(|name| Value::from(*name)).collect();
        let args = vec![
            Value::from("1"),
            Value::Boolean(read_only),
            Value::Array(names),
        ];
        done(call(&mut s, "register", args));
        s
    }

    /// Return a session on `storage`, registered with `declaration`.
    fn declaring_on(storage: &Arc<NamedStorage>, declaration: &[&str]) -> Client {
        let mut s = Client::new(Arc::clone(storage), outbox());
        let names = declaration.iter().map(|name| Value::from(*name)).collect();
        let args = vec![Value::from("1"), Value::Boolean(false), Value::Array(names)];
        done(call(&mut s, "register", args));
        s
    }

    fn lock_intent(s: &mut Client, oids: &[u64]) -> Outcome {
        let oids = oids.iter().map(|&oid| p64(oid)).collect();
        call(s, "lock_intent", vec![Value::Array(oids)])
    }

    #[test]
    fn lock_intent_needs_the_declaration_and_gives_the_current_serials() {
        let mut stock = registered("intent-stock");
        let fault = failed(lock_intent(&mut stock, &[1]));
        assert_eq!(fault.class, "builtins.ValueError");
        let storage = storage("intent-serials", Queues::DEFAULT);
        let mut s = declaring_on(&storage, &["lock_intent"]);
        let one = commit(&mut s, 1, 1, 0, MAPPING).unwrap();
        let serials = done(lock_intent(&mut s, &[1, 2]));
        assert_eq!(serials, Value::Array(vec![p64(one), p64(0)]));
        assert!(s.txn.is_open());
        // A lower oid, or one that the session holds, fails and takes
        // nothing.
        for oids in [&[1][..], &[2], &[0]] {
            let fault = failed(lock_intent(&mut s, oids));
            assert_eq!(fault.class, TRANSACTION_ERROR, "{oids:?}");
        }
        assert_eq!(done(call(&mut s, "unlock_intent", vec![])), Value::Nil);
        assert!(!s.txn.is_open());
    }

    #[test]
    fn the_next_transaction_adopts_the_intent_locks_of_the_session() {
        let storage = storage("intent-adopt", Queues::DEFAULT);
        let mut s = declaring_on(&storage, &["lock_intent"]);
        let mut other = declaring_on(&storage, &["lock_intent"]);
        let one = commit(&mut s, 1, 1, 0, MAPPING).unwrap();
        done(lock_intent(&mut s, &[1]));
        // The vote of the session takes its own intent lock at once.
        let voted = store_and_vote(&mut s, 2, &[(1, one)], MAPPING);
        assert!(matches!(voted, Outcome::Done(_)), "{voted:?}");
        // No lock_intent in an open transaction.
        let fault = failed(lock_intent(&mut s, &[5]));
        assert_eq!(fault.class, TRANSACTION_ERROR);
        let two = finish(&mut s, Value::from(2));
        // The end of the transaction released the lock: another session
        // takes it at once, and reads the commit.
        assert_eq!(
            done(lock_intent(&mut other, &[1])),
            Value::Array(vec![p64(two)])
        );
    }

    #[test]
    fn a_read_only_session_takes_no_intent_lock() {
        let mut s = declaring("intent-read-only", true, &["lock_intent"]);
        assert_eq!(failed(lock_intent(&mut s, &[1])), Fault::read_only());
    }

    #[test]
    fn a_session_that_holds_intent_locks_past_the_bound_loses_them() {
        let storage = storage("intent-bound", Queues::DEFAULT);
        let mut s = declaring_on(&storage, &["lock_intent"]);
        let mut other = declaring_on(&storage, &["lock_intent"]);
        done(lock_intent(&mut s, &[3]));
        assert!(
            s.txn
                .intent_hold_left()
                .is_some_and(|left| left <= INTENT_HOLD)
        );
        s.txn.drop_intents();
        assert!(!s.txn.is_open());
        assert_eq!(s.txn.intent_hold_left(), None);
        done(lock_intent(&mut other, &[3]));
    }

    #[test]
    fn a_begin_that_fails_releases_the_intent_locks_of_the_session() {
        let storage = storage("intent-failed-begin", Queues::DEFAULT);
        let mut s = declaring_on(&storage, &["lock_intent"]);
        done(lock_intent(&mut s, &[3]));
        // A status that the server does not support fails the begin.
        let begin = vec![
            Value::from(7),
            Value::Nil,
            Value::Nil,
            Value::Map(vec![]),
            Value::Nil,
            Value::from("u"),
        ];
        done(call(&mut s, "tpc_begin", begin));
        assert!(!s.txn.holds_intents());
        assert!(storage.store.lock_table_is_empty());
    }

    #[test]
    fn only_a_declaration_of_trace_context_lets_the_calls_have_a_trace_context() {
        assert!(!session("traces-unregistered").session.traces());
        let mut stock = session("traces-stock");
        done(call(
            &mut stock,
            "register",
            vec![Value::from("1"), Value::Boolean(false)],
        ));
        assert!(!stock.session.traces());
        let other = declaring("traces-other", false, &["sequences"]);
        assert!(!other.session.traces());
        let s = declaring("traces", false, &["sequences", "trace_context"]);
        assert!(s.session.traces());
    }

    fn sequence_next(name: Value) -> Call {
        Call {
            id: Value::from(1),
            oneway: false,
            method: "sequence_next".into(),
            args: vec![name],
            context: None,
        }
    }

    #[test]
    fn sequence_next_gives_the_values_of_each_name_from_one() {
        let mut s = declaring("sequence-next", false, &["sequences"]);
        let info = done(call(&mut s, "get_info", vec![]));
        assert_eq!(
            get(&info, "extensions"),
            &Value::Array(vec![Value::from("sequences")])
        );
        let values: Vec<_> = (0..150)
            .map(|_| done(s.call(&sequence_next(Value::from("intids")))))
            .collect();
        let expected: Vec<_> = (1..=150_u64).map(Value::from).collect();
        assert_eq!(values, expected);
        assert_eq!(
            done(s.call(&sequence_next(Value::from("lexicon")))),
            Value::from(1)
        );
        let fault = failed(s.call(&sequence_next(Value::Binary(b"intids".to_vec()))));
        assert_eq!(fault.class, "builtins.TypeError");
        let fault = failed(call(&mut s, "sequence_next", vec![]));
        assert_eq!(fault.class, "builtins.TypeError");
    }

    #[test]
    fn sequence_next_is_outside_the_transaction() {
        let mut s = declaring("sequence-abort", false, &["sequences"]);
        begin(&mut s, 7, None);
        let next = sequence_next(Value::from("intids"));
        assert_eq!(done(s.call(&next)), Value::from(1));
        done(call(&mut s, "tpc_abort", vec![Value::from(7)]));
        assert_eq!(done(s.call(&next)), Value::from(2));
    }

    #[test]
    fn a_read_only_connection_takes_no_value() {
        let mut s = declaring("sequence-read-only", true, &["sequences"]);
        let fault = failed(s.call(&sequence_next(Value::from("intids"))));
        assert_eq!(fault, Fault::read_only());
    }

    #[test]
    fn sequence_next_needs_the_declaration_in_both_dispatches() {
        let next = sequence_next(Value::from("intids"));
        for mut s in [
            registered("sequence-stock"),
            declaring("sequence-none", false, &[]),
            declaring("sequence-other", false, &["unknown"]),
        ] {
            let mut reply = Reply::new(s.session.outbox.blocking_room());
            let fault = failed(s.session.call(&next, &mut reply));
            assert_eq!(fault.class, "builtins.ValueError");
            assert_eq!(
                failed(s.txn.call(&mut next.clone())).class,
                "builtins.ValueError"
            );
            // The connection stays open, and the store gave no value.
            assert_eq!(done(call(&mut s, "ping", vec![])), Value::Nil);
            assert_eq!(s.store.sequence("intids").next().unwrap(), 1);
        }
    }

    fn bin(bytes: &[u8]) -> Value {
        Value::Binary(bytes.to_vec())
    }

    /// Commit, in the transaction `id`, the namespace call `method` with
    /// `args`. Return the tid.
    fn commit_kv(s: &mut Client, id: i64, method: &str, args: Vec<Value>) -> u64 {
        begin(s, id, None);
        assert_eq!(done(call(s, method, args)), Value::Nil);
        let voted = done(call(s, "vote", vec![Value::from(id)]));
        assert_eq!(voted, Value::Array(vec![]));
        finish(s, Value::from(id))
    }

    #[test]
    fn kv_set_commits_with_the_transaction_and_kv_get_reads_it() {
        let mut s = declaring("kv-set", false, &["kv"]);
        let info = done(call(&mut s, "get_info", vec![]));
        assert_eq!(
            get(&info, "extensions"),
            &Value::Array(vec![Value::from("kv")])
        );
        let get_title = || {
            vec![
                Value::from("durable"),
                Value::from("registry"),
                bin(b"title"),
            ]
        };
        assert_eq!(done(call(&mut s, "kv_get", get_title())), Value::Nil);
        let set = vec![
            Value::from("durable"),
            Value::from("registry"),
            bin(b"title"),
            bin(b"site"),
            Value::Nil,
        ];
        let tid = commit_kv(&mut s, 7, "kv_set", set);
        assert_eq!(done(call(&mut s, "kv_get", get_title())), bin(b"site"));
        let generation = vec![Value::from("registry")];
        assert_eq!(done(call(&mut s, "kv_generation", generation)), p64(tid));
        let ephemeral = vec![
            Value::from("ephemeral"),
            Value::from("sessions"),
            bin(b"s1"),
            bin(b"data"),
            Value::F64(60.5),
        ];
        commit_kv(&mut s, 8, "kv_set", ephemeral);
        let scan = vec![
            Value::from("ephemeral"),
            Value::from("sessions"),
            Value::Nil,
            Value::Nil,
            Value::from(1000),
        ];
        assert_eq!(
            done(call(&mut s, "kv_scan", scan)),
            Value::Array(vec![Value::Array(vec![bin(b"s1"), bin(b"data")])])
        );
        let delete = vec![
            Value::from("durable"),
            Value::from("registry"),
            bin(b"title"),
        ];
        commit_kv(&mut s, 9, "kv_delete", delete);
        assert_eq!(done(call(&mut s, "kv_get", get_title())), Value::Nil);
    }

    #[test]
    fn an_aborted_kv_set_writes_nothing() {
        let mut s = declaring("kv-abort", false, &["kv"]);
        begin(&mut s, 7, None);
        let set = vec![
            Value::from("durable"),
            Value::from("registry"),
            bin(b"title"),
            bin(b"site"),
            Value::Nil,
        ];
        assert_eq!(done(call(&mut s, "kv_set", set)), Value::Nil);
        done(call(&mut s, "tpc_abort", vec![Value::from(7)]));
        let get = vec![
            Value::from("durable"),
            Value::from("registry"),
            bin(b"title"),
        ];
        assert_eq!(done(call(&mut s, "kv_get", get)), Value::Nil);
    }

    #[test]
    fn a_bad_kv_set_fails_the_vote() {
        for (ttl, flavour, class) in [
            (Value::Nil, "ephemeral", "builtins.ValueError"),
            (Value::from(5), "durable", "builtins.ValueError"),
            (Value::from(-1), "ephemeral", "builtins.ValueError"),
            (Value::F64(f64::NAN), "ephemeral", "builtins.ValueError"),
            (Value::from("5"), "ephemeral", "builtins.TypeError"),
            (Value::from(5), "other", "builtins.ValueError"),
        ] {
            let mut s = declaring("kv-bad", false, &["kv"]);
            begin(&mut s, 7, None);
            let set = vec![
                Value::from(flavour),
                Value::from("sessions"),
                bin(b"k"),
                bin(b"v"),
                ttl.clone(),
            ];
            assert_eq!(done(call(&mut s, "kv_set", set)), Value::Nil);
            let fault = failed(call(&mut s, "vote", vec![Value::from(7)]));
            assert_eq!(fault.class, class, "{flavour} {ttl:?}");
        }
    }

    #[test]
    fn kv_set_outside_a_transaction_fails() {
        let mut s = declaring("kv-outside", false, &["kv"]);
        let set = vec![
            Value::from("durable"),
            Value::from("registry"),
            bin(b"k"),
            bin(b"v"),
            Value::Nil,
        ];
        assert_eq!(failed(call(&mut s, "kv_set", set)).class, TRANSACTION_ERROR);
    }

    #[test]
    fn a_bad_read_of_a_namespace_fails_and_the_connection_stays_open() {
        let mut s = declaring("kv-bad-read", false, &["kv"]);
        for (method, args, class) in [
            (
                "kv_get",
                vec![Value::from("durable"), Value::from("r"), Value::from("k")],
                "builtins.TypeError",
            ),
            (
                "kv_get",
                vec![Value::from("durable"), Value::from("r"), bin(b"")],
                "builtins.ValueError",
            ),
            (
                "kv_get",
                vec![Value::from("lasting"), Value::from("r"), bin(b"k")],
                "builtins.ValueError",
            ),
            (
                "kv_get",
                vec![Value::from(1), Value::from("r"), bin(b"k")],
                "builtins.TypeError",
            ),
            (
                "kv_get",
                vec![Value::from("durable"), Value::from("r")],
                "builtins.TypeError",
            ),
            (
                "kv_scan",
                vec![
                    Value::from("durable"),
                    Value::from("r"),
                    Value::Nil,
                    Value::Nil,
                    Value::from(0),
                ],
                "builtins.ValueError",
            ),
            (
                "kv_scan",
                vec![
                    Value::from("durable"),
                    Value::from("r"),
                    Value::Nil,
                    Value::Nil,
                    Value::from(-1),
                ],
                "builtins.ValueError",
            ),
            (
                "kv_scan",
                vec![
                    Value::from("durable"),
                    Value::from("r"),
                    Value::Nil,
                    Value::Nil,
                    Value::from("1"),
                ],
                "builtins.TypeError",
            ),
            (
                "kv_generation",
                vec![Value::from("")],
                "builtins.ValueError",
            ),
            (
                "kv_generation",
                vec![bin(b"registry")],
                "builtins.TypeError",
            ),
        ] {
            let fault = failed(call(&mut s, method, args.clone()));
            assert_eq!(fault.class, class, "{method} {args:?}");
            assert_eq!(done(call(&mut s, "ping", vec![])), Value::Nil);
        }
    }

    #[test]
    fn the_calls_of_the_namespaces_need_the_declaration_of_kv() {
        let get = Call {
            id: Value::from(1),
            oneway: false,
            method: "kv_get".into(),
            args: vec![Value::from("durable"), Value::from("r"), bin(b"k")],
            context: None,
        };
        let set = Call {
            id: Value::from(1),
            oneway: true,
            method: "kv_set".into(),
            args: vec![
                Value::from("durable"),
                Value::from("r"),
                bin(b"k"),
                bin(b"v"),
                Value::Nil,
            ],
            context: None,
        };
        for mut s in [
            registered("kv-stock"),
            declaring("kv-other", false, &["sequences"]),
        ] {
            let mut reply = Reply::new(s.session.outbox.blocking_room());
            assert_eq!(
                failed(s.session.call(&get, &mut reply)).class,
                "builtins.ValueError"
            );
            assert_eq!(
                failed(s.txn.call(&mut set.clone())).class,
                "builtins.ValueError"
            );
            assert_eq!(done(call(&mut s, "ping", vec![])), Value::Nil);
        }
    }

    #[test]
    fn a_read_only_connection_reads_the_namespaces_and_its_vote_fails() {
        let mut s = declaring("kv-read-only", true, &["kv"]);
        let get = vec![
            Value::from("durable"),
            Value::from("registry"),
            bin(b"title"),
        ];
        assert_eq!(done(call(&mut s, "kv_get", get)), Value::Nil);
        let set = vec![
            Value::from("durable"),
            Value::from("registry"),
            bin(b"title"),
            bin(b"site"),
            Value::Nil,
        ];
        begin(&mut s, 8, None);
        assert_eq!(done(call(&mut s, "kv_set", set)), Value::Nil);
        let fault = failed(call(&mut s, "vote", vec![Value::from(8)]));
        assert_eq!(fault, Fault::read_only());
    }

    #[test]
    fn the_extension_of_a_get_or_a_head_marks_the_transaction() {
        let ext = |method: Value| Value::Map(vec![(Value::from(REQUEST_METHOD), method)]);
        assert!(of_get(&ext(Value::from("GET"))));
        assert!(of_get(&ext(Value::from("head"))));
        assert!(!of_get(&ext(Value::from("POST"))));
        assert!(!of_get(&ext(Value::Binary(b"GET".to_vec()))));
        assert!(!of_get(&Value::Map(vec![])));
        assert!(!of_get(&Value::Nil));
    }

    #[test]
    fn ping_answers_nil_and_set_client_label_takes_one_label() {
        let mut s = registered("ping");
        assert_eq!(done(call(&mut s, "ping", vec![])), Value::Nil);
        let label = vec![Value::from("a label")];
        assert_eq!(done(call(&mut s, "set_client_label", label)), Value::Nil);
        let fault = failed(call(&mut s, "set_client_label", vec![]));
        assert_eq!(fault.class, "builtins.TypeError");
    }

    #[test]
    fn server_status_counts_as_zeo_counts() {
        let mut s = registered("status");
        let first = done(call(&mut s, "server_status", vec![]));
        assert_eq!(
            keys(&first),
            [
                "connections",
                "loads",
                "stores",
                "commits",
                "aborts",
                "active_txns",
                "lock_time",
                "conflicts",
                "conflicts_resolved",
                "start",
                "waiting",
                "timeout-thread-is-alive",
                "last-transaction"
            ]
        );
        assert_eq!(count(&first, "connections"), 1);
        assert_eq!(
            get(&first, "last-transaction"),
            &Value::from("0".repeat(16))
        );

        let tid = commit(&mut s, 1, 1, 0, b"x").unwrap();
        // A load of a record and a load of an oid without a record.
        done(call(&mut s, "loadBefore", vec![p64(1), p64(u64::MAX)]));
        failed(call(&mut s, "loadBefore", vec![p64(9), p64(u64::MAX)]));
        // A transaction that is open, then aborts.
        let begin = vec![Value::from(2), Value::Nil, Value::Nil, Value::Map(vec![])];
        done(call(&mut s, "tpc_begin", begin));
        let open = done(call(&mut s, "server_status", vec![]));
        assert_eq!(count(&open, "active_txns"), 1);
        done(call(&mut s, "tpc_abort", vec![Value::from(2)]));
        // A stale serial fails the vote with a conflict.
        commit(&mut s, 3, 1, 0, b"y").unwrap_err();

        let last = done(call(&mut s, "server_status", vec![]));
        assert_eq!(count(&last, "commits"), 1);
        assert_eq!(count(&last, "stores"), 2);
        assert_eq!(count(&last, "loads"), 2);
        assert_eq!(count(&last, "aborts"), 1);
        assert_eq!(count(&last, "active_txns"), 0);
        assert_eq!(count(&last, "conflicts"), 1);
        assert_eq!(count(&last, "conflicts_resolved"), 0);
        assert_eq!(get(&last, "lock_time"), &Value::Nil);
        assert_eq!(count(&last, "waiting"), 0);
        assert_eq!(get(&last, "timeout-thread-is-alive"), &Value::from("stub"));
        assert_eq!(
            get(&last, "last-transaction"),
            &Value::from(format!("{tid:016x}"))
        );
    }

    #[test]
    fn a_closed_session_leaves_the_status() {
        let mut s = registered("closed");
        let storage = Arc::clone(&s.storage);
        let mut other = Client::new(Arc::clone(&storage), outbox());
        // A session that does not register is not a connection.
        let _unregistered = Client::new(Arc::clone(&storage), outbox());
        done(call(
            &mut other,
            "register",
            vec![Value::from("1"), Value::Boolean(false)],
        ));
        let begin = vec![Value::from(1), Value::Nil, Value::Nil, Value::Map(vec![])];
        done(call(&mut s, "tpc_begin", begin));
        let status = storage.status().to_value();
        assert_eq!(count(&status, "connections"), 2);
        assert_eq!(count(&status, "active_txns"), 1);

        drop(s);
        let status = storage.status().to_value();
        assert_eq!(count(&status, "connections"), 1);
        assert_eq!(count(&status, "active_txns"), 0);
        assert_eq!(count(&status, "aborts"), 1);
        drop(other);
        assert_eq!(count(&storage.status().to_value(), "connections"), 0);
    }

    #[test]
    fn history_is_newest_first_and_bounded() {
        let mut s = registered("history");
        let mut tids = Vec::new();
        let mut serial = 0;
        for n in 0..4 {
            serial = commit(&mut s, n, 1, serial, &[b'p'; 10][..=n as usize]).unwrap();
            tids.push(serial);
        }
        let history = done(call(&mut s, "history", vec![p64(1), Value::from(3)]));
        let history = history.as_array().unwrap();
        let got: Vec<_> = history
            .iter()
            .map(|entry| u64_of(get(entry, "tid")).unwrap())
            .collect();
        assert_eq!(got, [tids[3], tids[2], tids[1]]);
        let newest = &history[0];
        assert_eq!(
            keys(newest),
            ["time", "user_name", "description", "tid", "size"]
        );
        assert_eq!(
            get(newest, "time"),
            &Value::F64(clock::tid_time(Tid(tids[3])))
        );
        assert_eq!(get(newest, "user_name"), &Value::Binary(b"user-3".to_vec()));
        assert_eq!(
            get(newest, "description"),
            &Value::Binary(b"description".to_vec())
        );
        assert_eq!(get(newest, "size"), &Value::from(4));

        let len = |reply: Value| reply.as_array().unwrap().len();
        // The size is 1 by default. A size of 0 or less gives no entry.
        assert_eq!(len(done(call(&mut s, "history", vec![p64(1)]))), 1);
        assert_eq!(
            len(done(call(&mut s, "history", vec![p64(1), Value::from(10)]))),
            4
        );
        for size in [0, -1] {
            let args = vec![p64(1), Value::from(size)];
            assert_eq!(len(done(call(&mut s, "history", args))), 0);
        }
        let fault = failed(call(&mut s, "history", vec![p64(9), Value::from(1)]));
        assert_eq!(fault.class, "ZODB.POSException.POSKeyError");
    }

    #[test]
    fn history_of_an_oid_that_is_not_8_bytes_is_a_poskeyerror_before_the_size() {
        let mut s = registered("history-bad-oid");
        commit(&mut s, 1, 1, 0, b"data").unwrap();
        let oids = [
            Value::Nil,
            Value::from("\0\0\0\0\0\0\0\x01"),
            Value::Binary(vec![0; 7]),
        ];
        for oid in oids {
            for size in [Value::Nil, Value::from(1)] {
                let fault = failed(call(&mut s, "history", vec![oid.clone(), size]));
                assert_eq!(fault, Fault::no_object(oid.clone()));
            }
            let fault = failed(call(&mut s, "history", vec![oid.clone()]));
            assert_eq!(fault, Fault::no_object(oid));
        }
        // A good oid with a size that is not an int still fails with
        // `TypeError`.
        let fault = failed(call(&mut s, "history", vec![p64(1), Value::Nil]));
        assert_eq!(fault, Fault::type_error("size is not an int"));
        let fault = failed(call(&mut s, "history", vec![]));
        assert_eq!(fault.class, "builtins.TypeError");
    }

    #[test]
    fn load_serial_reads_one_revision() {
        let mut s = registered("load-serial");
        let first = commit(&mut s, 1, 1, 0, b"first").unwrap();
        commit(&mut s, 2, 1, first, b"second").unwrap();
        assert_eq!(
            done(call(&mut s, "loadSerial", vec![p64(1), p64(first)])),
            Value::Binary(b"first".to_vec())
        );
        let fault = failed(call(&mut s, "loadSerial", vec![p64(1), p64(0)]));
        assert_eq!(fault, Fault::from(ZoeError::NotFound { oid: Oid(1) }));
    }

    #[test]
    fn record_iternext_walks_the_current_records() {
        let mut s = registered("record-iternext");
        let fault = failed(call(&mut s, "record_iternext", vec![Value::Nil]));
        assert_eq!(fault.class, "builtins.ValueError");

        let first = commit(&mut s, 1, 1, 0, b"one").unwrap();
        let third = commit(&mut s, 2, 3, 0, b"three").unwrap();
        let later = commit(&mut s, 3, 1, first, b"later").unwrap();
        assert_eq!(
            done(call(&mut s, "record_iternext", vec![Value::Nil])),
            Value::Array(vec![
                p64(1),
                p64(later),
                Value::Binary(b"later".to_vec()),
                p64(3)
            ])
        );
        assert_eq!(
            done(call(&mut s, "record_iternext", vec![p64(2)])),
            Value::Array(vec![
                p64(3),
                p64(third),
                Value::Binary(b"three".to_vec()),
                Value::Nil
            ])
        );
    }

    /// A registered session of `storage`. Its frames wait in the receiver
    /// for the test.
    fn connected(storage: &Arc<NamedStorage>) -> (Client, Inbox) {
        let (outbox, frames, _closing) = Outbox::new(1_000, 1_000);
        let mut s = Client::new(Arc::clone(storage), outbox);
        done(call(
            &mut s,
            "register",
            vec![Value::from("1"), Value::Boolean(false)],
        ));
        (s, frames)
    }

    /// Take the messages that wait for a client, as `(method, args)`.
    fn messages(frames: &mut Inbox) -> Vec<(String, Value)> {
        let mut taken = Vec::new();
        while let Some(frame) = frames.try_recv() {
            let message = rmpv::decode::read_value(&mut frame.to_vec().as_slice()).unwrap();
            let Value::Array(parts) = message else {
                panic!("a message is not an array: {message:?}");
            };
            let [_, _, method, args] = <[Value; 4]>::try_from(parts).unwrap();
            taken.push((method.as_str().unwrap().to_owned(), args));
        }
        taken
    }

    /// The invalidation of the commit `tid` of `oids`.
    fn invalidation(tid: u64, oids: &[u64]) -> (String, Value) {
        let oids = oids.iter().map(|oid| p64(*oid)).collect();
        (
            "invalidateTransaction".to_owned(),
            Value::Array(vec![p64(tid), Value::Array(oids)]),
        )
    }

    #[test]
    fn the_other_sessions_get_the_invalidation_and_the_committer_its_reply() {
        let storage = storage("publish", Queues::DEFAULT);
        let (mut writer, mut own) = connected(&storage);
        let (_reader, mut others) = connected(&storage);
        let first = commit(&mut writer, 1, 1, 0, b"x").unwrap();

        let own = messages(&mut own);
        let methods: Vec<&str> = own.iter().map(|(method, _)| method.as_str()).collect();
        assert_eq!(methods, ["info", ".reply"]);
        let info = &own[0].1.as_array().unwrap()[0];
        assert_eq!(keys(info), ["length", "size"]);
        assert_eq!(own[1].1, p64(first));
        assert_eq!(messages(&mut others), [invalidation(first, &[1])]);

        // The reader registers after the commit: it gets the later commit.
        let (_late, mut late) = connected(&storage);
        let second = commit(&mut writer, 2, 2, 0, b"y").unwrap();
        assert_eq!(messages(&mut others), [invalidation(second, &[2])]);
        assert_eq!(messages(&mut late), [invalidation(second, &[2])]);
    }

    #[test]
    fn a_load_reply_goes_out_before_the_invalidation_of_a_later_commit() {
        let storage = storage("load-order", Queues::DEFAULT);
        let (mut writer, _own) = connected(&storage);
        let (mut reader, mut frames) = connected(&storage);
        let first = commit(&mut writer, 1, 1, 0, b"old").unwrap();
        assert_eq!(messages(&mut frames), [invalidation(first, &[1])]);

        let current = done(call(&mut reader, "loadBefore", vec![p64(1), p64(u64::MAX)]));
        assert_eq!(current.as_array().unwrap()[2], Value::Nil);
        assert_eq!(reader.sent, Some(true));
        // A commit after the read goes out after the reply.
        let second = commit(&mut writer, 2, 1, first, b"new").unwrap();
        let messages = messages(&mut frames);
        assert_eq!(messages[0].0, ".reply");
        assert_eq!(messages[1], invalidation(second, &[1]));
    }

    #[test]
    fn a_load_shows_only_the_commits_that_the_hub_published() {
        let storage = storage("load-published", Queues::DEFAULT);
        let (mut writer, _own) = connected(&storage);
        let (mut reader, mut frames) = connected(&storage);
        let first = commit(&mut writer, 1, 1, 0, b"old").unwrap();
        assert_eq!(messages(&mut frames), [invalidation(first, &[1])]);

        // A commit that the store made visible and the hub did not publish
        // yet, as while its session reads its oids.
        let mut txn = storage.store.tpc_begin(b"", b"", b"").unwrap();
        txn.store(Oid(1), Tid(first), b"new").unwrap();
        txn.vote().unwrap();
        let hidden = txn.finish_after().unwrap();
        assert_eq!(hidden.previous, Tid(first));

        // The current record is the published one, with no next serial.
        let old =
            |next: Value| Value::Array(vec![Value::Binary(b"old".to_vec()), p64(first), next]);
        let load = vec![p64(1), p64(u64::MAX)];
        let current = done(call(&mut reader, "loadBefore", load.clone()));
        assert_eq!(current, old(Value::Nil));
        // A load before the hidden commit does not name it.
        let before = vec![p64(1), p64(hidden.tid.0)];
        let previous = done(call(&mut reader, "loadBefore", before.clone()));
        assert_eq!(previous, old(Value::Nil));

        // The hub publishes the commit: its invalidation comes after the
        // replies, and later loads show it.
        storage.invalidations.publish(Commit {
            previous: hidden.previous,
            tid: hidden.tid,
            origin: None,
            outbox: outbox(),
            own: Vec::new(),
            others: Others::Send(message::notify(
                "invalidateTransaction",
                vec![p64(hidden.tid.0), Value::Array(vec![p64(1)])],
            )),
        });
        let current = done(call(&mut reader, "loadBefore", load));
        let new = Value::Array(vec![
            Value::Binary(b"new".to_vec()),
            p64(hidden.tid.0),
            Value::Nil,
        ]);
        assert_eq!(current, new);
        let previous = done(call(&mut reader, "loadBefore", before));
        assert_eq!(previous, old(p64(hidden.tid.0)));
        let methods: Vec<String> = messages(&mut frames).into_iter().map(|(m, _)| m).collect();
        assert_eq!(
            methods,
            [
                ".reply",
                ".reply",
                "invalidateTransaction",
                ".reply",
                ".reply"
            ]
        );
    }

    #[test]
    fn a_commit_without_oids_sends_an_empty_invalidation() {
        let storage = storage("empty-commit", Queues::DEFAULT);
        let (mut writer, mut own) = connected(&storage);
        let (_reader, mut others) = connected(&storage);
        begin(&mut writer, 1, None);
        let tid = vote_and_finish(&mut writer, 1).unwrap();
        assert_eq!(messages(&mut others), [invalidation(tid, &[])]);
        let methods: Vec<String> = messages(&mut own).into_iter().map(|(m, _)| m).collect();
        assert_eq!(methods, ["info", ".reply"]);
    }

    #[test]
    fn a_finish_that_fails_publishes_nothing_and_blocks_nothing() {
        let storage = storage("failed-finish", Queues::DEFAULT);
        let (mut writer, mut own) = connected(&storage);
        let (_reader, mut others) = connected(&storage);
        let id = Value::from(1);
        let begin = vec![id.clone(), Value::Nil, Value::Nil, Value::Map(vec![])];
        done(call(&mut writer, "tpc_begin", begin));
        let data = Value::Binary(b"x".to_vec());
        done(call(
            &mut writer,
            "storea",
            vec![p64(1), p64(0), data, id.clone()],
        ));
        // A finish before the vote fails in the core.
        let fault = failed(call(&mut writer, "tpc_finish", vec![id]));
        assert_eq!(fault.class, "ZODB.POSException.StorageTransactionError");
        assert!(messages(&mut own).is_empty());
        assert!(messages(&mut others).is_empty());
        assert_eq!(storage.invalidations.published(), Tid(0));

        let tid = commit(&mut writer, 2, 1, 0, b"x").unwrap();
        assert_eq!(messages(&mut others), [invalidation(tid, &[1])]);
    }

    #[test]
    fn get_invalidations_covers_the_gap_or_gives_nil() {
        let queues = Queues {
            invalidations: 2,
            ..Queues::DEFAULT
        };
        let mut s = Client::new(storage("get-invalidations", queues), outbox());
        done(call(
            &mut s,
            "register",
            vec![Value::from("1"), Value::Boolean(false)],
        ));
        let first = commit(&mut s, 1, 1, 0, b"a").unwrap();
        let second = commit(&mut s, 2, 2, 0, b"b").unwrap();
        commit(&mut s, 3, 3, 0, b"c").unwrap();
        let last = commit(&mut s, 4, 1, first, b"d").unwrap();
        let get = |s: &mut Client, tid: Value| done(call(s, "getInvalidations", vec![tid]));

        // Two commits after `second` wrote the oids 3 and 1.
        assert_eq!(
            get(&mut s, p64(second)),
            Value::Array(vec![p64(last), Value::Array(vec![p64(1), p64(3)])])
        );
        // Three commits come after `first`: more than the queue size.
        assert_eq!(get(&mut s, p64(first)), Value::Nil);
        let nothing = Value::Array(vec![p64(last), Value::Array(vec![])]);
        assert_eq!(get(&mut s, p64(last)), nothing);
        assert_eq!(get(&mut s, p64(u64::MAX)), nothing);
        let fault = failed(call(&mut s, "getInvalidations", vec![Value::from(1)]));
        assert_eq!(fault.class, "builtins.TypeError");
    }

    #[test]
    fn get_invalidations_covers_a_young_tid_past_the_queue_size() {
        let queues = Queues {
            invalidations: 2,
            invalidation_age: std::time::Duration::from_secs(3_600),
            ..Queues::DEFAULT
        };
        let mut s = Client::new(storage("invalidation-age", queues), outbox());
        done(call(
            &mut s,
            "register",
            vec![Value::from("1"), Value::Boolean(false)],
        ));
        let first = commit(&mut s, 1, 1, 0, b"a").unwrap();
        let second = commit(&mut s, 2, 2, 0, b"b").unwrap();
        commit(&mut s, 3, 3, 0, b"c").unwrap();
        commit(&mut s, 4, 2, second, b"d").unwrap();
        let last = commit(&mut s, 5, 1, first, b"e").unwrap();
        let get = |s: &mut Client, tid: Value| done(call(s, "getInvalidations", vec![tid]));

        // Four commits come after `first`, and each oid comes once.
        assert_eq!(
            get(&mut s, p64(first)),
            Value::Array(vec![p64(last), Value::Array(vec![p64(1), p64(2), p64(3)])])
        );
        // The tid 0 is from 1900: older than the age.
        assert_eq!(get(&mut s, p64(0)), Value::Nil);
    }

    #[test]
    fn concurrent_commits_reach_each_session_in_commit_order() {
        const WRITERS: u64 = 4;
        const COMMITS: u64 = 25;
        let storage = storage("concurrent", Queues::DEFAULT);
        let (_idle, mut idle) = connected(&storage);
        let registered = std::sync::Barrier::new(WRITERS as usize);
        let mut queues: Vec<Inbox> = std::thread::scope(|scope| {
            let writers: Vec<_> = (0..WRITERS)
                .map(|writer| {
                    let (storage, registered) = (&storage, &registered);
                    scope.spawn(move || {
                        let (mut s, frames) = connected(storage);
                        // Each session registers before the first commit,
                        // so each one gets every commit.
                        registered.wait();
                        for n in 0..COMMITS {
                            let oid = 1 + writer * COMMITS + n;
                            commit(&mut s, n as i64, oid, 0, b"x").unwrap();
                        }
                        // The session stays in the hub until all commit.
                        (s, frames)
                    })
                })
                .collect();
            let joined: Vec<_> = writers.into_iter().map(|w| w.join().unwrap()).collect();
            joined.into_iter().map(|(_, frames)| frames).collect()
        });
        // Each writer sees every commit once, in commit order: its own
        // replies among the invalidations of the others.
        let mut all = None;
        for frames in &mut queues {
            let tids = tids(frames);
            assert_eq!(tids.len() as u64, WRITERS * COMMITS);
            assert!(tids.windows(2).all(|pair| pair[0] < pair[1]), "{tids:?}");
            assert!(all.is_none_or(|all| all == tids));
            all = Some(tids);
        }
        let seen: Vec<u64> = messages(&mut idle)
            .into_iter()
            .map(|(_, args)| u64_of(&args.as_array().unwrap()[0]).unwrap())
            .collect();
        assert_eq!(Some(seen), all);
    }

    /// Return the tids of the replies and the invalidations that wait for
    /// a client, in their order.
    fn tids(frames: &mut Inbox) -> Vec<u64> {
        messages(frames)
            .into_iter()
            .filter_map(|(method, args)| match method.as_str() {
                ".reply" => Some(u64_of(&args).unwrap()),
                "invalidateTransaction" => Some(u64_of(&args.as_array().unwrap()[0]).unwrap()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_late_session_gets_exactly_the_commits_after_its_registration() {
        const COMMITS: u64 = 10;
        let storage = storage("late", Queues::DEFAULT);
        let (_idle, mut idle) = connected(&storage);
        let early = std::sync::Barrier::new(2);
        let (early_frames, late_registered, late_frames) = std::thread::scope(|scope| {
            let (storage, early) = (&storage, &early);
            let first = scope.spawn(move || {
                let (mut s, frames) = connected(storage);
                for n in 0..COMMITS {
                    commit(&mut s, n as i64, 1 + n, 0, b"x").unwrap();
                    if n == 4 {
                        early.wait();
                    }
                }
                (s, frames)
            });
            let late = scope.spawn(move || {
                // This session registers after 5 commits of the first one.
                early.wait();
                let (outbox, frames, _closing) = Outbox::new(1_000, 1_000);
                let mut s = Client::new(Arc::clone(storage), outbox);
                let register = vec![Value::from("1"), Value::Boolean(false)];
                let registered = u64_of(&done(call(&mut s, "register", register))).unwrap();
                for n in 0..COMMITS {
                    commit(&mut s, n as i64, 100 + n, 0, b"x").unwrap();
                }
                (s, registered, frames)
            });
            let (_first, early_frames) = first.join().unwrap();
            let (_late, registered, late_frames) = late.join().unwrap();
            (early_frames, registered, late_frames)
        });
        let all: Vec<u64> = messages(&mut idle)
            .into_iter()
            .map(|(_, args)| u64_of(&args.as_array().unwrap()[0]).unwrap())
            .collect();
        assert_eq!(all.len() as u64, 2 * COMMITS);
        assert!(all.windows(2).all(|pair| pair[0] < pair[1]), "{all:?}");
        // The early session registered before every commit.
        assert_eq!(tids(&mut { early_frames }), all);
        // The late one gets each commit after the tid of its registration,
        // once, in commit order, and no earlier commit.
        let after: Vec<u64> = all
            .iter()
            .copied()
            .filter(|tid| *tid > late_registered)
            .collect();
        assert!(after.len() as u64 <= 2 * COMMITS - 5);
        assert_eq!(tids(&mut { late_frames }), after);
    }

    /// Return the files under the blob directory of `storage`, but the
    /// layout file. A blob directory that does not exist holds no file.
    fn blob_tree(storage: &NamedStorage) -> Vec<std::path::PathBuf> {
        fn walk(dir: &std::path::Path, found: &mut Vec<std::path::PathBuf>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, found);
                } else if path.file_name().unwrap() != ".layout" {
                    found.push(path);
                }
            }
        }
        let mut found = Vec::new();
        walk(&storage.path.join("blobs"), &mut found);
        found
    }

    /// Begin the transaction `id`, store a pickle for `oid` and open an
    /// upload with one chunk, as a stock client does.
    fn upload(s: &mut Client, id: i64, oid: u64, chunk: &[u8]) {
        begin(s, id, None);
        store(s, id, oid, 0, b"pickle");
        done(call(s, "storeBlobStart", vec![]));
        done(call(
            s,
            "storeBlobChunk",
            vec![Value::Binary(chunk.to_vec())],
        ));
    }

    fn store_blob_end(s: &mut Client, id: i64, oid: u64, data: &[u8]) -> Outcome {
        let data = Value::Binary(data.to_vec());
        call(
            s,
            "storeBlobEnd",
            vec![p64(oid), p64(0), data, Value::from(id)],
        )
    }

    #[test]
    fn an_uploaded_blob_commits_with_the_pickle_of_storea() {
        let mut s = registered("blob-commit");
        upload(&mut s, 1, 1, b"the blob");
        done(store_blob_end(&mut s, 1, 1, b"pickle"));
        let tid = Tid(vote_and_finish(&mut s, 1).unwrap());
        assert_eq!(s.store.load_serial(Oid(1), tid).unwrap(), b"pickle");
        let path = s.store.blob_path(Oid(1), tid).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"the blob");
        assert_eq!(blob_tree(&s.storage), [path]);
        // A stock client counts the storea, not the blob.
        assert_eq!(count(&s.storage.status().to_value(), "stores"), 1);
    }

    #[test]
    fn a_commit_with_a_blob_publishes_its_invalidation() {
        let storage = storage("blob-invalidation", Queues::DEFAULT);
        let (mut writer, mut own) = connected(&storage);
        let (_reader, mut others) = connected(&storage);
        upload(&mut writer, 1, 7, b"the blob");
        done(store_blob_end(&mut writer, 1, 7, b"pickle"));
        let tid = vote_and_finish(&mut writer, 1).unwrap();
        assert_eq!(messages(&mut others), [invalidation(tid, &[7])]);
        let methods: Vec<String> = messages(&mut own).into_iter().map(|(m, _)| m).collect();
        assert_eq!(methods, ["info", ".reply"]);
    }

    #[test]
    fn an_aborted_or_dropped_upload_leaves_no_file() {
        let none = Vec::<std::path::PathBuf>::new();
        // An abort after storeBlobEnd: the core took the file.
        let mut s = registered("blob-abort");
        upload(&mut s, 1, 1, b"the blob");
        done(store_blob_end(&mut s, 1, 1, b"pickle"));
        assert_eq!(
            blob_tree(&s.storage).len(),
            1,
            "the transaction holds the file"
        );
        done(call(&mut s, "tpc_abort", vec![Value::from(1)]));
        assert_eq!(blob_tree(&s.storage), none);

        // An abort before storeBlobEnd: the upload file is still open.
        upload(&mut s, 2, 1, b"the blob");
        assert_eq!(blob_tree(&s.storage).len(), 1, "the upload file");
        done(call(&mut s, "tpc_abort", vec![Value::from(2)]));
        assert_eq!(blob_tree(&s.storage), none);

        // The end of the connection, during the upload and after it.
        upload(&mut s, 3, 1, b"the blob");
        let storage = Arc::clone(&s.storage);
        drop(s);
        assert_eq!(blob_tree(&storage), none);
        let (mut s, _frames) = connected(&storage);
        upload(&mut s, 4, 1, b"the blob");
        done(store_blob_end(&mut s, 4, 1, b"pickle"));
        drop(s);
        assert_eq!(blob_tree(&storage), none);

        // A finish with an open upload commits the pickle alone.
        let (mut s, _frames) = connected(&storage);
        upload(&mut s, 5, 1, b"the blob");
        let tid = Tid(vote_and_finish(&mut s, 5).unwrap());
        assert_eq!(s.store.load_serial(Oid(1), tid).unwrap(), b"pickle");
        assert_eq!(blob_tree(&storage), none);
    }

    #[test]
    fn a_blob_that_the_core_refuses_fails_the_vote_and_leaves_no_file() {
        let none = Vec::<std::path::PathBuf>::new();
        let mut s = registered("blob-refused");
        upload(&mut s, 1, 1, b"the blob");
        // The core refuses an empty pickle, and the drop of the file removes it.
        done(store_blob_end(&mut s, 1, 1, b""));
        assert_eq!(blob_tree(&s.storage), none);
        let fault = failed(call(&mut s, "vote", vec![Value::from(1)]));
        assert_eq!(fault.class, TRANSACTION_ERROR);
        assert!(s.txn.open.is_none());

        // An oid that is not 8 bytes fails the vote, as in storea.
        upload(&mut s, 2, 1, b"the blob");
        let data = Value::Binary(b"pickle".to_vec());
        let end = vec![Value::from(1), p64(0), data, Value::from(2)];
        done(call(&mut s, "storeBlobEnd", end));
        assert_eq!(blob_tree(&s.storage), none);
        let fault = failed(call(&mut s, "vote", vec![Value::from(2)]));
        assert_eq!(fault.class, "builtins.TypeError");
    }

    #[test]
    fn after_an_error_of_the_transaction_the_blob_calls_write_no_file() {
        let none = Vec::<std::path::PathBuf>::new();
        // A read-only connection: tpc_begin fails the transaction.
        let mut s = session("blob-read-only");
        let register = vec![Value::from("1"), Value::Boolean(true)];
        done(call(&mut s, "register", register));
        upload(&mut s, 1, 1, b"the blob");
        done(store_blob_end(&mut s, 1, 1, b"pickle"));
        assert_eq!(blob_tree(&s.storage), none);
        let fault = failed(call(&mut s, "vote", vec![Value::from(1)]));
        assert_eq!(fault, Fault::read_only());

        // A store that fails during an upload: the end removes the file.
        let mut s = registered("blob-failed-store");
        let chunk = || vec![Value::Binary(b"part".to_vec())];
        begin(&mut s, 1, None);
        done(call(&mut s, "storeBlobStart", vec![]));
        done(call(&mut s, "storeBlobChunk", chunk()));
        assert_eq!(blob_tree(&s.storage).len(), 1, "the upload file");
        // The core refuses an empty pickle.
        store(&mut s, 1, 1, 0, b"");
        done(call(&mut s, "storeBlobChunk", chunk()));
        done(store_blob_end(&mut s, 1, 1, b"pickle"));
        assert_eq!(blob_tree(&s.storage), none);
        let fault = failed(call(&mut s, "vote", vec![Value::from(1)]));
        assert_eq!(fault.class, TRANSACTION_ERROR);
    }

    #[test]
    fn blob_calls_out_of_order_fail() {
        let mut s = registered("blob-order");
        let chunk = || vec![Value::Binary(b"more".to_vec())];
        let fault = failed(call(&mut s, "storeBlobStart", vec![]));
        assert_eq!(fault.class, TRANSACTION_ERROR);
        let fault = failed(call(&mut s, "storeBlobChunk", chunk()));
        assert_eq!(fault.class, "builtins.ValueError");
        upload(&mut s, 1, 1, b"the blob");
        let fault = failed(call(&mut s, "storeBlobStart", vec![]));
        assert_eq!(fault.class, "builtins.ValueError");
        // A storeBlobEnd of another transaction leaves the upload open.
        let fault = failed(store_blob_end(&mut s, 2, 1, b"pickle"));
        assert_eq!(fault.class, TRANSACTION_ERROR);
        done(store_blob_end(&mut s, 1, 1, b"pickle"));
        let fault = failed(store_blob_end(&mut s, 1, 1, b"pickle"));
        assert_eq!(fault.class, "builtins.ValueError");
        let fault = failed(call(&mut s, "storeBlobChunk", chunk()));
        assert_eq!(fault.class, "builtins.ValueError");
        let fault = failed(call(&mut s, "storeBlobChunk", vec![Value::from("text")]));
        assert_eq!(fault.class, "builtins.TypeError");
    }

    /// Store `chunk` as the blob of `oid` over `serial` in the open
    /// transaction `id`, as a stock client does: `storea`, then the upload.
    fn store_blob(s: &mut Client, id: i64, oid: u64, serial: u64, chunk: &[u8]) {
        store(s, id, oid, serial, b"pickle");
        done(call(s, "storeBlobStart", vec![]));
        let chunk = vec![Value::Binary(chunk.to_vec())];
        done(call(s, "storeBlobChunk", chunk));
        let data = Value::Binary(b"pickle".to_vec());
        let end = vec![p64(oid), p64(serial), data, Value::from(id)];
        done(call(s, "storeBlobEnd", end));
    }

    #[test]
    fn a_stale_blob_is_listed_and_its_transaction_leaves_no_file() {
        let mut s = registered("blob-conflict");
        begin(&mut s, 1, None);
        store_blob(&mut s, 1, 1, 0, b"first");
        let first = vote_and_finish(&mut s, 1).unwrap();
        begin(&mut s, 2, None);
        store_blob(&mut s, 2, 1, first, b"second");
        let second = vote_and_finish(&mut s, 2).unwrap();
        let committed = blob_tree(&s.storage);
        assert_eq!(committed.len(), 2);

        // A blob over the stale serial. The first vote lists the conflict,
        // as for a pickle. A stock client cannot resolve a blob, so it
        // aborts.
        begin(&mut s, 3, None);
        store_blob(&mut s, 3, 1, first, b"stale");
        let listed = done(call(&mut s, "vote", vec![Value::from(3)]));
        let conflicts = listed.as_array().unwrap();
        assert_eq!(conflicts.len(), 1);
        assert_eq!(
            get(&conflicts[0], "serials"),
            &Value::Array(vec![p64(second), p64(first)])
        );
        done(call(&mut s, "tpc_abort", vec![Value::from(3)]));
        assert_eq!(blob_tree(&s.storage), committed);

        // A second vote without a resolved pickle fails with ConflictError.
        // It ends the transaction, and that removes the staged blob and an
        // upload that is still open.
        begin(&mut s, 4, None);
        store_blob(&mut s, 4, 1, first, b"stale");
        done(call(&mut s, "storeBlobStart", vec![]));
        done(call(
            &mut s,
            "storeBlobChunk",
            vec![Value::Binary(b"open".to_vec())],
        ));
        assert_eq!(blob_tree(&s.storage).len(), 4);
        done(call(&mut s, "vote", vec![Value::from(4)]));
        let fault = failed(call(&mut s, "vote", vec![Value::from(4)]));
        assert_eq!(fault.class, CONFLICT_ERROR);
        assert!(s.txn.open.is_none());
        assert_eq!(blob_tree(&s.storage), committed);
    }

    #[test]
    fn send_blob_checks_the_blob_before_the_first_chunk() {
        let storage = storage("send-blob", Queues::DEFAULT);
        let (mut s, mut frames) = connected(&storage);
        upload(&mut s, 1, 1, b"the blob");
        done(store_blob_end(&mut s, 1, 1, b"pickle"));
        let tid = vote_and_finish(&mut s, 1).unwrap();
        messages(&mut frames);
        let send_blob =
            |s: &mut Client, oid: u64, tid: u64| call(s, "sendBlob", vec![p64(oid), p64(tid)]);
        assert_eq!(done(send_blob(&mut s, 1, tid)), Value::Nil);
        let methods: Vec<String> = messages(&mut frames).into_iter().map(|(m, _)| m).collect();
        assert_eq!(
            methods,
            ["receiveBlobStart", "receiveBlobChunk", "receiveBlobStop"]
        );

        // No blob record: a POSKeyError, as in ZEO.
        let fault = failed(send_blob(&mut s, 1, tid + 1));
        assert_eq!(fault, Fault::from(ZoeError::NotFound { oid: Oid(1) }));
        let fault = failed(send_blob(&mut s, 2, tid));
        assert_eq!(fault.class, "ZODB.POSException.POSKeyError");

        // A file that does not match its record, then a missing file.
        let path = s.store.blob_path(Oid(1), Tid(tid)).unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        std::fs::set_permissions(&path, perms).unwrap();
        std::fs::write(&path, b"the blub").unwrap();
        let corrupt = Fault::from(ZoeError::CorruptBlob {
            oid: Oid(1),
            tid: Tid(tid),
        });
        assert_eq!(failed(send_blob(&mut s, 1, tid)), corrupt);
        std::fs::remove_file(&path).unwrap();
        assert_eq!(failed(send_blob(&mut s, 1, tid)), corrupt);
        assert_eq!(corrupt.class, "ZODB.POSException.StorageSystemError");
        // A blob that fails sends no chunk.
        assert!(messages(&mut frames).is_empty());
    }

    #[test]
    fn a_download_to_a_client_that_is_gone_ends_the_session() {
        // The frames of this session go nowhere.
        let mut s = registered("send-blob-gone");
        upload(&mut s, 1, 1, b"the blob");
        done(store_blob_end(&mut s, 1, 1, b"pickle"));
        let tid = vote_and_finish(&mut s, 1).unwrap();
        let outcome = call(&mut s, "sendBlob", vec![p64(1), p64(tid)]);
        assert!(matches!(outcome, Outcome::Gone), "{outcome:?}");
    }

    #[test]
    fn the_calls_of_the_read_pool_are_the_calls_that_read() {
        for method in [
            "loadBefore",
            "loadSerial",
            "history",
            "new_oids",
            "iterator_next",
            "sequence_next",
        ] {
            assert!(on_read_pool(method), "{method}");
        }
        for method in TRANSACTION_CALLS {
            assert!(!on_read_pool(method), "{method}");
        }
        for method in ["set_client_label", "sendBlob", "pack"] {
            assert!(!on_read_pool(method), "{method}");
        }
    }

    #[test]
    fn errors_have_the_class_names_of_zodb() {
        let fault = Fault::from(ZoeError::NotFound { oid: Oid(3) });
        assert_eq!(fault.class, "ZODB.POSException.POSKeyError");
        assert_eq!(fault.args, Value::Array(vec![p64(3)]));
        assert_eq!(Fault::from(ZoeError::ReadOnly), Fault::read_only());
        assert_eq!(
            Fault::from(ZoeError::Halted).class,
            "ZODB.POSException.StorageSystemError"
        );
        assert_eq!(
            Fault::from(ZoeError::Protocol("no")).args,
            Value::Array(vec![Value::from("two-phase commit out of order: no")])
        );
    }

    /// Run `method` with `args` as a call without a reply.
    fn send(session: &mut Client, method: &str, args: Vec<Value>) -> Outcome {
        session.call(&Call {
            id: Value::from(0),
            oneway: true,
            method: method.into(),
            args,
            context: None,
        })
    }

    /// Commit the transaction `id` with the extension `ext`. It stores
    /// `data` for each `(oid, serial)` of `oids`. Return its tid.
    fn commit_with(s: &mut Client, id: i64, ext: Value, oids: &[(u64, u64)], data: &[u8]) -> u64 {
        let id = Value::from(id);
        let begin = vec![
            id.clone(),
            Value::Binary(b"user".to_vec()),
            Value::Binary(b"description".to_vec()),
            ext,
        ];
        done(call(s, "tpc_begin", begin));
        for &(oid, serial) in oids {
            let data = Value::Binary(data.to_vec());
            done(send(
                s,
                "storea",
                vec![p64(oid), p64(serial), data, id.clone()],
            ));
        }
        done(call(s, "vote", vec![id.clone()]));
        finish(s, id)
    }

    /// The items of the array `value`.
    fn items(value: &Value) -> &[Value] {
        value.as_array().unwrap()
    }

    #[test]
    fn the_iterators_give_transactions_and_records_as_zeo_does() {
        let mut s = registered("iterators");
        let ext = Value::Map(vec![(Value::from("note"), Value::from("first"))]);
        let first = commit_with(&mut s, 1, ext.clone(), &[(1, 0), (2, 0)], MAPPING);
        let second = {
            begin(&mut s, 2, None);
            let delete = vec![p64(2), p64(first), Value::from(2)];
            done(send(&mut s, "deleteObject", delete));
            vote_and_finish(&mut s, 2).unwrap()
        };

        let iid = done(call(&mut s, "iterator_start", vec![Value::Nil, Value::Nil]));
        assert_eq!(iid, Value::from(0));
        let txn = done(call(&mut s, "iterator_next", vec![iid.clone()]));
        assert_eq!(
            txn,
            Value::Array(vec![
                p64(first),
                Value::from(" "),
                Value::Binary(b"user".to_vec()),
                Value::Binary(b"description".to_vec()),
                ext,
            ])
        );
        let rid = done(call(
            &mut s,
            "iterator_record_start",
            vec![iid.clone(), p64(first)],
        ));
        assert_eq!(rid, Value::from(1));
        for oid in [1, 2] {
            let record = done(call(&mut s, "iterator_record_next", vec![rid.clone()]));
            assert_eq!(
                record,
                Value::Array(vec![
                    p64(oid),
                    p64(first),
                    Value::Binary(MAPPING.to_vec()),
                    Value::Nil
                ])
            );
        }
        assert_eq!(
            done(call(&mut s, "iterator_record_next", vec![rid.clone()])),
            Value::Nil
        );
        // A record iterator at its end is gone.
        let fault = failed(call(&mut s, "iterator_record_next", vec![rid.clone()]));
        assert_eq!(fault, Fault::no_iterator(&rid));
        assert_eq!(fault.args, Value::Array(vec![Value::from(1)]));

        // An empty extension is an empty map, and a deletion has nil data.
        let txn = done(call(&mut s, "iterator_next", vec![iid.clone()]));
        assert_eq!(items(&txn)[0], p64(second));
        assert_eq!(items(&txn)[4], Value::Map(vec![]));
        let out_of_order = call(
            &mut s,
            "iterator_record_start",
            vec![iid.clone(), p64(first)],
        );
        assert_eq!(failed(out_of_order).class, "builtins.Exception");
        let rid = done(call(
            &mut s,
            "iterator_record_start",
            vec![iid.clone(), p64(second)],
        ));
        assert_eq!(rid, Value::from(2));
        let record = done(call(&mut s, "iterator_record_next", vec![rid]));
        assert_eq!(
            record,
            Value::Array(vec![p64(2), p64(second), Value::Nil, Value::Nil])
        );
        assert_eq!(
            done(call(&mut s, "iterator_next", vec![iid.clone()])),
            Value::Nil
        );
        let fault = failed(call(&mut s, "iterator_next", vec![iid.clone()]));
        assert_eq!(fault, Fault::no_iterator(&iid));

        // The bounds of the range are tids, both included.
        let range = |s: &mut Client, start: Value, stop: Value| {
            let iid = done(call(s, "iterator_start", vec![start, stop]));
            std::iter::from_fn(|| {
                let txn = done(call(s, "iterator_next", vec![iid.clone()]));
                (!txn.is_nil()).then(|| u64_of(&items(&txn)[0]).unwrap())
            })
            .collect::<Vec<_>>()
        };
        assert_eq!(range(&mut s, p64(second), Value::Nil), [second]);
        assert_eq!(range(&mut s, Value::Nil, p64(first)), [first]);
        let fault = failed(call(
            &mut s,
            "iterator_start",
            vec![Value::from(1), Value::Nil],
        ));
        assert_eq!(fault.class, "builtins.TypeError");
    }

    #[test]
    fn an_extension_that_msgpack_cannot_carry_fails_iterator_next() {
        let mut s = registered("bad-extension");
        // A restore transaction with the extension of a class instance.
        let ext = b"\x80\x03}q\x00X\x01\x00\x00\x00kcdatetime\ndate\nq\x01C\x04\x07\xea\x0a\x0cq\x02\x85Rq\x03s.";
        let mut txn = Txn::begin(Arc::clone(&s.store), b"", b"", ext);
        txn.store(Oid(1), Tid::ZERO, b"a").unwrap();
        txn.vote().unwrap();
        let tid = txn.finish().unwrap();
        let iid = done(call(&mut s, "iterator_start", vec![Value::Nil, Value::Nil]));
        let fault = failed(call(&mut s, "iterator_next", vec![iid.clone()]));
        assert_eq!(fault.class, "builtins.ValueError");
        let Value::Array(args) = &fault.args else {
            panic!("{fault:?}");
        };
        assert!(
            args[0]
                .as_str()
                .unwrap()
                .contains(&format!("{:016x}", tid.0)),
            "{fault:?}"
        );
        // The iterator stays, and it is at its end.
        assert_eq!(done(call(&mut s, "iterator_next", vec![iid])), Value::Nil);
    }

    #[test]
    fn iterator_gc_ignores_the_ids_that_it_does_not_know() {
        let mut s = registered("iterator-gc");
        commit(&mut s, 1, 1, 0, b"a").unwrap();
        let iid = done(call(&mut s, "iterator_start", vec![Value::Nil, Value::Nil]));
        let other = done(call(&mut s, "iterator_start", vec![Value::Nil, Value::Nil]));
        // Ids that never existed, an id of no int type, and a known id.
        let ids = vec![
            Value::from(5),
            Value::from(-1),
            Value::from("x"),
            iid.clone(),
            iid.clone(),
        ];
        let gc = send(&mut s, "iterator_gc", vec![Value::Array(ids)]);
        assert!(matches!(gc, Outcome::Done(Value::Nil)), "{gc:?}");
        assert_eq!(
            failed(call(&mut s, "iterator_next", vec![iid.clone()])),
            Fault::no_iterator(&iid)
        );
        assert!(!done(call(&mut s, "iterator_next", vec![other])).is_nil());
        // A gc without a list is an error, so the call without a reply
        // closes the connection.
        let gc = send(&mut s, "iterator_gc", vec![Value::from(1)]);
        assert!(matches!(gc, Outcome::Failed(_)), "{gc:?}");
    }

    #[test]
    fn iterator_ids_belong_to_their_connection() {
        let storage = storage("iterator-ids", Queues::DEFAULT);
        let (mut s, _frames) = connected(&storage);
        let (mut other, _other_frames) = connected(&storage);
        commit(&mut s, 1, 1, 0, b"a").unwrap();
        let start = vec![Value::Nil, Value::Nil];
        let iid = done(call(&mut s, "iterator_start", start.clone()));
        // Each connection counts its own ids from 0.
        assert_eq!(done(call(&mut other, "iterator_start", start)), iid);
        send(
            &mut other,
            "iterator_gc",
            vec![Value::Array(vec![iid.clone()])],
        );
        assert!(!done(call(&mut s, "iterator_next", vec![iid.clone()])).is_nil());
        assert_eq!(
            failed(call(&mut other, "iterator_next", vec![iid.clone()])),
            Fault::no_iterator(&iid)
        );
    }

    #[test]
    fn a_connection_refuses_an_iterator_over_the_bound() {
        let mut s = registered("iterator-bound");
        let start = vec![Value::Nil, Value::Nil];
        for _ in 0..iteration::MAX_OPEN {
            done(call(&mut s, "iterator_start", start.clone()));
        }
        let fault = failed(call(&mut s, "iterator_start", start.clone()));
        assert_eq!(fault.class, "ZODB.POSException.StorageError");
        // An iterator over no transaction ends at its first call. Then the
        // connection takes a new iterator.
        assert_eq!(
            done(call(&mut s, "iterator_next", vec![Value::from(0)])),
            Value::Nil
        );
        assert_eq!(
            done(call(&mut s, "iterator_start", start)),
            Value::from(iteration::MAX_OPEN as u64)
        );
    }

    #[test]
    fn restorea_copies_a_record_only_in_a_restore_transaction() {
        let mut s = registered("restorea");
        let restore = |s: &mut Client, id: i64, oid: u64, data: Value| {
            let args = vec![p64(oid), p64(0), data, Value::Nil, Value::from(id)];
            send(s, "restorea", args)
        };
        // A transaction without a tid fails its vote.
        begin(&mut s, 1, None);
        let data = Value::Binary(b"a".to_vec());
        assert!(matches!(
            restore(&mut s, 1, 1, data.clone()),
            Outcome::Done(Value::Nil)
        ));
        let fault = vote_and_finish(&mut s, 1).unwrap_err();
        assert_eq!(fault.class, TRANSACTION_ERROR);

        // A restore transaction commits the record under its tid.
        let wanted = s.store.last_transaction().0 + 10;
        begin(&mut s, 2, Some(wanted));
        restore(&mut s, 2, 1, data);
        assert_eq!(vote_and_finish(&mut s, 2), Ok(wanted));
        let load = done(call(&mut s, "loadSerial", vec![p64(1), p64(wanted)]));
        assert_eq!(load, Value::Binary(b"a".to_vec()));
        let stats = s.storage.status().to_value();
        assert_eq!(count(&stats, "stores"), 2);

        // A transaction that a pack of FileStorage changed has the status
        // "p". It copies as any other.
        let packed = vec![
            Value::from(4),
            Value::Nil,
            Value::Nil,
            Value::Map(vec![]),
            p64(wanted + 1),
            Value::from("p"),
        ];
        done(call(&mut s, "tpc_begin", packed));
        restore(&mut s, 4, 2, Value::Binary(b"b".to_vec()));
        assert_eq!(vote_and_finish(&mut s, 4), Ok(wanted + 1));

        // Nil data copies a deletion.
        begin(&mut s, 3, Some(wanted + 2));
        restore(&mut s, 3, 1, Value::Nil);
        assert_eq!(vote_and_finish(&mut s, 3), Ok(wanted + 2));
        let fault = failed(call(&mut s, "loadBefore", vec![p64(1), p64(u64::MAX)]));
        assert_eq!(fault, Fault::from(ZoeError::NotFound { oid: Oid(1) }));

        // A restorea of a transaction that is not the open one fails, and
        // so the connection closes.
        let outcome = restore(&mut s, 9, 1, Value::Nil);
        assert!(matches!(outcome, Outcome::Failed(_)), "{outcome:?}");
    }

    #[test]
    fn a_restore_transaction_publishes_its_commit_through_the_hub() {
        let storage = storage("restore-publish", Queues::DEFAULT);
        let (mut writer, mut own) = connected(&storage);
        let (_reader, mut others) = connected(&storage);
        let wanted = storage.store.last_transaction().0 + 10;
        begin(&mut writer, 1, Some(wanted));
        let args = vec![
            p64(1),
            p64(0),
            Value::Binary(b"a".to_vec()),
            Value::Nil,
            Value::from(1),
        ];
        done(send(&mut writer, "restorea", args));
        assert_eq!(vote_and_finish(&mut writer, 1), Ok(wanted));

        let own = messages(&mut own);
        let methods: Vec<&str> = own.iter().map(|(method, _)| method.as_str()).collect();
        assert_eq!(methods, ["info", ".reply"]);
        assert_eq!(own[1].1, p64(wanted));
        assert_eq!(messages(&mut others), [invalidation(wanted, &[1])]);
        assert_eq!(storage.invalidations.published(), Tid(wanted));
    }

    /// The message `info` with the sizes of `storage`.
    fn info_of(storage: &NamedStorage) -> Vec<u8> {
        let size = storage.size().unwrap();
        let length = storage.store.object_count().unwrap_or(0);
        message::notify(
            "info",
            vec![Value::Map(vec![
                (Value::from("length"), Value::from(length)),
                (Value::from("size"), Value::from(size)),
            ])],
        )
    }

    #[test]
    fn pack_sends_info_through_the_hub_to_every_client_before_its_reply() {
        let storage = storage("pack-info", Queues::DEFAULT);
        let (mut other, mut other_frames) = connected(&storage);
        let (outbox, mut unregistered_frames, _closing) = Outbox::new(10, 10);
        let _unregistered = Client::new(Arc::clone(&storage), outbox);
        let root = commit(&mut other, 1, 0, 0, MAPPING).unwrap();
        let dropped = commit(&mut other, 2, 1, 0, MAPPING).unwrap();
        // The info and the reply of each commit.
        assert_eq!(messages(&mut other_frames).len(), 4);

        // The packing client runs as a connection does.
        let (calls, received) = tokio::sync::mpsc::channel(crate::protocol::QUEUE);
        let (outbox, mut frames, _closing) = Outbox::new(10, 10);
        let worker = std::thread::spawn({
            let storage = Arc::clone(&storage);
            move || run(serving(storage), received, outbox)
        });
        let request = |id: i64, method: &str, args: Vec<Value>| Call {
            id: Value::from(id),
            oneway: false,
            method: method.into(),
            args,
            context: None,
        };
        let register = vec![Value::from("1"), Value::Boolean(false)];
        calls
            .blocking_send(request(1, "register", register))
            .unwrap();
        // A time in 2099: the pack packs at the last published tid.
        let pack = vec![Value::F64(4_102_444_799.0), Value::from(1)];
        calls.blocking_send(request(2, "pack", pack)).unwrap();
        drop(calls);
        worker.join().unwrap();

        let info = info_of(&storage);
        let got: Vec<Vec<u8>> = std::iter::from_fn(|| frames.try_recv())
            .map(|frame| frame.to_vec())
            .collect();
        assert_eq!(
            got,
            [
                message::reply(Value::from(1), p64(dropped)),
                info.clone(),
                message::reply(Value::from(2), Value::Nil),
            ]
        );
        assert_eq!(
            other_frames.try_recv().map(|frame| frame.to_vec()),
            Some(info)
        );
        assert!(other_frames.try_recv().is_none());
        assert!(unregistered_frames.try_recv().is_none());

        // The pack removed the object that the root does not reach.
        let fault = failed(call(&mut other, "loadSerial", vec![p64(1), p64(dropped)]));
        assert_eq!(fault, Fault::from(ZoeError::NotFound { oid: Oid(1) }));
        let load = done(call(&mut other, "loadSerial", vec![p64(0), p64(root)]));
        assert_eq!(load, Value::Binary(MAPPING.to_vec()));
    }

    #[test]
    fn a_pack_keeps_the_records_that_a_read_of_the_published_commits_needs() {
        let mut s = registered("pack-published");
        let root = commit(&mut s, 1, 0, 0, MAPPING).unwrap();
        // A commit of a new revision of the root is durable, and the hub
        // does not publish it yet.
        let mut txn = Txn::begin(Arc::clone(&s.store), b"", b"", b"");
        txn.store(Oid(0), Tid(root), MAPPING).unwrap();
        txn.vote().unwrap();
        let finished = txn.finish_after().unwrap();
        let next = finished.tid;
        assert_eq!(s.store.last_transaction(), next);
        assert_eq!(s.storage.invalidations.published(), Tid(root));

        // A pack at a time after that commit packs at the last published
        // tid. So it keeps the revision that a current read gives.
        let pack = vec![Value::F64(4_102_444_799.0), Value::from(1)];
        assert_eq!(done(call(&mut s, "pack", pack)), Value::Nil);
        let current = vec![p64(0), p64(u64::MAX)];
        assert_eq!(
            done(call(&mut s, "loadBefore", current.clone())),
            Value::Array(vec![Value::Binary(MAPPING.to_vec()), p64(root), Value::Nil])
        );
        assert_eq!(
            done(call(&mut s, "loadSerial", vec![p64(0), p64(root)])),
            Value::Binary(MAPPING.to_vec())
        );

        // After the publication, a read gives the new revision.
        s.publish(finished, None);
        assert_eq!(
            done(call(&mut s, "loadBefore", current)),
            Value::Array(vec![
                Value::Binary(MAPPING.to_vec()),
                p64(next.0),
                Value::Nil
            ])
        );
    }

    #[test]
    fn pack_takes_a_time_and_a_read_only_store_refuses_it() {
        let mut s = registered("pack-args");
        for args in [vec![], vec![Value::from("now")]] {
            let fault = failed(call(&mut s, "pack", args));
            assert_eq!(fault.class, "builtins.TypeError");
        }
        let fault = failed(call(&mut s, "pack", vec![Value::F64(f64::NAN)]));
        assert_eq!(fault.class, "builtins.ValueError");
        // An int is a time too.
        let pack = vec![Value::from(1_000_000_000), Value::from(0)];
        assert_eq!(done(call(&mut s, "pack", pack)), Value::Nil);

        // A store that the server opened read-only refuses the pack, also
        // for a connection that is not read-only.
        let dir = s.storage.path.clone();
        drop(s);
        let store = Arc::new(Store::open_read_only(&dir).unwrap());
        let storage = Arc::new(NamedStorage::new("1", &dir, store, Queues::DEFAULT));
        for read_only in [false, true] {
            let mut s = Client::new(Arc::clone(&storage), outbox());
            done(call(
                &mut s,
                "register",
                vec![Value::from("1"), Value::Boolean(read_only)],
            ));
            let fault = failed(call(&mut s, "pack", vec![Value::F64(1e9)]));
            assert_eq!(fault, Fault::from(ZoeError::ReadOnly));
        }
    }

    #[test]
    fn a_read_only_connection_packs_a_writable_store() {
        let storage = storage("pack-read-only-client", Queues::DEFAULT);
        let (mut writer, _frames) = connected(&storage);
        let old = commit(&mut writer, 1, 0, 0, MAPPING).unwrap();
        let new = commit(&mut writer, 2, 0, old, MAPPING).unwrap();

        // A read-only connection, as `zeopack` opens it.
        let (outbox, mut frames, _closing) = Outbox::new(10, 10);
        let mut reader = Client::new(Arc::clone(&storage), outbox);
        let register = vec![Value::from("1"), Value::Boolean(true)];
        assert_eq!(done(call(&mut reader, "register", register)), p64(new));
        let pack = vec![Value::F64(4_102_444_799.0), Value::from(1)];
        assert_eq!(done(call(&mut reader, "pack", pack)), Value::Nil);
        assert_eq!(
            frames.try_recv().map(|frame| frame.to_vec()),
            Some(info_of(&storage))
        );

        // The pack removed the old revision of the root and kept the new one.
        let fault = failed(call(&mut reader, "loadSerial", vec![p64(0), p64(old)]));
        assert_eq!(fault, Fault::from(ZoeError::NotFound { oid: Oid(0) }));
        let load = done(call(&mut reader, "loadSerial", vec![p64(0), p64(new)]));
        assert_eq!(load, Value::Binary(MAPPING.to_vec()));
        // The connection stays read-only for a commit.
        assert_eq!(
            failed(call(&mut reader, "new_oids", vec![])),
            Fault::read_only()
        );
    }

    #[test]
    fn a_read_only_client_does_not_pack() {
        let storage = storage("pack-read-only-certificate", Queues::DEFAULT);
        let (mut writer, _frames) = connected(&storage);
        let old = commit(&mut writer, 1, 0, 0, MAPPING).unwrap();
        let new = commit(&mut writer, 2, 0, old, MAPPING).unwrap();

        // A client with a certificate of the read-only client CA.
        let (outbox, mut frames, _closing) = Outbox::new(10, 10);
        let session = Session::new(Arc::clone(&storage), outbox, never_stops());
        let mut reader = Client::of(session.read_only_client(true));
        let register = vec![Value::from("1"), Value::Boolean(true)];
        assert_eq!(done(call(&mut reader, "register", register)), p64(new));
        let pack = vec![Value::F64(4_102_444_799.0), Value::from(1)];
        assert_eq!(failed(call(&mut reader, "pack", pack)), Fault::read_only());
        assert!(frames.try_recv().is_none());

        // The store keeps the old revision of the root.
        let load = done(call(&mut reader, "loadSerial", vec![p64(0), p64(old)]));
        assert_eq!(load, Value::Binary(MAPPING.to_vec()));
    }

    #[test]
    fn a_pack_whose_sizes_do_not_read_replies_nil_and_sends_no_info() {
        let dir =
            std::env::temp_dir().join(format!("zoe-session-pack-sizes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Arc::new(Store::open(&dir).unwrap());
        // The sizes come from the directory of the storage. This one does
        // not exist, so they do not read.
        let missing = dir.join("missing");
        let storage = Arc::new(NamedStorage::new("1", &missing, store, Queues::DEFAULT));
        assert!(storage.size().is_err());
        let (mut writer, _frames) = connected(&storage);
        let old = commit(&mut writer, 1, 0, 0, MAPPING).unwrap();
        let new = commit(&mut writer, 2, 0, old, MAPPING).unwrap();
        let (mut packer, mut frames) = connected(&storage);

        let pack = vec![Value::F64(4_102_444_799.0), Value::from(1)];
        assert_eq!(done(call(&mut packer, "pack", pack)), Value::Nil);
        assert!(frames.try_recv().is_none());
        // The pack is done.
        let fault = failed(call(&mut packer, "loadSerial", vec![p64(0), p64(old)]));
        assert_eq!(fault, Fault::from(ZoeError::NotFound { oid: Oid(0) }));
        let load = done(call(&mut packer, "loadSerial", vec![p64(0), p64(new)]));
        assert_eq!(load, Value::Binary(MAPPING.to_vec()));
    }
    #[test]
    fn a_failed_load_puts_its_error_in_its_room_and_never_waits_for_the_client() {
        // A session queue of 2 frames that no writer drains.
        let (outbox, mut frames, _closing) = Outbox::new(2, 100);
        let mut s = Client::new(storage("failed-load-room", Queues::DEFAULT), outbox.clone());
        let register = vec![Value::from("1"), Value::Boolean(false)];
        done(call(&mut s, "register", register));
        // One reply waits in the queue: the client reads nothing.
        let waiting = message::reply(Value::from(1), Value::Nil);
        assert!(outbox.blocking_room().put(Some(waiting)));
        // The load takes the last room before it starts.
        let room = outbox.blocking_room();
        // The reader waits for room for the next call.
        let reader = outbox.clone();
        let next = std::thread::spawn(move || reader.blocking_room());
        std::thread::sleep(std::time::Duration::from_millis(100));
        let session = Arc::clone(&s.session);
        let load = std::thread::spawn(move || {
            let call = Call {
                id: Value::from(5),
                oneway: false,
                method: "loadBefore".into(),
                args: vec![p64(42), p64(u64::MAX >> 1)],
                context: None,
            };
            let mut reply = Reply::new(room);
            let outcome = session.call(&call, &mut reply);
            reply.send(&call, outcome, session.outbox())
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !load.is_finished() {
            assert!(
                std::time::Instant::now() < deadline,
                "the failed load waits for the client"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(load.join().unwrap());
        // The error went into the room of the load, so the reader still
        // waits. It gets room when the client reads.
        assert!(!next.is_finished());
        let taken = messages(&mut frames);
        assert!(next.join().unwrap().put(None));
        assert_eq!(taken.len(), 2, "{taken:?}");
        let class = taken[1].1.as_array().map(|error| error[0].clone());
        assert_eq!(class, Some(Value::from("ZODB.POSException.POSKeyError")));
        drop(s);
    }

    /// A writer into a buffer that the test reads.
    #[derive(Clone, Default)]
    struct Logs(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Logs {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_lock_that_the_end_of_a_connection_releases_is_logged_with_its_client() {
        let dir = std::env::temp_dir().join(format!("zoe-session-lock-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let options = zoe_core::Options {
            lock_warn: Some(std::time::Duration::ZERO),
            ..zoe_core::Options::default()
        };
        let store = Arc::new(Store::open_with(&dir, options).unwrap());
        let storage = Arc::new(NamedStorage::new("main", &dir, store, Queues::DEFAULT));
        let (calls, received) = tokio::sync::mpsc::channel(16);
        let (replies, _inbox, _closing) = Outbox::new(16, 16);
        let send = |oneway: bool, method: &str, args: Vec<Value>| {
            let call = Call {
                id: Value::from(1),
                oneway,
                method: method.into(),
                args,
                context: None,
            };
            calls.blocking_send(call).unwrap();
        };
        send(
            false,
            "register",
            vec![Value::from("main"), Value::Boolean(false)],
        );
        send(false, "set_client_label", vec![Value::from("worker-7")]);
        let begin = vec![Value::from(7), Value::Nil, Value::Nil, Value::Map(vec![])];
        send(true, "tpc_begin", begin);
        let data = Value::Binary(b"secret-pickle".to_vec());
        send(true, "storea", vec![p64(5), p64(0), data, Value::from(7)]);
        send(false, "vote", vec![Value::from(7)]);
        // The connection ends with the transaction voted.
        drop(calls);
        let logs = Logs::default();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_writer({
                let logs = logs.clone();
                move || logs.clone()
            })
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            // Another test thread can register the callsite of the
            // connection span while this subscriber registers. tracing can
            // then cache the interest of the span as "never", and the line
            // of the lock loses the fields of the span. One rebuild does
            // not always fix it: a registration that is in progress can
            // store its interest after the rebuild. A span that is enabled
            // proves that the callsite is registered for good, so rebuild
            // until the span of a session is enabled.
            let probe = Session::new(Arc::clone(&storage), outbox(), never_stops());
            let mut enabled = false;
            for _ in 0..10_000 {
                tracing::callsite::rebuild_interest_cache();
                if !probe.span("probe").is_disabled() {
                    enabled = true;
                    break;
                }
                std::thread::yield_now();
            }
            assert!(enabled, "the span of the connection stays disabled");
            run(serving(Arc::clone(&storage)), received, replies);
        });

        let text = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
        assert!(!text.contains("secret-pickle"), "{text}");
        let line = text
            .lines()
            .find(|line| line.contains("\"oid\":5"))
            .unwrap_or_else(|| panic!("no line of the lock in:\n{text}"));
        assert!(line.contains("\"client\":\"worker-7\""), "{line}");
        assert!(line.contains("\"storage\":\"main\""), "{line}");
        assert!(storage.store.lock_table_is_empty());
    }

    #[test]
    fn the_metrics_count_commits_votes_and_raised_conflicts() {
        use std::sync::atomic::Ordering;

        let mut s = registered("metrics");
        let metrics = Arc::clone(&s.storage.metrics);
        let get = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        let one = commit(&mut s, 1, 1, 0, MAPPING).unwrap();
        commit(&mut s, 2, 1, one, MAPPING).unwrap();
        assert_eq!(get(&metrics.commits), 2);

        // A listed conflict that the client aborts is raised in the client.
        done(store_and_vote(&mut s, 3, &[(1, one)], MAPPING));
        done(call(&mut s, "tpc_abort", vec![Value::from(3)]));
        assert_eq!(get(&metrics.conflicts_raised), 1);
        // So is a vote that fails with a conflict: a new object over a
        // stale serial is never listed.
        let fault = commit(&mut s, 4, 9, 1, MAPPING).unwrap_err();
        assert!(fault.is_conflict(), "{fault:?}");
        assert_eq!(get(&metrics.conflicts_raised), 2);
        // A listed conflict that the end of the connection leaves.
        done(store_and_vote(&mut s, 5, &[(1, one)], MAPPING));
        let text = |storage: &NamedStorage| super::super::metrics::render(&[storage.exported()]);
        let storage = Arc::clone(&s.storage);
        drop(s);
        assert_eq!(get(&metrics.conflicts_raised), 3);
        assert_eq!(get(&metrics.commits), 2);
        let text = text(&storage);
        // Five first votes, each with its locks, two finishes, and two syncs
        // of one commit each.
        for line in [
            "zoe_vote_latency_seconds_count{storage=\"1\"} 5\n",
            "zoe_finish_latency_seconds_count{storage=\"1\"} 2\n",
            "zoe_fsync_batch_size_bucket{storage=\"1\",le=\"1\"} 2\n",
            "zoe_lock_wait_seconds_count{storage=\"1\"} 5\n",
            "zoe_clients_connected{storage=\"1\"} 0\n",
        ] {
            assert!(text.contains(line), "no {line} in:\n{text}");
        }
    }

    #[test]
    fn a_client_label_is_one_line_of_at_most_200_characters() {
        assert_eq!(client_label(&Value::from("worker\n1")), "worker 1");
        let long = "x".repeat(500);
        assert_eq!(client_label(&Value::from(long.as_str())).len(), 200);
        let mut s = registered("label");
        done(call(&mut s, "set_client_label", vec![Value::from("web-1")]));
        assert_eq!(s.label().as_deref(), Some("web-1"));
        // An empty label gives the address of the peer back.
        done(call(&mut s, "set_client_label", vec![Value::from(" ")]));
        assert_eq!(s.label(), None);
    }

    /// Stage a store of `data` for `oid` with the serial `serial` in the
    /// open transaction `id`.
    fn store_with(s: &mut Client, id: i64, oid: u64, serial: Value, data: &[u8]) {
        let args = vec![
            p64(oid),
            serial,
            Value::Binary(data.to_vec()),
            Value::from(id),
        ];
        assert_eq!(done(call(s, "storea", args)), Value::Nil);
    }

    #[test]
    fn a_nil_serial_is_the_serial_of_a_new_object() {
        let mut s = session("nil-serial");
        done(call(
            &mut s,
            "register",
            vec![Value::from("1"), Value::Boolean(false)],
        ));

        // A new oid commits with a nil serial.
        begin(&mut s, 1, None);
        store_with(&mut s, 1, 1, Value::Nil, MAPPING);
        let first = vote_and_finish(&mut s, 1).unwrap();
        let load = done(call(&mut s, "loadBefore", vec![p64(1), p64(u64::MAX)]));
        assert_eq!(
            load,
            Value::Array(vec![
                Value::Binary(MAPPING.to_vec()),
                p64(first),
                Value::Nil
            ])
        );

        // An oid with a record fails the vote, as with a zero serial. The
        // core does not hand the conflict back for the client to resolve.
        begin(&mut s, 2, None);
        store_with(&mut s, 2, 1, Value::Nil, MAPPING);
        let fault = vote_and_finish(&mut s, 2).unwrap_err();
        assert_eq!(
            fault,
            Fault::conflict(
                CONFLICT_ERROR,
                CONFLICT,
                Oid(1),
                Some((Tid(first), Tid(0))),
                Some("persistent.mapping.PersistentMapping".into())
            )
        );
        assert_eq!(
            done(call(&mut s, "tpc_abort", vec![Value::from(2)])),
            Value::Nil
        );
        assert_eq!(
            u64_of(&done(call(&mut s, "getTid", vec![p64(1)]))).unwrap(),
            first
        );

        // A serial that is not nil must still be 8 bytes.
        for (id, serial) in [(3, Value::from("x")), (4, Value::Binary(vec![0; 7]))] {
            begin(&mut s, id, None);
            store_with(&mut s, id, 2, serial, MAPPING);
            let fault = vote_and_finish(&mut s, id).unwrap_err();
            assert_eq!(fault.class, "builtins.TypeError");
        }
    }

    #[test]
    fn a_str_serial_of_8_bytes_reads_as_those_bytes() {
        let mut s = registered("str-serial");
        let zero = Value::from("\0".repeat(8));

        // A new oid commits with the str serial of a new object.
        begin(&mut s, 1, None);
        store_with(&mut s, 1, 1, zero.clone(), MAPPING);
        let first = vote_and_finish(&mut s, 1).unwrap();
        assert_eq!(
            u64_of(&done(call(&mut s, "getTid", vec![p64(1)]))).unwrap(),
            first
        );

        // An oid with a record fails the vote, as with the zero tid in
        // bytes.
        begin(&mut s, 2, None);
        store_with(&mut s, 2, 1, zero.clone(), MAPPING);
        let fault = vote_and_finish(&mut s, 2).unwrap_err();
        assert_eq!(
            fault,
            Fault::conflict(
                CONFLICT_ERROR,
                CONFLICT,
                Oid(1),
                Some((Tid(first), Tid(0))),
                Some("persistent.mapping.PersistentMapping".into())
            )
        );

        // `checkCurrentSerialInTransaction` and `deleteObject` read the
        // same serial.
        begin(&mut s, 3, None);
        let check = vec![p64(1), zero.clone(), Value::from(3)];
        done(call(&mut s, "checkCurrentSerialInTransaction", check));
        let fault = vote_and_finish(&mut s, 3).unwrap_err();
        assert_eq!(fault.class, READ_CONFLICT_ERROR);
        begin(&mut s, 4, None);
        done(call(
            &mut s,
            "deleteObject",
            vec![p64(1), zero.clone(), Value::from(4)],
        ));
        let fault = vote_and_finish(&mut s, 4).unwrap_err();
        assert_eq!(fault.class, CONFLICT_ERROR);

        // A str of another length, and an oid as a str, still fail.
        let not_bytes = Fault::type_error("an oid or a tid is not bytes");
        begin(&mut s, 5, None);
        store_with(&mut s, 5, 2, Value::from("\0".repeat(7)), MAPPING);
        assert_eq!(vote_and_finish(&mut s, 5).unwrap_err(), not_bytes);
        begin(&mut s, 6, None);
        let args = vec![
            zero.clone(),
            zero,
            Value::Binary(MAPPING.to_vec()),
            Value::from(6),
        ];
        done(call(&mut s, "storea", args));
        assert_eq!(vote_and_finish(&mut s, 6).unwrap_err(), not_bytes);

        // The UTF-8 bytes of the str are the serial.
        let text = "\u{e9}".repeat(4);
        let bytes = <[u8; 8]>::try_from(text.as_bytes()).unwrap();
        assert_eq!(
            serial_of(&Value::from(text)),
            Ok(Tid(u64::from_be_bytes(bytes)))
        );
        assert_eq!(store_serial(&Value::Nil), Ok(Tid::ZERO));
    }

    #[test]
    fn get_tid_gives_the_serial_of_the_current_record() {
        let mut s = session("get-tid");
        done(call(
            &mut s,
            "register",
            vec![Value::from("1"), Value::Boolean(false)],
        ));
        let missing = Fault::from(ZoeError::NotFound { oid: Oid(1) });
        assert_eq!(failed(call(&mut s, "getTid", vec![p64(1)])), missing);

        let first = commit(&mut s, 1, 1, 0, MAPPING).unwrap();
        assert_eq!(done(call(&mut s, "getTid", vec![p64(1)])), p64(first));
        let second = commit(&mut s, 2, 1, first, MAPPING).unwrap();
        assert_eq!(done(call(&mut s, "getTid", vec![p64(1)])), p64(second));

        // A read needs no transaction and takes no lock: it works while
        // the open transaction holds the lock of the oid after its vote.
        begin(&mut s, 3, None);
        store(&mut s, 3, 1, second, MAPPING);
        done(call(&mut s, "vote", vec![Value::from(3)]));
        assert_eq!(done(call(&mut s, "getTid", vec![p64(1)])), p64(second));
        let abort = done(call(&mut s, "tpc_abort", vec![Value::from(3)]));
        assert_eq!(abort, Value::Nil);

        // A deletion is the current record: the oid has no tid.
        begin(&mut s, 4, None);
        let delete = vec![p64(1), p64(second), Value::from(4)];
        done(call(&mut s, "deleteObject", delete));
        vote_and_finish(&mut s, 4).unwrap();
        assert_eq!(failed(call(&mut s, "getTid", vec![p64(1)])), missing);

        // Wrong arguments fail the call, and the connection stays open.
        let fault = failed(call(&mut s, "getTid", vec![]));
        assert_eq!(fault, Fault::type_error("getTid takes 1 arguments"));
        let fault = failed(call(&mut s, "getTid", vec![Value::from(1)]));
        assert_eq!(fault, Fault::type_error("an oid or a tid is not bytes"));
        assert!(matches!(call(&mut s, "ping", vec![]), Outcome::Done(_)));
    }

    #[test]
    fn get_tid_shows_only_the_commits_that_the_hub_published() {
        let storage = storage("get-tid-published", Queues::DEFAULT);
        let (mut writer, _own) = connected(&storage);
        let (mut reader, mut frames) = connected(&storage);
        let first = commit(&mut writer, 1, 1, 0, b"old").unwrap();
        assert_eq!(messages(&mut frames), [invalidation(first, &[1])]);

        // A commit that the store made visible and the hub did not publish
        // yet: a new record of oid 1, and the first record of oid 2.
        let mut txn = storage.store.tpc_begin(b"", b"", b"").unwrap();
        txn.store(Oid(1), Tid(first), b"new").unwrap();
        txn.store(Oid(2), Tid(0), b"two").unwrap();
        txn.vote().unwrap();
        let hidden = txn.finish_after().unwrap();

        assert_eq!(done(call(&mut reader, "getTid", vec![p64(1)])), p64(first));
        let fault = failed(call(&mut reader, "getTid", vec![p64(2)]));
        assert_eq!(fault, Fault::from(ZoeError::NotFound { oid: Oid(2) }));

        // The hub publishes the commit. Then the reads show it.
        storage.invalidations.publish(Commit {
            previous: hidden.previous,
            tid: hidden.tid,
            origin: None,
            outbox: outbox(),
            own: Vec::new(),
            others: Others::Send(message::notify(
                "invalidateTransaction",
                vec![p64(hidden.tid.0), Value::Array(vec![p64(1), p64(2)])],
            )),
        });
        for oid in [1, 2] {
            let tid = done(call(&mut reader, "getTid", vec![p64(oid)]));
            assert_eq!(tid, p64(hidden.tid.0));
        }
        // The call does not put its reply under the mutex of the hub, so
        // its reply goes out after the invalidation of the commit that it
        // shows.
        assert!(reader.sent.is_none());
    }

    #[test]
    fn a_blob_with_a_nil_serial_is_a_new_object() {
        let mut s = registered("blob-nil-serial");
        // A stock `storeBlob(oid, None, ...)`: `storea`, the upload and
        // `storeBlobEnd`, each with the nil serial.
        let send = |s: &mut Client, id: i64, data: &[u8]| {
            begin(s, id, None);
            store_with(s, id, 1, Value::Nil, b"pickle");
            done(call(s, "storeBlobStart", vec![]));
            done(call(
                s,
                "storeBlobChunk",
                vec![Value::Binary(data.to_vec())],
            ));
            let end = vec![
                p64(1),
                Value::Nil,
                Value::Binary(b"pickle".to_vec()),
                Value::from(id),
            ];
            done(call(s, "storeBlobEnd", end));
        };
        send(&mut s, 1, b"the blob");
        let tid = Tid(vote_and_finish(&mut s, 1).unwrap());
        assert_eq!(s.store.load_serial(Oid(1), tid).unwrap(), b"pickle");
        let path = s.store.blob_path(Oid(1), tid).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"the blob");

        // The oid has a record now: the nil serial fails the vote.
        send(&mut s, 2, b"another blob");
        let fault = vote_and_finish(&mut s, 2).unwrap_err();
        assert_eq!(fault.class, CONFLICT_ERROR);
        assert_eq!(
            item(&fault, "serials"),
            Value::Array(vec![p64(tid.0), p64(0)])
        );
        assert_eq!(
            done(call(&mut s, "tpc_abort", vec![Value::from(2)])),
            Value::Nil
        );
        assert_eq!(blob_tree(&s.storage), [path]);
    }

    #[test]
    fn a_read_only_storage_refuses_a_writing_register_and_takes_a_read_only_one() {
        let dir = storage("register-read-only", Queues::DEFAULT).path.clone();
        let store = Arc::new(Store::open_read_only(&dir).unwrap());
        let storage = NamedStorage::new("1", &dir, store, Queues::DEFAULT).read_only(true);
        let mut s = Client::new(Arc::new(storage), outbox());
        let write = vec![Value::from("1"), Value::Boolean(false)];
        assert_eq!(failed(call(&mut s, "register", write)), Fault::read_only());
        // The connection stays open, and the client registers again.
        let read = vec![Value::from("1"), Value::Boolean(true)];
        assert_eq!(done(call(&mut s, "register", read)), p64(0));
        assert_eq!(done(call(&mut s, "lastTransaction", vec![])), p64(0));
        for (method, args) in [("new_oids", vec![]), ("pack", vec![Value::F64(1e9)])] {
            assert_eq!(failed(call(&mut s, method, args)), Fault::read_only());
        }
    }

    #[test]
    fn a_read_only_client_registers_read_only_only_on_a_writable_storage() {
        let storage = storage("register-read-only-client", Queues::DEFAULT);
        let session = Session::new(Arc::clone(&storage), outbox(), never_stops());
        let mut s = Client::of(session.read_only_client(true));
        let write = vec![Value::from("1"), Value::Boolean(false)];
        assert_eq!(failed(call(&mut s, "register", write)), Fault::read_only());
        // The connection stays open, and the client registers again.
        let read = vec![Value::from("1"), Value::Boolean(true)];
        assert_eq!(done(call(&mut s, "register", read)), p64(0));
        assert_eq!(failed(call(&mut s, "new_oids", vec![])), Fault::read_only());

        // Another client of the same storage writes.
        let mut writer = Client::new(storage, outbox());
        let write = vec![Value::from("1"), Value::Boolean(false)];
        assert_eq!(done(call(&mut writer, "register", write)), p64(0));
        assert!(matches!(
            call(&mut writer, "new_oids", vec![]),
            Outcome::Done(_)
        ));
    }

    #[test]
    fn the_stop_of_the_server_stops_a_pack_between_two_batches() {
        let storage = storage("pack-stop", Queues::DEFAULT);
        let (stop, stopping) = Stopping::new();
        let (outbox, _frames, _closing) = Outbox::new(10, 10);
        let mut s = Client::stopping(Arc::clone(&storage), outbox, stopping);
        let register = vec![Value::from("1"), Value::Boolean(false)];
        done(call(&mut s, "register", register));
        // The root refers to nothing. The 1,500 other objects make two
        // batches of the pack.
        let oids: Vec<(u64, u64)> = (0..=1_500).map(|oid| (oid, 0)).collect();
        let tid = commit_with(&mut s, 1, Value::Map(vec![]), &oids, MAPPING);

        // The stop comes before the pack. The pack still runs its first
        // batch, which removes the oldest 1,000 keys, then stops.
        stop.send_replace(true);
        let pack = vec![Value::F64(4_102_444_799.0), Value::from(1)];
        let fault = failed(call(&mut s, "pack", pack.clone()));
        assert_eq!(fault, Fault::storage("pack stopped: the server stops"));
        let load = |s: &mut Client, oid: u64| call(s, "loadSerial", vec![p64(oid), p64(tid)]);
        assert_eq!(
            failed(load(&mut s, 1_000)),
            Fault::from(ZoeError::NotFound { oid: Oid(1_000) })
        );
        assert_eq!(done(load(&mut s, 1_001)), Value::Binary(MAPPING.to_vec()));
        drop(s);

        // A session that no stop reaches removes what the pack left.
        let (mut s, _frames) = connected(&storage);
        assert_eq!(done(call(&mut s, "pack", pack)), Value::Nil);
        assert_eq!(
            failed(load(&mut s, 1_500)),
            Fault::from(ZoeError::NotFound { oid: Oid(1_500) })
        );
        assert_eq!(done(load(&mut s, 0)), Value::Binary(MAPPING.to_vec()));
    }

    /// The acceptance tests of the compression pool: a `storea` replies
    /// before its encode ends, the vote waits for the encodes of its
    /// transaction before it takes a lock, a failed encode fails the vote,
    /// and an abort or a lost connection cancels the encodes that wait and
    /// leaves no lock.
    mod compression {
        use std::num::NonZeroUsize;
        use std::sync::Condvar;
        use std::time::{Duration, Instant};

        use zoe_core::{Codec, Encoded};

        use super::*;
        use crate::protocol::compress::{Compressor, Hook};
        use crate::runtime::Tokio;

        /// The gate of the jobs of a compression pool. It records the oid
        /// of each job that starts. While it is closed, it holds the first
        /// `hold` jobs. A job of the oid `fail` fails.
        struct Gate {
            state: Mutex<GateState>,
            changed: Condvar,
        }

        struct GateState {
            started: Vec<Oid>,
            closed: bool,
            hold: usize,
            fail: Option<Oid>,
        }

        impl Gate {
            fn new(hold: usize, fail: Option<Oid>) -> Arc<Self> {
                Arc::new(Self {
                    state: Mutex::new(GateState {
                        started: Vec::new(),
                        closed: hold > 0,
                        hold,
                        fail,
                    }),
                    changed: Condvar::new(),
                })
            }

            fn lock(&self) -> MutexGuard<'_, GateState> {
                self.state.lock().unwrap()
            }

            fn hook(self: &Arc<Self>) -> Hook {
                let gate = Arc::clone(self);
                Arc::new(move |oid| {
                    let mut state = gate.lock();
                    state.started.push(oid);
                    let held = state.started.len() <= state.hold;
                    gate.changed.notify_all();
                    while held && state.closed {
                        state = gate.changed.wait(state).unwrap();
                    }
                    state.fail != Some(oid)
                })
            }

            /// Let the held jobs go on, and hold no later job.
            fn open(&self) {
                self.lock().closed = false;
                self.changed.notify_all();
            }

            /// Wait until `n` jobs started. Fail after 30 seconds.
            fn wait_started(&self, n: usize) {
                let deadline = Instant::now() + Duration::from_secs(30);
                let mut state = self.lock();
                while state.started.len() < n {
                    let left = deadline.saturating_duration_since(Instant::now());
                    assert!(!left.is_zero(), "{n} jobs start: {:?}", state.started);
                    state = self.changed.wait_timeout(state, left).unwrap().0;
                }
            }

            fn started(&self) -> Vec<Oid> {
                self.lock().started.clone()
            }
        }

        /// A tokio runtime for the tasks of the jobs.
        fn tokio() -> tokio::runtime::Runtime {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap()
        }

        /// A storage whose votes wait at most 200 ms for a lock, so a test
        /// sees a lock that stays. Its store compresses with zstd at level
        /// 3, so its transactions encode on the compression pool.
        fn storage(name: &str) -> Arc<NamedStorage> {
            let dir = std::env::temp_dir().join(format!(
                "zoe-session-compress-{name}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            let options = Options {
                lock_timeout: Duration::from_millis(200),
                compression: Compression::Zstd { level: 3 },
                ..Options::default()
            };
            let store = Arc::new(Store::open_with(&dir, options).unwrap());
            Arc::new(NamedStorage::new("1", &dir, store, Queues::DEFAULT))
        }

        /// A registered connection of `storage` whose transactions encode
        /// on `compressor`. Call it inside a tokio runtime.
        fn client(storage: &Arc<NamedStorage>, compressor: &Compressor) -> Client {
            let session = Arc::new(Session::new(Arc::clone(storage), outbox(), never_stops()));
            let txn = Transaction::new(Arc::clone(&session))
                .compressing(compressor.clone(), Arc::new(Tokio));
            let mut s = Client {
                session,
                txn,
                sent: None,
            };
            let register = vec![Value::from("1"), Value::Boolean(false)];
            done(call(&mut s, "register", register));
            s
        }

        /// A pickle of 100 KB that zstd compresses well.
        fn large(oid: u64) -> Vec<u8> {
            let mut data = format!("pickle of {oid} ").into_bytes();
            while data.len() < 100 * 1024 {
                data.extend_from_slice(b"X\x05\x00\x00\x00titleq\x02X\x04\x00\x00\x00text");
            }
            data
        }

        /// Run the transaction `id` of `s` on another thread, inside
        /// `tokio`: it begins, stores `oid` with each pickle of `stores`
        /// over the serial 0, tells the receiver that each `storea`
        /// returned, then votes. The core takes the calls of a transaction
        /// only from the thread that began it.
        fn transaction_elsewhere(
            tokio: &tokio::runtime::Runtime,
            mut s: Client,
            id: i64,
            stores: Vec<(u64, Vec<u8>)>,
        ) -> (
            std::sync::mpsc::Receiver<()>,
            std::thread::JoinHandle<(Client, Outcome)>,
        ) {
            let handle = tokio.handle().clone();
            let (stored, returned) = std::sync::mpsc::channel();
            let thread = std::thread::spawn(move || {
                let _entered = handle.enter();
                begin(&mut s, id, None);
                for (oid, data) in &stores {
                    store(&mut s, id, *oid, 0, data);
                }
                stored.send(()).unwrap();
                let outcome = call(&mut s, "vote", vec![Value::from(id)]);
                (s, outcome)
            });
            (returned, thread)
        }

        #[test]
        fn a_storea_returns_before_its_encode_ends_and_the_vote_waits_for_it() {
            let tokio = tokio();
            let _entered = tokio.enter();
            let gate = Gate::new(usize::MAX, None);
            let compressor = Compressor::new(NonZeroUsize::new(2).unwrap()).with_hook(gate.hook());
            let storage = storage("held");
            let s = client(&storage, &compressor);
            let stores = (1..=3).map(|oid| (oid, large(oid))).collect();
            let (stored, voting) = transaction_elsewhere(&tokio, s, 1, stores);
            // Each `storea` returns while the gate holds the encodes.
            stored.recv_timeout(Duration::from_secs(30)).unwrap();
            gate.wait_started(2);
            std::thread::sleep(Duration::from_millis(300));
            assert!(!voting.is_finished(), "the vote waits for the encodes");
            gate.open();
            let (mut s, vote) = voting.join().unwrap();
            assert_eq!(done(vote), Value::Array(vec![]));
            let tid = finish(&mut s, Value::from(1));
            assert_eq!(gate.started().len(), 3);
            for oid in 1..=3 {
                let data = large(oid);
                assert_eq!(s.store.load(Oid(oid)).unwrap(), (data.clone(), Tid(tid)));
                let info = s.store.record_info(Oid(oid), Tid(tid)).unwrap();
                let inline = Encoded::encode(&data, s.store.compression());
                assert_eq!(info.codec, Codec::Zstd);
                assert_eq!(info.stored_len, inline.stored_len() as u64);
            }
        }

        #[test]
        fn a_failed_encode_fails_the_vote_and_leaves_no_lock() {
            let tokio = tokio();
            let _entered = tokio.enter();
            let gate = Gate::new(0, Some(Oid(2)));
            let compressor = Compressor::new(NonZeroUsize::new(2).unwrap()).with_hook(gate.hook());
            let storage = storage("failed");
            let mut s = client(&storage, &compressor);
            begin(&mut s, 1, None);
            for oid in 1..=3 {
                store(&mut s, 1, oid, 0, &large(oid));
            }
            let fault = failed(call(&mut s, "vote", vec![Value::from(1)]));
            assert_eq!(fault.class, "ZODB.POSException.StorageError");
            assert!(s.txn.open.is_none());
            assert!(s.store.load(Oid(1)).is_err());
            assert_eq!(count(&storage.status().to_value(), "active_txns"), 0);
            // No lock stays: each oid commits at once.
            for oid in [1, 3] {
                commit(&mut s, 10 + oid as i64, oid, 0, &large(oid)).unwrap();
            }
        }

        #[test]
        fn a_vote_that_waits_for_its_encodes_holds_no_lock() {
            let tokio = tokio();
            let _entered = tokio.enter();
            let gate = Gate::new(1, None);
            let compressor = Compressor::new(NonZeroUsize::new(2).unwrap()).with_hook(gate.hook());
            let storage = storage("no-lock");
            let waiting = client(&storage, &compressor);
            let (stored, voting) = transaction_elsewhere(&tokio, waiting, 1, vec![(1, large(1))]);
            stored.recv_timeout(Duration::from_secs(30)).unwrap();
            gate.wait_started(1);
            std::thread::sleep(Duration::from_millis(100));
            assert!(!voting.is_finished());

            // Another transaction writes the oid and commits while the vote
            // waits: the waiting vote holds no lock.
            let mut other = client(&storage, &compressor);
            let tid = commit(&mut other, 2, 1, 0, &large(1)).unwrap();
            gate.open();
            let (_, vote) = voting.join().unwrap();
            let fault = failed(vote);
            assert_eq!(fault.class, CONFLICT_ERROR);
            assert_eq!(
                item(&fault, "serials"),
                Value::Array(vec![p64(tid), p64(0)])
            );
        }

        #[test]
        fn an_abort_cancels_the_encodes_that_wait_and_leaves_no_lock() {
            let tokio = tokio();
            let _entered = tokio.enter();
            let gate = Gate::new(1, None);
            let compressor = Compressor::new(NonZeroUsize::MIN).with_hook(gate.hook());
            let storage = storage("abort");
            let mut s = client(&storage, &compressor);
            begin(&mut s, 1, None);
            for oid in 1..=3 {
                store(&mut s, 1, oid, 0, &large(oid));
            }
            gate.wait_started(1);
            done(call(&mut s, "tpc_abort", vec![Value::from(1)]));
            assert!(s.txn.open.is_none());
            gate.open();
            // The one thread of the pool runs the job of the oid 4 after
            // the jobs that waited before it.
            commit(&mut s, 2, 4, 0, &large(4)).unwrap();
            assert_eq!(gate.started(), vec![Oid(1), Oid(4)]);
            for oid in 1..=3 {
                commit(&mut s, 2 + oid as i64, oid, 0, &large(oid)).unwrap();
            }
        }

        #[test]
        fn a_lost_connection_cancels_the_encodes_that_wait_and_leaves_no_lock() {
            let gate = Gate::new(1, None);
            let compressor = Compressor::new(NonZeroUsize::MIN).with_hook(gate.hook());
            let storage = storage("lost");
            let storages = Arc::new(
                Storages::new(vec![Arc::clone(&storage)], Queues::DEFAULT.client)
                    .with_compressor(compressor.clone()),
            );
            let (calls, received) = tokio::sync::mpsc::channel(crate::protocol::QUEUE);
            let (replies, _sent, _closing) = Outbox::new(1_000, 1_000);
            let session = std::thread::spawn(move || run(storages, received, replies));
            let send = |method: &str, args: Vec<Value>| {
                let call = Call {
                    id: Value::from(0),
                    oneway: true,
                    method: method.into(),
                    args,
                    context: None,
                };
                calls.blocking_send(call).unwrap();
            };
            send("register", vec![Value::from("1"), Value::Boolean(false)]);
            let begin = vec![Value::from(7), Value::Nil, Value::Nil, Value::Map(vec![])];
            send("tpc_begin", begin);
            for oid in 1..=3 {
                let data = Value::Binary(large(oid));
                send("storea", vec![p64(oid), p64(0), data, Value::from(7)]);
            }
            gate.wait_started(1);
            // The client goes. The end of the connection aborts the
            // transaction while its first encode is held.
            drop(calls);
            let deadline = Instant::now() + Duration::from_secs(30);
            while count(&storage.status().to_value(), "aborts") == 0 {
                assert!(Instant::now() < deadline, "the transaction aborts");
                std::thread::sleep(Duration::from_millis(10));
            }
            gate.open();
            session.join().unwrap();

            let tokio = tokio();
            let _entered = tokio.enter();
            let mut s = client(&storage, &compressor);
            commit(&mut s, 2, 4, 0, &large(4)).unwrap();
            assert_eq!(gate.started(), vec![Oid(1), Oid(4)]);
            for oid in 1..=3 {
                commit(&mut s, 2 + oid as i64, oid, 0, &large(oid)).unwrap();
            }
        }

        #[test]
        fn the_pool_stores_the_records_of_an_inline_encode() {
            let tokio = tokio();
            let _entered = tokio.enter();
            let compressor = Compressor::new(NonZeroUsize::new(2).unwrap());
            let (pooled, inline) = (storage("pooled"), storage("inline"));
            let mut a = client(&pooled, &compressor);
            let mut b = Client::new(Arc::clone(&inline), outbox());
            done(call(
                &mut b,
                "register",
                vec![Value::from("1"), Value::Boolean(false)],
            ));
            let tid = 0x0300_0000_0000_0001;
            let records: Vec<(u64, Vec<u8>)> = vec![
                (1, large(1)),
                (2, MAPPING.to_vec()),
                (
                    3,
                    (0..5_000u64)
                        .map(|i| ((i * 2_654_435_761) >> 13) as u8)
                        .collect(),
                ),
            ];
            for s in [&mut a, &mut b] {
                begin(s, 1, Some(tid));
                for (oid, data) in &records {
                    let args = vec![
                        p64(*oid),
                        p64(0),
                        Value::Binary(data.clone()),
                        Value::Nil,
                        Value::from(1),
                    ];
                    assert_eq!(done(call(s, "restorea", args)), Value::Nil);
                }
                assert_eq!(vote_and_finish(s, 1).unwrap(), tid);
            }
            for (oid, data) in &records {
                let (oid, tid) = (Oid(*oid), Tid(tid));
                assert_eq!(a.store.load(oid).unwrap(), (data.clone(), tid));
                assert_eq!(
                    a.store.record_info(oid, tid).unwrap(),
                    b.store.record_info(oid, tid).unwrap()
                );
            }
        }
    }
}
