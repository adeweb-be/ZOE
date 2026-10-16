//! Clients that send calls and read nothing hold few threads of the server,
//! so they never stop the other clients.
//!
//! The reader of such a connection waits for room for its next call, so
//! the idle timeout does not close it. Its calls must not hold threads of
//! the read pool or many threads of the blocking pool while it reads
//! nothing.

use std::io::Write;
use std::time::{Duration, Instant};

mod wire;

use wire::{Conn, MAX, Server, Value, begin, commit, fresh_dir, new_oids, p64, record, s, u64_of};

/// The frame of the call `id` of `method` with `args`.
fn frame(id: i64, method: &str, args: Vec<Value>) -> Vec<u8> {
    let call = Value::Array(vec![
        Value::from(id),
        Value::Boolean(false),
        Value::from(method),
        Value::Array(args),
    ]);
    let mut message = Vec::new();
    rmpv::encode::write_value(&mut message, &call).unwrap();
    let mut frame = u32::try_from(message.len()).unwrap().to_be_bytes().to_vec();
    frame.extend_from_slice(&message);
    frame
}

/// The number of threads of the process `pid`.
fn threads(pid: u32) -> u64 {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
    status
        .lines()
        .find_map(|line| line.strip_prefix("Threads:"))
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

/// Check that a new client registers and loads `oid` within a few seconds.
fn a_new_client_registers_and_loads(server: &Server, oid: u64) {
    let mut conn = server.connect();
    conn.stream()
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let id = conn.send_call("register", vec![Value::from("1"), Value::Boolean(false)]);
    let reply = conn.recv();
    assert!(
        matches!(&reply, Ok(message) if message.id == id && !message.is_error()),
        "no reply to register: {reply:?}"
    );
    let load = conn.call("loadBefore", vec![p64(oid), Value::Binary(MAX.to_vec())]);
    assert!(load.is_ok(), "{load:?}");
}

#[test]
fn failed_loads_of_clients_that_read_nothing_hold_no_thread_of_the_read_pool() {
    let dir = fresh_dir("failed-loads");
    let path = dir.join("store");
    let server = Server::start(&["--path", s(&path), "--blocking-threads", "4"], &[]);
    let mut writer = server.client("1");
    let oid = new_oids(&mut writer, 1)[0];
    commit(&mut writer, 1, &[(oid, 0, vec![7; 16 << 20])]).unwrap();

    // Each client loads the large record 4 times: the replies fill the
    // buffers of its socket. Then it loads an object that does not exist,
    // and each of these loads fails. The client reads nothing.
    let mut clients: Vec<Conn> = Vec::new();
    for _ in 0..6 {
        let conn = server.client("1");
        conn.stream()
            .set_write_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut stream = conn.stream();
        for id in 10..14 {
            let args = vec![p64(oid), Value::Binary(MAX.to_vec())];
            stream.write_all(&frame(id, "loadBefore", args)).unwrap();
        }
        for id in 100..110 {
            let args = vec![p64(0xdead_beef), Value::Binary(MAX.to_vec())];
            if stream.write_all(&frame(id, "loadBefore", args)).is_err() {
                break;
            }
        }
        clients.push(conn);
    }
    std::thread::sleep(Duration::from_secs(2));

    a_new_client_registers_and_loads(&server, oid);
    drop(clients);
}

#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "reads the threads of the server in /proc: Linux only"
)]
fn downloads_of_clients_that_read_nothing_hold_few_threads() {
    const CLIENTS: u64 = 10;
    let dir = fresh_dir("downloads");
    let server = Server::path(&dir.join("store"));
    let mut writer = server.client("1");
    let oid = new_oids(&mut writer, 1)[0];
    begin(&mut writer, 1);
    let data = record(b"blob");
    let store = vec![p64(oid), p64(0), Value::Binary(data), Value::from(1)];
    writer.send_async("storea", store.clone());
    writer.send_async("storeBlobStart", vec![]);
    writer.send_async("storeBlobChunk", vec![Value::Binary(vec![7; 1 << 20])]);
    writer.send_async("storeBlobEnd", store);
    writer.call("vote", vec![Value::from(1)]).unwrap();
    let tid = u64_of(&writer.call("tpc_finish", vec![Value::from(1)]).unwrap());
    let before = threads(server.pid());

    // Each client sends many downloads of the blob of 1 MiB and reads
    // nothing.
    let download = frame(5, "sendBlob", vec![p64(oid), p64(tid)]);
    let mut clients: Vec<Conn> = Vec::new();
    for _ in 0..CLIENTS {
        let conn = server.client("1");
        conn.stream()
            .set_write_timeout(Some(Duration::from_millis(500)))
            .unwrap();
        let started = Instant::now();
        for _ in 0..2_000 {
            if started.elapsed() > Duration::from_secs(2)
                || conn.stream().write_all(&download).is_err()
            {
                break;
            }
        }
        clients.push(conn);
    }
    std::thread::sleep(Duration::from_secs(1));
    let after = threads(server.pid());

    // A connection runs at most 4 downloads, each on a thread of its own.
    assert!(
        after < before + 6 * CLIENTS,
        "{CLIENTS} clients took the server from {before} to {after} threads"
    );
    a_new_client_registers_and_loads(&server, oid);
    drop(clients);
}
