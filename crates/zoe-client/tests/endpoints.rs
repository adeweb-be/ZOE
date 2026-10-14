//! Tests of the open of a client on several endpoints, on unix domain
//! sockets and with the read-only fallback, through a `Runtime` of the
//! test: scripted servers on in-memory streams.

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rmpv::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::sync::oneshot;
use zoe_client::runtime::{BoxFuture, Stream};
use zoe_client::{Address, Client, Endpoint, Error, Options, Runtime};

const WAIT: Duration = Duration::from_secs(30);

/// A runtime whose connections are in-memory streams, one for each address
/// at most. A connection to another address is refused.
#[derive(Default)]
struct Peers {
    streams: Mutex<HashMap<Address, DuplexStream>>,
    /// False for a runtime without unix sockets.
    unix: bool,
}

impl Peers {
    fn take(&self, address: &Address) -> BoxFuture<io::Result<Box<dyn Stream>>> {
        let stream = self.streams.lock().unwrap().remove(address);
        Box::pin(async move {
            let stream = stream.ok_or(io::ErrorKind::ConnectionRefused)?;
            Ok(Box::new(stream) as Box<dyn Stream>)
        })
    }
}

impl Runtime for Peers {
    fn connect(&self, addr: SocketAddr) -> BoxFuture<io::Result<Box<dyn Stream>>> {
        self.take(&Address::Tcp(addr))
    }

    fn connect_unix(&self, path: PathBuf) -> BoxFuture<io::Result<Box<dyn Stream>>> {
        if self.unix {
            self.take(&Address::Unix(path))
        } else {
            Box::pin(async { Err(io::ErrorKind::Unsupported.into()) })
        }
    }

    fn spawn(&self, task: BoxFuture<()>) {
        tokio::spawn(task);
    }

    fn sleep(&self, duration: Duration) -> BoxFuture<()> {
        Box::pin(tokio::time::sleep(duration))
    }
}

/// Return a runtime with unix sockets and a server side for each of
/// `addresses`.
fn peers(addresses: &[Address]) -> (Arc<dyn Runtime>, Vec<Peer>) {
    let mut streams = HashMap::new();
    let mut servers = Vec::new();
    for address in addresses {
        let (client, server) = tokio::io::duplex(1 << 16);
        streams.insert(address.clone(), client);
        servers.push(Peer(server));
    }
    let runtime = Peers {
        streams: Mutex::new(streams),
        unix: true,
    };
    (Arc::new(runtime), servers)
}

/// The server side of a scripted connection.
struct Peer(DuplexStream);

impl Peer {
    async fn send_raw(&mut self, payload: &[u8]) {
        let length = u32::try_from(payload.len()).unwrap();
        self.0.write_all(&length.to_be_bytes()).await.unwrap();
        self.0.write_all(payload).await.unwrap();
    }

    async fn recv_raw(&mut self) -> Vec<u8> {
        let mut length = [0; 4];
        self.0.read_exact(&mut length).await.unwrap();
        let mut payload = vec![0; u32::from_be_bytes(length) as usize];
        self.0.read_exact(&mut payload).await.unwrap();
        payload
    }

    async fn send(&mut self, message: Value) {
        let mut payload = Vec::new();
        rmpv::encode::write_value(&mut payload, &message).unwrap();
        self.send_raw(&payload).await;
    }

    /// Read one call: its id, its method and its arguments.
    async fn recv(&mut self) -> (Value, String, Value) {
        let payload = self.recv_raw().await;
        let Value::Array(items) = rmpv::decode::read_value(&mut payload.as_slice()).unwrap() else {
            panic!("not an array");
        };
        let [id, _flags, method, args]: [Value; 4] = items.try_into().unwrap();
        (id, method.as_str().unwrap().to_owned(), args)
    }

    async fn reply(&mut self, id: Value, value: Value) {
        self.send(Value::Array(vec![id, 0.into(), ".reply".into(), value]))
            .await;
    }

    async fn fail(&mut self, id: Value, class: &str) {
        let error = Value::Array(vec![class.into(), Value::Array(vec![])]);
        self.send(Value::Array(vec![id, 2.into(), ".reply".into(), error]))
            .await;
    }

    /// Agree on the protocol, and expect `register` with `read_only`.
    async fn register(&mut self, read_only: bool) -> Value {
        let (id, method, args) = self.recv().await;
        assert_eq!(method, "register");
        assert_eq!(args, Value::Array(vec!["1".into(), read_only.into()]));
        id
    }

    async fn protocol(&mut self) {
        self.send_raw(b"M5").await;
        assert_eq!(self.recv_raw().await, b"M5");
    }

    /// Answer `register` with the tid `last`, then `get_info`.
    async fn accept(&mut self, id: Value, last: u64) {
        self.reply(id, bin8(last)).await;
        let (id, method, _) = self.recv().await;
        assert_eq!(method, "get_info");
        self.reply(id, Value::Map(vec![("length".into(), 0.into())]))
            .await;
    }

    /// Wait until the client closes the connection.
    async fn closed(&mut self) {
        let mut rest = Vec::new();
        self.0.read_to_end(&mut rest).await.unwrap();
    }
}

fn bin8(n: u64) -> Value {
    Value::Binary(n.to_be_bytes().to_vec())
}

fn tcp(port: u16) -> Address {
    Address::Tcp(SocketAddr::from(([127, 0, 0, 1], port)))
}

fn endpoints(addresses: &[Address]) -> Vec<Endpoint> {
    addresses.iter().cloned().map(Endpoint::new).collect()
}

fn fallback() -> Options {
    Options {
        read_only_fallback: true,
        ..Options::default()
    }
}

#[tokio::test]
async fn the_open_skips_a_refused_address() {
    let open = tcp(2);
    let (runtime, mut servers) = peers(std::slice::from_ref(&open));
    let mut server = servers.pop().unwrap();
    let script = tokio::spawn(async move {
        server.protocol().await;
        let id = server.register(false).await;
        server.accept(id, 5).await;
        server
    });
    let all = endpoints(&[tcp(1), open.clone()]);
    let client = Client::open_first(runtime, &all, "1", &Options::default(), WAIT)
        .await
        .unwrap();
    assert_eq!(client.address(), &open);
    assert!(!client.read_only());
    let _server = script.await.unwrap();
    client.close().await;
}

#[tokio::test]
async fn the_open_keeps_a_writable_server_after_a_read_only_one() {
    let (read_only, writable) = (tcp(1), tcp(2));
    let (runtime, servers) = peers(&[read_only.clone(), writable.clone()]);
    let [mut first, mut second]: [Peer; 2] = servers.try_into().ok().unwrap();
    let (registered, later) = oneshot::channel();
    let read_only_script = tokio::spawn(async move {
        first.protocol().await;
        let id = first.register(false).await;
        first.fail(id, "ZODB.POSException.ReadOnlyError").await;
        // The fallback registers again, read-only, on the same connection.
        let id = first.register(true).await;
        first.accept(id, 3).await;
        registered.send(()).unwrap();
        // The client keeps the writable server and closes this one.
        first.closed().await;
    });
    let writable_script = tokio::spawn(async move {
        later.await.unwrap();
        second.protocol().await;
        let id = second.register(false).await;
        second.accept(id, 4).await;
        second
    });
    let all = endpoints(&[read_only, writable.clone()]);
    let client = Client::open_first(runtime, &all, "1", &fallback(), WAIT)
        .await
        .unwrap();
    assert_eq!(client.address(), &writable);
    assert!(!client.read_only());
    read_only_script.await.unwrap();
    let _second = writable_script.await.unwrap();
    client.close().await;
}

#[tokio::test]
async fn the_open_keeps_a_read_only_server_when_no_writable_one_opens() {
    let read_only = tcp(1);
    let (runtime, mut servers) = peers(std::slice::from_ref(&read_only));
    let mut server = servers.pop().unwrap();
    let script = tokio::spawn(async move {
        server.protocol().await;
        let id = server.register(false).await;
        server.fail(id, "ZODB.POSException.ReadOnlyError").await;
        let id = server.register(true).await;
        server.accept(id, 3).await;
        server
    });
    let all = endpoints(&[read_only.clone(), tcp(2)]);
    let client = Client::open_first(runtime, &all, "1", &fallback(), WAIT)
        .await
        .unwrap();
    assert_eq!(client.address(), &read_only);
    assert!(client.read_only());
    let _server = script.await.unwrap();
    client.close().await;
}

#[tokio::test]
async fn without_the_fallback_a_read_only_server_fails_the_open() {
    let read_only = tcp(1);
    let (runtime, mut servers) = peers(std::slice::from_ref(&read_only));
    let mut server = servers.pop().unwrap();
    let script = tokio::spawn(async move {
        server.protocol().await;
        let id = server.register(false).await;
        server.fail(id, "ZODB.POSException.ReadOnlyError").await;
        server
    });
    let all = endpoints(&[read_only]);
    let opened = Client::open_first(runtime, &all, "1", &Options::default(), WAIT).await;
    match opened {
        Err(Error::Server { class, .. }) => assert!(class.ends_with("ReadOnlyError")),
        other => panic!("{other:?}"),
    }
    let _server = script.await.unwrap();
}

#[tokio::test]
async fn an_error_of_a_server_comes_before_an_error_of_a_connection() {
    let answers = tcp(2);
    let (runtime, mut servers) = peers(std::slice::from_ref(&answers));
    let mut server = servers.pop().unwrap();
    let script = tokio::spawn(async move {
        server.protocol().await;
        let id = server.register(false).await;
        server.fail(id, "builtins.ValueError").await;
        server
    });
    let all = endpoints(&[tcp(1), answers, tcp(3)]);
    let opened = Client::open_first(runtime, &all, "1", &Options::default(), WAIT).await;
    match opened {
        Err(Error::Server { class, .. }) => assert_eq!(class, "builtins.ValueError"),
        other => panic!("{other:?}"),
    }
    let _server = script.await.unwrap();
}

#[tokio::test]
async fn a_unix_endpoint_connects_through_the_runtime() {
    let path = Address::Unix(PathBuf::from("./zeo.sock"));
    let (runtime, mut servers) = peers(std::slice::from_ref(&path));
    let mut server = servers.pop().unwrap();
    let script = tokio::spawn(async move {
        server.protocol().await;
        let id = server.register(false).await;
        server.accept(id, 1).await;
        server
    });
    let all = endpoints(std::slice::from_ref(&path));
    let client = Client::open_first(runtime, &all, "1", &Options::default(), WAIT)
        .await
        .unwrap();
    assert_eq!(client.address(), &path);
    let _server = script.await.unwrap();
    client.close().await;
}

#[tokio::test]
async fn a_runtime_without_unix_sockets_fails_a_unix_endpoint() {
    let runtime: Arc<dyn Runtime> = Arc::new(Peers::default());
    let all = endpoints(&[Address::Unix(PathBuf::from("./zeo.sock"))]);
    let opened = Client::open_first(runtime, &all, "1", &Options::default(), WAIT).await;
    match opened {
        Err(Error::Io(error)) => assert_eq!(error.kind(), io::ErrorKind::Unsupported),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn the_open_ends_when_the_wait_passes() {
    let silent = tcp(1);
    // The server takes the connection and never speaks.
    let (runtime, _servers) = peers(std::slice::from_ref(&silent));
    let all = endpoints(&[silent]);
    let wait = Duration::from_millis(50);
    let opened = Client::open_first(runtime, &all, "1", &Options::default(), wait).await;
    match opened {
        Err(Error::Io(error)) => assert_eq!(error.kind(), io::ErrorKind::TimedOut),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn the_open_needs_an_endpoint() {
    let runtime: Arc<dyn Runtime> = Arc::new(Peers::default());
    let opened = Client::open_first(runtime, &[], "1", &Options::default(), WAIT).await;
    assert!(matches!(opened, Err(Error::Invalid(_))), "{opened:?}");
}
