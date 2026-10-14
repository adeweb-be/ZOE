//! Acceptance test of a pack that runs when `zoe serve` gets SIGTERM (N10).
//!
//! The pack stops between two of its batches. The server then closes its
//! stores cleanly and exits with 0 within `--shutdown-timeout`. A stopped
//! pack loses nothing: each batch is one backend write, whole or absent,
//! and a later pack at the same tid removes what the stopped pack left
//! (ADR 0025).

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

mod wire;

use wire::{Server, Value, fresh_dir, load, record, s};
use zoe_core::Tid;

/// The objects that no reader reaches. The pack removes them all, in
/// batches of 1,000 records.
const GARBAGE: usize = 50_000;

/// The objects that each transaction writes.
const PER_TXN: usize = 1_000;

/// Write `GARBAGE` objects that nothing refers to into a new store in
/// `path`. Return their oids, in ascending order.
fn write_garbage(path: &Path) -> Vec<u64> {
    let store = wire::open_store(path);
    let mut oids = Vec::with_capacity(GARBAGE);
    for _ in 0..GARBAGE / PER_TXN {
        let mut txn = store.tpc_begin(b"user", b"garbage", b"").unwrap();
        for _ in 0..PER_TXN {
            let oid = store.new_oid().unwrap();
            txn.store(oid, Tid::ZERO, &record(b"garbage")).unwrap();
            oids.push(oid.0);
        }
        txn.vote().unwrap();
        txn.finish().unwrap();
    }
    oids
}

#[test]
fn sigterm_stops_a_running_pack_between_two_batches() {
    let dir = fresh_dir("pack");
    let path = dir.join("store");
    let oids = write_garbage(&path);
    let mut server = Server::start(&["--path", s(&path), "--shutdown-timeout", "5"], &[]);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    let mut packer = server.client("1");
    packer.send_call("pack", vec![Value::F64(now), Value::Boolean(true)]);

    // The pack still runs after 200 ms.
    let early = Some(Duration::from_millis(200));
    packer.stream().set_read_timeout(early).unwrap();
    let message = packer.recv();
    assert!(
        message.is_err(),
        "the pack ended before SIGTERM: {message:?}"
    );

    let (status, took) = server.terminate(Duration::from_secs(120));
    assert_eq!(status.code(), Some(0), "zoe serve did not exit cleanly");
    assert!(took < Duration::from_secs(5), "the shutdown took {took:?}");

    // The stores closed cleanly: a read-only open accepts the store.
    {
        let _spawn = wire::spawn_lock();
        zoe_core::Store::open_read_only(&path).unwrap();
    }

    // The pack stopped before its end. It removes the records in oid
    // order, so the last object is still there.
    server.restart();
    let mut conn = server.client("1");
    let last = oids[GARBAGE - 1];
    assert!(load(&mut conn, last).is_ok(), "the pack ran to its end");

    // A later pack at the same time removes what the stopped pack left.
    let patient = Some(Duration::from_secs(300));
    conn.stream().set_read_timeout(patient).unwrap();
    let packed = conn.call("pack", vec![Value::F64(now), Value::Boolean(true)]);
    assert_eq!(packed, Ok(Value::Nil));
    for oid in [oids[1], last] {
        assert!(load(&mut conn, oid).is_err(), "oid {oid} is still there");
    }
}
