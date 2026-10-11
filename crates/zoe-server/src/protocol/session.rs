//! The calls of one connection, run in order on one thread.
//!
//! The session holds the open transaction of the connection. Its thread
//! begins the core transaction and makes every later call of it, so the
//! core never refuses a call with `WrongThread`.

use std::sync::{Arc, mpsc};

use rmpv::Value;
use tokio::sync::mpsc::UnboundedSender;
use zoe_core::{Oid, Store, Tid, Txn, ZoeError};

use super::extension;
use super::message::{self, Call};

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
    store: Arc<Store>,
    name: String,
    registered: bool,
    read_only: bool,
    open: Option<Open>,
}

/// Run the calls that arrive on `calls` in order, and send each reply to
/// `replies`. Return when `calls` closes, or when the session closes the
/// connection. A transaction still open aborts.
pub(crate) fn run(
    store: Arc<Store>,
    name: String,
    calls: mpsc::Receiver<Call>,
    replies: UnboundedSender<Vec<u8>>,
) {
    let mut session = Session::new(store, name);
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
    pub(crate) fn new(store: Arc<Store>, name: String) -> Self {
        Self {
            store,
            name,
            registered: false,
            read_only: false,
            open: None,
        }
    }

    /// Run one call.
    fn call(&mut self, call: &Call) -> Outcome {
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
            "get_info" => Ok(self.info()),
            "new_oids" => self.new_oids(args),
            "loadBefore" => self.load_before(args),
            "loadSerial" => self.load_serial(args),
            "record_iternext" => self.record_iternext(args),
            "tpc_begin" => self.tpc_begin(args),
            "storea" => self.storea(args),
            "checkCurrentSerialInTransaction" => self.check_current_serial(args),
            "deleteObject" => self.delete_object(args),
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
        if storage != self.name {
            return Err(Fault::value_error(format!("unknown storage: {storage}")));
        }
        self.read_only = truth(read_only);
        self.registered = true;
        Ok(p64(self.store.last_transaction().0))
    }

    /// The information that a stock client asks for after `register`.
    fn info(&self) -> Value {
        Value::Map(vec![
            (Value::from("length"), Value::from(0)),
            (Value::from("size"), Value::from(0)),
            (Value::from("name"), Value::from(self.name.as_str())),
            (Value::from("supportsUndo"), Value::Boolean(false)),
            (
                Value::from("supports_record_iternext"),
                Value::Boolean(false),
            ),
            (Value::from("interfaces"), Value::Array(vec![])),
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

    /// Return the pickle of `oid` at `serial`. A deletion, or no record,
    /// fails with `POSKeyError`.
    fn load_serial(&self, args: &[Value]) -> Result<Value, Fault> {
        let [oid, serial] = arity::<2>(args, "loadSerial")?;
        let (oid, serial) = (Oid(u64_of(oid)?), Tid(u64_of(serial)?));
        Ok(Value::Binary(self.store.load_serial(oid, serial)?))
    }

    /// Return the current record of the first oid at or after the oid of
    /// the call, or of the first oid for nil: `[oid, tid, data, next oid
    /// or nil]`. A deleted oid has no current record. With no such oid,
    /// fail with `ValueError`, as FileStorage does.
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

    /// A pickle of a `persistent.mapping.PersistentMapping` record, as ZODB
    /// writes it: the class pickle, then the state pickle.
    const MAPPING: &[u8] = b"\x80\x03X\x12\x00\x00\x00persistent.mappingq\x00\
        X\x11\x00\x00\x00PersistentMappingq\x01\x86q\x02N\x86q\x03.\x80\x03}q\x00.";

    fn session(name: &str) -> Session {
        let dir = std::env::temp_dir().join(format!("zoe-session-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Session::new(Arc::new(Store::open(&dir).unwrap()), "1".into())
    }

    /// A session of a client that registered, read-only or not.
    fn registered(name: &str, read_only: bool) -> Session {
        let mut s = session(name);
        done(call(
            &mut s,
            "register",
            vec![Value::from("1"), Value::Boolean(read_only)],
        ));
        s
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

    /// Commit `data` for `oid` in a new transaction.
    fn commit(s: &mut Session, id: i64, oid: u64, serial: u64, data: &[u8]) -> Result<u64, Fault> {
        begin(s, id, None);
        store(s, id, oid, serial, data);
        vote_and_finish(s, id)
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
        let mut s = registered("read-only", true);
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
        let mut s = registered("new-oids", false);
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
        let mut s = registered("failed-store", false);
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
        let mut s = registered("begin-errors", false);
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
        let mut s = registered("other-transaction", false);
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
        let mut s = registered("conflict", false);
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
        let mut s = registered("read-conflict", false);
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
        let mut s = registered("delete", false);
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
        let mut s = registered("restore", false);
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
