//! Tests of the transaction timeout of `zoe serve`.
//!
//! `--transaction-timeout` (`ZOE_TRANSACTION_TIMEOUT`, 0 by default) is the
//! key `transaction-timeout` of ZEO. A transaction holds its locks from a
//! vote that passed until it ends. When it stays open for longer than the
//! timeout after that vote, the server closes its connection once: the
//! transaction aborts, its locks go and it leaves nothing. A connection
//! without a voted transaction stays open, whatever its idle time. The
//! idle timeout keeps its own rule.

use std::io;
use std::time::{Duration, Instant};

mod wire;

use wire::{
    Conn, Server, TIMEOUT, Value, ZOE, begin, commit, fresh_dir, load, new_oids, p64, record, s,
    u64_of,
};

/// The heartbeat of ZEO 6.2: `[-1, 0, ".reply", nil]`.
const HEARTBEAT: [u8; 11] = [
    0x94, 0xff, 0x00, 0xa6, b'.', b'r', b'e', b'p', b'l', b'y', 0xc0,
];

/// Begin the transaction `txn` on `conn`, store `data` over `serial` for
/// `oid`, and vote. Return the reply of the vote.
fn store_and_vote(conn: &mut Conn, txn: i64, oid: u64, serial: u64, data: &[u8]) -> Value {
    begin(conn, txn);
    conn.send_async(
        "storea",
        vec![
            p64(oid),
            p64(serial),
            Value::Binary(record(data)),
            Value::from(txn),
        ],
    );
    conn.call("vote", vec![Value::from(txn)]).unwrap()
}

/// Send a heartbeat on `conn` every 300 ms until `until` passed. Fail if
/// the connection closes.
fn beat_until(conn: &mut Conn, until: Instant) {
    while Instant::now() < until {
        conn.write_frame(&HEARTBEAT).unwrap();
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// Return the value of `key` in the status of the storage of `conn`.
fn status(conn: &mut Conn, key: &str) -> Value {
    let Value::Map(entries) = conn.call("server_status", vec![]).unwrap() else {
        panic!("the status is not a map");
    };
    entries
        .into_iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .map(|(_, v)| v)
        .unwrap_or_else(|| panic!("no {key} in the status"))
}

#[test]
fn the_help_names_the_timeout_and_its_twin() {
    let output = {
        let _spawn = wire::spawn_lock();
        std::process::Command::new(ZOE)
            .args(["serve", "--help"])
            .output()
            .unwrap()
    };
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(help.contains("--transaction-timeout"));
    assert!(help.contains("ZOE_TRANSACTION_TIMEOUT"));
}

#[test]
fn a_voted_transaction_that_stalls_is_closed_once_and_its_locks_go() {
    let dir = fresh_dir("txn-timeout-stall");
    let store = dir.join("store");
    let server = Server::start(&["--path", s(&store), "--transaction-timeout", "2"], &[]);
    let mut setup = server.client("1");
    let oid = new_oids(&mut setup, 1)[0];
    let first = commit(&mut setup, 1, &[(oid, 0, record(b"first"))]).unwrap();

    // The holder votes, then sends nothing more.
    let mut holder = server.client("1");
    store_and_vote(&mut holder, 2, oid, first, b"stalled");
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
    waiter.stream().set_read_timeout(Some(TIMEOUT)).unwrap();
    let reply = waiter.reply_to(&vote);
    let waited = stalled.elapsed();
    assert!(reply.is_ok(), "{reply:?}");
    assert!(
        waited >= Duration::from_millis(1500),
        "the lock came after {waited:?}"
    );
    assert!(
        waited < Duration::from_secs(8),
        "the lock came after {waited:?}"
    );
    assert!(holder.closed_within(Duration::from_secs(5)));
    let tid = u64_of(&waiter.call("tpc_finish", vec![Value::from(3)]).unwrap());

    // The stalled transaction left nothing.
    assert_eq!(load(&mut waiter, oid).unwrap(), (record(b"waiter"), tid));

    // The client of the holder connects again and stays idle for more than
    // twice the timeout: the server closes it no more.
    let mut again = server.client("1");
    assert!(!again.closed_within(Duration::from_secs(5)));
    again.call("lastTransaction", vec![]).unwrap();
}

#[test]
fn a_connection_without_a_voted_transaction_stays_open() {
    let dir = fresh_dir("txn-timeout-idle");
    let store = dir.join("store");
    // The twin sets the timeout, with the syntax of `--lock-timeout`.
    let server = Server::start(&["--path", s(&store)], &[("ZOE_TRANSACTION_TIMEOUT", "1s")]);
    let mut idle = server.client("1");
    let mut staging = server.client("1");
    let oid = new_oids(&mut staging, 1)[0];
    begin(&mut staging, 1);
    staging.send_async(
        "storea",
        vec![
            p64(oid),
            p64(0),
            Value::Binary(record(b"staged")),
            Value::from(1),
        ],
    );
    // Three times the timeout without a frame.
    assert!(!idle.closed_within(Duration::from_secs(3)));
    assert!(!staging.closed_within(Duration::from_millis(100)));
    idle.call("lastTransaction", vec![]).unwrap();

    // The transaction that did not vote yet commits.
    staging.call("vote", vec![Value::from(1)]).unwrap();
    let tid = u64_of(&staging.call("tpc_finish", vec![Value::from(1)]).unwrap());
    assert_eq!(load(&mut staging, oid).unwrap(), (record(b"staged"), tid));
    // The status says that a timeout thread runs, as ZEO's server with a
    // transaction timeout says.
    assert_eq!(
        status(&mut staging, "timeout-thread-is-alive"),
        Value::Boolean(true)
    );
}

#[test]
fn a_finish_within_the_timeout_commits_and_keeps_the_connection() {
    let dir = fresh_dir("txn-timeout-finish");
    let store = dir.join("store");
    let server = Server::start(&["--path", s(&store), "--transaction-timeout", "3"], &[]);
    let mut conn = server.client("1");
    let oid = new_oids(&mut conn, 1)[0];
    let mut serial = 0;
    for round in 1..=3 {
        let data = format!("round {round}");
        store_and_vote(&mut conn, round, oid, serial, data.as_bytes());
        std::thread::sleep(Duration::from_secs(1));
        serial = u64_of(&conn.call("tpc_finish", vec![Value::from(round)]).unwrap());
    }
    // Three voted transactions held their locks for 3 s in all, each for
    // less than the timeout.
    assert!(!conn.closed_within(Duration::from_secs(4)));
    assert_eq!(load(&mut conn, oid).unwrap(), (record(b"round 3"), serial));
}

#[test]
fn a_second_vote_does_not_start_the_time_again() {
    let dir = fresh_dir("txn-timeout-second-vote");
    let store = dir.join("store");
    let server = Server::start(&["--path", s(&store), "--transaction-timeout", "3"], &[]);
    let mut setup = server.client("1");
    let oid = new_oids(&mut setup, 1)[0];
    let first = commit(&mut setup, 1, &[(oid, 0, record(b"first"))]).unwrap();
    let second = commit(&mut setup, 2, &[(oid, first, record(b"second"))]).unwrap();

    // The first vote lists the conflict of the stale serial for the client
    // to resolve. The transaction keeps its locks, and the time starts.
    let mut conn = server.client("1");
    let reply = store_and_vote(&mut conn, 3, oid, first, b"stale");
    let first_vote = Instant::now();
    let listed = wire::items(&reply).into_iter().any(|item| {
        item.as_map().is_some_and(|map| {
            map.iter()
                .any(|(k, v)| k.as_str() == Some("oid") && *v == p64(oid))
        })
    });
    assert!(
        listed,
        "the first vote did not list the conflict: {reply:?}"
    );

    // The client resolves, and votes again 2 s later.
    std::thread::sleep(Duration::from_secs(2));
    conn.send_async(
        "storea",
        vec![
            p64(oid),
            p64(second),
            Value::Binary(record(b"resolved")),
            Value::from(3),
        ],
    );
    conn.call("vote", vec![Value::from(3)]).unwrap();

    // The close comes 3 s after the first vote, not 3 s after the second.
    assert!(conn.closed_within(Duration::from_secs(5)));
    let closed = first_vote.elapsed();
    assert!(
        closed >= Duration::from_millis(2500) && closed < Duration::from_millis(4500),
        "closed after {closed:?}"
    );
    // The transaction aborted: the record is the one of the second commit.
    assert_eq!(load(&mut setup, oid).unwrap(), (record(b"second"), second));
}

#[test]
fn heartbeats_do_not_keep_a_voted_transaction_past_the_timeout() {
    let dir = fresh_dir("txn-timeout-beats");
    let store = dir.join("store");
    // The idle timeout stays at its default of 300 s.
    let server = Server::start(&["--path", s(&store), "--transaction-timeout", "2"], &[]);
    let mut conn = server.client("1");
    let oid = new_oids(&mut conn, 1)[0];
    store_and_vote(&mut conn, 1, oid, 0, b"beaten");
    let voted = Instant::now();
    // The client sends heartbeats, so it is not idle. The server still
    // closes it after the timeout.
    let closed = loop {
        assert!(voted.elapsed() < Duration::from_secs(10), "no close");
        match conn.write_frame(&HEARTBEAT) {
            Ok(()) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
                ) =>
            {
                break voted.elapsed();
            }
            Err(error) => panic!("the write failed: {error}"),
        }
        if conn.closed_within(Duration::from_millis(300)) {
            break voted.elapsed();
        }
    };
    assert!(
        closed >= Duration::from_millis(1500),
        "closed after {closed:?}"
    );
    // The transaction aborted: the oid has no record.
    let mut other = server.client("1");
    assert!(load(&mut other, oid).is_err());
}

#[test]
fn the_idle_timeout_still_closes_a_silent_connection_without_a_transaction() {
    let dir = fresh_dir("txn-timeout-with-idle");
    let store = dir.join("store");
    let server = Server::start(
        &[
            "--path",
            s(&store),
            "--transaction-timeout",
            "10",
            "--idle-timeout",
            "1",
        ],
        &[],
    );
    // The idle timeout closes a silent connection, as without the
    // transaction timeout.
    let mut silent = server.client("1");
    assert!(silent.closed_within(Duration::from_secs(5)));
    // The idle timeout also closes a silent voted transaction before the
    // longer transaction timeout.
    let mut holder = server.client("1");
    let oid = new_oids(&mut holder, 1)[0];
    store_and_vote(&mut holder, 1, oid, 0, b"silent");
    let voted = Instant::now();
    assert!(holder.closed_within(Duration::from_secs(5)));
    assert!(voted.elapsed() < Duration::from_secs(5));
    // A client that beats stays open past the idle timeout.
    let mut beating = server.client("1");
    beat_until(&mut beating, Instant::now() + Duration::from_secs(3));
    beating.call("lastTransaction", vec![]).unwrap();
}
