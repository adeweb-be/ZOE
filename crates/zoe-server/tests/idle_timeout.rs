//! Acceptance test of the idle timeout of `zoe serve`.
//!
//! The server closes a connection that sends no frame for `--idle-timeout`
//! seconds (`ZOE_IDLE_TIMEOUT`, 300 by default), with the syntax of
//! `--lock-timeout`. The timeout covers the whole connection, from the
//! protocol of the client on. The close ends the connection as a close of
//! the client does: its open transaction aborts and its locks go. So a
//! client that stalls after its vote, or a host that dies without a reset,
//! holds its locks at most this long. A heartbeat is a frame: a stock
//! client sends one every 60 s, busy or idle, so it keeps its connection.

use std::io;
use std::time::{Duration, Instant};

mod common;

use common::{
    Conn, Server, TIMEOUT, Value, ZOE, begin, commit, fresh_dir, load, new_oids, p64, record, s,
    u64_of,
};

/// The heartbeat of ZEO 6.2: `[-1, 0, ".reply", nil]`.
const HEARTBEAT: [u8; 11] = [
    0x94, 0xff, 0x00, 0xa6, b'.', b'r', b'e', b'p', b'l', b'y', 0xc0,
];

/// The time between two heartbeats of these tests.
const BEAT: Duration = Duration::from_millis(500);

/// Wait at most `limit` for the reply to the call `id`, and send a
/// heartbeat every `BEAT` meanwhile, as a stock client does every 60 s.
fn reply_with_heartbeats(conn: &mut Conn, id: &Value, limit: Duration) -> common::Message {
    let started = Instant::now();
    conn.stream().set_read_timeout(Some(BEAT)).unwrap();
    loop {
        assert!(started.elapsed() < limit, "no reply within {limit:?}");
        match conn.recv() {
            Ok(message) if message.method == ".reply" && &message.id == id => {
                conn.stream().set_read_timeout(Some(TIMEOUT)).unwrap();
                return message;
            }
            Ok(message) => conn.received.push_back(message),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                conn.write_frame(&HEARTBEAT).unwrap();
            }
            Err(error) => panic!("the connection failed: {error}"),
        }
    }
}

#[test]
fn the_help_names_the_timeout_and_its_twin() {
    let output = {
        let _spawn = common::spawn_lock();
        std::process::Command::new(ZOE)
            .args(["serve", "--help"])
            .output()
            .unwrap()
    };
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(help.contains("--idle-timeout"));
    assert!(help.contains("ZOE_IDLE_TIMEOUT"));
}

#[test]
fn a_client_that_stalls_after_its_vote_loses_its_locks_after_the_timeout() {
    let dir = fresh_dir("stall");
    let store = dir.join("store");
    let server = Server::start(&["--path", s(&store), "--idle-timeout", "2"], &[]);
    let mut setup = server.client("1");
    let oid = new_oids(&mut setup, 1)[0];
    let first = commit(&mut setup, 1, &[(oid, 0, record(b"first"))]).unwrap();

    // The holder votes, then sends nothing more.
    let mut holder = server.client("1");
    begin(&mut holder, 2);
    holder.send_async(
        "storea",
        vec![
            p64(oid),
            p64(first),
            Value::Binary(record(b"stalled")),
            Value::from(2),
        ],
    );
    holder.call("vote", vec![Value::from(2)]).unwrap();
    let stalled = Instant::now();

    // A vote on the same oid waits for the lock, and gets it when the
    // server closes the holder: long before the lock timeout of 10 s.
    let mut waiter = server.client("1");
    begin(&mut waiter, 3);
    waiter.send_async(
        "storea",
        vec![
            p64(oid),
            p64(first),
            Value::Binary(record(b"waiter")),
            Value::from(3),
        ],
    );
    let vote = waiter.send_call("vote", vec![Value::from(3)]);
    let reply = reply_with_heartbeats(&mut waiter, &vote, Duration::from_secs(20));
    let waited = stalled.elapsed();
    assert!(!reply.is_error(), "{reply:?}");
    assert!(
        waited >= Duration::from_secs(1),
        "the lock came after {waited:?}"
    );
    assert!(
        waited < Duration::from_secs(8),
        "the lock came after {waited:?}"
    );

    let tid = u64_of(&waiter.call("tpc_finish", vec![Value::from(3)]).unwrap());
    assert!(holder.closed_within(Duration::from_secs(5)));
    assert_eq!(load(&mut waiter, oid).unwrap(), (record(b"waiter"), tid));
}

#[test]
fn heartbeats_keep_a_connection_and_its_transaction() {
    let dir = fresh_dir("heartbeats");
    let store = dir.join("store");
    let server = Server::start(&["--path", s(&store), "--idle-timeout", "2"], &[]);
    let mut conn = server.client("1");
    let oid = new_oids(&mut conn, 1)[0];
    begin(&mut conn, 1);
    conn.send_async(
        "storea",
        vec![
            p64(oid),
            p64(0),
            Value::Binary(record(b"kept")),
            Value::from(1),
        ],
    );
    // Heartbeats alone for 6 s: three times the timeout.
    for _ in 0..12 {
        std::thread::sleep(BEAT);
        conn.write_frame(&HEARTBEAT).unwrap();
    }
    conn.call("vote", vec![Value::from(1)]).unwrap();
    let tid = u64_of(&conn.call("tpc_finish", vec![Value::from(1)]).unwrap());
    assert_eq!(load(&mut conn, oid).unwrap(), (record(b"kept"), tid));
}

#[test]
fn a_connection_that_sends_nothing_is_closed_from_the_protocol_on() {
    let dir = fresh_dir("silent");
    let store = dir.join("store");
    // The twin sets the timeout, with the syntax of `--lock-timeout`.
    let server = Server::start(&["--path", s(&store)], &[("ZOE_IDLE_TIMEOUT", "2s")]);
    let mut unanswered = Conn::open_silent(server.addr);
    let mut unregistered = server.connect();
    let mut registered = server.client("1");
    for conn in [&mut unanswered, &mut unregistered, &mut registered] {
        assert!(conn.closed_within(Duration::from_secs(10)));
    }
    // The server stays up.
    let mut conn = server.client("1");
    conn.call("lastTransaction", vec![]).unwrap();
}

#[test]
fn the_default_timeout_keeps_a_connection_that_is_idle_for_seconds() {
    let dir = fresh_dir("default");
    let server = Server::path(&dir.join("store"));
    let mut conn = server.client("1");
    assert!(!conn.closed_within(Duration::from_secs(3)));
    conn.call("lastTransaction", vec![]).unwrap();
}
