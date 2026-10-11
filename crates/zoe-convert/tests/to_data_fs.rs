//! Acceptance test of the ZOE to `Data.fs` converter, the exit path. The
//! writer turns a store into a `Data.fs` in FileStorage's format and a
//! blob directory. This test reads the file back with its own reader: the
//! header, the transaction records, the data records with their back
//! pointers, previous-record positions and transaction positions, and the
//! redundant lengths. A round trip `Data.fs` to ZOE to `Data.fs` to ZOE
//! keeps the transactions, the pickles and the blobs. A conversion resumes
//! after the last tid of the file.
//!
//! The Python test `test_exit_path.py` runs `data_fs_round_trips_through_zoe`
//! with `ZOE_EXIT_PATH_DIR` set to a directory that holds a `Data.fs` and
//! its `blobs` directory, and opens the result in ZODB.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use zoe_convert::{from_datafs, to_datafs};
use zoe_core::{Oid, Store, Tid};

/// The variable that names the input directory of the Python test.
const EXIT_PATH_DIR: &str = "ZOE_EXIT_PATH_DIR";

/// The fixture of the `Data.fs` reader.
fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data-fs")
}

fn fresh_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("zoe-to-data-fs-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// One transaction of a `Data.fs`, as this test reads it.
#[derive(Debug, PartialEq)]
struct FsTxn {
    tid: Tid,
    status: u8,
    user: Vec<u8>,
    description: Vec<u8>,
    extension: Vec<u8>,
    /// The records in file order: the oid and the data, after the back
    /// pointers. The data is `None` for a back pointer to no data.
    records: Vec<(Oid, Option<Vec<u8>>)>,
}

fn number(data: &[u8], at: usize) -> u64 {
    u64::from_be_bytes(data[at..at + 8].try_into().unwrap())
}

fn short(data: &[u8], at: usize) -> usize {
    usize::from(u16::from_be_bytes(data[at..at + 2].try_into().unwrap()))
}

/// Read a `Data.fs` and check every rule of FileStorage's format on the
/// way.
fn read_data_fs(data: &[u8]) -> Vec<FsTxn> {
    assert_eq!(&data[..4], b"FS30", "the file header");
    let mut txns = Vec::new();
    // The position of the newest data record of each oid, and the data of
    // each data record by position.
    let mut newest: BTreeMap<Oid, u64> = BTreeMap::new();
    let mut by_position: BTreeMap<u64, (Oid, Option<Vec<u8>>)> = BTreeMap::new();
    let mut pos = 4;
    let mut last_tid = Tid::ZERO;
    while pos < data.len() {
        let tid = Tid(number(data, pos));
        assert!(tid > last_tid, "the tids increase at {pos}");
        last_tid = tid;
        let len = number(data, pos + 8) as usize;
        let end = pos + len;
        assert_eq!(
            number(data, end) as usize,
            len,
            "the redundant length at {end}"
        );
        let status = data[pos + 16];
        let (user_len, description_len, extension_len) = (
            short(data, pos + 17),
            short(data, pos + 19),
            short(data, pos + 21),
        );
        let mut at = pos + 23;
        let mut field = |n: usize| {
            let bytes = data[at..at + n].to_vec();
            at += n;
            bytes
        };
        let (user, description, extension) = (
            field(user_len),
            field(description_len),
            field(extension_len),
        );

        let mut records = Vec::new();
        while at < end {
            let record = at as u64;
            let oid = Oid(number(data, at));
            assert_eq!(Tid(number(data, at + 8)), tid, "the serial at {at}");
            let previous = newest.get(&oid).copied().unwrap_or(0);
            assert_eq!(
                number(data, at + 16),
                previous,
                "the previous record at {at}"
            );
            assert_eq!(
                number(data, at + 24),
                pos as u64,
                "the transaction position at {at}"
            );
            assert_eq!(short(data, at + 32), 0, "the version length at {at}");
            let data_len = number(data, at + 34) as usize;
            let pickle = if data_len > 0 {
                at += 42 + data_len;
                Some(data[at - data_len..at].to_vec())
            } else {
                let back = number(data, at + 42);
                at += 50;
                if back == 0 {
                    None
                } else {
                    let (back_oid, back_data) = by_position
                        .get(&back)
                        .unwrap_or_else(|| panic!("the back pointer at {record} to {back}"));
                    assert_eq!(*back_oid, oid, "the back pointer at {record}");
                    back_data.clone()
                }
            };
            newest.insert(oid, record);
            by_position.insert(record, (oid, pickle.clone()));
            records.push((oid, pickle));
        }
        assert_eq!(at, end, "the data records end with their transaction");
        txns.push(FsTxn {
            tid,
            status,
            user,
            description,
            extension,
            records,
        });
        pos = end + 8;
    }
    assert_eq!(pos, data.len(), "the file ends after its last transaction");
    txns
}

/// Return the name that ZODB gives to an oid or a tid in a blob path:
/// `0x`, then the hex digits without leading zeros, in pairs.
fn repr(n: u64) -> String {
    let hex = format!("{n:x}");
    let hex = if hex.len().is_multiple_of(2) {
        hex
    } else {
        format!("0{hex}")
    };
    format!("0x{hex}")
}

/// Return the path of the blob file of `oid` at `tid` in `dir`, in the
/// layout that the `.layout` file of `dir` names.
fn blob_file(dir: &Path, oid: Oid, tid: Tid) -> PathBuf {
    let layout = std::fs::read_to_string(dir.join(".layout")).unwrap();
    let mut path = dir.to_path_buf();
    match layout.trim() {
        "bushy" => {
            for byte in oid.0.to_be_bytes() {
                path.push(format!("0x{byte:02x}"));
            }
        }
        "lawn" => path.push(repr(oid.0)),
        other => panic!("unknown blob layout {other}"),
    }
    path.push(format!("{}.blob", repr(tid.0)));
    path
}

/// The blobs of a store: the oid, the tid and the bytes of each.
type Blobs = Vec<(Oid, Tid, Vec<u8>)>;

/// The transactions of a store, in the shape of `FsTxn`, and the blobs of
/// its records.
fn store_txns(store: &Store) -> (Vec<FsTxn>, Blobs) {
    let mut txns = Vec::new();
    let mut blobs = Vec::new();
    for txn in store.iterator(None, None).unwrap() {
        let txn = txn.unwrap();
        for record in &txn.records {
            if let Ok(mut reader) = store.open_blob(record.oid, txn.tid) {
                let mut bytes = Vec::new();
                reader.read_to_end(&mut bytes).unwrap();
                blobs.push((record.oid, txn.tid, bytes));
            }
        }
        txns.push(FsTxn {
            tid: txn.tid,
            status: b' ',
            user: txn.user,
            description: txn.description,
            extension: txn.extension,
            records: txn
                .records
                .into_iter()
                .map(|record| (record.oid, record.data))
                .collect(),
        });
    }
    (txns, blobs)
}

/// Sort the records of each transaction by oid.
fn by_oid(mut txns: Vec<FsTxn>) -> Vec<FsTxn> {
    for txn in &mut txns {
        txn.records.sort();
    }
    txns
}

/// Check that `data_fs` and `blob_dir` hold exactly the transactions and
/// the blobs of `store`.
fn assert_written(store: &Store, data_fs: &Path, blob_dir: &Path) {
    let written = read_data_fs(&std::fs::read(data_fs).unwrap());
    assert!(
        written.iter().all(|txn| txn.status == b' '),
        "a written transaction is complete and not packed"
    );
    let (txns, blobs) = store_txns(store);
    assert_eq!(by_oid(written), by_oid(txns));
    for (oid, tid, bytes) in blobs {
        let path = blob_file(blob_dir, oid, tid);
        assert_eq!(std::fs::read(&path).unwrap(), bytes, "{}", path.display());
    }
}

#[test]
fn data_fs_round_trips_through_zoe() {
    // The Python exit-path test gives its own `Data.fs`. Without it, the
    // test converts the fixture of the reader.
    let (input, input_blobs, work) = match std::env::var_os(EXIT_PATH_DIR) {
        Some(dir) => {
            let dir = PathBuf::from(dir);
            (dir.join("Data.fs"), dir.join("blobs"), dir)
        }
        None => (
            fixture().join("Data.fs"),
            fixture().join("blobs-bushy"),
            fresh_dir("round-trip"),
        ),
    };
    let first = Store::open(work.join("zoe")).unwrap();
    let read = from_datafs(&input, Some(&input_blobs), &first).unwrap();
    assert!(read.problems.is_empty(), "{read:?}");

    let out = work.join("out");
    std::fs::create_dir_all(&out).unwrap();
    let (data_fs, blob_dir) = (out.join("Data.fs"), out.join("blobs"));
    let report = to_datafs(&first, &data_fs, &blob_dir).unwrap();
    assert!(report.problems.is_empty(), "{report:?}");
    assert_eq!(report.transactions, read.transactions, "{report:?}");
    assert_eq!(report.copied, read.transactions, "{report:?}");
    assert_eq!(report.records, read.records, "{report:?}");
    assert_eq!(report.blobs, read.blobs, "{report:?}");
    assert_eq!(report.last_tid, read.last_tid, "{report:?}");
    assert_written(&first, &data_fs, &blob_dir);

    // The written file reads back into the same store.
    let second = Store::open(work.join("zoe-again")).unwrap();
    let again = from_datafs(&data_fs, Some(&blob_dir), &second).unwrap();
    assert!(again.problems.is_empty(), "{again:?}");
    assert_eq!(store_txns(&second), store_txns(&first));
}

/// Commit `writes` and blobs `blobs` in one transaction.
fn commit(
    store: &Store,
    meta: (&[u8], &[u8], &[u8]),
    writes: &[(Oid, Tid, Vec<u8>)],
    blobs: &[(Oid, Tid, &[u8])],
) -> Tid {
    let mut txn = store.tpc_begin(meta.0, meta.1, meta.2).unwrap();
    for (oid, serial, data) in writes {
        txn.store(*oid, *serial, data).unwrap();
    }
    for &(oid, serial, bytes) in blobs {
        let mut file = store.new_blob_file().unwrap();
        file.write_all(bytes).unwrap();
        txn.store_blob(oid, serial, BLOB_PICKLE, file).unwrap();
    }
    txn.vote().unwrap();
    txn.finish().unwrap()
}

const BLOB_PICKLE: &[u8] = b"\x80\x03cZODB.blob\nBlob\nq\x00.\x80\x03N.";

fn pickle(text: &str) -> Vec<u8> {
    let mut data = b"\x80\x03cpersistent.mapping\nPersistentMapping\nq\x00.\x80\x03X".to_vec();
    data.extend_from_slice(&(text.len() as u32).to_le_bytes());
    data.extend_from_slice(text.as_bytes());
    data.push(b'.');
    data
}

/// Write a store with commits: the root and other objects in several
/// revisions, a blob in two revisions, empty and binary metadata, and a
/// deletion. Return the tids.
fn native_store(store: &Store) -> Vec<Tid> {
    let (root, a, b, f) = (Oid(0), Oid(1), Oid(2), Oid(0x1234));
    let extension = b"\x80\x03}q\x00X\x04\x00\x00\x00noteq\x01X\x05\x00\x00\x00valueq\x02s.";
    let t1 = commit(
        store,
        (b"someone", b"create", extension),
        &[
            (root, Tid::ZERO, pickle("root 1")),
            (a, Tid::ZERO, pickle("a 1")),
            (b, Tid::ZERO, pickle("b 1")),
        ],
        &[(f, Tid::ZERO, b"first blob revision".as_slice())],
    );
    let t2 = commit(
        store,
        (b"", b"", b""),
        &[(a, t1, pickle("a 2")), (root, t1, pickle("root 2"))],
        &[(f, t1, b"second blob revision".as_slice())],
    );
    let mut delete = store
        .tpc_begin_restore(Tid(t2.0 + 1), b"someone", b"delete b", b"")
        .unwrap();
    delete.restore(b, None).unwrap();
    delete.vote().unwrap();
    let t3 = delete.finish().unwrap();
    vec![t1, t2, t3]
}

#[test]
fn a_store_written_by_commits_converts_to_data_fs() {
    let dir = fresh_dir("native");
    let store = Store::open(dir.join("zoe")).unwrap();
    let tids = native_store(&store);
    let (data_fs, blob_dir) = (dir.join("Data.fs"), dir.join("blobs"));

    let report = to_datafs(&store, &data_fs, &blob_dir).unwrap();
    assert!(report.problems.is_empty(), "{report:?}");
    assert_eq!(report.transactions, 3, "{report:?}");
    assert_eq!(report.records, 8, "{report:?}");
    assert_eq!(report.blobs, 2, "{report:?}");
    assert_eq!(report.last_tid, tids[2], "{report:?}");
    assert_written(&store, &data_fs, &blob_dir);

    // The deletion is a data record with a back pointer to no data.
    let written = read_data_fs(&std::fs::read(&data_fs).unwrap());
    assert_eq!(written[2].records, vec![(Oid(2), None)]);
}

#[test]
fn a_conversion_resumes_after_the_last_tid_of_the_file() {
    let dir = fresh_dir("resume");
    let store = Store::open(dir.join("zoe")).unwrap();
    let tids = native_store(&store);
    let (data_fs, blob_dir) = (dir.join("Data.fs"), dir.join("blobs"));
    to_datafs(&store, &data_fs, &blob_dir).unwrap();

    // Two more transactions, one with a blob.
    let (a, f) = (Oid(1), Oid(0x1234));
    let t4 = commit(
        &store,
        (b"someone", b"later", b""),
        &[(a, tids[1], pickle("a 3"))],
        &[(f, tids[1], b"third blob revision".as_slice())],
    );
    commit(
        &store,
        (b"someone", b"latest", b""),
        &[(a, t4, pickle("a 4"))],
        &[],
    );

    let report = to_datafs(&store, &data_fs, &blob_dir).unwrap();
    assert!(report.problems.is_empty(), "{report:?}");
    assert_eq!(report.transactions, 5, "{report:?}");
    assert_eq!(report.copied, 2, "{report:?}");
    assert_written(&store, &data_fs, &blob_dir);

    // The resumed file is the file of a conversion in one run.
    let whole = dir.join("whole");
    std::fs::create_dir_all(&whole).unwrap();
    to_datafs(&store, &whole.join("Data.fs"), &whole.join("blobs")).unwrap();
    assert!(
        std::fs::read(&data_fs).unwrap() == std::fs::read(whole.join("Data.fs")).unwrap(),
        "the resumed file differs from a file written in one run"
    );

    // A run on a file that has every transaction copies nothing.
    let report = to_datafs(&store, &data_fs, &blob_dir).unwrap();
    assert!(report.problems.is_empty(), "{report:?}");
    assert_eq!(report.copied, 0, "{report:?}");
}
