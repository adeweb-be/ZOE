//! `Session`: the state of a connection that all its calls share, and the
//! calls that are not calls of the transaction. These calls run on many
//! threads at once.

use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use rmpv::Value;
use zoe_core::{Finished, LockCancel, Oid, Store, Tid, TxnMeta, ZoeError, undo_id};
use zoe_wire::extension::{SERVER_WRITES, TRACE_CONTEXT};
use zoe_wire::p64;

use super::args::{
    self, Args, bin_of, flavour_of, optional_bin, optional_tid, storage_name, str_of, truth, u64_of,
};
use super::{Fault, Outcome, Reply};
use crate::protocol::blob::Download;
use crate::protocol::clock;
use crate::protocol::extension;
use crate::protocol::invalidation::{ClientId, Commit, Others, Publication};
use crate::protocol::iteration::{Iterators, Refused};
use crate::protocol::message::{self, Call};
use crate::protocol::method::{self, Op};
use crate::protocol::negotiation;
use crate::protocol::outbox::{Outbox, Parts};
use crate::protocol::status::Stats;
use crate::protocol::{NamedStorage, Stopping};

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

/// The most transactions that one read of the undo log takes.
const UNDO_LOG_CHUNK: usize = 256;

/// The interfaces that the store provides, as `(module, name)`, in the
/// order of `providedBy(zoe.Storage(path)).__iro__`. A stock client takes
/// the storage interfaces of this list as its own. The store provides
/// `IExternalGC`, because `deleteObject` writes a deletion, and
/// `IStorageUndoable`.
pub(super) const INTERFACES: [(&str, &str); 9] = [
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
    pub(super) storage: Arc<NamedStorage>,
    /// The store of `storage`.
    pub(super) store: Arc<Store>,
    /// The frames that wait for the client.
    pub(super) outbox: Outbox,
    /// The registration of the connection, from `register` on.
    registration: OnceLock<Registration>,
    /// The iterators of the connection. One call at a time uses them.
    pub(super) iterators: Mutex<Iterators>,
    /// The label that the client gave with `set_client_label`, for the log
    /// lines of the calls that start after it.
    pub(super) label: Mutex<Option<String>>,
    /// The stop of the server. A pack stops between two batches when the
    /// server stops.
    pub(super) stopping: Stopping,
    /// The end of the connection ends the lock wait of each transaction of
    /// the session, also a wait that starts after the end.
    pub(super) lock_cancel: LockCancel,
    /// True when the client may register read-only only, as the holder of
    /// a certificate of the read-only client CA.
    pub(super) read_only_client: bool,
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
            .is_some_and(|accepted| accepted.contains(&TRACE_CONTEXT))
    }

    /// Tell if the server accepted `server_writes` from the declaration of
    /// the client. Only then can the vote of the connection write a record
    /// that its transaction did not store (ADR 0049).
    pub(crate) fn server_writes(&self) -> bool {
        self.registration
            .get()
            .and_then(|r| r.extensions.as_deref())
            .is_some_and(|accepted| accepted.contains(&SERVER_WRITES))
    }

    /// Tell if the client registered read-only.
    pub(super) fn read_only(&self) -> bool {
        self.registration.get().is_some_and(|r| r.read_only)
    }

    /// Return the error of a call to a method of an extension that the
    /// connection did not declare. The connection stays open.
    pub(super) fn undeclared(&self, name: &str) -> Option<Fault> {
        let accepted = self
            .registration
            .get()
            .and_then(|r| r.extensions.as_deref());
        negotiation::refused(name, accepted, method::METHODS).map(Fault::value_error)
    }

    /// The id of the connection in the invalidations of the storage.
    pub(super) fn client(&self) -> Option<ClientId> {
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
    ///
    /// The caller puts the reply after this count. A `loadBefore` puts its
    /// reply itself, and counts itself before it (`Session::loaded`). So a
    /// client that has a reply reads metrics that count its call.
    pub(crate) fn call(&self, call: &Call, reply: &mut Reply) -> Outcome {
        let op = method::find(&call.method).map(|method| method.op);
        let outcome = self.dispatch(call, op, reply);
        self.count(op, &outcome);
        outcome
    }

    /// Count a call that ran, as ZEO counts it.
    fn count(&self, op: Option<Op>, outcome: &Outcome) {
        if op == Some(Op::Register) && matches!(outcome, Outcome::Done(_)) {
            Stats::add(&self.storage.stats.connections);
        }
    }

    /// Count one `loadBefore` that ran for `elapsed`, in the status and the
    /// metrics of the storage, as ZEO counts it. The load calls this once,
    /// before its reply goes into the queue.
    fn loaded(&self, elapsed: Duration) {
        Stats::add(&self.storage.stats.loads);
        let metrics = &self.storage.metrics;
        metrics.load_latency.observe_duration(elapsed);
    }

    /// Run one call, whose method has the arm `op` of the dispatch.
    fn dispatch(&self, call: &Call, op: Option<Op>, reply: &mut Reply) -> Outcome {
        let args = Args::of(call);
        if !self.is_registered() {
            return match op {
                Some(Op::Register) => self.register(args).into(),
                _ => Outcome::Close(format!("{} before register", call.method)),
            };
        }
        if let Some(fault) = self.undeclared(&call.method) {
            return Outcome::Failed(fault);
        }
        let Some(op) = op else {
            return Outcome::Close(format!("unknown method {}", call.method));
        };
        let result = match op {
            Op::Register => Err(Fault::value_error("duplicate register() call")),
            Op::LastTransaction => Ok(p64(self.storage.invalidations.published().0)),
            Op::GetInfo => args.exactly::<0>().and_then(|_| self.info()),
            Op::GetSizeInfo => args
                .exactly::<0>()
                .and_then(|_| self.sizes())
                .map(|items| Value::Map(items.into())),
            Op::ServerStatus => args
                .exactly::<0>()
                .map(|_| self.storage.status().to_value()),
            Op::Ping => args.exactly::<0>().map(|_| Value::Nil),
            // ZEO puts the label in its log lines, and so does the server.
            Op::SetClientLabel => args.exactly::<1>().map(|[label]| {
                // An empty label gives the address of the peer back.
                *self.label.lock().unwrap_or_else(PoisonError::into_inner) =
                    Some(client_label(label)).filter(|label| !label.trim().is_empty());
                Value::Nil
            }),
            Op::GetTid => self.get_tid(args),
            Op::NewOids => self.new_oids(args),
            Op::LoadBefore => return self.load_before(call, reply),
            Op::LoadSerial => self.load_serial(args),
            Op::GetInvalidations => self.get_invalidations(args),
            Op::History => self.history(args),
            Op::UndoLog => self.undo_log(args),
            Op::UndoInfo => self.undo_info(args),
            Op::RecordIternext => self.record_iternext(args),
            Op::IteratorStart => self.iterator_start(args),
            Op::IteratorNext => self.iterator_next(args),
            Op::IteratorRecordStart => self.iterator_record_start(args),
            Op::IteratorRecordNext => self.iterator_record_next(args),
            Op::IteratorGc => self.iterator_gc(args),
            Op::Pack => self.pack(args),
            Op::SendBlob => return self.send_blob(args, reply),
            Op::SequenceNext => self.sequence_next(args),
            Op::KvGet => self.kv_get(args),
            Op::KvScan => self.kv_scan(args),
            Op::KvGeneration => self.kv_generation(args),
            // The calls of the transaction run in `Transaction::dispatch`.
            _ => return Outcome::Close(format!("unknown method {}", call.method)),
        };
        result.into()
    }

    /// Return the value of a key of a namespace, or nil: `kv_get(flavour,
    /// name, key)`. The read gives the last committed value, and an expired
    /// key reads as nil. It runs outside the transaction of the connection.
    fn kv_get(&self, args: Args<'_>) -> Result<Value, Fault> {
        let [flavour, name, key] = args.exactly::<3>()?;
        let (flavour, name, key) = (
            flavour_of(flavour)?,
            str_of(name, "name of a namespace")?,
            bin_of(key, "key")?,
        );
        let value = self.store.kv_get(flavour, name, key)?;
        Ok(value.map_or(Value::Nil, Value::Binary))
    }

    /// Return the pairs of a namespace in the byte order of the keys:
    /// `kv_scan(flavour, name, start, stop, limit)`. `start` is included and
    /// `stop` excluded, and nil is an open end. The reply holds at most
    /// `limit` pairs, and at most 1,000, as `[[key, value], ...]`.
    fn kv_scan(&self, args: Args<'_>) -> Result<Value, Fault> {
        let [flavour, name, start, stop, limit] = args.exactly::<5>()?;
        let flavour = flavour_of(flavour)?;
        let name = str_of(name, "name of a namespace")?;
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
    fn kv_generation(&self, args: Args<'_>) -> Result<Value, Fault> {
        let [name] = args.exactly::<1>()?;
        Ok(p64(self
            .store
            .kv_generation(str_of(name, "name of a namespace")?)?
            .0))
    }

    /// Return the next value of the sequence `name` of the store (F29),
    /// for a connection that declared the extension `sequences`. The value
    /// is not part of a transaction: an abort does not give it back. A
    /// read-only connection fails with `ReadOnlyError`, as `zoe.Storage`
    /// does. Every 100 values of a name, the call waits for one write of
    /// the backend.
    fn sequence_next(&self, args: Args<'_>) -> Result<Value, Fault> {
        let [name] = args.exactly::<1>()?;
        let name = str_of(name, "name of a sequence")?;
        if self.read_only() {
            return Err(Fault::read_only());
        }
        Ok(Value::from(self.store.sequence(name).next()?))
    }

    /// Register the connection with its storage. A ZOE client declares the
    /// extensions that it wants in a third argument. The server keeps the
    /// names that it serves and leaves out the other names.
    fn register(&self, args: Args<'_>) -> Result<Value, Fault> {
        let (storage, read_only, declaration) = args::register(args)?;
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
    ///
    /// The call counts itself before its reply goes into the queue: with
    /// the put, under the mutex of the hub. A put that fails counts
    /// nothing, so the call counts once.
    fn load_before(&self, call: &Call, reply: &mut Reply) -> Outcome {
        let started = Instant::now();
        let parsed = Args::of(call)
            .exactly::<2>()
            .and_then(|[oid, tid]| Ok((Oid(u64_of(oid)?), Tid(u64_of(tid)?))));
        let (oid, tid) = match parsed {
            Ok(parsed) => parsed,
            Err(fault) => {
                // The caller puts the reply of the fault after this count.
                self.loaded(started.elapsed());
                return Outcome::Failed(fault);
            }
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
            // Read the clock outside the mutex of the hub.
            let elapsed = started.elapsed();
            match invalidations.put_since(seen, room, frame, || self.loaded(elapsed)) {
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
            self.loaded(started.elapsed());
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
    fn send_blob(&self, args: Args<'_>, reply: &mut Reply) -> Outcome {
        let download = args.exactly::<2>().and_then(|[oid, serial]| {
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
    /// count of the store, as `len(zoe.Storage)` gives it. A store without
    /// a count has the length 0. The size is the size of the files in the
    /// store directory.
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

    fn new_oids(&self, args: Args<'_>) -> Result<Value, Fault> {
        let n = match args.values {
            [] => MAX_NEW_OIDS,
            [n] => n
                .as_i64()
                .ok_or_else(|| Fault::type_error("n is not an int"))?
                .clamp(1, 100) as u64,
            _ => return Err(args.takes("at most 1 argument")),
        };
        if self.read_only() {
            return Err(Fault::read_only());
        }
        let oids = (0..n)
            .map(|_| self.store.new_oid().map(|oid| p64(oid.0)))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Value::Array(oids))
    }

    fn load_serial(&self, args: Args<'_>) -> Result<Value, Fault> {
        let [oid, serial] = args.exactly::<2>()?;
        let (oid, serial) = (Oid(u64_of(oid)?), Tid(u64_of(serial)?));
        Ok(Value::Binary(self.store.load_serial(oid, serial)?))
    }

    /// Return at most `size` records of an oid, newest first, as the maps
    /// of FileStorage's `history`. A map has no keys of the transaction
    /// extension.
    ///
    /// An oid that is not 8 bytes names no object: the call fails with
    /// `POSKeyError` before it checks the size, as ZEO's server does.
    fn history(&self, args: Args<'_>) -> Result<Value, Fault> {
        let (oid, size) = match args.values {
            [oid] => (oid, None),
            [oid, size] => (oid, Some(size)),
            _ => return Err(args.takes("1 or 2 arguments")),
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
    fn undo_log(&self, args: Args<'_>) -> Result<Value, Fault> {
        let (first, last) = match args.values {
            [] => (0, -20),
            [first] => (int_of(first)?, -20),
            [first, last] => (int_of(first)?, int_of(last)?),
            _ => return Err(args.takes("0 to 2 arguments")),
        };
        self.undo_entries(first, last, &[])
    }

    /// Return the transactions of `undoLog(first, last)` whose map holds
    /// each key of the map `specification` with its value, as `undoInfo`
    /// of ZODB does.
    fn undo_info(&self, args: Args<'_>) -> Result<Value, Fault> {
        let (first, last, specification) = match args.values {
            [] => (0, -20, None),
            [first] => (int_of(first)?, -20, None),
            [first, last] => (int_of(first)?, int_of(last)?, None),
            [first, last, specification] => (int_of(first)?, int_of(last)?, Some(specification)),
            _ => return Err(args.takes("0 to 3 arguments")),
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
    fn record_iternext(&self, args: Args<'_>) -> Result<Value, Fault> {
        let next = match args.values {
            [] | [Value::Nil] => None,
            [next] => Some(Oid(u64_of(next)?)),
            _ => return Err(args.takes("1 argument")),
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

    /// Return the publication of a commit of this connection. Dropping it
    /// publishes the commit. Every other client gets its invalidation, with
    /// an empty list for a commit that wrote no oid, as from ZEO. This
    /// client gets `info`, then `reply`.
    ///
    /// The oids come from the transaction index of the store. When they do
    /// not read, the hub closes the other connections, so each client
    /// verifies its cache when it connects again. The hub publishes the
    /// commit also when a read panics, so later commits never wait for
    /// this one.
    pub(super) fn publication(
        &self,
        finished: Finished,
        reply: Option<Vec<u8>>,
    ) -> Publication<'_> {
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
        publication
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
    fn get_tid(&self, args: Args<'_>) -> Result<Value, Fault> {
        let [oid] = args.exactly::<1>()?;
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
    fn get_invalidations(&self, args: Args<'_>) -> Result<Value, Fault> {
        let [tid] = args.exactly::<1>()?;
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
    fn iterator_start(&self, args: Args<'_>) -> Result<Value, Fault> {
        let [start, stop] = args.exactly::<2>()?;
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
    fn iterator_next(&self, args: Args<'_>) -> Result<Value, Fault> {
        let [id] = args.exactly::<1>()?;
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
    fn iterator_record_start(&self, args: Args<'_>) -> Result<Value, Fault> {
        let [id, tid] = args.exactly::<2>()?;
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
    fn iterator_record_next(&self, args: Args<'_>) -> Result<Value, Fault> {
        let [id] = args.exactly::<1>()?;
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
    fn iterator_gc(&self, args: Args<'_>) -> Result<Value, Fault> {
        let [ids] = args.exactly::<1>()?;
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
    fn pack(&self, args: Args<'_>) -> Result<Value, Fault> {
        // A pack deletes data. A read-only client may not pack. A
        // connection that registered read-only by its own choice packs, as
        // `zeopack` needs.
        if self.read_only_client {
            return Err(Fault::read_only());
        }
        let time = match args.values {
            [time] | [time, _] => time,
            _ => return Err(args.takes("1 or 2 arguments")),
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

/// Return the map of the undo log of the transaction row `row`: the keys
/// `id`, `time`, `user_name` and `description` of FileStorage, then the
/// keys of the extension, which replace a key of the same name, as in
/// FileStorage. An extension that gives no msgpack map adds no key.
pub(super) fn undo_entry(row: &TxnMeta) -> Value {
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
pub(super) fn python_eq(a: &Value, b: &Value) -> bool {
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
pub(super) fn int_of(value: &Value) -> Result<i64, Fault> {
    value
        .as_i64()
        .ok_or_else(|| Fault::type_error("an index of the undo log is not an int"))
}

/// Return the label of a client for the log lines: its text, with each
/// control character replaced by a space, and at most `MAX_LABEL`
/// characters.
pub(super) fn client_label(value: &Value) -> String {
    let text = storage_name(value);
    text.chars()
        .take(MAX_LABEL)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}
