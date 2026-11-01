//! The tests of the calls of a transaction that commits: `tpc_begin`,
//! the stores, the serials, the intent locks and the metrics of a commit.

use super::*;

#[test]
fn new_oids_gives_at_most_100_and_at_least_1() {
    let mut s = registered("new-oids");
    let count = |v: Value| v.as_array().unwrap().len();
    assert_eq!(count(done(call(&mut s, "new_oids", vec![]))), 100);
    assert_eq!(
        count(done(call(&mut s, "new_oids", vec![Value::from(500)]))),
        100
    );
    assert_eq!(
        count(done(call(&mut s, "new_oids", vec![Value::from(0)]))),
        1
    );
}

#[test]
fn a_failed_store_fails_the_vote_and_ends_the_transaction() {
    let mut s = registered("failed-store");
    begin(&mut s, 9, None);
    // The core refuses an empty pickle. The calls after it do nothing.
    store(&mut s, 9, 1, 0, b"");
    store(&mut s, 9, 2, 0, b"a");
    let fault = failed(call(&mut s, "vote", vec![Value::from(9)]));
    assert_eq!(fault.class, TRANSACTION_ERROR);
    assert!(s.txn.open.is_none());
    // As in ZEO, a finish or an abort of no transaction gives None.
    assert_eq!(
        done(call(&mut s, "tpc_finish", vec![Value::from(9)])),
        Value::Nil
    );
    assert_eq!(
        done(call(&mut s, "tpc_abort", vec![Value::from(9)])),
        Value::Nil
    );
    let load = vec![p64(2), p64(u64::MAX)];
    assert_eq!(
        failed(call(&mut s, "loadBefore", load)).class,
        "ZODB.POSException.POSKeyError"
    );
}

#[test]
fn an_error_of_tpc_begin_fails_the_vote_and_keeps_the_connection() {
    let mut s = registered("begin-errors");
    let first = commit(&mut s, 1, 1, 0, b"a").unwrap();
    let bad_status = vec![
        Value::from(2),
        Value::Nil,
        Value::Nil,
        Value::Map(vec![]),
        Value::Nil,
        Value::from("u"),
    ];
    let bad_extension = vec![Value::from(2), Value::Nil, Value::Nil, Value::from(3)];
    let old_tid = vec![
        Value::from(2),
        Value::Nil,
        Value::Nil,
        Value::Map(vec![]),
        p64(first),
    ];
    let short_tid = vec![
        Value::from(2),
        Value::Nil,
        Value::Nil,
        Value::Map(vec![]),
        Value::Binary(vec![1]),
    ];
    for args in [bad_status, bad_extension, old_tid, short_tid] {
        assert_eq!(done(call(&mut s, "tpc_begin", args.clone())), Value::Nil);
        store(&mut s, 2, 1, first, b"b");
        let fault = failed(call(&mut s, "vote", vec![Value::from(2)]));
        assert!(
            [TRANSACTION_ERROR, "builtins.TypeError"].contains(&fault.class),
            "{args:?}: {fault:?}"
        );
        assert_eq!(
            done(call(&mut s, "tpc_abort", vec![Value::from(2)])),
            Value::Nil
        );
    }
    // Nothing changed, and the session takes the next transaction.
    assert_eq!(s.store.last_transaction(), Tid(first));
    commit(&mut s, 3, 1, first, b"c").unwrap();
}

#[test]
fn a_call_of_another_transaction_names_both_ids() {
    let mut s = registered("other-transaction");
    let fault = failed(call(&mut s, "vote", vec![Value::from(7)]));
    assert_eq!(fault, Fault::not_current(None, &Value::from(7)));
    assert_eq!(fault.args, Value::Array(vec![Value::Nil, Value::from(7)]));

    begin(&mut s, 1, None);
    // A second tpc_begin, of this transaction or of another one, does
    // nothing.
    begin(&mut s, 1, None);
    begin(&mut s, 2, None);
    let fault = failed(call(&mut s, "vote", vec![Value::from(2)]));
    assert_eq!(
        fault.args,
        Value::Array(vec![Value::from(1), Value::from(2)])
    );
    for method in ["storea", "checkCurrentSerialInTransaction", "deleteObject"] {
        let mut args = vec![p64(1), p64(0), Value::Binary(b"a".to_vec())];
        if method != "storea" {
            args.pop();
        }
        args.push(Value::from(2));
        let fault = failed(call(&mut s, method, args));
        assert_eq!(fault.class, TRANSACTION_ERROR, "{method}");
    }
    assert_eq!(
        done(call(&mut s, "tpc_finish", vec![Value::from(2)])),
        Value::Nil
    );
    assert_eq!(
        done(call(&mut s, "tpc_abort", vec![Value::from(2)])),
        Value::Nil
    );
    // The transaction 1 is still open.
    store(&mut s, 1, 1, 0, b"a");
    vote_and_finish(&mut s, 1).unwrap();
}

#[test]
fn delete_object_writes_a_deletion_with_the_serial_check_of_storea() {
    let mut s = registered("delete");
    let first = commit(&mut s, 1, 1, 0, MAPPING).unwrap();
    // A stale serial is a conflict. A deletion has no class.
    begin(&mut s, 2, None);
    let delete = vec![p64(1), p64(0), Value::from(2)];
    assert_eq!(done(call(&mut s, "deleteObject", delete)), Value::Nil);
    let fault = vote_and_finish(&mut s, 2).unwrap_err();
    assert_eq!(
        fault,
        Fault::conflict(
            CONFLICT_ERROR,
            CONFLICT,
            Oid(1),
            Some((Tid(first), Tid(0))),
            None
        )
    );

    begin(&mut s, 3, None);
    let delete = vec![p64(1), p64(first), Value::from(3)];
    done(call(&mut s, "deleteObject", delete));
    let deleted = vote_and_finish(&mut s, 3).unwrap();
    assert!(deleted > first);
    let load = vec![p64(1), p64(u64::MAX)];
    let fault = failed(call(&mut s, "loadBefore", load));
    assert_eq!(fault, Fault::from(ZoeError::NotFound { oid: Oid(1) }));
    // The revision before the deletion still loads.
    let load = vec![p64(1), p64(deleted)];
    let loaded = done(call(&mut s, "loadBefore", load));
    assert_eq!(
        loaded,
        Value::Array(vec![
            Value::Binary(MAPPING.to_vec()),
            p64(first),
            p64(deleted)
        ])
    );
    let load = vec![p64(1), p64(first)];
    let loaded = done(call(&mut s, "loadSerial", load));
    assert_eq!(loaded, Value::Binary(MAPPING.to_vec()));
    let load = vec![p64(1), p64(deleted)];
    let fault = failed(call(&mut s, "loadSerial", load));
    assert_eq!(fault, Fault::from(ZoeError::NotFound { oid: Oid(1) }));
    // `record_iternext` skips the deleted oid.
    let fault = failed(call(&mut s, "record_iternext", vec![Value::Nil]));
    assert_eq!(fault.class, "builtins.ValueError");
    let kept = commit(&mut s, 4, 2, 0, MAPPING).unwrap();
    let current = done(call(&mut s, "record_iternext", vec![Value::Nil]));
    assert_eq!(
        current,
        Value::Array(vec![
            p64(2),
            p64(kept),
            Value::Binary(MAPPING.to_vec()),
            Value::Nil
        ])
    );
}

#[test]
fn tpc_begin_with_a_tid_commits_under_that_tid() {
    let mut s = registered("restore");
    let first = commit(&mut s, 1, 1, 0, b"a").unwrap();
    let wanted = first + 2;
    begin(&mut s, 2, Some(wanted));
    store(&mut s, 2, 1, first, b"b");
    assert_eq!(vote_and_finish(&mut s, 2), Ok(wanted));
    assert_eq!(s.store.last_transaction(), Tid(wanted));
    // The serial check of storea holds in a restore transaction: the
    // vote lists the conflict, and a second vote without a resolved
    // pickle fails.
    begin(&mut s, 3, Some(wanted + 1));
    store(&mut s, 3, 1, first, b"c");
    let listed = done(call(&mut s, "vote", vec![Value::from(3)]));
    assert_eq!(listed.as_array().map(Vec::len), Some(1));
    let fault = failed(call(&mut s, "vote", vec![Value::from(3)]));
    assert_eq!(fault.class, CONFLICT_ERROR);
}

#[test]
fn the_calls_that_wait_at_the_end_of_a_connection_still_run() {
    let storage = storage("calls-at-end", Queues::DEFAULT);
    let (calls, received) = tokio::sync::mpsc::channel(16);
    let (replies, _inbox, _closing) = Outbox::new(16, 16);
    let send = |oneway: bool, method: &str, args: Vec<Value>| {
        let call = Call {
            id: Value::from(1),
            oneway,
            method: method.into(),
            args,
            context: None,
        };
        calls.blocking_send(call).unwrap();
    };
    let oids = [1, 2, 3];
    send(
        false,
        "register",
        vec![Value::from("1"), Value::Boolean(false)],
    );
    let begin = vec![Value::from(7), Value::Nil, Value::Nil, Value::Map(vec![])];
    send(true, "tpc_begin", begin);
    for oid in oids {
        let data = Value::Binary(MAPPING.to_vec());
        send(true, "storea", vec![p64(oid), p64(0), data, Value::from(7)]);
    }
    send(false, "vote", vec![Value::from(7)]);
    send(false, "tpc_finish", vec![Value::from(7)]);
    // The connection ends before the session runs the first call.
    drop(calls);
    run(serving(Arc::clone(&storage)), received, replies);

    // The session ran each call, so the finish committed the three oids
    // together, and no lock stays.
    let tid = storage.invalidations.published();
    assert_ne!(tid, Tid(0));
    for oid in oids {
        assert_eq!(storage.store.load(Oid(oid)).unwrap().1, tid);
    }
    assert!(storage.store.lock_table_is_empty());
    let status = storage.status().to_value();
    assert_eq!(count(&status, "commits"), 1);
    assert_eq!(count(&status, "active_txns"), 0);
}

fn lock_intent(s: &mut Client, oids: &[u64]) -> Outcome {
    let oids = oids.iter().map(|&oid| p64(oid)).collect();
    call(s, "lock_intent", vec![Value::Array(oids)])
}

#[test]
fn lock_intent_needs_the_declaration_and_gives_the_current_serials() {
    let mut stock = registered("intent-stock");
    let fault = failed(lock_intent(&mut stock, &[1]));
    assert_eq!(fault.class, "builtins.ValueError");
    let storage = storage("intent-serials", Queues::DEFAULT);
    let mut s = declaring_on(&storage, &["lock_intent"]);
    let one = commit(&mut s, 1, 1, 0, MAPPING).unwrap();
    let serials = done(lock_intent(&mut s, &[1, 2]));
    assert_eq!(serials, Value::Array(vec![p64(one), p64(0)]));
    assert!(s.txn.is_open());
    // A lower oid, or one that the session holds, fails and takes
    // nothing.
    for oids in [&[1][..], &[2], &[0]] {
        let fault = failed(lock_intent(&mut s, oids));
        assert_eq!(fault.class, TRANSACTION_ERROR, "{oids:?}");
    }
    assert_eq!(done(call(&mut s, "unlock_intent", vec![])), Value::Nil);
    assert!(!s.txn.is_open());
}

#[test]
fn the_next_transaction_adopts_the_intent_locks_of_the_session() {
    let storage = storage("intent-adopt", Queues::DEFAULT);
    let mut s = declaring_on(&storage, &["lock_intent"]);
    let mut other = declaring_on(&storage, &["lock_intent"]);
    let one = commit(&mut s, 1, 1, 0, MAPPING).unwrap();
    done(lock_intent(&mut s, &[1]));
    // The vote of the session takes its own intent lock at once.
    let voted = store_and_vote(&mut s, 2, &[(1, one)], MAPPING);
    assert!(matches!(voted, Outcome::Done(_)), "{voted:?}");
    // No lock_intent in an open transaction.
    let fault = failed(lock_intent(&mut s, &[5]));
    assert_eq!(fault.class, TRANSACTION_ERROR);
    let two = finish(&mut s, Value::from(2));
    // The end of the transaction released the lock: another session
    // takes it at once, and reads the commit.
    assert_eq!(
        done(lock_intent(&mut other, &[1])),
        Value::Array(vec![p64(two)])
    );
}

#[test]
fn a_read_only_session_takes_no_intent_lock() {
    let mut s = declaring("intent-read-only", true, &["lock_intent"]);
    assert_eq!(failed(lock_intent(&mut s, &[1])), Fault::read_only());
}

#[test]
fn a_session_that_holds_intent_locks_past_the_bound_loses_them() {
    let storage = storage("intent-bound", Queues::DEFAULT);
    let mut s = declaring_on(&storage, &["lock_intent"]);
    let mut other = declaring_on(&storage, &["lock_intent"]);
    done(lock_intent(&mut s, &[3]));
    assert!(
        s.txn
            .intent_hold_left()
            .is_some_and(|left| left <= INTENT_HOLD)
    );
    s.txn.drop_intents();
    assert!(!s.txn.is_open());
    assert_eq!(s.txn.intent_hold_left(), None);
    done(lock_intent(&mut other, &[3]));
}

#[test]
fn a_begin_that_fails_releases_the_intent_locks_of_the_session() {
    let storage = storage("intent-failed-begin", Queues::DEFAULT);
    let mut s = declaring_on(&storage, &["lock_intent"]);
    done(lock_intent(&mut s, &[3]));
    // A status that the server does not support fails the begin.
    let begin = vec![
        Value::from(7),
        Value::Nil,
        Value::Nil,
        Value::Map(vec![]),
        Value::Nil,
        Value::from("u"),
    ];
    done(call(&mut s, "tpc_begin", begin));
    assert!(!s.txn.holds_intents());
    assert!(storage.store.lock_table_is_empty());
}

#[test]
fn the_extension_of_a_get_or_a_head_marks_the_transaction() {
    let ext = |method: Value| Value::Map(vec![(Value::from(REQUEST_METHOD), method)]);
    assert!(of_get(&ext(Value::from("GET"))));
    assert!(of_get(&ext(Value::from("head"))));
    assert!(!of_get(&ext(Value::from("POST"))));
    assert!(!of_get(&ext(Value::Binary(b"GET".to_vec()))));
    assert!(!of_get(&Value::Map(vec![])));
    assert!(!of_get(&Value::Nil));
}

#[test]
fn errors_have_the_class_names_of_zodb() {
    let fault = Fault::from(ZoeError::NotFound { oid: Oid(3) });
    assert_eq!(fault.class, "ZODB.POSException.POSKeyError");
    assert_eq!(fault.args, Value::Array(vec![p64(3)]));
    assert_eq!(Fault::from(ZoeError::ReadOnly), Fault::read_only());
    assert_eq!(
        Fault::from(ZoeError::Halted).class,
        "ZODB.POSException.StorageSystemError"
    );
    assert_eq!(
        Fault::from(ZoeError::Protocol("no")).args,
        Value::Array(vec![Value::from("two-phase commit out of order: no")])
    );
}

#[test]
fn the_metrics_count_commits_votes_and_raised_conflicts() {
    use std::sync::atomic::Ordering;

    let mut s = registered("metrics");
    let metrics = Arc::clone(&s.storage.metrics);
    let get = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
    let one = commit(&mut s, 1, 1, 0, MAPPING).unwrap();
    commit(&mut s, 2, 1, one, MAPPING).unwrap();
    assert_eq!(get(&metrics.commits), 2);

    // A listed conflict that the client aborts is raised in the client.
    done(store_and_vote(&mut s, 3, &[(1, one)], MAPPING));
    done(call(&mut s, "tpc_abort", vec![Value::from(3)]));
    assert_eq!(get(&metrics.conflicts_raised), 1);
    // So is a vote that fails with a conflict: a new object over a
    // stale serial is never listed.
    let fault = commit(&mut s, 4, 9, 1, MAPPING).unwrap_err();
    assert!(fault.is_conflict(), "{fault:?}");
    assert_eq!(get(&metrics.conflicts_raised), 2);
    // A listed conflict that the end of the connection leaves.
    done(store_and_vote(&mut s, 5, &[(1, one)], MAPPING));
    let text = |storage: &NamedStorage| crate::protocol::metrics::render(&[storage.exported()]);
    let storage = Arc::clone(&s.storage);
    drop(s);
    assert_eq!(get(&metrics.conflicts_raised), 3);
    assert_eq!(get(&metrics.commits), 2);
    let text = text(&storage);
    // Five first votes, each with its locks, two finishes, and two syncs
    // of one commit each.
    for line in [
        "zoe_vote_latency_seconds_count{storage=\"1\"} 5\n",
        "zoe_finish_latency_seconds_count{storage=\"1\"} 2\n",
        "zoe_fsync_batch_size_bucket{storage=\"1\",le=\"1\"} 2\n",
        "zoe_lock_wait_seconds_count{storage=\"1\"} 5\n",
        "zoe_clients_connected{storage=\"1\"} 0\n",
    ] {
        assert!(text.contains(line), "no {line} in:\n{text}");
    }
}

#[test]
fn a_nil_serial_is_the_serial_of_a_new_object() {
    let mut s = session("nil-serial");
    done(call(
        &mut s,
        "register",
        vec![Value::from("1"), Value::Boolean(false)],
    ));

    // A new oid commits with a nil serial.
    begin(&mut s, 1, None);
    store_with(&mut s, 1, 1, Value::Nil, MAPPING);
    let first = vote_and_finish(&mut s, 1).unwrap();
    let load = done(call(&mut s, "loadBefore", vec![p64(1), p64(u64::MAX)]));
    assert_eq!(
        load,
        Value::Array(vec![
            Value::Binary(MAPPING.to_vec()),
            p64(first),
            Value::Nil
        ])
    );

    // An oid with a record fails the vote, as with a zero serial. The
    // core does not hand the conflict back for the client to resolve.
    begin(&mut s, 2, None);
    store_with(&mut s, 2, 1, Value::Nil, MAPPING);
    let fault = vote_and_finish(&mut s, 2).unwrap_err();
    assert_eq!(
        fault,
        Fault::conflict(
            CONFLICT_ERROR,
            CONFLICT,
            Oid(1),
            Some((Tid(first), Tid(0))),
            Some("persistent.mapping.PersistentMapping".into())
        )
    );
    assert_eq!(
        done(call(&mut s, "tpc_abort", vec![Value::from(2)])),
        Value::Nil
    );
    assert_eq!(
        u64_of(&done(call(&mut s, "getTid", vec![p64(1)]))).unwrap(),
        first
    );

    // A serial that is not nil must still be 8 bytes.
    for (id, serial) in [(3, Value::from("x")), (4, Value::Binary(vec![0; 7]))] {
        begin(&mut s, id, None);
        store_with(&mut s, id, 2, serial, MAPPING);
        let fault = vote_and_finish(&mut s, id).unwrap_err();
        assert_eq!(fault.class, "builtins.TypeError");
    }
}

#[test]
fn a_str_serial_of_8_bytes_reads_as_those_bytes() {
    let mut s = registered("str-serial");
    let zero = Value::from("\0".repeat(8));

    // A new oid commits with the str serial of a new object.
    begin(&mut s, 1, None);
    store_with(&mut s, 1, 1, zero.clone(), MAPPING);
    let first = vote_and_finish(&mut s, 1).unwrap();
    assert_eq!(
        u64_of(&done(call(&mut s, "getTid", vec![p64(1)]))).unwrap(),
        first
    );

    // An oid with a record fails the vote, as with the zero tid in
    // bytes.
    begin(&mut s, 2, None);
    store_with(&mut s, 2, 1, zero.clone(), MAPPING);
    let fault = vote_and_finish(&mut s, 2).unwrap_err();
    assert_eq!(
        fault,
        Fault::conflict(
            CONFLICT_ERROR,
            CONFLICT,
            Oid(1),
            Some((Tid(first), Tid(0))),
            Some("persistent.mapping.PersistentMapping".into())
        )
    );

    // `checkCurrentSerialInTransaction` and `deleteObject` read the
    // same serial.
    begin(&mut s, 3, None);
    let check = vec![p64(1), zero.clone(), Value::from(3)];
    done(call(&mut s, "checkCurrentSerialInTransaction", check));
    let fault = vote_and_finish(&mut s, 3).unwrap_err();
    assert_eq!(fault.class, READ_CONFLICT_ERROR);
    begin(&mut s, 4, None);
    done(call(
        &mut s,
        "deleteObject",
        vec![p64(1), zero.clone(), Value::from(4)],
    ));
    let fault = vote_and_finish(&mut s, 4).unwrap_err();
    assert_eq!(fault.class, CONFLICT_ERROR);

    // A str of another length, and an oid as a str, still fail.
    let not_bytes = Fault::type_error("an oid or a tid is not bytes");
    begin(&mut s, 5, None);
    store_with(&mut s, 5, 2, Value::from("\0".repeat(7)), MAPPING);
    assert_eq!(vote_and_finish(&mut s, 5).unwrap_err(), not_bytes);
    begin(&mut s, 6, None);
    let args = vec![
        zero.clone(),
        zero,
        Value::Binary(MAPPING.to_vec()),
        Value::from(6),
    ];
    done(call(&mut s, "storea", args));
    assert_eq!(vote_and_finish(&mut s, 6).unwrap_err(), not_bytes);

    // The UTF-8 bytes of the str are the serial.
    let text = "\u{e9}".repeat(4);
    let bytes = <[u8; 8]>::try_from(text.as_bytes()).unwrap();
    assert_eq!(
        serial_of(&Value::from(text)),
        Ok(Tid(u64::from_be_bytes(bytes)))
    );
    assert_eq!(store_serial(&Value::Nil), Ok(Tid::ZERO));
}
