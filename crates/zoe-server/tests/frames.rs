//! Frames outside the calls of `zoe serve`.
//!
//! A stock client sends a heartbeat every 60 s, busy or idle. The server
//! keeps the connection and its transaction, and sends no reply. A frame
//! that no client sends, as one nested too deep or one longer than
//! `--max-frame-size`, closes its connection. The server and its other
//! connections stay up. The calls that wait for a session make the reader
//! wait, and the connection stays open.

mod wire;

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use wire::{
    MAX, Server, TIMEOUT, Value, begin, bytes_of, commit, fresh_dir, items, load, new_oids, p64,
    record, s, u64_of,
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

/// The peak resident memory of the process `pid`, in KiB.
fn peak_kib(pid: u32) -> u64 {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
    status
        .lines()
        .find_map(|line| line.strip_prefix("VmHWM:"))
        .and_then(|value| value.split_whitespace().next())
        .and_then(|kib| kib.parse().ok())
        .unwrap()
}

#[test]
fn a_frame_of_many_small_values_closes_its_connection_and_takes_little_memory() {
    let dir = fresh_dir("many-values");
    let mut server = Server::path(&dir.join("store"));
    let mut other = server.client("1");
    let before = peak_kib(server.pid());

    // An array of 4 Mi nils: 4 MiB of frame, which rmpv would make into
    // more than 160 MiB of values.
    let mut nils = vec![0xdd];
    nils.extend_from_slice(&(4u32 << 20).to_be_bytes());
    nils.resize(nils.len() + (4 << 20), 0xc0);
    // An array of 60,000 arrays of 15 nils: each array announces few
    // values, and the frame holds 960,001.
    let mut arrays = vec![0xdd];
    arrays.extend_from_slice(&60_000u32.to_be_bytes());
    for _ in 0..60_000 {
        arrays.push(0x9f);
        arrays.extend_from_slice(&[0xc0; 15]);
    }
    for frame in [nils, arrays] {
        let mut conn = server.connect();
        conn.write_frame(&frame).unwrap();
        assert!(conn.closed_within(TIMEOUT));
    }

    other.call("lastTransaction", vec![]).unwrap();
    assert!(server.running());
    let after = peak_kib(server.pid());
    assert!(
        after < before + 32 * 1024,
        "the peak resident memory grew from {before} KiB to {after} KiB"
    );
}

/// The frame of the call `id` of `method` with `args`.
fn call_frame(id: u32, method: &str, args: Vec<Value>) -> Vec<u8> {
    let message = Value::Array(vec![
        Value::from(id),
        Value::Boolean(false),
        Value::from(method),
        Value::Array(args),
    ]);
    let mut payload = Vec::new();
    rmpv::encode::write_value(&mut payload, &message).unwrap();
    framed(&payload)
}

/// `payload` after its 4-byte big-endian length.
fn framed(payload: &[u8]) -> Vec<u8> {
    let mut frame = u32::try_from(payload.len()).unwrap().to_be_bytes().to_vec();
    frame.extend_from_slice(payload);
    frame
}

/// The frame of the call `id` of `ping` with `chains` arguments. Each
/// argument is 55 arrays of one element, one in the other, around a nil:
/// 56 values in 56 bytes, which rmpv makes into about 10 KB. The frame
/// stays under both bounds of the decode.
fn costly_ping(id: u32, chains: u16) -> Vec<u8> {
    let mut payload = vec![0x94, 0xce];
    payload.extend_from_slice(&id.to_be_bytes());
    payload.extend_from_slice(&[0xc2, 0xa4, b'p', b'i', b'n', b'g', 0xdc]);
    payload.extend_from_slice(&chains.to_be_bytes());
    for _ in 0..chains {
        payload.extend_from_slice(&[0x91; 55]);
        payload.push(0xc0);
    }
    framed(&payload)
}

#[test]
fn the_calls_that_wait_for_a_session_hold_little_memory_and_keep_the_connection() {
    const PINGS: u32 = 64;
    let dir = fresh_dir("waiting-session");
    let store = dir.join("store");
    let mut server = Server::start(&["--path", s(&store), "--lock-timeout", "120"], &[]);
    let pid = server.pid();

    // The vote of `a` locks the object, so the vote of `b` waits, and the
    // session of `b` runs no other call.
    let mut a = server.client("1");
    let oid = new_oids(&mut a, 1)[0];
    let tid = commit(&mut a, 1, &[(oid, 0, record(b"first"))]).unwrap();
    begin(&mut a, 2);
    a.send_async(
        "storea",
        vec![
            p64(oid),
            p64(tid),
            Value::Binary(record(b"a")),
            Value::from(2),
        ],
    );
    a.call("vote", vec![Value::from(2)]).unwrap();
    let mut b = server.client("1");
    begin(&mut b, 3);
    b.send_async(
        "storea",
        vec![
            p64(oid),
            p64(tid),
            Value::Binary(record(b"b")),
            Value::from(3),
        ],
    );
    let vote = b.send_call("vote", vec![Value::from(3)]);
    let before = peak_kib(pid);

    // Each frame costs about 4.4 MB once decoded: without a bound on the
    // calls that wait, the 64 frames take about 280 MB.
    let written = Arc::new(AtomicUsize::new(0));
    let writer = {
        let mut stream = b.stream().try_clone().unwrap();
        let written = Arc::clone(&written);
        thread::spawn(move || {
            for n in 0..PINGS {
                stream.write_all(&costly_ping(1_000 + n, 446)).unwrap();
                written.fetch_add(1, Ordering::SeqCst);
            }
        })
    };
    // Wait until the writer and the peak memory of the server stay still
    // for 500 ms: the server reads no more frames.
    let deadline = Instant::now() + TIMEOUT;
    let (mut last, mut still) = ((usize::MAX, 0), 0);
    while still < 5 {
        assert!(Instant::now() < deadline, "the server did not settle");
        thread::sleep(Duration::from_millis(100));
        let now = (written.load(Ordering::SeqCst), peak_kib(pid));
        still = if now == last { still + 1 } else { 0 };
        last = now;
    }
    let waiting = peak_kib(pid);

    // The abort of `a` frees the session of `b`, which runs every call.
    a.call("tpc_abort", vec![Value::from(2)]).unwrap();
    writer.join().unwrap();
    b.reply_to(&vote).unwrap();
    for n in 0..PINGS {
        // `ping` takes no argument: each call gets an error, not a close.
        assert!(b.reply_to(&Value::from(1_000 + n)).is_err());
    }
    b.call("tpc_abort", vec![Value::from(3)]).unwrap();
    b.call("lastTransaction", vec![]).unwrap();
    assert!(server.running());
    let after = peak_kib(pid);
    for peak in [waiting, after] {
        assert!(
            peak < before + 64 * 1024,
            "the peak resident memory grew from {before} KiB to {peak} KiB"
        );
    }
}

#[test]
fn a_client_that_reads_while_it_sends_gets_every_reply_when_the_queues_are_full() {
    const LOADS: u32 = 300;
    let dir = fresh_dir("pipeline");
    let store = dir.join("store");
    // A session queue of 2 replies. A commit puts 2 frames in the queue of
    // the hub of its client, so 2 is the least that lets `other` commit.
    let server = Server::start(&["--path", s(&store), "--client-queue-size", "2"], &[]);
    let mut other = server.client("1");
    let oid = new_oids(&mut other, 1)[0];
    let data = large_record(64 << 10);
    let tid = commit(&mut other, 1, &[(oid, 0, data.clone())]).unwrap();

    // One thread sends the loads, as the loop of a stock client does. The
    // replies wait 500 ms: they fill the buffers of TCP and the session
    // queue, so the session waits. Then the calls fill their queue, and
    // the reader waits.
    let mut conn = server.client("1");
    let writer = {
        let mut stream = conn.stream().try_clone().unwrap();
        thread::spawn(move || {
            for n in 0..LOADS {
                let args = vec![p64(oid), Value::Binary(MAX.to_vec())];
                stream
                    .write_all(&call_frame(1_000 + n, "loadBefore", args))
                    .unwrap();
            }
        })
    };
    thread::sleep(Duration::from_millis(500));
    for n in 0..LOADS {
        let reply = items(&conn.reply_to(&Value::from(1_000 + n)).unwrap());
        assert!(bytes_of(&reply[0]) == data);
        assert_eq!(u64_of(&reply[1]), tid);
    }
    writer.join().unwrap();
    conn.call("lastTransaction", vec![]).unwrap();
}
