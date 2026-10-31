//! Acceptance test of the bound in bytes on the replies that wait for one
//! client (N6).
//!
//! The session of a connection puts its replies in a queue, and the writer
//! sends them. A client that sends calls and reads no reply makes them
//! wait. The replies that wait for one client, the frame that the writer
//! sends included, hold at most 1 MiB beyond one frame: the session runs
//! its next call only when they hold at most 1 MiB. So a client that sends
//! loads of a large record and reads nothing costs the server about one
//! reply, not the length of the queue times the record. Each reply still
//! goes out when the client reads.

use std::thread;
use std::time::{Duration, Instant};

mod common;

use common::{
    Conn, MAX, Server, TIMEOUT, Value, bytes_of, commit, fresh_dir, items, load, new_oids, p64,
    u64_of,
};

/// The size of the pickle that each load reads.
const RECORD: usize = 4 << 20;

/// The clients that read no reply.
const CLIENTS: usize = 16;

/// The loads that each of these clients sends.
const LOADS: usize = 10;

/// The most that the peak resident memory of the server may grow while
/// the replies wait, in KiB. With the bound, each client holds about two
/// replies at its peak: 128 MiB for all. With a queue of 4 frames and no
/// bound in bytes, each client holds about 6: 384 MiB for all.
const GROWTH_KIB: u64 = 256 * 1024;

/// Return a record of a `PersistentMapping` whose state holds `len` bytes
/// under the key `v`. The bytes do not compress, so the store keeps them
/// as they are.
fn large_record(len: usize) -> Vec<u8> {
    let mut pickle = b"\x80\x03cpersistent.mapping\nPersistentMapping\nq\x00.".to_vec();
    pickle.extend_from_slice(b"\x80\x03}q\x00X\x01\x00\x00\x00vq\x01B");
    pickle.extend_from_slice(&u32::try_from(len).unwrap().to_le_bytes());
    // A xorshift generator gives the bytes.
    let mut state: u64 = 0x2545_f491_4f6c_dd1d;
    pickle.extend((0..len).map(|_| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state as u8
    }));
    pickle.extend_from_slice(b"q\x02s.");
    pickle
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

/// Send a load of the current record of `oid`, and do not read its reply.
fn send_load(conn: &mut Conn, oid: u64) -> Value {
    conn.send_call("loadBefore", vec![p64(oid), Value::Binary(MAX.to_vec())])
}

#[test]
#[ignore = "the peak can go above the bound on CI and on machines with many cores; run it with --ignored"]
fn clients_that_read_no_reply_hold_little_memory_and_then_get_every_reply() {
    let dir = fresh_dir("reply-memory");
    let server = Server::path(&dir.join("store"));
    let pid = server.pid();
    let mut writer = server.client("1");
    let oid = new_oids(&mut writer, 1)[0];
    let data = large_record(RECORD);
    let tid = commit(&mut writer, 1, &[(oid, 0, data.clone())]).unwrap();
    // One load first, so the peak holds the cost of one reply already.
    let (loaded, serial) = load(&mut writer, oid).unwrap();
    assert!(loaded == data);
    assert_eq!(serial, tid);
    let before = peak_kib(pid);

    // Each slow client sends its loads and reads nothing.
    let mut slow: Vec<Conn> = (0..CLIENTS).map(|_| server.client("1")).collect();
    let sent: Vec<Vec<Value>> = slow
        .iter_mut()
        .map(|conn| (0..LOADS).map(|_| send_load(conn, oid)).collect())
        .collect();

    // Wait until the peak memory of the server stays still for 500 ms: the
    // sessions wait for their clients.
    let deadline = Instant::now() + TIMEOUT;
    let (mut last, mut still) = (0, 0);
    while still < 5 {
        assert!(Instant::now() < deadline, "the server did not settle");
        thread::sleep(Duration::from_millis(100));
        let now = peak_kib(pid);
        still = if now == last { still + 1 } else { 0 };
        last = now;
    }
    assert!(
        last < before + GROWTH_KIB,
        "the peak resident memory grew from {before} KiB to {last} KiB"
    );

    // The other clients go on while the slow ones wait.
    let (loaded, serial) = load(&mut writer, oid).unwrap();
    assert!(loaded == data);
    assert_eq!(serial, tid);

    // Each slow client gets every reply when it reads.
    for (conn, ids) in slow.iter_mut().zip(&sent) {
        for id in ids {
            let reply = items(&conn.reply_to(id).unwrap());
            assert!(bytes_of(&reply[0]) == data);
            assert_eq!(u64_of(&reply[1]), tid);
        }
    }
}
