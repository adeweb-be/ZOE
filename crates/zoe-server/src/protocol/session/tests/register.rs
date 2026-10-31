//! The tests of `register`, of the information and the status of a
//! connection, of the declarations of the extensions and of the label.

use super::*;

#[test]
fn a_call_before_register_closes_the_connection() {
    let mut s = session("before-register");
    assert!(matches!(
        call(&mut s, "lastTransaction", vec![]),
        Outcome::Close(_)
    ));
}

#[test]
fn register_takes_only_the_storage_1_and_only_once() {
    let mut s = session("register");
    let fault = failed(call(
        &mut s,
        "register",
        vec![Value::from("2"), Value::Boolean(false)],
    ));
    assert_eq!(fault.class, "builtins.ValueError");
    assert_eq!(
        done(call(
            &mut s,
            "register",
            vec![Value::from("1"), Value::Boolean(false)]
        )),
        p64(0)
    );
    let fault = failed(call(
        &mut s,
        "register",
        vec![Value::from("1"), Value::Boolean(false)],
    ));
    assert_eq!(fault.class, "builtins.ValueError");
    assert!(matches!(
        call(&mut s, "no_such_method", vec![]),
        Outcome::Close(_)
    ));
}

#[test]
fn register_selects_the_storage_of_the_connection_by_its_name() {
    let named = |name: &str| {
        let dir =
            std::env::temp_dir().join(format!("zoe-session-select-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Arc::new(Store::open(&dir).unwrap());
        Arc::new(NamedStorage::new(name, &dir, store, Queues::DEFAULT))
    };
    let storages = Storages::new(vec![named("a"), named("b")], Queues::DEFAULT.client);
    let request = |method: &str, args: Vec<Value>| Call {
        id: Value::from(1),
        oneway: false,
        method: method.into(),
        args,
        context: None,
    };
    let register = |name: &str| request("register", vec![Value::from(name), Value::Boolean(false)]);
    for name in ["a", "b"] {
        let storage = select(&storages, &register(name)).ok().unwrap();
        assert_eq!(storage.name, name);
    }
    let by_bytes = request(
        "register",
        vec![Value::Binary(b"b".to_vec()), Value::Boolean(false)],
    );
    assert_eq!(select(&storages, &by_bytes).ok().unwrap().name, "b");
    let declaring = request(
        "register",
        vec![
            Value::from("a"),
            Value::Boolean(false),
            Value::Array(vec![]),
        ],
    );
    assert_eq!(select(&storages, &declaring).ok().unwrap().name, "a");
    let fault = failed(select(&storages, &register("1")).err().unwrap());
    assert_eq!(fault.class, "builtins.ValueError");
    let fault = failed(
        select(&storages, &request("register", vec![]))
            .err()
            .unwrap(),
    );
    assert_eq!(fault.class, "builtins.TypeError");
    assert!(matches!(
        select(&storages, &request("lastTransaction", vec![])),
        Err(Outcome::Close(_))
    ));
}

#[test]
fn a_read_only_client_gets_no_oid_and_its_vote_fails() {
    let mut s = session("read-only");
    done(call(
        &mut s,
        "register",
        vec![Value::from("1"), Value::Boolean(true)],
    ));
    let fault = failed(call(&mut s, "new_oids", vec![]));
    assert_eq!(fault, Fault::read_only());
    begin(&mut s, 1, None);
    store(&mut s, 1, 1, 0, b"a");
    let fault = failed(call(&mut s, "vote", vec![Value::from(1)]));
    assert_eq!(fault, Fault::read_only());
    assert!(s.txn.open.is_none());
}

#[test]
fn get_info_describes_the_store() {
    let mut s = registered("get-info");
    let info = done(call(&mut s, "get_info", vec![]));
    assert_eq!(
        keys(&info),
        [
            "length",
            "size",
            "name",
            "supportsUndo",
            "supports_record_iternext",
            "interfaces"
        ]
    );
    assert_eq!(get(&info, "length"), &Value::from(0));
    assert!(count(&info, "size") > 0);
    assert_eq!(
        get(&info, "name").as_str(),
        s.storage.path.to_str(),
        "the name is the directory of the store"
    );
    assert_eq!(get(&info, "supportsUndo"), &Value::Boolean(true));
    assert_eq!(
        get(&info, "supports_record_iternext"),
        &Value::Boolean(true)
    );
    let interfaces: Vec<_> = get(&info, "interfaces")
        .as_array()
        .unwrap()
        .iter()
        .map(|pair| match pair.as_array().unwrap().as_slice() {
            [module, name] => (module.as_str().unwrap(), name.as_str().unwrap()),
            other => panic!("not a pair: {other:?}"),
        })
        .collect();
    assert_eq!(interfaces, INTERFACES);
    assert!(interfaces.contains(&("ZODB.interfaces", "IExternalGC")));
    assert!(interfaces.contains(&("ZODB.interfaces", "IStorageUndoable")));

    let sizes = done(call(&mut s, "get_size_info", vec![]));
    assert_eq!(keys(&sizes), ["length", "size"]);
    assert_eq!(get(&sizes, "size"), get(&info, "size"));
    let fault = failed(call(&mut s, "get_info", vec![Value::Nil]));
    assert_eq!(fault.class, "builtins.TypeError");
}

/// The `length` of `get_size_info`.
fn length(s: &mut Client) -> u64 {
    count(&done(call(s, "get_size_info", vec![])), "length")
}

#[test]
fn the_length_is_the_object_count_of_the_store() {
    let mut s = registered("length");
    assert_eq!(length(&mut s), 0);
    let first = commit(&mut s, 1, 1, 0, MAPPING).unwrap();
    commit(&mut s, 2, 2, 0, MAPPING).unwrap();
    assert_eq!(length(&mut s), 2);
    // A new revision is no new object.
    commit(&mut s, 3, 1, first, MAPPING).unwrap();
    assert_eq!(length(&mut s), 2);
    let info = done(call(&mut s, "get_info", vec![]));
    assert_eq!(get(&info, "length"), &Value::from(2));
}

#[test]
fn get_info_gives_back_the_accepted_extensions_of_a_declaration() {
    let mut s = session("declared");
    let register = |declaration: Value| vec![Value::from("1"), Value::Boolean(false), declaration];
    // A declaration that is not a list of names fails, and the client
    // can register again.
    let fault = failed(call(&mut s, "register", register(Value::from("a"))));
    assert_eq!(fault.class, "builtins.TypeError");
    assert!(!s.session.is_registered());
    let declaration = Value::Array(vec![Value::from("unknown"), Value::from("unknown")]);
    assert_eq!(
        done(call(&mut s, "register", register(declaration))),
        p64(0)
    );
    let info = done(call(&mut s, "get_info", vec![]));
    assert_eq!(keys(&info).last(), Some(&"extensions"));
    assert_eq!(get(&info, "extensions"), &Value::Array(vec![]));
    let again = vec![Value::from("1"), Value::Boolean(false)];
    assert_eq!(
        failed(call(&mut s, "register", again)).class,
        "builtins.ValueError"
    );
}

#[test]
fn only_a_declaration_of_trace_context_lets_the_calls_have_a_trace_context() {
    assert!(!session("traces-unregistered").session.traces());
    let mut stock = session("traces-stock");
    done(call(
        &mut stock,
        "register",
        vec![Value::from("1"), Value::Boolean(false)],
    ));
    assert!(!stock.session.traces());
    let other = declaring("traces-other", false, &["sequences"]);
    assert!(!other.session.traces());
    let s = declaring("traces", false, &["sequences", "trace_context"]);
    assert!(s.session.traces());
}

#[test]
fn a_declaration_of_server_writes_reaches_the_vote() {
    let mut declared = declaring("server-writes", false, &["server_writes"]);
    let info = done(call(&mut declared, "get_info", vec![]));
    assert_eq!(
        get(&info, "extensions"),
        &Value::Array(vec![Value::from("server_writes")])
    );
    assert!(declared.session.server_writes());
    // A stock client declares nothing, and a ZOE client that does not
    // name the extension does not get it.
    assert!(!registered("server-writes-stock").session.server_writes());
    let other = declaring("server-writes-other", false, &["sequences"]);
    assert!(!other.session.server_writes());
}

#[test]
fn ping_answers_nil_and_set_client_label_takes_one_label() {
    let mut s = registered("ping");
    assert_eq!(done(call(&mut s, "ping", vec![])), Value::Nil);
    let label = vec![Value::from("a label")];
    assert_eq!(done(call(&mut s, "set_client_label", label)), Value::Nil);
    let fault = failed(call(&mut s, "set_client_label", vec![]));
    assert_eq!(fault.class, "builtins.TypeError");
}

#[test]
fn server_status_counts_as_zeo_counts() {
    let mut s = registered("status");
    let first = done(call(&mut s, "server_status", vec![]));
    assert_eq!(
        keys(&first),
        [
            "connections",
            "loads",
            "stores",
            "commits",
            "aborts",
            "active_txns",
            "lock_time",
            "conflicts",
            "conflicts_resolved",
            "start",
            "waiting",
            "timeout-thread-is-alive",
            "last-transaction"
        ]
    );
    assert_eq!(count(&first, "connections"), 1);
    assert_eq!(
        get(&first, "last-transaction"),
        &Value::from("0".repeat(16))
    );

    let tid = commit(&mut s, 1, 1, 0, b"x").unwrap();
    // A load of a record and a load of an oid without a record.
    done(call(&mut s, "loadBefore", vec![p64(1), p64(u64::MAX)]));
    failed(call(&mut s, "loadBefore", vec![p64(9), p64(u64::MAX)]));
    // A transaction that is open, then aborts.
    let begin = vec![Value::from(2), Value::Nil, Value::Nil, Value::Map(vec![])];
    done(call(&mut s, "tpc_begin", begin));
    let open = done(call(&mut s, "server_status", vec![]));
    assert_eq!(count(&open, "active_txns"), 1);
    done(call(&mut s, "tpc_abort", vec![Value::from(2)]));
    // A stale serial fails the vote with a conflict.
    commit(&mut s, 3, 1, 0, b"y").unwrap_err();

    let last = done(call(&mut s, "server_status", vec![]));
    assert_eq!(count(&last, "commits"), 1);
    assert_eq!(count(&last, "stores"), 2);
    assert_eq!(count(&last, "loads"), 2);
    assert_eq!(count(&last, "aborts"), 1);
    assert_eq!(count(&last, "active_txns"), 0);
    assert_eq!(count(&last, "conflicts"), 1);
    assert_eq!(count(&last, "conflicts_resolved"), 0);
    assert_eq!(get(&last, "lock_time"), &Value::Nil);
    assert_eq!(count(&last, "waiting"), 0);
    assert_eq!(get(&last, "timeout-thread-is-alive"), &Value::from("stub"));
    assert_eq!(
        get(&last, "last-transaction"),
        &Value::from(format!("{tid:016x}"))
    );
}

/// The counters of a storage that a call can change: the status, then
/// the metrics.
fn counters(storage: &NamedStorage) -> [u64; 12] {
    let (stats, metrics) = (&storage.stats, &storage.metrics);
    [
        get_counter(&stats.connections),
        get_counter(&stats.loads),
        get_counter(&stats.stores),
        get_counter(&stats.commits),
        get_counter(&stats.aborts),
        get_counter(&stats.active_txns),
        get_counter(&stats.conflicts),
        get_counter(&metrics.commits),
        get_counter(&metrics.conflicts_raised),
        metrics.load_latency.totals().0,
        metrics.vote_latency.totals().0,
        metrics.finish_latency.totals().0,
    ]
}

/// The waker of the writer of a connection. A frame that goes into a
/// queue of the connection wakes it, in the thread of the put. Then it
/// reads the counters of the storage.
struct AtPut {
    storage: Arc<NamedStorage>,
    seen: Mutex<Vec<[u64; 12]>>,
}

impl std::task::Wake for AtPut {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        let counters = counters(&self.storage);
        self.seen.lock().unwrap().push(counters);
    }
}

/// Run `method` with `args` as the connection runs it, and put its
/// reply. Return the counters at each put of a frame.
fn run_at_put(
    s: &mut Client,
    frames: &mut Inbox,
    at_put: &Arc<AtPut>,
    method: &str,
    args: Vec<Value>,
) -> Vec<[u64; 12]> {
    use std::future::Future;
    use std::task::{Context, Waker};

    let waker = Waker::from(Arc::clone(at_put));
    let mut cx = Context::from_waker(&waker);
    let mut next = std::pin::pin!(frames.recv());
    assert!(next.as_mut().poll(&mut cx).is_pending(), "a frame waits");
    let mut call = Call {
        id: Value::from(1),
        oneway: false,
        method: method.into(),
        args,
        context: None,
    };
    let outbox = s.session.outbox().clone();
    let room = outbox.blocking_room();
    let open = if s.is_registered() && method::lane(method) == Lane::Transaction {
        let outcome = s.txn.call(&mut call);
        Reply::new(room).send(&call, outcome, &outbox)
    } else {
        let mut reply = Reply::new(room);
        let outcome = s.session.call(&call, &mut reply);
        reply.send(&call, outcome, &outbox)
    };
    assert!(open, "{method} closed the connection");
    assert!(
        next.as_mut().poll(&mut cx).is_ready(),
        "{method} sent no reply"
    );
    std::mem::take(&mut *at_put.seen.lock().unwrap())
}

#[test]
fn a_call_counts_before_its_reply_goes_out() {
    let storage = storage("count-before-reply", Queues::DEFAULT);
    let (outbox, mut frames, _closing) = Outbox::new(1_000, 1_000);
    let mut s = Client::new(Arc::clone(&storage), outbox);
    let at_put = Arc::new(AtPut {
        storage: Arc::clone(&storage),
        seen: Mutex::new(Vec::new()),
    });
    let begin = |id: i64| vec![Value::from(id), Value::Nil, Value::Nil, Value::Map(vec![])];
    let store = |id: i64, serial: u64| {
        let data = Value::Binary(b"x".to_vec());
        vec![p64(1), p64(serial), data, Value::from(id)]
    };
    let mut run = |s: &mut Client, calls: Vec<(&str, Vec<Value>)>| {
        for (method, args) in calls {
            let seen = run_at_put(s, &mut frames, &at_put, method, args);
            while frames.try_recv().is_some() {}
            let now = counters(&storage);
            assert!(!seen.is_empty(), "{method} put no frame");
            for counted in seen {
                assert_eq!(counted, now, "{method} counted after its reply");
            }
        }
    };
    run(
        &mut s,
        vec![
            ("register", vec![Value::from("1"), Value::Boolean(false)]),
            ("tpc_begin", begin(1)),
            ("storea", store(1, 0)),
            ("vote", vec![Value::from(1)]),
            ("tpc_finish", vec![Value::from(1)]),
        ],
    );
    let tid = u64_of(&done(call(&mut s, "lastTransaction", vec![]))).unwrap();
    run(
        &mut s,
        vec![
            // A load of a record, of an oid without a record, and with
            // bad arguments.
            ("loadBefore", vec![p64(1), p64(u64::MAX)]),
            ("loadBefore", vec![p64(9), p64(u64::MAX)]),
            ("loadBefore", vec![p64(1)]),
            // A stale serial fails the vote with a conflict.
            ("tpc_begin", begin(2)),
            ("storea", store(2, 0)),
            ("vote", vec![Value::from(2)]),
            // A transaction that aborts.
            ("tpc_begin", begin(3)),
            ("tpc_abort", vec![Value::from(3)]),
            // A second commit, and a load of its record.
            ("tpc_begin", begin(4)),
            ("storea", store(4, tid)),
            ("vote", vec![Value::from(4)]),
            ("tpc_finish", vec![Value::from(4)]),
            ("loadBefore", vec![p64(1), p64(u64::MAX)]),
        ],
    );
    let [
        connections,
        loads,
        stores,
        commits,
        aborts,
        active,
        conflicts,
        ..,
    ] = counters(&storage);
    assert_eq!(
        [
            connections,
            loads,
            stores,
            commits,
            aborts,
            active,
            conflicts
        ],
        [1, 4, 3, 2, 1, 0, 1]
    );
    let [
        ..,
        committed,
        raised,
        load_latency,
        vote_latency,
        finish_latency,
    ] = counters(&storage);
    assert_eq!(
        [
            committed,
            raised,
            load_latency,
            vote_latency,
            finish_latency
        ],
        [2, 1, 4, 3, 2]
    );
}

#[test]
fn a_closed_session_leaves_the_status() {
    let mut s = registered("closed");
    let storage = Arc::clone(&s.storage);
    let mut other = Client::new(Arc::clone(&storage), outbox());
    // A session that does not register is not a connection.
    let _unregistered = Client::new(Arc::clone(&storage), outbox());
    done(call(
        &mut other,
        "register",
        vec![Value::from("1"), Value::Boolean(false)],
    ));
    let begin = vec![Value::from(1), Value::Nil, Value::Nil, Value::Map(vec![])];
    done(call(&mut s, "tpc_begin", begin));
    let status = storage.status().to_value();
    assert_eq!(count(&status, "connections"), 2);
    assert_eq!(count(&status, "active_txns"), 1);

    drop(s);
    let status = storage.status().to_value();
    assert_eq!(count(&status, "connections"), 1);
    assert_eq!(count(&status, "active_txns"), 0);
    assert_eq!(count(&status, "aborts"), 1);
    drop(other);
    assert_eq!(count(&storage.status().to_value(), "connections"), 0);
}

#[test]
fn a_client_label_is_one_line_of_at_most_200_characters() {
    assert_eq!(client_label(&Value::from("worker\n1")), "worker 1");
    let long = "x".repeat(500);
    assert_eq!(client_label(&Value::from(long.as_str())).len(), 200);
    let mut s = registered("label");
    done(call(&mut s, "set_client_label", vec![Value::from("web-1")]));
    assert_eq!(s.label().as_deref(), Some("web-1"));
    // An empty label gives the address of the peer back.
    done(call(&mut s, "set_client_label", vec![Value::from(" ")]));
    assert_eq!(s.label(), None);
}

#[test]
fn a_read_only_storage_refuses_a_writing_register_and_takes_a_read_only_one() {
    let dir = storage("register-read-only", Queues::DEFAULT).path.clone();
    let store = Arc::new(Store::open_read_only(&dir).unwrap());
    let storage = NamedStorage::new("1", &dir, store, Queues::DEFAULT).read_only(true);
    let mut s = Client::new(Arc::new(storage), outbox());
    let write = vec![Value::from("1"), Value::Boolean(false)];
    assert_eq!(failed(call(&mut s, "register", write)), Fault::read_only());
    // The connection stays open, and the client registers again.
    let read = vec![Value::from("1"), Value::Boolean(true)];
    assert_eq!(done(call(&mut s, "register", read)), p64(0));
    assert_eq!(done(call(&mut s, "lastTransaction", vec![])), p64(0));
    for (method, args) in [("new_oids", vec![]), ("pack", vec![Value::F64(1e9)])] {
        assert_eq!(failed(call(&mut s, method, args)), Fault::read_only());
    }
}

#[test]
fn a_read_only_client_registers_read_only_only_on_a_writable_storage() {
    let storage = storage("register-read-only-client", Queues::DEFAULT);
    let session = Session::new(Arc::clone(&storage), outbox(), never_stops());
    let mut s = Client::of(session.read_only_client(true));
    let write = vec![Value::from("1"), Value::Boolean(false)];
    assert_eq!(failed(call(&mut s, "register", write)), Fault::read_only());
    // The connection stays open, and the client registers again.
    let read = vec![Value::from("1"), Value::Boolean(true)];
    assert_eq!(done(call(&mut s, "register", read)), p64(0));
    assert_eq!(failed(call(&mut s, "new_oids", vec![])), Fault::read_only());

    // Another client of the same storage writes.
    let mut writer = Client::new(storage, outbox());
    let write = vec![Value::from("1"), Value::Boolean(false)];
    assert_eq!(done(call(&mut writer, "register", write)), p64(0));
    assert!(matches!(
        call(&mut writer, "new_oids", vec![]),
        Outcome::Done(_)
    ));
}
