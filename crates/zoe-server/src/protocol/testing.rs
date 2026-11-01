//! The helpers that the unit tests of the server share: a stop signal
//! that never stops, a new directory that the test removes at its end, and
//! two raw clients of the ZEO 6 msgpack protocol. The functions over a
//! `TcpStream` send a call with an id of their choice. `Client` numbers
//! its calls and reads their replies.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rmpv::Value;
use zoe_core::{Oid, Tid};
use zoe_wire::p64;

use super::{PROTOCOL, Stopping, frame};

/// The longest time that a raw client waits for the server.
pub(crate) const TIMEOUT: Duration = Duration::from_secs(30);

/// Return a signal that never tells a stop: its sender is gone.
pub(crate) fn never_stops() -> Stopping {
    Stopping::new().1
}

/// A path for one test, in the temporary directory. The drop removes the
/// directory at the path. After a failure of the test, the directory
/// stays, so that its files can be examined.
pub(crate) struct Fresh(PathBuf);

impl Deref for Fresh {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for Fresh {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl Drop for Fresh {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

/// Return a path for the test `name` of the module that calls this
/// function. Nothing is at the path: the function removes what an earlier
/// run left there. The name of the directory holds the module, so two
/// modules can use the same test name.
#[track_caller]
pub(crate) fn fresh_dir(name: &str) -> Fresh {
    let file = std::panic::Location::caller().file();
    let module = file
        .rsplit_once("src/")
        .map_or(file, |(_, module)| module)
        .trim_end_matches(".rs")
        .replace('/', "-");
    let dir = std::env::temp_dir().join(format!("zoe-{module}-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    Fresh(dir)
}

/// Read one frame from `stream`.
pub(crate) fn read_frame(stream: &mut TcpStream) -> Vec<u8> {
    let mut len = [0u8; 4];
    stream.read_exact(&mut len).unwrap();
    let mut payload = vec![0u8; u32::from_be_bytes(len) as usize];
    stream.read_exact(&mut payload).unwrap();
    payload
}

/// Write `payload` as one frame to `stream`.
pub(crate) fn write_frame(stream: &mut TcpStream, payload: &[u8]) {
    stream.write_all(&frame::encode(payload).unwrap()).unwrap();
}

/// Encode a call. The id 0 is a call without a reply.
pub(crate) fn call_bytes(id: i64, method: &str, args: Vec<Value>) -> Vec<u8> {
    let call = Value::Array(vec![
        Value::from(id),
        Value::Boolean(id == 0),
        Value::from(method),
        Value::Array(args),
    ]);
    let mut bytes = Vec::new();
    rmpv::encode::write_value(&mut bytes, &call).unwrap();
    bytes
}

/// Send a call. The id 0 is a call without a reply.
pub(crate) fn send(stream: &mut TcpStream, id: i64, method: &str, args: Vec<Value>) {
    write_frame(stream, &call_bytes(id, method, args));
}

/// Connect to `addr`, answer the protocol and register.
pub(crate) fn registered(addr: SocketAddr) -> TcpStream {
    registered_as(addr, "1")
}

/// Connect to `addr`, answer the protocol and register with the storage
/// `name`.
pub(crate) fn registered_as(addr: SocketAddr, name: &str) -> TcpStream {
    let mut stream = TcpStream::connect(addr).unwrap();
    register(&mut stream, name);
    stream
}

/// Answer the protocol on `stream` and register with the storage `name`.
pub(crate) fn register(stream: &mut TcpStream, name: &str) {
    stream.set_read_timeout(Some(TIMEOUT)).unwrap();
    assert_eq!(read_frame(stream), PROTOCOL);
    write_frame(stream, PROTOCOL);
    send(
        stream,
        1,
        "register",
        vec![Value::from(name), Value::Boolean(false)],
    );
    read_frame(stream);
}

/// Decode the frame of a message: `[id, flags, method, args]`.
pub(crate) fn decode(frame: &[u8]) -> (Value, Value, String, Value) {
    let value = rmpv::decode::read_value(&mut &frame[..]).unwrap();
    let Value::Array(parts) = value else {
        panic!("not a message: {value:?}");
    };
    let [id, flags, method, args] = <[Value; 4]>::try_from(parts).unwrap();
    (id, flags, method.as_str().unwrap().to_owned(), args)
}

/// Return the bytes of a msgpack bin.
pub(crate) fn bytes(value: &Value) -> &[u8] {
    match value {
        Value::Binary(bytes) => bytes,
        other => panic!("not bytes: {other:?}"),
    }
}

/// A raw client of the ZEO 6 msgpack protocol.
pub(crate) struct Client {
    stream: TcpStream,
    next: i64,
}

impl Client {
    /// Connect to `addr`, then register for the storage `1`.
    pub(crate) fn connect(addr: SocketAddr) -> Self {
        Self::register(TcpStream::connect(addr).unwrap())
    }

    /// Connect as `connect` does, with a small receive buffer. So the
    /// server soon waits for this client to read.
    pub(crate) fn connect_slow(addr: SocketAddr) -> Self {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .build()
            .unwrap();
        let stream = runtime.block_on(async {
            let socket = tokio::net::TcpSocket::new_v4().unwrap();
            socket.set_recv_buffer_size(16 * 1024).unwrap();
            let stream = socket.connect(addr).await.unwrap();
            stream.into_std().unwrap()
        });
        stream.set_nonblocking(false).unwrap();
        Self::register(stream)
    }

    /// Connect, do the handshake, then register read-only for the
    /// storage `1`.
    pub(crate) fn connect_read_only(addr: SocketAddr) -> Self {
        let stream = TcpStream::connect(addr).unwrap();
        stream.set_read_timeout(Some(TIMEOUT)).unwrap();
        let mut client = Self { stream, next: 1 };
        assert_eq!(client.read_frame(), b"M5");
        client.write_frame(b"M5");
        client.call("register", vec![Value::from("1"), Value::Boolean(true)]);
        client
    }

    /// Do the handshake on `stream`, then register for the storage `1`.
    pub(crate) fn register(stream: TcpStream) -> Self {
        stream.set_read_timeout(Some(TIMEOUT)).unwrap();
        let mut client = Self { stream, next: 1 };
        assert_eq!(client.read_frame(), b"M5");
        client.write_frame(b"M5");
        client.call("register", vec![Value::from("1"), Value::Boolean(false)]);
        client
    }

    /// Write `payload` as one frame.
    pub(crate) fn write_frame(&mut self, payload: &[u8]) {
        write_frame(&mut self.stream, payload);
    }

    /// Read one frame.
    pub(crate) fn read_frame(&mut self) -> Vec<u8> {
        read_frame(&mut self.stream)
    }

    pub(crate) fn write(&mut self, id: Value, flags: bool, method: &str, args: Vec<Value>) {
        let message = Value::Array(vec![
            id,
            Value::Boolean(flags),
            Value::from(method),
            Value::Array(args),
        ]);
        let mut payload = Vec::new();
        rmpv::encode::write_value(&mut payload, &message).unwrap();
        self.write_frame(&payload);
    }

    /// Send a call without a reply.
    pub(crate) fn send_async(&mut self, method: &str, args: Vec<Value>) {
        self.write(Value::from(0), true, method, args);
    }

    /// Send a call. Return its id.
    pub(crate) fn send(&mut self, method: &str, args: Vec<Value>) -> Value {
        let id = Value::from(self.next);
        self.next += 1;
        self.write(id.clone(), false, method, args);
        id
    }

    /// Begin the transaction `txn`.
    pub(crate) fn begin(&mut self, txn: &Value) {
        let empty = Value::Binary(Vec::new());
        let begin = vec![txn.clone(), empty.clone(), empty, Value::Map(vec![])];
        self.send_async("tpc_begin", begin);
    }

    /// Tell if the server closed the connection. Wait at most
    /// `TIMEOUT` for the end of the stream.
    pub(crate) fn is_closed(&mut self) -> bool {
        let mut byte = [0; 1];
        match self.stream.read(&mut byte) {
            Ok(n) => n == 0,
            Err(error) => error.kind() == io::ErrorKind::ConnectionReset,
        }
    }

    /// Send a call and return the args of its reply, which must not be
    /// an error. Skip the calls of the server before the reply, as
    /// `info` before the reply of `tpc_finish`.
    pub(crate) fn call(&mut self, method: &str, args: Vec<Value>) -> Value {
        let id = self.send(method, args);
        loop {
            let (got, flags, name, args) = decode(&self.read_frame());
            if (&got, &flags) == (&Value::from(0), &Value::Boolean(true)) {
                continue;
            }
            assert_eq!((got, flags, name.as_str()), (id, Value::from(0), ".reply"));
            return args;
        }
    }

    /// Upload `blob` as the blob of `oid` in a new transaction, in
    /// chunks of 59,000 bytes as a stock client sends them. Return the
    /// tid of the commit.
    pub(crate) fn commit_blob(&mut self, oid: Oid, blob: &[u8]) -> Tid {
        let txn = Value::from(1);
        let pickle = Value::Binary(b"pickle".to_vec());
        self.begin(&txn);
        let store = vec![p64(oid.0), p64(0), pickle, txn.clone()];
        self.send_async("storea", store.clone());
        self.send_async("storeBlobStart", vec![]);
        for chunk in blob.chunks(59_000) {
            self.send_async("storeBlobChunk", vec![Value::Binary(chunk.to_vec())]);
        }
        self.send_async("storeBlobEnd", store);
        self.call("vote", vec![txn.clone()]);
        let tid = self.call("tpc_finish", vec![txn]);
        Tid(u64::from_be_bytes(bytes(&tid).try_into().unwrap()))
    }
}
