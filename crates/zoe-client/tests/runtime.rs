//! Tests of the client through its `Runtime`: a scripted server on an
//! in-memory stream, and a heartbeat on a fast clock.

use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rmpv::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use zoe_client::runtime::{BoxFuture, Stream};
use zoe_client::{BLOB_CHUNK, Client, Error, Listed, Options, Runtime, Tokio, Voted};
use zoe_core::{Oid, Tid};
use zoe_server::serve::{ServeOptions, Server};

const TIMEOUT: Duration = Duration::from_secs(30);

/// A runtime whose one connection is an in-memory stream, whose sleeps
/// take `1 / speed` of their time, and whose spawned tasks start `late`.
struct Scripted {
    stream: Mutex<Option<DuplexStream>>,
    speed: u32,
    late: Duration,
}

impl Runtime for Scripted {
    fn connect(&self, _addr: SocketAddr) -> BoxFuture<io::Result<Box<dyn Stream>>> {
        let stream = self.stream.lock().unwrap().take();
        Box::pin(async move {
            let stream = stream.ok_or(io::ErrorKind::ConnectionRefused)?;
            Ok(Box::new(stream) as Box<dyn Stream>)
        })
    }

    fn spawn(&self, task: BoxFuture<()>) {
        let late = self.late;
        tokio::spawn(async move {
            if !late.is_zero() {
                tokio::time::sleep(late).await;
            }
            task.await
        });
    }

    fn sleep(&self, duration: Duration) -> BoxFuture<()> {
        Box::pin(tokio::time::sleep(duration / self.speed))
    }
}

/// The server side of a scripted connection.
struct Peer(DuplexStream);

impl Peer {
    async fn send_raw(&mut self, payload: &[u8]) {
        self.0
            .write_all(&(payload.len() as u32).to_be_bytes())
            .await
            .unwrap();
        self.0.write_all(payload).await.unwrap();
    }

    async fn recv_raw(&mut self) -> Vec<u8> {
        let mut len = [0; 4];
        self.0.read_exact(&mut len).await.unwrap();
        let mut payload = vec![0; u32::from_be_bytes(len) as usize];
        self.0.read_exact(&mut payload).await.unwrap();
        payload
    }

    async fn send(&mut self, message: Value) {
        let mut payload = Vec::new();
        rmpv::encode::write_value(&mut payload, &message).unwrap();
        self.send_raw(&payload).await;
    }

    /// Read one message: `(id, flags, method, args)`.
    async fn recv(&mut self) -> (Value, Value, String, Value) {
        let payload = self.recv_raw().await;
        let Value::Array(items) = rmpv::decode::read_value(&mut payload.as_slice()).unwrap() else {
            panic!("not an array");
        };
        let [id, flags, method, args]: [Value; 4] = items.try_into().unwrap();
        (id, flags, method.as_str().unwrap().to_owned(), args)
    }

    async fn reply(&mut self, id: Value, value: Value) {
        self.send(Value::Array(vec![id, 0.into(), ".reply".into(), value]))
            .await;
    }

    /// Send a call of the server without a reply, as `receiveBlobChunk`.
    async fn notify(&mut self, method: &str, args: Vec<Value>) {
        let message = vec![0.into(), true.into(), method.into(), Value::Array(args)];
        self.send(Value::Array(message)).await;
    }

    /// Send the pieces of a download of the blob of `oid` at `tid`: the
    /// start, one chunk for each item of `chunks`, and the stop.
    async fn send_pieces(&mut self, oid: u64, tid: u64, chunks: &[&[u8]]) {
        self.notify("receiveBlobStart", vec![bin8(oid), bin8(tid)])
            .await;
        for chunk in chunks {
            let args = vec![bin8(oid), bin8(tid), chunk.to_vec().into()];
            self.notify("receiveBlobChunk", args).await;
        }
        self.notify("receiveBlobStop", vec![bin8(oid), bin8(tid)])
            .await;
    }

    /// Answer the protocol and `register` with the tid `last`.
    async fn handshake(&mut self, last: u64) {
        self.send_raw(b"M5").await;
        assert_eq!(self.recv_raw().await, b"M5");
        let (id, flags, method, args) = self.recv().await;
        assert_eq!(method, "register");
        assert_eq!(flags, Value::from(false));
        assert_eq!(args, Value::Array(vec!["1".into(), false.into()]));
        self.reply(id, bin8(last)).await;
    }
}

fn bin8(n: u64) -> Value {
    Value::Binary(n.to_be_bytes().to_vec())
}

/// A runtime with one scripted connection, and the server side of it.
fn scripted(speed: u32) -> (Arc<dyn Runtime>, Peer) {
    scripted_late(speed, Duration::ZERO)
}

/// A runtime with one scripted connection whose spawned tasks start
/// `late`, as a scheduler that runs them after the caller, and the server
/// side of it.
fn scripted_late(speed: u32, late: Duration) -> (Arc<dyn Runtime>, Peer) {
    let (client, server) = tokio::io::duplex(1 << 16);
    let runtime = Scripted {
        stream: Mutex::new(Some(client)),
        speed,
        late,
    };
    (Arc::new(runtime), Peer(server))
}

fn addr() -> SocketAddr {
    "127.0.0.1:1".parse().unwrap()
}

#[tokio::test]
async fn replies_out_of_order_go_to_their_calls() {
    let (runtime, mut peer) = scripted(1);
    let server = tokio::spawn(async move {
        peer.handshake(7).await;
        let mut calls = Vec::new();
        for _ in 0..3 {
            let (id, _, method, args) = peer.recv().await;
            assert_eq!(method, "loadBefore");
            calls.push((id, args));
        }
        // Reply in the reverse order. Each record carries its oid.
        for (id, args) in calls.into_iter().rev() {
            let oid = args.as_array().unwrap()[0].clone();
            let data = oid.as_slice().unwrap().to_vec();
            peer.reply(id, Value::Array(vec![data.into(), bin8(7), Value::Nil]))
                .await;
        }
        peer
    });
    let client = Client::connect_with(runtime, addr(), "1").await.unwrap();
    assert_eq!(client.last_transaction(), Tid(7));
    let (a, b, c) = tokio::join!(
        client.load(Oid(1)),
        client.load(Oid(2)),
        client.load(Oid(3))
    );
    for (oid, result) in [(1u64, a), (2, b), (3, c)] {
        assert_eq!(result.unwrap(), (oid.to_be_bytes().to_vec(), Tid(7)));
    }
    let _peer = server.await.unwrap();
    client.close().await;
}

#[tokio::test]
async fn a_lost_connection_fails_every_pending_call() {
    let (runtime, mut peer) = scripted(1);
    let server = tokio::spawn(async move {
        peer.handshake(0).await;
        for _ in 0..10 {
            peer.recv().await;
        }
        // The server goes away with 10 calls that wait.
        drop(peer);
    });
    let client = Client::connect_with(runtime, addr(), "1").await.unwrap();
    let loads = (0..10).map(|oid| client.load(Oid(oid)));
    let results = tokio::time::timeout(TIMEOUT, futures::future::join_all(loads))
        .await
        .unwrap();
    for result in results {
        assert!(matches!(result, Err(Error::Disconnected)), "{result:?}");
    }
    server.await.unwrap();
    // A later call fails at once.
    assert!(matches!(client.new_oids(1).await, Err(Error::Disconnected)));
    let mut invalidations = client.invalidations();
    assert!(matches!(
        invalidations.recv().await,
        Err(Error::Disconnected)
    ));
    client.close().await;
}

#[tokio::test]
async fn a_bad_frame_closes_the_connection() {
    let (runtime, mut peer) = scripted(1);
    let server = tokio::spawn(async move {
        peer.handshake(0).await;
        peer.recv().await;
        peer.send_raw(b"\xc1").await;
        peer
    });
    let client = Client::connect_with(runtime, addr(), "1").await.unwrap();
    let result = tokio::time::timeout(TIMEOUT, client.new_oids(1))
        .await
        .unwrap();
    assert!(matches!(result, Err(Error::Disconnected)), "{result:?}");
    let _peer = server.await.unwrap();
}

#[tokio::test]
async fn a_bad_invalidation_closes_the_connection() {
    let (runtime, mut peer) = scripted(1);
    let (registered, ready) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        peer.handshake(0).await;
        ready.await.unwrap();
        peer.recv().await;
        // An oid of 3 bytes.
        let args = Value::Array(vec![
            bin8(9),
            Value::Array(vec![Value::Binary(vec![0, 0, 1])]),
        ]);
        peer.send(Value::Array(vec![
            0.into(),
            true.into(),
            "invalidateTransaction".into(),
            args,
        ]))
        .await;
        peer
    });
    let client = Client::connect_with(runtime, addr(), "1").await.unwrap();
    let mut invalidations = client.invalidations();
    registered.send(()).unwrap();
    let pending = tokio::time::timeout(TIMEOUT, client.new_oids(1))
        .await
        .unwrap();
    assert!(matches!(pending, Err(Error::Disconnected)), "{pending:?}");
    let next = tokio::time::timeout(TIMEOUT, invalidations.recv())
        .await
        .unwrap();
    assert!(matches!(next, Err(Error::Disconnected)), "{next:?}");
    assert_eq!(client.last_transaction(), Tid(0));
    let _peer = server.await.unwrap();
}

#[tokio::test]
async fn another_protocol_is_refused() {
    let (runtime, mut peer) = scripted(1);
    let server = tokio::spawn(async move {
        peer.send_raw(b"Z5").await;
        peer
    });
    match Client::connect_with(runtime, addr(), "1").await {
        Err(Error::Protocol(_)) => {}
        other => panic!("not a protocol error: {other:?}"),
    }
    let _peer = server.await.unwrap();
}

#[tokio::test]
async fn a_listed_conflict_fails_the_vote_and_aborts() {
    let (runtime, mut peer) = scripted(1);
    let server = tokio::spawn(async move {
        peer.handshake(0).await;
        let (_, flags, method, args) = peer.recv().await;
        assert_eq!((flags, method.as_str()), (Value::from(true), "tpc_begin"));
        let txn = args.as_array().unwrap()[0].clone();
        assert_eq!(args.as_array().unwrap()[3], Value::Map(vec![]));
        let (_, _, method, _) = peer.recv().await;
        assert_eq!(method, "storea");
        let (id, _, method, _) = peer.recv().await;
        assert_eq!(method, "vote");
        // The conflict map of a server with conflict resolution in the
        // client: the client must abort to release the locks.
        let conflict = Value::Map(vec![
            ("oid".into(), bin8(5)),
            ("serials".into(), Value::Array(vec![bin8(3), bin8(2)])),
            ("data".into(), Value::Binary(b"p".to_vec())),
        ]);
        peer.reply(id, Value::Array(vec![conflict])).await;
        let (id, _, method, args) = peer.recv().await;
        assert_eq!(method, "tpc_abort");
        assert_eq!(args, Value::Array(vec![txn]));
        peer.reply(id, Value::Nil).await;
        // After the abort, the transaction sends nothing: the next frame
        // is the next call of the client.
        let (id, _, method, _) = peer.recv().await;
        assert_eq!(method, "new_oids");
        peer.reply(id, Value::Array(vec![bin8(1)])).await;
        peer
    });
    let client = Client::connect_with(runtime, addr(), "1").await.unwrap();
    let mut txn = client.begin(b"u", b"d", b"").await.unwrap();
    txn.store(Oid(5), Tid(2), b"p").await.unwrap();
    match txn.vote().await {
        Err(Error::Conflict { oid, serials }) => {
            assert_eq!(oid, Oid(5));
            assert_eq!(serials, Some((Tid(3), Tid(2))));
        }
        other => panic!("not a conflict: {other:?}"),
    }
    // The server ended the transaction, so the client refuses more calls.
    let store = txn.store(Oid(5), Tid(3), b"q").await;
    assert!(matches!(store, Err(Error::Invalid(_))), "{store:?}");
    let vote = txn.vote().await;
    assert!(matches!(vote, Err(Error::Invalid(_))), "{vote:?}");
    drop(txn);
    assert_eq!(client.new_oids(1).await.unwrap(), vec![Oid(1)]);
    let _peer = server.await.unwrap();
    client.close().await;
}

#[tokio::test]
async fn a_listed_conflict_keeps_the_transaction_open_for_the_resolution() {
    let (runtime, mut peer) = scripted(1);
    let server = tokio::spawn(async move {
        peer.handshake(0).await;
        let (_, _, method, args) = peer.recv().await;
        assert_eq!(method, "tpc_begin");
        let txn = args.as_array().unwrap()[0].clone();
        let (_, _, method, _) = peer.recv().await;
        assert_eq!(method, "storea");
        let (id, _, method, _) = peer.recv().await;
        assert_eq!(method, "vote");
        let conflict = Value::Map(vec![
            ("oid".into(), bin8(5)),
            ("serials".into(), Value::Array(vec![bin8(3), bin8(2)])),
            ("data".into(), Value::Binary(b"p".to_vec())),
        ]);
        peer.reply(id, Value::Array(vec![bin8(4), conflict])).await;
        // No abort: the next frame is the store of the resolution, over
        // the committed serial.
        let (_, _, method, args) = peer.recv().await;
        assert_eq!(method, "storea");
        let resolved = vec![bin8(5), bin8(3), Value::Binary(b"r".to_vec()), txn];
        assert_eq!(args, Value::Array(resolved));
        let (id, _, method, _) = peer.recv().await;
        assert_eq!(method, "vote");
        peer.reply(id, Value::Array(vec![])).await;
        let (id, _, method, _) = peer.recv().await;
        assert_eq!(method, "tpc_finish");
        peer.reply(id, bin8(9)).await;
        peer
    });
    let client = Client::connect_with(runtime, addr(), "1").await.unwrap();
    let mut txn = client.begin(b"u", b"d", b"").await.unwrap();
    txn.store(Oid(5), Tid(2), b"p").await.unwrap();
    let voted = txn.vote_listed().await.unwrap();
    assert_eq!(voted.resolved, vec![Oid(4)]);
    let listed = Listed {
        oid: Oid(5),
        committed: Tid(3),
        read: Tid(2),
        data: b"p".to_vec(),
    };
    assert_eq!(voted.conflicts, vec![listed]);
    txn.store(Oid(5), Tid(3), b"r").await.unwrap();
    assert_eq!(txn.vote_listed().await.unwrap(), Voted::default());
    assert_eq!(txn.finish().await.unwrap(), Tid(9));
    let _peer = server.await.unwrap();
    client.close().await;
}

#[tokio::test]
async fn a_dropped_transaction_aborts() {
    let (runtime, mut peer) = scripted(1);
    let server = tokio::spawn(async move {
        peer.handshake(0).await;
        let (_, _, method, args) = peer.recv().await;
        assert_eq!(method, "tpc_begin");
        let txn = args.as_array().unwrap()[0].clone();
        let (_, _, method, args) = peer.recv().await;
        assert_eq!(method, "tpc_abort");
        assert_eq!(args, Value::Array(vec![txn]));
        peer
    });
    let client = Client::connect_with(runtime, addr(), "1").await.unwrap();
    let txn = client.begin(b"u", b"d", b"").await.unwrap();
    drop(txn);
    let _peer = tokio::time::timeout(TIMEOUT, server)
        .await
        .unwrap()
        .unwrap();
    // The next transaction begins: the dropped one released the lock.
    let txn = client.begin(b"u", b"d", b"").await.unwrap();
    drop(txn);
    client.close().await;
}

#[tokio::test]
async fn the_abort_of_a_dropped_transaction_comes_before_the_next_begin() {
    // The task that sends the abort of a full queue starts after the next
    // `begin` asked for room in the queue.
    let (runtime, mut peer) = scripted_late(1, Duration::from_millis(50));
    let (start, reading) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        peer.handshake(0).await;
        // Read nothing until the client waits, so its queue stays full.
        reading.await.unwrap();
        let mut methods = Vec::new();
        loop {
            let (_, _, method, _) = peer.recv().await;
            if method != "storea" {
                methods.push(method.clone());
            }
            if method == "tpc_begin" && methods.len() > 1 {
                return methods;
            }
        }
    });
    let client = Client::connect_with(runtime, addr(), "1").await.unwrap();
    let mut txn = client.begin(b"u", b"d", b"").await.unwrap();
    // Store until a store waits for room: the outgoing channel is full.
    let data = vec![0u8; 16 << 10];
    let mut stores = 0;
    while tokio::time::timeout(Duration::from_millis(100), txn.store(Oid(1), Tid(0), &data))
        .await
        .is_ok()
    {
        stores += 1;
        assert!(stores < 10_000, "the queue never filled");
    }
    drop(txn);
    let (next, ()) = tokio::join!(client.begin(b"u", b"d", b""), async {
        start.send(()).unwrap();
    });
    let next = next.unwrap();
    let methods = tokio::time::timeout(TIMEOUT, server)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(methods, ["tpc_begin", "tpc_abort", "tpc_begin"]);
    drop(next);
    client.close().await;
}

#[tokio::test]
async fn the_heartbeat_goes_out_on_the_clock_of_the_runtime() {
    // At 60,000 times the speed, the heartbeat comes every millisecond.
    let (runtime, mut peer) = scripted(60_000);
    let client = Client::connect_with(runtime, addr(), "1");
    let (client, ()) = tokio::join!(client, peer.handshake(0));
    let client = client.unwrap();
    for _ in 0..3 {
        let (id, flags, method, args) = tokio::time::timeout(TIMEOUT, peer.recv()).await.unwrap();
        assert_eq!(
            (id, flags, method.as_str(), args),
            (Value::from(-1), Value::from(0), ".reply", Value::Nil)
        );
    }
    client.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_heartbeat_keeps_an_idle_connection() {
    let dir = std::env::temp_dir().join(format!("zoe-client-idle-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let server =
        Server::start(ServeOptions::new(dir.join("store")).idle_timeout(Duration::from_secs(1)))
            .unwrap();

    /// The tokio runtime, with a heartbeat every 300 ms.
    struct Fast;
    impl Runtime for Fast {
        fn connect(&self, addr: SocketAddr) -> BoxFuture<io::Result<Box<dyn Stream>>> {
            Tokio.connect(addr)
        }
        fn spawn(&self, task: BoxFuture<()>) {
            Tokio.spawn(task)
        }
        fn sleep(&self, duration: Duration) -> BoxFuture<()> {
            Tokio.sleep(duration / 200)
        }
    }

    let beating = Client::connect_with(Arc::new(Fast), server.addr(), "1")
        .await
        .unwrap();
    let silent = Client::connect(server.addr(), "1").await.unwrap();
    tokio::time::sleep(Duration::from_millis(2500)).await;
    // The idle timeout closed the connection without a heartbeat.
    assert!(matches!(silent.new_oids(1).await, Err(Error::Disconnected)));
    assert_eq!(beating.new_oids(1).await.unwrap().len(), 1);
    beating.close().await;
    silent.close().await;
    server.stop();
}

#[tokio::test]
async fn a_blob_goes_out_as_a_stock_client_sends_it() {
    let (runtime, mut peer) = scripted(1);
    let blob: Vec<u8> = (0..2 * zoe_client::BLOB_CHUNK + 7)
        .map(|i| (i % 251) as u8)
        .collect();
    let expected = blob.clone();
    let server = tokio::spawn(async move {
        peer.handshake(0).await;
        let (_, _, method, args) = peer.recv().await;
        assert_eq!(method, "tpc_begin");
        let txn = args.as_array().unwrap()[0].clone();
        let store = Value::Array(vec![bin8(7), bin8(0), b"pickle".as_slice().into(), txn]);
        let mut methods = Vec::new();
        let mut got = Vec::new();
        loop {
            let (id, flags, method, args) = peer.recv().await;
            // No call of the upload has a reply.
            assert_eq!((id, flags), (Value::from(0), Value::from(true)), "{method}");
            match method.as_str() {
                "storea" | "storeBlobEnd" => assert_eq!(args, store, "{method}"),
                "storeBlobStart" => assert_eq!(args, Value::Array(vec![])),
                "storeBlobChunk" => {
                    let chunk = args.as_array().unwrap()[0].as_slice().unwrap();
                    assert!(chunk.len() <= zoe_client::BLOB_CHUNK);
                    got.extend_from_slice(chunk);
                }
                other => panic!("an unexpected call: {other}"),
            }
            methods.push(method.clone());
            if method == "storeBlobEnd" {
                break;
            }
        }
        assert_eq!(
            methods,
            [
                "storea",
                "storeBlobStart",
                "storeBlobChunk",
                "storeBlobChunk",
                "storeBlobChunk",
                "storeBlobEnd"
            ]
        );
        assert!(got == expected, "the chunks hold the blob");
        peer
    });
    let client = Client::connect_with(runtime, addr(), "1").await.unwrap();
    let mut txn = client.begin(b"u", b"d", b"").await.unwrap();
    txn.store_blob(Oid(7), Tid(0), b"pickle", &blob)
        .await
        .unwrap();
    let _peer = tokio::time::timeout(TIMEOUT, server)
        .await
        .unwrap()
        .unwrap();
    drop(txn);
    client.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_blob_commits_against_the_server() {
    let dir = std::env::temp_dir().join(format!("zoe-client-blob-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("store");
    let server = Server::start(ServeOptions::new(&path)).unwrap();
    let blob: Vec<u8> = (0..zoe_client::BLOB_CHUNK + 3)
        .map(|i| (i % 241) as u8)
        .collect();
    let pickle = b"\x80\x03cZODB.blob\nBlob\nq\x00.\x80\x03N.";
    let client = Client::connect(server.addr(), "1").await.unwrap();
    let mut txn = client.begin(b"u", b"d", b"").await.unwrap();
    txn.store_blob(Oid(1), Tid(0), pickle, &blob).await.unwrap();
    txn.vote().await.unwrap();
    let tid = txn.finish().await.unwrap();
    client.close().await;
    server.stop();

    let store = zoe_core::Store::open_read_only(&path).unwrap();
    assert_eq!(store.load_serial(Oid(1), tid).unwrap(), pickle);
    let mut got = Vec::new();
    std::io::Read::read_to_end(&mut store.open_blob(Oid(1), tid).unwrap(), &mut got).unwrap();
    assert!(got == blob, "the store holds the blob");
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The options of a client that declares `sequences`.
fn declaring() -> Options {
    Options {
        extensions: vec!["sequences".to_owned()],
        ..Options::default()
    }
}

#[tokio::test]
async fn open_declares_the_extensions_and_reads_get_info() {
    let (runtime, mut peer) = scripted(1);
    let server = tokio::spawn(async move {
        peer.send_raw(b"M5").await;
        assert_eq!(peer.recv_raw().await, b"M5");
        let (id, _, method, args) = peer.recv().await;
        assert_eq!(method, "register");
        let declared = Value::Array(vec!["sequences".into()]);
        assert_eq!(args, Value::Array(vec!["1".into(), false.into(), declared]));
        peer.reply(id, bin8(4)).await;
        let (id, _, method, _) = peer.recv().await;
        assert_eq!(method, "get_info");
        let info = Value::Map(vec![
            ("length".into(), 2.into()),
            ("extensions".into(), Value::Array(vec!["sequences".into()])),
        ]);
        peer.reply(id, info).await;
        let (id, _, method, args) = peer.recv().await;
        assert_eq!(method, "sequence_next");
        assert_eq!(args, Value::Array(vec!["a".into()]));
        peer.reply(id, 1.into()).await;
        peer
    });
    let client = Client::open(runtime, addr(), "1", &declaring())
        .await
        .unwrap();
    assert_eq!(client.last_transaction(), Tid(4));
    assert_eq!(client.extensions(), ["sequences".to_owned()]);
    assert!(client.info().is_map());
    assert!(client.is_connected());
    assert_eq!(client.sequence_next("a").await.unwrap(), 1);
    let _peer = server.await.unwrap();
    client.close().await;
}

#[tokio::test]
async fn open_registers_again_with_two_arguments_after_a_type_error() {
    let (runtime, mut peer) = scripted(1);
    let server = tokio::spawn(async move {
        peer.send_raw(b"M5").await;
        assert_eq!(peer.recv_raw().await, b"M5");
        // A stock server refuses the third argument.
        let (id, _, method, args) = peer.recv().await;
        assert_eq!(method, "register");
        assert_eq!(args.as_array().unwrap().len(), 3);
        let error = Value::Array(vec![
            "builtins.TypeError".into(),
            Value::Array(vec!["register() takes 3 positional arguments".into()]),
        ]);
        peer.send(Value::Array(vec![id, 2.into(), ".reply".into(), error]))
            .await;
        let (id, _, method, args) = peer.recv().await;
        assert_eq!(method, "register");
        assert_eq!(args, Value::Array(vec!["1".into(), false.into()]));
        peer.reply(id, bin8(3)).await;
        let (id, _, method, _) = peer.recv().await;
        assert_eq!(method, "get_info");
        peer.reply(id, Value::Map(vec![("length".into(), 0.into())]))
            .await;
        peer
    });
    let client = Client::open(runtime, addr(), "1", &declaring())
        .await
        .unwrap();
    assert_eq!(client.last_transaction(), Tid(3));
    assert!(client.extensions().is_empty());
    // The server accepted no extension, so the call sends nothing.
    let next = client.sequence_next("a").await;
    assert!(matches!(next, Err(Error::Unsupported(_))), "{next:?}");
    let _peer = server.await.unwrap();
    client.close().await;
}

#[tokio::test]
async fn a_stale_read_fails_the_vote_with_a_read_conflict() {
    let (runtime, mut peer) = scripted(1);
    let server = tokio::spawn(async move {
        peer.handshake(0).await;
        let (_, _, method, args) = peer.recv().await;
        assert_eq!(method, "tpc_begin");
        let txn = args.as_array().unwrap()[0].clone();
        let (_, flags, method, args) = peer.recv().await;
        assert_eq!(flags, Value::from(true));
        assert_eq!(method, "checkCurrentSerialInTransaction");
        assert_eq!(args, Value::Array(vec![bin8(5), bin8(2), txn.clone()]));
        let (id, _, method, _) = peer.recv().await;
        assert_eq!(method, "vote");
        let conflict = Value::Map(vec![
            ("oid".into(), bin8(5)),
            ("serials".into(), Value::Array(vec![bin8(3), bin8(2)])),
        ]);
        let error = Value::Array(vec!["ZODB.POSException.ReadConflictError".into(), conflict]);
        peer.send(Value::Array(vec![id, 2.into(), ".reply".into(), error]))
            .await;
        let (id, _, method, args) = peer.recv().await;
        assert_eq!(method, "tpc_abort");
        assert_eq!(args, Value::Array(vec![txn]));
        peer.reply(id, Value::Nil).await;
        peer
    });
    let client = Client::connect_with(runtime, addr(), "1").await.unwrap();
    let mut txn = client.begin(b"u", b"d", b"").await.unwrap();
    txn.check_current_serial(Oid(5), Tid(2)).await.unwrap();
    match txn.vote().await {
        Err(Error::ReadConflict { oid, serials }) => {
            assert_eq!(oid, Oid(5));
            assert_eq!(serials, Some((Tid(3), Tid(2))));
        }
        other => panic!("not a read conflict: {other:?}"),
    }
    drop(txn);
    let _peer = server.await.unwrap();
    client.close().await;
}

#[tokio::test]
async fn a_vote_gives_the_oids_that_the_server_resolved() {
    let (runtime, mut peer) = scripted(1);
    let server = tokio::spawn(async move {
        peer.handshake(0).await;
        let (_, _, method, _) = peer.recv().await;
        assert_eq!(method, "tpc_begin");
        let (_, _, method, _) = peer.recv().await;
        assert_eq!(method, "storea");
        let (id, _, method, _) = peer.recv().await;
        assert_eq!(method, "vote");
        peer.reply(id, Value::Array(vec![bin8(5)])).await;
        let (id, _, method, _) = peer.recv().await;
        assert_eq!(method, "tpc_finish");
        peer.reply(id, bin8(9)).await;
        peer
    });
    let client = Client::connect_with(runtime, addr(), "1").await.unwrap();
    let mut txn = client.begin(b"u", b"d", b"").await.unwrap();
    txn.store(Oid(5), Tid(2), b"p").await.unwrap();
    assert_eq!(txn.vote_resolved().await.unwrap(), vec![Oid(5)]);
    assert_eq!(txn.finish().await.unwrap(), Tid(9));
    let _peer = server.await.unwrap();
    client.close().await;
}

#[tokio::test]
async fn disconnect_fails_the_later_calls() {
    let (runtime, mut peer) = scripted(1);
    let server = tokio::spawn(async move {
        peer.handshake(0).await;
        peer
    });
    let client = Client::connect_with(runtime, addr(), "1").await.unwrap();
    let mut invalidations = client.invalidations();
    assert!(matches!(invalidations.try_recv(), Ok(None)));
    client.disconnect();
    let next = tokio::time::timeout(TIMEOUT, invalidations.recv())
        .await
        .unwrap();
    assert!(matches!(next, Err(Error::Disconnected)), "{next:?}");
    assert!(!client.is_connected());
    assert!(matches!(client.new_oids(1).await, Err(Error::Disconnected)));
    assert!(matches!(invalidations.try_recv(), Err(Error::Disconnected)));
    let _peer = server.await.unwrap();
    client.close().await;
}

/// Read one message of `peer` with all its elements.
async fn recv_parts(peer: &mut Peer) -> Vec<Value> {
    let payload = peer.recv_raw().await;
    let Value::Array(items) = rmpv::decode::read_value(&mut payload.as_slice()).unwrap() else {
        panic!("not an array");
    };
    items
}

/// Answer the protocol, `register` and `get_info`, and accept the
/// extensions `accepted`.
async fn open_with(peer: &mut Peer, accepted: &[&str]) {
    peer.send_raw(b"M5").await;
    assert_eq!(peer.recv_raw().await, b"M5");
    let (id, _, method, _) = peer.recv().await;
    assert_eq!(method, "register");
    peer.reply(id, bin8(1)).await;
    let (id, _, method, _) = peer.recv().await;
    assert_eq!(method, "get_info");
    let names = accepted.iter().map(|name| Value::from(*name)).collect();
    let info = Value::Map(vec![("extensions".into(), Value::Array(names))]);
    peer.reply(id, info).await;
}

const TRACEPARENT: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

/// The options of a client that declares `trace_context`.
fn tracing() -> Options {
    Options {
        extensions: vec!["trace_context".to_owned()],
        ..Options::default()
    }
}

#[tokio::test]
async fn a_call_inside_traced_carries_the_traceparent() {
    let (runtime, mut peer) = scripted(1);
    let server = tokio::spawn(async move {
        open_with(&mut peer, &["trace_context"]).await;
        let context = Value::Map(vec![("traceparent".into(), TRACEPARENT.into())]);
        // A load inside `traced`.
        let parts = recv_parts(&mut peer).await;
        assert_eq!(parts[2], Value::from("loadBefore"));
        assert_eq!(parts.get(4), Some(&context));
        peer.reply(parts[0].clone(), Value::Nil).await;
        // A load outside `traced`.
        let parts = recv_parts(&mut peer).await;
        assert_eq!(parts[2], Value::from("loadBefore"));
        assert_eq!(parts.len(), 4);
        peer.reply(parts[0].clone(), Value::Nil).await;
        // A load inside `traced` with no traceparent.
        let parts = recv_parts(&mut peer).await;
        assert_eq!(parts.len(), 4);
        peer.reply(parts[0].clone(), Value::Nil).await;
        // A call without a reply inside `traced`.
        let parts = recv_parts(&mut peer).await;
        assert_eq!(parts[2], Value::from("tpc_begin"));
        assert_eq!(parts.get(4), Some(&context));
        peer
    });
    let traced = Some(TRACEPARENT.to_owned());
    // `register` and `get_info` have four elements, also inside `traced`:
    // `Peer::recv` reads only four.
    let options = tracing();
    let open = Client::open(runtime, addr(), "1", &options);
    let client = zoe_client::traced(traced.clone(), open).await.unwrap();
    assert_eq!(client.extensions(), ["trace_context".to_owned()]);
    let load = client.load_before(Oid(1), Tid(9));
    let loaded = zoe_client::traced(traced.clone(), load).await;
    assert_eq!(loaded.unwrap(), None);
    assert_eq!(client.load_before(Oid(1), Tid(9)).await.unwrap(), None);
    let load = client.load_before(Oid(1), Tid(9));
    assert_eq!(zoe_client::traced(None, load).await.unwrap(), None);
    let begin = client.begin(b"u", b"d", b"");
    let txn = zoe_client::traced(traced, begin).await.unwrap();
    let _peer = server.await.unwrap();
    drop(txn);
    client.close().await;
}

#[tokio::test]
async fn a_call_carries_no_traceparent_when_the_server_did_not_accept_it() {
    let (runtime, mut peer) = scripted(1);
    let server = tokio::spawn(async move {
        open_with(&mut peer, &[]).await;
        let parts = recv_parts(&mut peer).await;
        assert_eq!(parts[2], Value::from("loadBefore"));
        assert_eq!(parts.len(), 4);
        peer.reply(parts[0].clone(), Value::Nil).await;
        peer
    });
    let client = Client::open(runtime, addr(), "1", &tracing())
        .await
        .unwrap();
    assert!(client.extensions().is_empty());
    let load = client.load_before(Oid(1), Tid(9));
    let traced = zoe_client::traced(Some(TRACEPARENT.to_owned()), load);
    assert_eq!(traced.await.unwrap(), None);
    let _peer = server.await.unwrap();
    client.close().await;
}

#[tokio::test]
async fn a_download_writes_the_chunks_of_its_blob_and_ends_at_the_reply() {
    let (runtime, mut peer) = scripted(1);
    let chunks: Vec<Vec<u8>> = (0..40u8).map(|n| vec![n; 1024]).collect();
    let expected = chunks.concat();
    let server = tokio::spawn(async move {
        peer.handshake(0).await;
        let (id, flags, method, args) = peer.recv().await;
        assert_eq!((method.as_str(), flags), ("sendBlob", Value::from(false)));
        assert_eq!(args, Value::Array(vec![bin8(1), bin8(5)]));
        // More chunks than the channel of a download holds: the reader
        // waits for the download.
        peer.notify("receiveBlobStart", vec![bin8(1), bin8(5)])
            .await;
        for (n, chunk) in chunks.iter().enumerate() {
            let args = vec![bin8(1), bin8(5), chunk.clone().into()];
            peer.notify("receiveBlobChunk", args).await;
            if n == 3 {
                // A piece of a blob that no download waits for goes
                // nowhere.
                let args = vec![bin8(2), bin8(5), b"xx".to_vec().into()];
                peer.notify("receiveBlobChunk", args).await;
            }
        }
        peer.notify("receiveBlobStop", vec![bin8(1), bin8(5)]).await;
        peer.reply(id, Value::Nil).await;
        peer
    });
    let client = Client::connect_with(runtime, addr(), "1").await.unwrap();
    let mut sink = Vec::new();
    let size = client.send_blob(Oid(1), Tid(5), &mut sink).await.unwrap();
    assert_eq!(size, expected.len() as u64);
    assert!(sink == expected, "the sink holds the chunks in order");
    let _peer = server.await.unwrap();
    client.close().await;
}

#[tokio::test]
async fn a_download_fails_without_its_stop_or_with_an_error_reply() {
    let (runtime, mut peer) = scripted(1);
    let server = tokio::spawn(async move {
        peer.handshake(0).await;
        // A reply before receiveBlobStop.
        let (id, _, _, _) = peer.recv().await;
        peer.notify("receiveBlobStart", vec![bin8(1), bin8(5)])
            .await;
        let args = vec![bin8(1), bin8(5), b"ab".to_vec().into()];
        peer.notify("receiveBlobChunk", args).await;
        peer.reply(id, Value::Nil).await;
        // A chunk before receiveBlobStart.
        let (id, _, _, _) = peer.recv().await;
        let args = vec![bin8(1), bin8(5), b"ab".to_vec().into()];
        peer.notify("receiveBlobChunk", args).await;
        peer.send_pieces(1, 5, &[b"cd"]).await;
        peer.reply(id, Value::Nil).await;
        // An error reply, as for a blob that the server does not have.
        let (id, _, _, _) = peer.recv().await;
        let error = Value::Array(vec![
            "ZODB.POSException.POSKeyError".into(),
            Value::Array(vec!["No blob file".into()]),
        ]);
        peer.send(Value::Array(vec![id, 2.into(), ".reply".into(), error]))
            .await;
        // The connection still serves a download.
        let (id, _, _, _) = peer.recv().await;
        peer.send_pieces(1, 5, &[b"ok"]).await;
        peer.reply(id, Value::Nil).await;
        peer
    });
    let client = Client::connect_with(runtime, addr(), "1").await.unwrap();
    for _ in 0..2 {
        let result = client.send_blob(Oid(1), Tid(5), &mut Vec::new()).await;
        assert!(matches!(result, Err(Error::Protocol(_))), "{result:?}");
    }
    let result = client.send_blob(Oid(1), Tid(5), &mut Vec::new()).await;
    assert!(
        matches!(&result, Err(Error::Server { class, .. }) if class.ends_with("POSKeyError")),
        "{result:?}"
    );
    let mut sink = Vec::new();
    assert_eq!(
        client.send_blob(Oid(1), Tid(5), &mut sink).await.unwrap(),
        2
    );
    assert_eq!(sink, b"ok");
    let _peer = server.await.unwrap();
    client.close().await;
}

/// A sink whose writes fail, as on a full disk.
struct Full;

impl io::Write for Full {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::Error::other("the disk is full"))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn a_sink_that_fails_fails_the_download_after_the_reply() {
    let (runtime, mut peer) = scripted(1);
    let server = tokio::spawn(async move {
        peer.handshake(0).await;
        let (id, _, _, _) = peer.recv().await;
        peer.send_pieces(1, 5, &[b"ab", b"cd"]).await;
        peer.reply(id, Value::Nil).await;
        // The next call of the connection gets its own reply.
        let (id, _, method, _) = peer.recv().await;
        assert_eq!(method, "loadBefore");
        peer.reply(id, Value::Nil).await;
        peer
    });
    let client = Client::connect_with(runtime, addr(), "1").await.unwrap();
    let result = client.send_blob(Oid(1), Tid(5), &mut Full).await;
    assert!(matches!(result, Err(Error::Io(_))), "{result:?}");
    assert_eq!(client.load_before(Oid(1), Tid(9)).await.unwrap(), None);
    let _peer = server.await.unwrap();
    client.close().await;
}

#[tokio::test]
async fn a_second_download_of_a_blob_waits_for_the_reply_of_the_first() {
    let (runtime, mut peer) = scripted(1);
    let (sent, started) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        peer.handshake(0).await;
        let (first, _, method, _) = peer.recv().await;
        assert_eq!(method, "sendBlob");
        peer.notify("receiveBlobStart", vec![bin8(1), bin8(5)])
            .await;
        let args = vec![bin8(1), bin8(5), b"old".to_vec().into()];
        peer.notify("receiveBlobChunk", args).await;
        sent.send(()).unwrap();
        // The first download dropped, and a second one starts. Its
        // sendBlob waits for the reply of the first.
        let early = tokio::time::timeout(Duration::from_millis(200), peer.recv()).await;
        assert!(early.is_err(), "a second sendBlob of the blob went out");
        let args = vec![bin8(1), bin8(5), b"rest".to_vec().into()];
        peer.notify("receiveBlobChunk", args).await;
        peer.notify("receiveBlobStop", vec![bin8(1), bin8(5)]).await;
        peer.reply(first, Value::Nil).await;
        let (second, _, method, _) = peer.recv().await;
        assert_eq!(method, "sendBlob");
        peer.send_pieces(1, 5, &[b"new"]).await;
        peer.reply(second, Value::Nil).await;
        peer
    });
    let client = Client::connect_with(runtime, addr(), "1").await.unwrap();
    let mut dropped = Vec::new();
    {
        let first = client.send_blob(Oid(1), Tid(5), &mut dropped);
        tokio::pin!(first);
        tokio::select! {
            biased;
            _ = started => {}
            result = &mut first => panic!("the first download ended: {result:?}"),
        }
    }
    let mut sink = Vec::new();
    let second = client.send_blob(Oid(1), Tid(5), &mut sink);
    let size = tokio::time::timeout(TIMEOUT, second)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((size, sink.as_slice()), (3, &b"new"[..]));
    let _peer = server.await.unwrap();
    client.close().await;
}

/// A reader that gives at most 1,000 bytes at a time.
struct Trickle(usize);

impl io::Read for Trickle {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.0.min(buf.len()).min(1000);
        buf[..n].fill(7);
        self.0 -= n;
        Ok(n)
    }
}

#[tokio::test]
async fn a_blob_from_a_reader_goes_out_in_full_chunks_and_a_shared_blob_by_its_name() {
    let (runtime, mut peer) = scripted(1);
    let server = tokio::spawn(async move {
        peer.handshake(0).await;
        let (_, _, method, args) = peer.recv().await;
        assert_eq!(method, "tpc_begin");
        let txn = args.as_array().unwrap()[0].clone();
        let mut calls = Vec::new();
        for _ in 0..8 {
            let (id, flags, method, args) = peer.recv().await;
            assert_eq!((id, flags), (Value::from(0), Value::from(true)));
            calls.push((method, args));
        }
        let methods: Vec<&str> = calls.iter().map(|(method, _)| method.as_str()).collect();
        assert_eq!(
            methods,
            [
                "storea",
                "storeBlobStart",
                "storeBlobChunk",
                "storeBlobChunk",
                "storeBlobChunk",
                "storeBlobEnd",
                "storea",
                "storeBlobShared",
            ]
        );
        let lengths: Vec<usize> = calls[2..5]
            .iter()
            .map(|(_, args)| args.as_array().unwrap()[0].as_slice().unwrap().len())
            .collect();
        assert_eq!(lengths, [BLOB_CHUNK, BLOB_CHUNK, 5]);
        let end = vec![bin8(1), bin8(0), Value::Binary(b"p".to_vec()), txn.clone()];
        assert_eq!(calls[5].1, Value::Array(end));
        let shared = vec![
            bin8(2),
            bin8(0),
            Value::Binary(b"q".to_vec()),
            "0x00-x.tmp-".into(),
            txn,
        ];
        assert_eq!(calls[7].1, Value::Array(shared));
        peer
    });
    let client = Client::connect_with(runtime, addr(), "1").await.unwrap();
    let mut txn = client.begin(b"", b"", b"").await.unwrap();
    let mut blob = Trickle(2 * BLOB_CHUNK + 5);
    txn.store_blob_from(Oid(1), Tid(0), b"p", &mut blob)
        .await
        .unwrap();
    txn.store_blob_shared(Oid(2), Tid(0), b"q", "0x00-x.tmp-")
        .await
        .unwrap();
    let _peer = tokio::time::timeout(TIMEOUT, server)
        .await
        .unwrap()
        .unwrap();
    drop(txn);
    client.close().await;
}

/// A reader that gives one full chunk of a blob, then fails.
struct Broken(bool);

impl io::Read for Broken {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.0 {
            return Err(io::Error::other("the file is gone"));
        }
        self.0 = true;
        let n = buf.len().min(BLOB_CHUNK);
        buf[..n].fill(1);
        Ok(n)
    }
}

#[tokio::test]
async fn a_failed_read_of_a_blob_ends_the_transaction() {
    let (runtime, mut peer) = scripted(1);
    let server = tokio::spawn(async move {
        peer.handshake(0).await;
        let mut methods = Vec::new();
        loop {
            let (_, _, method, _) = peer.recv().await;
            methods.push(method.clone());
            if method == "tpc_abort" {
                return methods;
            }
        }
    });
    let client = Client::connect_with(runtime, addr(), "1").await.unwrap();
    let mut txn = client.begin(b"", b"", b"").await.unwrap();
    let failed = txn
        .store_blob_from(Oid(1), Tid(0), b"p", &mut Broken(false))
        .await;
    assert!(matches!(failed, Err(Error::Io(_))), "{failed:?}");
    // Only the abort may follow: the later calls send nothing.
    let store = txn.store(Oid(2), Tid(0), b"q").await;
    assert!(matches!(store, Err(Error::Invalid(_))), "{store:?}");
    let vote = txn.vote().await;
    assert!(matches!(vote, Err(Error::Invalid(_))), "{vote:?}");
    drop(txn);
    let methods = tokio::time::timeout(TIMEOUT, server)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        methods,
        [
            "tpc_begin",
            "storea",
            "storeBlobStart",
            "storeBlobChunk",
            "tpc_abort"
        ]
    );
    client.close().await;
}

#[tokio::test]
async fn open_sends_the_client_label_after_get_info() {
    let (runtime, mut peer) = scripted(1);
    let server = tokio::spawn(async move {
        peer.handshake(4).await;
        let (id, _, method, _) = peer.recv().await;
        assert_eq!(method, "get_info");
        peer.reply(id, Value::Map(vec![("length".into(), 1.into())]))
            .await;
        // The label goes without a reply, as ZEO sends it.
        let (id, flags, method, args) = peer.recv().await;
        assert_eq!(method, "set_client_label");
        assert_eq!((id, flags), (Value::from(0), Value::from(true)));
        assert_eq!(args, Value::Array(vec!["worker".into()]));
        // An `info` call before the reply of a call is in the info of the
        // client when the call returns.
        let (id, _, method, _) = peer.recv().await;
        assert_eq!(method, "ping");
        let sizes = Value::Map(vec![("length".into(), 3.into()), ("size".into(), 9.into())]);
        peer.notify("info", vec![sizes]).await;
        peer.reply(id, Value::Nil).await;
        peer
    });
    let options = Options {
        client_label: Some("worker".to_owned()),
        ..Options::default()
    };
    let client = Client::open(runtime, addr(), "1", &options).await.unwrap();
    assert_eq!(client.pushed_info(), None);
    client.ping().await.unwrap();
    let (_, pushed) = client.pushed_info().unwrap();
    let sizes = Value::Map(vec![("length".into(), 3.into()), ("size".into(), 9.into())]);
    assert_eq!(pushed, sizes);
    let _peer = server.await.unwrap();
    client.close().await;
}
