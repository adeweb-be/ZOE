//! The extension `sequences` on the wire (F29, F13).
//!
//! A connection that declares `sequences` in `register` calls
//! `sequence_next(name)` and gets the next value of the sequence `name` of
//! the store, an int. A connection that does not declare it gets
//! `ValueError` for that call, outside and inside a transaction, and the
//! connection stays open.

mod common;

use common::{Server, Value, begin, commit, fresh_dir, new_oids, p64, record};

/// Register `conn` with the storage `1` and the declaration `names`.
fn declare(conn: &mut common::Conn, read_only: bool, names: &[&str]) {
    let names = names.iter().map(|name| Value::from(*name)).collect();
    conn.call(
        "register",
        vec![
            Value::from("1"),
            Value::Boolean(read_only),
            Value::Array(names),
        ],
    )
    .unwrap();
}

fn next(conn: &mut common::Conn, name: &str) -> Result<u64, common::Fault> {
    conn.call("sequence_next", vec![Value::from(name)])
        .map(|value| value.as_u64().expect("sequence_next gives an int"))
}

#[test]
fn a_declared_connection_gets_the_values_from_one() {
    let dir = fresh_dir("sequences-declared");
    let server = Server::path(&dir.join("store"));
    let mut conn = server.connect();
    declare(&mut conn, false, &["sequences"]);
    let info = conn.call("get_info", vec![]).unwrap();
    let extensions = info
        .as_map()
        .unwrap()
        .iter()
        .find(|(key, _)| key.as_str() == Some("extensions"))
        .map(|(_, value)| value.clone());
    assert_eq!(
        extensions,
        Some(Value::Array(vec![Value::from("sequences")]))
    );
    let values: Vec<u64> = (0..150)
        .map(|_| next(&mut conn, "intids").unwrap())
        .collect();
    assert_eq!(values, (1..=150).collect::<Vec<_>>());
    assert_eq!(next(&mut conn, "lexicon").unwrap(), 1);

    // A second connection shares the sequences of the store.
    let mut other = server.connect();
    declare(&mut other, false, &["sequences"]);
    assert_eq!(next(&mut other, "intids").unwrap(), 151);
}

#[test]
fn an_undeclared_call_is_refused_and_the_connection_stays_open() {
    let dir = fresh_dir("sequences-undeclared");
    let server = Server::path(&dir.join("store"));
    let mut stock = server.connect();
    stock.register("1", false).unwrap();
    let mut empty = server.connect();
    declare(&mut empty, false, &[]);
    let mut other = server.connect();
    declare(&mut other, false, &["no_such_extension"]);
    for conn in [&mut stock, &mut empty, &mut other] {
        let fault = next(conn, "intids").unwrap_err();
        assert_eq!(fault.class, "builtins.ValueError");
        assert_eq!(conn.call("ping", vec![]).unwrap(), Value::Nil);
    }
    // The refused calls took no value.
    let mut declared = server.connect();
    declare(&mut declared, false, &["sequences"]);
    assert_eq!(next(&mut declared, "intids").unwrap(), 1);
}

#[test]
fn an_undeclared_call_in_a_transaction_is_refused_and_the_transaction_commits() {
    let dir = fresh_dir("sequences-undeclared-txn");
    let server = Server::path(&dir.join("store"));
    let mut conn = server.connect();
    conn.register("1", false).unwrap();
    let oid = new_oids(&mut conn, 1)[0];
    let id = Value::from(7);
    begin(&mut conn, 7);
    conn.send_async(
        "storea",
        vec![p64(oid), p64(0), Value::Binary(record(b"a")), id.clone()],
    );
    let fault = next(&mut conn, "intids").unwrap_err();
    assert_eq!(fault.class, "builtins.ValueError");
    conn.call("vote", vec![id.clone()]).unwrap();
    conn.call("tpc_finish", vec![id]).unwrap();
    assert_eq!(common::load(&mut conn, oid).unwrap().0, record(b"a"));
}

#[test]
fn an_abort_keeps_its_values() {
    let dir = fresh_dir("sequences-abort");
    let server = Server::path(&dir.join("store"));
    let mut conn = server.connect();
    declare(&mut conn, false, &["sequences"]);
    let oid = new_oids(&mut conn, 1)[0];
    begin(&mut conn, 7);
    assert_eq!(next(&mut conn, "intids").unwrap(), 1);
    conn.call("tpc_abort", vec![Value::from(7)]).unwrap();
    assert_eq!(next(&mut conn, "intids").unwrap(), 2);
    commit(&mut conn, 8, &[(oid, 0, record(b"a"))]).unwrap();
    assert_eq!(next(&mut conn, "intids").unwrap(), 3);
}

#[test]
fn a_read_only_connection_and_a_bad_name_are_refused() {
    let dir = fresh_dir("sequences-refused");
    let server = Server::path(&dir.join("store"));
    let mut reader = server.connect();
    declare(&mut reader, true, &["sequences"]);
    let fault = next(&mut reader, "intids").unwrap_err();
    assert_eq!(fault.class, "ZODB.POSException.ReadOnlyError");
    let mut conn = server.connect();
    declare(&mut conn, false, &["sequences"]);
    for args in [
        vec![],
        vec![Value::from(7)],
        vec![Value::Binary(b"a".to_vec())],
    ] {
        let fault = conn.call("sequence_next", args).unwrap_err();
        assert_eq!(fault.class, "builtins.TypeError");
    }
    assert_eq!(next(&mut conn, "intids").unwrap(), 1);
}

#[test]
fn values_keep_increasing_after_a_restart() {
    let dir = fresh_dir("sequences-restart");
    let mut server = Server::path(&dir.join("store"));
    let mut conn = server.connect();
    declare(&mut conn, false, &["sequences"]);
    let last = (0..5)
        .map(|_| next(&mut conn, "intids").unwrap())
        .last()
        .unwrap();
    drop(conn);
    server.kill();
    server.restart();
    let mut conn = server.connect();
    declare(&mut conn, false, &["sequences"]);
    let value = next(&mut conn, "intids").unwrap();
    assert!(last < value && value <= last + 101, "{last} then {value}");
}
