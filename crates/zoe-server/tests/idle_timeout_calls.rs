//! Acceptance test of the idle timeout of `zoe serve` while a call runs.
//!
//! A client that waits for the reply of its own call is not idle. The idle
//! timer pauses while a call of the connection runs. It counts again from
//! the reply of the call, or from the last frame that the server read,
//! whichever is later. So a call that runs longer than `--idle-timeout`
//! keeps its connection, also when the client sends no heartbeat. A client
//! that then sends nothing more is closed after the timeout.

use std::io;
use std::time::{Duration, Instant};

mod wire;

use wire::{
    Conn, Server, TIMEOUT, Value, begin, commit, fresh_dir, load, new_oids, p64, record, s,
};

/// The heartbeat of ZEO 6.2: `[-1, 0, ".reply", nil]`.
const HEARTBEAT: [u8; 11] = [
    0x94, 0xff, 0x00, 0xa6, b'.', b'r', b'e', b'p', b'l', b'y', 0xc0,
];

/// The time between two heartbeats of the holder.
const BEAT: Duration = Duration::from_millis(500);

/// Wait at most `limit` for the reply to the call `id`, and send nothing
/// meanwhile. Fail when the server closes the connection first.
fn reply_silently(conn: &mut Conn, id: &Value, limit: Duration) -> wire::Message {
    let started = Instant::now();
    conn.stream().set_read_timeout(Some(limit)).unwrap();
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
                    io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset
                ) =>
            {
                panic!(
                    "the server closed the connection after {:?}, while the client waited for its reply",
                    started.elapsed()
                );
            }
            Err(error) => panic!("no reply within {limit:?}: {error}"),
        }
    }
}

#[test]
fn a_client_that_waits_for_the_reply_of_its_call_is_not_idle() {
    let dir = fresh_dir("idle-calls");
    let store = dir.join("store");
    let server = Server::start(
        &[
            "--path",
            s(&store),
            "--idle-timeout",
            "2",
            "--lock-timeout",
            "30",
        ],
        &[],
    );
    let mut setup = server.client("1");
    let oid = new_oids(&mut setup, 1)[0];
    let first = commit(&mut setup, 1, &[(oid, 0, record(b"first"))]).unwrap();

    // The holder votes and keeps the lock of the oid. It sends heartbeats,
    // so the server keeps its connection.
    let mut holder = server.client("1");
    begin(&mut holder, 2);
    holder.send_async(
        "storea",
        vec![
            p64(oid),
            p64(first),
            Value::Binary(record(b"holder")),
            Value::from(2),
        ],
    );
    holder.call("vote", vec![Value::from(2)]).unwrap();

    // The vote of the waiter waits for the lock. The waiter sends nothing
    // while it waits.
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
    let voted = Instant::now();

    // The holder keeps the lock for 5 s: more than twice the timeout. Then
    // it aborts, and the vote of the waiter gets the lock.
    for _ in 0..10 {
        std::thread::sleep(BEAT);
        holder.write_frame(&HEARTBEAT).unwrap();
    }
    holder.call("tpc_abort", vec![Value::from(2)]).unwrap();

    let reply = reply_silently(&mut waiter, &vote, Duration::from_secs(20));
    let waited = voted.elapsed();
    assert!(!reply.is_error(), "{reply:?}");
    assert!(
        waited >= Duration::from_secs(4),
        "the vote got the lock after {waited:?}"
    );

    // The timer counts again from the reply, not from the frame of the
    // vote. So the waiter can wait for half the timeout before its finish.
    std::thread::sleep(Duration::from_secs(1));
    let finish = waiter.send_call("tpc_finish", vec![Value::from(3)]);
    let reply = reply_silently(&mut waiter, &finish, Duration::from_secs(20));
    assert!(!reply.is_error(), "{reply:?}");
    let tid = wire::u64_of(&reply.args);

    // Then the waiter sends nothing more, and the server closes it after
    // the timeout.
    assert!(!waiter.closed_within(Duration::from_secs(1)));
    assert!(waiter.closed_within(Duration::from_secs(10)));

    // The commit of the waiter is durable, and the server stays up.
    let mut reader = server.client("1");
    assert_eq!(load(&mut reader, oid).unwrap(), (record(b"waiter"), tid));
}
