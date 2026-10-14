//! Acceptance test of the lingering close at a stop of `zoe serve` (N10).
//!
//! At a stop, the reader of a connection reads no more calls. A client
//! that shares one connection between threads can send bytes after the
//! stop: loads of other threads, or heartbeats. The server must not reset
//! such a connection. A reset makes the kernel drop the bytes that the
//! server did not send yet, so the client can lose the reply of a
//! `tpc_finish` whose commit is durable.
//!
//! The client votes, then sends `tpc_finish` and three loads of a large
//! record, and reads nothing. The server reads the four calls, commits,
//! and its replies fill the socket. Then the server gets SIGTERM, and the
//! client sends heartbeats, which the server does not read as calls. Then
//! the client reads slowly. It gets the three loads, `info` and the reply
//! of `tpc_finish`, then the end of the connection, never a reset. The
//! commit is durable, under the tid of the reply.

use std::io::{self, Read};
use std::process::Command;
use std::time::Duration;

mod wire;

use wire::{
    Conn, MAX, Server, Value, begin, bytes_of, commit, fresh_dir, load, new_oids, p64, record, s,
    u64_of,
};

/// The size of the large record. Three loads of it are far larger than
/// the buffers of the two sockets together.
const LARGE: usize = 8_000_000;

/// Read one frame in small pieces, with a pause after each piece, as a
/// slow client reads. So the socket of the server stays full while the
/// server sends its last frames.
fn read_frame_slowly(conn: &Conn) -> io::Result<Vec<u8>> {
    let mut stream = conn.stream();
    let mut len = [0u8; 4];
    stream.read_exact(&mut len)?;
    let mut bytes = vec![0u8; u32::from_be_bytes(len) as usize];
    for piece in bytes.chunks_mut(64 * 1024) {
        stream.read_exact(piece)?;
        std::thread::sleep(Duration::from_millis(2));
    }
    Ok(bytes)
}

/// Decode a message: its id, its flags, its method and its arguments.
fn decode(bytes: &[u8]) -> (Value, Value, String, Value) {
    let value = rmpv::decode::read_value(&mut &bytes[..]).unwrap();
    let Value::Array(mut parts) = value else {
        panic!("a message is not an array: {value:?}");
    };
    let args = parts.pop().unwrap();
    let method = parts.pop().unwrap().as_str().unwrap().to_owned();
    let flags = parts.pop().unwrap();
    let id = parts.pop().unwrap();
    (id, flags, method, args)
}

#[test]
fn a_stop_never_resets_a_connection_that_sent_bytes_after_it() {
    let dir = fresh_dir("lingering-close");
    let path = dir.join("store");
    let mut server = Server::start(&["--path", s(&path), "--shutdown-timeout", "20"], &[]);
    let mut writer = server.client("1");
    let large = new_oids(&mut writer, 1)[0];
    let data: Vec<u8> = (0..LARGE).map(|n| n as u8).collect();
    commit(&mut writer, 1, &[(large, 0, data)]).unwrap();
    drop(writer);

    let mut client = server.client("1");
    let oid = new_oids(&mut client, 1)[0];
    begin(&mut client, 2);
    let store = vec![
        p64(oid),
        p64(0),
        Value::Binary(record(b"committed")),
        Value::from(2),
    ];
    client.send_async("storea", store);
    client.call("vote", vec![Value::from(2)]).unwrap();
    let finish = client.send_call("tpc_finish", vec![Value::from(2)]);
    let loads: Vec<Value> = (0..3)
        .map(|_| client.send_call("loadBefore", vec![p64(large), Value::Binary(MAX.to_vec())]))
        .collect();
    // The server reads the four calls, commits, and fills the socket.
    std::thread::sleep(Duration::from_millis(500));

    let sent = Command::new("kill")
        .args(["-TERM", &server.pid().to_string()])
        .status()
        .unwrap();
    assert!(sent.success());
    // The connection reads no more calls after the stop. The client sends
    // bytes all the same, as another thread of a stock client does.
    std::thread::sleep(Duration::from_millis(300));
    for _ in 0..16 {
        client.send(Value::from(-1), Value::from(0), ".reply", vec![]);
    }

    let mut tid = None;
    let mut info = false;
    let mut loaded = Vec::new();
    let end = loop {
        match read_frame_slowly(&client) {
            Ok(bytes) => {
                let (id, flags, method, args) = decode(&bytes);
                match method.as_str() {
                    "info" => info = true,
                    ".reply" if id == finish => {
                        assert_eq!(flags, Value::from(0), "{args:?}");
                        tid = Some(u64_of(&args));
                    }
                    ".reply" if loads.contains(&id) => {
                        assert_eq!(flags, Value::from(0), "{args:?}");
                        let parts = args.as_array().unwrap();
                        assert_eq!(bytes_of(&parts[0]).len(), LARGE);
                        loaded.push(id);
                    }
                    other => panic!("an unexpected message: {other} {args:?}"),
                }
            }
            Err(error) => break error,
        }
    };
    assert_eq!(
        end.kind(),
        io::ErrorKind::UnexpectedEof,
        "the connection ended with {end} after {} loads, info {info}, finish {tid:?}",
        loaded.len()
    );
    assert_eq!(loaded.len(), 3, "a load reply is lost");
    assert!(info, "the info of the commit is lost");
    let tid = tid.expect("the reply of tpc_finish is lost");

    // The server stops once the connection is closed.
    for _ in 0..2_000 {
        if !server.running() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!server.running(), "zoe serve did not stop");
    server.kill();

    // The commit is durable, under the tid of the reply.
    server.restart();
    let mut reader = server.client("1");
    let (data, serial) = load(&mut reader, oid).unwrap();
    assert_eq!(data, record(b"committed"));
    assert_eq!(serial, tid);
}
