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
use super::message::{self, Call};
use super::status::Stats;

/// The most oids that one `new_oids` call gives, as in ZEO.
const MAX_NEW_OIDS: u64 = 100;

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
        Self::new("ZODB.POSException.StorageTransactionError", text)
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

    /// Return the exception of ZODB for `error`. `serials` are the current
    /// serial and the serial that the transaction read, for a conflict.
    fn of(error: &ZoeError, serials: Option<(Tid, Tid)>) -> Self {
        let conflict = |class, message: &str, oid: Oid| {
            let serials = serials.map_or(Value::Nil, |(current, read)| {
                Value::Array(vec![p64(current.0), p64(read.0)])
            });
            Self {
                class,
                args: Value::Map(vec![
                    (Value::from("message"), Value::from(message)),
                    (Value::from("oid"), p64(oid.0)),
                    (Value::from("class_name"), Value::Nil),
                    (Value::from("serials"), serials),
                ]),
            }
        };
        let text = error.to_string();
        match error {
            ZoeError::NotFound { oid } => Self {
                class: "ZODB.POSException.POSKeyError",
                args: Value::Array(vec![p64(oid.0)]),
            },
            ZoeError::Conflict { oid } => {
                conflict("ZODB.POSException.ConflictError", CONFLICT, *oid)
            }
            ZoeError::ReadConflict { oid } => {
                conflict("ZODB.POSException.ReadConflictError", READ_CONFLICT, *oid)
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
        Self::of(&error, None)
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
    txn: Txn<Arc<Store>>,
    /// The first error of a `storea`. The vote fails with it, as ZEO
    /// reports the errors of its stores at the vote.
    failed: Option<ZoeError>,
}

/// The state of one connection.
pub(crate) struct Session {
    storage: Arc<NamedStorage>,
    /// The store of `storage`.
    store: Arc<Store>,
    registered: bool,
    read_only: bool,
    open: Option<Open>,
}

/// Run the calls that arrive on `calls` in order, and send each reply to
/// `replies`. Return when `calls` closes, or when the session closes the
/// connection. A transaction still open aborts.
pub(crate) fn run(
    storage: Arc<NamedStorage>,
    calls: mpsc::Receiver<Call>,
    replies: UnboundedSender<Vec<u8>>,
) {
    let mut session = Session::new(storage);
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
    pub(crate) fn new(storage: Arc<NamedStorage>) -> Self {
        Self {
            store: Arc::clone(&storage.store),
            storage,
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
            "vote" => self.vote(args),
            "tpc_finish" => self.tpc_finish(args),
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

    fn tpc_begin(&mut self, args: &[Value]) -> Result<Value, Fault> {
        if args.len() < 4 || args.len() > 6 {
            return Err(Fault::type_error("tpc_begin takes 4 to 6 arguments"));
        }
        if self.read_only {
            return Err(Fault::read_only());
        }
        let id = &args[0];
        if let Some(open) = &self.open {
            if &open.id == id {
                return Ok(Value::Nil);
            }
            return Err(Fault::transaction(
                "Multiple simultaneous tpc_begin requests from one client.",
            ));
        }
        if args.get(4).is_some_and(|tid| !tid.is_nil()) {
            return Err(Fault::transaction(
                "the server does not take restore transactions yet",
            ));
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
        let txn = Txn::begin(Arc::clone(&self.store), &user, &description, &ext);
        self.open = Some(Open {
            id: id.clone(),
            txn,
            failed: None,
        });
        Ok(Value::Nil)
    }

    /// Return the open transaction with the id `id`.
    fn current(&mut self, id: &Value, method: &str) -> Result<&mut Open, Fault> {
        match &mut self.open {
            Some(open) if &open.id == id => Ok(open),
            Some(_) => Err(Fault::transaction(format!(
                "{method}: the transaction is not the current one"
            ))),
            None => Err(Fault::transaction(format!(
                "{method}: no current transaction"
            ))),
        }
    }

    fn storea(&mut self, args: &[Value]) -> Result<Value, Fault> {
        let [oid, serial, data, id] = arity::<4>(args, "storea")?;
        let (oid, serial) = (Oid(u64_of(oid)?), Tid(u64_of(serial)?));
        let Value::Binary(data) = data else {
            return Err(Fault::type_error("data is not bytes"));
        };
        let open = self.current(id, "storea")?;
        if open.failed.is_none()
            && let Err(error) = open.txn.store(oid, serial, data)
        {
            open.failed = Some(error);
        }
        Ok(Value::Nil)
    }

    fn vote(&mut self, args: &[Value]) -> Result<Value, Fault> {
        let [id] = arity::<1>(args, "vote")?;
        let open = self.current(id, "vote")?;
        if let Some(error) = open.failed.take() {
            self.open = None;
            return Err(error.into());
        }
        match open.txn.vote() {
            Ok(()) => Ok(Value::Array(vec![])),
            Err(error) => {
                let fault = Fault::of(&error, open.txn.conflict_serials());
                // Dropping the transaction aborts it.
                self.open = None;
                Err(fault)
            }
        }
    }

    fn tpc_finish(&mut self, args: &[Value]) -> Result<Value, Fault> {
        let [id] = arity::<1>(args, "tpc_finish")?;
        self.current(id, "tpc_finish")?;
        let open = self.open.take().expect("the current transaction");
        // The core returns only after the commit is durable.
        let tid = open.txn.finish()?;
        Ok(p64(tid.0))
    }

    fn tpc_abort(&mut self, args: &[Value]) -> Result<Value, Fault> {
        let [id] = arity::<1>(args, "tpc_abort")?;
        // As in ZEO, an abort of another transaction does nothing.
        if self.open.as_ref().is_some_and(|open| &open.id == id)
            && let Some(open) = self.open.take()
        {
            open.txn.abort();
        }
        Ok(Value::Nil)
    }
}

impl Drop for Session {
    /// The end of a connection leaves the status: a transaction that is
    /// still open counts as an abort, as in ZEO.
    fn drop(&mut self) {
        let stats = &self.storage.stats;
        if self.open.is_some() {
            Stats::sub(&stats.active_txns);
            Stats::add(&stats.aborts);
        }
        if self.registered {
            Stats::sub(&stats.connections);
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
    use super::*;

    fn session(name: &str) -> Session {
        let dir = std::env::temp_dir().join(format!("zoe-session-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Arc::new(Store::open(&dir).unwrap());
        Session::new(Arc::new(NamedStorage::new("1", &dir, store)))
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
    fn a_read_only_client_gets_no_oid_and_begins_nothing() {
        let mut s = session("read-only");
        done(call(
            &mut s,
            "register",
            vec![Value::from("1"), Value::Boolean(true)],
        ));
        let fault = failed(call(&mut s, "new_oids", vec![]));
        assert_eq!(fault, Fault::read_only());
        let args = vec![Value::from(1), Value::Nil, Value::Nil, Value::Map(vec![])];
        assert_eq!(failed(call(&mut s, "tpc_begin", args)), Fault::read_only());
    }

    #[test]
    fn new_oids_gives_at_most_100_and_at_least_1() {
        let mut s = session("new-oids");
        done(call(
            &mut s,
            "register",
            vec![Value::from("1"), Value::Boolean(false)],
        ));
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
        let mut s = session("failed-store");
        done(call(
            &mut s,
            "register",
            vec![Value::from("1"), Value::Boolean(false)],
        ));
        let id = Value::from(9);
        let begin = vec![id.clone(), Value::Nil, Value::Nil, Value::Map(vec![])];
        done(call(&mut s, "tpc_begin", begin));
        // An empty pickle is refused by the core.
        let store = vec![p64(1), p64(0), Value::Binary(vec![]), id.clone()];
        done(call(&mut s, "storea", store));
        let fault = failed(call(&mut s, "vote", vec![id.clone()]));
        assert_eq!(fault.class, "ZODB.POSException.StorageTransactionError");
        assert!(s.open.is_none());
        let fault = failed(call(&mut s, "tpc_finish", vec![id]));
        assert_eq!(fault.class, "ZODB.POSException.StorageTransactionError");
    }

    #[test]
    fn a_stale_serial_is_a_conflict_error_with_the_serials() {
        let mut s = session("conflict");
        done(call(
            &mut s,
            "register",
            vec![Value::from("1"), Value::Boolean(false)],
        ));
        let commit = |s: &mut Session, id: i64, serial: u64| {
            let id = Value::from(id);
            done(call(
                s,
                "tpc_begin",
                vec![id.clone(), Value::Nil, Value::Nil, Value::Nil],
            ));
            let data = Value::Binary(b"x".to_vec());
            done(call(
                s,
                "storea",
                vec![p64(0), p64(serial), data, id.clone()],
            ));
            match call(s, "vote", vec![id.clone()]) {
                Outcome::Done(_) => Ok(u64_of(&done(call(s, "tpc_finish", vec![id]))).unwrap()),
                outcome => Err(failed(outcome)),
            }
        };
        let first = commit(&mut s, 1, 0).unwrap();
        let fault = commit(&mut s, 2, 0).unwrap_err();
        assert_eq!(fault.class, "ZODB.POSException.ConflictError");
        let Value::Map(items) = fault.args else {
            panic!("the args of a conflict are a map");
        };
        let serials = items
            .iter()
            .find(|(key, _)| key.as_str() == Some("serials"))
            .map(|(_, value)| value.clone())
            .unwrap();
        assert_eq!(serials, Value::Array(vec![p64(first), p64(0)]));
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
        let mut other = Session::new(Arc::clone(&storage));
        // A session that does not register is not a connection.
        let _unregistered = Session::new(Arc::clone(&storage));
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
    }
}
