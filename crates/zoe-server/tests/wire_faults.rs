//! Acceptance test of faults on the wire (harness E).
//!
//! A client that disconnects at any state of the two-phase commit leaves
//! no lock and nothing staged: the server aborts its transaction. A client
//! that disconnects after it sent `tpc_finish` leaves a transaction that
//! is committed whole or not at all. An upload of a blob that stops in the
//! middle leaves no file. A server killed during a vote commits nothing of
//! it. A client that does not read its messages is closed when more than
//! `--client-queue-size` messages (`ZOE_CLIENT_QUEUE_SIZE`, 1,000 by
//! default) wait for it, and the other clients do not notice.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

mod common;

use common::{
    Conn, MAX, Server, Value, begin, commit, fresh_dir, load, new_oids, p64, record, s, u64_of,
};

/// A commit through a fresh client that must not wait for a lock.
fn commit_promptly(server: &Server, oid: u64, serial: u64) -> u64 {
    let mut conn = server.client("1");
    let started = Instant::now();
    let tid = commit(&mut conn, 1, &[(oid, serial, record(b"after"))]).unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the commit waited for a lock of a dead client"
    );
    tid
}

/// The value of `zoe_clients_connected` on the admin port `admin`: the
/// connections that registered.
fn clients_connected(admin: SocketAddr) -> f64 {
    let mut stream = TcpStream::connect_timeout(&admin, Duration::from_secs(10)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    write!(
        stream,
        "GET /metrics HTTP/1.1\r\nHost: zoe\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    response
        .lines()
        .filter(|line| line.split('{').next() == Some("zoe_clients_connected"))
        .map(|line| line.rsplit_once(' ').unwrap().1.parse::<f64>().unwrap())
        .sum()
}

#[test]
fn a_disconnect_at_each_state_leaves_no_lock_and_nothing_staged() {
    let dir = fresh_dir("states");
    let server = Server::start(
        &["--path", s(&dir.join("store")), "--lock-timeout", "30"],
        &[],
    );
    let mut setup = server.client("1");
    let oid = new_oids(&mut setup, 1)[0];
    let mut serial = commit(&mut setup, 1, &[(oid, 0, record(b"first"))]).unwrap();

    for state in ["begun", "staged", "voted"] {
        let mut conn = server.client("1");
        begin(&mut conn, 9);
        if state != "begun" {
            conn.send_async(
                "storea",
                vec![
                    p64(oid),
                    p64(serial),
                    Value::Binary(record(b"lost")),
                    Value::from(9),
                ],
            );
        }
        if state == "voted" {
            conn.call("vote", vec![Value::from(9)]).unwrap();
        }
        conn.drop_now();

        // Nothing of the dead transaction is there, and its lock is free.
        let (data, current) = load(&mut setup, oid).unwrap();
        assert_ne!(data, record(b"lost"), "{state}");
        assert_eq!(current, serial, "{state}");
        serial = commit_promptly(&server, oid, serial);
    }
}

#[test]
fn a_disconnect_after_tpc_finish_commits_whole_or_not_at_all() {
    let dir = fresh_dir("finish");
    let server = Server::path(&dir.join("store"));
    let mut setup = server.client("1");
    let oids = new_oids(&mut setup, 3);
    let stores: Vec<_> = oids.iter().map(|oid| (*oid, 0, record(b"first"))).collect();
    let first = commit(&mut setup, 1, &stores).unwrap();

    let mut conn = server.client("1");
    begin(&mut conn, 5);
    for oid in &oids {
        conn.send_async(
            "storea",
            vec![
                p64(*oid),
                p64(first),
                Value::Binary(record(b"second")),
                Value::from(5),
            ],
        );
    }
    conn.call("vote", vec![Value::from(5)]).unwrap();
    conn.send_call("tpc_finish", vec![Value::from(5)]);
    conn.drop_now();

    // Wait until the finish is done or dropped, then check that the three
    // oids moved together.
    std::thread::sleep(Duration::from_millis(200));
    let serials: Vec<u64> = oids
        .iter()
        .map(|oid| load(&mut setup, *oid).unwrap().1)
        .collect();
    assert!(
        serials.iter().all(|serial| *serial == serials[0]),
        "{serials:?}"
    );
    commit_promptly(&server, oids[0], serials[0]);
}

#[test]
fn an_interrupted_blob_upload_leaves_no_file() {
    let dir = fresh_dir("blob");
    let path = dir.join("store");
    let server = Server::path(&path);
    let mut conn = server.client("1");
    begin(&mut conn, 3);
    conn.send_async("storeBlobStart", vec![]);
    for _ in 0..10 {
        conn.send_async("storeBlobChunk", vec![Value::Binary(vec![7u8; 59_000])]);
    }
    // The client dies before storeBlobEnd.
    let _ = conn.call("ping", vec![]);
    conn.drop_now();
    std::thread::sleep(Duration::from_millis(300));

    let leftovers: Vec<_> = walk(&path.join("blobs"))
        .into_iter()
        .filter(|file| !file.ends_with(".layout"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

fn walk(dir: &std::path::Path) -> Vec<String> {
    let mut files = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return files;
    };
    for entry in entries {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files.extend(walk(&path));
        } else {
            files.push(path.display().to_string());
        }
    }
    files
}

#[test]
fn a_server_killed_during_a_vote_commits_nothing_of_it() {
    let dir = fresh_dir("vote");
    let path = dir.join("store");
    let mut server = Server::start(&["--path", s(&path), "--lock-timeout", "30"], &[]);
    let mut holder = server.client("1");
    let mut voter = server.client("1");
    let oid = new_oids(&mut holder, 1)[0];
    let first = commit(&mut holder, 1, &[(oid, 0, record(b"first"))]).unwrap();

    // The holder keeps the lock, so the vote of the voter waits.
    begin(&mut holder, 2);
    holder.send_async(
        "storea",
        vec![
            p64(oid),
            p64(first),
            Value::Binary(record(b"held")),
            Value::from(2),
        ],
    );
    holder.call("vote", vec![Value::from(2)]).unwrap();
    begin(&mut voter, 3);
    voter.send_async(
        "storea",
        vec![
            p64(oid),
            p64(first),
            Value::Binary(record(b"voting")),
            Value::from(3),
        ],
    );
    voter.send_call("vote", vec![Value::from(3)]);
    std::thread::sleep(Duration::from_millis(200));

    server.kill();
    server.restart();
    let mut conn = server.client("1");
    assert_eq!(load(&mut conn, oid).unwrap(), (record(b"first"), first));
    commit_promptly(&server, oid, first);
}

#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "the socket buffers of macOS can hold every reply, so the hub queue does not always fill: Linux only"
)]
fn a_client_that_does_not_read_is_closed_and_the_others_go_on() {
    let dir = fresh_dir("slow");
    let server = Server::start(
        &[
            "--path",
            s(&dir.join("store")),
            "--client-queue-size",
            "10",
            "--admin-listen",
            "127.0.0.1:0",
        ],
        &[],
    );
    let admin = server.admin.expect("the server has no admin port");
    let mut writer = server.client("1");
    let mut reader = server.client("1");
    let mut slow = server.client("1");
    let connected = clients_connected(admin);

    // A large object fills the socket of the slow client fast.
    let big = new_oids(&mut writer, 1)[0];
    let large: Vec<u8> = (0..2_000_000u32).map(|n| n as u8).collect();
    commit(&mut writer, 1, &[(big, 0, large)]).unwrap();
    let _ = reader.wait_for("invalidateTransaction", Duration::from_secs(10));
    let _ = slow.wait_for("invalidateTransaction", Duration::from_secs(10));
    for _ in 0..20 {
        slow.send_call("loadBefore", vec![p64(big), Value::Binary(MAX.to_vec())]);
    }

    // The slow client reads nothing while commits go out. The hub queue of
    // the slow client fills only after a reply blocks its socket, and a
    // load can end after many commits. So the commits go on until the
    // server closes the slow client.
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut txn = 2;
    while clients_connected(admin) == connected {
        assert!(
            Instant::now() < deadline,
            "the slow client was not closed after {} commits",
            txn - 2
        );
        let oid = new_oids(&mut writer, 1)[0];
        let tid = commit(&mut writer, txn, &[(oid, 0, record(b"v"))]).unwrap();
        txn += 1;
        // The reader gets each invalidation, in order.
        let args = reader
            .wait_for("invalidateTransaction", Duration::from_secs(30))
            .expect("the reader missed an invalidation");
        assert_eq!(u64_of(&common::items(&args)[0]), tid);
    }
    // The slow client was closed.
    assert!(slow.closed_within(Duration::from_secs(60)));
    // The server goes on.
    let mut conn = Conn::open(server.addr);
    conn.register("1", false).unwrap();
}
