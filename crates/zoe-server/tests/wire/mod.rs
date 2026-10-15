//! The `zoe serve` process and a raw client of the ZEO 6 msgpack protocol,
//! for the acceptance tests of the server.
//!
//! A frame is a 4-byte big-endian length, then the bytes of the frame. The
//! server sends its protocol first, `M5`. The client answers with the same
//! bytes. Each later frame is a msgpack array `[message_id, flags, method,
//! args]`. A stock client sends `false` as the flags of a call. A reply has
//! the method `.reply`. Its flags are 2 when it carries an error, and then
//! its args are `[class, args]`. A call from the server to the client has
//! the flags `true` and no reply.
//!
//! `zoe serve` writes the line `listening <address>` to stdout when it
//! accepts connections. With `--admin-listen`, it writes `admin <address>`
//! before that line.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

pub use rmpv::Value;

/// The binary under test.
pub const ZOE: &str = env!("CARGO_BIN_EXE_zoe");

/// The longest time that a test waits for the server.
pub const TIMEOUT: Duration = Duration::from_secs(30);

/// The protocol of ZEO 6 with msgpack.
pub const PROTOCOL: &[u8] = b"M5";

/// The tid and the serial of an object that has no record yet.
pub const ZERO: [u8; 8] = [0; 8];

/// The tid that loads the current record.
pub const MAX: [u8; 8] = [0x7f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff];

/// A spawned child keeps a copy of each file descriptor of the test
/// process until it runs `exec`, the lock of a store too. So each spawn
/// and each open of a store in the test process holds this mutex.
static SPAWN: Mutex<()> = Mutex::new(());

/// Take the mutex of the spawns.
pub fn spawn_lock() -> MutexGuard<'static, ()> {
    SPAWN.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Open the store in `dir` in the test process.
pub fn open_store(dir: &Path) -> zoe_core::Store {
    let _spawn = spawn_lock();
    zoe_core::Store::open(dir).unwrap()
}

/// Return a new empty directory for the test `name`.
pub fn fresh_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("zoe-serve-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Return the text of a path.
pub fn s(path: &Path) -> &str {
    path.to_str().unwrap()
}

/// A `zoe serve` process. Dropping it kills the process.
pub struct Server {
    /// The `zoe` binary that runs.
    binary: String,
    child: Option<Child>,
    stdout: Option<BufReader<ChildStdout>>,
    args: Vec<String>,
    env: Vec<(String, String)>,
    /// The address of the ZEO port.
    pub addr: SocketAddr,
    /// The address of the admin port, when the server has one.
    pub admin: Option<SocketAddr>,
}

impl Server {
    /// Serve the store in `dir` under the storage name `1`.
    pub fn path(dir: &Path) -> Server {
        Server::start(&["--path", s(dir)], &[])
    }

    /// Start `zoe serve` with `args` and the variables `env`. No other
    /// `ZOE_*` variable reaches it. Without `--listen` in `args`, the server
    /// listens on a free port of 127.0.0.1.
    pub fn start(args: &[&str], env: &[(&str, &str)]) -> Server {
        Server::start_binary(ZOE, args, env)
    }

    /// Start `zoe serve` of the `zoe` binary `binary`, as `start` does.
    pub fn start_binary(binary: &str, args: &[&str], env: &[(&str, &str)]) -> Server {
        let mut args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        if !args
            .iter()
            .any(|a| a == "--listen" || a.starts_with("--listen="))
        {
            args.extend(["--listen".to_string(), "127.0.0.1:0".to_string()]);
        }
        let env = env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let mut server = Server {
            binary: binary.to_string(),
            child: None,
            stdout: None,
            args,
            env,
            addr: "127.0.0.1:0".parse().unwrap(),
            admin: None,
        };
        server.spawn();
        server
    }

    /// Start the server again with the same arguments, on the same ports.
    pub fn restart(&mut self) {
        assert!(self.child.is_none(), "the server still runs");
        let listen = self.addr.to_string();
        let mut args = Vec::new();
        let mut rest = self.args.iter();
        while let Some(arg) = rest.next() {
            if arg == "--listen" {
                rest.next();
            } else if !arg.starts_with("--listen=") {
                args.push(arg.clone());
            }
        }
        args.extend(["--listen".to_string(), listen]);
        if let Some(admin) = self.admin {
            let mut kept = Vec::new();
            let mut rest = args.iter();
            while let Some(arg) = rest.next() {
                if arg == "--admin-listen" {
                    rest.next();
                } else if !arg.starts_with("--admin-listen=") {
                    kept.push(arg.clone());
                }
            }
            kept.extend(["--admin-listen".to_string(), admin.to_string()]);
            args = kept;
        }
        self.args = args;
        self.spawn();
    }

    fn spawn(&mut self) {
        let mut command = Command::new(&self.binary);
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("ZOE_") {
                command.env_remove(name);
            }
        }
        command
            .arg("serve")
            .args(&self.args)
            .envs(self.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut child = {
            let _spawn = spawn_lock();
            command.spawn().unwrap()
        };
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        self.admin = None;
        loop {
            let mut line = String::new();
            let read = stdout.read_line(&mut line).unwrap();
            if read == 0 {
                let status = child.wait().unwrap();
                panic!(
                    "zoe serve {:?} stopped before it listened: {status}",
                    self.args
                );
            }
            let line = line.trim_end();
            if let Some(addr) = line.strip_prefix("admin ") {
                self.admin = Some(addr.parse().unwrap());
            } else if let Some(addr) = line.strip_prefix("listening ") {
                self.addr = addr.parse().unwrap();
                break;
            }
        }
        self.child = Some(child);
        self.stdout = Some(stdout);
    }

    /// The process id of the server.
    pub fn pid(&self) -> u32 {
        self.child.as_ref().unwrap().id()
    }

    /// Tell if the process still runs.
    pub fn running(&mut self) -> bool {
        match self.child.as_mut() {
            Some(child) => child.try_wait().unwrap().is_none(),
            None => false,
        }
    }

    /// Kill the server with SIGKILL and wait for it.
    pub fn kill(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.stdout = None;
    }

    /// Send SIGTERM to the server and wait at most `timeout` for its exit.
    /// Return its exit status and the time it took to stop.
    pub fn terminate(&mut self, timeout: Duration) -> (ExitStatus, Duration) {
        let mut child = self.child.take().expect("the server does not run");
        let started = Instant::now();
        let sent = Command::new("kill")
            .args(["-TERM", &child.id().to_string()])
            .status()
            .unwrap();
        assert!(sent.success());
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                self.stdout = None;
                return (status, started.elapsed());
            }
            if started.elapsed() > timeout {
                let _ = child.kill();
                let _ = child.wait();
                panic!("zoe serve did not stop within {timeout:?} after SIGTERM");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Open a connection to the server and answer its protocol.
    pub fn connect(&self) -> Conn {
        Conn::open(self.addr)
    }

    /// Open a connection, and register it with the storage `name`.
    pub fn client(&self, name: &str) -> Conn {
        let mut conn = self.connect();
        conn.register(name, false).unwrap();
        conn
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.kill();
    }
}

/// An error that the server sent as a reply.
#[derive(Debug, Clone, PartialEq)]
pub struct Fault {
    /// The class of the exception, for example
    /// `ZODB.POSException.ConflictError`.
    pub class: String,
    /// The arguments of the exception, or its attributes.
    pub args: Value,
}

/// A message that the server sent.
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    /// The message id. A reply carries the id of its call.
    pub id: Value,
    /// The flags: `true` or 1 for a call from the server, 2 for an error.
    pub flags: Value,
    /// The method, or `.reply`.
    pub method: String,
    /// The arguments, or the result.
    pub args: Value,
}

impl Message {
    /// Tell if the message is an error reply.
    pub fn is_error(&self) -> bool {
        self.method == ".reply" && self.flags.as_u64() == Some(2)
    }
}

/// A raw connection to the server.
pub struct Conn {
    stream: TcpStream,
    /// The protocol that the server announced.
    pub protocol: Vec<u8>,
    next_id: i64,
    /// The calls from the server, and the replies that no call waits for,
    /// in the order of arrival.
    pub received: VecDeque<Message>,
}

impl Conn {
    /// Connect to `addr`, read the protocol of the server and answer it.
    pub fn open(addr: SocketAddr) -> Conn {
        let stream = TcpStream::connect_timeout(&addr, TIMEOUT).unwrap();
        stream.set_read_timeout(Some(TIMEOUT)).unwrap();
        stream.set_nodelay(true).unwrap();
        let mut conn = Conn {
            stream,
            protocol: Vec::new(),
            next_id: 0,
            received: VecDeque::new(),
        };
        conn.protocol = conn.read_frame().unwrap();
        conn.write_frame(PROTOCOL).unwrap();
        conn
    }

    /// Connect to `addr` and read the protocol, but do not answer it.
    pub fn open_silent(addr: SocketAddr) -> Conn {
        let stream = TcpStream::connect_timeout(&addr, TIMEOUT).unwrap();
        stream.set_read_timeout(Some(TIMEOUT)).unwrap();
        let mut conn = Conn {
            stream,
            protocol: Vec::new(),
            next_id: 0,
            received: VecDeque::new(),
        };
        conn.protocol = conn.read_frame().unwrap();
        conn
    }

    /// The underlying stream.
    pub fn stream(&self) -> &TcpStream {
        &self.stream
    }

    /// Write one frame.
    pub fn write_frame(&mut self, bytes: &[u8]) -> io::Result<()> {
        let len = u32::try_from(bytes.len()).unwrap();
        let mut frame = len.to_be_bytes().to_vec();
        frame.extend_from_slice(bytes);
        self.stream.write_all(&frame)
    }

    /// Read one frame. End of file is `UnexpectedEof`.
    pub fn read_frame(&mut self) -> io::Result<Vec<u8>> {
        let mut len = [0u8; 4];
        self.stream.read_exact(&mut len)?;
        let mut bytes = vec![0u8; u32::from_be_bytes(len) as usize];
        self.stream.read_exact(&mut bytes)?;
        Ok(bytes)
    }

    /// Send a message with the id `id`, the flags `flags`, the method
    /// `method` and the arguments `args`.
    pub fn send(&mut self, id: Value, flags: Value, method: &str, args: Vec<Value>) {
        let message = Value::Array(vec![id, flags, Value::from(method), Value::Array(args)]);
        let mut bytes = Vec::new();
        rmpv::encode::write_value(&mut bytes, &message).unwrap();
        self.write_frame(&bytes).unwrap();
    }

    /// Send a call and return its message id. Do not wait for the reply.
    pub fn send_call(&mut self, method: &str, args: Vec<Value>) -> Value {
        self.next_id += 1;
        let id = Value::from(self.next_id);
        self.send(id.clone(), Value::Boolean(false), method, args);
        id
    }

    /// Send a call that has no reply, as `storea` and `storeBlobChunk` are.
    pub fn send_async(&mut self, method: &str, args: Vec<Value>) {
        self.next_id += 1;
        self.send(
            Value::from(self.next_id),
            Value::Boolean(true),
            method,
            args,
        );
    }

    /// Read one message from the server.
    pub fn recv(&mut self) -> io::Result<Message> {
        let bytes = self.read_frame()?;
        let value = rmpv::decode::read_value(&mut bytes.as_slice()).unwrap();
        let Value::Array(mut parts) = value else {
            panic!("a message is not an array: {value:?}");
        };
        assert_eq!(parts.len(), 4, "a message has four parts: {parts:?}");
        let args = parts.pop().unwrap();
        let method = parts.pop().unwrap().as_str().unwrap().to_string();
        let flags = parts.pop().unwrap();
        let id = parts.pop().unwrap();
        Ok(Message {
            id,
            flags,
            method,
            args,
        })
    }

    /// Read messages until the reply to the call `id`. Keep the others in
    /// `received`.
    pub fn reply_to(&mut self, id: &Value) -> Result<Value, Fault> {
        if let Some(at) = self
            .received
            .iter()
            .position(|m| m.method == ".reply" && &m.id == id)
        {
            return into_result(self.received.remove(at).unwrap());
        }
        loop {
            let message = self.recv().unwrap();
            if message.method == ".reply" && &message.id == id {
                return into_result(message);
            }
            self.received.push_back(message);
        }
    }

    /// Call `method` with `args` and wait for its reply.
    pub fn call(&mut self, method: &str, args: Vec<Value>) -> Result<Value, Fault> {
        let id = self.send_call(method, args);
        self.reply_to(&id)
    }

    /// Register with the storage `name`. Return the last tid of the
    /// storage.
    pub fn register(&mut self, name: &str, read_only: bool) -> Result<Value, Fault> {
        self.call(
            "register",
            vec![Value::from(name), Value::Boolean(read_only)],
        )
    }

    /// Wait at most `timeout` for a call from the server named `method`.
    /// Return its arguments, or `None` when none comes in time.
    pub fn wait_for(&mut self, method: &str, timeout: Duration) -> Option<Value> {
        if let Some(at) = self.received.iter().position(|m| m.method == method) {
            return Some(self.received.remove(at).unwrap().args);
        }
        self.stream.set_read_timeout(Some(timeout)).unwrap();
        let found = loop {
            match self.recv() {
                Ok(message) if message.method == method => break Some(message.args),
                Ok(message) => self.received.push_back(message),
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    break None;
                }
                Err(error) => panic!("the connection failed: {error}"),
            }
        };
        self.stream.set_read_timeout(Some(TIMEOUT)).unwrap();
        found
    }

    /// Tell if the server closed the connection within `timeout`. Read and
    /// keep the messages that come before the end.
    pub fn closed_within(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                self.stream.set_read_timeout(Some(TIMEOUT)).unwrap();
                return false;
            }
            self.stream.set_read_timeout(Some(left)).unwrap();
            match self.recv() {
                Ok(message) => self.received.push_back(message),
                Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return true,
                Err(error) if error.kind() == io::ErrorKind::ConnectionReset => return true,
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    self.stream.set_read_timeout(Some(TIMEOUT)).unwrap();
                    return false;
                }
                Err(error) => panic!("the connection failed: {error}"),
            }
        }
    }

    /// Close the connection at once, as a client that dies does.
    pub fn drop_now(self) {
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}

fn into_result(message: Message) -> Result<Value, Fault> {
    if message.is_error() {
        let Value::Array(mut parts) = message.args else {
            panic!("an error reply is not [class, args]: {:?}", message.args);
        };
        let args = parts.pop().unwrap();
        let class = parts.pop().unwrap().as_str().unwrap().to_string();
        Err(Fault { class, args })
    } else {
        Ok(message.args)
    }
}

/// An oid or a tid as the protocol sends it: 8 bytes, big-endian.
pub fn p64(n: u64) -> Value {
    Value::Binary(n.to_be_bytes().to_vec())
}

/// The number in 8 big-endian bytes.
pub fn u64_of(value: &Value) -> u64 {
    let bytes: [u8; 8] = bytes_of(value).try_into().unwrap();
    u64::from_be_bytes(bytes)
}

/// The bytes of a msgpack bin value.
pub fn bytes_of(value: &Value) -> Vec<u8> {
    match value {
        Value::Binary(bytes) => bytes.clone(),
        other => panic!("not bytes: {other:?}"),
    }
}

/// The items of a msgpack array.
pub fn items(value: &Value) -> Vec<Value> {
    match value {
        Value::Array(items) => items.clone(),
        other => panic!("not an array: {other:?}"),
    }
}

/// A ZODB record of a `PersistentMapping` whose state holds `value` under
/// the key `v`.
pub fn record(value: &[u8]) -> Vec<u8> {
    assert!(value.len() < 256);
    let mut pickle = b"\x80\x03cpersistent.mapping\nPersistentMapping\nq\x00.".to_vec();
    pickle.extend_from_slice(b"\x80\x03}q\x00X\x01\x00\x00\x00vq\x01C");
    pickle.push(value.len() as u8);
    pickle.extend_from_slice(value);
    pickle.extend_from_slice(b"q\x02s.");
    pickle
}

/// Begin the transaction `txn` as a stock client does: `tpc_begin` has no
/// reply.
pub fn begin(conn: &mut Conn, txn: i64) {
    conn.send_async(
        "tpc_begin",
        vec![
            Value::from(txn),
            Value::Binary(b"user".to_vec()),
            Value::Binary(b"description".to_vec()),
            Value::Map(vec![]),
            Value::Nil,
            Value::from(" "),
        ],
    );
}

/// Commit one transaction that stores each `(oid, serial, data)` of
/// `stores` through `conn`. Return the tid of the commit.
pub fn commit(conn: &mut Conn, txn: i64, stores: &[(u64, u64, Vec<u8>)]) -> Result<u64, Fault> {
    let id = Value::from(txn);
    begin(conn, txn);
    for (oid, serial, data) in stores {
        conn.send_async(
            "storea",
            vec![
                p64(*oid),
                p64(*serial),
                Value::Binary(data.clone()),
                id.clone(),
            ],
        );
    }
    conn.call("vote", vec![id.clone()])?;
    let tid = conn.call("tpc_finish", vec![id])?;
    Ok(u64_of(&tid))
}

/// Ask the server for `n` new oids.
pub fn new_oids(conn: &mut Conn, n: u64) -> Vec<u64> {
    let oids = conn.call("new_oids", vec![Value::from(n)]).unwrap();
    items(&oids).iter().map(u64_of).collect()
}

/// Load the current record of `oid`: its data and its serial, or the
/// error of the server.
pub fn load(conn: &mut Conn, oid: u64) -> Result<(Vec<u8>, u64), Fault> {
    let reply = conn.call("loadBefore", vec![p64(oid), Value::Binary(MAX.to_vec())])?;
    let parts = items(&reply);
    Ok((bytes_of(&parts[0]), u64_of(&parts[1])))
}
