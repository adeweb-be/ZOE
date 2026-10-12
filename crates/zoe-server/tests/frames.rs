//! Frames outside the calls of `zoe serve`.
//!
//! A stock client sends a heartbeat every 60 s, busy or idle. The server
//! keeps the connection and its transaction, and sends no reply. A frame
//! that no client sends, as one nested too deep or one longer than
//! `--max-frame-size`, closes its connection. The server and its other
//! connections stay up.

mod wire;

use wire::{
    Server, TIMEOUT, Value, begin, commit, fresh_dir, load, new_oids, p64, record, s, u64_of,
};

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

/// Return the payload of the call `set_client_label` with the id `id`, of
/// exactly `len` bytes. `len` is from 56 to 279.
fn label_call(id: i64, len: usize) -> Vec<u8> {
    for size in 0..len {
        let call = Value::Array(vec![
            Value::from(id),
            Value::Boolean(false),
            Value::from("set_client_label"),
            Value::Array(vec![Value::from("x".repeat(size))]),
        ]);
        let mut bytes = Vec::new();
        rmpv::encode::write_value(&mut bytes, &call).unwrap();
        if bytes.len() == len {
            return bytes;
        }
    }
    panic!("no call of {len} bytes");
}

#[test]
fn a_frame_longer_than_the_limit_closes_its_connection_and_the_server_stays_up() {
    let dir = fresh_dir("frame-limit");
    let store = dir.join("store");
    // The twin sets the limit.
    let mut server = Server::start(&["--path", s(&store)], &[("ZOE_MAX_FRAME_SIZE", "200")]);
    let mut other = server.client("1");
    let mut conn = server.client("1");
    // A frame of the limit passes.
    conn.write_frame(&label_call(100, 200)).unwrap();
    assert_eq!(conn.reply_to(&Value::from(100)), Ok(Value::Nil));
    // A frame one byte longer closes its connection.
    conn.write_frame(&label_call(101, 201)).unwrap();
    assert!(conn.closed_within(TIMEOUT));
    other.call("lastTransaction", vec![]).unwrap();
    assert!(server.running());
    drop(server);

    // The option on the command line wins over its twin.
    let server = Server::start(
        &["--path", s(&store), "--max-frame-size", "250"],
        &[("ZOE_MAX_FRAME_SIZE", "200")],
    );
    let mut conn = server.client("1");
    conn.write_frame(&label_call(100, 250)).unwrap();
    assert_eq!(conn.reply_to(&Value::from(100)), Ok(Value::Nil));
}

/// Return a record of a `PersistentMapping` whose state holds `len` bytes
/// under the key `v`.
fn large_record(len: usize) -> Vec<u8> {
    let mut pickle = b"\x80\x03cpersistent.mapping\nPersistentMapping\nq\x00.".to_vec();
    pickle.extend_from_slice(b"\x80\x03}q\x00X\x01\x00\x00\x00vq\x01B");
    pickle.extend_from_slice(&u32::try_from(len).unwrap().to_le_bytes());
    pickle.extend((0..len).map(|n| n as u8));
    pickle.extend_from_slice(b"q\x02s.");
    pickle
}

#[test]
fn the_default_limit_takes_a_record_larger_than_zodb_warns_about() {
    // ZODB warns about a record of more than 16 MiB. The default limit of
    // a frame is 256 MiB.
    let dir = fresh_dir("large-record");
    let server = Server::path(&dir.join("store"));
    let mut conn = server.client("1");
    let oid = new_oids(&mut conn, 1)[0];
    let data = large_record(20 << 20);
    let tid = commit(&mut conn, 1, &[(oid, 0, data.clone())]).unwrap();
    assert!(load(&mut conn, oid).unwrap() == (data, tid));
}
