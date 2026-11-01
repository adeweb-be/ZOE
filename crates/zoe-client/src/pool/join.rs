//! The join of a load to a load in flight of the same oid at the same tid.
//!
//! A Connection touches the first children of a record right after its
//! unpickle, often before the replies of their prefetch come back. So
//! `Shared` keeps the loads in flight, keyed by oid and tid, as
//! `load_before` of the client of ZEO does. A load of a key in flight
//! waits for the reply of that load and sends nothing. Else it sends the
//! request and records the key.
//!
//! The caller that sends the request runs the load on its own task, so
//! the call frame carries the traceparent of that caller. When the caller
//! drops its future before the reply, the load moves to a task of the
//! runtime, so the other waiters still get the reply. The key goes when
//! the reply is in the cache, and each waiter gets the reply at that time.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::io;
use std::sync::Arc;

use tokio::sync::oneshot;
use zoe_core::{Oid, Tid};

use super::Shared;
use crate::client::Record;
use crate::runtime::BoxFuture;
use crate::{Error, lock};

/// The result of a load: the record or none, and the position of the
/// connection when the reply came.
pub(super) type Loaded = Result<(Option<Record>, Tid), Error>;

/// The loads in flight: for each oid and tid, the waiters of its reply.
pub(super) type Flights = HashMap<(Oid, Tid), Vec<oneshot::Sender<Loaded>>>;

impl Shared {
    /// Load as `Pool::load_before` does, without the prefetch of the
    /// references of the record. When a load of `oid` at `tid` is in
    /// flight, wait for its reply, and send nothing.
    pub(super) async fn load_before(self: &Arc<Self>, oid: Oid, tid: Tid) -> Loaded {
        let key = (oid, tid);
        let waiter = {
            let mut flights = lock(&self.flights);
            match flights.entry(key) {
                Entry::Occupied(mut waiters) => {
                    let (sender, receiver) = oneshot::channel();
                    waiters.get_mut().push(sender);
                    Some(receiver)
                }
                Entry::Vacant(slot) => {
                    slot.insert(Vec::new());
                    None
                }
            }
        };
        if let Some(receiver) = waiter {
            // A load that ends without a reply drops the sender.
            return receiver.await.unwrap_or(Err(Error::Disconnected));
        }
        let shared = self.clone();
        let flight = Flight {
            shared: self.clone(),
            key,
            load: Some(Box::pin(async move { shared.fetch(oid, tid).await })),
            detached: false,
        };
        flight.run().await
    }

    /// Load the record of `oid` before `tid` from the server, through any
    /// connection, and store it in the cache with the position of that
    /// connection.
    async fn fetch(&self, oid: Oid, tid: Tid) -> Loaded {
        let (record, position) = self.any_connection()?.load_before_at(oid, tid).await?;
        if let (Some(cache), Some(record)) = (&self.cache, &record) {
            cache.store(oid, record.tid, record.end, &record.data, position);
        }
        Ok((record, position))
    }

    /// Remove `key` from the loads in flight, give a copy of `loaded` to
    /// each of its waiters, and return `loaded`.
    fn land(&self, key: (Oid, Tid), loaded: Loaded) -> Loaded {
        let waiters = lock(&self.flights).remove(&key).unwrap_or_default();
        for waiter in waiters {
            // A waiter that dropped its future gets nothing.
            if !waiter.is_closed() {
                let _ = waiter.send(copy(&loaded));
            }
        }
        loaded
    }
}

/// A load in flight that a caller or a task of the runtime runs.
struct Flight {
    shared: Arc<Shared>,
    key: (Oid, Tid),
    /// The load, until it ends.
    load: Option<BoxFuture<Loaded>>,
    /// The flight runs on a task of the runtime.
    detached: bool,
}

impl Flight {
    /// Run the load until it ends, give its result to the waiters, and
    /// return it.
    async fn run(mut self) -> Loaded {
        let load = self.load.as_mut().expect("a flight holds its load");
        let loaded = load.await;
        self.load = None;
        self.shared.land(self.key, loaded)
    }
}

impl Drop for Flight {
    /// A caller that drops its future before the end of the load moves
    /// the load to a task of the runtime. A task that drops it, as at the
    /// stop of the runtime, or a panic ends the load without a reply: each
    /// waiter then gets `Error::Disconnected`.
    fn drop(&mut self) {
        let Some(load) = self.load.take() else {
            return;
        };
        if self.detached || std::thread::panicking() {
            lock(&self.shared.flights).remove(&self.key);
            return;
        }
        let flight = Flight {
            shared: self.shared.clone(),
            key: self.key,
            load: Some(load),
            detached: true,
        };
        self.shared.runtime.spawn(Box::pin(async move {
            let _ = flight.run().await;
        }));
    }
}

/// Return a copy of `loaded` for a waiter.
fn copy(loaded: &Loaded) -> Loaded {
    match loaded {
        Ok((record, position)) => Ok((record.clone(), *position)),
        Err(error) => Err(copy_error(error)),
    }
}

/// Return a copy of `error`. An `Error::Io` keeps its kind and its text.
fn copy_error(error: &Error) -> Error {
    match error {
        Error::Server {
            class,
            message,
            value,
        } => Error::Server {
            class: class.clone(),
            message: message.clone(),
            value: value.clone(),
        },
        Error::Conflict {
            oid,
            serials,
            class_name,
        } => Error::Conflict {
            oid: *oid,
            serials: *serials,
            class_name: class_name.clone(),
        },
        Error::ReadConflict { oid, serials } => Error::ReadConflict {
            oid: *oid,
            serials: *serials,
        },
        Error::Unsupported(text) => Error::Unsupported(text.clone()),
        Error::Disconnected => Error::Disconnected,
        Error::Protocol(text) => Error::Protocol(text.clone()),
        Error::OtherProtocol(name) => Error::OtherProtocol(name.clone()),
        Error::Invalid(text) => Error::Invalid(text.clone()),
        Error::Lagged(missed) => Error::Lagged(*missed),
        Error::Io(error) => Error::Io(io::Error::new(error.kind(), error.to_string())),
    }
}
