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
        Self::new("ZODB.POSException.StorageTransactionError", text)
    }

    fn read_only() -> Self {
        Self {
            class: "ZODB.POSException.ReadOnlyError",
            args: Value::Array(vec![]),
        }
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
        Session::new(Arc::new(Store::open(&dir).unwrap()), "1".into())
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
