//! Tests of the client through its `Runtime`: a scripted server on an
//! in-memory stream, and a heartbeat on a fast clock.

use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rmpv::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use zoe_client::runtime::{BoxFuture, Stream};
use zoe_client::{Client, Error, Runtime, Tokio};
use zoe_core::{Oid, Tid};
use zoe_server::serve::{ServeOptions, Server};

const TIMEOUT: Duration = Duration::from_secs(30);

/// A runtime whose one connection is an in-memory stream, and whose
/// sleeps take `1 / speed` of their time.
struct Scripted {
    stream: Mutex<Option<DuplexStream>>,
    speed: u32,
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
        tokio::spawn(task);
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
    let (client, server) = tokio::io::duplex(1 << 16);
    let runtime = Scripted {
        stream: Mutex::new(Some(client)),
        speed,
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
    drop(txn);
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
