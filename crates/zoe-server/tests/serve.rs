//! Acceptance test of `zoe serve`, the walking skeleton of the server.
//!
//! `zoe serve --path STORE --listen HOST:PORT` serves the store under the
//! storage name `1`, with the ZEO 6 msgpack protocol. It writes the line
//! `listening <address>` to stdout when it accepts connections, so a port
//! of 0 gives a free port. The options have the twins `ZOE_PATH` and
//! `ZOE_LISTEN`. A client registers, asks for oids, loads, stores, votes
//! and finishes. Each reply carries the message id of its call, and a
//! message id can be any msgpack value: the stock client sends a tuple for
//! `loadBefore`.
//!
//! The server is also a library: `zoe_server::serve::Server` runs it in the
//! process of a test. The store keeps one format in both modes, so a store
//! that the server wrote opens in-process.
//!
//! `python/zoedb/tests/server/test_walking_skeleton.py` drives the server
//! with an unmodified `ZEO.ClientStorage`, and runs the kill loop.

use zoe_core::{Oid, Tid};
use zoe_server::serve::{ServeOptions, Server as InProcess};

mod common;

use common::{
    Conn, MAX, Server, Value, ZERO, assert_exit, bytes_of, commit, fresh_dir, items, load,
    new_oids, open_store, p64, record, s, u64_of, zoe,
};

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn the_command_has_a_serve_subcommand_with_its_options_and_twins() {
    let output = zoe(&["--help"], &[]);
    assert_exit(&output, 0, "zoe --help");
    assert!(text(&output.stdout).contains("serve"));

    let output = zoe(&["serve", "--help"], &[]);
    assert_exit(&output, 0, "zoe serve --help");
    let help = text(&output.stdout);
    for name in ["--path", "ZOE_PATH", "--listen", "ZOE_LISTEN"] {
        assert!(help.contains(name), "{name} is not in:\n{help}");
    }
}

#[test]
fn serve_without_a_store_is_a_usage_error_and_creates_nothing() {
    let dir = fresh_dir("no-store");
    let output = zoe(&["serve", "--listen", "127.0.0.1:0"], &[]);
    assert_exit(&output, 2, "zoe serve without --path");

    let output = zoe(
        &[
            "serve",
            "--path",
            s(&dir.join("store")),
            "--listen",
            "nowhere",
        ],
        &[],
    );
    assert_exit(
        &output,
        2,
        "zoe serve with a listen address that does not parse",
    );
    assert!(
        !dir.join("store").exists(),
        "a usage error created the store"
    );
}

#[test]
fn the_twins_give_the_store_and_the_address() {
    let dir = fresh_dir("twins");
    let store = dir.join("store");
    let mut server = Server::start(
        &[],
        &[("ZOE_PATH", s(&store)), ("ZOE_LISTEN", "127.0.0.1:0")],
    );
    let mut conn = server.connect();
    assert_eq!(u64_of(&conn.register("1", false).unwrap()), 0);
    server.kill();
    assert!(store.join("zoe.redb").exists());
}

#[test]
fn the_server_announces_msgpack_and_the_client_registers() {
    let dir = fresh_dir("handshake");
    let server = Server::path(&dir.join("store"));
    let mut conn = Conn::open(server.addr);
    assert_eq!(conn.protocol, b"M5");

    // A new store has no transaction: register gives the zero tid.
    let last = conn.register("1", false).unwrap();
    assert_eq!(bytes_of(&last), ZERO);

    // The client takes the last transaction again, as the stock client
    // does after it registers.
    assert_eq!(
        bytes_of(&conn.call("lastTransaction", vec![]).unwrap()),
        ZERO
    );

    // A storage name that the server does not serve is refused.
    let mut other = Conn::open(server.addr);
    assert!(other.register("nothing-here", false).is_err());
}

#[test]
fn an_object_commits_and_loads_back_through_the_wire() {
    let dir = fresh_dir("commit");
    let server = Server::path(&dir.join("store"));
    let mut conn = server.client("1");

    let oids = new_oids(&mut conn, 3);
    assert_eq!(oids.len(), 3);
    let mut sorted = oids.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), 3, "new_oids gave an oid twice: {oids:?}");

    let oid = oids[0];
    let tid = commit(&mut conn, 1, &[(oid, 0, record(b"one"))]).unwrap();
    assert_eq!(u64_of(&conn.call("lastTransaction", vec![]).unwrap()), tid);
    assert_eq!(load(&mut conn, oid).unwrap(), (record(b"one"), tid));

    // A second revision is current; loadBefore the second tid gives the
    // first, with the second tid as its end.
    let second = commit(&mut conn, 2, &[(oid, tid, record(b"two"))]).unwrap();
    assert!(second > tid);
    assert_eq!(load(&mut conn, oid).unwrap(), (record(b"two"), second));
    let before = conn
        .call("loadBefore", vec![p64(oid), p64(second)])
        .unwrap();
    let parts = items(&before);
    assert_eq!(bytes_of(&parts[0]), record(b"one"));
    assert_eq!(u64_of(&parts[1]), tid);
    assert_eq!(u64_of(&parts[2]), second);

    // An oid without a record is POSKeyError.
    let missing = load(&mut conn, oids[2]).unwrap_err();
    assert_eq!(missing.class, "ZODB.POSException.POSKeyError");
}

#[test]
fn a_reply_carries_the_message_id_of_its_call_whatever_its_form() {
    let dir = fresh_dir("message-ids");
    let server = Server::path(&dir.join("store"));
    let mut conn = server.client("1");
    let oid = new_oids(&mut conn, 1)[0];
    let tid = commit(&mut conn, 1, &[(oid, 0, record(b"x"))]).unwrap();

    // The stock client sends loadBefore with the message id (oid, tid).
    let id = Value::Array(vec![p64(oid), Value::Binary(MAX.to_vec())]);
    conn.send(
        id.clone(),
        Value::Boolean(false),
        "loadBefore",
        vec![p64(oid), Value::Binary(MAX.to_vec())],
    );
    let reply = conn.reply_to(&id).unwrap();
    assert_eq!(u64_of(&items(&reply)[1]), tid);
}

#[test]
fn an_aborted_transaction_leaves_nothing() {
    let dir = fresh_dir("abort");
    let server = Server::path(&dir.join("store"));
    let mut conn = server.client("1");
    let oid = new_oids(&mut conn, 1)[0];

    common::begin(&mut conn, 7);
    conn.send_async(
        "storea",
        vec![
            p64(oid),
            p64(0),
            Value::Binary(record(b"gone")),
            Value::from(7),
        ],
    );
    conn.call("tpc_abort", vec![Value::from(7)]).unwrap();
    assert!(load(&mut conn, oid).is_err());
    assert_eq!(
        bytes_of(&conn.call("lastTransaction", vec![]).unwrap()),
        ZERO
    );
}

#[test]
fn the_server_runs_in_the_process_of_a_test() {
    let dir = fresh_dir("in-process");
    let store = dir.join("store");
    let server = InProcess::start(ServeOptions::new(&store)).unwrap();
    let addr = server.addr();
    assert!(addr.ip().is_loopback());
    assert_ne!(addr.port(), 0);

    let mut conn = Conn::open(addr);
    conn.register("1", false).unwrap();
    let oid = new_oids(&mut conn, 1)[0];
    let tid = commit(&mut conn, 1, &[(oid, 0, record(b"in"))]).unwrap();
    drop(conn);
    server.stop();

    // The store that the server wrote opens in-process, with the same
    // format.
    let store = open_store(&store);
    assert_eq!(store.load(Oid(oid)).unwrap(), (record(b"in"), Tid(tid)));
}

#[test]
fn a_store_written_in_process_serves_as_it_is() {
    let dir = fresh_dir("same-format");
    let path = dir.join("store");
    let tid = {
        let store = open_store(&path);
        // The root is oid 0. ZODB writes it without new_oid.
        let oid = Oid(0);
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.store(oid, Tid(0), &record(b"root")).unwrap();
        txn.vote().unwrap();
        let tid = txn.finish().unwrap();
        drop(store);
        tid
    };

    let server = Server::path(&path);
    let mut conn = server.client("1");
    assert_eq!(
        u64_of(&conn.call("lastTransaction", vec![]).unwrap()),
        tid.0
    );
    assert_eq!(load(&mut conn, 0).unwrap(), (record(b"root"), tid.0));
    // The next oid of the store comes after the oid written in-process.
    assert!(new_oids(&mut conn, 1)[0] > 0);
}

#[test]
fn a_store_survives_a_kill_of_the_server() {
    let dir = fresh_dir("kill");
    let path = dir.join("store");
    let mut server = Server::path(&path);
    let mut conn = server.client("1");
    let oid = new_oids(&mut conn, 1)[0];
    let tid = commit(&mut conn, 1, &[(oid, 0, record(b"kept"))]).unwrap();
    drop(conn);

    server.kill();
    server.restart();
    let mut conn = server.client("1");
    assert_eq!(load(&mut conn, oid).unwrap(), (record(b"kept"), tid));
}
