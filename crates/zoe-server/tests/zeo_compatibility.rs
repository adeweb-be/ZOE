//! Test of three answers of `zoe serve` that ZEO's suites look at.
//!
//! - ZEO's tests store with the serial `'\0' * 8`, a str. A stock client
//!   sends it as a msgpack str. ZEO's server takes it, so `zoe serve`
//!   reads a str serial whose UTF-8 bytes are 8 bytes as those bytes.
//! - `history` of an oid that is not 8 bytes fails with `POSKeyError`
//!   before the check of the size, as ZEO's server does.
//! - A stock client sends `iterator_gc` with no reply, then can call
//!   `iterator_next` for an iterator that it listed. The reader starts no
//!   call before `iterator_gc` ends, so that call fails with `KeyError`.

mod wire;

use wire::{Conn, Fault, Server, Value, begin, commit, fresh_dir, load, new_oids, p64, record};

const CONFLICT_ERROR: &str = "ZODB.POSException.ConflictError";
const POS_KEY_ERROR: &str = "ZODB.POSException.POSKeyError";
const TYPE_ERROR: &str = "builtins.TypeError";
const KEY_ERROR: &str = "builtins.KeyError";

/// Commit one transaction that stores `data` for `oid` over `serial`, sent
/// as it is. Return the tid of the commit, or the error of the vote.
fn commit_over(
    conn: &mut Conn,
    txn: i64,
    oid: u64,
    serial: Value,
    data: Vec<u8>,
) -> Result<u64, Fault> {
    let id = Value::from(txn);
    begin(conn, txn);
    conn.send_async(
        "storea",
        vec![p64(oid), serial, Value::Binary(data), id.clone()],
    );
    let voted = conn.call("vote", vec![id.clone()]);
    if voted.is_err() {
        conn.call("tpc_abort", vec![id]).unwrap();
        return voted.map(|_| 0);
    }
    let tid = conn.call("tpc_finish", vec![id])?;
    Ok(wire::u64_of(&tid))
}

#[test]
fn a_str_serial_of_eight_bytes_reads_as_those_bytes() {
    let dir = fresh_dir("zeo-str-serial");
    let server = Server::path(&dir.join("store"));
    let mut client = server.client("1");
    let zero = Value::from("\0".repeat(8));
    let oid = new_oids(&mut client, 1)[0];

    // The serial of a new object commits a new object.
    let tid = commit_over(&mut client, 1, oid, zero.clone(), record(b"first")).unwrap();
    assert_eq!(load(&mut client, oid).unwrap(), (record(b"first"), tid));

    // The object has a record now: the serial of a new object conflicts.
    let fault = commit_over(&mut client, 2, oid, zero, record(b"second")).unwrap_err();
    assert_eq!(fault.class, CONFLICT_ERROR);
    assert_eq!(load(&mut client, oid).unwrap(), (record(b"first"), tid));

    // A str of another length still fails, and the connection stays open.
    let other = new_oids(&mut client, 1)[0];
    let short = Value::from("\0".repeat(7));
    let fault = commit_over(&mut client, 3, other, short, record(b"x")).unwrap_err();
    assert_eq!(fault.class, TYPE_ERROR);
    let tid2 = commit(&mut client, 4, &[(oid, tid, record(b"second"))]).unwrap();
    assert_eq!(load(&mut client, oid).unwrap(), (record(b"second"), tid2));
}

#[test]
fn history_of_an_oid_that_is_not_bytes_is_a_poskeyerror() {
    let dir = fresh_dir("zeo-history-oid");
    let server = Server::path(&dir.join("store"));
    let mut client = server.client("1");
    let oid = new_oids(&mut client, 1)[0];
    commit(&mut client, 1, &[(oid, 0, record(b"data"))]).unwrap();

    for size in [Value::Nil, Value::from(1)] {
        let fault = client.call("history", vec![Value::Nil, size]).unwrap_err();
        assert_eq!(fault.class, POS_KEY_ERROR);
        assert_eq!(fault.args, Value::Array(vec![Value::Nil]));
    }
    let fault = client
        .call("history", vec![p64(oid), Value::Nil])
        .unwrap_err();
    assert_eq!(fault.class, TYPE_ERROR);
    let history = client
        .call("history", vec![p64(oid), Value::from(1)])
        .unwrap();
    assert_eq!(wire::items(&history).len(), 1);
}

#[test]
fn iterator_gc_takes_effect_before_the_next_call() {
    let dir = fresh_dir("zeo-iterator-gc");
    let server = Server::path(&dir.join("store"));
    let mut client = server.client("1");
    let oid = new_oids(&mut client, 1)[0];
    let tid = commit(&mut client, 1, &[(oid, 0, record(b"first"))]).unwrap();
    commit(&mut client, 2, &[(oid, tid, record(b"second"))]).unwrap();

    // Without `iterator_gc`, the iterator gives its second transaction.
    let iid = client
        .call("iterator_start", vec![Value::Nil, Value::Nil])
        .unwrap();
    client.call("iterator_next", vec![iid.clone()]).unwrap();
    let next = client.call("iterator_next", vec![iid]).unwrap();
    assert!(!next.is_nil());

    for _ in 0..50 {
        let iid = client
            .call("iterator_start", vec![Value::Nil, Value::Nil])
            .unwrap();
        client.call("iterator_next", vec![iid.clone()]).unwrap();
        // The client does not wait between the two calls.
        client.send_async("iterator_gc", vec![Value::Array(vec![iid.clone()])]);
        let fault = client.call("iterator_next", vec![iid.clone()]).unwrap_err();
        assert_eq!(fault.class, KEY_ERROR);
        assert_eq!(fault.args, Value::Array(vec![iid]));
    }
}

#[test]
fn a_second_iterator_gc_of_an_iterator_does_nothing() {
    let dir = fresh_dir("zeo-iterator-gc-twice");
    let server = Server::path(&dir.join("store"));
    let mut client = server.client("1");
    let oid = new_oids(&mut client, 1)[0];
    commit(&mut client, 1, &[(oid, 0, record(b"data"))]).unwrap();

    let iid = client
        .call("iterator_start", vec![Value::Nil, Value::Nil])
        .unwrap();
    client.send_async("iterator_gc", vec![Value::Array(vec![iid.clone()])]);
    client.send_async("iterator_gc", vec![Value::Array(vec![iid.clone()])]);
    let fault = client.call("iterator_next", vec![iid]).unwrap_err();
    assert_eq!(fault.class, KEY_ERROR);
    assert_eq!(load(&mut client, oid).unwrap().0, record(b"data"));
}
