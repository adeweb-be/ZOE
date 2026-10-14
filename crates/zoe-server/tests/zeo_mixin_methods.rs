//! Acceptance test of two calls that ZODB's storage mixins make through a
//! stock `ZEO.ClientStorage`: `storea` with a nil serial, and `getTid`.
//!
//! The mixins store a new object with the serial `None`, and a stock client
//! sends it unchanged. ZEO 6 gives it to the storage unchanged. FileStorage
//! checks no serial for an oid that has no record, so the store commits.
//! For an oid that has a record, `None` is not its serial, so the vote
//! fails with `ConflictError`. A nil serial is thus the serial of a new
//! object, the zero tid.
//!
//! `getTid(oid)` gives the tid of the current record of `oid`. An oid that
//! has no record, and an oid whose current record is a deletion, give
//! `POSKeyError`, as FileStorage does. The error is a reply: the connection
//! stays open.

mod wire;

use wire::{
    Conn, Fault, Server, Value, begin, commit, fresh_dir, load, new_oids, p64, record, u64_of,
};

const POS_KEY_ERROR: &str = "ZODB.POSException.POSKeyError";
const CONFLICT_ERROR: &str = "ZODB.POSException.ConflictError";

/// Commit one transaction that stores `data` for `oid` with a nil serial,
/// as a stock client sends a store of ZODB's mixins. Return the tid of the
/// commit, or the error of the vote.
fn commit_with_nil_serial(
    conn: &mut Conn,
    txn: i64,
    oid: u64,
    data: Vec<u8>,
) -> Result<u64, Fault> {
    let id = Value::from(txn);
    begin(conn, txn);
    conn.send_async(
        "storea",
        vec![p64(oid), Value::Nil, Value::Binary(data), id.clone()],
    );
    conn.call("vote", vec![id.clone()])?;
    let tid = conn.call("tpc_finish", vec![id])?;
    Ok(u64_of(&tid))
}

/// Call `getTid` for `oid`. Return the tid, or the error of the server.
fn get_tid(conn: &mut Conn, oid: u64) -> Result<u64, Fault> {
    conn.call("getTid", vec![p64(oid)]).map(|tid| u64_of(&tid))
}

#[test]
fn a_new_object_stored_with_a_nil_serial_commits_and_loads_back() {
    let dir = fresh_dir("mixin-nil-new");
    let server = Server::path(&dir.join("store"));
    let mut client = server.client("1");
    let oid = new_oids(&mut client, 1)[0];

    let tid = commit_with_nil_serial(&mut client, 1, oid, record(b"eleven")).unwrap();

    let (data, serial) = load(&mut client, oid).unwrap();
    assert_eq!(data, record(b"eleven"));
    assert_eq!(serial, tid);
    // Another connection reads the same record.
    let mut other = server.client("1");
    assert_eq!(load(&mut other, oid).unwrap(), (record(b"eleven"), tid));
}

#[test]
fn a_nil_serial_for_an_object_that_has_a_record_fails_the_vote() {
    let dir = fresh_dir("mixin-nil-old");
    let server = Server::path(&dir.join("store"));
    let mut client = server.client("1");
    let oid = new_oids(&mut client, 1)[0];
    let first = commit(&mut client, 1, &[(oid, 0, record(b"first"))]).unwrap();

    // The nil serial is the serial of a new object, not "no check".
    let fault = commit_with_nil_serial(&mut client, 2, oid, record(b"second")).unwrap_err();
    assert_eq!(fault.class, CONFLICT_ERROR);
    client.call("tpc_abort", vec![Value::from(2)]).unwrap();

    assert_eq!(load(&mut client, oid).unwrap(), (record(b"first"), first));
}

#[test]
fn get_tid_gives_the_tid_of_the_last_commit_of_an_oid() {
    let dir = fresh_dir("mixin-get-tid");
    let server = Server::path(&dir.join("store"));
    let mut client = server.client("1");
    let oids = new_oids(&mut client, 2);
    let (oid, untouched) = (oids[0], oids[1]);

    let first = commit(
        &mut client,
        1,
        &[(oid, 0, record(b"41")), (untouched, 0, record(b"x"))],
    )
    .unwrap();
    assert_eq!(get_tid(&mut client, oid).unwrap(), first);

    let second = commit(&mut client, 2, &[(oid, first, record(b"42"))]).unwrap();
    assert!(second > first);
    assert_eq!(get_tid(&mut client, oid).unwrap(), second);
    // An oid that the second commit did not write keeps its tid.
    assert_eq!(get_tid(&mut client, untouched).unwrap(), first);

    // Another connection sees the commit once its tpc_finish replied.
    let mut other = server.client("1");
    let third = commit(&mut other, 3, &[(oid, second, record(b"43"))]).unwrap();
    assert_eq!(get_tid(&mut client, oid).unwrap(), third);
    assert_eq!(get_tid(&mut other, oid).unwrap(), third);
}

#[test]
fn get_tid_of_an_oid_without_a_record_is_a_pos_key_error_and_the_connection_stays_open() {
    let dir = fresh_dir("mixin-get-tid-unknown");
    let server = Server::path(&dir.join("store"));
    let mut client = server.client("1");
    let oids = new_oids(&mut client, 2);
    let (unknown, oid) = (oids[0], oids[1]);

    // An empty store: no oid has a record.
    let fault = get_tid(&mut client, unknown).unwrap_err();
    assert_eq!(fault.class, POS_KEY_ERROR);

    // The connection stays open, and the next calls work.
    let tid = commit(&mut client, 1, &[(oid, 0, record(b"x"))]).unwrap();
    assert_eq!(get_tid(&mut client, oid).unwrap(), tid);
    let fault = get_tid(&mut client, unknown).unwrap_err();
    assert_eq!(fault.class, POS_KEY_ERROR);
    assert_eq!(get_tid(&mut client, oid).unwrap(), tid);
}

#[test]
fn get_tid_of_a_deleted_object_is_a_pos_key_error() {
    let dir = fresh_dir("mixin-get-tid-deleted");
    let server = Server::path(&dir.join("store"));
    let mut client = server.client("1");
    let oid = new_oids(&mut client, 1)[0];
    let first = commit(&mut client, 1, &[(oid, 0, record(b"x"))]).unwrap();

    let id = Value::from(2);
    begin(&mut client, 2);
    client.send_async("deleteObject", vec![p64(oid), p64(first), id.clone()]);
    client.call("vote", vec![id.clone()]).unwrap();
    client.call("tpc_finish", vec![id]).unwrap();

    // The current record is a deletion, so the object has no tid, as in
    // FileStorage. The older record still loads by its serial.
    let fault = get_tid(&mut client, oid).unwrap_err();
    assert_eq!(fault.class, POS_KEY_ERROR);
    let older = client.call("loadSerial", vec![p64(oid), p64(first)]);
    assert_eq!(older.unwrap(), Value::Binary(record(b"x")));
    // The connection stays open.
    let mut other = server.client("1");
    assert_eq!(get_tid(&mut other, oid).unwrap_err().class, POS_KEY_ERROR);
    assert!(client.call("lastTransaction", vec![]).is_ok());
}
