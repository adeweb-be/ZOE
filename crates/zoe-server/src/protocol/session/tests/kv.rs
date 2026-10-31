//! The tests of the extensions `sequences` and `kv`.

use super::*;

fn sequence_next(name: Value) -> Call {
    Call {
        id: Value::from(1),
        oneway: false,
        method: "sequence_next".into(),
        args: vec![name],
        context: None,
    }
}

#[test]
fn sequence_next_gives_the_values_of_each_name_from_one() {
    let mut s = declaring("sequence-next", false, &["sequences"]);
    let info = done(call(&mut s, "get_info", vec![]));
    assert_eq!(
        get(&info, "extensions"),
        &Value::Array(vec![Value::from("sequences")])
    );
    let values: Vec<_> = (0..150)
        .map(|_| done(s.call(&sequence_next(Value::from("intids")))))
        .collect();
    let expected: Vec<_> = (1..=150_u64).map(Value::from).collect();
    assert_eq!(values, expected);
    assert_eq!(
        done(s.call(&sequence_next(Value::from("lexicon")))),
        Value::from(1)
    );
    let fault = failed(s.call(&sequence_next(Value::Binary(b"intids".to_vec()))));
    assert_eq!(fault.class, "builtins.TypeError");
    let fault = failed(call(&mut s, "sequence_next", vec![]));
    assert_eq!(fault.class, "builtins.TypeError");
}

#[test]
fn sequence_next_is_outside_the_transaction() {
    let mut s = declaring("sequence-abort", false, &["sequences"]);
    begin(&mut s, 7, None);
    let next = sequence_next(Value::from("intids"));
    assert_eq!(done(s.call(&next)), Value::from(1));
    done(call(&mut s, "tpc_abort", vec![Value::from(7)]));
    assert_eq!(done(s.call(&next)), Value::from(2));
}

#[test]
fn a_read_only_connection_takes_no_value() {
    let mut s = declaring("sequence-read-only", true, &["sequences"]);
    let fault = failed(s.call(&sequence_next(Value::from("intids"))));
    assert_eq!(fault, Fault::read_only());
}

#[test]
fn sequence_next_needs_the_declaration_in_both_dispatches() {
    let next = sequence_next(Value::from("intids"));
    for mut s in [
        registered("sequence-stock"),
        declaring("sequence-none", false, &[]),
        declaring("sequence-other", false, &["unknown"]),
    ] {
        let mut reply = Reply::new(s.session.outbox.blocking_room());
        let fault = failed(s.session.call(&next, &mut reply));
        assert_eq!(fault.class, "builtins.ValueError");
        assert_eq!(
            failed(s.txn.call(&mut next.clone())).class,
            "builtins.ValueError"
        );
        // The connection stays open, and the store gave no value.
        assert_eq!(done(call(&mut s, "ping", vec![])), Value::Nil);
        assert_eq!(s.store.sequence("intids").next().unwrap(), 1);
    }
}

fn bin(bytes: &[u8]) -> Value {
    Value::Binary(bytes.to_vec())
}

/// Commit, in the transaction `id`, the namespace call `method` with
/// `args`. Return the tid.
fn commit_kv(s: &mut Client, id: i64, method: &str, args: Vec<Value>) -> u64 {
    begin(s, id, None);
    assert_eq!(done(call(s, method, args)), Value::Nil);
    let voted = done(call(s, "vote", vec![Value::from(id)]));
    assert_eq!(voted, Value::Array(vec![]));
    finish(s, Value::from(id))
}

#[test]
fn kv_set_commits_with_the_transaction_and_kv_get_reads_it() {
    let mut s = declaring("kv-set", false, &["kv"]);
    let info = done(call(&mut s, "get_info", vec![]));
    assert_eq!(
        get(&info, "extensions"),
        &Value::Array(vec![Value::from("kv")])
    );
    let get_title = || {
        vec![
            Value::from("durable"),
            Value::from("registry"),
            bin(b"title"),
        ]
    };
    assert_eq!(done(call(&mut s, "kv_get", get_title())), Value::Nil);
    let set = vec![
        Value::from("durable"),
        Value::from("registry"),
        bin(b"title"),
        bin(b"site"),
        Value::Nil,
    ];
    let tid = commit_kv(&mut s, 7, "kv_set", set);
    assert_eq!(done(call(&mut s, "kv_get", get_title())), bin(b"site"));
    let generation = vec![Value::from("registry")];
    assert_eq!(done(call(&mut s, "kv_generation", generation)), p64(tid));
    let ephemeral = vec![
        Value::from("ephemeral"),
        Value::from("sessions"),
        bin(b"s1"),
        bin(b"data"),
        Value::F64(60.5),
    ];
    commit_kv(&mut s, 8, "kv_set", ephemeral);
    let scan = vec![
        Value::from("ephemeral"),
        Value::from("sessions"),
        Value::Nil,
        Value::Nil,
        Value::from(1000),
    ];
    assert_eq!(
        done(call(&mut s, "kv_scan", scan)),
        Value::Array(vec![Value::Array(vec![bin(b"s1"), bin(b"data")])])
    );
    let delete = vec![
        Value::from("durable"),
        Value::from("registry"),
        bin(b"title"),
    ];
    commit_kv(&mut s, 9, "kv_delete", delete);
    assert_eq!(done(call(&mut s, "kv_get", get_title())), Value::Nil);
}

#[test]
fn an_aborted_kv_set_writes_nothing() {
    let mut s = declaring("kv-abort", false, &["kv"]);
    begin(&mut s, 7, None);
    let set = vec![
        Value::from("durable"),
        Value::from("registry"),
        bin(b"title"),
        bin(b"site"),
        Value::Nil,
    ];
    assert_eq!(done(call(&mut s, "kv_set", set)), Value::Nil);
    done(call(&mut s, "tpc_abort", vec![Value::from(7)]));
    let get = vec![
        Value::from("durable"),
        Value::from("registry"),
        bin(b"title"),
    ];
    assert_eq!(done(call(&mut s, "kv_get", get)), Value::Nil);
}

#[test]
fn a_bad_kv_set_fails_the_vote() {
    for (ttl, flavour, class) in [
        (Value::Nil, "ephemeral", "builtins.ValueError"),
        (Value::from(5), "durable", "builtins.ValueError"),
        (Value::from(-1), "ephemeral", "builtins.ValueError"),
        (Value::F64(f64::NAN), "ephemeral", "builtins.ValueError"),
        (Value::from("5"), "ephemeral", "builtins.TypeError"),
        (Value::from(5), "other", "builtins.ValueError"),
    ] {
        let mut s = declaring("kv-bad", false, &["kv"]);
        begin(&mut s, 7, None);
        let set = vec![
            Value::from(flavour),
            Value::from("sessions"),
            bin(b"k"),
            bin(b"v"),
            ttl.clone(),
        ];
        assert_eq!(done(call(&mut s, "kv_set", set)), Value::Nil);
        let fault = failed(call(&mut s, "vote", vec![Value::from(7)]));
        assert_eq!(fault.class, class, "{flavour} {ttl:?}");
    }
}

#[test]
fn kv_set_outside_a_transaction_fails() {
    let mut s = declaring("kv-outside", false, &["kv"]);
    let set = vec![
        Value::from("durable"),
        Value::from("registry"),
        bin(b"k"),
        bin(b"v"),
        Value::Nil,
    ];
    assert_eq!(failed(call(&mut s, "kv_set", set)).class, TRANSACTION_ERROR);
}

#[test]
fn a_bad_read_of_a_namespace_fails_and_the_connection_stays_open() {
    let mut s = declaring("kv-bad-read", false, &["kv"]);
    for (method, args, class) in [
        (
            "kv_get",
            vec![Value::from("durable"), Value::from("r"), Value::from("k")],
            "builtins.TypeError",
        ),
        (
            "kv_get",
            vec![Value::from("durable"), Value::from("r"), bin(b"")],
            "builtins.ValueError",
        ),
        (
            "kv_get",
            vec![Value::from("lasting"), Value::from("r"), bin(b"k")],
            "builtins.ValueError",
        ),
        (
            "kv_get",
            vec![Value::from(1), Value::from("r"), bin(b"k")],
            "builtins.TypeError",
        ),
        (
            "kv_get",
            vec![Value::from("durable"), Value::from("r")],
            "builtins.TypeError",
        ),
        (
            "kv_scan",
            vec![
                Value::from("durable"),
                Value::from("r"),
                Value::Nil,
                Value::Nil,
                Value::from(0),
            ],
            "builtins.ValueError",
        ),
        (
            "kv_scan",
            vec![
                Value::from("durable"),
                Value::from("r"),
                Value::Nil,
                Value::Nil,
                Value::from(-1),
            ],
            "builtins.ValueError",
        ),
        (
            "kv_scan",
            vec![
                Value::from("durable"),
                Value::from("r"),
                Value::Nil,
                Value::Nil,
                Value::from("1"),
            ],
            "builtins.TypeError",
        ),
        (
            "kv_generation",
            vec![Value::from("")],
            "builtins.ValueError",
        ),
        (
            "kv_generation",
            vec![bin(b"registry")],
            "builtins.TypeError",
        ),
    ] {
        let fault = failed(call(&mut s, method, args.clone()));
        assert_eq!(fault.class, class, "{method} {args:?}");
        assert_eq!(done(call(&mut s, "ping", vec![])), Value::Nil);
    }
}

#[test]
fn the_calls_of_the_namespaces_need_the_declaration_of_kv() {
    let get = Call {
        id: Value::from(1),
        oneway: false,
        method: "kv_get".into(),
        args: vec![Value::from("durable"), Value::from("r"), bin(b"k")],
        context: None,
    };
    let set = Call {
        id: Value::from(1),
        oneway: true,
        method: "kv_set".into(),
        args: vec![
            Value::from("durable"),
            Value::from("r"),
            bin(b"k"),
            bin(b"v"),
            Value::Nil,
        ],
        context: None,
    };
    for mut s in [
        registered("kv-stock"),
        declaring("kv-other", false, &["sequences"]),
    ] {
        let mut reply = Reply::new(s.session.outbox.blocking_room());
        assert_eq!(
            failed(s.session.call(&get, &mut reply)).class,
            "builtins.ValueError"
        );
        assert_eq!(
            failed(s.txn.call(&mut set.clone())).class,
            "builtins.ValueError"
        );
        assert_eq!(done(call(&mut s, "ping", vec![])), Value::Nil);
    }
}

#[test]
fn a_read_only_connection_reads_the_namespaces_and_its_vote_fails() {
    let mut s = declaring("kv-read-only", true, &["kv"]);
    let get = vec![
        Value::from("durable"),
        Value::from("registry"),
        bin(b"title"),
    ];
    assert_eq!(done(call(&mut s, "kv_get", get)), Value::Nil);
    let set = vec![
        Value::from("durable"),
        Value::from("registry"),
        bin(b"title"),
        bin(b"site"),
        Value::Nil,
    ];
    begin(&mut s, 8, None);
    assert_eq!(done(call(&mut s, "kv_set", set)), Value::Nil);
    let fault = failed(call(&mut s, "vote", vec![Value::from(8)]));
    assert_eq!(fault, Fault::read_only());
}
