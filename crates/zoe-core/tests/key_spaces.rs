//! Acceptance test of the key space in the frame checksum.
//!
//! Records and blob records have the same 16-byte key `(oid, tid)`.
//! Transaction rows have the 8-byte key of the tid. The test copies a
//! stored frame, byte for byte, from one key space into the place of a
//! row of another key space. The copy must fail its checksum there. So
//! every read of it raises `CorruptRecord`, `CorruptBlob` or `CorruptTxn`
//! and never returns data. The test covers each ordered pair of the key
//! spaces `records`, `blobs` and `txns`. A store that nothing damaged
//! reads back what it wrote, with codec 0 and with codec 1.
//!
//! The test reads and writes the stored frames through the test hook
//! `zoe_core::testing::{Key, read_frame, write_frame}`. The feature
//! `testing` of `zoe-core` builds the hook. The dev-dependency of
//! `zoe-core` on itself turns the feature on.

mod support;

use std::collections::BTreeMap;
use std::io::{Read, Write};

use support::fresh_dir;
use zoe_core::testing::{self, Key, compressible};
use zoe_core::vfs::MemVfs;
use zoe_core::{Codec, Compression, Oid, Options, Store, Tid, ZoeError};

const USER: &[u8] = b"someone";

const EXTENSION: &[u8] = b"ext";

const BLOB_PICKLE: &[u8] = b"\x80\x03cZODB.blob\nBlob\nq\x00.\x80\x03N.";

/// A small protocol-3 record.
fn small(title: &str) -> Vec<u8> {
    let mut data = b"\x80\x03cpersistent.mapping\nPersistentMapping\nq\x00.\x80\x03X".to_vec();
    data.extend_from_slice(&(title.len() as u32).to_le_bytes());
    data.extend_from_slice(title.as_bytes());
    data.push(b'.');
    data
}

/// Open a store in memory that compresses its records with zstd, so that
/// the fixture holds a zstd frame.
fn open_zstd() -> Store {
    let options = Options {
        compression: Compression::Zstd { level: 3 },
        ..Options::default()
    };
    Store::open_full(MemVfs::new(), zoe_core::SystemClock, options).unwrap()
}

fn content(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 7 + 3) as u8).collect()
}

/// The 40-byte payload of the blob record of `bytes`: the BLAKE3 hash,
/// then the size as a little-endian `u64`.
fn blob_payload(bytes: &[u8]) -> Vec<u8> {
    let mut payload = blake3::hash(bytes).as_bytes().to_vec();
    payload.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    payload
}

/// What `fill` committed.
struct Fixture {
    /// Each record with its pickle.
    records: BTreeMap<(Oid, Tid), Vec<u8>>,
    /// Each blob with its bytes.
    blobs: BTreeMap<(Oid, Tid), Vec<u8>>,
    /// The description of each transaction.
    txns: BTreeMap<Tid, &'static [u8]>,
    /// The record that zstd shrinks.
    compressed: (Oid, Tid),
}

impl Fixture {
    /// Tell if the record is the newest record of its oid.
    fn is_current(&self, oid: Oid, tid: Tid) -> bool {
        self.records.keys().all(|&(o, t)| o != oid || t <= tid)
    }
}

/// Commit two transactions:
///
/// - The first writes a small record of oid 1 and a record of oid 2 that
///   zstd shrinks.
/// - The second writes a new record of oid 1, and the blobs of oids 3 and
///   4. The pickle of oid 4 is the payload of its own blob record. So the
///   record and the blob record of oid 4 have the same key, the same
///   payload and the same header up to the checksum. Only the key space
///   tells them apart.
fn fill(store: &Store) -> Fixture {
    let (a, b, c, d) = (Oid(1), Oid(2), Oid(3), Oid(4));
    let mut txn = store.tpc_begin(USER, b"records", EXTENSION).unwrap();
    txn.store(a, Tid::ZERO, &small("first")).unwrap();
    txn.store(b, Tid::ZERO, &compressible()).unwrap();
    txn.vote().unwrap();
    let t1 = txn.finish().unwrap();

    let (c_bytes, d_bytes) = (content(256), content(100_000));
    let d_pickle = blob_payload(&d_bytes);
    let mut txn = store.tpc_begin(USER, b"blobs", EXTENSION).unwrap();
    txn.store(a, t1, &small("second")).unwrap();
    for (oid, pickle, bytes) in [(c, BLOB_PICKLE, &c_bytes), (d, &d_pickle[..], &d_bytes)] {
        let mut file = store.new_blob_file().unwrap();
        file.write_all(bytes).unwrap();
        txn.store_blob(oid, Tid::ZERO, pickle, file).unwrap();
    }
    txn.vote().unwrap();
    let t2 = txn.finish().unwrap();

    Fixture {
        records: BTreeMap::from([
            ((a, t1), small("first")),
            ((b, t1), compressible()),
            ((a, t2), small("second")),
            ((c, t2), BLOB_PICKLE.to_vec()),
            ((d, t2), d_pickle),
        ]),
        blobs: BTreeMap::from([((c, t2), c_bytes), ((d, t2), d_bytes)]),
        txns: BTreeMap::from([(t1, &b"records"[..]), (t2, &b"blobs"[..])]),
        compressed: (b, t1),
    }
}

/// Store `bytes` under `key` in the place of its row, run `check`, then
/// put the row back.
fn with_frame(store: &Store, key: Key, bytes: &[u8], check: impl FnOnce()) {
    let original = testing::read_frame(store, key).unwrap();
    testing::write_frame(store, key, bytes).unwrap();
    check();
    testing::write_frame(store, key, &original).unwrap();
}

/// What a read gave: a short account of the data that it returned, or its
/// error.
type Outcome = Result<String, ZoeError>;

/// Run every read that touches the record of `oid` at `tid`.
fn record_reads(
    store: &Store,
    fixture: &Fixture,
    oid: Oid,
    tid: Tid,
) -> Vec<(&'static str, Outcome)> {
    let mut reads = vec![
        (
            "load_serial",
            store
                .load_serial(oid, tid)
                .map(|data| format!("{} bytes", data.len())),
        ),
        (
            "load_before",
            store
                .load_before(oid, Tid(tid.0 + 1))
                .map(|found| format!("{:?} bytes", found.map(|found| found.data.len()))),
        ),
        (
            "history",
            store.history(oid, 100).map(|entries| {
                let sizes: Vec<u64> = entries.iter().map(|entry| entry.size).collect();
                format!("sizes {sizes:?}")
            }),
        ),
        (
            "record_info",
            store.record_info(oid, tid).map(|info| format!("{info:?}")),
        ),
        ("iterator", iterate(store, Some(tid), Some(tid))),
    ];
    if fixture.is_current(oid, tid) {
        reads.push((
            "load",
            store
                .load(oid)
                .map(|(data, _)| format!("{} bytes", data.len())),
        ));
        reads.push((
            "record_iternext",
            store
                .record_iternext(Some(oid))
                .map(|found| format!("{:?} bytes", found.map(|found| found.data.len()))),
        ));
    }
    reads
}

/// Iterate over the transactions from `start` to `stop`.
fn iterate(store: &Store, start: Option<Tid>, stop: Option<Tid>) -> Outcome {
    let txns = store
        .iterator(start, stop)?
        .collect::<Result<Vec<_>, _>>()?;
    let sizes: Vec<Vec<Option<usize>>> = txns
        .iter()
        .map(|txn| {
            txn.records
                .iter()
                .map(|record| record.data.as_ref().map(Vec::len))
                .collect()
        })
        .collect();
    Ok(format!("record sizes {sizes:?}"))
}

/// Run every read that touches the transaction row of `tid`. `oid` has a
/// record at `tid`.
fn txn_reads(store: &Store, tid: Tid, oid: Oid) -> Vec<(&'static str, Outcome)> {
    vec![
        ("iterator over the log", iterate(store, None, None)),
        (
            "iterator over the transaction",
            iterate(store, Some(tid), Some(tid)),
        ),
        (
            "history",
            store.history(oid, 100).map(|entries| {
                let descriptions: Vec<String> = entries
                    .iter()
                    .map(|entry| String::from_utf8_lossy(&entry.description).into_owned())
                    .collect();
                format!("descriptions {descriptions:?}")
            }),
        ),
    ]
}

/// Read the blob of `oid` at `tid` to its end. A failed read gives the
/// `ZoeError` inside its I/O error.
fn read_blob(store: &Store, oid: Oid, tid: Tid) -> Result<Vec<u8>, ZoeError> {
    let mut reader = store.open_blob(oid, tid)?;
    let mut bytes = Vec::new();
    match reader.read_to_end(&mut bytes) {
        Ok(_) => Ok(bytes),
        Err(e) if e.get_ref().is_some_and(|inner| inner.is::<ZoeError>()) => {
            let inner = e.into_inner().unwrap().downcast::<ZoeError>().unwrap();
            Err(*inner)
        }
        Err(e) => Err(e.into()),
    }
}

/// Run every read that touches the blob record of `oid` at `tid`.
fn blob_reads(store: &Store, oid: Oid, tid: Tid) -> Vec<(&'static str, Outcome)> {
    vec![
        (
            "open_blob and a read to the end",
            read_blob(store, oid, tid).map(|bytes| format!("{} bytes", bytes.len())),
        ),
        (
            "verify_blob",
            store
                .verify_blob(oid, tid)
                .map(|()| "a valid blob".to_owned()),
        ),
        (
            "blob_path",
            store
                .blob_path(oid, tid)
                .map(|path| format!("{}", path.display())),
        ),
    ]
}

fn assert_corrupt_record(store: &Store, fixture: &Fixture, oid: Oid, tid: Tid, moved: &str) {
    for (read, outcome) in record_reads(store, fixture, oid, tid) {
        assert!(
            matches!(outcome, Err(ZoeError::CorruptRecord { oid: o, tid: t }) if o == oid && t == tid),
            "{moved} in the place of the record of {oid:?} at {tid:?}: {read} gave {outcome:?}"
        );
    }
}

fn assert_corrupt_blob(store: &Store, oid: Oid, tid: Tid, moved: &str) {
    for (read, outcome) in blob_reads(store, oid, tid) {
        assert!(
            matches!(outcome, Err(ZoeError::CorruptBlob { oid: o, tid: t }) if o == oid && t == tid),
            "{moved} in the place of the blob record of {oid:?} at {tid:?}: {read} gave {outcome:?}"
        );
    }
}

fn assert_corrupt_txn(store: &Store, tid: Tid, oid: Oid, moved: &str) {
    for (read, outcome) in txn_reads(store, tid, oid) {
        assert!(
            matches!(outcome, Err(ZoeError::CorruptTxn { tid: t }) if t == tid),
            "{moved} in the place of the transaction row of {tid:?}: {read} gave {outcome:?}"
        );
    }
}

/// Check that every read gives what `fill` committed.
fn assert_intact(store: &Store, fixture: &Fixture) {
    for (&(oid, tid), data) in &fixture.records {
        let what = format!("{oid:?} at {tid:?}");
        assert_eq!(&store.load_serial(oid, tid).unwrap(), data, "{what}");
        let found = store.load_before(oid, Tid(tid.0 + 1)).unwrap().unwrap();
        assert_eq!((&found.data, found.serial), (data, tid), "{what}");
        let info = store.record_info(oid, tid).unwrap();
        assert_eq!(info.data_len, data.len() as u64, "{what}");
        if fixture.is_current(oid, tid) {
            assert_eq!(store.load(oid).unwrap(), (data.clone(), tid), "{what}");
        }

        let history: Vec<(Tid, Vec<u8>, Vec<u8>, u64)> = store
            .history(oid, 100)
            .unwrap()
            .into_iter()
            .map(|entry| (entry.tid, entry.user, entry.description, entry.size))
            .collect();
        let expected: Vec<(Tid, Vec<u8>, Vec<u8>, u64)> = fixture
            .records
            .iter()
            .rev()
            .filter(|((o, _), _)| *o == oid)
            .map(|(&(_, t), data)| {
                let description = fixture.txns[&t].to_vec();
                (t, USER.to_vec(), description, data.len() as u64)
            })
            .collect();
        assert_eq!(history, expected, "history of {oid:?}");
    }

    let mut iterated = BTreeMap::new();
    let mut tids = Vec::new();
    for txn in store.iterator(None, None).unwrap() {
        let txn = txn.unwrap();
        assert_eq!(txn.user, USER, "{:?}", txn.tid);
        assert_eq!(txn.description, fixture.txns[&txn.tid], "{:?}", txn.tid);
        assert_eq!(txn.extension, EXTENSION, "{:?}", txn.tid);
        tids.push(txn.tid);
        for record in txn.records {
            assert_eq!(record.tid, txn.tid);
            iterated.insert((record.oid, record.tid), record.data.unwrap());
        }
    }
    assert_eq!(tids, fixture.txns.keys().copied().collect::<Vec<_>>());
    assert_eq!(iterated, fixture.records);

    let mut current = BTreeMap::new();
    let mut next = None;
    while let Some(found) = store.record_iternext(next).unwrap() {
        current.insert(found.oid, (found.tid, found.data));
        match found.next {
            Some(oid) => next = Some(oid),
            None => break,
        }
    }
    let expected: BTreeMap<Oid, (Tid, Vec<u8>)> = fixture
        .records
        .iter()
        .filter(|((oid, tid), _)| fixture.is_current(*oid, *tid))
        .map(|(&(oid, tid), data)| (oid, (tid, data.clone())))
        .collect();
    assert_eq!(current, expected, "record_iternext");

    for (&(oid, tid), bytes) in &fixture.blobs {
        let what = format!("the blob of {oid:?} at {tid:?}");
        assert_eq!(read_blob(store, oid, tid).unwrap(), *bytes, "{what}");
        store.verify_blob(oid, tid).unwrap();
        store.blob_path(oid, tid).unwrap();
    }
}

/// A damaged branch page of redb can send a lookup in `records` to a leaf
/// of `blobs`. The blob record that the lookup finds must not read as a
/// pickle.
#[test]
fn a_blob_record_in_the_place_of_a_record_raises_corrupt_record() {
    let store = open_zstd();
    let fixture = fill(&store);

    for &(oid, tid) in fixture.blobs.keys() {
        let blob = testing::read_frame(&store, Key::Blob(oid, tid)).unwrap();
        with_frame(&store, Key::Record(oid, tid), &blob, || {
            assert_corrupt_record(&store, &fixture, oid, tid, "the blob record");
        });
    }
    assert_intact(&store, &fixture);
}

#[test]
fn a_record_in_the_place_of_a_blob_record_raises_corrupt_blob() {
    let store = open_zstd();
    let fixture = fill(&store);

    for &(oid, tid) in fixture.blobs.keys() {
        let record = testing::read_frame(&store, Key::Record(oid, tid)).unwrap();
        with_frame(&store, Key::Blob(oid, tid), &record, || {
            assert_corrupt_blob(&store, oid, tid, "the record");
        });
    }
    assert_intact(&store, &fixture);
}

#[test]
fn a_transaction_row_in_the_place_of_a_record_raises_corrupt_record() {
    let store = open_zstd();
    let fixture = fill(&store);

    for &(oid, tid) in fixture.records.keys() {
        let row = testing::read_frame(&store, Key::Txn(tid)).unwrap();
        with_frame(&store, Key::Record(oid, tid), &row, || {
            assert_corrupt_record(&store, &fixture, oid, tid, "the transaction row");
        });
    }
    assert_intact(&store, &fixture);
}

#[test]
fn a_record_in_the_place_of_a_transaction_row_raises_corrupt_txn() {
    let store = open_zstd();
    let fixture = fill(&store);

    for &(oid, tid) in fixture.records.keys() {
        let record = testing::read_frame(&store, Key::Record(oid, tid)).unwrap();
        with_frame(&store, Key::Txn(tid), &record, || {
            let moved = format!("the record of {oid:?}");
            assert_corrupt_txn(&store, tid, oid, &moved);
        });
    }
    assert_intact(&store, &fixture);
}

#[test]
fn a_transaction_row_in_the_place_of_a_blob_record_raises_corrupt_blob() {
    let store = open_zstd();
    let fixture = fill(&store);

    for &(oid, tid) in fixture.blobs.keys() {
        let row = testing::read_frame(&store, Key::Txn(tid)).unwrap();
        with_frame(&store, Key::Blob(oid, tid), &row, || {
            assert_corrupt_blob(&store, oid, tid, "the transaction row");
        });
    }
    assert_intact(&store, &fixture);
}

#[test]
fn a_blob_record_in_the_place_of_a_transaction_row_raises_corrupt_txn() {
    let store = open_zstd();
    let fixture = fill(&store);

    for &(oid, tid) in fixture.blobs.keys() {
        let blob = testing::read_frame(&store, Key::Blob(oid, tid)).unwrap();
        with_frame(&store, Key::Txn(tid), &blob, || {
            let moved = format!("the blob record of {oid:?}");
            assert_corrupt_txn(&store, tid, oid, &moved);
        });
    }
    assert_intact(&store, &fixture);
}

#[test]
fn a_store_without_compression_reads_back_what_it_wrote() {
    let dir = fresh_dir("plain");
    let options = Options {
        compression: Compression::None,
        ..Options::default()
    };
    let fixture = fill(&Store::open_with(&dir, options).unwrap());

    let store = Store::open(&dir).unwrap();
    assert_intact(&store, &fixture);
    for &(oid, tid) in fixture.records.keys() {
        let info = store.record_info(oid, tid).unwrap();
        assert_eq!(info.codec, Codec::None, "{oid:?} at {tid:?}");
    }
}

#[test]
fn a_store_with_zstd_reads_back_what_it_wrote() {
    let dir = fresh_dir("zstd");
    let options = || Options {
        compression: Compression::Zstd { level: 3 },
        ..Options::default()
    };
    let fixture = fill(&Store::open_with(&dir, options()).unwrap());

    let store = Store::open_with(&dir, options()).unwrap();
    assert_intact(&store, &fixture);
    let (oid, tid) = fixture.compressed;
    let info = store.record_info(oid, tid).unwrap();
    assert_eq!(info.codec, Codec::Zstd);
    assert!(info.stored_len < info.data_len, "{info:?}");
}
