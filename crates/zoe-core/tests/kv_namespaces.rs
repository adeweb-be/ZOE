//! The KV namespaces of the core (ADR 0043): writes that join the
//! two-phase commit, the expiry of the ephemeral keys without a pack, the
//! generation of a durable namespace, the sweep step, a crash, and the
//! checks of `zoe verify`.

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use zoe_core::testing;
use zoe_core::vfs::MemVfs;
use zoe_core::{Clock, Flavour, Oid, Options, Store, Tid, Txn, ZoeError};

const PICKLE: &[u8] =
    b"\x80\x03cpersistent.mapping\nPersistentMapping\nq\x00.\x80\x03}q\x01X\x04\x00\x00\x00datag\x01s.";

const MINUTE: Duration = Duration::from_secs(60);

/// A clock that moves only when the test moves it.
#[derive(Clone)]
struct ManualClock(Arc<Mutex<SystemTime>>);

impl ManualClock {
    fn new() -> Self {
        let start = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        Self(Arc::new(Mutex::new(start)))
    }

    fn advance(&self, by: Duration) {
        *self.0.lock().unwrap() += by;
    }

    fn millis(&self) -> u64 {
        let now = *self.0.lock().unwrap();
        now.duration_since(UNIX_EPOCH).unwrap().as_millis() as u64
    }
}

impl Clock for ManualClock {
    fn now(&self) -> SystemTime {
        *self.0.lock().unwrap()
    }
}

fn open() -> (Store, ManualClock, MemVfs) {
    open_with(Options::default())
}

fn open_with(options: Options) -> (Store, ManualClock, MemVfs) {
    let clock = ManualClock::new();
    let vfs = MemVfs::new();
    let store = Store::open_full(vfs.clone(), clock.clone(), options).unwrap();
    (store, clock, vfs)
}

/// Commit one ephemeral write of `value` for `key` in `sessions`, with
/// `ttl`. Return the tid.
fn set_session(store: &Store, key: &[u8], value: &[u8], ttl: Duration) -> Tid {
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.kv_set(Flavour::Ephemeral, "sessions", key, value, Some(ttl))
        .unwrap();
    txn.vote().unwrap();
    txn.finish().unwrap()
}

/// Commit one durable write of `value` for `key` in `registry`. Return the
/// tid.
fn set_registry(store: &Store, key: &[u8], value: &[u8]) -> Tid {
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.kv_set(Flavour::Durable, "registry", key, value, None)
        .unwrap();
    txn.vote().unwrap();
    txn.finish().unwrap()
}

fn session(store: &Store, key: &[u8]) -> Option<Vec<u8>> {
    store.kv_get(Flavour::Ephemeral, "sessions", key).unwrap()
}

fn registry(store: &Store, key: &[u8]) -> Option<Vec<u8>> {
    store.kv_get(Flavour::Durable, "registry", key).unwrap()
}

#[test]
fn a_write_is_visible_after_its_commit_and_never_before() {
    let (store, _, _) = open();
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.kv_set(Flavour::Durable, "registry", b"title", b"site", None)
        .unwrap();
    txn.kv_set(Flavour::Ephemeral, "sessions", b"s1", b"data", Some(MINUTE))
        .unwrap();
    txn.vote().unwrap();
    assert_eq!(registry(&store, b"title"), None);
    assert_eq!(session(&store, b"s1"), None);
    txn.finish().unwrap();
    assert_eq!(registry(&store, b"title").as_deref(), Some(&b"site"[..]));
    assert_eq!(session(&store, b"s1").as_deref(), Some(&b"data"[..]));
}

#[test]
fn an_abort_a_failed_vote_and_a_drop_write_nothing() {
    let (store, _, _) = open();
    let oid = store.new_oid().unwrap();
    let first = {
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.store(oid, Tid::ZERO, PICKLE).unwrap();
        txn.vote().unwrap();
        txn.finish().unwrap()
    };

    let mut aborted = store.tpc_begin(b"", b"", b"").unwrap();
    aborted
        .kv_set(Flavour::Durable, "registry", b"a", b"1", None)
        .unwrap();
    aborted.vote().unwrap();
    aborted.abort();

    // The vote fails on a stale serial, so its namespace write goes.
    let mut stale = store.tpc_begin(b"", b"", b"").unwrap();
    stale.store(oid, Tid::ZERO, PICKLE).unwrap();
    stale
        .kv_set(Flavour::Durable, "registry", b"b", b"2", None)
        .unwrap();
    assert!(matches!(stale.vote(), Err(ZoeError::Conflict { .. })));
    drop(stale);

    let mut dropped = store.tpc_begin(b"", b"", b"").unwrap();
    dropped
        .kv_set(Flavour::Durable, "registry", b"c", b"3", None)
        .unwrap();
    drop(dropped);

    assert_eq!(registry(&store, b"a"), None);
    assert_eq!(registry(&store, b"b"), None);
    assert_eq!(registry(&store, b"c"), None);
    assert_eq!(store.last_transaction(), first);
    assert_eq!(store.kv_generation("registry").unwrap(), Tid::ZERO);
}

#[test]
fn a_write_commits_with_the_records_of_its_transaction() {
    let (store, _, _) = open();
    let oid = store.new_oid().unwrap();
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(oid, Tid::ZERO, PICKLE).unwrap();
    txn.kv_set(Flavour::Durable, "registry", b"title", b"site", None)
        .unwrap();
    txn.vote().unwrap();
    let tid = txn.finish().unwrap();
    assert_eq!(store.load(oid).unwrap().1, tid);
    assert_eq!(registry(&store, b"title").as_deref(), Some(&b"site"[..]));
    assert_eq!(store.kv_generation("registry").unwrap(), tid);
    let txn = store.iterator(None, None).unwrap().last().unwrap().unwrap();
    assert_eq!(txn.tid, tid);
    assert_eq!(txn.records.len(), 1);
}

#[test]
fn a_transaction_with_only_namespace_writes_is_a_commit_without_a_record() {
    let (store, _, _) = open();
    let before = store.last_transaction();
    let tid = set_session(&store, b"s1", b"data", MINUTE);
    assert!(tid > before);
    assert_eq!(store.last_transaction(), tid);
    let txn = store.iterator(None, None).unwrap().last().unwrap().unwrap();
    assert_eq!(txn.tid, tid);
    assert!(txn.records.is_empty());
}

#[test]
fn a_read_gives_the_last_committed_value_and_the_later_writer_wins() {
    let (store, _, _) = open();
    set_registry(&store, b"title", b"one");
    // Two transactions write one key: no lock, no conflict, and the later
    // commit wins.
    let mut first = store.tpc_begin(b"", b"", b"").unwrap();
    let mut second = store.tpc_begin(b"", b"", b"").unwrap();
    first
        .kv_set(Flavour::Durable, "registry", b"title", b"two", None)
        .unwrap();
    second
        .kv_set(Flavour::Durable, "registry", b"title", b"three", None)
        .unwrap();
    second.vote().unwrap();
    first.vote().unwrap();
    let one = first.finish().unwrap();
    let two = second.finish().unwrap();
    assert!(two > one);
    assert_eq!(registry(&store, b"title").as_deref(), Some(&b"three"[..]));
    assert_eq!(store.kv_generation("registry").unwrap(), two);
}

#[test]
fn a_later_write_of_a_key_in_a_transaction_replaces_the_earlier_one() {
    let (store, _, _) = open();
    set_registry(&store, b"gone", b"x");
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.kv_set(Flavour::Durable, "registry", b"k", b"one", None)
        .unwrap();
    txn.kv_set(Flavour::Durable, "registry", b"k", b"two", None)
        .unwrap();
    txn.kv_set(Flavour::Durable, "registry", b"gone", b"y", None)
        .unwrap();
    txn.kv_delete(Flavour::Durable, "registry", b"gone")
        .unwrap();
    // A delete of a key that has no row does nothing.
    txn.kv_delete(Flavour::Durable, "registry", b"never")
        .unwrap();
    txn.vote().unwrap();
    txn.finish().unwrap();
    assert_eq!(registry(&store, b"k").as_deref(), Some(&b"two"[..]));
    assert_eq!(registry(&store, b"gone"), None);
    assert_eq!(registry(&store, b"never"), None);
}

#[test]
fn an_ephemeral_key_expires_after_its_ttl_without_a_pack() {
    let (store, clock, _) = open();
    set_session(&store, b"s1", b"data", MINUTE);
    clock.advance(Duration::from_secs(59));
    assert_eq!(session(&store, b"s1").as_deref(), Some(&b"data"[..]));
    clock.advance(Duration::from_secs(1));
    assert_eq!(session(&store, b"s1"), None);
    let scan = store
        .kv_scan(Flavour::Ephemeral, "sessions", None, None, 1000)
        .unwrap();
    assert!(scan.is_empty());
}

#[test]
fn each_write_of_a_key_sets_its_ttl_again() {
    let (store, clock, _) = open();
    set_session(&store, b"s1", b"one", MINUTE);
    clock.advance(Duration::from_secs(50));
    set_session(&store, b"s1", b"two", MINUTE);
    clock.advance(Duration::from_secs(50));
    assert_eq!(session(&store, b"s1").as_deref(), Some(&b"two"[..]));
    // The sweep takes the index key of the first expiry, and keeps the row
    // that the second write refreshed.
    let swept = store.sweep(1000).unwrap();
    assert_eq!((swept.taken, swept.removed), (1, 0));
    assert_eq!(session(&store, b"s1").as_deref(), Some(&b"two"[..]));
    clock.advance(Duration::from_secs(10));
    let swept = store.sweep(1000).unwrap();
    assert_eq!((swept.taken, swept.removed), (1, 1));
    assert_eq!(
        testing::read_kv_frame(
            &store,
            Flavour::Ephemeral,
            &testing::kv_row_key(b"sessions", b"s1")
        )
        .map_err(|e| e.to_string()),
        Err("backend error: no namespace row under the key".to_owned())
    );
}

#[test]
fn a_sweep_step_takes_at_most_its_limit_in_the_order_of_the_expiries() {
    let (store, clock, _) = open();
    for i in 0..5u8 {
        set_session(
            &store,
            &[b'k', i],
            b"v",
            Duration::from_secs(10 + u64::from(i)),
        );
    }
    set_session(&store, b"late", b"v", Duration::from_secs(3600));
    clock.advance(Duration::from_secs(20));
    let swept = store.sweep(3).unwrap();
    assert_eq!((swept.taken, swept.removed), (3, 3));
    let swept = store.sweep(3).unwrap();
    assert_eq!((swept.taken, swept.removed), (2, 2));
    assert_eq!(store.sweep(3).unwrap().taken, 0);
    assert_eq!(session(&store, b"late").as_deref(), Some(&b"v"[..]));
    // A step with nothing to take writes nothing.
    let last = store.last_transaction();
    assert_eq!(store.sweep(1000).unwrap().taken, 0);
    assert_eq!(store.last_transaction(), last);
}

#[test]
fn a_sweep_step_keeps_a_row_that_fails_its_checksum() {
    let (store, clock, _) = open();
    set_session(&store, b"s1", b"data", MINUTE);
    let row = testing::kv_row_key(b"sessions", b"s1");
    let mut frame = testing::read_kv_frame(&store, Flavour::Ephemeral, &row).unwrap();
    // Flip a bit of the expiry: a read raises, and never reads the key as
    // expired.
    frame[32] ^= 1;
    testing::write_kv_frame(&store, Flavour::Ephemeral, &row, Some(&frame)).unwrap();
    assert!(matches!(
        store.kv_get(Flavour::Ephemeral, "sessions", b"s1"),
        Err(ZoeError::CorruptKv { .. })
    ));
    clock.advance(Duration::from_secs(3600));
    let swept = store.sweep(1000).unwrap();
    assert_eq!((swept.taken, swept.removed), (1, 0));
    assert_eq!(
        testing::read_kv_frame(&store, Flavour::Ephemeral, &row).unwrap(),
        frame
    );
}

#[test]
fn a_damaged_index_key_never_removes_a_live_key() {
    let (store, clock, _) = open();
    set_session(&store, b"s1", b"data", Duration::from_secs(3600));
    let row = testing::kv_row_key(b"sessions", b"s1");
    // An index key that says the row expires now.
    let key = testing::kv_expiry_key(clock.millis(), &row);
    testing::write_kv_expiry_key(&store, &key, true).unwrap();
    let swept = store.sweep(1000).unwrap();
    assert_eq!((swept.taken, swept.removed), (1, 0));
    assert_eq!(session(&store, b"s1").as_deref(), Some(&b"data"[..]));
}

#[test]
fn a_scan_gives_live_keys_in_byte_order_between_its_bounds() {
    let (store, clock, _) = open();
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    for key in [&b"b"[..], b"a", b"c", b"ab", "\u{e9}".as_bytes()] {
        txn.kv_set(Flavour::Ephemeral, "sessions", key, key, Some(MINUTE))
            .unwrap();
    }
    txn.kv_set(
        Flavour::Ephemeral,
        "sessions",
        b"short",
        b"",
        Some(Duration::from_secs(1)),
    )
    .unwrap();
    // A key of another namespace stays out of the scan.
    txn.kv_set(Flavour::Ephemeral, "session", b"z", b"z", Some(MINUTE))
        .unwrap();
    txn.kv_set(Flavour::Ephemeral, "sessionsx", b"z", b"z", Some(MINUTE))
        .unwrap();
    txn.vote().unwrap();
    txn.finish().unwrap();
    clock.advance(Duration::from_secs(1));
    let keys = |start: Option<&[u8]>, stop: Option<&[u8]>, limit| -> Vec<Vec<u8>> {
        store
            .kv_scan(Flavour::Ephemeral, "sessions", start, stop, limit)
            .unwrap()
            .into_iter()
            .map(|(key, value)| {
                assert_eq!(key, value);
                key
            })
            .collect()
    };
    let all: Vec<Vec<u8>> = vec![
        b"a".to_vec(),
        b"ab".to_vec(),
        b"b".to_vec(),
        b"c".to_vec(),
        "\u{e9}".as_bytes().to_vec(),
    ];
    assert_eq!(keys(None, None, 1000), all);
    assert_eq!(keys(Some(b"ab"), Some(b"c"), 1000), all[1..3]);
    assert_eq!(keys(None, None, 2), all[..2]);
    assert_eq!(keys(Some(b"c"), Some(b"b"), 1000), Vec::<Vec<u8>>::new());
    assert!(matches!(
        store.kv_scan(Flavour::Ephemeral, "sessions", None, None, 0),
        Err(ZoeError::InvalidKv(_))
    ));
}

#[test]
fn a_scan_gives_at_most_1000_pairs() {
    let (store, _, _) = open();
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    for i in 0..1100u32 {
        txn.kv_set(Flavour::Durable, "registry", &i.to_be_bytes(), b"v", None)
            .unwrap();
    }
    txn.vote().unwrap();
    txn.finish().unwrap();
    let scan = store
        .kv_scan(Flavour::Durable, "registry", None, None, 5000)
        .unwrap();
    assert_eq!(scan.len(), 1000);
    let next = store
        .kv_scan(
            Flavour::Durable,
            "registry",
            Some(&1000u32.to_be_bytes()),
            None,
            5000,
        )
        .unwrap();
    assert_eq!(next.len(), 100);
}

#[test]
fn a_scan_stops_after_the_pair_that_reaches_its_bytes() {
    let (store, _, _) = open();
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    let value = vec![7; 300 * 1024];
    for key in [&b"a"[..], b"b", b"c", b"d", b"e"] {
        txn.kv_set(Flavour::Durable, "big", key, &value, None)
            .unwrap();
    }
    txn.vote().unwrap();
    txn.finish().unwrap();
    // Four values of 300 KiB pass 1 MiB: the scan gives four, then one.
    let first = store
        .kv_scan(Flavour::Durable, "big", None, None, 1000)
        .unwrap();
    assert_eq!(first.len(), 4);
    let mut after = first.last().unwrap().0.clone();
    after.push(0);
    let rest = store
        .kv_scan(Flavour::Durable, "big", Some(&after), None, 1000)
        .unwrap();
    assert_eq!(rest.len(), 1);
    assert_eq!(rest[0].0, b"e");
    assert_eq!(zoe_core::SCAN_BYTES, 1 << 20);
}

#[test]
fn a_durable_scan_leaves_out_the_generation_row() {
    let (store, _, _) = open();
    set_registry(&store, b"a", b"1");
    let scan = store
        .kv_scan(Flavour::Durable, "registry", None, None, 1000)
        .unwrap();
    assert_eq!(scan, [(b"a".to_vec(), b"1".to_vec())]);
    // An empty start key is an open start.
    let scan = store
        .kv_scan(Flavour::Durable, "registry", Some(b""), None, 1000)
        .unwrap();
    assert_eq!(scan.len(), 1);
}

#[test]
fn the_generation_is_the_tid_of_the_last_commit_that_wrote_the_namespace() {
    let (store, _, _) = open();
    assert_eq!(store.kv_generation("registry").unwrap(), Tid::ZERO);
    let one = set_registry(&store, b"a", b"1");
    assert_eq!(store.kv_generation("registry").unwrap(), one);
    // A commit of another namespace or of an object changes nothing.
    set_session(&store, b"s", b"v", MINUTE);
    let oid = store.new_oid().unwrap();
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(oid, Tid::ZERO, PICKLE).unwrap();
    txn.vote().unwrap();
    txn.finish().unwrap();
    assert_eq!(store.kv_generation("registry").unwrap(), one);
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.kv_delete(Flavour::Durable, "registry", b"a").unwrap();
    txn.vote().unwrap();
    let two = txn.finish().unwrap();
    assert_eq!(store.kv_generation("registry").unwrap(), two);
}

#[test]
fn a_durable_key_survives_a_reopen_and_a_crash() {
    let vfs = MemVfs::new();
    {
        let store = Store::open_vfs(vfs.clone()).unwrap();
        set_registry(&store, b"title", b"site");
        vfs.mark("acknowledged");
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.kv_set(Flavour::Durable, "registry", b"later", b"x", None)
            .unwrap();
        txn.kv_set(Flavour::Ephemeral, "sessions", b"later", b"x", Some(MINUTE))
            .unwrap();
        txn.vote().unwrap();
        vfs.mark("voted");
        txn.finish().unwrap();
        vfs.mark("finished");
    }
    let images = vfs.crash_images();
    assert!(images.len() > 1);
    for image in &images {
        let store =
            Store::open_vfs(image.vfs()).unwrap_or_else(|e| panic!("{image:?} does not open: {e}"));
        if image.is_after("acknowledged") {
            assert_eq!(
                registry(&store, b"title").as_deref(),
                Some(&b"site"[..]),
                "{image:?}"
            );
        }
        // The second commit is whole or absent.
        let later = registry(&store, b"later");
        assert_eq!(
            later.is_some(),
            session(&store, b"later").is_some(),
            "{image:?}"
        );
        if image.is_after("finished") {
            assert!(later.is_some(), "{image:?}");
        }
        if !image.is_after("voted") {
            assert!(later.is_none(), "{image:?}");
        }
    }
}

#[test]
fn a_store_without_namespace_writes_has_no_namespace_tables() {
    let (store, _, _) = open();
    let oid = store.new_oid().unwrap();
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(oid, Tid::ZERO, PICKLE).unwrap();
    txn.vote().unwrap();
    txn.finish().unwrap();
    assert!(!store.has_ephemeral_namespaces().unwrap());
    assert_eq!(store.sweep(1000).unwrap().taken, 0);
    assert_eq!(registry(&store, b"title"), None);
    assert_eq!(store.verify_kv().unwrap(), zoe_core::KvCheck::default());
    // A durable write creates only the durable table.
    set_registry(&store, b"title", b"site");
    assert!(!store.has_ephemeral_namespaces().unwrap());
    set_session(&store, b"s", b"v", MINUTE);
    assert!(store.has_ephemeral_namespaces().unwrap());
}

#[test]
fn a_read_only_store_reads_the_namespaces_and_writes_none() {
    let dir = std::env::temp_dir().join(format!("zoe-kv-read-only-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    {
        let store = Store::open(&dir).unwrap();
        set_registry(&store, b"title", b"site");
    }
    let store = Store::open_read_only(&dir).unwrap();
    assert_eq!(registry(&store, b"title").as_deref(), Some(&b"site"[..]));
    let mut txn = Txn::begin(&store, b"", b"", b"");
    txn.kv_set(Flavour::Durable, "registry", b"title", b"other", None)
        .unwrap();
    assert!(matches!(txn.vote(), Err(ZoeError::ReadOnly)));
    assert!(matches!(store.sweep(1000), Err(ZoeError::ReadOnly)));
    drop(txn);
    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn bad_arguments_are_refused_and_stage_nothing() {
    let (store, _, _) = open();
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    let long_name = "n".repeat(256);
    let long_key = vec![b'k'; 1025];
    let refusals = [
        txn.kv_set(Flavour::Durable, "", b"k", b"v", None),
        txn.kv_set(Flavour::Durable, &long_name, b"k", b"v", None),
        txn.kv_set(Flavour::Durable, "registry", b"", b"v", None),
        txn.kv_set(Flavour::Durable, "registry", &long_key, b"v", None),
        txn.kv_set(Flavour::Durable, "registry", b"k", b"v", Some(MINUTE)),
        txn.kv_set(Flavour::Ephemeral, "sessions", b"k", b"v", None),
        txn.kv_set(
            Flavour::Ephemeral,
            "sessions",
            b"k",
            b"v",
            Some(Duration::ZERO),
        ),
        txn.kv_set(
            Flavour::Ephemeral,
            "sessions",
            b"k",
            b"v",
            Some(Duration::from_secs(u64::from(u32::MAX) + 1)),
        ),
        txn.kv_delete(Flavour::Durable, "registry", b""),
    ];
    for refusal in refusals {
        assert!(
            matches!(refusal, Err(ZoeError::InvalidKv(_))),
            "{refusal:?}"
        );
    }
    txn.kv_set(
        Flavour::Durable,
        &"n".repeat(255),
        &[b'k'; 1024],
        b"v",
        None,
    )
    .unwrap();
    txn.kv_set(
        Flavour::Ephemeral,
        "sessions",
        b"k",
        b"v",
        Some(Duration::from_secs(u64::from(u32::MAX))),
    )
    .unwrap();
    txn.vote().unwrap();
    // After the vote, a namespace write is out of order.
    assert!(matches!(
        txn.kv_set(Flavour::Durable, "registry", b"k", b"v", None),
        Err(ZoeError::Protocol(_))
    ));
    txn.finish().unwrap();
}

#[test]
fn a_value_counts_toward_the_size_limits() {
    let options = Options {
        max_record_size: Some(10),
        // A row key is 1 + 8 + 1 bytes here, and it counts toward the size of
        // the transaction with the value.
        max_transaction_size: Some(36),
        ..Options::default()
    };
    let (store, _, _) = open_with(options);
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    assert!(matches!(
        txn.kv_set(Flavour::Durable, "registry", b"k", &[0; 11], None),
        Err(ZoeError::KvTooLarge {
            size: 11,
            max: 10,
            ..
        })
    ));
    txn.kv_set(Flavour::Durable, "registry", b"k", &[0; 10], None)
        .unwrap();
    // A value that replaces a staged value of its key counts once.
    txn.kv_set(Flavour::Durable, "registry", b"k", &[0; 9], None)
        .unwrap();
    let refused = txn.kv_set(Flavour::Durable, "registry", b"l", &[0; 8], None);
    assert!(
        matches!(
            refused,
            Err(ZoeError::KvTooLarge {
                size: 37,
                max: 36,
                ..
            })
        ),
        "{refused:?}"
    );
    txn.kv_set(Flavour::Durable, "registry", b"l", &[0; 7], None)
        .unwrap();
    txn.vote().unwrap();
    txn.finish().unwrap();
    assert_eq!(registry(&store, b"k"), Some(vec![0; 9]));
}

#[test]
fn a_restore_transaction_refuses_namespace_writes() {
    let (store, _, _) = open();
    let tid = Tid(store.last_transaction().0 + 1000);
    let mut txn = store.tpc_begin_restore(tid, b"", b"", b"").unwrap();
    assert!(matches!(
        txn.kv_set(Flavour::Durable, "registry", b"k", b"v", None),
        Err(ZoeError::Protocol(_))
    ));
    assert!(matches!(
        txn.kv_delete(Flavour::Durable, "registry", b"k"),
        Err(ZoeError::Protocol(_))
    ));
}

#[test]
fn another_thread_cannot_write_a_namespace_of_the_transaction() {
    let (store, _, _) = open();
    let store = Arc::new(store);
    let mut txn = Txn::begin(Arc::clone(&store), b"", b"", b"");
    let mut txn = std::thread::scope(|scope| {
        scope
            .spawn(move || {
                let refused = txn.kv_set(Flavour::Durable, "registry", b"k", b"v", None);
                assert!(matches!(refused, Err(ZoeError::WrongThread)));
                txn
            })
            .join()
            .unwrap()
    });
    txn.vote().unwrap();
    txn.finish().unwrap();
    assert_eq!(registry(&store, b"k"), None);
}

#[test]
fn values_past_the_staging_memory_spill_and_commit() {
    let options = Options {
        staging_memory: 4,
        ..Options::default()
    };
    let (store, _, vfs) = open_with(options);
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.kv_set(Flavour::Durable, "registry", b"a", b"0123456789", None)
        .unwrap();
    assert!(vfs.names().iter().any(|name| name.starts_with("staging/")));
    txn.vote().unwrap();
    txn.finish().unwrap();
    assert_eq!(registry(&store, b"a").as_deref(), Some(&b"0123456789"[..]));
    assert!(!vfs.names().iter().any(|name| name.ends_with(".spill")));
}

#[test]
fn a_pack_leaves_the_namespaces_as_they_are() {
    // The test pickles name no references, so the pack collects no
    // garbage.
    let (store, _, _) = open_with(Options {
        pack_gc: false,
        ..Options::default()
    });
    let oid = store.new_oid().unwrap();
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(Oid(0), Tid::ZERO, PICKLE).unwrap();
    txn.vote().unwrap();
    let root = txn.finish().unwrap();
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(oid, Tid::ZERO, PICKLE).unwrap();
    txn.vote().unwrap();
    txn.finish().unwrap();
    set_registry(&store, b"title", b"site");
    set_session(&store, b"s", b"v", MINUTE);
    store.pack(store.last_transaction()).unwrap();
    assert_eq!(registry(&store, b"title").as_deref(), Some(&b"site"[..]));
    assert_eq!(session(&store, b"s").as_deref(), Some(&b"v"[..]));
    assert!(store.load(Oid(0)).unwrap().1 == root);
}

#[test]
fn verify_counts_the_keys_and_finds_damaged_rows() {
    let (store, clock, _) = open();
    set_session(&store, b"s1", b"v", MINUTE);
    set_session(&store, b"s2", b"v", Duration::from_secs(3600));
    set_registry(&store, b"title", b"site");
    clock.advance(MINUTE);
    let check = store.verify_kv().unwrap();
    assert_eq!(check.ephemeral_keys, 2);
    assert_eq!(check.durable_keys, 1);
    assert_eq!(check.expired_keys, 1);
    assert!(check.findings.is_empty(), "{:?}", check.findings);

    // A damaged frame, a row without its index key, and a namespace
    // without its generation row.
    let row = testing::kv_row_key(b"registry", b"title");
    let mut frame = testing::read_kv_frame(&store, Flavour::Durable, &row).unwrap();
    frame[33] ^= 0x10;
    testing::write_kv_frame(&store, Flavour::Durable, &row, Some(&frame)).unwrap();
    let s2 = testing::kv_row_key(b"sessions", b"s2");
    let s2_frame = testing::read_kv_frame(&store, Flavour::Ephemeral, &s2).unwrap();
    let expiry = u64::from_le_bytes(s2_frame[32..40].try_into().unwrap());
    testing::write_kv_expiry_key(&store, &testing::kv_expiry_key(expiry, &s2), false).unwrap();
    // A commit writes the namespace `other`, then its generation row goes.
    let generation = testing::kv_row_key(b"other", b"");
    let orphan = testing::kv_row_key(b"other", b"k");
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.kv_set(Flavour::Durable, "other", b"k", b"v", None)
        .unwrap();
    txn.vote().unwrap();
    txn.finish().unwrap();
    testing::write_kv_frame(&store, Flavour::Durable, &generation, None).unwrap();
    let check = store.verify_kv().unwrap();
    let problems: Vec<_> = check
        .findings
        .iter()
        .map(|f| (f.flavour, f.namespace.clone(), f.problem.clone()))
        .collect();
    assert!(
        problems.contains(&(
            Flavour::Durable,
            Some("registry".to_owned()),
            "the frame fails its checksum".to_owned()
        )),
        "{problems:?}"
    );
    assert!(problems.contains(&(
        Flavour::Ephemeral,
        Some("sessions".to_owned()),
        "the index of the expiries has no key for the row".to_owned()
    )));
    assert!(problems.contains(&(
        Flavour::Durable,
        Some("other".to_owned()),
        "the namespace has keys and no generation row".to_owned()
    )));
    // A finding names no key, only the hash of the key.
    for finding in &check.findings {
        assert!(!finding.problem.contains("title"));
    }
    let damaged = check
        .findings
        .iter()
        .find(|f| f.problem == "the frame fails its checksum")
        .unwrap();
    assert_eq!(damaged.key_hash, Some(xxhash_rust::xxh3::xxh3_64(b"title")));
    assert!(
        testing::read_kv_frame(&store, Flavour::Durable, &orphan).is_ok(),
        "the orphan row stays"
    );
}

#[test]
fn every_flipped_bit_of_a_namespace_frame_raises_corrupt_kv() {
    let (store, _, _) = open();
    set_session(&store, b"s1", b"session data", MINUTE);
    set_registry(&store, b"title", b"site title");
    for (flavour, name, key) in [
        (Flavour::Ephemeral, "sessions", &b"s1"[..]),
        (Flavour::Durable, "registry", &b"title"[..]),
        (Flavour::Durable, "registry", &b""[..]),
    ] {
        let row = testing::kv_row_key(name.as_bytes(), key);
        let frame = testing::read_kv_frame(&store, flavour, &row).unwrap();
        for byte in 0..frame.len() {
            for bit in 0..8 {
                let mut bad = frame.clone();
                bad[byte] ^= 1 << bit;
                testing::write_kv_frame(&store, flavour, &row, Some(&bad)).unwrap();
                let read = if key.is_empty() {
                    store.kv_generation(name).map(|_| None)
                } else {
                    store.kv_get(flavour, name, key)
                };
                let scan = store.kv_scan(flavour, name, None, None, 1000);
                assert!(
                    matches!(read, Err(ZoeError::CorruptKv { .. })),
                    "{flavour:?} byte {byte} bit {bit}: {read:?}"
                );
                if !key.is_empty() {
                    assert!(
                        matches!(scan, Err(ZoeError::CorruptKv { .. })),
                        "{flavour:?} byte {byte} bit {bit}: {scan:?}"
                    );
                }
            }
        }
        testing::write_kv_frame(&store, flavour, &row, Some(&frame)).unwrap();
    }
}
