//! Acceptance test of the invalidations of `zoe serve`, at the wire.
//!
//! After the acknowledgement of a commit, every other client of the same
//! storage gets one `invalidateTransaction(tid, oids)` call, in commit
//! order. The client that committed gets none for its own commit.
//!
//! `getInvalidations(tid)` returns `[last_tid, oids]`: the last tid of the
//! store and every oid that a transaction after `tid` wrote. It returns
//! nil when more than `--invalidation-queue-size` transactions (default
//! 100,000) come after `tid`: the client then verifies its cache. The
//! server reads the transaction log of the store, so the answer does not
//! depend on what the process saw before a restart.

use std::collections::BTreeSet;
use std::time::Duration;

mod wire;

use wire::{Server, Value, commit, fresh_dir, items, new_oids, p64, record, s, u64_of};

/// The time that a test waits for a message that must not come.
const QUIET: Duration = Duration::from_millis(300);

#[test]
fn every_other_client_gets_each_commit_in_order() {
    let dir = fresh_dir("broadcast");
    let server = Server::path(&dir.join("store"));
    let mut writer = server.client("1");
    let mut first = server.client("1");
    let mut second = server.client("1");

    let oids = new_oids(&mut writer, 3);
    let mut tids = Vec::new();
    for (n, oid) in oids.iter().enumerate() {
        tids.push(commit(&mut writer, n as i64 + 1, &[(*oid, 0, record(b"v"))]).unwrap());
    }

    for reader in [&mut first, &mut second] {
        for (tid, oid) in tids.iter().zip(&oids) {
            let args = reader
                .wait_for("invalidateTransaction", wire::TIMEOUT)
                .expect("no invalidation came");
            let args = items(&args);
            assert_eq!(u64_of(&args[0]), *tid, "invalidations out of commit order");
            let invalidated: Vec<u64> = items(&args[1]).iter().map(u64_of).collect();
            assert_eq!(invalidated, vec![*oid]);
        }
    }
    // The client that committed is not told about its own commits.
    assert!(writer.wait_for("invalidateTransaction", QUIET).is_none());
}

#[test]
fn a_transaction_of_several_oids_is_one_invalidation() {
    let dir = fresh_dir("one-message");
    let server = Server::path(&dir.join("store"));
    let mut writer = server.client("1");
    let mut reader = server.client("1");
    let oids = new_oids(&mut writer, 4);
    let stores: Vec<_> = oids.iter().map(|oid| (*oid, 0, record(b"v"))).collect();
    let tid = commit(&mut writer, 1, &stores).unwrap();

    let args = items(
        &reader
            .wait_for("invalidateTransaction", wire::TIMEOUT)
            .unwrap(),
    );
    assert_eq!(u64_of(&args[0]), tid);
    let invalidated: BTreeSet<u64> = items(&args[1]).iter().map(u64_of).collect();
    assert_eq!(invalidated, oids.iter().copied().collect());
    assert!(reader.wait_for("invalidateTransaction", QUIET).is_none());
}

#[test]
fn get_invalidations_covers_the_gap_or_gives_nil() {
    let dir = fresh_dir("gap");
    let path = dir.join("store");
    let mut server = Server::start(&["--path", s(&path), "--invalidation-queue-size", "3"], &[]);
    let mut conn = server.client("1");
    let oids = new_oids(&mut conn, 5);
    let mut tids = Vec::new();
    for (n, oid) in oids.iter().enumerate() {
        tids.push(commit(&mut conn, n as i64 + 1, &[(*oid, 0, record(b"v"))]).unwrap());
    }
    let last = *tids.last().unwrap();

    let check = |conn: &mut wire::Conn| {
        // Two transactions after tids[2]: inside the queue size.
        let reply = conn.call("getInvalidations", vec![p64(tids[2])]).unwrap();
        let parts = items(&reply);
        assert_eq!(u64_of(&parts[0]), last);
        let invalidated: BTreeSet<u64> = items(&parts[1]).iter().map(u64_of).collect();
        assert_eq!(invalidated, BTreeSet::from([oids[3], oids[4]]));

        // Three transactions after tids[1]: still inside.
        let reply = conn.call("getInvalidations", vec![p64(tids[1])]).unwrap();
        assert_eq!(items(&items(&reply)[1]).len(), 3);

        // Four transactions after tids[0]: longer than the queue size.
        let reply = conn.call("getInvalidations", vec![p64(tids[0])]).unwrap();
        assert_eq!(reply, Value::Nil);

        // Nothing after the last tid.
        let reply = conn.call("getInvalidations", vec![p64(last)]).unwrap();
        let parts = items(&reply);
        assert_eq!(u64_of(&parts[0]), last);
        assert!(items(&parts[1]).is_empty());
    };
    check(&mut conn);
    drop(conn);

    // The answer comes from the store, so a restart keeps it.
    server.kill();
    server.restart();
    let mut conn = server.client("1");
    check(&mut conn);
}
