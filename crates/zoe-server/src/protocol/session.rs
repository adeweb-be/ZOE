//! The calls of one connection, run in order on one thread.
//!
//! The session holds the open transaction of the connection. Its thread
//! begins the core transaction and makes every later call of it, so the
//! core never refuses a call with `WrongThread`.

use std::sync::{Arc, mpsc};

use rmpv::Value;
use tokio::sync::mpsc::UnboundedSender;
use zoe_core::{Oid, Store, Tid, Txn, ZoeError};

use super::NamedStorage;
use super::clock;
use super::extension;
use super::iteration::{self, Iterators, Refused};
use super::message::{self, Call};
use super::status::Stats;

/// The most oids that one `new_oids` call gives, as in ZEO.
const MAX_NEW_OIDS: u64 = 100;

/// The class of a conflict error of ZODB.
const CONFLICT_ERROR: &str = "ZODB.POSException.ConflictError";

/// The class of a read conflict error of ZODB.
const READ_CONFLICT_ERROR: &str = "ZODB.POSException.ReadConflictError";

/// The class of a transaction error of ZODB.
const TRANSACTION_ERROR: &str = "ZODB.POSException.StorageTransactionError";

/// The text of a conflict error of ZODB.
const CONFLICT: &str = "database conflict error";

/// The text of a read conflict error of ZODB.
const READ_CONFLICT: &str = "database read conflict error";

/// The interfaces that the store provides, as `(module, name)`, in the
/// order of `providedBy(zoe.Storage(path)).__iro__`. A stock client takes
/// the storage interfaces of this list as its own. The store does not
/// provide `IStorageUndoable` or `IExternalGC`.
const INTERFACES: [(&str, &str); 7] = [
    ("ZODB.interfaces", "IBlobStorageRestoreable"),
    ("ZODB.interfaces", "IBlobStorage"),
    ("ZODB.interfaces", "IStorageRestoreable"),
    ("ZODB.interfaces", "IStorageCurrentRecordIteration"),
    ("ZODB.interfaces", "IStorage"),
    ("ZODB.interfaces", "IStorageIteration"),
    ("zope.interface", "Interface"),
];

/// The methods that ZEO counts in `stores`.
const STORES: [&str; 3] = ["storea", "deleteObject", "restorea"];

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
                "a connection keeps at most {} open iterators; iterator_gc frees them",
                iteration::MAX_OPEN
            )),
        }
    }

    /// Return the exception of ZODB for an error of a pack, as
    /// `zoe.Storage.pack` raises it. A record that fails its checksum or
    /// does not decode stops the pack with `StorageSystemError`.
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
            ZoeError::NotFound { oid } => Self {
                class: "ZODB.POSException.POSKeyError",
                args: Value::Array(vec![p64(oid.0)]),
            },
            ZoeError::Conflict { oid } => Self::conflict(CONFLICT_ERROR, CONFLICT, oid, None, None),
            ZoeError::ReadConflict { oid } => {
                Self::conflict(READ_CONFLICT_ERROR, READ_CONFLICT, oid, None, None)
            }
            ZoeError::ReadOnly => Self::read_only(),
            ZoeError::Protocol(_) | ZoeError::LockOrder { .. } | ZoeError::WrongThread => {
                Self::transaction(text)
            }
            ZoeError::Halted
            | ZoeError::CorruptRecord { .. }
            | ZoeError::CorruptBlob { .. }
            | ZoeError::CorruptTxn { .. }
            | ZoeError::CorruptSequence { .. }
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
enum Outcome {
    /// The result of the call.
    Done(Value),
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

/// The state of one connection.
pub(crate) struct Session {
    storage: Arc<NamedStorage>,
    /// The store of `storage`.
    store: Arc<Store>,
    /// The queue of the frames that go out to the client.
    queue: UnboundedSender<Vec<u8>>,
    /// The id of the connection among the clients of `storage`, after
    /// `register`.
    client: Option<u64>,
    registered: bool,
    read_only: bool,
    open: Option<Open>,
    iterators: Iterators,
}

/// Run the calls that arrive on `calls` in order, and send each reply to
/// `replies`. Return when `calls` closes, or when the session closes the
/// connection. A transaction still open aborts.
pub(crate) fn run(
    storage: Arc<NamedStorage>,
    calls: mpsc::Receiver<Call>,
    replies: UnboundedSender<Vec<u8>>,
) {
    let mut session = Session::new(storage, replies.clone());
    while let Ok(call) = calls.recv() {
        let reply = match session.call(&call) {
            Outcome::Done(_) if call.oneway => continue,
            Outcome::Done(value) => message::reply(call.id, value),
            // A call without a reply has no way to report its error, so
            // the server closes the connection, as ZEO does.
            Outcome::Failed(fault) if call.oneway => {
                eprintln!(
                    "zoe serve: {} failed: {} {:?}",
                    call.method, fault.class, fault.args
                );
                break;
            }
            Outcome::Failed(fault) => message::error(call.id, fault.class, fault.args),
            Outcome::Close(why) => {
                eprintln!("zoe serve: closing a connection: {why}");
                break;
            }
        };
        if replies.send(reply).is_err() {
            break;
        }
    }
}

impl Session {
    /// Return the session of a connection of `storage`. The frames that the
    /// server sends without a call go to `queue`, the queue of the replies.
    pub(crate) fn new(storage: Arc<NamedStorage>, queue: UnboundedSender<Vec<u8>>) -> Self {
        Self {
            store: Arc::clone(&storage.store),
            storage,
            queue,
            client: None,
            registered: false,
            read_only: false,
            open: None,
            iterators: Iterators::default(),
        }
    }

    /// Run one call, and count it in the status of the storage.
    fn call(&mut self, call: &Call) -> Outcome {
        let was_open = self.open.is_some();
        let outcome = self.dispatch(call);
        self.count(&call.method, &outcome, was_open);
        outcome
    }

    /// Count a call that ran, as ZEO counts it. `was_open` tells if a
    /// transaction was open before the call.
    fn count(&self, method: &str, outcome: &Outcome, was_open: bool) {
        let stats = &self.storage.stats;
        let is_open = self.open.is_some();
        match (was_open, is_open) {
            (false, true) => Stats::add(&stats.active_txns),
            (true, false) => Stats::sub(&stats.active_txns),
            _ => {}
        }
        // An end of the transaction that the call named.
        let ended = was_open && !is_open;
        let counter = match outcome {
            Outcome::Close(_) => None,
            Outcome::Done(_) if method == "register" => Some(&stats.connections),
            Outcome::Done(_) if STORES.contains(&method) => Some(&stats.stores),
            Outcome::Failed(fault) if method == "vote" && fault.is_conflict() => {
                Some(&stats.conflicts)
            }
            _ if method == "loadBefore" => Some(&stats.loads),
            _ if method == "tpc_finish" && ended => Some(&stats.commits),
            _ if method == "tpc_abort" && ended => Some(&stats.aborts),
            _ => None,
        };
        if let Some(counter) = counter {
            Stats::add(counter);
        }
    }

    /// Run one call.
    fn dispatch(&mut self, call: &Call) -> Outcome {
        let args = call.args.as_slice();
        if !self.registered {
            return match call.method.as_str() {
                "register" => self.register(args).into(),
                method => Outcome::Close(format!("{method} before register")),
            };
        }
        let result = match call.method.as_str() {
            "register" => Err(Fault::value_error("duplicate register() call")),
            "lastTransaction" => Ok(p64(self.store.last_transaction().0)),
            "get_info" => arity::<0>(args, "get_info").and_then(|_| self.info()),
            "get_size_info" => arity::<0>(args, "get_size_info")
                .and_then(|_| self.sizes())
                .map(|items| Value::Map(items.into())),
            "server_status" => {
                arity::<0>(args, "server_status").map(|_| self.storage.status().to_value())
            }
            "ping" => arity::<0>(args, "ping").map(|_| Value::Nil),
            // ZEO puts the label in its log lines. The server does not keep
            // it.
            "set_client_label" => arity::<1>(args, "set_client_label").map(|_| Value::Nil),
            "new_oids" => self.new_oids(args),
            "loadBefore" => self.load_before(args),
            "loadSerial" => self.load_serial(args),
            "history" => self.history(args),
            "record_iternext" => self.record_iternext(args),
            "tpc_begin" => self.tpc_begin(args),
            "storea" => self.storea(args),
            "restorea" => self.restorea(args),
            "checkCurrentSerialInTransaction" => self.check_current_serial(args),
            "deleteObject" => self.delete_object(args),
            "vote" => self.vote(args),
            "tpc_finish" => self.tpc_finish(args),
            "tpc_abort" => self.tpc_abort(args),
            "iterator_start" => self.iterator_start(args),
            "iterator_next" => self.iterator_next(args),
            "iterator_record_start" => self.iterator_record_start(args),
            "iterator_record_next" => self.iterator_record_next(args),
            "iterator_gc" => self.iterator_gc(args),
            "pack" => self.pack(args),
            method => return Outcome::Close(format!("unknown method {method}")),
        };
        result.into()
    }

    fn register(&mut self, args: &[Value]) -> Result<Value, Fault> {
        let [storage, read_only] = arity::<2>(args, "register")?;
        let storage = match storage {
            Value::String(text) => text.as_str().unwrap_or_default().to_owned(),
            Value::Binary(bytes) => String::from_utf8_lossy(bytes).into_owned(),
            other => other.to_string(),
        };
        if storage != self.storage.name {
            return Err(Fault::value_error(format!("unknown storage: {storage}")));
        }
        self.read_only = truth(read_only);
        self.registered = true;
        self.client = Some(self.storage.join(self.queue.clone()));
        Ok(p64(self.store.last_transaction().0))
    }

    /// The information that a stock client asks for after `register`. The
    /// name is the directory of the store, as `zoe.Storage.getName` gives
    /// it.
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
            (Value::from("supportsUndo"), Value::Boolean(false)),
            (
                Value::from("supports_record_iternext"),
                Value::Boolean(true),
            ),
            (Value::from("interfaces"), Value::Array(interfaces)),
        ]);
        Ok(Value::Map(items))
    }

    /// The items `length` and `size` of the store. The length is 0, as
    /// `len(zoe.Storage)` is: the store keeps no object count, and
    /// `IStorage` asks only for an approximate length. The size is the
    /// size of the files in the store directory.
    fn sizes(&self) -> Result<[(Value, Value); 2], Fault> {
        let size = self
            .storage
            .size()
            .map_err(|error| Fault::storage(format!("the size of the store: {error}")))?;
        Ok([
            (Value::from("length"), Value::from(0)),
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
        if self.read_only {
            return Err(Fault::read_only());
        }
        let oids = (0..n)
            .map(|_| self.store.new_oid().map(|oid| p64(oid.0)))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Value::Array(oids))
    }

    fn load_before(&self, args: &[Value]) -> Result<Value, Fault> {
        let [oid, tid] = arity::<2>(args, "loadBefore")?;
        let (oid, tid) = (Oid(u64_of(oid)?), Tid(u64_of(tid)?));
        Ok(match self.store.load_before(oid, tid)? {
            Some(record) => Value::Array(vec![
                Value::Binary(record.data),
                p64(record.serial.0),
                record.next.map_or(Value::Nil, |next| p64(next.0)),
            ]),
            None => Value::Nil,
        })
    }

    fn load_serial(&self, args: &[Value]) -> Result<Value, Fault> {
        let [oid, serial] = arity::<2>(args, "loadSerial")?;
        let (oid, serial) = (Oid(u64_of(oid)?), Tid(u64_of(serial)?));
        Ok(Value::Binary(self.store.load_serial(oid, serial)?))
    }

    /// Return at most `size` records of an oid, newest first, as the maps
    /// of FileStorage's `history`. A map has no keys of the transaction
    /// extension.
    fn history(&self, args: &[Value]) -> Result<Value, Fault> {
        let (oid, size) = match args {
            [oid] => (oid, 1),
            // A negative size gives no entry, as in FileStorage.
            [oid, Value::Integer(size)] => (oid, size.as_u64().unwrap_or(0)),
            [_, _] => return Err(Fault::type_error("size is not an int")),
            _ => return Err(Fault::type_error("history takes 1 or 2 arguments")),
        };
        let oid = Oid(u64_of(oid)?);
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
            let txn = self.begin(args);
            self.open = Some(Open {
                id: id.clone(),
                txn,
            });
        }
        Ok(Value::Nil)
    }

    /// Return the core transaction that `args` of `tpc_begin` describe.
    fn begin(&self, args: &[Value]) -> Result<Txn<Arc<Store>>, Fault> {
        if args.len() < 4 || args.len() > 6 {
            return Err(Fault::type_error("tpc_begin takes 4 to 6 arguments"));
        }
        if self.read_only {
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
        let store = Arc::clone(&self.store);
        match args.get(4).filter(|tid| !tid.is_nil()) {
            None => Ok(Txn::begin(store, &user, &description, &ext)),
            Some(tid) => {
                let tid = Tid(u64_of(tid)?);
                Ok(Txn::begin_restore(store, tid, &user, &description, &ext)?)
            }
        }
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

    fn storea(&mut self, args: &[Value]) -> Result<Value, Fault> {
        let [oid, serial, data, id] = arity::<4>(args, "storea")?;
        self.current(id)?.stage(|txn| {
            let (oid, serial) = (Oid(u64_of(oid)?), Tid(u64_of(serial)?));
            let Value::Binary(data) = data else {
                return Err(Fault::type_error("data is not bytes"));
            };
            Ok(txn.store(oid, serial, data)?)
        });
        Ok(Value::Nil)
    }

    /// Stage a copy of a record in the restore transaction `id`, a
    /// transaction that `tpc_begin` began with a tid. Nil data copies a
    /// deletion. As in `zoe.Storage`, the record gets the tid of the
    /// transaction, not `serial`, and `prev_txn` is not used. In a
    /// transaction without a tid, the vote fails with
    /// `StorageTransactionError`.
    fn restorea(&mut self, args: &[Value]) -> Result<Value, Fault> {
        let [oid, _serial, data, _prev_txn, id] = arity::<5>(args, "restorea")?;
        self.current(id)?.stage(|txn| {
            let oid = Oid(u64_of(oid)?);
            let data = match data {
                Value::Binary(data) => Some(data.as_slice()),
                Value::Nil => None,
                _ => return Err(Fault::type_error("data is not bytes or None")),
            };
            Ok(txn.restore(oid, data)?)
        });
        Ok(Value::Nil)
    }

    /// Record that the transaction read `oid` at `serial`. The vote takes
    /// a shared lock on `oid`, and a stale serial fails the vote with
    /// `ReadConflictError`.
    fn check_current_serial(&mut self, args: &[Value]) -> Result<Value, Fault> {
        let [oid, serial, id] = arity::<3>(args, "checkCurrentSerialInTransaction")?;
        self.current(id)?.stage(|txn| {
            let (oid, serial) = (Oid(u64_of(oid)?), Tid(u64_of(serial)?));
            Ok(txn.check_current_serial(oid, serial)?)
        });
        Ok(Value::Nil)
    }

    /// Stage a deletion of `oid`. The vote checks `serial` as it does for
    /// `storea`.
    fn delete_object(&mut self, args: &[Value]) -> Result<Value, Fault> {
        let [oid, serial, id] = arity::<3>(args, "deleteObject")?;
        self.current(id)?.stage(|txn| {
            let (oid, serial) = (Oid(u64_of(oid)?), Tid(u64_of(serial)?));
            Ok(txn.delete(oid, serial)?)
        });
        Ok(Value::Nil)
    }

    /// Vote. A failed vote ends the transaction, as in ZEO: the core
    /// transaction drops, which releases its locks. So the `tpc_abort`
    /// that follows finds no transaction.
    fn vote(&mut self, args: &[Value]) -> Result<Value, Fault> {
        let [id] = arity::<1>(args, "vote")?;
        let open = self.current(id)?;
        let voted = match &mut open.txn {
            Ok(txn) => txn.vote().map_err(|error| Fault::of_vote(&error, txn)),
            Err(fault) => Err(fault.clone()),
        };
        if voted.is_err() {
            self.open = None;
        }
        voted.map(|()| Value::Array(vec![]))
    }

    /// Commit the transaction and return its tid. As in ZEO, a finish of a
    /// transaction that is not the open one does nothing and gives None.
    fn tpc_finish(&mut self, args: &[Value]) -> Result<Value, Fault> {
        let [id] = arity::<1>(args, "tpc_finish")?;
        if !self.open.as_ref().is_some_and(|open| &open.id == id) {
            return Ok(Value::Nil);
        }
        let open = self.open.take().expect("the open transaction");
        let Ok(txn) = open.txn else {
            return Err(Fault::transaction("tpc_finish before vote"));
        };
        // The core returns only after the commit is durable.
        let tid = txn.finish()?;
        Ok(p64(tid.0))
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

    /// Start an iterator over the transactions from the tid `start` to the
    /// tid `stop`, both included. Nil is no bound. Return the id of the
    /// iterator.
    fn iterator_start(&mut self, args: &[Value]) -> Result<Value, Fault> {
        let [start, stop] = arity::<2>(args, "iterator_start")?;
        let (start, stop) = (optional_tid(start)?, optional_tid(stop)?);
        let transactions = self.store.iterator(start, stop)?;
        let id = self
            .iterators
            .start(transactions)
            .map_err(|refused| Fault::of_iterator(refused, &Value::Nil))?;
        Ok(Value::from(id))
    }

    /// Return the next transaction of the iterator `id`, as `[tid, status,
    /// user, description, extension]`, or nil at its end. The status is
    /// always `" "`. The extension is the map of the extension bytes that
    /// the store keeps, because a stock client takes the value of the wire
    /// as the extension.
    fn iterator_next(&mut self, args: &[Value]) -> Result<Value, Fault> {
        let [id] = arity::<1>(args, "iterator_next")?;
        let next = id
            .as_u64()
            .ok_or(Refused::Unknown)
            .and_then(|iid| self.iterators.next_transaction(iid))
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
    fn iterator_record_start(&mut self, args: &[Value]) -> Result<Value, Fault> {
        let [id, tid] = arity::<2>(args, "iterator_record_start")?;
        let tid = Tid(u64_of(tid)?);
        let records = id
            .as_u64()
            .ok_or(Refused::Unknown)
            .and_then(|iid| self.iterators.start_records(iid, tid))
            .map_err(|refused| Fault::of_iterator(refused, id))?;
        Ok(Value::from(records))
    }

    /// Return the next record of the record iterator `id`, as `[oid, tid,
    /// data, data_txn]`, or nil at its end. The data of a deletion is nil.
    /// The store keeps every pickle whole, so `data_txn` is always nil.
    fn iterator_record_next(&mut self, args: &[Value]) -> Result<Value, Fault> {
        let [id] = arity::<1>(args, "iterator_record_next")?;
        let next = id
            .as_u64()
            .ok_or(Refused::Unknown)
            .and_then(|iid| self.iterators.next_record(iid))
            .map_err(|refused| Fault::of_iterator(refused, id))?;
        Ok(next.map_or(Value::Nil, |record| {
            Value::Array(vec![
                p64(record.oid.0),
                p64(record.tid.0),
                record.data.map_or(Value::Nil, Value::Binary),
                Value::Nil,
            ])
        }))
    }

    /// Remove the iterators that the client dropped. The client lists each
    /// record iterator that it dropped, also one at its end, which is gone.
    /// So an id that names no open iterator does nothing, as in ZEO.
    fn iterator_gc(&mut self, args: &[Value]) -> Result<Value, Fault> {
        let [ids] = arity::<1>(args, "iterator_gc")?;
        let Value::Array(ids) = ids else {
            return Err(Fault::type_error("the iterator ids are not a list"));
        };
        self.iterators.gc(ids.iter().filter_map(Value::as_u64));
        Ok(Value::Nil)
    }

    /// Pack the store at the tid of the time `time`, as FileStorage
    /// computes it. The other connections read and commit while the pack
    /// runs. The later calls of this connection wait for the pack, because
    /// a connection runs its calls in order.
    ///
    /// After the pack, every client of the storage gets `info` with the new
    /// size. The reply goes out after the `info` of this client. It also
    /// goes out after the pack when `wait` is false.
    ///
    /// A read-only connection and a read-only store refuse the pack with
    /// `ReadOnlyError`.
    fn pack(&mut self, args: &[Value]) -> Result<Value, Fault> {
        let time = match args {
            [time] | [time, _] => time,
            _ => return Err(Fault::type_error("pack takes 1 or 2 arguments")),
        };
        if self.read_only {
            return Err(Fault::read_only());
        }
        let time = match time {
            Value::F64(time) => *time,
            Value::F32(time) => f64::from(*time),
            Value::Integer(time) => time.as_f64().unwrap_or(f64::NAN),
            _ => return Err(Fault::type_error("the pack time is not a number")),
        };
        let tid = clock::pack_tid(time)
            .ok_or_else(|| Fault::value_error(format!("the pack time {time} has no tid")))?;
        self.store
            .pack(tid)
            .map_err(|error| Fault::of_pack(&error))?;
        let info = Value::Map(Vec::from(self.sizes()?));
        self.storage.broadcast(&message::notify("info", vec![info]));
        Ok(Value::Nil)
    }
}

impl Drop for Session {
    /// The end of a connection leaves the status: a transaction that is
    /// still open counts as an abort, as in ZEO. The connection leaves the
    /// clients of the storage, and its iterators go with the session.
    fn drop(&mut self) {
        let stats = &self.storage.stats;
        if self.open.is_some() {
            Stats::sub(&stats.active_txns);
            Stats::add(&stats.aborts);
        }
        if self.registered {
            Stats::sub(&stats.connections);
        }
        if let Some(client) = self.client {
            self.storage.leave(client);
        }
    }
}

/// Return the `N` arguments of `method`.
fn arity<'a, const N: usize>(args: &'a [Value], method: &str) -> Result<&'a [Value; N], Fault> {
    args.try_into()
        .map_err(|_| Fault::type_error(format!("{method} takes {N} arguments")))
}

/// An oid or a tid as the protocol sends it: 8 bytes, big-endian.
fn p64(n: u64) -> Value {
    Value::Binary(n.to_be_bytes().to_vec())
}

/// A tid, or nil for none.
fn optional_tid(value: &Value) -> Result<Option<Tid>, Fault> {
    match value {
        Value::Nil => Ok(None),
        tid => Ok(Some(Tid(u64_of(tid)?))),
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
    use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

    use super::*;

    /// A pickle of a `persistent.mapping.PersistentMapping` record, as ZODB
    /// writes it: the class pickle, then the state pickle.
    const MAPPING: &[u8] = b"\x80\x03X\x12\x00\x00\x00persistent.mappingq\x00\
        X\x11\x00\x00\x00PersistentMappingq\x01\x86q\x02N\x86q\x03.\x80\x03}q\x00.";

    /// A storage on a new store in the directory of the test `name`.
    fn storage(name: &str) -> Arc<NamedStorage> {
        let dir = std::env::temp_dir().join(format!("zoe-session-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Arc::new(Store::open(&dir).unwrap());
        Arc::new(NamedStorage::new("1", &dir, store))
    }

    /// A session of `storage`, and the queue of the frames that it sends
    /// without a call.
    fn connect(storage: &Arc<NamedStorage>) -> (Session, UnboundedReceiver<Vec<u8>>) {
        let (queue, frames) = unbounded_channel();
        (Session::new(Arc::clone(storage), queue), frames)
    }

    fn session(name: &str) -> Session {
        connect(&storage(name)).0
    }

    fn call(session: &mut Session, method: &str, args: Vec<Value>) -> Outcome {
        session.call(&Call {
            id: Value::from(1),
            oneway: false,
            method: method.into(),
            args,
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

    /// Begin the transaction `id`, with the tid `tid` for a restore
    /// transaction.
    fn begin(s: &mut Session, id: i64, tid: Option<u64>) {
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

    fn store(s: &mut Session, id: i64, oid: u64, serial: u64, data: &[u8]) {
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
    fn vote_and_finish(s: &mut Session, id: i64) -> Result<u64, Fault> {
        match call(s, "vote", vec![Value::from(id)]) {
            Outcome::Done(_) => {
                let tid = done(call(s, "tpc_finish", vec![Value::from(id)]));
                Ok(u64_of(&tid).unwrap())
            }
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
        assert!(s.open.is_none());
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
        assert!(s.open.is_none());
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
        assert!(s.open.is_none());
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
        // The serial check of storea holds in a restore transaction.
        begin(&mut s, 3, Some(wanted + 1));
        store(&mut s, 3, 1, first, b"c");
        let fault = vote_and_finish(&mut s, 3).unwrap_err();
        assert_eq!(fault.class, CONFLICT_ERROR);
    }

    fn registered(name: &str) -> Session {
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
    fn commit(s: &mut Session, id: i64, oid: u64, serial: u64, data: &[u8]) -> Result<u64, Fault> {
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
            Outcome::Done(_) => Ok(u64_of(&done(call(s, "tpc_finish", vec![id]))).unwrap()),
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
        assert_eq!(get(&info, "supportsUndo"), &Value::Boolean(false));
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
        for name in ["IStorageUndoable", "IExternalGC"] {
            assert!(!interfaces.contains(&("ZODB.interfaces", name)), "{name}");
        }

        let sizes = done(call(&mut s, "get_size_info", vec![]));
        assert_eq!(keys(&sizes), ["length", "size"]);
        assert_eq!(get(&sizes, "size"), get(&info, "size"));
        let fault = failed(call(&mut s, "get_info", vec![Value::Nil]));
        assert_eq!(fault.class, "builtins.TypeError");
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
        let (mut other, _) = connect(&storage);
        // A session that does not register is not a connection.
        let (_unregistered, _) = connect(&storage);
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
    fn send(session: &mut Session, method: &str, args: Vec<Value>) -> Outcome {
        session.call(&Call {
            id: Value::from(0),
            oneway: true,
            method: method.into(),
            args,
        })
    }

    /// Commit the transaction `id` with the extension `ext`. It stores
    /// `data` for each `(oid, serial)` of `oids`. Return its tid.
    fn commit_with(s: &mut Session, id: i64, ext: Value, oids: &[(u64, u64)], data: &[u8]) -> u64 {
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
        u64_of(&done(call(s, "tpc_finish", vec![id]))).unwrap()
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
        let range = |s: &mut Session, start: Value, stop: Value| {
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
        let storage = storage("iterator-ids");
        let (mut s, _) = connect(&storage);
        let (mut other, _) = connect(&storage);
        for session in [&mut s, &mut other] {
            done(call(
                session,
                "register",
                vec![Value::from("1"), Value::Boolean(false)],
            ));
        }
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
        // A closed connection leaves the clients of the storage.
        drop(other);
        assert_eq!(storage.clients.lock().unwrap().queues.len(), 1);
        drop(s);
        assert!(storage.clients.lock().unwrap().queues.is_empty());
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
        let restore = |s: &mut Session, id: i64, oid: u64, data: Value| {
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
    fn pack_sends_info_to_every_client_before_its_reply() {
        let storage = storage("pack-info");
        let (mut other, mut other_frames) = connect(&storage);
        done(call(
            &mut other,
            "register",
            vec![Value::from("1"), Value::Boolean(false)],
        ));
        let (_unregistered, mut unregistered_frames) = connect(&storage);
        let root = commit(&mut other, 1, 0, 0, MAPPING).unwrap();
        let dropped = commit(&mut other, 2, 1, 0, MAPPING).unwrap();

        // The packing client runs as a connection does.
        let (calls, received) = mpsc::channel();
        let (queue, mut frames) = unbounded_channel();
        let worker = std::thread::spawn({
            let storage = Arc::clone(&storage);
            move || run(storage, received, queue)
        });
        let request = |id: i64, method: &str, args: Vec<Value>| Call {
            id: Value::from(id),
            oneway: false,
            method: method.into(),
            args,
        };
        let register = vec![Value::from("1"), Value::Boolean(false)];
        calls.send(request(1, "register", register)).unwrap();
        // A time in 2099: the pack packs at the last tid.
        let pack = vec![Value::F64(4_102_444_799.0), Value::from(1)];
        calls.send(request(2, "pack", pack)).unwrap();
        drop(calls);
        worker.join().unwrap();

        let size = storage.size().unwrap();
        let info = message::notify(
            "info",
            vec![Value::Map(vec![
                (Value::from("length"), Value::from(0)),
                (Value::from("size"), Value::from(size)),
            ])],
        );
        let got: Vec<_> = std::iter::from_fn(|| frames.try_recv().ok()).collect();
        assert_eq!(
            got,
            [
                message::reply(Value::from(1), p64(dropped)),
                info.clone(),
                message::reply(Value::from(2), Value::Nil),
            ]
        );
        assert_eq!(other_frames.try_recv().unwrap(), info);
        assert!(other_frames.try_recv().is_err());
        assert!(unregistered_frames.try_recv().is_err());

        // The pack removed the object that the root does not reach.
        let fault = failed(call(&mut other, "loadSerial", vec![p64(1), p64(dropped)]));
        assert_eq!(fault, Fault::from(ZoeError::NotFound { oid: Oid(1) }));
        let load = done(call(&mut other, "loadSerial", vec![p64(0), p64(root)]));
        assert_eq!(load, Value::Binary(MAPPING.to_vec()));
    }

    #[test]
    fn pack_takes_a_time_and_refuses_a_read_only_store() {
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

        // A read-only connection.
        let mut s = session("pack-read-only-client");
        done(call(
            &mut s,
            "register",
            vec![Value::from("1"), Value::Boolean(true)],
        ));
        let fault = failed(call(&mut s, "pack", vec![Value::F64(1e9)]));
        assert_eq!(fault, Fault::read_only());

        // A read-only store.
        let dir = s.storage.path.clone();
        drop(s);
        let store = Arc::new(Store::open_read_only(&dir).unwrap());
        let storage = Arc::new(NamedStorage::new("1", &dir, store));
        let (mut s, _) = connect(&storage);
        done(call(
            &mut s,
            "register",
            vec![Value::from("1"), Value::Boolean(false)],
        ));
        let fault = failed(call(&mut s, "pack", vec![Value::F64(1e9)]));
        assert_eq!(fault, Fault::read_only());
    }
}
