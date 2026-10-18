//! Acceptance test of pipelined requests on one connection.
//!
//! `zoe serve` handles the calls of one connection concurrently. A reply
//! carries the message id of its call, so replies can leave in another
//! order than their calls. Loads run on a blocking pool whose size is
//! `--blocking-threads` (`ZOE_BLOCKING_THREADS`, the number of cores by
//! default). A vote that waits for a lock holds no thread of that pool, so
//! loads go on, also with a pool of one thread.

use std::collections::BTreeSet;

mod wire;

use wire::{
    MAX, Server, Value, ZOE, begin, commit, fresh_dir, items, new_oids, p64, record, s, u64_of,
};

#[test]
fn the_help_names_the_pool_option_and_its_twin() {
    let output = {
        let _spawn = wire::spawn_lock();
        std::process::Command::new(ZOE)
            .args(["serve", "--help"])
            .output()
            .unwrap()
    };
    assert!(output.status.success());
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(help.contains("--blocking-threads"));
    assert!(help.contains("ZOE_BLOCKING_THREADS"));
}

#[test]
fn many_pipelined_loads_each_get_their_reply() {
    let dir = fresh_dir("many-loads");
    let server = Server::path(&dir.join("store"));
    let mut conn = server.client("1");
    let oids = new_oids(&mut conn, 50);
    let stores: Vec<_> = oids
        .iter()
        .map(|oid| (*oid, 0, record(&oid.to_be_bytes())))
        .collect();
    let tid = commit(&mut conn, 1, &stores).unwrap();

    // Send every load before reading any reply.
    let ids: Vec<Value> = oids
        .iter()
        .map(|oid| conn.send_call("loadBefore", vec![p64(*oid), Value::Binary(MAX.to_vec())]))
        .collect();
    let mut seen = BTreeSet::new();
    for (id, oid) in ids.iter().zip(&oids) {
        let reply = conn.reply_to(id).unwrap();
        let parts = items(&reply);
        assert_eq!(wire::bytes_of(&parts[0]), record(&oid.to_be_bytes()));
        assert_eq!(u64_of(&parts[1]), tid);
        seen.insert(*oid);
    }
    assert_eq!(seen.len(), oids.len());
}

/// A load on a connection is answered while a vote of the same connection
/// waits for a lock that another connection holds.
fn a_load_passes_a_waiting_vote(extra: &[&str]) {
    let dir = fresh_dir(&format!("passes-{}", extra.len()));
    let path = dir.join("store");
    let mut args = vec!["--path", s(&path)];
    args.extend_from_slice(extra);
    let server = Server::start(&args, &[]);
    let mut holder = server.client("1");
    let mut waiter = server.client("1");

    let oids = new_oids(&mut holder, 2);
    let (locked, free) = (oids[0], oids[1]);
    let first = commit(
        &mut holder,
        1,
        &[(locked, 0, record(b"a")), (free, 0, record(b"b"))],
    )
    .unwrap();

    // `holder` votes and keeps the lock of `locked`.
    begin(&mut holder, 2);
    holder.send_async(
        "storea",
        vec![
            p64(locked),
            p64(first),
            Value::Binary(record(b"a2")),
            Value::from(2),
        ],
    );
    holder.call("vote", vec![Value::from(2)]).unwrap();

    // `waiter` votes on the same oid, and its vote waits.
    begin(&mut waiter, 3);
    waiter.send_async(
        "storea",
        vec![
            p64(locked),
            p64(first),
            Value::Binary(record(b"a3")),
            Value::from(3),
        ],
    );
    let vote = waiter.send_call("vote", vec![Value::from(3)]);
    // Then it loads another object on the same connection.
    let load = waiter.send_call("loadBefore", vec![p64(free), Value::Binary(MAX.to_vec())]);

    // The load is answered first, while the vote still waits. The waiter
    // registered before the first commit, so it gets that invalidation first.
    let message = loop {
        let message = waiter.recv().unwrap();
        if message.method == ".reply" {
            break message;
        }
    };
    assert_eq!(message.id, load, "the vote blocked the load");
    assert_eq!(u64_of(&items(&message.args)[1]), first);

    // When the holder finishes, the vote of the waiter ends: its serial is
    // stale now. The server raises the conflict, or lists it in the vote
    // reply for the client to resolve.
    holder.call("tpc_finish", vec![Value::from(2)]).unwrap();
    assert_conflict(waiter.reply_to(&vote), locked);
}

/// Check that a vote ended in a conflict on `oid`: an error reply, or a
/// reply that lists the conflict of `oid` for the client to resolve.
fn assert_conflict(outcome: Result<Value, wire::Fault>, oid: u64) {
    match outcome {
        Err(fault) => assert_eq!(fault.class, "ZODB.POSException.ConflictError"),
        Ok(reply) => {
            let listed = items(&reply).into_iter().any(|item| {
                item.as_map().is_some_and(|map| {
                    map.iter()
                        .any(|(k, v)| k.as_str() == Some("oid") && *v == p64(oid))
                })
            });
            assert!(listed, "the vote did not report the conflict: {reply:?}");
        }
    }
}

#[test]
fn a_load_passes_a_waiting_vote_on_the_same_connection() {
    a_load_passes_a_waiting_vote(&[]);
}

#[test]
fn a_waiting_vote_holds_no_thread_of_the_load_pool() {
    a_load_passes_a_waiting_vote(&["--blocking-threads", "1"]);
}
