//! Acceptance test of a hub queue of one message (F2).
//!
//! `--client-queue-size` (`ZOE_CLIENT_QUEUE_SIZE`) bounds the messages that
//! wait for one client in its hub queue, and takes 1 and more. The `info`
//! and the reply of one `tpc_finish` count as one message: they go in
//! together. So a client commits with a queue of one message, and a client
//! that reads its invalidations gets each of them.

use std::time::Duration;

mod wire;

use wire::{Server, commit, fresh_dir, items, load, new_oids, record, s, u64_of};

#[test]
fn a_queue_of_one_message_lets_a_client_commit() {
    let dir = fresh_dir("queue-of-one");
    let server = Server::start(
        &["--path", s(&dir.join("store")), "--client-queue-size", "1"],
        &[],
    );
    let mut writer = server.client("1");
    let mut reader = server.client("1");
    let oid = new_oids(&mut writer, 1)[0];
    let mut serial = 0;
    for n in 1..=3 {
        let value = [b'v', n as u8];
        serial = commit(&mut writer, n, &[(oid, serial, record(&value))]).unwrap();
        // The writer got `info` before its reply, and stays connected.
        assert!(
            writer.received.iter().any(|m| m.method == "info"),
            "commit {n}"
        );
        writer.received.clear();
        // The reader gets the invalidation of each commit.
        let args = reader
            .wait_for("invalidateTransaction", Duration::from_secs(10))
            .expect("the reader missed an invalidation");
        assert_eq!(u64_of(&items(&args)[0]), serial);
    }
    let last = record(b"v\x03");
    assert_eq!(load(&mut writer, oid).unwrap(), (last, serial));
}
