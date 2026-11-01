//! Acceptance test of the order of the replies to a burst of loads (N2,
//! N4).
//!
//! A client sends many `loadBefore` calls before it reads a reply, while
//! another client commits. ZEO's protocol requires this order on each
//! connection: the reply of a load comes after the invalidation of each
//! commit that it shows, and before the invalidation of each later commit.
//! So the serial of each reply is the tid of the last invalidation of its
//! oid that came before it. A heartbeat of the client gets no reply and
//! stops no load. A client that reads nothing does not hold the loads of
//! another client. A load never shows a transaction that did not finish.

use std::collections::HashMap;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

mod common;

use common::{
    Conn, MAX, Server, Value, begin, bytes_of, commit, fresh_dir, items, load, new_oids, p64,
    record, s, u64_of,
};

/// The objects of the test of the order.
const OBJECTS: u64 = 20;

/// The commits of the writer in the test of the order.
const COMMITS: u64 = 60;

/// The bursts of the reader, and the loads of each burst.
const BURSTS: usize = 40;
const BURST: usize = 50;

/// The data of each record, by oid and serial.
type Written = HashMap<(u64, u64), Vec<u8>>;

/// Return the frame of a heartbeat of a stock client: a reply with the id
/// -1.
fn heartbeat() -> Vec<u8> {
    let message = Value::Array(vec![
        Value::from(-1),
        Value::from(0),
        Value::from(".reply"),
        Value::Array(vec![]),
    ]);
    let mut bytes = Vec::new();
    rmpv::encode::write_value(&mut bytes, &message).unwrap();
    bytes
}

/// The args of a load of the current record of `oid`.
fn current(oid: u64) -> Vec<Value> {
    vec![p64(oid), Value::Binary(MAX.to_vec())]
}

#[test]
fn the_replies_to_a_burst_of_loads_keep_the_order_of_the_invalidations() {
    let dir = fresh_dir("load-burst-order");
    let server = Server::path(&dir.join("store"));
    let mut writer = server.client("1");
    // The reader registers first, so it gets the invalidation of each
    // commit, the first one included.
    let mut reader = server.client("1");
    let oids = new_oids(&mut writer, OBJECTS);
    let written: Arc<Mutex<Written>> = Arc::default();
    let stores: Vec<_> = oids
        .iter()
        .map(|oid| (*oid, 0, record(format!("{oid} 0").as_bytes())))
        .collect();
    let first = commit(&mut writer, 1, &stores).unwrap();
    for (oid, _, data) in stores {
        written.lock().unwrap().insert((oid, first), data);
    }

    // The writer commits while the reader loads. Each commit writes 3 of
    // the objects.
    let committing = {
        let oids = oids.clone();
        let written = Arc::clone(&written);
        thread::spawn(move || {
            let mut serials: HashMap<u64, u64> = oids.iter().map(|oid| (*oid, first)).collect();
            for n in 1..=COMMITS {
                let picked: Vec<u64> = (0..3)
                    .map(|k| oids[usize::try_from((n * 7 + k * 5) % OBJECTS).unwrap()])
                    .collect();
                let stores: Vec<_> = picked
                    .iter()
                    .map(|oid| (*oid, serials[oid], record(format!("{oid} {n}").as_bytes())))
                    .collect();
                let tid = commit(&mut writer, i64::try_from(n + 1).unwrap(), &stores).unwrap();
                let mut written = written.lock().unwrap();
                for (oid, _, data) in stores {
                    serials.insert(oid, tid);
                    written.insert((oid, tid), data);
                }
                drop(written);
                thread::sleep(Duration::from_micros(300));
            }
        })
    };

    // The tid of the last invalidation of each oid that came before.
    let mut invalidated: HashMap<u64, u64> = HashMap::new();
    let mut seen_commits = 0;
    // Each reply: the oid, the serial and the data.
    let mut replies = Vec::new();
    for burst in 0..BURSTS {
        // The loads that wait for their reply, by message id.
        let mut pending: HashMap<i64, u64> = HashMap::new();
        for n in 0..BURST {
            let oid = oids[(burst * BURST + n * 3) % oids.len()];
            let id = reader.send_call("loadBefore", current(oid));
            pending.insert(id.as_i64().unwrap(), oid);
            if n % 10 == 0 {
                reader.write_frame(&heartbeat()).unwrap();
            }
        }
        while !pending.is_empty() {
            let message = reader.recv().unwrap();
            match message.method.as_str() {
                "invalidateTransaction" => {
                    let args = items(&message.args);
                    let tid = u64_of(&args[0]);
                    for oid in items(&args[1]) {
                        let previous = invalidated.insert(u64_of(&oid), tid);
                        assert!(previous.is_none_or(|previous| previous < tid));
                    }
                    seen_commits += 1;
                }
                ".reply" => {
                    assert!(!message.is_error(), "{message:?}");
                    let oid = message
                        .id
                        .as_i64()
                        .and_then(|id| pending.remove(&id))
                        .unwrap_or_else(|| panic!("a reply to no load: {message:?}"));
                    let parts = items(&message.args);
                    let serial = u64_of(&parts[1]);
                    assert_eq!(
                        Some(serial),
                        invalidated.get(&oid).copied(),
                        "the reply of oid {oid} does not show the last invalidation before it"
                    );
                    assert_eq!(parts[2], Value::Nil);
                    replies.push((oid, serial, bytes_of(&parts[0])));
                }
                other => panic!("an unexpected message: {other}"),
            }
        }
    }
    committing.join().unwrap();
    assert_eq!(replies.len(), BURSTS * BURST);
    // Each reply holds the data that the writer committed at its serial.
    let written = written.lock().unwrap();
    for (oid, serial, data) in replies {
        assert_eq!(
            Some(&data),
            written.get(&(oid, serial)),
            "oid {oid} at {serial}"
        );
    }
    // The reader saw the commits while it loaded.
    assert!(
        seen_commits > 1,
        "only {seen_commits} commits during the loads"
    );
}

#[test]
fn a_client_that_reads_nothing_does_not_hold_the_loads_of_another() {
    let dir = fresh_dir("load-burst-slow");
    let server = Server::start(
        &["--path", s(&dir.join("store")), "--blocking-threads", "1"],
        &[],
    );
    let mut writer = server.client("1");
    let oids = new_oids(&mut writer, 2);
    let (large, small) = (oids[0], oids[1]);
    let mut big = b"\x80\x03cpersistent.mapping\nPersistentMapping\nq\x00.".to_vec();
    big.extend_from_slice(b"\x80\x03}q\x00X\x01\x00\x00\x00vq\x01B");
    big.extend_from_slice(&(4u32 << 20).to_le_bytes());
    big.extend((0..4u32 << 20).map(|n| (n.wrapping_mul(2_654_435_761) >> 24) as u8));
    big.extend_from_slice(b"q\x02s.");
    commit(&mut writer, 1, &[(large, 0, big), (small, 0, record(b"s"))]).unwrap();

    // The slow client sends its loads of the large record in one write and
    // reads nothing.
    let mut slow = server.client("1");
    let mut burst = Vec::new();
    for id in 100..150 {
        let call = Value::Array(vec![
            Value::from(id),
            Value::Boolean(false),
            Value::from("loadBefore"),
            Value::Array(current(large)),
        ]);
        let mut bytes = Vec::new();
        rmpv::encode::write_value(&mut bytes, &call).unwrap();
        burst.extend_from_slice(&u32::try_from(bytes.len()).unwrap().to_be_bytes());
        burst.extend_from_slice(&bytes);
    }
    slow.stream().write_all(&burst).unwrap();

    // The loads of another client go on, on a read pool of one thread.
    let mut fast = server.client("1");
    let started = Instant::now();
    for _ in 0..300 {
        assert_eq!(load(&mut fast, small).unwrap().0, record(b"s"));
    }
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "300 loads took {:?}",
        started.elapsed()
    );
    // The slow client still gets its replies when it reads.
    for _ in 0..50 {
        let message = slow.recv().unwrap();
        assert_eq!(message.method, ".reply");
        assert!(!message.is_error());
    }
}

#[test]
fn a_load_never_shows_a_transaction_that_did_not_finish() {
    let dir = fresh_dir("load-burst-unfinished");
    let server = Server::path(&dir.join("store"));
    let mut writer = server.client("1");
    let mut reader = server.client("1");
    let oids = new_oids(&mut writer, 2);
    let (old, new) = (oids[0], oids[1]);
    let first = commit(&mut writer, 1, &[(old, 0, record(b"old"))]).unwrap();
    assert_eq!(
        reader
            .wait_for("invalidateTransaction", common::TIMEOUT)
            .map(|args| u64_of(&items(&args)[0])),
        Some(first)
    );

    // The writer votes a transaction that changes one object and adds
    // another, and does not finish it.
    begin(&mut writer, 2);
    for (oid, serial, data) in [(old, first, record(b"changed")), (new, 0, record(b"added"))] {
        writer.send_async(
            "storea",
            vec![p64(oid), p64(serial), Value::Binary(data), Value::from(2)],
        );
    }
    writer.call("vote", vec![Value::from(2)]).unwrap();

    // A burst of loads sees none of it.
    let ids: Vec<(Value, u64)> = (0..BURST)
        .map(|n| {
            let oid = if n % 2 == 0 { old } else { new };
            (reader.send_call("loadBefore", current(oid)), oid)
        })
        .collect();
    for (id, oid) in &ids {
        let reply = reader.reply_to(id);
        if *oid == old {
            let parts = items(&reply.unwrap());
            assert_eq!(bytes_of(&parts[0]), record(b"old"));
            assert_eq!(u64_of(&parts[1]), first);
            assert_eq!(parts[2], Value::Nil);
        } else {
            assert_eq!(reply.unwrap_err().class, "ZODB.POSException.POSKeyError");
        }
    }
    assert!(reader.received.is_empty());

    // After the finish, the invalidation comes, then loads show it.
    let tid = u64_of(&writer.call("tpc_finish", vec![Value::from(2)]).unwrap());
    let ids: Vec<Value> = [old, new]
        .iter()
        .map(|oid| reader.send_call("loadBefore", current(*oid)))
        .collect();
    let invalidation = reader
        .wait_for("invalidateTransaction", common::TIMEOUT)
        .unwrap();
    assert_eq!(u64_of(&items(&invalidation)[0]), tid);
    for (id, data) in ids.iter().zip([record(b"changed"), record(b"added")]) {
        let parts = items(&reader.reply_to(id).unwrap());
        assert_eq!(bytes_of(&parts[0]), data);
        assert_eq!(u64_of(&parts[1]), tid);
    }
}

/// A load that a client sends after an invalidation shows the commit of
/// that invalidation.
#[test]
fn a_load_after_an_invalidation_shows_its_commit() {
    let dir = fresh_dir("load-burst-after");
    let server = Server::path(&dir.join("store"));
    let mut writer = server.client("1");
    let mut reader: Conn = server.client("1");
    let oid = new_oids(&mut writer, 1)[0];
    let mut serial = 0;
    for n in 0..30u8 {
        serial = commit(
            &mut writer,
            i64::from(n) + 1,
            &[(oid, serial, record(&[n]))],
        )
        .unwrap();
        let invalidation = reader
            .wait_for("invalidateTransaction", common::TIMEOUT)
            .unwrap();
        assert_eq!(u64_of(&items(&invalidation)[0]), serial);
        let (data, loaded) = load(&mut reader, oid).unwrap();
        assert_eq!((data, loaded), (record(&[n]), serial));
    }
}
