//! Acceptance test of the `Data.fs` to ZOE converter. It reads the
//! synthetic fixture in `tests/data-fs`, which `generate.py` wrote with
//! ZODB, and checks that the store holds the same transactions with the
//! same tids and metadata, byte-identical pickles, every back pointer
//! followed, and every blob in the bushy and the lawn layouts. A
//! conversion resumes after the last tid of the store, and its report
//! names what the verification found.

use std::io::Read;
use std::path::PathBuf;

use zoe_convert::from_datafs;
use zoe_core::{Oid, Store, Tid};

/// The fixture directory.
fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data-fs")
}

fn fresh_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("zoe-from-data-fs-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// One transaction of `expected.txt`.
struct Txn {
    tid: Tid,
    user: Vec<u8>,
    description: Vec<u8>,
    extension: Vec<u8>,
    /// The records in file order. The data is `None` for a record that
    /// undoes the creation of its object.
    records: Vec<(Oid, Option<Vec<u8>>)>,
    /// The blob files: the oid, then the path in the bushy and in the lawn
    /// directory.
    blobs: Vec<(Oid, String, String)>,
}

fn number(field: &str) -> u64 {
    u64::from_str_radix(field, 16).unwrap()
}

fn bytes(field: &str) -> Vec<u8> {
    let hex = field.strip_prefix('x').unwrap();
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect()
}

/// Read `expected.txt`: what ZODB reads from the fixture.
fn expected() -> Vec<Txn> {
    let text = std::fs::read_to_string(fixture().join("expected.txt")).unwrap();
    let mut txns: Vec<Txn> = Vec::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split(' ').collect();
        match fields[..] {
            ["txn", tid, _status, user, description, extension] => txns.push(Txn {
                tid: Tid(number(tid)),
                user: bytes(user),
                description: bytes(description),
                extension: bytes(extension),
                records: Vec::new(),
                blobs: Vec::new(),
            }),
            ["rec", oid, data] => {
                let data = (data != "-").then(|| bytes(data));
                txns.last_mut()
                    .unwrap()
                    .records
                    .push((Oid(number(oid)), data));
            }
            ["blob", oid, tid, bushy, lawn] => {
                let txn = txns.last_mut().unwrap();
                assert_eq!(Tid(number(tid)), txn.tid);
                txn.blobs
                    .push((Oid(number(oid)), bushy.to_owned(), lawn.to_owned()));
            }
            _ => panic!("unknown line in expected.txt: {line}"),
        }
    }
    txns
}

fn read_blob(store: &Store, oid: Oid, tid: Tid) -> Vec<u8> {
    let mut bytes = Vec::new();
    store
        .open_blob(oid, tid)
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    bytes
}

/// Check that the store holds the transactions `expected`, with their
/// records and their blobs.
fn assert_store_holds(store: &Store, expected: &[Txn]) {
    let txns: Vec<_> = store
        .iterator(None, None)
        .unwrap()
        .map(|txn| txn.unwrap())
        .collect();
    let tids: Vec<Tid> = txns.iter().map(|txn| txn.tid).collect();
    let expected_tids: Vec<Tid> = expected.iter().map(|txn| txn.tid).collect();
    assert_eq!(tids, expected_tids);

    for (txn, want) in txns.iter().zip(expected) {
        let tid = want.tid;
        assert_eq!(txn.user, want.user, "{tid:?}");
        assert_eq!(txn.description, want.description, "{tid:?}");
        assert_eq!(txn.extension, want.extension, "{tid:?}");
        let mut records: Vec<(Oid, Option<Vec<u8>>)> = txn
            .records
            .iter()
            .map(|record| {
                assert_eq!(record.tid, tid);
                (record.oid, record.data.clone())
            })
            .collect();
        records.sort();
        let mut want_records = want.records.clone();
        want_records.sort();
        assert_eq!(records, want_records, "{tid:?}");

        for (oid, bushy, _) in &want.blobs {
            let file = std::fs::read(fixture().join("blobs-bushy").join(bushy)).unwrap();
            assert_eq!(read_blob(store, *oid, tid), file, "{oid:?} at {tid:?}");
            store.verify_blob(*oid, tid).unwrap();
        }
    }
    assert_eq!(store.last_transaction(), expected.last().unwrap().tid);
}

/// The file offsets where the transactions of a `Data.fs` end.
fn transaction_ends(data: &[u8]) -> Vec<usize> {
    let mut ends = Vec::new();
    let mut pos = 4;
    while pos < data.len() {
        let len = u64::from_be_bytes(data[pos + 8..pos + 16].try_into().unwrap()) as usize;
        pos += len + 8;
        ends.push(pos);
    }
    ends
}

fn record_count(expected: &[Txn]) -> u64 {
    expected.iter().map(|txn| txn.records.len() as u64).sum()
}

fn blob_count(expected: &[Txn]) -> u64 {
    expected.iter().map(|txn| txn.blobs.len() as u64).sum()
}

/// Return the back pointers of the data records of a `Data.fs`.
fn back_pointers(data: &[u8]) -> Vec<u64> {
    let number = |at: usize| u64::from_be_bytes(data[at..at + 8].try_into().unwrap());
    let field = |at: usize| usize::from(u16::from_be_bytes(data[at..at + 2].try_into().unwrap()));
    let mut backs = Vec::new();
    let mut pos = 4;
    for end in transaction_ends(data) {
        let mut record = pos + 23 + field(pos + 17) + field(pos + 19) + field(pos + 21);
        while record < end - 8 {
            let len = number(record + 34) as usize;
            if len == 0 {
                backs.push(number(record + 42));
                record += 50;
            } else {
                record += 42 + len;
            }
        }
        pos = end;
    }
    backs
}

#[test]
fn the_fixture_holds_what_the_tests_need() {
    let data = std::fs::read(fixture().join("Data.fs")).unwrap();
    assert_eq!(&data[..4], b"FS30");
    assert_eq!(*transaction_ends(&data).last().unwrap(), data.len());
    // A back pointer to an earlier record, and one to no data.
    let backs = back_pointers(&data);
    assert!(backs.iter().any(|&back| back != 0), "{backs:?}");
    assert!(backs.contains(&0), "{backs:?}");
    // A packed transaction, and two blob revisions of one object.
    let text = std::fs::read_to_string(fixture().join("expected.txt")).unwrap();
    assert!(
        text.lines()
            .any(|line| line.starts_with("txn ") && line.contains(" x70 "))
    );
    assert!(blob_count(&expected()) >= 2);
}

#[test]
fn a_data_fs_with_bushy_blobs_converts_with_everything_in_it() {
    let expected = expected();
    let store = Store::open(fresh_dir("bushy")).unwrap();
    let report = from_datafs(
        &fixture().join("Data.fs"),
        Some(&fixture().join("blobs-bushy")),
        &store,
    )
    .unwrap();

    assert!(report.problems.is_empty(), "{report:?}");
    assert_eq!(report.transactions, expected.len() as u64, "{report:?}");
    assert_eq!(report.copied, expected.len() as u64, "{report:?}");
    assert_eq!(report.records, record_count(&expected), "{report:?}");
    assert_eq!(report.blobs, blob_count(&expected), "{report:?}");
    assert_eq!(report.last_tid, expected.last().unwrap().tid, "{report:?}");
    assert_store_holds(&store, &expected);

    // New oids never reuse a converted oid.
    let largest = expected
        .iter()
        .flat_map(|txn| txn.records.iter().map(|(oid, _)| *oid))
        .max()
        .unwrap();
    assert!(store.new_oid().unwrap() > largest);
}

#[test]
fn a_data_fs_with_lawn_blobs_converts_the_same() {
    let expected = expected();
    let store = Store::open(fresh_dir("lawn")).unwrap();
    let report = from_datafs(
        &fixture().join("Data.fs"),
        Some(&fixture().join("blobs-lawn")),
        &store,
    )
    .unwrap();
    assert!(report.problems.is_empty(), "{report:?}");
    assert_eq!(report.blobs, blob_count(&expected), "{report:?}");
    assert_store_holds(&store, &expected);
}

#[test]
fn a_conversion_resumes_after_the_last_tid_of_the_store() {
    let expected = expected();
    let dir = fresh_dir("resume");
    let data = std::fs::read(fixture().join("Data.fs")).unwrap();
    let ends = transaction_ends(&data);
    let first = ends.len() / 2;
    // A copy of the first transactions stands in for a `Data.fs` that grew
    // after the first run.
    let partial = dir.join("partial.fs");
    std::fs::write(&partial, &data[..ends[first - 1]]).unwrap();
    let blobs = fixture().join("blobs-bushy");
    let store = Store::open(dir.join("store")).unwrap();

    let report = from_datafs(&partial, Some(&blobs), &store).unwrap();
    assert!(report.problems.is_empty(), "{report:?}");
    assert_eq!(report.transactions, first as u64, "{report:?}");
    assert_eq!(report.copied, first as u64, "{report:?}");
    assert_eq!(report.last_tid, expected[first - 1].tid, "{report:?}");
    assert_store_holds(&store, &expected[..first]);

    let report = from_datafs(&fixture().join("Data.fs"), Some(&blobs), &store).unwrap();
    assert!(report.problems.is_empty(), "{report:?}");
    assert_eq!(report.transactions, expected.len() as u64, "{report:?}");
    assert_eq!(report.copied, (expected.len() - first) as u64, "{report:?}");
    assert_store_holds(&store, &expected);

    // A run on a store that has every transaction copies nothing.
    let report = from_datafs(&fixture().join("Data.fs"), Some(&blobs), &store).unwrap();
    assert!(report.problems.is_empty(), "{report:?}");
    assert_eq!(report.copied, 0, "{report:?}");
    assert_store_holds(&store, &expected);
}

#[test]
fn the_report_names_a_missing_blob() {
    let expected = expected();
    let store = Store::open(fresh_dir("missing-blob")).unwrap();
    let blobs = fixture().join("blobs-bushy");
    from_datafs(&fixture().join("Data.fs"), Some(&blobs), &store).unwrap();

    let txn = expected.iter().find(|txn| !txn.blobs.is_empty()).unwrap();
    let (oid, _, _) = &txn.blobs[0];
    std::fs::remove_file(store.blob_path(*oid, txn.tid).unwrap()).unwrap();

    let report = from_datafs(&fixture().join("Data.fs"), Some(&blobs), &store).unwrap();
    assert_eq!(report.copied, 0, "{report:?}");
    assert!(!report.problems.is_empty(), "{report:?}");
}

#[test]
fn a_store_with_other_transactions_does_not_verify() {
    let store = Store::open(fresh_dir("other")).unwrap();
    let oid = store.new_oid().unwrap();
    let mut txn = store.tpc_begin(b"", b"not from the Data.fs", b"").unwrap();
    txn.store(
        oid,
        Tid::ZERO,
        b"\x80\x03cpersistent.mapping\nPersistentMapping\nq\x00.\x80\x03}.",
    )
    .unwrap();
    txn.vote().unwrap();
    txn.finish().unwrap();

    let blobs = fixture().join("blobs-bushy");
    match from_datafs(&fixture().join("Data.fs"), Some(&blobs), &store) {
        Err(_) => {}
        Ok(report) => assert!(!report.problems.is_empty(), "{report:?}"),
    }
}

#[test]
fn a_file_that_is_not_a_data_fs_is_refused() {
    let dir = fresh_dir("not-a-data-fs");
    let path = dir.join("Data.fs");
    std::fs::write(&path, b"this is not a FileStorage file").unwrap();
    let store = Store::open(dir.join("store")).unwrap();

    assert!(from_datafs(&path, None, &store).is_err());
    assert_eq!(store.last_transaction(), Tid::ZERO);
}

#[test]
fn each_blob_directory_of_the_fixture_names_its_layout() {
    for (dir, layout) in [("blobs-bushy", "bushy"), ("blobs-lawn", "lawn")] {
        let path = fixture().join(dir).join(".layout");
        assert_eq!(std::fs::read_to_string(path).unwrap().trim(), layout);
    }
}
