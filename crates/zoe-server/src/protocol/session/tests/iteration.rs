//! The tests of the reads of records and of the iterators: `history`,
//! `loadSerial`, `record_iternext`, the iterators and `restorea`.

use super::*;

#[test]
fn history_is_newest_first_and_bounded() {
    let mut s = registered("history");
    let mut tids = Vec::new();
    let mut serial = 0;
    for n in 0..4 {
        serial = commit(&mut s, n, 1, serial, &[b'p'; 10][..=n as usize]).unwrap();
        tids.push(serial);
    }
    let history = done(call(&mut s, "history", vec![p64(1), Value::from(3)]));
    let history = history.as_array().unwrap();
    let got: Vec<_> = history
        .iter()
        .map(|entry| u64_of(get(entry, "tid")).unwrap())
        .collect();
    assert_eq!(got, [tids[3], tids[2], tids[1]]);
    let newest = &history[0];
    assert_eq!(
        keys(newest),
        ["time", "user_name", "description", "tid", "size"]
    );
    assert_eq!(
        get(newest, "time"),
        &Value::F64(clock::tid_time(Tid(tids[3])))
    );
    assert_eq!(get(newest, "user_name"), &Value::Binary(b"user-3".to_vec()));
    assert_eq!(
        get(newest, "description"),
        &Value::Binary(b"description".to_vec())
    );
    assert_eq!(get(newest, "size"), &Value::from(4));

    let len = |reply: Value| reply.as_array().unwrap().len();
    // The size is 1 by default. A size of 0 or less gives no entry.
    assert_eq!(len(done(call(&mut s, "history", vec![p64(1)]))), 1);
    assert_eq!(
        len(done(call(&mut s, "history", vec![p64(1), Value::from(10)]))),
        4
    );
    for size in [0, -1] {
        let args = vec![p64(1), Value::from(size)];
        assert_eq!(len(done(call(&mut s, "history", args))), 0);
    }
    let fault = failed(call(&mut s, "history", vec![p64(9), Value::from(1)]));
    assert_eq!(fault.class, "ZODB.POSException.POSKeyError");
}

#[test]
fn history_of_an_oid_that_is_not_8_bytes_is_a_poskeyerror_before_the_size() {
    let mut s = registered("history-bad-oid");
    commit(&mut s, 1, 1, 0, b"data").unwrap();
    let oids = [
        Value::Nil,
        Value::from("\0\0\0\0\0\0\0\x01"),
        Value::Binary(vec![0; 7]),
    ];
    for oid in oids {
        for size in [Value::Nil, Value::from(1)] {
            let fault = failed(call(&mut s, "history", vec![oid.clone(), size]));
            assert_eq!(fault, Fault::no_object(oid.clone()));
        }
        let fault = failed(call(&mut s, "history", vec![oid.clone()]));
        assert_eq!(fault, Fault::no_object(oid));
    }
    // A good oid with a size that is not an int still fails with
    // `TypeError`.
    let fault = failed(call(&mut s, "history", vec![p64(1), Value::Nil]));
    assert_eq!(fault, Fault::type_error("size is not an int"));
    let fault = failed(call(&mut s, "history", vec![]));
    assert_eq!(fault.class, "builtins.TypeError");
}

#[test]
fn load_serial_reads_one_revision() {
    let mut s = registered("load-serial");
    let first = commit(&mut s, 1, 1, 0, b"first").unwrap();
    commit(&mut s, 2, 1, first, b"second").unwrap();
    assert_eq!(
        done(call(&mut s, "loadSerial", vec![p64(1), p64(first)])),
        Value::Binary(b"first".to_vec())
    );
    let fault = failed(call(&mut s, "loadSerial", vec![p64(1), p64(0)]));
    assert_eq!(fault, Fault::from(ZoeError::NotFound { oid: Oid(1) }));
}

#[test]
fn record_iternext_walks_the_current_records() {
    let mut s = registered("record-iternext");
    let fault = failed(call(&mut s, "record_iternext", vec![Value::Nil]));
    assert_eq!(fault.class, "builtins.ValueError");

    let first = commit(&mut s, 1, 1, 0, b"one").unwrap();
    let third = commit(&mut s, 2, 3, 0, b"three").unwrap();
    let later = commit(&mut s, 3, 1, first, b"later").unwrap();
    assert_eq!(
        done(call(&mut s, "record_iternext", vec![Value::Nil])),
        Value::Array(vec![
            p64(1),
            p64(later),
            Value::Binary(b"later".to_vec()),
            p64(3)
        ])
    );
    assert_eq!(
        done(call(&mut s, "record_iternext", vec![p64(2)])),
        Value::Array(vec![
            p64(3),
            p64(third),
            Value::Binary(b"three".to_vec()),
            Value::Nil
        ])
    );
}

/// The items of the array `value`.
fn items(value: &Value) -> &[Value] {
    value.as_array().unwrap()
}

#[test]
fn the_iterators_give_transactions_and_records_as_zeo_does() {
    let mut s = registered("iterators");
    let ext = Value::Map(vec![(Value::from("note"), Value::from("first"))]);
    let first = commit_with(&mut s, 1, ext.clone(), &[(1, 0), (2, 0)], MAPPING);
    let second = {
        begin(&mut s, 2, None);
        let delete = vec![p64(2), p64(first), Value::from(2)];
        done(send(&mut s, "deleteObject", delete));
        vote_and_finish(&mut s, 2).unwrap()
    };

    let iid = done(call(&mut s, "iterator_start", vec![Value::Nil, Value::Nil]));
    assert_eq!(iid, Value::from(0));
    let txn = done(call(&mut s, "iterator_next", vec![iid.clone()]));
    assert_eq!(
        txn,
        Value::Array(vec![
            p64(first),
            Value::from(" "),
            Value::Binary(b"user".to_vec()),
            Value::Binary(b"description".to_vec()),
            ext,
        ])
    );
    let rid = done(call(
        &mut s,
        "iterator_record_start",
        vec![iid.clone(), p64(first)],
    ));
    assert_eq!(rid, Value::from(1));
    for oid in [1, 2] {
        let record = done(call(&mut s, "iterator_record_next", vec![rid.clone()]));
        assert_eq!(
            record,
            Value::Array(vec![
                p64(oid),
                p64(first),
                Value::Binary(MAPPING.to_vec()),
                Value::Nil
            ])
        );
    }
    assert_eq!(
        done(call(&mut s, "iterator_record_next", vec![rid.clone()])),
        Value::Nil
    );
    // A record iterator at its end is gone.
    let fault = failed(call(&mut s, "iterator_record_next", vec![rid.clone()]));
    assert_eq!(fault, Fault::no_iterator(&rid));
    assert_eq!(fault.args, Value::Array(vec![Value::from(1)]));

    // An empty extension is an empty map, and a deletion has nil data.
    let txn = done(call(&mut s, "iterator_next", vec![iid.clone()]));
    assert_eq!(items(&txn)[0], p64(second));
    assert_eq!(items(&txn)[4], Value::Map(vec![]));
    let out_of_order = call(
        &mut s,
        "iterator_record_start",
        vec![iid.clone(), p64(first)],
    );
    assert_eq!(failed(out_of_order).class, "builtins.Exception");
    let rid = done(call(
        &mut s,
        "iterator_record_start",
        vec![iid.clone(), p64(second)],
    ));
    assert_eq!(rid, Value::from(2));
    let record = done(call(&mut s, "iterator_record_next", vec![rid]));
    assert_eq!(
        record,
        Value::Array(vec![p64(2), p64(second), Value::Nil, Value::Nil])
    );
    assert_eq!(
        done(call(&mut s, "iterator_next", vec![iid.clone()])),
        Value::Nil
    );
    let fault = failed(call(&mut s, "iterator_next", vec![iid.clone()]));
    assert_eq!(fault, Fault::no_iterator(&iid));

    // The bounds of the range are tids, both included.
    let range = |s: &mut Client, start: Value, stop: Value| {
        let iid = done(call(s, "iterator_start", vec![start, stop]));
        std::iter::from_fn(|| {
            let txn = done(call(s, "iterator_next", vec![iid.clone()]));
            (!txn.is_nil()).then(|| u64_of(&items(&txn)[0]).unwrap())
        })
        .collect::<Vec<_>>()
    };
    assert_eq!(range(&mut s, p64(second), Value::Nil), [second]);
    assert_eq!(range(&mut s, Value::Nil, p64(first)), [first]);
    let fault = failed(call(
        &mut s,
        "iterator_start",
        vec![Value::from(1), Value::Nil],
    ));
    assert_eq!(fault.class, "builtins.TypeError");
}

#[test]
fn an_extension_that_msgpack_cannot_carry_fails_iterator_next() {
    let mut s = registered("bad-extension");
    // A restore transaction with the extension of a class instance.
    let ext = b"\x80\x03}q\x00X\x01\x00\x00\x00kcdatetime\ndate\nq\x01C\x04\x07\xea\x0a\x0cq\x02\x85Rq\x03s.";
    let mut txn = Txn::begin(Arc::clone(&s.store), b"", b"", ext);
    txn.store(Oid(1), Tid::ZERO, b"a").unwrap();
    txn.vote().unwrap();
    let tid = txn.finish().unwrap();
    let iid = done(call(&mut s, "iterator_start", vec![Value::Nil, Value::Nil]));
    let fault = failed(call(&mut s, "iterator_next", vec![iid.clone()]));
    assert_eq!(fault.class, "builtins.ValueError");
    let Value::Array(args) = &fault.args else {
        panic!("{fault:?}");
    };
    assert!(
        args[0]
            .as_str()
            .unwrap()
            .contains(&format!("{:016x}", tid.0)),
        "{fault:?}"
    );
    // The iterator stays, and it is at its end.
    assert_eq!(done(call(&mut s, "iterator_next", vec![iid])), Value::Nil);
}

#[test]
fn iterator_gc_ignores_the_ids_that_it_does_not_know() {
    let mut s = registered("iterator-gc");
    commit(&mut s, 1, 1, 0, b"a").unwrap();
    let iid = done(call(&mut s, "iterator_start", vec![Value::Nil, Value::Nil]));
    let other = done(call(&mut s, "iterator_start", vec![Value::Nil, Value::Nil]));
    // Ids that never existed, an id of no int type, and a known id.
    let ids = vec![
        Value::from(5),
        Value::from(-1),
        Value::from("x"),
        iid.clone(),
        iid.clone(),
    ];
    let gc = send(&mut s, "iterator_gc", vec![Value::Array(ids)]);
    assert!(matches!(gc, Outcome::Done(Value::Nil)), "{gc:?}");
    assert_eq!(
        failed(call(&mut s, "iterator_next", vec![iid.clone()])),
        Fault::no_iterator(&iid)
    );
    assert!(!done(call(&mut s, "iterator_next", vec![other])).is_nil());
    // A gc without a list is an error, so the call without a reply
    // closes the connection.
    let gc = send(&mut s, "iterator_gc", vec![Value::from(1)]);
    assert!(matches!(gc, Outcome::Failed(_)), "{gc:?}");
}

#[test]
fn iterator_ids_belong_to_their_connection() {
    let storage = storage("iterator-ids", Queues::DEFAULT);
    let (mut s, _frames) = connected(&storage);
    let (mut other, _other_frames) = connected(&storage);
    commit(&mut s, 1, 1, 0, b"a").unwrap();
    let start = vec![Value::Nil, Value::Nil];
    let iid = done(call(&mut s, "iterator_start", start.clone()));
    // Each connection counts its own ids from 0.
    assert_eq!(done(call(&mut other, "iterator_start", start)), iid);
    send(
        &mut other,
        "iterator_gc",
        vec![Value::Array(vec![iid.clone()])],
    );
    assert!(!done(call(&mut s, "iterator_next", vec![iid.clone()])).is_nil());
    assert_eq!(
        failed(call(&mut other, "iterator_next", vec![iid.clone()])),
        Fault::no_iterator(&iid)
    );
}

#[test]
fn a_connection_refuses_an_iterator_over_the_bound() {
    let mut s = registered("iterator-bound");
    let start = vec![Value::Nil, Value::Nil];
    for _ in 0..crate::protocol::iteration::MAX_OPEN {
        done(call(&mut s, "iterator_start", start.clone()));
    }
    let fault = failed(call(&mut s, "iterator_start", start.clone()));
    assert_eq!(fault.class, "ZODB.POSException.StorageError");
    // An iterator over no transaction ends at its first call. Then the
    // connection takes a new iterator.
    assert_eq!(
        done(call(&mut s, "iterator_next", vec![Value::from(0)])),
        Value::Nil
    );
    assert_eq!(
        done(call(&mut s, "iterator_start", start)),
        Value::from(crate::protocol::iteration::MAX_OPEN as u64)
    );
}

#[test]
fn restorea_copies_a_record_only_in_a_restore_transaction() {
    let mut s = registered("restorea");
    let restore = |s: &mut Client, id: i64, oid: u64, data: Value| {
        let args = vec![p64(oid), p64(0), data, Value::Nil, Value::from(id)];
        send(s, "restorea", args)
    };
    // A transaction without a tid fails its vote.
    begin(&mut s, 1, None);
    let data = Value::Binary(b"a".to_vec());
    assert!(matches!(
        restore(&mut s, 1, 1, data.clone()),
        Outcome::Done(Value::Nil)
    ));
    let fault = vote_and_finish(&mut s, 1).unwrap_err();
    assert_eq!(fault.class, TRANSACTION_ERROR);

    // A restore transaction commits the record under its tid.
    let wanted = s.store.last_transaction().0 + 10;
    begin(&mut s, 2, Some(wanted));
    restore(&mut s, 2, 1, data);
    assert_eq!(vote_and_finish(&mut s, 2), Ok(wanted));
    let load = done(call(&mut s, "loadSerial", vec![p64(1), p64(wanted)]));
    assert_eq!(load, Value::Binary(b"a".to_vec()));
    let stats = s.storage.status().to_value();
    assert_eq!(count(&stats, "stores"), 2);

    // A transaction that a pack of FileStorage changed has the status
    // "p". It copies as any other.
    let packed = vec![
        Value::from(4),
        Value::Nil,
        Value::Nil,
        Value::Map(vec![]),
        p64(wanted + 1),
        Value::from("p"),
    ];
    done(call(&mut s, "tpc_begin", packed));
    restore(&mut s, 4, 2, Value::Binary(b"b".to_vec()));
    assert_eq!(vote_and_finish(&mut s, 4), Ok(wanted + 1));

    // Nil data copies a deletion.
    begin(&mut s, 3, Some(wanted + 2));
    restore(&mut s, 3, 1, Value::Nil);
    assert_eq!(vote_and_finish(&mut s, 3), Ok(wanted + 2));
    let fault = failed(call(&mut s, "loadBefore", vec![p64(1), p64(u64::MAX)]));
    assert_eq!(fault, Fault::from(ZoeError::NotFound { oid: Oid(1) }));

    // A restorea of a transaction that is not the open one fails, and
    // so the connection closes.
    let outcome = restore(&mut s, 9, 1, Value::Nil);
    assert!(matches!(outcome, Outcome::Failed(_)), "{outcome:?}");
}

#[test]
fn a_restore_transaction_publishes_its_commit_through_the_hub() {
    let storage = storage("restore-publish", Queues::DEFAULT);
    let (mut writer, mut own) = connected(&storage);
    let (_reader, mut others) = connected(&storage);
    let wanted = storage.store.last_transaction().0 + 10;
    begin(&mut writer, 1, Some(wanted));
    let args = vec![
        p64(1),
        p64(0),
        Value::Binary(b"a".to_vec()),
        Value::Nil,
        Value::from(1),
    ];
    done(send(&mut writer, "restorea", args));
    assert_eq!(vote_and_finish(&mut writer, 1), Ok(wanted));

    let own = messages(&mut own);
    let methods: Vec<&str> = own.iter().map(|(method, _)| method.as_str()).collect();
    assert_eq!(methods, ["info", ".reply"]);
    assert_eq!(own[1].1, p64(wanted));
    assert_eq!(messages(&mut others), [invalidation(wanted, &[1])]);
    assert_eq!(storage.invalidations.published(), Tid(wanted));
}
