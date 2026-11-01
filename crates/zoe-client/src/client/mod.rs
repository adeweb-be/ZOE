//! A client of one storage of a ZEO 6 server: the open of its connection,
//! its calls and the begin of its transactions.
//!
//! `connection` holds the connection: its calls, its replies, the calls of
//! the server and its three tasks. `transaction` holds `Transaction`, an
//! open transaction of a client.

mod connection;
mod transaction;

use std::any::Any;
use std::future::{Future, poll_fn};
use std::io::{self, Write};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::task::Poll;
use std::time::Duration;

use rmpv::Value;
use tokio::sync::{broadcast, mpsc};
use zoe_core::{Flavour, Oid, Tid};
use zoe_wire::extension::{KV, LOCK_INTENT, SEQUENCES, TRACE_CONTEXT};
use zoe_wire::{PROTOCOL, p64, u64_of};

use crate::endpoint::{Address, Endpoint};
use crate::protocol::{self, get};
use crate::runtime::{Runtime, Stream, Tokio};
use crate::{Error, lock};
pub use connection::traced;
use connection::{Inner, closed};
pub use transaction::{BLOB_CHUNK, Listed, Transaction, Voted};

/// The time between two heartbeats, as `ZEO.ClientStorage` sends them.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(60);

/// The most oids that one `new_oids` call of the protocol gives.
const OIDS_PER_CALL: usize = 100;

/// A client of one storage of a ZEO 6 server.
///
/// Its methods take `&self`, so many calls of one client can wait for
/// their replies at once. The server answers the calls of one connection
/// in order, but each reply goes to its call by its message id, whatever
/// the order.
pub struct Client {
    inner: Arc<Inner>,
    /// Each task of the connection holds a sender. `close` waits until
    /// every sender is gone.
    tasks: mpsc::Receiver<()>,
    /// The reply of `get_info` at `Client::open`, or nil.
    info: Value,
    /// The extensions that the server accepted at `Client::open`.
    extensions: Vec<String>,
    /// `true` when the client registered read-only.
    read_only: bool,
    /// The address of the server.
    address: Address,
}

/// One attempt of `Client::open_first` to open a client.
type Attempt<'a> = Pin<Box<dyn Future<Output = Result<Client, Error>> + Send + 'a>>;

/// What `Client::open` declares to the server in `register`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Options {
    /// Register read-only. The server then refuses each write of the
    /// connection with `ReadOnlyError`.
    pub read_only: bool,
    /// When `read_only` is false and the server refuses the register with
    /// `ReadOnlyError`, register again read-only on the same connection,
    /// as the `read_only_fallback` of `ZEO.ClientStorage`.
    pub read_only_fallback: bool,
    /// The extensions that the client declares in a third argument of
    /// `register`. With none, `register` has two arguments.
    pub extensions: Vec<String>,
    /// The label of the connection, as the `client_label` of
    /// `ZEO.ClientStorage`. `Client::open` sends it in `set_client_label`
    /// after `get_info`, and the server puts it in its log lines.
    pub client_label: Option<String>,
}

/// One version of an object, as `loadBefore` gives it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    /// The pickle of the record.
    pub data: Vec<u8>,
    /// The tid that wrote the record: its serial.
    pub tid: Tid,
    /// The tid of the next record of the object, or `None` when this
    /// record is current.
    pub end: Option<Tid>,
}

/// One entry of the history of an object, newest first.
#[derive(Clone, Debug, PartialEq)]
pub struct HistoryEntry {
    /// The tid of the transaction that wrote the record.
    pub tid: Tid,
    /// The time of the transaction, in seconds since the epoch.
    pub time: f64,
    /// The user of the transaction.
    pub user_name: Vec<u8>,
    /// The description of the transaction.
    pub description: Vec<u8>,
    /// The size of the pickle, in bytes.
    pub size: u64,
}

/// An invalidation: the oids that a transaction of another client wrote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invalidation {
    /// The tid of the transaction.
    pub tid: Tid,
    /// The oids that the transaction wrote.
    pub oids: Vec<Oid>,
}

/// A receiver of the invalidations of a client.
pub struct Invalidations(broadcast::Receiver<Invalidation>);

impl Invalidations {
    /// Return the next invalidation that waits, or `None` when none waits.
    /// It does not wait. It fails as `recv` does.
    pub fn try_recv(&mut self) -> Result<Option<Invalidation>, Error> {
        match self.0.try_recv() {
            Ok(invalidation) => Ok(Some(invalidation)),
            Err(broadcast::error::TryRecvError::Empty) => Ok(None),
            Err(broadcast::error::TryRecvError::Closed) => Err(Error::Disconnected),
            Err(broadcast::error::TryRecvError::Lagged(missed)) => Err(Error::Lagged(missed)),
        }
    }

    /// Wait for the next invalidation. It fails with `Error::Lagged` when
    /// the receiver missed invalidations, and with `Error::Disconnected`
    /// after the last invalidation of a closed connection.
    pub async fn recv(&mut self) -> Result<Invalidation, Error> {
        match self.0.recv().await {
            Ok(invalidation) => Ok(invalidation),
            Err(broadcast::error::RecvError::Closed) => Err(Error::Disconnected),
            Err(broadcast::error::RecvError::Lagged(missed)) => Err(Error::Lagged(missed)),
        }
    }
}

impl Client {
    /// Connect to the ZEO 6 server at `addr` on the tokio runtime, and
    /// register with the storage `storage`, as `"1"`.
    pub async fn connect(addr: SocketAddr, storage: &str) -> Result<Client, Error> {
        Client::connect_with(Arc::new(Tokio), addr, storage).await
    }

    /// Connect to the ZEO 6 server at `addr` through `runtime`, and register
    /// with the storage `storage`.
    pub async fn connect_with(
        runtime: Arc<dyn Runtime>,
        addr: SocketAddr,
        storage: &str,
    ) -> Result<Client, Error> {
        let client = Client::handshake(runtime, &Endpoint::new(addr)).await?;
        let reply = client
            .inner
            .call("register", vec![storage.into(), false.into()])
            .await?;
        client.registered(reply).await?;
        Ok(client)
    }

    /// Connect to the ZEO 6 server at `addr` through `runtime`, register
    /// with the storage `storage` as `options` says, and call `get_info`.
    ///
    /// The client declares `options.extensions` in a third argument of
    /// `register`. A stock server takes two arguments only: it refuses the
    /// third with `TypeError` and keeps the connection open and
    /// unregistered. The client then registers again with two arguments.
    /// The extensions that the server accepted are the names of the key
    /// `extensions` of `get_info` that the client declared.
    pub async fn open(
        runtime: Arc<dyn Runtime>,
        addr: SocketAddr,
        storage: &str,
        options: &Options,
    ) -> Result<Client, Error> {
        Client::open_endpoint(runtime, &Endpoint::new(addr), storage, options).await
    }

    /// Connect to the ZEO 6 server of `endpoint` through `runtime`, and
    /// open the client as `Client::open` does.
    ///
    /// With `options.read_only_fallback`, a server that refuses a writable
    /// register with `ReadOnlyError` gets a second `register`, read-only,
    /// on the same connection. `Client::read_only` then tells it.
    pub async fn open_endpoint(
        runtime: Arc<dyn Runtime>,
        endpoint: &Endpoint,
        storage: &str,
        options: &Options,
    ) -> Result<Client, Error> {
        let mut client = Client::handshake(runtime, endpoint).await?;
        let mut declare = !options.extensions.is_empty();
        let mut read_only = options.read_only;
        let reply = loop {
            let mut args = vec![storage.into(), read_only.into()];
            if declare {
                let names = options.extensions.iter();
                args.push(Value::Array(
                    names.map(|name| name.as_str().into()).collect(),
                ));
            }
            match client.inner.call("register", args).await {
                Err(Error::Server { class, .. }) if declare && is_type_error(&class) => {
                    declare = false;
                }
                Err(Error::Server { class, .. })
                    if !read_only && options.read_only_fallback && is_read_only_error(&class) =>
                {
                    read_only = true;
                }
                reply => break reply?,
            }
        };
        client.read_only = read_only;
        client.registered(reply).await?;
        let info = client.inner.call("get_info", vec![]).await?;
        // ZEO sends the label without a reply, after `get_info`.
        if let Some(label) = &options.client_label {
            let args = vec![label.as_str().into()];
            client.inner.notify("set_client_label", args).await?;
        }
        client.extensions = accepted(&info, &options.extensions);
        client.info = info;
        let traces = client.extensions.iter().any(|name| name == TRACE_CONTEXT);
        client.inner.traces.store(traces, Ordering::SeqCst);
        Ok(client)
    }

    /// Open a client to one server of `endpoints`, as `ZEO.ClientStorage`
    /// does with a list of addresses.
    ///
    /// The client connects to all of them at once and keeps the first that
    /// opens as `options` asks. With `options.read_only_fallback`, a server
    /// that registers only read-only comes after a writable one: the client
    /// keeps the first read-only server only when no other server can open
    /// writable, or when `wait` passes. The client closes each other
    /// connection, so it holds one connection at the end.
    ///
    /// When no server opens, the open fails with the most specific error:
    /// the first error that is not of the connection, else the last error
    /// of a TLS handshake or of a bad stream (`InvalidData`), else the last
    /// error of a connection. When `wait` passes first, it fails with that
    /// error too, or with `Error::Io` of the kind `TimedOut` when no attempt
    /// failed yet. The attempts run in the task of the caller and through
    /// `runtime`, in the order of `endpoints`.
    pub async fn open_first(
        runtime: Arc<dyn Runtime>,
        endpoints: &[Endpoint],
        storage: &str,
        options: &Options,
        wait: Duration,
    ) -> Result<Client, Error> {
        if endpoints.is_empty() {
            return Err(Error::Invalid("no address to connect to".into()));
        }
        let mut attempts: Vec<Option<Attempt<'_>>> = endpoints
            .iter()
            .map(|endpoint| {
                let attempt = Client::open_endpoint(runtime.clone(), endpoint, storage, options);
                Some(Box::pin(attempt) as Attempt<'_>)
            })
            .collect();
        let mut timeout = runtime.sleep(wait);
        let mut fallback: Option<Client> = None;
        let mut failure: Option<Error> = None;
        loop {
            // The attempts come first, in their order, then the timeout.
            let next = poll_fn(|cx| {
                for slot in &mut attempts {
                    if let Some(attempt) = slot
                        && let Poll::Ready(result) = attempt.as_mut().poll(cx)
                    {
                        *slot = None;
                        return Poll::Ready(Some(result));
                    }
                }
                if attempts.iter().all(Option::is_none) {
                    return Poll::Ready(None);
                }
                timeout.as_mut().poll(cx).map(|()| None)
            })
            .await;
            match next {
                Some(Ok(client)) if client.read_only && !options.read_only => {
                    // A read-only fallback waits for a writable server.
                    if fallback.is_none() {
                        fallback = Some(client);
                    }
                }
                // The drop of the other attempts and of a fallback closes
                // their connections.
                Some(Ok(client)) => return Ok(client),
                Some(Err(error)) => {
                    if failure.as_ref().is_none_or(|kept| keeps(&error, kept)) {
                        failure = Some(error);
                    }
                }
                None => break,
            }
        }
        match (fallback, failure) {
            (Some(client), _) => Ok(client),
            (None, Some(error)) => Err(error),
            (None, None) => Err(Error::Io(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("no server opened within {wait:?}"),
            ))),
        }
    }

    /// Open the stream to `endpoint`, agree on the protocol and start the
    /// tasks of the connection.
    async fn handshake(runtime: Arc<dyn Runtime>, endpoint: &Endpoint) -> Result<Client, Error> {
        let mut stream = endpoint.connect(runtime.as_ref()).await?;
        match protocol::read_frame(&mut stream).await? {
            Some(theirs) if theirs == PROTOCOL => {}
            Some(theirs) => return Err(Error::OtherProtocol(theirs)),
            None => return Err(Error::Disconnected),
        }
        protocol::write_frame(&mut stream, PROTOCOL).await?;
        Ok(Client::start(runtime, stream, endpoint.address().clone()))
    }

    /// Take the last tid from the reply of `register`. A nil reply asks
    /// the server for it with `lastTransaction`.
    async fn registered(&self, reply: Value) -> Result<(), Error> {
        let last = match reply {
            Value::Nil => self.inner.call("lastTransaction", vec![]).await?,
            last => last,
        };
        let last = tid(&last, "register")?;
        self.inner.last_tid.fetch_max(last.0, Ordering::SeqCst);
        Ok(())
    }

    /// Start the tasks of the connection on `stream`.
    fn start(runtime: Arc<dyn Runtime>, stream: Box<dyn Stream>, address: Address) -> Client {
        let (inner, tasks) = connection::start(runtime, stream);
        Client {
            inner,
            tasks,
            info: Value::Nil,
            extensions: Vec::new(),
            read_only: false,
            address,
        }
    }

    /// Return the reply of `get_info` at `Client::open`: a map of the
    /// server, such as `length`, `size` and `supportsUndo`. A client of
    /// `connect` did not call `get_info`, and gets nil.
    pub fn info(&self) -> &Value {
        &self.info
    }

    /// Return the map of the `info` calls that the server sent on this
    /// connection, merged in their order, with the stamp of the last one.
    /// A higher stamp is a later call, also on another client of the
    /// process. Return `None` before the first one. A server sends `info`
    /// with the new sizes of the store after a commit and after a pack.
    pub fn pushed_info(&self) -> Option<(u64, Value)> {
        lock(&self.inner.pushed).clone()
    }

    /// Return the extensions that the server accepted at `Client::open`.
    pub fn extensions(&self) -> &[String] {
        &self.extensions
    }

    /// Return the address of the server.
    pub fn address(&self) -> &Address {
        &self.address
    }

    /// Tell if the client registered read-only: as `Options::read_only`
    /// asked, or after a read-only fallback.
    pub fn read_only(&self) -> bool {
        self.read_only
    }

    /// Tell if the connection is open: no call failed yet with
    /// `Error::Disconnected` because the connection closed.
    pub fn is_connected(&self) -> bool {
        !lock(&self.inner.calls).closed
    }

    /// Wait until the connection closes.
    pub async fn closed(&self) {
        closed(&mut self.inner.closing.subscribe()).await;
    }

    /// Return the last tid that the client knows: the tid of `register`,
    /// of the last invalidation, or of the last commit of this client.
    pub fn last_transaction(&self) -> Tid {
        Tid(self.inner.last_tid.load(Ordering::SeqCst))
    }

    /// Return `n` new oids.
    pub async fn new_oids(&self, n: usize) -> Result<Vec<Oid>, Error> {
        let mut oids = Vec::with_capacity(n);
        while oids.len() < n {
            let ask = (n - oids.len()).min(OIDS_PER_CALL);
            let reply = self
                .inner
                .call("new_oids", vec![(ask as u64).into()])
                .await?;
            let Value::Array(items) = reply else {
                return Err(bad("new_oids", &reply));
            };
            if items.is_empty() {
                return Err(Error::Protocol("new_oids gave no oid".into()));
            }
            for item in &items {
                oids.push(oid(item, "new_oids")?);
            }
        }
        oids.truncate(n);
        Ok(oids)
    }

    /// Load the current record of `oid`: its pickle and its serial. An oid
    /// without a record fails with the `POSKeyError` of the server.
    pub async fn load(&self, oid: Oid) -> Result<(Vec<u8>, Tid), Error> {
        match self.load_before(oid, Tid(zoe_wire::MAX_TID)).await? {
            Some(record) => Ok((record.data, record.tid)),
            None => Err(protocol::pos_key_error(oid)),
        }
    }

    /// Load the record of `oid` that was current just before `tid`, or
    /// `None` when no record of `oid` is older than `tid`.
    pub async fn load_before(&self, oid: Oid, tid: Tid) -> Result<Option<Record>, Error> {
        self.load_before_at(oid, tid)
            .await
            .map(|(record, _)| record)
    }

    /// Load as `load_before` does, and return the position of the
    /// connection with the record: the last tid that the connection had
    /// read from its own stream when the reply came. The stream holds the
    /// tid of `register`, the invalidations and the replies of the finishes
    /// of the connection. The reader takes the position, not the caller.
    pub async fn load_before_at(&self, oid: Oid, tid: Tid) -> Result<(Option<Record>, Tid), Error> {
        let (reply, position) = self
            .inner
            .call_at("loadBefore", vec![p64(oid.0), p64(tid.0)], false)
            .await;
        let reply = reply?;
        if reply.is_nil() {
            return Ok((None, position));
        }
        let record = record(reply)?;
        Ok((Some(record), position))
    }

    /// Load the record of `oid` that the transaction `serial` wrote. An
    /// oid without such a record fails with the `POSKeyError` of the
    /// server.
    pub async fn load_serial(&self, oid: Oid, serial: Tid) -> Result<Vec<u8>, Error> {
        let reply = self
            .inner
            .call("loadSerial", vec![p64(oid.0), p64(serial.0)])
            .await?;
        match reply {
            // The data moves out of the reply: no copy.
            Value::Binary(data) => Ok(data),
            reply => Err(bad("loadSerial", &reply)),
        }
    }

    /// Download the blob of `oid` that the transaction `serial` wrote into
    /// `sink`, as `ZEO.ClientStorage.loadBlob` does: `sendBlob`, which the
    /// server answers with `receiveBlobStart`, the chunks and
    /// `receiveBlobStop` before its reply. Return the number of bytes.
    ///
    /// It succeeds only when the server sent the whole blob: the reply
    /// passed and `receiveBlobStop` came. A blob that the server does not
    /// have fails with the `POSKeyError` of the server. A write to `sink`
    /// that fails gives `Error::Io` after the reply; the download writes
    /// nothing more after it. The connection runs one download of a blob
    /// at a time: a second download of the same blob waits for the reply
    /// of the first, also when the future of the first drops before.
    ///
    /// The caller polls the future until it ends, or drops it. While the
    /// channel of the download is full, the reader of the connection waits,
    /// and the other replies and invalidations of the connection wait too.
    pub async fn send_blob<W: Write>(
        &self,
        oid: Oid,
        serial: Tid,
        sink: &mut W,
    ) -> Result<u64, Error> {
        self.inner.send_blob(oid, serial, sink).await
    }

    /// Return the commits after `tid`, as `getInvalidations` of the server
    /// gives them: the last tid of the server and each oid that a commit
    /// after `tid` wrote. Return `None` when the server does not keep the
    /// commits after `tid`: the caller then drops its cache.
    pub async fn get_invalidations(&self, tid: Tid) -> Result<Option<Invalidation>, Error> {
        let reply = self
            .inner
            .call("getInvalidations", vec![p64(tid.0)])
            .await?;
        if reply.is_nil() {
            return Ok(None);
        }
        let Some([last, oids]) = reply
            .as_array()
            .and_then(|items| <&[Value; 2]>::try_from(items.as_slice()).ok())
        else {
            return Err(bad("getInvalidations", &reply));
        };
        let Some(oids) = oids.as_array() else {
            return Err(bad("getInvalidations", &reply));
        };
        Ok(Some(Invalidation {
            tid: self::tid(last, "getInvalidations")?,
            oids: oids
                .iter()
                .map(|item| oid(item, "getInvalidations"))
                .collect::<Result<_, _>>()?,
        }))
    }

    /// Return true when the server holds a commit at exactly `tid`. The
    /// client starts a transaction iterator from `tid` to `tid`, reads its
    /// first transaction and releases the iterator. A stock ZEO 6 server
    /// and `zoe serve` both answer these calls.
    pub async fn holds_transaction(&self, tid: Tid) -> Result<bool, Error> {
        let id = self
            .inner
            .call("iterator_start", vec![p64(tid.0), p64(tid.0)])
            .await?;
        let next = self.inner.call("iterator_next", vec![id.clone()]).await;
        self.inner
            .notify("iterator_gc", vec![Value::Array(vec![id])])
            .await?;
        let next = next?;
        if next.is_nil() {
            return Ok(false);
        }
        let Some(first) = next.as_array().and_then(|items| items.first()) else {
            return Err(bad("iterator_next", &next));
        };
        Ok(self::tid(first, "iterator_next")? == tid)
    }

    /// Return the next value of the sequence `name` of the store. It needs
    /// the extension `sequences`: without it, it fails with
    /// `Error::Unsupported` and sends nothing.
    pub async fn sequence_next(&self, name: &str) -> Result<u64, Error> {
        self.check_extension(SEQUENCES)?;
        let reply = self.inner.call("sequence_next", vec![name.into()]).await?;
        reply.as_u64().ok_or_else(|| bad("sequence_next", &reply))
    }

    /// Return the value of `key` in the namespace `name` of `flavour`, or
    /// `None` when the key has no value or is expired. It needs the
    /// extension `kv`: without it, it fails with `Error::Unsupported` and
    /// sends nothing.
    pub async fn kv_get(
        &self,
        flavour: Flavour,
        name: &str,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, Error> {
        self.check_extension(KV)?;
        let args = vec![flavour.name().into(), name.into(), key.into()];
        match self.inner.call("kv_get", args).await? {
            Value::Nil => Ok(None),
            Value::Binary(value) => Ok(Some(value)),
            other => Err(bad("kv_get", &other)),
        }
    }

    /// Return at most `limit` pairs of the namespace `name` of `flavour`,
    /// in the byte order of the keys, from `start` (included) to `stop`
    /// (excluded). `None` is an open end. The server gives at most 1,000
    /// pairs. It needs the extension `kv`, as `kv_get` does.
    pub async fn kv_scan(
        &self,
        flavour: Flavour,
        name: &str,
        start: Option<&[u8]>,
        stop: Option<&[u8]>,
        limit: u64,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, Error> {
        self.check_extension(KV)?;
        let bound = |bound: Option<&[u8]>| bound.map_or(Value::Nil, Value::from);
        let args = vec![
            flavour.name().into(),
            name.into(),
            bound(start),
            bound(stop),
            limit.into(),
        ];
        let reply = self.inner.call("kv_scan", args).await?;
        let Value::Array(pairs) = reply else {
            return Err(bad("kv_scan", &reply));
        };
        pairs
            .into_iter()
            .map(|pair| match pair {
                Value::Array(items) => match <[Value; 2]>::try_from(items) {
                    Ok([Value::Binary(key), Value::Binary(value)]) => Ok((key, value)),
                    Ok(items) => Err(bad("kv_scan", &Value::Array(items.into()))),
                    Err(items) => Err(bad("kv_scan", &Value::Array(items))),
                },
                other => Err(bad("kv_scan", &other)),
            })
            .collect()
    }

    /// Return the generation of the durable namespace `name`: the tid of
    /// the last commit that wrote it, or `Tid(0)`. It needs the extension
    /// `kv`, as `kv_get` does.
    pub async fn kv_generation(&self, name: &str) -> Result<Tid, Error> {
        self.check_extension(KV)?;
        let reply = self.inner.call("kv_generation", vec![name.into()]).await?;
        self::tid(&reply, "kv_generation")
    }

    /// Take the early write-intent lock of each oid of `oids`, in ascending
    /// order, for the next transaction of this connection (ADR 0045).
    /// Return the current serial of each oid, in the same order, when the
    /// server holds them all. It needs the extension `lock_intent`: without
    /// it, it fails with `Error::Unsupported` and sends nothing.
    ///
    /// An oid at or below an oid that the connection holds fails with the
    /// `StorageTransactionError` of the server, and a wait past the lock
    /// timeout with its `ConflictError`.
    pub async fn lock_intent(&self, oids: &[Oid]) -> Result<Vec<Tid>, Error> {
        self.check_extension(LOCK_INTENT)?;
        let oids = oids.iter().map(|oid| p64(oid.0)).collect();
        let reply = self
            .inner
            .call("lock_intent", vec![Value::Array(oids)])
            .await?;
        let Some(serials) = reply.as_array() else {
            return Err(bad("lock_intent", &reply));
        };
        serials
            .iter()
            .map(|serial| tid(serial, "lock_intent"))
            .collect()
    }

    /// Release every intent lock of this connection, and wait for the
    /// reply of the server.
    pub async fn unlock_intent(&self) -> Result<(), Error> {
        self.check_extension(LOCK_INTENT)?;
        self.inner.call("unlock_intent", vec![]).await.map(drop)
    }

    /// Send `unlock_intent` and do not wait for its reply. The server
    /// answers the calls of a connection in order, so a later call of the
    /// connection comes after the release. `keep` drops only after the
    /// frame is in the channel: a pool gives the connection back with it,
    /// so the next transaction of the connection comes after the release.
    pub fn unlock_intent_later(&self, keep: Box<dyn Any + Send>) {
        self.inner.send_later("unlock_intent", vec![], keep);
    }

    /// Fail with `Error::Unsupported` when the server did not accept the
    /// extension `name`.
    fn check_extension(&self, name: &str) -> Result<(), Error> {
        if self.extensions.iter().any(|accepted| accepted == name) {
            return Ok(());
        }
        Err(unsupported(name))
    }

    /// Call `method` of the server with `args`, and return its reply, as
    /// `_call` of `ZEO.ClientStorage` does. An error reply fails with the
    /// error of the server.
    pub async fn call(&self, method: &str, args: Vec<Value>) -> Result<Value, Error> {
        self.inner.call(method, args).await
    }

    /// Send `method` with `args` as a call without a reply, as `_async` of
    /// `ZEO.ClientStorage` does.
    pub async fn notify(&self, method: &str, args: Vec<Value>) -> Result<(), Error> {
        self.inner.notify(method, args).await
    }

    /// Call `ping`, and wait for its reply. The server answers the calls of
    /// a connection in order, so each invalidation that the server sent
    /// before the reply is in the channel of the invalidations when this
    /// returns.
    pub async fn ping(&self) -> Result<(), Error> {
        self.inner.call("ping", vec![]).await.map(drop)
    }

    /// Return at most `size` entries of the history of `oid`, newest first.
    pub async fn history(&self, oid: Oid, size: usize) -> Result<Vec<HistoryEntry>, Error> {
        let reply = self
            .inner
            .call("history", vec![p64(oid.0), (size as u64).into()])
            .await?;
        let Value::Array(items) = &reply else {
            return Err(bad("history", &reply));
        };
        items
            .iter()
            .map(|item| {
                let bytes = |key| get(item, key).and_then(Value::as_slice).map(<[u8]>::to_vec);
                Some(HistoryEntry {
                    tid: Tid(get(item, "tid").and_then(u64_of)?),
                    time: get(item, "time").and_then(Value::as_f64)?,
                    user_name: bytes("user_name")?,
                    description: bytes("description")?,
                    size: get(item, "size").and_then(Value::as_u64)?,
                })
            })
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| bad("history", &reply))
    }

    /// Begin a transaction. `extension` is the msgpack encoding of the map
    /// of the transaction extension, or empty for an empty map. A client
    /// has one transaction at a time: a second `begin` waits until the
    /// first transaction ends.
    pub async fn begin(
        &self,
        user: &[u8],
        description: &[u8],
        extension: &[u8],
    ) -> Result<Transaction, Error> {
        self.begin_with(user, description, extension, None, " ")
            .await
    }

    /// Begin a transaction as `begin` does, with the tid and the status of
    /// `tpc_begin`. With a tid, the transaction commits under that tid, as
    /// ZODB's copy functions ask. `status` is `" "`, or `"p"` for a
    /// transaction that a pack changed.
    pub async fn begin_with(
        &self,
        user: &[u8],
        description: &[u8],
        extension: &[u8],
        tid: Option<Tid>,
        status: &str,
    ) -> Result<Transaction, Error> {
        let extension = extension_map(extension)?;
        let guard = self.inner.commit.clone().lock_owned().await;
        let id = self.inner.next_txn.fetch_add(1, Ordering::SeqCst);
        self.inner
            .notify(
                "tpc_begin",
                vec![
                    id.into(),
                    user.into(),
                    description.into(),
                    extension,
                    tid.map_or(Value::Nil, |tid| p64(tid.0)),
                    status.into(),
                ],
            )
            .await?;
        let kv = self.check_extension(KV).is_ok();
        Ok(Transaction::new(self.inner.clone(), id, guard, kv))
    }

    /// Return a receiver of the invalidations that the server sends from
    /// now on: the commits of the other clients of the storage. The
    /// commits of this client give no invalidation.
    ///
    /// Subscribe first, then read `last_transaction`. The client raises
    /// `last_transaction` before it sends an invalidation to the receivers.
    /// So the receiver gets each invalidation after the tid that
    /// `last_transaction` then gives. In the other order, an invalidation
    /// can come between the two steps, and the receiver misses it.
    pub fn invalidations(&self) -> Invalidations {
        match &*lock(&self.inner.invalidations) {
            Some(sender) => Invalidations(sender.subscribe()),
            None => Invalidations(broadcast::channel(1).1),
        }
    }

    /// Close the connection, and do not wait for its tasks. Each call that
    /// waits fails with `Error::Disconnected`, and so does each later
    /// call.
    pub fn disconnect(&self) {
        self.inner.closing.send_replace(true);
    }

    /// Close the connection, and wait until its tasks stop.
    pub async fn close(mut self) {
        self.inner.closing.send_replace(true);
        while self.tasks.recv().await.is_some() {}
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.inner.closing.send_replace(true);
    }
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("last_transaction", &self.last_transaction())
            .finish_non_exhaustive()
    }
}

/// Tell if `class` is the class of a `TypeError`, as `builtins.TypeError`.
fn is_type_error(class: &str) -> bool {
    class.rsplit('.').next() == Some("TypeError")
}

/// Tell if the exception class `class` is a `ReadOnlyError`, of any
/// module.
fn is_read_only_error(class: &str) -> bool {
    class.rsplit('.').next() == Some("ReadOnlyError")
}

/// Tell if `error` is an error of the connection, which a later attempt
/// can avoid: the stream failed or closed.
fn is_of_the_connection(error: &Error) -> bool {
    matches!(error, Error::Io(_) | Error::Disconnected)
}

/// Return how specific `error` is. An error of the server or of the
/// protocol tells the most. A failed TLS handshake, a bad stream
/// (`InvalidData`) or a server of another protocol comes next. A refused,
/// lost or closed connection tells the least.
fn specificity(error: &Error) -> u8 {
    match error {
        Error::Io(error) if error.kind() == io::ErrorKind::InvalidData => 1,
        Error::OtherProtocol(_) => 1,
        error if is_of_the_connection(error) => 0,
        _ => 2,
    }
}

/// Tell if `Client::open_first` keeps the new error `error` in place of
/// `kept`: a more specific error, or a later error of the connection.
fn keeps(error: &Error, kept: &Error) -> bool {
    let (new, old) = (specificity(error), specificity(kept));
    new > old || (new == old && new < 2)
}

/// Return the names of the key `extensions` of the map `info` that the
/// client declared in `declared`, in the order of the server.
fn accepted(info: &Value, declared: &[String]) -> Vec<String> {
    let Some(Value::Array(names)) = get(info, "extensions") else {
        return Vec::new();
    };
    names
        .iter()
        .filter_map(Value::as_str)
        .filter(|name| declared.iter().any(|declared| declared.as_str() == *name))
        .map(str::to_owned)
        .collect()
}

/// Put each entry of `entries` into the map `into`: a key that `into` has
/// takes the new value, and a new key goes at the end.
pub(crate) fn merge(into: &mut Vec<(Value, Value)>, entries: &[(Value, Value)]) {
    for (key, value) in entries {
        match into.iter_mut().find(|(k, _)| k == key) {
            Some((_, old)) => *old = value.clone(),
            None => into.push((key.clone(), value.clone())),
        }
    }
}

/// Return the map of the extension: empty for no bytes, else the one
/// msgpack map that `bytes` encode.
fn extension_map(bytes: &[u8]) -> Result<Value, Error> {
    if bytes.is_empty() {
        return Ok(Value::Map(vec![]));
    }
    let mut rest = bytes;
    match rmpv::decode::read_value(&mut rest) {
        Ok(map @ Value::Map(_)) if rest.is_empty() => Ok(map),
        _ => Err(Error::Invalid(
            "the extension is not the msgpack encoding of one map".into(),
        )),
    }
}

/// Read a tid, or fail with a protocol error of `method`.
fn tid(value: &Value, method: &str) -> Result<Tid, Error> {
    u64_of(value).map(Tid).ok_or_else(|| bad(method, value))
}

/// Read an oid, or fail with a protocol error of `method`.
fn oid(value: &Value, method: &str) -> Result<Oid, Error> {
    u64_of(value).map(Oid).ok_or_else(|| bad(method, value))
}

/// Read the record of a reply of `loadBefore`: `[data, start, end]`. The
/// data moves out of the reply: no copy.
fn record(reply: Value) -> Result<Record, Error> {
    let Value::Array(items) = reply else {
        return Err(bad("loadBefore", &reply));
    };
    let [data, start, end]: [Value; 3] = items
        .try_into()
        .map_err(|items| bad("loadBefore", &Value::Array(items)))?;
    let next = match &end {
        Value::Nil => Some(None),
        end => u64_of(end).map(Some),
    };
    match (data, u64_of(&start), next) {
        (Value::Binary(data), Some(tid), Some(next)) => Ok(Record {
            data,
            tid: Tid(tid),
            end: next.map(Tid),
        }),
        (data, ..) => Err(bad("loadBefore", &Value::Array(vec![data, start, end]))),
    }
}

/// The error of a call that needs the extension `name`, which the server
/// did not accept.
fn unsupported(name: &str) -> Error {
    Error::Unsupported(format!("the server did not accept the extension {name}"))
}

/// The protocol error of a reply of `method` that has the wrong shape.
fn bad(method: &str, value: &Value) -> Error {
    Error::Protocol(format!("{method} gave {value}"))
}

#[cfg(test)]
mod tests {
    use super::connection::invalidation;
    use super::transaction::vote_reply;
    use super::*;

    /// Return the bin of the 8 big-endian bytes of `n`, as a stock server
    /// sends an oid or a tid. The tests build it without `zoe-wire`.
    fn bin8(n: u64) -> Value {
        Value::Binary(n.to_be_bytes().to_vec())
    }

    #[test]
    fn the_record_of_a_reply_keeps_the_buffer_of_its_data() {
        let data = vec![7u8; 1000];
        let at = data.as_ptr();
        let reply = Value::Array(vec![Value::Binary(data), bin8(5), Value::Nil]);
        let read = record(reply).unwrap();
        assert_eq!(read.data.as_ptr(), at);
        assert_eq!((read.data.len(), read.tid, read.end), (1000, Tid(5), None));
        let reply = Value::Array(vec![Value::Binary(vec![1]), bin8(5), bin8(9)]);
        assert_eq!(record(reply).unwrap().end, Some(Tid(9)));
        for reply in [
            Value::Nil,
            Value::Array(vec![Value::Binary(vec![1]), bin8(5)]),
            Value::Array(vec![Value::Nil, bin8(5), Value::Nil]),
            Value::Array(vec![Value::Binary(vec![1]), 5.into(), Value::Nil]),
            Value::Array(vec![Value::Binary(vec![1]), bin8(5), 9.into()]),
        ] {
            let text = reply.to_string();
            match record(reply) {
                Err(Error::Protocol(message)) => {
                    assert_eq!(message, format!("loadBefore gave {text}"))
                }
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn an_empty_extension_is_an_empty_map() {
        assert_eq!(extension_map(b"").unwrap(), Value::Map(vec![]));
    }

    #[test]
    fn an_extension_is_one_msgpack_map() {
        let mut map = Vec::new();
        let value = Value::Map(vec![("note".into(), "x".into())]);
        rmpv::encode::write_value(&mut map, &value).unwrap();
        assert_eq!(extension_map(&map).unwrap(), value);
        for bad in [
            b"\x90".to_vec(),
            [map.clone(), vec![0]].concat(),
            b"\xc1".to_vec(),
        ] {
            assert!(matches!(extension_map(&bad), Err(Error::Invalid(_))));
        }
    }

    fn conflict_map(data: Option<Value>, serials: Value) -> Value {
        let mut map = vec![("oid".into(), bin8(5)), ("serials".into(), serials)];
        map.extend(data.map(|data| ("data".into(), data)));
        Value::Map(map)
    }

    #[test]
    fn a_vote_reply_gives_the_resolved_oids_and_the_listed_conflicts() {
        let serials = Value::Array(vec![bin8(3), bin8(2)]);
        let map = conflict_map(Some(Value::Binary(b"p".to_vec())), serials.clone());
        let voted = vote_reply(&[bin8(4), map, bin8(6)]).unwrap();
        assert_eq!(voted.resolved, vec![Oid(4), Oid(6)]);
        let listed = Listed {
            oid: Oid(5),
            committed: Tid(3),
            read: Tid(2),
            data: b"p".to_vec(),
        };
        assert_eq!(voted.conflicts, vec![listed]);
        assert_eq!(vote_reply(&[]).unwrap(), Voted::default());
        // A map without the data, without both serials, or with data that
        // is not bytes, does not read.
        for map in [
            conflict_map(None, serials.clone()),
            conflict_map(Some(Value::Binary(vec![])), Value::Nil),
            conflict_map(Some(Value::Binary(vec![])), Value::Array(vec![bin8(3)])),
            conflict_map(Some(Value::from(1)), serials),
        ] {
            assert!(matches!(vote_reply(&[map]), Err(Error::Protocol(_))));
        }
        assert!(matches!(
            vote_reply(&[Value::from("x")]),
            Err(Error::Protocol(_))
        ));
    }

    #[test]
    fn a_type_error_of_any_module_is_a_type_error() {
        assert!(is_type_error("builtins.TypeError"));
        assert!(is_type_error("exceptions.TypeError"));
        assert!(!is_type_error("builtins.ValueError"));
        assert!(!is_type_error("ZODB.POSException.NotTypeError"));
    }

    #[test]
    fn a_read_only_error_of_any_module_is_a_read_only_error() {
        assert!(is_read_only_error("ZODB.POSException.ReadOnlyError"));
        assert!(is_read_only_error("ReadOnlyError"));
        assert!(!is_read_only_error("ZODB.POSException.ConflictError"));
        assert!(!is_read_only_error("ZODB.POSException.NotReadOnlyError"));
    }

    #[test]
    fn only_a_failed_or_closed_stream_is_an_error_of_the_connection() {
        let refused = Error::Io(io::Error::from(io::ErrorKind::ConnectionRefused));
        assert!(is_of_the_connection(&refused));
        assert!(is_of_the_connection(&Error::Disconnected));
        let server = Error::Server {
            class: "builtins.ValueError".into(),
            message: "unknown storage: 2".into(),
            value: Value::Array(vec!["unknown storage: 2".into()]),
        };
        assert!(!is_of_the_connection(&server));
        assert!(!is_of_the_connection(&Error::Protocol("M4".into())));
    }

    #[test]
    fn the_open_keeps_the_most_specific_error() {
        let refused = || Error::Io(io::Error::from(io::ErrorKind::ConnectionRefused));
        let handshake = || Error::Io(io::Error::new(io::ErrorKind::InvalidData, "UnknownIssuer"));
        let server = || Error::Server {
            class: "builtins.ValueError".into(),
            message: String::new(),
            value: Value::Array(vec![]),
        };
        // A TLS error is not lost behind a later refused connection.
        assert!(!keeps(&refused(), &handshake()));
        assert!(keeps(&handshake(), &refused()));
        // An error of the server stays over every error of a connection.
        assert!(!keeps(&handshake(), &server()));
        assert!(keeps(&server(), &handshake()));
        // The first error of a server stays, and a later error of a
        // connection of the same rank replaces an earlier one.
        assert!(!keeps(&server(), &server()));
        assert!(keeps(&refused(), &Error::Disconnected));
        assert!(keeps(&handshake(), &handshake()));
    }

    #[test]
    fn the_accepted_extensions_are_the_declared_names_of_get_info() {
        let declared = vec!["sequences".to_owned(), "other".to_owned()];
        let info = Value::Map(vec![(
            "extensions".into(),
            Value::Array(vec!["sequences".into(), "unknown".into()]),
        )]);
        assert_eq!(accepted(&info, &declared), vec!["sequences".to_owned()]);
        // A stock server gives no key `extensions`.
        let stock = Value::Map(vec![("length".into(), 0.into())]);
        assert!(accepted(&stock, &declared).is_empty());
        assert!(accepted(&Value::Nil, &declared).is_empty());
    }

    #[test]
    fn a_merge_replaces_a_key_and_adds_a_new_one_at_the_end() {
        let mut into = vec![("length".into(), 1.into()), ("size".into(), 2.into())];
        merge(
            &mut into,
            &[("size".into(), 5.into()), ("name".into(), "x".into())],
        );
        let merged: Vec<(Value, Value)> = vec![
            ("length".into(), 1.into()),
            ("size".into(), 5.into()),
            ("name".into(), "x".into()),
        ];
        assert_eq!(into, merged);
    }

    #[test]
    fn an_invalidation_reads_its_tid_and_oids() {
        let args = Value::Array(vec![bin8(9), Value::Array(vec![bin8(1), bin8(2)])]);
        assert_eq!(
            invalidation(&args),
            Some(Invalidation {
                tid: Tid(9),
                oids: vec![Oid(1), Oid(2)]
            })
        );
        assert_eq!(invalidation(&Value::Array(vec![bin8(9)])), None);
    }
}
