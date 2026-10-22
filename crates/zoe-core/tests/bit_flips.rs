//! Acceptance test of the bit-flip injection harness. A flipped bit in a
//! record frame, a transaction row, a blob record or a blob file makes
//! every read of it raise `CorruptRecord`, `CorruptTxn` or `CorruptBlob`,
//! and never return data. The stored frames are flipped through the test
//! hook `zoe_core::testing`. A flip in the raw database file may make the
//! store refuse to open, but a read never returns wrong data. Every flip
//! comes from a seeded generator, so a failure reproduces.

mod support;

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use redb::StorageBackend;
use support::{Rng, compressible, fresh_dir};
use zoe_core::testing::{self, Key};
use zoe_core::testing::{Meta, commit_as};
use zoe_core::vfs::MemVfs;
use zoe_core::{Codec, Compression, Oid, Options, Store, SystemClock, Tid, ZoeError};

/// The seed of the flips. A failure message names the byte and the bit.
const SEED: u64 = 0x9e37_79b9_7f4a_7c15;

/// The user, the description and the extension of each commit.
const META: Meta = (b"someone", b"bit flips", b"ext");

/// Open the store in `dir` with zstd at level 3. So `fill` writes records
/// of both codecs.
fn open_zstd(dir: impl AsRef<Path>) -> Store {
    Store::open_with(dir, zstd()).unwrap()
}

/// Open the store in `vfs` with zstd at level 3.
fn open_zstd_vfs(vfs: MemVfs) -> zoe_core::Result<Store> {
    Store::open_full(vfs, SystemClock, zstd())
}

/// The options of a store that compresses its records with zstd at
/// level 3.
fn zstd() -> Options {
    Options {
        compression: Compression::Zstd { level: 3 },
        ..Options::default()
    }
}

/// A small protocol-3 record.
fn small(title: &str) -> Vec<u8> {
    let mut data = b"\x80\x03cpersistent.mapping\nPersistentMapping\nq\x00.\x80\x03X".to_vec();
    data.extend_from_slice(&(title.len() as u32).to_le_bytes());
    data.extend_from_slice(title.as_bytes());
    data.push(b'.');
    data
}

const BLOB_PICKLE: &[u8] = b"\x80\x03cZODB.blob\nBlob\nq\x00.\x80\x03N.";

fn content(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 7 + 3) as u8).collect()
}

/// The records of a store: each record with its pickle, `None` for a
/// deletion.
struct Records {
    records: BTreeMap<(Oid, Tid), Option<Vec<u8>>>,
}

impl Records {
    /// Tell if the record is the newest record of its oid.
    fn is_current(&self, oid: Oid, tid: Tid) -> bool {
        self.records.keys().all(|&(o, t)| o != oid || t <= tid)
    }

    fn txns(&self) -> Vec<Tid> {
        let mut tids: Vec<Tid> = self.records.keys().map(|&(_, tid)| tid).collect();
        tids.sort();
        tids.dedup();
        tids
    }
}

/// Write three transactions: a small record in two revisions, a
/// compressed record, and a record and its deletion.
fn fill(store: &Store) -> Records {
    let (a, c, d) = (Oid(1), Oid(2), Oid(3));
    let t1 = commit_as(
        store,
        META,
        &[
            (a, Tid::ZERO, small("a first")),
            (c, Tid::ZERO, compressible()),
            (d, Tid::ZERO, small("d first")),
        ],
    );
    let t2 = commit_as(store, META, &[(a, t1, small("a second"))]);
    let mut delete = store
        .tpc_begin_restore(Tid(t2.0 + 1), b"someone", b"deletion", b"")
        .unwrap();
    delete.restore(d, None).unwrap();
    delete.vote().unwrap();
    let t3 = delete.finish().unwrap();

    assert_eq!(store.record_info(c, t1).unwrap().codec, Codec::Zstd);
    let records = BTreeMap::from([
        ((a, t1), Some(small("a first"))),
        ((a, t2), Some(small("a second"))),
        ((c, t1), Some(compressible())),
        ((d, t1), Some(small("d first"))),
        ((d, t3), None),
    ]);
    Records { records }
}

/// Return the frame with the bit `bit` of the byte `byte` flipped.
fn flip(frame: &[u8], byte: usize, bit: usize) -> Vec<u8> {
    let mut flipped = frame.to_vec();
    flipped[byte] ^= 1 << bit;
    flipped
}

/// Run every read that touches the record of `oid` at `tid`. Each result
/// is `Ok` when the read returned data.
fn read_record(
    store: &Store,
    records: &Records,
    oid: Oid,
    tid: Tid,
) -> Vec<(&'static str, Result<(), ZoeError>)> {
    let mut reads = vec![
        ("load_serial", store.load_serial(oid, tid).map(drop)),
        (
            "load_before",
            store.load_before(oid, Tid(tid.0 + 1)).map(drop),
        ),
        ("history", store.history(oid, 100).map(drop)),
        ("record_info", store.record_info(oid, tid).map(drop)),
        (
            "iterator",
            store
                .iterator(Some(tid), Some(tid))
                .and_then(|txns| txns.collect::<Result<Vec<_>, _>>())
                .map(drop),
        ),
    ];
    if records.is_current(oid, tid) {
        reads.push(("load", store.load(oid).map(drop)));
        if records.records[&(oid, tid)].is_some() {
            reads.push((
                "record_iternext",
                store.record_iternext(Some(oid)).map(drop),
            ));
        }
    }
    reads
}

fn assert_corrupt_record(store: &Store, records: &Records, oid: Oid, tid: Tid, flip: &str) {
    for (read, result) in read_record(store, records, oid, tid) {
        assert!(
            matches!(result, Err(ZoeError::CorruptRecord { oid: o, tid: t }) if o == oid && t == tid),
            "{flip} of the record of {oid:?} at {tid:?}: {read} gave {result:?}"
        );
    }
}

/// Check that every record reads back as it was committed.
fn assert_intact(store: &Store, records: &Records) {
    for (&(oid, tid), data) in &records.records {
        let loaded = store.load_before(oid, Tid(tid.0 + 1));
        match data {
            Some(data) => assert_eq!(&loaded.unwrap().unwrap().data, data, "{oid:?} at {tid:?}"),
            None => assert!(
                matches!(loaded, Err(ZoeError::NotFound { .. })),
                "{oid:?} at {tid:?}: {loaded:?}"
            ),
        }
    }
    let txns: Vec<Tid> = store
        .iterator(None, None)
        .unwrap()
        .map(|txn| txn.unwrap().tid)
        .collect();
    assert_eq!(txns, records.txns());
}

#[test]
fn every_flipped_bit_in_a_record_frame_raises_corrupt_record() {
    let dir = fresh_dir("records");
    let store = open_zstd(&dir);
    let records = fill(&store);
    let mut rng = Rng::new(SEED);

    for &(oid, tid) in records.records.keys() {
        let frame = testing::read_frame(&store, Key::Record(oid, tid)).unwrap();
        // Every bit of the deletion frame, and one seeded bit of every byte
        // of the other frames.
        let deletion = records.records[&(oid, tid)].is_none();
        for byte in 0..frame.len() {
            let bits: Vec<usize> = if deletion {
                (0..8).collect()
            } else {
                vec![rng.below(8)]
            };
            for bit in bits {
                testing::write_frame(&store, Key::Record(oid, tid), &flip(&frame, byte, bit))
                    .unwrap();
                assert_corrupt_record(
                    &store,
                    &records,
                    oid,
                    tid,
                    &format!("byte {byte} bit {bit}"),
                );
            }
        }
        testing::write_frame(&store, Key::Record(oid, tid), &frame).unwrap();
        assert_intact(&store, &records);
    }
}

#[test]
fn every_cut_or_grown_record_frame_raises_corrupt_record() {
    let dir = fresh_dir("cuts");
    let store = open_zstd(&dir);
    let records = fill(&store);

    for &(oid, tid) in records.records.keys() {
        let frame = testing::read_frame(&store, Key::Record(oid, tid)).unwrap();
        for len in 0..frame.len() {
            testing::write_frame(&store, Key::Record(oid, tid), &frame[..len]).unwrap();
            assert_corrupt_record(&store, &records, oid, tid, &format!("a cut to {len} bytes"));
        }
        let mut grown = frame.clone();
        grown.push(0);
        testing::write_frame(&store, Key::Record(oid, tid), &grown).unwrap();
        assert_corrupt_record(&store, &records, oid, tid, "an extra byte");
        testing::write_frame(&store, Key::Record(oid, tid), &frame).unwrap();
    }
    assert_intact(&store, &records);
}

#[test]
fn a_frame_under_another_key_raises_corrupt_record() {
    let dir = fresh_dir("moved");
    let store = open_zstd(&dir);
    let records = fill(&store);
    let keys: Vec<(Oid, Tid)> = records.records.keys().copied().collect();

    for &(from_oid, from_tid) in &keys {
        for &(oid, tid) in &keys {
            if (from_oid, from_tid) == (oid, tid) {
                continue;
            }
            let moved = testing::read_frame(&store, Key::Record(from_oid, from_tid)).unwrap();
            let original = testing::read_frame(&store, Key::Record(oid, tid)).unwrap();
            testing::write_frame(&store, Key::Record(oid, tid), &moved).unwrap();
            assert_corrupt_record(
                &store,
                &records,
                oid,
                tid,
                &format!("the frame of {from_oid:?} at {from_tid:?}"),
            );
            testing::write_frame(&store, Key::Record(oid, tid), &original).unwrap();
        }
    }
    assert_intact(&store, &records);
}

/// Run every read that touches the transaction row of `tid`.
fn read_txn(store: &Store, tid: Tid, oid: Oid) -> Vec<(&'static str, Result<(), ZoeError>)> {
    vec![
        (
            "iterator over the log",
            store
                .iterator(None, None)
                .and_then(|txns| txns.collect::<Result<Vec<_>, _>>())
                .map(drop),
        ),
        (
            "iterator over the transaction",
            store
                .iterator(Some(tid), Some(tid))
                .and_then(|txns| txns.collect::<Result<Vec<_>, _>>())
                .map(drop),
        ),
        ("history", store.history(oid, 100).map(drop)),
    ]
}

#[test]
fn every_flipped_bit_in_a_transaction_row_raises_corrupt_txn() {
    let dir = fresh_dir("txns");
    let store = open_zstd(&dir);
    let records = fill(&store);
    let mut rng = Rng::new(SEED);

    for tid in records.txns() {
        let (oid, _) = *records.records.keys().find(|(_, t)| *t == tid).unwrap();
        let row = testing::read_frame(&store, Key::Txn(tid)).unwrap();
        let mut damaged: Vec<(String, Vec<u8>)> = (0..row.len())
            .map(|byte| {
                let bit = rng.below(8);
                (format!("byte {byte} bit {bit}"), flip(&row, byte, bit))
            })
            .collect();
        damaged.extend(
            (0..row.len()).map(|len| (format!("a cut to {len} bytes"), row[..len].to_vec())),
        );
        for (what, bytes) in damaged {
            testing::write_frame(&store, Key::Txn(tid), &bytes).unwrap();
            for (read, result) in read_txn(&store, tid, oid) {
                assert!(
                    matches!(result, Err(ZoeError::CorruptTxn { tid: t }) if t == tid),
                    "{what} of the transaction row of {tid:?}: {read} gave {result:?}"
                );
            }
        }
        testing::write_frame(&store, Key::Txn(tid), &row).unwrap();
        assert_intact(&store, &records);
    }
}

/// Read the blob that `tid` wrote for `oid` to its end. A failure of the
/// reader gives the `ZoeError` inside its I/O error.
fn read_blob(store: &Store, oid: Oid, tid: Tid) -> Result<Vec<u8>, String> {
    let mut reader = store.open_blob(oid, tid).map_err(|e| format!("{e:?}"))?;
    let mut bytes = Vec::new();
    match reader.read_to_end(&mut bytes) {
        Ok(_) => Ok(bytes),
        Err(e) => match e
            .get_ref()
            .and_then(|inner| inner.downcast_ref::<ZoeError>())
        {
            Some(inner) => Err(format!("{inner:?}")),
            None => Err(format!("{e:?}")),
        },
    }
}

/// Check that every read of the blob of `oid` at `tid` raises
/// `CorruptBlob`.
fn assert_corrupt_blob(store: &Store, oid: Oid, tid: Tid, what: &str) {
    let verified = store.verify_blob(oid, tid);
    assert!(
        matches!(verified, Err(ZoeError::CorruptBlob { oid: o, tid: t }) if o == oid && t == tid),
        "{what}: verify_blob gave {verified:?}"
    );
    let expected = format!("{:?}", ZoeError::CorruptBlob { oid, tid });
    let read = read_blob(store, oid, tid);
    assert_eq!(read, Err(expected), "{what}: the read to the end");
}

/// Commit a small and a large blob, and return their oids and tid.
fn fill_blobs(store: &Store) -> ([(Oid, Vec<u8>); 2], Tid) {
    let blobs = [(Oid(1), content(256)), (Oid(2), content(100_000))];
    let mut txn = store.tpc_begin(b"", b"blobs", b"").unwrap();
    for (oid, bytes) in &blobs {
        let mut file = store.new_blob_file().unwrap();
        file.write_all(bytes).unwrap();
        txn.store_blob(*oid, Tid::ZERO, BLOB_PICKLE, file).unwrap();
    }
    txn.vote().unwrap();
    (blobs, txn.finish().unwrap())
}

fn rewrite(path: &Path, bytes: &[u8]) {
    use std::os::unix::fs::PermissionsExt;
    // A committed blob file can be read-only, as in ZODB.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).unwrap();
    std::fs::write(path, bytes).unwrap();
}

#[test]
fn every_flipped_bit_in_a_blob_file_raises_corrupt_blob() {
    let dir = fresh_dir("blob-files");
    let store = Store::open(&dir).unwrap();
    let (blobs, tid) = fill_blobs(&store);
    let mut rng = Rng::new(SEED);

    for (oid, bytes) in &blobs {
        let path = store.blob_path(*oid, tid).unwrap();
        // Every bit of the small blob, and 256 seeded bits of the large one.
        let flips: Vec<(usize, usize)> = if bytes.len() <= 256 {
            (0..bytes.len())
                .flat_map(|byte| (0..8).map(move |bit| (byte, bit)))
                .collect()
        } else {
            (0..256)
                .map(|_| (rng.below(bytes.len()), rng.below(8)))
                .collect()
        };
        for (byte, bit) in flips {
            rewrite(&path, &flip(bytes, byte, bit));
            assert_corrupt_blob(
                &store,
                *oid,
                tid,
                &format!("byte {byte} bit {bit} of {oid:?}"),
            );
        }
        for len in [0, 1, bytes.len() / 2, bytes.len() - 1] {
            rewrite(&path, &bytes[..len]);
            assert_corrupt_blob(
                &store,
                *oid,
                tid,
                &format!("a cut to {len} bytes of {oid:?}"),
            );
        }
        let mut grown = bytes.clone();
        grown.push(0);
        rewrite(&path, &grown);
        assert_corrupt_blob(&store, *oid, tid, &format!("an extra byte in {oid:?}"));

        rewrite(&path, bytes);
        store.verify_blob(*oid, tid).unwrap();
        assert_eq!(read_blob(&store, *oid, tid), Ok(bytes.clone()));
    }
}

#[test]
fn every_flipped_bit_in_a_blob_record_raises_corrupt_blob() {
    let dir = fresh_dir("blob-records");
    let store = Store::open(&dir).unwrap();
    let (blobs, tid) = fill_blobs(&store);
    let mut rng = Rng::new(SEED);

    for (oid, bytes) in &blobs {
        let frame = testing::read_frame(&store, Key::Blob(*oid, tid)).unwrap();
        for byte in 0..frame.len() {
            let bit = rng.below(8);
            testing::write_frame(&store, Key::Blob(*oid, tid), &flip(&frame, byte, bit)).unwrap();
            assert_corrupt_blob(
                &store,
                *oid,
                tid,
                &format!("byte {byte} bit {bit} of the blob record of {oid:?}"),
            );
        }
        for len in 0..frame.len() {
            testing::write_frame(&store, Key::Blob(*oid, tid), &frame[..len]).unwrap();
            assert_corrupt_blob(
                &store,
                *oid,
                tid,
                &format!("a cut to {len} bytes of the blob record of {oid:?}"),
            );
        }
        testing::write_frame(&store, Key::Blob(*oid, tid), &frame).unwrap();
        assert_eq!(read_blob(&store, *oid, tid), Ok(bytes.clone()));
    }
}

/// The bytes of the database file of `vfs`.
fn database(vfs: &MemVfs) -> Vec<u8> {
    let file = vfs.file("zoe.redb");
    let mut bytes = vec![0; file.len().unwrap() as usize];
    file.read(0, &mut bytes).unwrap();
    bytes
}

/// A filesystem with the database file `bytes` and nothing else.
fn with_database(bytes: &[u8]) -> MemVfs {
    let vfs = MemVfs::new();
    let file = vfs.file("zoe.redb");
    file.set_len(bytes.len() as u64).unwrap();
    file.write(0, bytes).unwrap();
    vfs
}

/// Return every offset of `needle` in `haystack`.
fn find_all(haystack: &[u8], needle: &[u8]) -> Vec<usize> {
    haystack
        .windows(needle.len())
        .enumerate()
        .filter(|(_, window)| *window == needle)
        .map(|(at, _)| at)
        .collect()
}

/// Run `check` on another thread, and fail when it takes longer than ten
/// seconds, so that a read that never returns fails the test.
fn within_time<T: Send + 'static>(what: String, check: impl FnOnce() -> T + Send + 'static) -> T {
    let (done, wait) = mpsc::channel();
    thread::spawn(move || {
        let _ = done.send(check());
    });
    match wait.recv_timeout(Duration::from_secs(10)) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => panic!("{what}: a read did not return"),
        Err(mpsc::RecvTimeoutError::Disconnected) => panic!("{what}: the check failed"),
    }
}

#[test]
fn flips_in_the_database_file_never_return_wrong_data() {
    let vfs = MemVfs::new();
    let (records, frames) = {
        let store = open_zstd_vfs(vfs.clone()).unwrap();
        let records = fill(&store);
        let frames: Vec<((Oid, Tid), Vec<u8>)> = records
            .records
            .keys()
            .map(|&(oid, tid)| {
                (
                    (oid, tid),
                    testing::read_frame(&store, Key::Record(oid, tid)).unwrap(),
                )
            })
            .collect();
        (records, frames)
    };
    let file = database(&vfs);
    let records = std::sync::Arc::new(records);
    let mut rng = Rng::new(SEED);

    // A flip inside a stored frame, in every copy of the frame in the file:
    // the store opens and the read of that record raises `CorruptRecord`.
    for ((oid, tid), frame) in &frames {
        let places = find_all(&file, frame);
        assert!(
            !places.is_empty(),
            "the frame of {oid:?} at {tid:?} is not in the file"
        );
        for _ in 0..16 {
            let (byte, bit) = (rng.below(frame.len()), rng.below(8));
            let mut flipped = file.clone();
            for &at in &places {
                flipped[at + byte] ^= 1 << bit;
            }
            let what = format!("byte {byte} bit {bit} of the stored frame of {oid:?} at {tid:?}");
            let (oid, tid, records) = (*oid, *tid, std::sync::Arc::clone(&records));
            within_time(what.clone(), move || {
                let Ok(store) = open_zstd_vfs(with_database(&flipped)) else {
                    return;
                };
                assert_corrupt_record(&store, &records, oid, tid, &what);
            });
        }
    }

    // Flips anywhere in the data of the file: the store refuses to open,
    // or each read returns an error or the committed data.
    let used: Vec<usize> = (0..file.len()).filter(|&at| file[at] != 0).collect();
    for _ in 0..128 {
        let (at, bit) = (used[rng.below(used.len())], rng.below(8));
        let flipped = flip(&file, at, bit);
        let what = format!("byte {at} bit {bit} of the database file");
        let records = std::sync::Arc::clone(&records);
        let wrong = within_time(what.clone(), move || {
            let mut wrong = Vec::new();
            // A panic is a refusal: it returns no data.
            let _ = panic::catch_unwind(AssertUnwindSafe(|| {
                let Ok(store) = open_zstd_vfs(with_database(&flipped)) else {
                    return;
                };
                for (&(oid, tid), data) in &records.records {
                    if let (Ok(read), Some(data)) = (store.load_serial(oid, tid), data)
                        && &read != data
                    {
                        wrong.push(format!("load_serial of {oid:?} at {tid:?}"));
                    }
                    if let Ok((read, serial)) = store.load(oid)
                        && records.records.get(&(oid, serial)) != Some(&Some(read))
                    {
                        wrong.push(format!("load of {oid:?}"));
                    }
                }
            }));
            wrong
        });
        assert!(wrong.is_empty(), "{what}: wrong data from {wrong:?}");
    }
}
