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
//!
//! `fault` holds the exceptions for the client, `session` the calls that
//! run on many threads, `transaction` the calls of the transaction,
//! `vote` the vote, and `args` the readers of the arguments of a call.

mod args;
mod fault;
// The module holds `Session`, the type that gives this module its name.
#[allow(clippy::module_inception)]
mod session;
#[cfg(test)]
mod tests;
mod transaction;
mod vote;

use std::sync::Arc;

use rmpv::Value;

use super::message::{self, Call};
use super::method::{self, Op};
use super::outbox::{Outbox, Room};
use super::{NamedStorage, Storages};
use args::Args;

pub(crate) use fault::Fault;
#[cfg(test)]
pub(crate) use session::LOADED;
pub(crate) use session::Session;
pub(crate) use transaction::{INTENT_HOLD, Transaction};

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
    fn keep(&mut self, parts: Option<super::outbox::Parts>) -> Option<super::outbox::Parts> {
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

/// Return the storage that `call` registers with, before the connection
/// has one. Only `register` can come first, and it must name one of
/// `storages`. Otherwise, return the outcome of the call: a call before
/// `register` closes the connection, and an unknown name fails the call,
/// as in ZEO.
pub(crate) fn select(storages: &Storages, call: &Call) -> Result<Arc<NamedStorage>, Outcome> {
    if method::find(&call.method).map(|m| m.op) != Some(Op::Register) {
        return Err(Outcome::Close(format!("{} before register", call.method)));
    }
    let (name, _, _) = args::register(Args::of(call)).map_err(Outcome::Failed)?;
    let name = args::storage_name(name);
    storages
        .get(&name)
        .cloned()
        .ok_or_else(|| Outcome::Failed(Fault::value_error(format!("unknown storage: {name}"))))
}
