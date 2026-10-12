//! Frames outside the calls of `zoe serve`.
//!
//! A stock client sends a heartbeat every 60 s, busy or idle. The server
//! keeps the connection and its transaction, and sends no reply. A frame
//! that no client sends, as one nested too deep, closes its connection.
//! The server and its other connections stay up.

mod wire;

use wire::{Server, TIMEOUT, Value, begin, fresh_dir, load, new_oids, p64, record, u64_of};

/// The heartbeat of ZEO 6.2: `[-1, 0, ".reply", nil]`.
const HEARTBEAT: [u8; 11] = [
    0x94, 0xff, 0x00, 0xa6, b'.', b'r', b'e', b'p', b'l', b'y', 0xc0,
];

#[test]
fn a_heartbeat_keeps_the_connection_and_its_transaction() {
    let dir = fresh_dir("heartbeat");
    let server = Server::path(&dir.join("store"));
    let mut conn = server.client("1");
    conn.write_frame(&HEARTBEAT).unwrap();
    let oid = new_oids(&mut conn, 1)[0];

    let txn = Value::from(1);
    begin(&mut conn, 1);
    conn.send_async(
        "storea",
        vec![
            p64(oid),
            p64(0),
            Value::Binary(record(b"beat")),
            txn.clone(),
        ],
    );
    conn.write_frame(&HEARTBEAT).unwrap();
    conn.call("vote", vec![txn.clone()]).unwrap();
    let tid = u64_of(&conn.call("tpc_finish", vec![txn]).unwrap());

    assert_eq!(load(&mut conn, oid).unwrap(), (record(b"beat"), tid));
    // No message answers a heartbeat.
    assert!(conn.received.iter().all(|m| m.id != Value::from(-1)));
}

#[test]
fn a_frame_nested_too_deep_closes_its_connection_and_the_server_stays_up() {
    let dir = fresh_dir("deep-frame");
    let mut server = Server::path(&dir.join("store"));
    let mut other = server.client("1");

    let mut conn = server.connect();
    let mut deep = vec![0x91; 100_000];
    deep.push(0xc0);
    conn.write_frame(&deep).unwrap();
    assert!(conn.closed_within(TIMEOUT));

    other.call("lastTransaction", vec![]).unwrap();
    assert!(server.running());
}
