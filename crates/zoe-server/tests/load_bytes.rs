//! Acceptance test of the bytes of a load reply (N2, N4).
//!
//! The server sends the reply of a load from the buffer of the record, in
//! parts. The client still gets one frame, byte for byte the frame that
//! msgpack gives for `[id, 0, ".reply", [data, serial, next]]`, for each
//! size of a record: around the limits of the bin types of msgpack, at
//! 100 KB, and above the size of the socket buffers. A load never shows a
//! large record of a transaction that did not finish.

mod wire;

use wire::{Conn, MAX, Server, Value, begin, commit, fresh_dir, items, new_oids, p64, u64_of};

/// The sizes of the records: around 255 and 65,535 bytes, the limits of
/// bin 8 and bin 16, then 100 KB, 1 MiB and 4 MiB.
const SIZES: [usize; 9] = [
    1,
    250,
    251,
    65_530,
    65_531,
    100_000,
    100_003,
    1 << 20,
    4 << 20,
];

/// The pickle of a record of `len` bytes. The bytes of an odd `seed` do
/// not compress. The bytes of an even `seed` repeat, so zstd compresses
/// them.
fn pickle(seed: u64, len: usize) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    (0..len)
        .map(|n| {
            if seed.is_multiple_of(2) {
                return b"words of a record "[n % 18];
            }
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect()
}

/// The frame of the reply to the call `id` with `result`, as msgpack
/// gives it.
fn reply_frame(id: &Value, result: Value) -> Vec<u8> {
    let message = Value::Array(vec![
        id.clone(),
        Value::from(0),
        Value::from(".reply"),
        result,
    ]);
    let mut bytes = Vec::new();
    rmpv::encode::write_value(&mut bytes, &message).unwrap();
    bytes
}

/// Load the record of `oid` before `tid` and return the raw frame of the
/// reply, and the id of the call.
fn raw_load(conn: &mut Conn, oid: u64, tid: Value) -> (Value, Vec<u8>) {
    let id = conn.send_call("loadBefore", vec![p64(oid), tid]);
    (id, conn.read_frame().unwrap())
}

#[test]
fn a_load_reply_is_the_msgpack_frame_of_the_record_for_each_size() {
    let dir = fresh_dir("load-bytes-sizes");
    let server = Server::path(&dir.join("store"));
    let mut writer = server.client("1");
    let mut reader = Conn::open(server.addr);
    // The reader does not register with the hub before the commits, so it
    // gets no invalidation between its replies.
    let oids = new_oids(&mut writer, 2 * SIZES.len() as u64);
    let mut records = Vec::new();
    for (n, oid) in oids.iter().enumerate() {
        // Each size comes twice: bytes that compress, then bytes that do
        // not.
        records.push((*oid, 0, pickle(n as u64, SIZES[n / 2])));
    }
    let first = commit(&mut writer, 1, &records[..records.len() / 2]).unwrap();
    let second = commit(&mut writer, 2, &records[records.len() / 2..]).unwrap();
    // A later version of the first record, so a load before it has a next
    // serial.
    let (old_oid, _, old_data) = records[0].clone();
    let third = commit(&mut writer, 3, &[(old_oid, first, pickle(9, 70_000))]).unwrap();
    reader.register("1", false).unwrap();

    for (n, (oid, _, data)) in records.iter().enumerate() {
        let serial = if n < records.len() / 2 { first } else { second };
        let serial = if *oid == old_oid { third } else { serial };
        let (id, frame) = raw_load(&mut reader, *oid, Value::Binary(MAX.to_vec()));
        let data = if *oid == old_oid {
            pickle(9, 70_000)
        } else {
            data.clone()
        };
        let result = Value::Array(vec![Value::Binary(data), p64(serial), Value::Nil]);
        assert_eq!(
            frame,
            reply_frame(&id, result),
            "a record of {} bytes",
            SIZES[n / 2]
        );
    }
    // A load before the later version gives the next serial.
    let (id, frame) = raw_load(&mut reader, old_oid, p64(third));
    let result = Value::Array(vec![Value::Binary(old_data), p64(first), p64(third)]);
    assert_eq!(frame, reply_frame(&id, result));
}

#[test]
fn a_load_never_shows_a_large_record_of_a_transaction_that_did_not_finish() {
    let dir = fresh_dir("load-bytes-unfinished");
    let server = Server::path(&dir.join("store"));
    let mut writer = server.client("1");
    let mut reader = server.client("1");
    let oid = new_oids(&mut writer, 1)[0];
    let old = pickle(1, 100_000);
    let first = commit(&mut writer, 1, &[(oid, 0, old.clone())]).unwrap();
    reader
        .wait_for("invalidateTransaction", wire::TIMEOUT)
        .unwrap();

    begin(&mut writer, 2);
    let new = pickle(3, 100_000);
    writer.send_async(
        "storea",
        vec![
            p64(oid),
            p64(first),
            Value::Binary(new.clone()),
            Value::from(2),
        ],
    );
    writer.call("vote", vec![Value::from(2)]).unwrap();
    for _ in 0..20 {
        let (id, frame) = raw_load(&mut reader, oid, Value::Binary(MAX.to_vec()));
        let result = Value::Array(vec![Value::Binary(old.clone()), p64(first), Value::Nil]);
        assert_eq!(frame, reply_frame(&id, result));
    }

    let tid = u64_of(&writer.call("tpc_finish", vec![Value::from(2)]).unwrap());
    let invalidation = reader
        .wait_for("invalidateTransaction", wire::TIMEOUT)
        .unwrap();
    assert_eq!(u64_of(&items(&invalidation)[0]), tid);
    let (id, frame) = raw_load(&mut reader, oid, Value::Binary(MAX.to_vec()));
    let result = Value::Array(vec![Value::Binary(new), p64(tid), Value::Nil]);
    assert_eq!(frame, reply_frame(&id, result));
}
