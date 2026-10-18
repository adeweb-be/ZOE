//! Acceptance test of the graceful shutdown of `zoe serve` (N10).
//!
//! On SIGTERM the server stops accepting connections, lets every
//! `tpc_finish` that it received complete and answers it, aborts the
//! transactions that are still staged or locked, closes each client
//! connection cleanly, closes the stores and exits with 0. All of this
//! takes at most `--shutdown-timeout` seconds (`ZOE_SHUTDOWN_TIMEOUT`, 30
//! by default). Under load, every commit that a client saw acknowledged is
//! in the store after the restart, and nothing of an aborted transaction
//! is.

use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

mod wire;

use wire::{Conn, Server, Value, begin, commit, fresh_dir, load, new_oids, p64, record, s};

#[test]
fn the_help_names_the_timeout_and_its_twin() {
    let output = {
        let _spawn = wire::spawn_lock();
        std::process::Command::new(wire::ZOE)
            .args(["serve", "--help"])
            .output()
            .unwrap()
    };
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(help.contains("--shutdown-timeout"));
    assert!(help.contains("ZOE_SHUTDOWN_TIMEOUT"));
}

#[test]
fn sigterm_under_load_keeps_every_acknowledged_commit() {
    let dir = fresh_dir("load");
    let path = dir.join("store");
    let mut server = Server::start(&["--path", s(&path), "--shutdown-timeout", "10"], &[]);
    let addr = server.addr;
    let stop = Arc::new(AtomicBool::new(false));
    let acknowledged = Arc::new(Mutex::new(Vec::new()));

    let writers: Vec<_> = (0..4)
        .map(|w| {
            let stop = stop.clone();
            let acknowledged = acknowledged.clone();
            thread::spawn(move || {
                let mut conn = Conn::open(addr);
                conn.register("1", false).unwrap();
                let mut n: i64 = 0;
                while !stop.load(Ordering::Relaxed) {
                    n += 1;
                    let value = format!("{w}-{n}").into_bytes();
                    // A read on a connection that the server closed panics
                    // in the helper: that ends the loop of this writer.
                    let round = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let oid = new_oids(&mut conn, 1)[0];
                        commit(&mut conn, n, &[(oid, 0, record(&value))])
                            .ok()
                            .map(|tid| (oid, tid))
                    }));
                    match round {
                        Ok(Some((oid, tid))) => {
                            acknowledged.lock().unwrap().push((oid, value, tid))
                        }
                        _ => break,
                    }
                }
            })
        })
        .collect();

    // A transaction left staged, and one left locked after its vote.
    let mut staged = server.client("1");
    let mut locked = server.client("1");
    let oids = new_oids(&mut staged, 2);
    begin(&mut staged, 1);
    staged.send_async(
        "storea",
        vec![
            p64(oids[0]),
            p64(0),
            Value::Binary(record(b"staged")),
            Value::from(1),
        ],
    );
    begin(&mut locked, 1);
    locked.send_async(
        "storea",
        vec![
            p64(oids[1]),
            p64(0),
            Value::Binary(record(b"locked")),
            Value::from(1),
        ],
    );
    locked.call("vote", vec![Value::from(1)]).unwrap();

    thread::sleep(Duration::from_millis(300));
    let (status, took) = server.terminate(Duration::from_secs(20));
    stop.store(true, Ordering::Relaxed);
    for writer in writers {
        writer.join().unwrap();
    }
    assert_eq!(status.code(), Some(0), "zoe serve did not exit cleanly");
    assert!(took < Duration::from_secs(10), "the shutdown took {took:?}");

    // Each client saw the connection close.
    assert!(staged.closed_within(Duration::from_secs(5)));
    assert!(locked.closed_within(Duration::from_secs(5)));
    // The listener is closed.
    assert!(TcpStream::connect_timeout(&addr, Duration::from_secs(1)).is_err());

    let acknowledged = acknowledged.lock().unwrap().clone();
    assert!(
        !acknowledged.is_empty(),
        "no commit went through before SIGTERM"
    );
    server.restart();
    let mut conn = server.client("1");
    for (oid, value, tid) in &acknowledged {
        assert_eq!(load(&mut conn, *oid).unwrap(), (record(value), *tid));
    }
    // The staged and the locked transactions left nothing.
    assert!(load(&mut conn, oids[0]).is_err());
    assert!(load(&mut conn, oids[1]).is_err());
}

#[test]
fn an_idle_server_stops_at_once_and_closes_its_store() {
    let dir = fresh_dir("idle");
    let path = dir.join("store");
    let mut server = Server::path(&path);
    let _idle = server.client("1");
    let (status, took) = server.terminate(Duration::from_secs(40));
    assert_eq!(status.code(), Some(0));
    assert!(
        took < Duration::from_secs(5),
        "an idle server took {took:?}"
    );
    // The store was closed cleanly: a read-only open accepts it.
    let _spawn = wire::spawn_lock();
    zoe_core::Store::open_read_only(&path).unwrap();
}

#[test]
fn a_client_that_never_answers_does_not_hold_the_shutdown_past_its_timeout() {
    let dir = fresh_dir("silent");
    let mut server = Server::start(
        &["--path", s(&dir.join("store")), "--shutdown-timeout", "2"],
        &[],
    );
    // A connection that never answers the protocol of the server.
    let _silent = Conn::open_silent(server.addr);
    let (status, took) = server.terminate(Duration::from_secs(20));
    assert_eq!(status.code(), Some(0));
    assert!(took < Duration::from_secs(4), "the shutdown took {took:?}");
}
