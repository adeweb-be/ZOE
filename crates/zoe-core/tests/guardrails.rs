//! Acceptance test of the guardrails. `max-record-size`,
//! `max-transaction-size` and `max-blob-size` refuse a store with a clear
//! error before anything is written. A lock held past `lock-warn` and a
//! transaction open past `transaction-warn` are logged through `tracing`
//! with the oid and the duration. No log line holds pickle bytes.

mod support;

use std::io::Write;
use std::path::Path;
use std::time::Duration;

use support::fresh_dir;
use tracing::Level;
use zoe_core::testing::{Captured, capture_events, commit, events, open_with_clock};
use zoe_core::{Oid, Options, SizeLimit, Store, Tid, ZoeError};

/// A text in every pickle of this test. No log line and no error may hold
/// it.
const MARKER: &str = "pickle-marker-5f2e";

/// The class pickle of every record of this test.
const CLASS: &[u8] = b"\x80\x03cpersistent.mapping\nPersistentMapping\nq\x00.";

/// The pickle of a blob record.
const BLOB_PICKLE: &[u8] = b"\x80\x03cZODB.blob\nBlob\nq\x00.\x80\x03N.";

/// A pickle of exactly `len` bytes. Its state is a text that starts with
/// the marker.
fn pickle(len: usize) -> Vec<u8> {
    let text_len = len - CLASS.len() - 8;
    let mut text = MARKER.as_bytes().to_vec();
    text.resize(text_len, b'x');
    let mut data = CLASS.to_vec();
    data.extend_from_slice(b"\x80\x03X");
    data.extend_from_slice(&(text_len as u32).to_le_bytes());
    data.extend_from_slice(&text);
    data.push(b'.');
    assert_eq!(data.len(), len);
    data
}

fn content(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 7 + 3) as u8).collect()
}

fn blob_files(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .map(|entry| entry.unwrap().path())
        .map(|path| {
            if path.is_dir() {
                blob_files(&path)
            } else {
                usize::from(path.extension().is_some_and(|ext| ext == "blob"))
            }
        })
        .sum()
}

fn assert_not_found(store: &Store, oid: Oid) {
    let loaded = store.load(oid);
    assert!(
        matches!(loaded, Err(ZoeError::NotFound { .. })),
        "{oid:?}: {loaded:?}"
    );
}

/// Check that the error names the guardrail and holds no pickle bytes.
fn assert_clear(error: &ZoeError, guardrail: &str) {
    let message = error.to_string();
    assert!(message.contains(guardrail), "{message}");
    assert_no_pickle_bytes(&message);
    assert_no_pickle_bytes(&format!("{error:?}"));
}

/// Check that `text` holds the marker in none of the forms that a log
/// line or an error can give to bytes.
fn assert_no_pickle_bytes(text: &str) {
    let bytes = MARKER.as_bytes();
    let lower: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let listed = bytes
        .iter()
        .map(|b| b.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    for form in [
        MARKER.to_owned(),
        lower.clone(),
        lower.to_uppercase(),
        listed,
    ] {
        assert!(!text.contains(&form), "{text} holds pickle bytes");
    }
}

#[test]
fn a_record_over_max_record_size_is_refused() {
    let options = Options {
        max_record_size: Some(1024),
        ..Options::default()
    };
    let store = Store::open_with(fresh_dir("record"), options).unwrap();
    let (fits, over) = (store.new_oid().unwrap(), store.new_oid().unwrap());
    let before = store.last_transaction();

    let mut txn = store.tpc_begin(b"", b"too large", b"").unwrap();
    txn.store(fits, Tid::ZERO, &pickle(1024)).unwrap();
    let error = txn.store(over, Tid::ZERO, &pickle(1025)).unwrap_err();
    assert!(
        matches!(
            error,
            ZoeError::TooLarge { oid, limit: SizeLimit::Record, size: 1025, max: 1024 }
                if oid == over
        ),
        "{error:?}"
    );
    assert_clear(&error, "max-record-size");
    txn.abort();

    assert_eq!(store.last_transaction(), before);
    assert_not_found(&store, fits);
    assert_not_found(&store, over);

    // A record of exactly the limit commits.
    let tid = commit(&store, fits, Tid::ZERO, &pickle(1024));
    assert_eq!(store.load(fits).unwrap(), (pickle(1024), tid));
}

#[test]
fn a_transaction_over_max_transaction_size_is_refused() {
    let options = Options {
        max_transaction_size: Some(1000),
        ..Options::default()
    };
    let store = Store::open_with(fresh_dir("transaction"), options).unwrap();
    let oids: Vec<Oid> = (0..3).map(|_| store.new_oid().unwrap()).collect();
    let before = store.last_transaction();

    // The store that takes the transaction past the limit fails.
    let mut txn = store.tpc_begin(b"", b"too large", b"").unwrap();
    txn.store(oids[0], Tid::ZERO, &pickle(400)).unwrap();
    txn.store(oids[1], Tid::ZERO, &pickle(400)).unwrap();
    let error = txn.store(oids[2], Tid::ZERO, &pickle(400)).unwrap_err();
    assert!(
        matches!(
            error,
            ZoeError::TooLarge { oid, limit: SizeLimit::Transaction, size: 1200, max: 1000 }
                if oid == oids[2]
        ),
        "{error:?}"
    );
    assert_clear(&error, "max-transaction-size");
    txn.abort();

    assert_eq!(store.last_transaction(), before);
    for &oid in &oids {
        assert_not_found(&store, oid);
    }

    // A transaction of exactly the limit commits.
    let mut txn = store.tpc_begin(b"", b"at the limit", b"").unwrap();
    for (&oid, len) in oids.iter().zip([400, 400, 200]) {
        txn.store(oid, Tid::ZERO, &pickle(len)).unwrap();
    }
    txn.vote().unwrap();
    let tid = txn.finish().unwrap();
    assert_eq!(store.load(oids[2]).unwrap(), (pickle(200), tid));
}

#[test]
fn a_blob_over_max_blob_size_is_refused_and_leaves_no_file() {
    let dir = fresh_dir("blob");
    let options = Options {
        max_blob_size: Some(4096),
        ..Options::default()
    };
    let store = Store::open_with(&dir, options).unwrap();
    let (fits, over) = (store.new_oid().unwrap(), store.new_oid().unwrap());

    let mut txn = store.tpc_begin(b"", b"too large", b"").unwrap();
    let mut file = store.new_blob_file().unwrap();
    file.write_all(&content(4096)).unwrap();
    txn.store_blob(fits, Tid::ZERO, BLOB_PICKLE, file).unwrap();
    let mut file = store.new_blob_file().unwrap();
    file.write_all(&content(4097)).unwrap();
    let error = txn
        .store_blob(over, Tid::ZERO, BLOB_PICKLE, file)
        .unwrap_err();
    assert!(
        matches!(
            error,
            ZoeError::TooLarge { oid, limit: SizeLimit::Blob, size: 4097, max: 4096 }
                if oid == over
        ),
        "{error:?}"
    );
    assert_clear(&error, "max-blob-size");
    txn.abort();

    assert_eq!(blob_files(&dir.join("blobs")), 0);
    assert_not_found(&store, fits);
    assert_not_found(&store, over);

    // A blob of exactly the limit commits.
    let mut txn = store.tpc_begin(b"", b"at the limit", b"").unwrap();
    let mut file = store.new_blob_file().unwrap();
    file.write_all(&content(4096)).unwrap();
    txn.store_blob(fits, Tid::ZERO, BLOB_PICKLE, file).unwrap();
    txn.vote().unwrap();
    let tid = txn.finish().unwrap();
    store.verify_blob(fits, tid).unwrap();
    assert_eq!(blob_files(&dir.join("blobs")), 1);
}

#[test]
fn blob_bytes_do_not_count_toward_the_transaction_size() {
    let options = Options {
        max_transaction_size: Some(1000),
        ..Options::default()
    };
    let store = Store::open_with(fresh_dir("blob-in-transaction"), options).unwrap();
    let (blob, record) = (store.new_oid().unwrap(), store.new_oid().unwrap());

    // The pickles hold 900 bytes and the blob 10,000 bytes.
    let mut txn = store.tpc_begin(b"", b"large blob", b"").unwrap();
    let mut file = store.new_blob_file().unwrap();
    file.write_all(&content(10_000)).unwrap();
    txn.store_blob(blob, Tid::ZERO, BLOB_PICKLE, file).unwrap();
    txn.store(record, Tid::ZERO, &pickle(900 - BLOB_PICKLE.len()))
        .unwrap();
    txn.vote().unwrap();
    let tid = txn.finish().unwrap();
    store.verify_blob(blob, tid).unwrap();
}

/// The `lock-warn` events of `oid`. The tests use oids that no other test
/// of this file uses, so these events are their own.
fn lock_events(oid: Oid) -> Vec<Captured> {
    events()
        .into_iter()
        .filter(|e| e.message().contains("lock-warn") && e.number("oid") == Some(oid.0))
        .collect()
}

/// The `transaction-warn` events with the duration `millis`. The tests
/// use durations that no other test of this file uses.
fn transaction_events(millis: u64) -> Vec<Captured> {
    events()
        .into_iter()
        .filter(|e| {
            e.message().contains("transaction-warn") && e.number("duration_ms") == Some(millis)
        })
        .collect()
}

#[test]
fn a_lock_held_past_lock_warn_is_logged_with_its_oid_and_duration() {
    capture_events();
    let (store, clock) = open_with_clock(Options {
        lock_warn: Some(Duration::from_secs(1)),
        ..Options::default()
    });
    let (written, read) = (Oid(0x51_01), Oid(0x51_02));

    // A commit that holds its locks for no time is not logged.
    let mut txn = store.tpc_begin(b"", b"quick", b"").unwrap();
    txn.store(written, Tid::ZERO, &pickle(100)).unwrap();
    txn.store(read, Tid::ZERO, &pickle(100)).unwrap();
    txn.vote().unwrap();
    let serial = txn.finish().unwrap();
    assert!(lock_events(written).is_empty());
    assert!(lock_events(read).is_empty());

    // An exclusive and a shared lock, held for 2,345 ms.
    let mut txn = store.tpc_begin(b"", b"slow", b"").unwrap();
    txn.check_current_serial(read, serial).unwrap();
    txn.store(written, serial, &pickle(100)).unwrap();
    txn.vote().unwrap();
    clock.advance(Duration::from_millis(2_345));
    txn.finish().unwrap();

    for oid in [written, read] {
        let events = lock_events(oid);
        assert_eq!(events.len(), 1, "{oid:?}: {events:?}");
        assert_eq!(events[0].level, Level::WARN, "{events:?}");
        assert_eq!(events[0].number("duration_ms"), Some(2_345), "{events:?}");
    }
}

#[test]
fn a_lock_released_before_lock_warn_is_not_logged() {
    capture_events();
    let (store, clock) = open_with_clock(Options {
        lock_warn: Some(Duration::from_secs(1)),
        ..Options::default()
    });
    let oid = Oid(0x51_11);

    let mut txn = store.tpc_begin(b"", b"under the threshold", b"").unwrap();
    txn.store(oid, Tid::ZERO, &pickle(100)).unwrap();
    txn.vote().unwrap();
    clock.advance(Duration::from_millis(999));
    let serial = txn.finish().unwrap();

    let mut txn = store.tpc_begin(b"", b"aborted quickly", b"").unwrap();
    txn.store(oid, serial, &pickle(100)).unwrap();
    txn.vote().unwrap();
    clock.advance(Duration::from_millis(10));
    txn.abort();

    assert!(lock_events(oid).is_empty(), "{:?}", lock_events(oid));
}

#[test]
fn a_transaction_open_past_transaction_warn_is_logged_with_its_duration() {
    capture_events();
    let (store, clock) = open_with_clock(Options {
        transaction_warn: Some(Duration::from_secs(1)),
        ..Options::default()
    });
    let (finished, aborted, quick) = (Oid(0x52_01), Oid(0x52_02), Oid(0x52_03));

    // A transaction that finishes 3,456 ms after it began.
    let mut txn = store.tpc_begin(b"", b"slow finish", b"").unwrap();
    txn.store(finished, Tid::ZERO, &pickle(100)).unwrap();
    clock.advance(Duration::from_millis(3_456));
    txn.vote().unwrap();
    txn.finish().unwrap();

    // A transaction that aborts 4,567 ms after it began.
    let mut txn = store.tpc_begin(b"", b"slow abort", b"").unwrap();
    txn.store(aborted, Tid::ZERO, &pickle(100)).unwrap();
    clock.advance(Duration::from_millis(4_567));
    txn.abort();

    // A transaction that finishes 17 ms after it began.
    let mut txn = store.tpc_begin(b"", b"quick", b"").unwrap();
    txn.store(quick, Tid::ZERO, &pickle(100)).unwrap();
    clock.advance(Duration::from_millis(17));
    txn.vote().unwrap();
    txn.finish().unwrap();

    for millis in [3_456, 4_567] {
        let events = transaction_events(millis);
        assert_eq!(events.len(), 1, "{millis} ms: {events:?}");
        assert_eq!(events[0].level, Level::WARN, "{events:?}");
    }
    assert!(transaction_events(17).is_empty());
}

#[test]
fn no_log_line_holds_pickle_bytes() {
    capture_events();
    let (store, clock) = open_with_clock(Options {
        lock_warn: Some(Duration::from_secs(1)),
        transaction_warn: Some(Duration::from_secs(1)),
        max_record_size: Some(2048),
        ..Options::default()
    });
    let (slow, refused) = (Oid(0x53_01), Oid(0x53_02));

    let mut txn = store.tpc_begin(b"", b"slow", b"").unwrap();
    txn.store(slow, Tid::ZERO, &pickle(1000)).unwrap();
    txn.vote().unwrap();
    clock.advance(Duration::from_secs(5));
    txn.finish().unwrap();

    let mut txn = store.tpc_begin(b"", b"refused", b"").unwrap();
    let error = txn.store(refused, Tid::ZERO, &pickle(4096)).unwrap_err();
    assert_clear(&error, "max-record-size");
    clock.advance(Duration::from_secs(5));
    txn.abort();

    // The slow transaction was logged, so the check below has lines to
    // read.
    assert_eq!(lock_events(slow).len(), 1);
    for event in events() {
        for value in event.fields.values() {
            assert_no_pickle_bytes(value);
        }
    }
}
