//! Acceptance test of idle connections (F2).
//!
//! A connection that runs no call and has no open transaction holds no
//! thread of the blocking pool. So idle clients, more of them than the 512
//! threads that the blocking pool of tokio takes by default, never stop a
//! new client: it registers and commits at once, and the idle clients still
//! work.

use std::time::Duration;

mod wire;

use wire::{Conn, Server, TIMEOUT, Value, commit, fresh_dir, load, new_oids, record};

/// The idle clients that registered.
const REGISTERED: usize = 50;

/// The idle clients that only answered the protocol of the server.
const SILENT: usize = 600;

#[test]
fn many_idle_connections_do_not_stop_a_new_client() {
    let dir = fresh_dir("idle-connections");
    let server = Server::path(&dir.join("store"));
    let mut registered: Vec<Conn> = (0..REGISTERED).map(|_| server.client("1")).collect();
    let mut silent: Vec<Conn> = (0..SILENT).map(|_| server.connect()).collect();

    // A new client gets the reply of its `register` within a few seconds.
    let mut conn = server.connect();
    let short = Some(Duration::from_secs(5));
    conn.stream().set_read_timeout(short).unwrap();
    let id = conn.send_call("register", vec![Value::from("1"), Value::Boolean(false)]);
    let reply = conn.recv();
    assert!(
        matches!(&reply, Ok(message) if message.id == id && !message.is_error()),
        "no reply to register while {} clients are idle: {reply:?}",
        REGISTERED + SILENT
    );
    conn.stream().set_read_timeout(Some(TIMEOUT)).unwrap();

    // It commits, and an idle client reads the commit.
    let oid = new_oids(&mut conn, 1)[0];
    let tid = commit(&mut conn, 1, &[(oid, 0, record(b"new"))]).unwrap();
    let loaded = load(&mut registered[0], oid).unwrap();
    assert_eq!(loaded, (record(b"new"), tid));
    // A silent client still registers.
    silent[0].register("1", false).unwrap();
}
