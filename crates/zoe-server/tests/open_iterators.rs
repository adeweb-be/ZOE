//! Acceptance test of the memory of open iterators (N6).
//!
//! Between two calls, an open iterator holds no pickle. A transaction
//! iterator keeps the tid of the last transaction that it gave. A record
//! iterator keeps its tid and its place, and reads a record when
//! `iterator_record_next` asks for it. So a client that opens iterators and
//! reads none to its end, as a read-only stock client that never sends
//! `iterator_gc` does, costs the server little memory, whatever the size of
//! the transactions. Each record iterator still gives its records.

mod common;

use common::{Conn, Server, Value, bytes_of, commit, fresh_dir, items, new_oids, p64, u64_of};

/// The size of the pickle of each transaction.
const RECORD: usize = 8 << 20;

/// The transactions, and the iterators of each kind that stay open.
const ITERATORS: usize = 24;

/// The most that the peak resident memory of the server may grow while
/// the iterators are open, in KiB. Iterators that keep their transactions
/// make it grow by `ITERATORS` times `RECORD`: 192 MiB.
const GROWTH_KIB: u64 = 64 * 1024;

/// Return a record of a `PersistentMapping` whose state holds `len` bytes
/// under the key `v`. `seed` makes the bytes of each record differ.
fn large_record(len: usize, seed: u8) -> Vec<u8> {
    let mut pickle = b"\x80\x03cpersistent.mapping\nPersistentMapping\nq\x00.".to_vec();
    pickle.extend_from_slice(b"\x80\x03}q\x00X\x01\x00\x00\x00vq\x01B");
    pickle.extend_from_slice(&u32::try_from(len).unwrap().to_le_bytes());
    pickle.extend((0..len).map(|n| (n as u8).wrapping_add(seed)));
    pickle.extend_from_slice(b"q\x02s.");
    pickle
}

/// The peak resident memory of the process `pid`, in KiB.
fn peak_kib(pid: u32) -> u64 {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
    status
        .lines()
        .find_map(|line| line.strip_prefix("VmHWM:"))
        .and_then(|value| value.split_whitespace().next())
        .and_then(|kib| kib.parse().ok())
        .unwrap()
}

/// Call `method` with the iterator id `id`, and return its reply.
fn call_on(conn: &mut Conn, method: &str, id: &Value) -> Value {
    conn.call(method, vec![id.clone()]).unwrap()
}

#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "reads the memory of the server in /proc: Linux only"
)]
fn open_iterators_hold_no_pickle_and_still_give_their_records() {
    let dir = fresh_dir("open-iterators");
    let server = Server::path(&dir.join("store"));
    let pid = server.pid();
    let mut writer = server.client("1");
    let oids = new_oids(&mut writer, ITERATORS as u64);
    let mut tids = Vec::with_capacity(ITERATORS);
    for (n, oid) in oids.iter().enumerate() {
        let data = large_record(RECORD, n as u8);
        tids.push(commit(&mut writer, n as i64 + 1, &[(*oid, 0, data)]).unwrap());
    }
    let mut conn = server.client("1");
    let before = peak_kib(pid);

    // Each transaction iterator gives its transaction, and a record
    // iterator starts on it. The client reads no record yet.
    let mut records = Vec::with_capacity(ITERATORS);
    for tid in &tids {
        let range = vec![p64(*tid), p64(*tid)];
        let iid = conn.call("iterator_start", range).unwrap();
        let txn = call_on(&mut conn, "iterator_next", &iid);
        assert_eq!(u64_of(&items(&txn)[0]), *tid);
        let args = vec![iid, p64(*tid)];
        records.push(conn.call("iterator_record_start", args).unwrap());
    }
    let open = peak_kib(pid);
    assert!(
        open < before + GROWTH_KIB,
        "the peak resident memory grew from {before} KiB to {open} KiB"
    );

    // Each record iterator gives its record, then its end.
    for (n, (rid, tid)) in records.iter().zip(&tids).enumerate() {
        let record = items(&call_on(&mut conn, "iterator_record_next", rid));
        assert_eq!(u64_of(&record[0]), oids[n]);
        assert_eq!(u64_of(&record[1]), *tid);
        assert!(bytes_of(&record[2]) == large_record(RECORD, n as u8));
        let end = call_on(&mut conn, "iterator_record_next", rid);
        assert_eq!(end, Value::Nil);
    }
}
