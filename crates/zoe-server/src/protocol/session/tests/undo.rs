//! The tests of `undoa`, `undoLog` and `undoInfo`.

use super::*;

/// Undo the transaction of the undo id `undo_id` in the transaction
/// `id`. Return the reply of the vote, or its fault.
fn undo_in(s: &mut Client, id: i64, undo_id: Value) -> Result<Value, Fault> {
    begin(s, id, None);
    done(call(s, "undoa", vec![undo_id, Value::from(id)]));
    match call(s, "vote", vec![Value::from(id)]) {
        Outcome::Done(reply) => {
            finish(s, Value::from(id));
            Ok(reply)
        }
        outcome => Err(failed(outcome)),
    }
}

#[test]
fn an_undo_stages_at_the_vote_and_the_reply_gives_its_oids() {
    let mut s = registered("undo");
    let first = commit(&mut s, 1, 7, 0, b"one").unwrap();
    let second = commit(&mut s, 2, 7, first, b"two").unwrap();
    let log = done(call(
        &mut s,
        "undoLog",
        vec![Value::from(0), Value::from(1)],
    ));
    let entry = &log.as_array().unwrap()[0];
    assert_eq!(get(entry, "id"), &Value::Binary(undo_id(Tid(second))));
    assert_eq!(get(entry, "user_name"), &Value::Binary(b"user-2".to_vec()));
    let reply = undo_in(&mut s, 3, get(entry, "id").clone()).unwrap();
    assert_eq!(reply, Value::Array(vec![p64(7)]));
    let loaded = done(call(&mut s, "loadBefore", vec![p64(7), p64(u64::MAX >> 1)]));
    assert_eq!(
        loaded.as_array().unwrap()[0],
        Value::Binary(b"one".to_vec())
    );
    // The undo is in the log, newest first.
    let log = done(call(
        &mut s,
        "undoLog",
        vec![Value::from(0), Value::from(-20)],
    ));
    assert_eq!(log.as_array().unwrap().len(), 3);
}

#[test]
fn an_undo_that_no_tier_merges_fails_the_vote_with_an_undo_error() {
    let mut s = registered("undo-error");
    let first = commit(&mut s, 1, 7, 0, b"one").unwrap();
    let second = commit(&mut s, 2, 7, first, b"two").unwrap();
    commit(&mut s, 3, 7, second, b"three").unwrap();
    let fault = undo_in(&mut s, 4, Value::Binary(undo_id(Tid(second)))).unwrap_err();
    assert_eq!(fault.class, "ZODB.POSException.MultipleUndoErrors");
    let errors = item(&fault, "_errs");
    let [error] = errors.as_array().unwrap().as_slice() else {
        panic!("one error: {fault:?}");
    };
    assert_eq!(error.as_array().unwrap()[0], p64(7));
    // An id that names no transaction fails the vote too.
    let fault = undo_in(&mut s, 5, Value::Binary(b"not an id".to_vec())).unwrap_err();
    assert_eq!(fault.class, "ZODB.POSException.MultipleUndoErrors");
}

#[test]
fn undo_info_keeps_the_entries_of_its_specification() {
    let mut s = registered("undo-info");
    let first = commit(&mut s, 1, 7, 0, b"one").unwrap();
    commit(&mut s, 2, 7, first, b"two").unwrap();
    let specification = Value::Map(vec![(
        Value::from("user_name"),
        Value::Binary(b"user-1".to_vec()),
    )]);
    let args = vec![Value::from(0), Value::from(-20), specification];
    let info = done(call(&mut s, "undoInfo", args));
    let entries = info.as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(get(&entries[0], "id"), &Value::Binary(undo_id(Tid(first))));
    let all = done(call(&mut s, "undoInfo", vec![]));
    assert_eq!(all.as_array().unwrap().len(), 2);
}

#[test]
fn undo_info_matches_a_missing_key_with_none_and_numbers_as_python() {
    let mut s = registered("undo-info-python");
    let first = commit(&mut s, 1, 7, 0, b"one").unwrap();
    commit(&mut s, 2, 7, first, b"two").unwrap();
    let info = |s: &mut Client, items: Vec<(Value, Value)>| {
        let args = vec![Value::from(0), Value::from(-20), Value::Map(items)];
        done(call(s, "undoInfo", args)).as_array().unwrap().len()
    };
    assert_eq!(info(&mut s, vec![(Value::from("missing"), Value::Nil)]), 2);
    assert_eq!(
        info(&mut s, vec![(Value::from("missing"), Value::from(1))]),
        0
    );
    assert!(python_eq(&Value::from(1), &Value::F64(1.0)));
    assert!(python_eq(&Value::Boolean(true), &Value::from(1)));
    assert!(!python_eq(&Value::from("x"), &Value::Binary(b"x".to_vec())));
    assert!(!python_eq(&Value::from(2), &Value::F64(2.5)));
}

#[test]
fn the_vote_of_an_undo_of_many_oids_gives_each_oid_once() {
    const OIDS: u64 = 20_000;
    let mut s = registered("undo-many");
    begin(&mut s, 1, None);
    for oid in 1..=OIDS {
        store(&mut s, 1, oid, 0, b"one");
    }
    let tid = vote_and_finish(&mut s, 1).unwrap();
    let started = Instant::now();
    let reply = undo_in(&mut s, 2, Value::Binary(undo_id(Tid(tid)))).unwrap();
    let oids = reply.as_array().unwrap();
    assert_eq!(oids.len() as u64, OIDS);
    let distinct: HashSet<u64> = oids.iter().map(|oid| u64_of(oid).unwrap()).collect();
    assert_eq!(distinct.len() as u64, OIDS);
    // The undo is linear in its oids: a list scan for each oid would
    // take minutes in a debug build.
    assert!(started.elapsed() < std::time::Duration::from_secs(60));
}
