//! The calls of one connection, run in order on one thread.
//!
//! The session holds the open transaction of the connection. Its thread
//! begins the core transaction and makes every later call of it, so the
//! core never refuses a call with `WrongThread`.
//!
//! `register` adds the connection to the invalidations of the storage. A
//! commit goes to the invalidations with the reply of its `tpc_finish`, so
//! the reply and the invalidations reach each client in commit order.

use std::sync::{Arc, mpsc};

use rmpv::Value;
use zoe_core::{Finished, Oid, Store, Tid, Txn, ZoeError};

use super::NamedStorage;
use super::clock;
use super::extension;
use super::invalidation::{ClientId, Commit, Others};
use super::message::{self, Call};
use super::outbox::{Outbox, Place};
use super::status::Stats;

/// The most oids that one `new_oids` call gives, as in ZEO.
const MAX_NEW_OIDS: u64 = 100;

/// The reads of a `loadBefore` that can each meet a publication before
/// the call reads under the mutex of the hub.
const LOAD_TRIES: usize = 3;

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
    /// A `tpc_finish` that committed. Its reply went out with the
    /// invalidations of the commit.
    Published,
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
    /// The frames that wait for the client.
    outbox: Outbox,
    /// The id of the connection in the invalidations of the storage, from
    /// `register` on.
    client: Option<ClientId>,
    /// The place in the order of the connection that the current call took
    /// for its reply, before the reply exists.
    place: Option<Place>,
    registered: bool,
    read_only: bool,
    open: Option<Open>,
}

/// Run the calls that arrive on `calls` in order, and put each reply in
/// `outbox`. Return when `calls` closes, or when the session closes the
/// connection. A transaction still open aborts.
pub(crate) fn run(storage: Arc<NamedStorage>, calls: mpsc::Receiver<Call>, outbox: Outbox) {
    let mut session = Session::new(storage, outbox.clone());
    while let Ok(call) = calls.recv() {
        let outcome = session.call(&call);
        let reply = match outcome {
            Outcome::Published => None,
            Outcome::Done(_) if call.oneway => None,
            Outcome::Done(value) => Some(message::reply(call.id, value)),
            // A call without a reply has no way to report its error, so
            // the server closes the connection, as ZEO does.
            Outcome::Failed(fault) if call.oneway => {
                eprintln!(
                    "zoe serve: {} failed: {} {:?}",
                    call.method, fault.class, fault.args
                );
                break;
            }
            Outcome::Failed(fault) => Some(message::error(call.id, fault.class, fault.args)),
            Outcome::Close(why) => {
                eprintln!("zoe serve: closing a connection: {why}");
                break;
            }
        };
        // A place that the call took gets the reply, or stays empty.
        let sent = match (session.place.take(), reply) {
            (Some(place), reply) => outbox.send_at(place, reply),
            (None, Some(reply)) => outbox.send(reply),
            (None, None) => true,
        };
        if !sent {
            break;
        }
    }
}

impl Session {
    /// Return the session of a connection to `storage`, whose frames go to
    /// `outbox`.
    pub(crate) fn new(storage: Arc<NamedStorage>, outbox: Outbox) -> Self {
        Self {
            store: Arc::clone(&storage.store),
            storage,
            outbox,
            client: None,
            place: None,
            registered: false,
            read_only: false,
            open: None,
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
            "lastTransaction" => Ok(p64(self.storage.invalidations.published().0)),
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
            "getInvalidations" => self.get_invalidations(args),
            "history" => self.history(args),
            "record_iternext" => self.record_iternext(args),
            "tpc_begin" => self.tpc_begin(args),
            "storea" => self.storea(args),
            "checkCurrentSerialInTransaction" => self.check_current_serial(args),
            "deleteObject" => self.delete_object(args),
            "vote" => self.vote(args),
            "tpc_finish" => return self.tpc_finish(call),
            "tpc_abort" => self.tpc_abort(args),
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
        // The client gets the invalidation of each commit after this tid.
        let (client, published) = self.storage.invalidations.subscribe(self.outbox.clone());
        self.client = Some(client);
        Ok(p64(published.0))
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

    /// Load the record of an oid before a tid: `[data, serial, next serial
    /// or nil]`, or nil.
    ///
    /// The reply shows the commits that the hub published, and no other:
    /// the read takes the record before the last published tid plus 1 at
    /// most, and it gives a next serial only when the hub published it. The
    /// reply then takes its place in the order of the connection, if the
    /// hub published no commit during the read. So the reply comes after
    /// the invalidation of each commit that it shows, and before the
    /// invalidation of each later commit. A stock client keeps a record
    /// without a next serial as the current one, and needs this order.
    ///
    /// A publication during the read makes the call read again. After
    /// `LOAD_TRIES` reads, it reads under the mutex of the hub.
    fn load_before(&mut self, args: &[Value]) -> Result<Value, Fault> {
        let [oid, tid] = arity::<2>(args, "loadBefore")?;
        let (oid, tid) = (Oid(u64_of(oid)?), Tid(u64_of(tid)?));
        let store = &self.store;
        let read = |published: Tid| -> Result<Value, Fault> {
            let before = tid.min(Tid(published.0.saturating_add(1)));
            Ok(match store.load_before(oid, before)? {
                Some(record) => Value::Array(vec![
                    Value::Binary(record.data),
                    p64(record.serial.0),
                    record
                        .next
                        .filter(|next| *next <= published)
                        .map_or(Value::Nil, |next| p64(next.0)),
                ]),
                None => Value::Nil,
            })
        };
        let invalidations = &self.storage.invalidations;
        for _ in 0..LOAD_TRIES {
            let seen = invalidations.published();
            let value = read(seen)?;
            if let Some(place) = invalidations.place_since(seen, &self.outbox) {
                self.place = Some(place);
                return Ok(value);
            }
        }
        let (value, place) = invalidations.read_placed(&self.outbox, read);
        self.place = Some(place);
        value
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
        if args
            .get(5)
            .is_some_and(|status| status.as_str() != Some(" "))
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
        self.publish(finished, reply);
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
        // The core returns only after the commit is durable.
        Ok(Some(txn.finish_after()?))
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
            origin: self.client,
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
                eprintln!(
                    "zoe serve: the oids of the commit {:016x} do not read: {error}",
                    tid.0
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

    /// Return the last published tid and every oid that a commit after the
    /// tid of the call wrote: `[tid, oids]`, each oid once. The oids come
    /// from the transaction index of the store, so the reply does not
    /// depend on what this process saw.
    ///
    /// Return nil when more than `invalidation-queue-size` transactions
    /// that wrote records come after the tid, or when the index does not
    /// read. The client then verifies its cache.
    fn get_invalidations(&self, args: &[Value]) -> Result<Value, Fault> {
        let [tid] = arity::<1>(args, "getInvalidations")?;
        let tid = u64_of(tid)?;
        // The connection gets the invalidation of each commit after `last`.
        let last = self.storage.invalidations.published();
        let oids = match tid.checked_add(1) {
            Some(from) => {
                self.store
                    .written_oids(Tid(from), last, self.storage.queues.invalidations)
            }
            None => Ok(Some(Vec::new())),
        };
        Ok(match oids {
            Ok(Some(oids)) => Value::Array(vec![
                p64(last.0),
                Value::Array(oids.into_iter().map(|oid| p64(oid.0)).collect()),
            ]),
            Ok(None) => Value::Nil,
            Err(error) => {
                eprintln!(
                    "zoe serve: getInvalidations: the transaction index does not read: {error}"
                );
                Value::Nil
            }
        })
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
}

impl Drop for Session {
    /// The end of a connection leaves the status: a transaction that is
    /// still open counts as an abort, as in ZEO. The connection leaves the
    /// invalidations of the storage.
    fn drop(&mut self) {
        let stats = &self.storage.stats;
        if self.open.is_some() {
            Stats::sub(&stats.active_txns);
            Stats::add(&stats.aborts);
        }
        if self.registered {
            Stats::sub(&stats.connections);
        }
        if let Some(client) = self.client.take() {
            self.storage.invalidations.unsubscribe(client);
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
    use super::super::outbox::Inbox;
    use super::*;
    use crate::protocol::Queues;

    /// A pickle of a `persistent.mapping.PersistentMapping` record, as ZODB
    /// writes it: the class pickle, then the state pickle.
    const MAPPING: &[u8] = b"\x80\x03X\x12\x00\x00\x00persistent.mappingq\x00\
        X\x11\x00\x00\x00PersistentMappingq\x01\x86q\x02N\x86q\x03.\x80\x03}q\x00.";

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

    fn session(name: &str) -> Session {
        Session::new(storage(name, Queues::DEFAULT), outbox())
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

    /// Finish the transaction `id`, and return its tid. With no other
    /// session, the commit is the last published one.
    fn finish(s: &mut Session, id: Value) -> u64 {
        match call(s, "tpc_finish", vec![id]) {
            Outcome::Published => {}
            other => panic!("not published: {other:?}"),
        }
        u64_of(&done(call(s, "lastTransaction", vec![]))).unwrap()
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
            Value::from("p"),
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
        let mut other = Session::new(Arc::clone(&storage), outbox());
        // A session that does not register is not a connection.
        let _unregistered = Session::new(Arc::clone(&storage), outbox());
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

    /// A registered session of `storage`. Its frames wait in the receiver
    /// for the test.
    fn connected(storage: &Arc<NamedStorage>) -> (Session, Inbox) {
        let (outbox, frames, _closing) = Outbox::new(1_000, 1_000);
        let mut s = Session::new(Arc::clone(storage), outbox);
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
            let message = rmpv::decode::read_value(&mut frame.as_slice()).unwrap();
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
        // A commit is published before the reply goes into the queue.
        let second = commit(&mut writer, 2, 1, first, b"new").unwrap();
        let place = reader.place.take().expect("loadBefore takes a place");
        let reply = message::reply(Value::from(1), current);
        assert!(reader.outbox.send_at(place, Some(reply)));
        let messages = messages(&mut frames);
        assert_eq!(messages[0].0, ".reply");
        assert_eq!(messages[1], invalidation(second, &[1]));
    }

    /// Put the reply `value` of the last call of `s` at the place that the
    /// call took.
    fn answer(s: &mut Session, value: Value) {
        let place = s.place.take().expect("the call took a place");
        assert!(
            s.outbox
                .send_at(place, Some(message::reply(Value::from(1), value)))
        );
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
        answer(&mut reader, current);
        // A load before the hidden commit does not name it.
        let before = vec![p64(1), p64(hidden.tid.0)];
        let previous = done(call(&mut reader, "loadBefore", before.clone()));
        assert_eq!(previous, old(Value::Nil));
        answer(&mut reader, previous);

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
        answer(&mut reader, current);
        let previous = done(call(&mut reader, "loadBefore", before));
        assert_eq!(previous, old(p64(hidden.tid.0)));
        answer(&mut reader, previous);
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
            client: 1_000,
        };
        let mut s = Session::new(storage("get-invalidations", queues), outbox());
        done(call(
            &mut s,
            "register",
            vec![Value::from("1"), Value::Boolean(false)],
        ));
        let first = commit(&mut s, 1, 1, 0, b"a").unwrap();
        let second = commit(&mut s, 2, 2, 0, b"b").unwrap();
        commit(&mut s, 3, 3, 0, b"c").unwrap();
        let last = commit(&mut s, 4, 1, first, b"d").unwrap();
        let get = |s: &mut Session, tid: Value| done(call(s, "getInvalidations", vec![tid]));

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
                let mut s = Session::new(Arc::clone(storage), outbox);
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
}
