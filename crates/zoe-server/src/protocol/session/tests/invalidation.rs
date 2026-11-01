//! The tests of the publication of the commits: the invalidations, the
//! order of the loads and `getTid` among them, and `getInvalidations`.

use super::*;

#[test]
fn the_other_sessions_get_the_invalidation_and_the_committer_its_reply() {
    let storage = storage("publish", Queues::DEFAULT);
    let (mut writer, mut own) = connected(&storage);
    let (_reader, mut others) = connected(&storage);
    let first = commit(&mut writer, 1, 1, 0, b"x").unwrap();

    let own = messages(&mut own);
    let methods: Vec<&str> = own.iter().map(|(method, _)| method.as_str()).collect();
    assert_eq!(methods, ["info", ".reply"]);
    let info = &own[0].1.as_array().unwrap()[0];
    assert_eq!(keys(info), ["length", "size"]);
    assert_eq!(own[1].1, p64(first));
    assert_eq!(messages(&mut others), [invalidation(first, &[1])]);

    // The reader registers after the commit: it gets the later commit.
    let (_late, mut late) = connected(&storage);
    let second = commit(&mut writer, 2, 2, 0, b"y").unwrap();
    assert_eq!(messages(&mut others), [invalidation(second, &[2])]);
    assert_eq!(messages(&mut late), [invalidation(second, &[2])]);
}

#[test]
fn a_load_reply_goes_out_before_the_invalidation_of_a_later_commit() {
    let storage = storage("load-order", Queues::DEFAULT);
    let (mut writer, _own) = connected(&storage);
    let (mut reader, mut frames) = connected(&storage);
    let first = commit(&mut writer, 1, 1, 0, b"old").unwrap();
    assert_eq!(messages(&mut frames), [invalidation(first, &[1])]);

    let current = done(call(&mut reader, "loadBefore", vec![p64(1), p64(u64::MAX)]));
    assert_eq!(current.as_array().unwrap()[2], Value::Nil);
    assert_eq!(reader.sent, Some(true));
    // A commit after the read goes out after the reply.
    let second = commit(&mut writer, 2, 1, first, b"new").unwrap();
    let messages = messages(&mut frames);
    assert_eq!(messages[0].0, ".reply");
    assert_eq!(messages[1], invalidation(second, &[1]));
}

#[test]
fn a_load_shows_only_the_commits_that_the_hub_published() {
    let storage = storage("load-published", Queues::DEFAULT);
    let (mut writer, _own) = connected(&storage);
    let (mut reader, mut frames) = connected(&storage);
    let first = commit(&mut writer, 1, 1, 0, b"old").unwrap();
    assert_eq!(messages(&mut frames), [invalidation(first, &[1])]);

    // A commit that the store made visible and the hub did not publish
    // yet, as while its session reads its oids.
    let mut txn = storage.store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(Oid(1), Tid(first), b"new").unwrap();
    txn.vote().unwrap();
    let hidden = txn.finish_after().unwrap();
    assert_eq!(hidden.previous, Tid(first));

    // The current record is the published one, with no next serial.
    let old = |next: Value| Value::Array(vec![Value::Binary(b"old".to_vec()), p64(first), next]);
    let load = vec![p64(1), p64(u64::MAX)];
    let current = done(call(&mut reader, "loadBefore", load.clone()));
    assert_eq!(current, old(Value::Nil));
    // A load before the hidden commit does not name it.
    let before = vec![p64(1), p64(hidden.tid.0)];
    let previous = done(call(&mut reader, "loadBefore", before.clone()));
    assert_eq!(previous, old(Value::Nil));

    // The hub publishes the commit: its invalidation comes after the
    // replies, and later loads show it.
    storage.invalidations.publish(Commit {
        previous: hidden.previous,
        tid: hidden.tid,
        origin: None,
        outbox: outbox(),
        own: Vec::new(),
        others: Others::Send(message::notify(
            "invalidateTransaction",
            vec![p64(hidden.tid.0), Value::Array(vec![p64(1)])],
        )),
    });
    let current = done(call(&mut reader, "loadBefore", load));
    let new = Value::Array(vec![
        Value::Binary(b"new".to_vec()),
        p64(hidden.tid.0),
        Value::Nil,
    ]);
    assert_eq!(current, new);
    let previous = done(call(&mut reader, "loadBefore", before));
    assert_eq!(previous, old(p64(hidden.tid.0)));
    let methods: Vec<String> = messages(&mut frames).into_iter().map(|(m, _)| m).collect();
    assert_eq!(
        methods,
        [
            ".reply",
            ".reply",
            "invalidateTransaction",
            ".reply",
            ".reply"
        ]
    );
}

#[test]
fn a_commit_without_oids_sends_an_empty_invalidation() {
    let storage = storage("empty-commit", Queues::DEFAULT);
    let (mut writer, mut own) = connected(&storage);
    let (_reader, mut others) = connected(&storage);
    begin(&mut writer, 1, None);
    let tid = vote_and_finish(&mut writer, 1).unwrap();
    assert_eq!(messages(&mut others), [invalidation(tid, &[])]);
    let methods: Vec<String> = messages(&mut own).into_iter().map(|(m, _)| m).collect();
    assert_eq!(methods, ["info", ".reply"]);
}

#[test]
fn a_finish_that_fails_publishes_nothing_and_blocks_nothing() {
    let storage = storage("failed-finish", Queues::DEFAULT);
    let (mut writer, mut own) = connected(&storage);
    let (_reader, mut others) = connected(&storage);
    let id = Value::from(1);
    let begin = vec![id.clone(), Value::Nil, Value::Nil, Value::Map(vec![])];
    done(call(&mut writer, "tpc_begin", begin));
    let data = Value::Binary(b"x".to_vec());
    done(call(
        &mut writer,
        "storea",
        vec![p64(1), p64(0), data, id.clone()],
    ));
    // A finish before the vote fails in the core.
    let fault = failed(call(&mut writer, "tpc_finish", vec![id]));
    assert_eq!(fault.class, "ZODB.POSException.StorageTransactionError");
    assert!(messages(&mut own).is_empty());
    assert!(messages(&mut others).is_empty());
    assert_eq!(storage.invalidations.published(), Tid(0));

    let tid = commit(&mut writer, 2, 1, 0, b"x").unwrap();
    assert_eq!(messages(&mut others), [invalidation(tid, &[1])]);
}

#[test]
fn get_invalidations_covers_the_gap_or_gives_nil() {
    let queues = Queues {
        invalidations: 2,
        ..Queues::DEFAULT
    };
    let mut s = Client::new(storage("get-invalidations", queues), outbox());
    done(call(
        &mut s,
        "register",
        vec![Value::from("1"), Value::Boolean(false)],
    ));
    let first = commit(&mut s, 1, 1, 0, b"a").unwrap();
    let second = commit(&mut s, 2, 2, 0, b"b").unwrap();
    commit(&mut s, 3, 3, 0, b"c").unwrap();
    let last = commit(&mut s, 4, 1, first, b"d").unwrap();
    let get = |s: &mut Client, tid: Value| done(call(s, "getInvalidations", vec![tid]));

    // Two commits after `second` wrote the oids 3 and 1.
    assert_eq!(
        get(&mut s, p64(second)),
        Value::Array(vec![p64(last), Value::Array(vec![p64(1), p64(3)])])
    );
    // Three commits come after `first`: more than the queue size.
    assert_eq!(get(&mut s, p64(first)), Value::Nil);
    let nothing = Value::Array(vec![p64(last), Value::Array(vec![])]);
    assert_eq!(get(&mut s, p64(last)), nothing);
    assert_eq!(get(&mut s, p64(u64::MAX)), nothing);
    let fault = failed(call(&mut s, "getInvalidations", vec![Value::from(1)]));
    assert_eq!(fault.class, "builtins.TypeError");
}

#[test]
fn get_invalidations_covers_a_young_tid_past_the_queue_size() {
    let queues = Queues {
        invalidations: 2,
        invalidation_age: std::time::Duration::from_secs(3_600),
        ..Queues::DEFAULT
    };
    let mut s = Client::new(storage("invalidation-age", queues), outbox());
    done(call(
        &mut s,
        "register",
        vec![Value::from("1"), Value::Boolean(false)],
    ));
    let first = commit(&mut s, 1, 1, 0, b"a").unwrap();
    let second = commit(&mut s, 2, 2, 0, b"b").unwrap();
    commit(&mut s, 3, 3, 0, b"c").unwrap();
    commit(&mut s, 4, 2, second, b"d").unwrap();
    let last = commit(&mut s, 5, 1, first, b"e").unwrap();
    let get = |s: &mut Client, tid: Value| done(call(s, "getInvalidations", vec![tid]));

    // Four commits come after `first`, and each oid comes once.
    assert_eq!(
        get(&mut s, p64(first)),
        Value::Array(vec![p64(last), Value::Array(vec![p64(1), p64(2), p64(3)])])
    );
    // The tid 0 is from 1900: older than the age.
    assert_eq!(get(&mut s, p64(0)), Value::Nil);
}

#[test]
fn concurrent_commits_reach_each_session_in_commit_order() {
    const WRITERS: u64 = 4;
    const COMMITS: u64 = 25;
    let storage = storage("concurrent", Queues::DEFAULT);
    let (_idle, mut idle) = connected(&storage);
    let registered = std::sync::Barrier::new(WRITERS as usize);
    let mut queues: Vec<Inbox> = std::thread::scope(|scope| {
        let writers: Vec<_> = (0..WRITERS)
            .map(|writer| {
                let (storage, registered) = (&storage, &registered);
                scope.spawn(move || {
                    let (mut s, frames) = connected(storage);
                    // Each session registers before the first commit,
                    // so each one gets every commit.
                    registered.wait();
                    for n in 0..COMMITS {
                        let oid = 1 + writer * COMMITS + n;
                        commit(&mut s, n as i64, oid, 0, b"x").unwrap();
                    }
                    // The session stays in the hub until all commit.
                    (s, frames)
                })
            })
            .collect();
        let joined: Vec<_> = writers.into_iter().map(|w| w.join().unwrap()).collect();
        joined.into_iter().map(|(_, frames)| frames).collect()
    });
    // Each writer sees every commit once, in commit order: its own
    // replies among the invalidations of the others.
    let mut all = None;
    for frames in &mut queues {
        let tids = tids(frames);
        assert_eq!(tids.len() as u64, WRITERS * COMMITS);
        assert!(tids.windows(2).all(|pair| pair[0] < pair[1]), "{tids:?}");
        assert!(all.is_none_or(|all| all == tids));
        all = Some(tids);
    }
    let seen: Vec<u64> = messages(&mut idle)
        .into_iter()
        .map(|(_, args)| u64_of(&args.as_array().unwrap()[0]).unwrap())
        .collect();
    assert_eq!(Some(seen), all);
}

/// Return the tids of the replies and the invalidations that wait for
/// a client, in their order.
fn tids(frames: &mut Inbox) -> Vec<u64> {
    messages(frames)
        .into_iter()
        .filter_map(|(method, args)| match method.as_str() {
            ".reply" => Some(u64_of(&args).unwrap()),
            "invalidateTransaction" => Some(u64_of(&args.as_array().unwrap()[0]).unwrap()),
            _ => None,
        })
        .collect()
}

#[test]
fn a_late_session_gets_exactly_the_commits_after_its_registration() {
    const COMMITS: u64 = 10;
    let storage = storage("late", Queues::DEFAULT);
    let (_idle, mut idle) = connected(&storage);
    let early = std::sync::Barrier::new(2);
    let (early_frames, late_registered, late_frames) = std::thread::scope(|scope| {
        let (storage, early) = (&storage, &early);
        let first = scope.spawn(move || {
            let (mut s, frames) = connected(storage);
            for n in 0..COMMITS {
                commit(&mut s, n as i64, 1 + n, 0, b"x").unwrap();
                if n == 4 {
                    early.wait();
                }
            }
            (s, frames)
        });
        let late = scope.spawn(move || {
            // This session registers after 5 commits of the first one.
            early.wait();
            let (outbox, frames, _closing) = Outbox::new(1_000, 1_000);
            let mut s = Client::new(Arc::clone(storage), outbox);
            let register = vec![Value::from("1"), Value::Boolean(false)];
            let registered = u64_of(&done(call(&mut s, "register", register))).unwrap();
            for n in 0..COMMITS {
                commit(&mut s, n as i64, 100 + n, 0, b"x").unwrap();
            }
            (s, registered, frames)
        });
        let (_first, early_frames) = first.join().unwrap();
        let (_late, registered, late_frames) = late.join().unwrap();
        (early_frames, registered, late_frames)
    });
    let all: Vec<u64> = messages(&mut idle)
        .into_iter()
        .map(|(_, args)| u64_of(&args.as_array().unwrap()[0]).unwrap())
        .collect();
    assert_eq!(all.len() as u64, 2 * COMMITS);
    assert!(all.windows(2).all(|pair| pair[0] < pair[1]), "{all:?}");
    // The early session registered before every commit.
    assert_eq!(tids(&mut { early_frames }), all);
    // The late one gets each commit after the tid of its registration,
    // once, in commit order, and no earlier commit.
    let after: Vec<u64> = all
        .iter()
        .copied()
        .filter(|tid| *tid > late_registered)
        .collect();
    assert!(after.len() as u64 <= 2 * COMMITS - 5);
    assert_eq!(tids(&mut { late_frames }), after);
}

#[test]
fn a_failed_load_puts_its_error_in_its_room_and_never_waits_for_the_client() {
    // A session queue of 2 frames that no writer drains.
    let (outbox, mut frames, _closing) = Outbox::new(2, 100);
    let mut s = Client::new(storage("failed-load-room", Queues::DEFAULT), outbox.clone());
    let register = vec![Value::from("1"), Value::Boolean(false)];
    done(call(&mut s, "register", register));
    // One reply waits in the queue: the client reads nothing.
    let waiting = message::reply(Value::from(1), Value::Nil);
    assert!(outbox.blocking_room().put(Some(waiting)));
    // The load takes the last room before it starts.
    let room = outbox.blocking_room();
    // The reader waits for room for the next call.
    let reader = outbox.clone();
    let next = std::thread::spawn(move || reader.blocking_room());
    std::thread::sleep(std::time::Duration::from_millis(100));
    let session = Arc::clone(&s.session);
    let load = std::thread::spawn(move || {
        let call = Call {
            id: Value::from(5),
            oneway: false,
            method: "loadBefore".into(),
            args: vec![p64(42), p64(u64::MAX >> 1)],
            context: None,
        };
        let mut reply = Reply::new(room);
        let outcome = session.call(&call, &mut reply);
        reply.send(&call, outcome, session.outbox())
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !load.is_finished() {
        assert!(
            std::time::Instant::now() < deadline,
            "the failed load waits for the client"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(load.join().unwrap());
    // The error went into the room of the load, so the reader still
    // waits. It gets room when the client reads.
    assert!(!next.is_finished());
    let taken = messages(&mut frames);
    assert!(next.join().unwrap().put(None));
    assert_eq!(taken.len(), 2, "{taken:?}");
    let class = taken[1].1.as_array().map(|error| error[0].clone());
    assert_eq!(class, Some(Value::from("ZODB.POSException.POSKeyError")));
    drop(s);
}

#[test]
fn get_tid_gives_the_serial_of_the_current_record() {
    let mut s = session("get-tid");
    done(call(
        &mut s,
        "register",
        vec![Value::from("1"), Value::Boolean(false)],
    ));
    let missing = Fault::from(ZoeError::NotFound { oid: Oid(1) });
    assert_eq!(failed(call(&mut s, "getTid", vec![p64(1)])), missing);

    let first = commit(&mut s, 1, 1, 0, MAPPING).unwrap();
    assert_eq!(done(call(&mut s, "getTid", vec![p64(1)])), p64(first));
    let second = commit(&mut s, 2, 1, first, MAPPING).unwrap();
    assert_eq!(done(call(&mut s, "getTid", vec![p64(1)])), p64(second));

    // A read needs no transaction and takes no lock: it works while
    // the open transaction holds the lock of the oid after its vote.
    begin(&mut s, 3, None);
    store(&mut s, 3, 1, second, MAPPING);
    done(call(&mut s, "vote", vec![Value::from(3)]));
    assert_eq!(done(call(&mut s, "getTid", vec![p64(1)])), p64(second));
    let abort = done(call(&mut s, "tpc_abort", vec![Value::from(3)]));
    assert_eq!(abort, Value::Nil);

    // A deletion is the current record: the oid has no tid.
    begin(&mut s, 4, None);
    let delete = vec![p64(1), p64(second), Value::from(4)];
    done(call(&mut s, "deleteObject", delete));
    vote_and_finish(&mut s, 4).unwrap();
    assert_eq!(failed(call(&mut s, "getTid", vec![p64(1)])), missing);

    // Wrong arguments fail the call, and the connection stays open.
    let fault = failed(call(&mut s, "getTid", vec![]));
    assert_eq!(fault, Fault::type_error("getTid takes 1 arguments"));
    let fault = failed(call(&mut s, "getTid", vec![Value::from(1)]));
    assert_eq!(fault, Fault::type_error("an oid or a tid is not bytes"));
    assert!(matches!(call(&mut s, "ping", vec![]), Outcome::Done(_)));
}

#[test]
fn get_tid_shows_only_the_commits_that_the_hub_published() {
    let storage = storage("get-tid-published", Queues::DEFAULT);
    let (mut writer, _own) = connected(&storage);
    let (mut reader, mut frames) = connected(&storage);
    let first = commit(&mut writer, 1, 1, 0, b"old").unwrap();
    assert_eq!(messages(&mut frames), [invalidation(first, &[1])]);

    // A commit that the store made visible and the hub did not publish
    // yet: a new record of oid 1, and the first record of oid 2.
    let mut txn = storage.store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(Oid(1), Tid(first), b"new").unwrap();
    txn.store(Oid(2), Tid(0), b"two").unwrap();
    txn.vote().unwrap();
    let hidden = txn.finish_after().unwrap();

    assert_eq!(done(call(&mut reader, "getTid", vec![p64(1)])), p64(first));
    let fault = failed(call(&mut reader, "getTid", vec![p64(2)]));
    assert_eq!(fault, Fault::from(ZoeError::NotFound { oid: Oid(2) }));

    // The hub publishes the commit. Then the reads show it.
    storage.invalidations.publish(Commit {
        previous: hidden.previous,
        tid: hidden.tid,
        origin: None,
        outbox: outbox(),
        own: Vec::new(),
        others: Others::Send(message::notify(
            "invalidateTransaction",
            vec![p64(hidden.tid.0), Value::Array(vec![p64(1), p64(2)])],
        )),
    });
    for oid in [1, 2] {
        let tid = done(call(&mut reader, "getTid", vec![p64(oid)]));
        assert_eq!(tid, p64(hidden.tid.0));
    }
    // The call does not put its reply under the mutex of the hub, so
    // its reply goes out after the invalidation of the commit that it
    // shows.
    assert!(reader.sent.is_none());
}
