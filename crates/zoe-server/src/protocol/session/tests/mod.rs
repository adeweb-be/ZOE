//! The tests of the calls of a session, by topic, and the helpers that
//! they share.

mod blob;
mod commit;
mod compression;
mod conflict;
mod invalidation;
mod iteration;
mod kv;
mod pack;
mod register;
mod undo;

use std::collections::HashSet;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use rmpv::Value;
use tokio::sync::mpsc::Receiver;
use zoe_core::{Compression, Oid, Options, Store, Tid, Txn, ZoeError, undo_id};
use zoe_wire::class::{
    CONFLICT_ERROR, READ_CONFLICT_ERROR, STORAGE_TRANSACTION_ERROR as TRANSACTION_ERROR,
};
use zoe_wire::p64;

use super::args::*;
use super::fault::{CONFLICT, READ_CONFLICT};
use super::session::*;
use super::transaction::*;
use super::*;
use crate::protocol::clock;
use crate::protocol::invalidation::{Commit, Others};
use crate::protocol::method::{self, Lane};
use crate::protocol::outbox::{Inbox, Outbox};
use crate::protocol::testing::never_stops;
use crate::protocol::{NamedStorage, Queues, ResolutionLog, Stopping, Storages};

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
        if self.session.is_registered() && method::lane(&call.method) == Lane::Transaction {
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
    use crate::protocol::pipeline::Calls;
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

fn registered(name: &str) -> Client {
    let mut s = session(name);
    done(call(
        &mut s,
        "register",
        vec![Value::from("1"), Value::Boolean(false)],
    ));
    s
}

fn count(status: &Value, key: &str) -> u64 {
    get(status, key).as_u64().unwrap()
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

fn get_counter(counter: &AtomicU64) -> u64 {
    counter.load(std::sync::atomic::Ordering::Relaxed)
}

/// The keys of the map `map`, in order.
fn keys(map: &Value) -> Vec<&str> {
    map.as_map()
        .unwrap()
        .iter()
        .map(|(key, _)| key.as_str().unwrap())
        .collect()
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
