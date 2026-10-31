//! Acceptance test of a lock wait whose client disconnects (harness E).
//!
//! A vote that waits for the lock of an object ends when its connection
//! ends: the server cancels the wait in the core, and the vote releases
//! the locks that it took and fails. So a client that disconnects during
//! its vote holds no lock after its disconnect, whatever the lock timeout.

use std::time::{Duration, Instant};

mod common;

use common::{Server, Value, begin, commit, fresh_dir, load, new_oids, p64, record, s, u64_of};

#[test]
fn a_vote_that_waits_for_a_lock_releases_its_locks_when_its_client_disconnects() {
    let dir = fresh_dir("lock-wait");
    let server = Server::start(
        &["--path", s(&dir.join("store")), "--lock-timeout", "20"],
        &[],
    );
    let mut holder = server.client("1");
    let oids = new_oids(&mut holder, 2);
    let (low, high) = (oids[0], oids[1]);
    let stores = [(low, 0, record(b"low")), (high, 0, record(b"high"))];
    let first = commit(&mut holder, 1, &stores).unwrap();

    // The holder locks the high oid.
    begin(&mut holder, 2);
    holder.send_async(
        "storea",
        vec![
            p64(high),
            p64(first),
            Value::Binary(record(b"held")),
            Value::from(2),
        ],
    );
    holder.call("vote", vec![Value::from(2)]).unwrap();

    // The vote of the voter takes the lock of the low oid, then waits for
    // the lock of the high oid. Then its client disconnects.
    let mut voter = server.client("1");
    begin(&mut voter, 3);
    for oid in [low, high] {
        voter.send_async(
            "storea",
            vec![
                p64(oid),
                p64(first),
                Value::Binary(record(b"gone")),
                Value::from(3),
            ],
        );
    }
    voter.send_call("vote", vec![Value::from(3)]);
    std::thread::sleep(Duration::from_millis(300));
    voter.drop_now();

    // A commit on the low oid does not wait for the lock timeout.
    let mut other = server.client("1");
    let started = Instant::now();
    let tid = commit(&mut other, 4, &[(low, first, record(b"after"))]).unwrap();
    let waited = started.elapsed();
    assert!(
        waited < Duration::from_secs(3),
        "the commit waited {waited:?} for the lock of a gone client"
    );

    // The holder commits, and nothing of the gone vote is there.
    let held = u64_of(&holder.call("tpc_finish", vec![Value::from(2)]).unwrap());
    assert_eq!(load(&mut other, low).unwrap(), (record(b"after"), tid));
    assert_eq!(load(&mut other, high).unwrap(), (record(b"held"), held));
}
