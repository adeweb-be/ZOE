//! The tests of the redb backend.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

use redb::StorageBackend;
use redb::backends::InMemoryBackend;

use super::*;
use crate::backend::{Backend, BlobRecord, TxnStatus};
use crate::store::Store;
use crate::testing::compressible;
use crate::vfs::MemVfs;

impl RedbBackend {
    /// Return the database of a store that is open for writes.
    fn database(&self) -> &Database {
        match &self.db {
            Db::ReadWrite(db) => db,
            Db::ReadOnly(_) => panic!("the store is read-only"),
        }
    }
}

/// Return an empty store in memory.
fn empty() -> RedbBackend {
    let db = redb::Builder::new()
        .create_with_backend(InMemoryBackend::new())
        .unwrap();
    RedbBackend::initialise(&db).unwrap();
    RedbBackend::open(db).unwrap()
}

/// Return the record frame of the row `key` with the codec byte `codec` and a
/// valid checksum.
fn frame_with_codec(key: &[u8], codec: u8, payload: &[u8]) -> Vec<u8> {
    format::encode_stored_frame(KeySpace::Records, key, codec, payload)
}

/// Store the record frame of `oid` at `tid` with the codec byte `codec`
/// and the payload `payload`.
fn insert_with_codec(backend: &RedbBackend, oid: Oid, tid: Tid, codec: u8, payload: &[u8]) {
    let key = format::record_key(oid, tid);
    let txn = backend.database().begin_write().unwrap();
    txn.open_table(RECORDS)
        .unwrap()
        .insert(&key, frame_with_codec(&key, codec, payload).as_slice())
        .unwrap();
    txn.commit().unwrap();
}

#[test]
fn a_record_of_a_newer_format_is_a_format_error() {
    let backend = empty();
    let (oid, tid) = (Oid(1), Tid(5));
    insert_with_codec(&backend, oid, tid, 2, b"pickle");

    let error = backend.load_before(oid, Tid::MAX);
    assert!(
        matches!(&error, Err(ZoeError::Format(message)) if message.contains("header byte 2")),
        "{error:?}"
    );
    let error = backend.record_info(oid, tid);
    assert!(matches!(error, Err(ZoeError::Format(_))), "{error:?}");
}

#[test]
fn a_zstd_record_that_does_not_decode_is_corrupt() {
    let backend = empty();
    let (oid, tid) = (Oid(1), Tid(5));
    insert_with_codec(&backend, oid, tid, format::Codec::Zstd.byte(), b"pickle");

    let error = backend.load_before(oid, Tid::MAX);
    assert!(
        matches!(
            error,
            Err(ZoeError::CorruptRecord {
                oid: Oid(1),
                tid: Tid(5)
            })
        ),
        "{error:?}"
    );
    let error = backend.load_serial(oid, tid);
    assert!(
        matches!(error, Err(ZoeError::CorruptRecord { .. })),
        "{error:?}"
    );
}

/// Commit the record of `oid` at `tid` with `data`.
fn commit_record(backend: &RedbBackend, oid: Oid, tid: Tid, data: &[u8]) {
    let mut write = backend.begin_write().unwrap();
    write.put_record(oid, tid, Tid::ZERO, data).unwrap();
    write.commit().unwrap();
}

#[test]
fn current_serials_reads_each_oid_as_load_before_does() {
    let backend = empty();
    commit_record(&backend, Oid(1), Tid(10), b"a");
    commit_record(&backend, Oid(1), Tid(20), &compressible());
    commit_record(&backend, Oid(3), Tid(30), b"c");
    let oids = [Oid(3), Oid(2), Oid(1)];
    let current = backend.current_serials(&mut oids.into_iter()).unwrap();
    let expected: Vec<_> = oids
        .iter()
        .map(|&oid| backend.load_before(oid, Tid::MAX).unwrap().map(|r| r.tid))
        .collect();
    assert_eq!(current, expected);
    assert_eq!(current, [Some(Tid(30)), None, Some(Tid(20))]);
}

#[test]
fn put_records_writes_each_record_and_its_index_key() {
    let backend = empty();
    let mut write = backend.begin_write().unwrap();
    let new = [(Oid(1), Tid::ZERO, b"a"), (Oid(2), Tid(5), b"b")];
    let mut records = new
        .iter()
        .map(|&(oid, prev, data)| Ok((oid, prev, Cow::Borrowed(&data[..]))));
    write.put_records(Tid(10), &mut records).unwrap();
    write.commit().unwrap();
    for (oid, prev, data) in new {
        let record = backend.load_before(oid, Tid::MAX).unwrap().unwrap();
        assert_eq!(
            (record.tid, record.prev, &record.data[..]),
            (Tid(10), prev, &data[..])
        );
    }
    let oids = backend.txn_oids(Tid(10), Tid(11)).unwrap();
    assert_eq!(oids[&Tid(10)], vec![Oid(1), Oid(2)]);
}

#[test]
fn a_put_counts_its_oid_once_whatever_its_previous_serial() {
    let backend = empty();
    commit_record(&backend, Oid(1), Tid(5), b"a");
    commit_record(&backend, Oid(1), Tid(6), b"b");
    commit_record(&backend, Oid(4), Tid(7), b"c");
    assert_eq!(backend.object_count(), Some(2));
    assert_eq!(backend.next_oid().unwrap(), Oid(5));
    let puts = [
        // The record of the previous serial exists.
        (Oid(1), Tid(6)),
        // No record has the previous serial, but the oid has records.
        (Oid(4), Tid(3)),
        // Oids before the next oid, without a record.
        (Oid(2), Tid(9)),
        (Oid(3), Tid::ZERO),
        // Oids at and after the next oid.
        (Oid(5), Tid::ZERO),
        (Oid(9), Tid(2)),
    ];
    let mut write = backend.begin_write().unwrap();
    let mut records = puts
        .iter()
        .map(|&(oid, prev)| Ok((oid, prev, Cow::Borrowed(&b"x"[..]))));
    write.put_records(Tid(10), &mut records).unwrap();
    // A second put of a new oid in the same write counts it once.
    write.put_record(Oid(5), Tid(11), Tid(10), b"y").unwrap();
    write.commit().unwrap();
    assert_eq!(backend.object_count(), Some(6));
    assert_eq!(backend.next_oid().unwrap(), Oid(10));
}

#[test]
fn a_next_oid_that_does_not_decode_keeps_the_count_exact() {
    let backend = empty();
    commit_record(&backend, Oid(1), Tid(5), b"a");
    let txn = backend.database().begin_write().unwrap();
    txn.open_table(META)
        .unwrap()
        .insert(META_NEXT_OID, &b"bad"[..])
        .unwrap();
    txn.commit().unwrap();

    let mut write = backend.begin_write().unwrap();
    write.put_record(Oid(1), Tid(6), Tid::ZERO, b"b").unwrap();
    write.put_record(Oid(2), Tid(6), Tid::ZERO, b"c").unwrap();
    write.commit().unwrap();
    assert_eq!(backend.object_count(), Some(2));
}

#[test]
fn every_read_path_decodes_a_compressed_record() {
    let backend = empty();
    let (oid, tid) = (Oid(1), Tid(5));
    let data = compressible();
    commit_record(&backend, oid, tid, &data);

    let info = backend.record_info(oid, tid).unwrap().unwrap();
    assert_eq!(info.codec, format::Codec::Zstd);
    assert_eq!(info.data_len, data.len() as u64);
    assert!(info.stored_len < info.data_len, "{info:?}");
    let record = backend.load_before(oid, Tid::MAX).unwrap().unwrap();
    assert_eq!(record.data, data);
    assert_eq!(backend.load_serial(oid, tid).unwrap(), Some(data.clone()));
    let history = backend.history(oid, 10).unwrap();
    assert_eq!(history[0].size, data.len() as u64);
    assert_eq!(backend.record_info(oid, Tid(6)).unwrap(), None);
}

#[test]
fn a_backend_without_compression_writes_plain_records() {
    let backend = empty().with_compression(Compression::None);
    let (oid, tid) = (Oid(1), Tid(5));
    let data = compressible();
    commit_record(&backend, oid, tid, &data);

    let info = backend.record_info(oid, tid).unwrap().unwrap();
    assert_eq!(info.codec, format::Codec::None);
    assert_eq!(
        (info.stored_len, info.data_len),
        (data.len() as u64, data.len() as u64)
    );
}

#[test]
fn a_record_under_another_key_is_corrupt() {
    let backend = empty();
    let mut write = backend.begin_write().unwrap();
    write
        .put_record(Oid(1), Tid(5), Tid::ZERO, b"pickle")
        .unwrap();
    write.commit().unwrap();

    let txn = backend.database().begin_write().unwrap();
    {
        let mut records = txn.open_table(RECORDS).unwrap();
        let frame = records
            .get(&format::record_key(Oid(1), Tid(5)))
            .unwrap()
            .unwrap()
            .value()
            .to_vec();
        records
            .insert(&format::record_key(Oid(2), Tid(5)), frame.as_slice())
            .unwrap();
    }
    txn.commit().unwrap();

    let error = backend.load_before(Oid(2), Tid::MAX);
    assert!(
        matches!(
            error,
            Err(ZoeError::CorruptRecord {
                oid: Oid(2),
                tid: Tid(5)
            })
        ),
        "{error:?}"
    );
}

/// Commit an empty transaction row at `tid`.
fn commit_txn(backend: &RedbBackend, tid: Tid) {
    let mut write = backend.begin_write().unwrap();
    write
        .put_txn(&TxnMeta {
            tid,
            user: b"user".to_vec(),
            description: Vec::new(),
            extension: Vec::new(),
            oid_count: 0,
            status: TxnStatus::Committed,
        })
        .unwrap();
    write.commit().unwrap();
}

#[test]
fn the_last_transaction_row_raises_a_damaged_last_tid() {
    let backend = empty();
    commit_txn(&backend, Tid(500));
    let txn = backend.database().begin_write().unwrap();
    txn.open_table(META)
        .unwrap()
        .insert(META_LAST_TID, &format::encode_u64(7)[..])
        .unwrap();
    txn.commit().unwrap();

    let reopened = RedbBackend::open_db(backend.db).unwrap();
    assert_eq!(reopened.last_tid(), Tid(500));
}

/// A flipped bit in the key of the last row gives a tid that the store
/// never gave. The frame of the row fails the checksum under that key,
/// so the tid does not raise the last tid.
#[test]
fn a_last_row_under_a_damaged_key_does_not_raise_the_last_tid() {
    let backend = empty();
    commit_txn(&backend, Tid(500));
    let txn = backend.database().begin_write().unwrap();
    {
        let mut txns = txn.open_table(TXNS).unwrap();
        let frame = txns
            .get(&format::txn_key(Tid(500)))
            .unwrap()
            .unwrap()
            .value()
            .to_vec();
        let damaged = Tid(500 | 1 << 62);
        txns.insert(&format::txn_key(damaged), frame.as_slice())
            .unwrap();
    }
    txn.commit().unwrap();

    let reopened = RedbBackend::open_db(backend.db).unwrap();
    assert_eq!(reopened.last_tid(), Tid(500));
}

#[test]
fn a_corrupt_transaction_row_names_its_tid() {
    let backend = empty();
    let tid = Tid(5);
    commit_txn(&backend, tid);

    let txn = backend.database().begin_write().unwrap();
    {
        let mut txns = txn.open_table(TXNS).unwrap();
        let key = format::txn_key(tid);
        let mut frame = txns.get(&key).unwrap().unwrap().value().to_vec();
        *frame.last_mut().unwrap() ^= 1;
        txns.insert(&key, frame.as_slice()).unwrap();
    }
    txn.commit().unwrap();

    let rows: Vec<_> = backend.iter_txns(Tid::ZERO, Tid::MAX).collect();
    assert!(
        matches!(rows[..], [Err(ZoeError::CorruptTxn { tid: Tid(5) })]),
        "{rows:?}"
    );
}

#[test]
fn a_replaced_row_keeps_its_branch_and_its_previous_serial() {
    let backend = empty();
    let tid = Tid(5);
    commit_txn(&backend, tid);
    // A row of another branch, with a previous serial.
    let key = format::txn_key(tid);
    let row = backend.iter_txns(tid, tid).next().unwrap().unwrap();
    let frame = format::encode_frame(KeySpace::Txns, &key, 7, Tid(3), &format::encode_txn(&row));
    let txn = backend.database().begin_write().unwrap();
    txn.open_table(TXNS)
        .unwrap()
        .insert(&key, frame.as_slice())
        .unwrap();
    txn.commit().unwrap();

    let mut write = backend.begin_write().unwrap();
    write
        .replace_txn(&TxnMeta {
            oid_count: 4,
            ..row.clone()
        })
        .unwrap();
    write.commit().unwrap();

    let txn = backend.db.begin_read().unwrap();
    let txns = txn.open_table(TXNS).unwrap();
    let stored = txns.get(&key).unwrap().unwrap();
    let frame = format::decode_frame(KeySpace::Txns, &key, stored.value()).unwrap();
    assert_eq!((frame.branch, frame.prev), (7, Tid(3)));
    let replaced = format::decode_txn(tid, &frame.data).unwrap();
    assert_eq!(replaced.oid_count, 4);
    assert_eq!(replaced.user, row.user);
    assert_eq!(backend.last_tid(), tid);
}

#[test]
fn a_row_with_typed_field_slots_is_not_replaced() {
    let backend = empty();
    let tid = Tid(5);
    commit_txn(&backend, tid);
    let key = format::txn_key(tid);
    let row = backend.iter_txns(tid, tid).next().unwrap().unwrap();
    // The frame of a newer build: two bytes of typed-field slots.
    let payload = format::encode_txn(&row);
    let mut frame = format::encode_frame(
        KeySpace::Txns,
        &key,
        format::MAIN_BRANCH,
        Tid::ZERO,
        &payload,
    );
    frame[20..22].copy_from_slice(&2u16.to_le_bytes());
    frame.splice(format::HEADER_LEN..format::HEADER_LEN, [1, 2]);
    format::seal(KeySpace::Txns, &key, &mut frame);
    let txn = backend.database().begin_write().unwrap();
    txn.open_table(TXNS)
        .unwrap()
        .insert(&key, frame.as_slice())
        .unwrap();
    txn.commit().unwrap();

    let mut write = backend.begin_write().unwrap();
    let error = write.replace_txn(&TxnMeta {
        oid_count: 4,
        ..row
    });
    assert!(matches!(error, Err(ZoeError::Format(_))), "{error:?}");
}

#[test]
fn txn_oids_groups_the_oids_of_each_transaction() {
    let backend = empty();
    let mut write = backend.begin_write().unwrap();
    for (oid, tid) in [(3, 5), (1, 9), (1, 5), (2, 7), (2, 5)] {
        write
            .put_record(Oid(oid), Tid(tid), Tid::ZERO, b"pickle")
            .unwrap();
    }
    write.commit().unwrap();

    let oids = backend.txn_oids(Tid(5), Tid(7)).unwrap();
    let expected = BTreeMap::from([
        (Tid(5), vec![Oid(1), Oid(2), Oid(3)]),
        (Tid(7), vec![Oid(2)]),
    ]);
    assert_eq!(oids, expected);
    assert!(backend.txn_oids(Tid(10), Tid::MAX).unwrap().is_empty());
}

fn blob() -> BlobRecord {
    BlobRecord {
        hash: *blake3::hash(b"blob").as_bytes(),
        size: 4,
    }
}

#[test]
fn a_blob_record_round_trips() {
    let backend = empty();
    let mut write = backend.begin_write().unwrap();
    write.put_blob(Oid(1), Tid(5), &blob()).unwrap();
    write.commit().unwrap();

    assert_eq!(backend.load_blob(Oid(1), Tid(5)).unwrap(), Some(blob()));
    assert_eq!(backend.load_blob(Oid(1), Tid(6)).unwrap(), None);
    assert_eq!(backend.next_oid().unwrap(), Oid(2));
}

/// A blob record stays uncompressed when the records of the store are
/// compressed. Its payload of zeros would shrink with zstd.
#[test]
fn a_blob_record_is_never_compressed() {
    let backend = empty();
    let zeros = BlobRecord {
        hash: [0; 32],
        size: 0,
    };
    let mut write = backend.begin_write().unwrap();
    write
        .put_record(Oid(1), Tid(5), Tid::ZERO, &[0; 4096])
        .unwrap();
    write.put_blob(Oid(1), Tid(5), &zeros).unwrap();
    write.commit().unwrap();

    let txn = backend.db.begin_read().unwrap();
    let key = format::record_key(Oid(1), Tid(5));
    let record = txn.open_table(RECORDS).unwrap();
    let record = record.get(&key).unwrap().unwrap();
    assert_eq!(record.value()[2], format::CODEC_ZSTD);
    let blobs = txn.open_table(BLOBS).unwrap();
    let blob = blobs.get(&key).unwrap().unwrap();
    assert_eq!(blob.value()[2], format::CODEC_NONE);
    assert_eq!(backend.load_blob(Oid(1), Tid(5)).unwrap(), Some(zeros));
}

#[test]
fn a_store_without_the_blobs_table_has_no_blob_record() {
    let db = redb::Builder::new()
        .create_with_backend(InMemoryBackend::new())
        .unwrap();
    let txn = db.begin_write().unwrap();
    txn.open_table(RECORDS).unwrap();
    txn.open_table(TXN_OIDS).unwrap();
    txn.open_table(TXNS).unwrap();
    {
        let mut meta = txn.open_table(META).unwrap();
        meta.insert(META_FORMAT, format::FORMAT_NAME).unwrap();
        meta.insert(META_LAST_TID, &format::encode_u64(0)[..])
            .unwrap();
        meta.insert(META_NEXT_OID, &format::encode_u64(1)[..])
            .unwrap();
        meta.insert(META_OBJECT_COUNT, &format::encode_u64(0)[..])
            .unwrap();
    }
    txn.commit().unwrap();

    let backend = RedbBackend::open(db).unwrap();
    assert_eq!(backend.load_blob(Oid(1), Tid(5)).unwrap(), None);
    let mut write = backend.begin_write().unwrap();
    write.put_blob(Oid(1), Tid(5), &blob()).unwrap();
    write.commit().unwrap();
    assert_eq!(backend.load_blob(Oid(1), Tid(5)).unwrap(), Some(blob()));
}

#[test]
fn a_damaged_blob_record_is_a_corrupt_blob() {
    let backend = empty();
    let mut write = backend.begin_write().unwrap();
    write.put_blob(Oid(1), Tid(5), &blob()).unwrap();
    write.commit().unwrap();

    let txn = backend.database().begin_write().unwrap();
    {
        let mut blobs = txn.open_table(BLOBS).unwrap();
        let key = format::record_key(Oid(1), Tid(5));
        let mut frame = blobs.get(&key).unwrap().unwrap().value().to_vec();
        *frame.last_mut().unwrap() ^= 1;
        blobs.insert(&key, frame.as_slice()).unwrap();
    }
    txn.commit().unwrap();

    let error = backend.load_blob(Oid(1), Tid(5));
    assert!(
        matches!(
            error,
            Err(ZoeError::CorruptBlob {
                oid: Oid(1),
                tid: Tid(5)
            })
        ),
        "{error:?}"
    );
}

/// The number of records of oid 1 that `many_records` writes, and of
/// rows that `many_rows` writes.
const LAST: u64 = 120;

/// Return the bytes of the database file `db` of `vfs`.
fn file_of(vfs: &MemVfs) -> Vec<u8> {
    let file = vfs.file("db");
    let mut bytes = vec![0; file.len().unwrap() as usize];
    file.read(0, &mut bytes).unwrap();
    bytes
}

/// Return an empty backend in the database file `db` of `vfs`. It
/// writes the data as it is.
fn backend_in(vfs: &MemVfs) -> RedbBackend {
    let db = redb::Builder::new()
        .create_with_backend(vfs.file("db"))
        .unwrap();
    RedbBackend::initialise(&db).unwrap();
    RedbBackend::open(db)
        .unwrap()
        .with_compression(Compression::None)
}

/// Return the offsets of the copies of each key of `keys` that is in
/// `file` more than once. Such a key is in a leaf page and in a branch
/// page, or in an old copy of a page. The keys have the same length and
/// the same first 6 bytes.
fn copies(file: &[u8], keys: &[Vec<u8>]) -> Vec<usize> {
    let (len, prefix) = (keys[0].len(), &keys[0][..6]);
    let mut places: BTreeMap<&[u8], Vec<usize>> = BTreeMap::new();
    for at in 0..file.len() - len {
        let bytes = &file[at..at + len];
        if bytes.starts_with(prefix) && keys.iter().any(|key| key[..] == *bytes) {
            places.entry(bytes).or_default().push(at);
        }
    }
    places
        .into_values()
        .filter(|places| places.len() > 1)
        .flatten()
        .collect()
}

/// Open a copy of `file` with `fill` written over `range`, and give the
/// backend to `check`. redb can panic on a damaged page. A panic gives
/// no data.
fn with_damage(
    file: &[u8],
    range: std::ops::Range<usize>,
    fill: u8,
    check: impl FnOnce(&RedbBackend),
) {
    let mut damaged = file.to_vec();
    damaged[range].fill(fill);
    let copy = MemVfs::new();
    let target = copy.file("db");
    target.set_len(damaged.len() as u64).unwrap();
    target.write(0, &damaged).unwrap();
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let db = redb::Builder::new().create_with_backend(target).ok()?;
        check(&RedbBackend::open(db).ok()?);
        Some(())
    }));
}

/// What the reads of damaged databases gave.
#[derive(Default)]
struct Scans {
    /// The keys outside a scan that a read gave.
    wrong: Vec<String>,
    /// The keys at or after the end of a backward scan that a check
    /// caught.
    late: usize,
    /// The keys before the start of a forward scan that a check caught.
    early: usize,
}

/// Write the records of oid 1 at the tids 1 to `LAST`, and one record of
/// oid 2, in the database file `db` of `vfs`. The records are large
/// enough that `records` has branch pages.
fn many_records(vfs: &MemVfs) {
    let backend = backend_in(vfs);
    let mut write = backend.begin_write().unwrap();
    for tid in 1..=LAST {
        let pickle = [tid as u8; 200];
        write
            .put_record(Oid(1), Tid(tid), Tid(tid - 1), &pickle)
            .unwrap();
    }
    write
        .put_record(Oid(2), Tid(1), Tid::ZERO, b"pickle")
        .unwrap();
    write.commit().unwrap();
}

/// Run the scans of `records` for oid 1 at each tid. Record each key
/// outside its scan that a read gave, and count the ones that a check
/// caught.
fn check_records(backend: &RedbBackend, scans: &mut Scans) {
    for t in 1..=LAST + 1 {
        let tid = Tid(t);
        match backend.load_before(Oid(1), tid) {
            Ok(Some(record)) if record.tid >= tid => {
                scans
                    .wrong
                    .push(format!("load_before {tid:?}: {:?}", record.tid));
            }
            Err(ZoeError::CorruptRecord {
                oid: Oid(1),
                tid: at,
            }) if at >= tid => {
                scans.late += 1;
            }
            _ => {}
        }
        match backend.load_before_next(Oid(1), tid) {
            Ok((before, next)) => {
                if before.as_ref().is_some_and(|r| r.tid >= tid)
                    || next.is_some_and(|next| next < tid)
                {
                    let before = before.map(|r| r.tid);
                    scans
                        .wrong
                        .push(format!("load_before_next {tid:?}: {before:?}, {next:?}"));
                }
            }
            Err(ZoeError::CorruptRecord {
                oid: Oid(1),
                tid: at,
            }) if at < tid => {
                scans.early += 1;
            }
            Err(_) => {}
        }
    }
    if let Ok(history) = backend.history(Oid(1), usize::MAX) {
        let tids: Vec<u64> = history.iter().map(|meta| meta.tid.0).collect();
        let newest_first = tids.windows(2).all(|pair| pair[0] > pair[1]);
        if !newest_first || tids.iter().any(|&tid| !(1..=LAST).contains(&tid)) {
            scans.wrong.push(format!("history: {tids:?}"));
        }
    }
    if let Ok(Some((oid, record))) = backend.current_from(Oid(1)) {
        let last = if oid == Oid(1) { LAST } else { 1 };
        if !(1..=last).contains(&record.tid.0) {
            scans
                .wrong
                .push(format!("current_from: {oid:?} {:?}", record.tid));
        }
    }
    if let Ok(oids) = backend.txn_oids(Tid::ZERO, Tid::MAX)
        && oids
            .values()
            .any(|oids| oids.windows(2).any(|pair| pair[0] >= pair[1]))
    {
        scans.wrong.push(format!("txn_oids: {oids:?}"));
    }
    for from in [Oid(1), Oid(2)] {
        if let Ok(keys) = backend.record_keys(from, usize::MAX)
            && (keys.windows(2).any(|pair| pair[0] >= pair[1])
                || keys.iter().any(|&(oid, _)| oid < from))
        {
            scans.wrong.push(format!("record_keys {from:?}: {keys:?}"));
        }
    }
}

/// A key of a branch page bounds the keys of a child page. A damaged
/// one moves the seek of a scan into the wrong child, and redb can then
/// give a key outside the scan. Every read of a record gives a key
/// inside its scan, or fails.
#[test]
fn a_damaged_branch_key_never_gives_a_record_outside_the_scan() {
    let vfs = MemVfs::new();
    many_records(&vfs);
    let file = file_of(&vfs);
    let keys: Vec<Vec<u8>> = (1..=LAST)
        .map(|tid| format::record_key(Oid(1), Tid(tid)).to_vec())
        .collect();
    let mut scans = Scans::default();
    for at in copies(&file, &keys) {
        // The lowest and the highest tid make the key bound too few or
        // too many keys.
        for fill in [0, 0xff] {
            with_damage(&file, at + 8..at + 16, fill, |backend| {
                check_records(backend, &mut scans);
            });
        }
    }
    assert!(scans.wrong.is_empty(), "{:?}", scans.wrong);
    // Without the checks of the scans, these reads give the keys
    // outside them.
    assert!(scans.late > 0, "no backward scan went past its end");
    assert!(scans.early > 0, "no forward scan started before its start");
}

/// The tid of the row `n` of `many_rows`. The high bytes make its key
/// easy to find in the file.
fn row_tid(n: u64) -> Tid {
    Tid(0x5a5a_0000_0000_0000 | n)
}

/// Write the rows `row_tid(1)` to `row_tid(LAST)` in the database file
/// `db` of `vfs`. The rows are large enough that `txns` has branch
/// pages.
fn many_rows(vfs: &MemVfs) {
    let backend = backend_in(vfs);
    let mut write = backend.begin_write().unwrap();
    for n in 1..=LAST {
        write
            .put_txn(&TxnMeta {
                tid: row_tid(n),
                user: b"user".to_vec(),
                description: vec![n as u8; 200],
                extension: Vec::new(),
                oid_count: 0,
                status: TxnStatus::Committed,
            })
            .unwrap();
    }
    write.commit().unwrap();
}

/// Run a scan of `txns` for each row. Record each row outside its scan
/// that a read gave, and count the ones that a check caught.
fn check_rows(backend: &RedbBackend, scans: &mut Scans) {
    for n in 1..=LAST {
        let tid = row_tid(n);
        for row in backend.iter_txns(tid, tid) {
            match row {
                Ok(row) if row.tid != tid => {
                    scans
                        .wrong
                        .push(format!("iter_txns {tid:?}: {:?}", row.tid));
                }
                Err(ZoeError::CorruptTxn { tid: at }) if at < tid => scans.early += 1,
                _ => {}
            }
        }
    }
    let tids: Vec<Tid> = backend
        .iter_txns(Tid::ZERO, Tid::MAX)
        .filter_map(|row| row.ok().map(|row| row.tid))
        .collect();
    if tids.windows(2).any(|pair| pair[0] >= pair[1]) {
        scans.wrong.push(format!("iter_txns: {tids:?}"));
    }
}

/// A damaged key of a branch page of `txns` never makes a scan give a
/// row outside it. The scan gives the rows inside it, or fails.
#[test]
fn a_damaged_branch_key_never_gives_a_row_outside_the_scan() {
    let vfs = MemVfs::new();
    many_rows(&vfs);
    let file = file_of(&vfs);
    let keys: Vec<Vec<u8>> = (1..=LAST)
        .map(|n| format::txn_key(row_tid(n)).to_vec())
        .collect();
    let mut scans = Scans::default();
    for at in copies(&file, &keys) {
        for fill in [0, 0xff] {
            with_damage(&file, at..at + 8, fill, |backend| {
                check_rows(backend, &mut scans);
            });
        }
    }
    assert!(scans.wrong.is_empty(), "{:?}", scans.wrong);
    // Without the check of the scan, these reads give the rows outside
    // it.
    assert!(scans.early > 0, "no forward scan started before its start");
}

/// The number of transactions that `many_index_keys` writes.
const INDEXED: u64 = 1000;

/// Write a transaction of one record at each tid `row_tid(n)`, for `n`
/// from 1 to `INDEXED`, in the database file `db` of `vfs`. The record
/// of `row_tid(n)` is of oid `n`. The index then has branch pages.
fn many_index_keys(vfs: &MemVfs) {
    let backend = backend_in(vfs);
    let mut write = backend.begin_write().unwrap();
    for n in 1..=INDEXED {
        let tid = row_tid(n);
        write.put_record(Oid(n), tid, Tid::ZERO, b"x").unwrap();
        write
            .put_txn(&TxnMeta {
                tid,
                user: Vec::new(),
                description: Vec::new(),
                extension: Vec::new(),
                oid_count: 1,
                status: TxnStatus::Committed,
            })
            .unwrap();
    }
    write.commit().unwrap();
}

/// Tell if `oids` is not in strictly ascending order.
fn unordered(oids: &[Oid]) -> bool {
    oids.windows(2).any(|pair| pair[0] >= pair[1])
}

/// Run the scans of the transaction index for each transaction of
/// `many_index_keys`. Record each key outside its scan, or out of
/// order, that a read gave, and count the ones that a check caught. A
/// damaged key in a leaf page changes the stored oid. That is not
/// outside the scan, and the read of its record catches it.
fn check_index(backend: &RedbBackend, scans: &mut Scans) {
    fn caught(at: Tid, tid: Tid, scans: &mut Scans) {
        match at.cmp(&tid) {
            std::cmp::Ordering::Less => scans.early += 1,
            std::cmp::Ordering::Greater => scans.late += 1,
            std::cmp::Ordering::Equal => {}
        }
    }
    for n in 1..=INDEXED {
        let tid = row_tid(n);
        match backend.txn_oids(tid, tid) {
            Ok(oids) => {
                if oids.keys().any(|&at| at != tid) || oids.values().any(|o| unordered(o)) {
                    scans.wrong.push(format!("txn_oids {tid:?}: {oids:?}"));
                }
            }
            Err(ZoeError::CorruptTxn { tid: at }) => caught(at, tid, scans),
            Err(_) => {}
        }
        match backend.indexed_tids(tid, tid, 2) {
            Ok(tids) => {
                if tids.iter().any(|&at| at != tid) {
                    scans.wrong.push(format!("indexed_tids {tid:?}: {tids:?}"));
                }
            }
            Err(ZoeError::CorruptTxn { tid: at }) => caught(at, tid, scans),
            Err(_) => {}
        }
        match backend.txn_records(tid) {
            Ok(Some((row, records))) => {
                let oids: Vec<Oid> = records.iter().map(|&(oid, _)| oid).collect();
                if row.tid != tid || unordered(&oids) {
                    scans.wrong.push(format!("txn_records {tid:?}: {oids:?}"));
                }
            }
            Err(ZoeError::CorruptTxn { tid: at }) => caught(at, tid, scans),
            _ => {}
        }
    }
    if let Ok(oids) = backend.txn_oids(Tid::ZERO, Tid::MAX)
        && oids.values().any(|oids| unordered(oids))
    {
        scans.wrong.push(format!("txn_oids: {oids:?}"));
    }
    if let Ok(tids) = backend.indexed_tids(Tid::ZERO, Tid::MAX, usize::MAX)
        && tids.windows(2).any(|pair| pair[0] >= pair[1])
    {
        scans.wrong.push(format!("indexed_tids: {tids:?}"));
    }
}

/// A damaged key of a branch page of the transaction index never
/// makes a scan give a key outside it, or a key twice. The scan gives
/// the keys inside it, or fails.
#[test]
fn a_damaged_branch_key_never_gives_an_index_key_outside_the_scan() {
    let vfs = MemVfs::new();
    many_index_keys(&vfs);
    let file = file_of(&vfs);
    let keys: Vec<Vec<u8>> = (1..=INDEXED)
        .map(|n| format::txn_oid_key(row_tid(n), Oid(n)).to_vec())
        .collect();
    let mut scans = Scans::default();
    for at in copies(&file, &keys) {
        // The tid half and the oid half of the key.
        for range in [at..at + 8, at + 8..at + 16] {
            for fill in [0, 0xff] {
                with_damage(&file, range.clone(), fill, |backend| {
                    check_index(backend, &mut scans);
                });
            }
        }
    }
    assert!(scans.wrong.is_empty(), "{:?}", scans.wrong);
    // Without the checks of the scans, these reads give the keys
    // outside them.
    assert!(
        scans.early + scans.late > 0,
        "no scan of the index went outside its range"
    );
}

#[test]
fn remove_record_refuses_a_record_without_its_index_key() {
    let backend = empty();
    commit_txn_of(&backend, Tid(5), &[1, 2]);
    backend.remove_index_key_for_test(Tid(5), Oid(1)).unwrap();

    let mut write = backend.begin_write().unwrap();
    let error = write.remove_record(Oid(1), Tid(5));
    assert!(
        matches!(error, Err(ZoeError::CorruptTxn { tid: Tid(5) })),
        "{error:?}"
    );
    drop(write);
    // The failed write removed nothing.
    assert_eq!(
        backend.load_serial(Oid(1), Tid(5)).unwrap(),
        Some(b"pickle".to_vec())
    );
}

#[test]
fn indexed_tids_gives_each_tid_once_up_to_the_limit() {
    let backend = empty();
    commit_txn_of(&backend, Tid(5), &[1, 2]);
    commit_txn_of(&backend, Tid(6), &[3]);
    commit_txn_of(&backend, Tid(8), &[1, 2, 3]);
    let tids = |from, to, limit| backend.indexed_tids(Tid(from), Tid(to), limit).unwrap();
    assert_eq!(tids(0, u64::MAX, 10), [Tid(5), Tid(6), Tid(8)]);
    assert_eq!(tids(0, u64::MAX, 2), [Tid(5), Tid(6)]);
    assert_eq!(tids(6, 8, 1), [Tid(6)]);
    assert_eq!(tids(7, 9, 10), [Tid(8)]);
    assert!(tids(7, 7, 10).is_empty());
    assert!(tids(9, 5, 10).is_empty());
    assert!(tids(0, u64::MAX, 0).is_empty());
    // The trait default gives the same tids.
    let oids = backend.txn_oids(Tid::ZERO, Tid::MAX).unwrap();
    let default: Vec<Tid> = oids.into_keys().take(2).collect();
    assert_eq!(default, tids(0, u64::MAX, 2));
}

#[test]
fn iter_txns_reads_its_rows_from_one_snapshot() {
    let backend = empty();
    commit_txn_of(&backend, Tid(5), &[1]);
    commit_txn_of(&backend, Tid(6), &[2]);
    let mut rows = backend.iter_txns(Tid::ZERO, Tid::MAX);
    assert_eq!(rows.next().unwrap().unwrap().tid, Tid(5));
    // A later commit is not in the snapshot of the iterator.
    commit_txn_of(&backend, Tid(7), &[3]);
    let rest: Vec<Tid> = rows.map(|row| row.unwrap().tid).collect();
    assert_eq!(rest, [Tid(6)]);
}

#[test]
fn txn_oids_of_an_empty_range_is_empty() {
    let backend = empty();
    commit_record(&backend, Oid(1), Tid(5), b"pickle");
    assert!(backend.txn_oids(Tid(6), Tid(5)).unwrap().is_empty());
    assert!(backend.txn_oids(Tid(6), Tid(9)).unwrap().is_empty());
    assert_eq!(
        backend.txn_oids(Tid(5), Tid(5)).unwrap(),
        BTreeMap::from([(Tid(5), vec![Oid(1)])])
    );
}

#[test]
fn put_record_writes_one_index_key() {
    let backend = empty();
    let mut write = backend.begin_write().unwrap();
    write
        .put_record(Oid(2), Tid(5), Tid::ZERO, b"pickle")
        .unwrap();
    write.put_record(Oid(1), Tid(5), Tid::ZERO, b"").unwrap();
    write.commit().unwrap();

    let txn = backend.db.begin_read().unwrap();
    let index = txn.open_table(TXN_OIDS).unwrap();
    let keys: Vec<[u8; 16]> = index
        .iter()
        .unwrap()
        .map(|entry| *entry.unwrap().0.value())
        .collect();
    // A deletion has its index key too.
    assert_eq!(
        keys,
        [
            format::txn_oid_key(Tid(5), Oid(1)),
            format::txn_oid_key(Tid(5), Oid(2))
        ]
    );
}

#[test]
fn txn_oids_reads_the_index_and_no_record() {
    let backend = empty();
    let mut write = backend.begin_write().unwrap();
    for oid in [1, 2, 3] {
        write
            .put_record(Oid(oid), Tid(5), Tid::ZERO, b"pickle")
            .unwrap();
    }
    write.commit().unwrap();
    let rest = BTreeMap::from([(Tid(5), vec![Oid(1), Oid(3)])]);

    // A record without its index key is not in the map.
    backend.remove_index_key_for_test(Tid(5), Oid(2)).unwrap();
    assert_eq!(
        backend.load_serial(Oid(2), Tid(5)).unwrap(),
        Some(b"pickle".to_vec())
    );
    assert_eq!(backend.txn_oids(Tid(5), Tid(5)).unwrap(), rest);

    // An index key without its record stays in the map.
    backend.remove_record_for_test(Oid(3), Tid(5)).unwrap();
    assert_eq!(backend.load_serial(Oid(3), Tid(5)).unwrap(), None);
    assert_eq!(backend.txn_oids(Tid(5), Tid(5)).unwrap(), rest);

    // A hook that finds nothing to remove fails.
    let error = backend.remove_index_key_for_test(Tid(5), Oid(2));
    assert!(
        matches!(error, Err(ZoeError::NotFound { oid: Oid(2) })),
        "{error:?}"
    );
    let error = backend.remove_record_for_test(Oid(3), Tid(5));
    assert!(
        matches!(error, Err(ZoeError::NotFound { oid: Oid(3) })),
        "{error:?}"
    );
}

/// Commit a transaction at `tid` that writes a record of each oid of
/// `oids`, with its row.
fn commit_txn_of(backend: &RedbBackend, tid: Tid, oids: &[u64]) {
    let mut write = backend.begin_write().unwrap();
    for &oid in oids {
        write
            .put_record(Oid(oid), tid, Tid::ZERO, b"pickle")
            .unwrap();
    }
    write
        .put_txn(&TxnMeta {
            tid,
            user: b"user".to_vec(),
            description: Vec::new(),
            extension: Vec::new(),
            oid_count: oids.len() as u64,
            status: TxnStatus::Committed,
        })
        .unwrap();
    write.commit().unwrap();
}

/// The oids of the records that `txn_records` gives for `tid`.
fn records_of(backend: &RedbBackend, tid: Tid) -> Result<Option<Vec<Oid>>> {
    Ok(backend
        .txn_records(tid)?
        .map(|(_, records)| records.into_iter().map(|(oid, _)| oid).collect()))
}

#[test]
fn txn_records_reads_the_index_keys_of_its_tid() {
    let backend = empty();
    commit_txn_of(&backend, Tid(5), &[3, 1, 2]);
    commit_txn_of(&backend, Tid(6), &[1]);
    assert_eq!(
        records_of(&backend, Tid(5)).unwrap(),
        Some(vec![Oid(1), Oid(2), Oid(3)])
    );
    // No row and no index key: a pack removed the transaction.
    assert_eq!(records_of(&backend, Tid(7)).unwrap(), None);

    // A record without its index key is left out. The caller finds the
    // records short of the oid count of the row.
    backend.remove_index_key_for_test(Tid(5), Oid(2)).unwrap();
    let (row, records) = backend.txn_records(Tid(5)).unwrap().unwrap();
    assert_eq!((row.oid_count, records.len()), (3, 2));

    // An index key without its record is corrupt.
    backend.remove_record_for_test(Oid(3), Tid(5)).unwrap();
    let error = records_of(&backend, Tid(5));
    assert!(
        matches!(error, Err(ZoeError::CorruptTxn { tid: Tid(5) })),
        "{error:?}"
    );

    // An index key of a tid that has no row is corrupt.
    let txn = backend.database().begin_write().unwrap();
    txn.open_table(TXN_OIDS)
        .unwrap()
        .insert(&format::txn_oid_key(Tid(7), Oid(1)), ())
        .unwrap();
    txn.commit().unwrap();
    let error = records_of(&backend, Tid(7));
    assert!(
        matches!(error, Err(ZoeError::CorruptTxn { tid: Tid(7) })),
        "{error:?}"
    );
    assert_eq!(records_of(&backend, Tid(6)).unwrap(), Some(vec![Oid(1)]));
}

#[test]
fn remove_record_removes_the_index_key() {
    let backend = empty();
    commit_txn_of(&backend, Tid(5), &[1, 2]);
    let mut write = backend.begin_write().unwrap();
    write.remove_record(Oid(1), Tid(5)).unwrap();
    write.commit().unwrap();

    assert_eq!(backend.load_serial(Oid(1), Tid(5)).unwrap(), None);
    assert_eq!(
        backend.txn_oids(Tid::ZERO, Tid::MAX).unwrap(),
        BTreeMap::from([(Tid(5), vec![Oid(2)])])
    );
}

#[test]
fn a_store_without_the_transaction_index_does_not_open() {
    let db = redb::Builder::new()
        .create_with_backend(InMemoryBackend::new())
        .unwrap();
    let txn = db.begin_write().unwrap();
    txn.open_table(RECORDS).unwrap();
    txn.open_table(TXNS).unwrap();
    {
        let mut meta = txn.open_table(META).unwrap();
        meta.insert(META_FORMAT, format::FORMAT_NAME).unwrap();
        meta.insert(META_LAST_TID, &format::encode_u64(0)[..])
            .unwrap();
        meta.insert(META_NEXT_OID, &format::encode_u64(1)[..])
            .unwrap();
        meta.insert(META_OBJECT_COUNT, &format::encode_u64(0)[..])
            .unwrap();
    }
    txn.commit().unwrap();

    let error = RedbBackend::open(db).err();
    assert!(
        matches!(&error, Some(ZoeError::Format(message)) if message.contains("transaction index")),
        "{error:?}"
    );
}

/// Check that the records and the transaction index of `backend` name
/// the same `(oid, tid)` pairs, and that each transaction row counts
/// the index keys of its tid. Return the tids of the rows.
fn assert_consistent(backend: &RedbBackend, image: &crate::vfs::CrashImage) -> Vec<Tid> {
    let txn = backend.db.begin_read().unwrap();
    let records: BTreeSet<(Tid, Oid)> = txn
        .open_table(RECORDS)
        .unwrap()
        .iter()
        .unwrap()
        .map(|entry| {
            let (oid, tid) = format::split_record_key(entry.unwrap().0.value());
            (tid, oid)
        })
        .collect();
    let index: BTreeSet<(Tid, Oid)> = txn
        .open_table(TXN_OIDS)
        .unwrap()
        .iter()
        .unwrap()
        .map(|entry| format::split_txn_oid_key(entry.unwrap().0.value()))
        .collect();
    assert_eq!(records, index, "{image:?}");

    let rows: Vec<TxnMeta> = backend
        .iter_txns(Tid::ZERO, Tid::MAX)
        .map(Result::unwrap)
        .collect();
    let mut counts: BTreeMap<Tid, u64> = BTreeMap::new();
    for (tid, _) in &index {
        *counts.entry(*tid).or_default() += 1;
    }
    for row in &rows {
        let count = counts.remove(&row.tid).unwrap_or(0);
        assert_eq!(count, row.oid_count, "{image:?}: {:?}", row.tid);
    }
    assert!(counts.is_empty(), "{image:?}: index keys without a row");
    rows.iter().map(|row| row.tid).collect()
}

#[test]
fn every_crash_image_keeps_the_index_and_the_records_together() {
    // The record of the filesystem starts after the store is created.
    let created = MemVfs::new();
    drop(Store::open_vfs(created.clone()).unwrap());
    let vfs = created.crash_images().pop().unwrap().vfs();
    let store = Store::open_vfs(vfs.clone()).unwrap();

    // A normal commit of three new objects.
    let oids: Vec<Oid> = (0..3).map(|_| store.new_oid().unwrap()).collect();
    let mut txn = store.tpc_begin(b"user", b"commit", b"").unwrap();
    for &oid in &oids {
        txn.store(oid, Tid::ZERO, b"pickle").unwrap();
    }
    txn.vote().unwrap();
    let committed = txn.finish().unwrap();
    vfs.mark("committed");

    // A restore of a new version, a deletion and a new object.
    let restored = Tid(committed.0 + 1);
    let mut txn = store
        .tpc_begin_restore(restored, b"user", b"restore", b"")
        .unwrap();
    txn.restore(oids[0], Some(b"copy")).unwrap();
    txn.restore(oids[1], None).unwrap();
    txn.restore(Oid(oids[2].0 + 1), Some(b"new")).unwrap();
    txn.vote().unwrap();
    txn.finish().unwrap();
    vfs.mark("restored");

    // A crash does not close the store, so the images end here.
    let images = vfs.crash_images();
    drop(store);
    assert!(images.len() > 1, "the simulation produced crash images");
    for image in &images {
        let db = redb::Builder::new()
            .create_with_backend(image.vfs().file("zoe.redb"))
            .unwrap_or_else(|e| panic!("{image:?} does not open: {e}"));
        let backend = RedbBackend::open(db).unwrap();
        let tids = assert_consistent(&backend, image);
        // A commit is whole or absent, and no acknowledged commit is
        // lost.
        let allowed: &[&[Tid]] = if image.is_after("restored") {
            &[&[committed, restored]]
        } else if image.is_after("committed") {
            &[&[committed], &[committed, restored]]
        } else {
            &[&[], &[committed]]
        };
        assert!(allowed.contains(&&tids[..]), "{image:?}: {tids:?}");
    }
}

/// Commit the row of the sequence `name` with `end`.
fn commit_sequence(backend: &RedbBackend, name: &str, end: u64) {
    let mut write = backend.begin_write().unwrap();
    write.put_sequence_end(name, end).unwrap();
    write.commit().unwrap();
}

#[test]
fn a_sequence_row_round_trips_and_changes_no_meta_row() {
    let backend = empty();
    assert_eq!(backend.sequence_end("intids").unwrap(), None);
    commit_sequence(&backend, "intids", 101);
    commit_sequence(&backend, "a name / with é", 7);
    assert_eq!(backend.sequence_end("intids").unwrap(), Some(101));
    assert_eq!(backend.sequence_end("a name / with é").unwrap(), Some(7));
    assert_eq!(backend.sequence_end("lexicon").unwrap(), None);
    assert_eq!(backend.last_tid(), Tid::ZERO);
    assert_eq!(backend.next_oid().unwrap(), Oid(1));

    let reopened = RedbBackend::open_db(backend.db).unwrap();
    assert_eq!(reopened.last_tid(), Tid::ZERO);
    assert_eq!(reopened.sequence_end("intids").unwrap(), Some(101));
}

#[test]
fn a_store_without_the_sequences_table_has_no_sequence_row() {
    let db = redb::Builder::new()
        .create_with_backend(InMemoryBackend::new())
        .unwrap();
    let txn = db.begin_write().unwrap();
    txn.open_table(RECORDS).unwrap();
    txn.open_table(TXN_OIDS).unwrap();
    txn.open_table(TXNS).unwrap();
    {
        let mut meta = txn.open_table(META).unwrap();
        meta.insert(META_FORMAT, format::FORMAT_NAME).unwrap();
        meta.insert(META_LAST_TID, &format::encode_u64(0)[..])
            .unwrap();
        meta.insert(META_NEXT_OID, &format::encode_u64(1)[..])
            .unwrap();
        meta.insert(META_OBJECT_COUNT, &format::encode_u64(0)[..])
            .unwrap();
    }
    txn.commit().unwrap();

    let backend = RedbBackend::open(db).unwrap();
    assert_eq!(backend.sequence_end("intids").unwrap(), None);
    assert!(backend.sequence_names().unwrap().is_empty());
    commit_sequence(&backend, "intids", 101);
    assert_eq!(backend.sequence_end("intids").unwrap(), Some(101));
    assert_eq!(backend.sequence_names().unwrap(), ["intids"]);
}

#[test]
fn the_sequence_names_come_in_key_order_without_a_read_of_the_rows() {
    let backend = empty();
    assert!(backend.sequence_names().unwrap().is_empty());
    for name in ["lexicon", "intids", "a name / with é"] {
        commit_sequence(&backend, name, 101);
    }
    let names = ["a name / with é", "intids", "lexicon"];
    assert_eq!(backend.sequence_names().unwrap(), names);

    // A row that does not pass its checksum is still listed. Only its
    // read fails.
    let mut flipped = format::encode_frame(
        KeySpace::Sequences,
        b"intids",
        format::MAIN_BRANCH,
        Tid::ZERO,
        &format::encode_sequence(101),
    );
    *flipped.last_mut().unwrap() ^= 1;
    insert_sequence_frame(&backend, "intids", &flipped);
    assert_eq!(backend.sequence_names().unwrap(), names);
    assert!(matches!(
        backend.sequence_end("intids"),
        Err(ZoeError::CorruptSequence { .. })
    ));
}

#[test]
fn a_sequence_name_that_is_not_utf8_fails_the_list_without_a_panic() {
    let vfs = MemVfs::new();
    let backend = backend_in(&vfs);
    for name in ["intids", "lexicon"] {
        commit_sequence(&backend, name, 101);
    }
    drop(backend);
    // Bit 0x80 makes the last byte of the name a lead byte without its
    // continuation bytes, so the name is not UTF-8.
    let mut file = file_of(&vfs);
    let mut flipped = 0;
    for at in 0..file.len() - b"lexicon".len() {
        if file[at..].starts_with(b"lexicon") {
            file[at + 6] ^= 0x80;
            flipped += 1;
        }
    }
    assert!(flipped > 0, "the name is not in the database file");
    let damaged = MemVfs::new();
    let target = damaged.file("db");
    target.set_len(file.len() as u64).unwrap();
    target.write(0, &file).unwrap();
    let db = redb::Builder::new().create_with_backend(target).unwrap();
    let backend = RedbBackend::open(db).unwrap();

    let error = backend.sequence_names().unwrap_err();
    assert!(matches!(error, ZoeError::Backend(_)), "{error}");
    let text = error.to_string();
    assert!(text.contains("corrupted"), "{text}");
    assert!(text.contains("after \"intids\""), "{text}");
}

/// Replace the stored row of the sequence `name` with `frame`.
fn insert_sequence_frame(backend: &RedbBackend, name: &str, frame: &[u8]) {
    let txn = backend.database().begin_write().unwrap();
    txn.open_table(SEQUENCES)
        .unwrap()
        .insert(name, frame)
        .unwrap();
    txn.commit().unwrap();
}

#[test]
fn a_damaged_sequence_row_is_a_corrupt_sequence() {
    let backend = empty();
    let good = format::encode_frame(
        KeySpace::Sequences,
        b"intids",
        format::MAIN_BRANCH,
        Tid::ZERO,
        &format::encode_sequence(101),
    );
    let mut flipped = good.clone();
    *flipped.last_mut().unwrap() ^= 1;
    let short = format::encode_frame(
        KeySpace::Sequences,
        b"intids",
        format::MAIN_BRANCH,
        Tid::ZERO,
        &[1; 7],
    );
    for frame in [flipped, short, good[..20].to_vec()] {
        insert_sequence_frame(&backend, "intids", &frame);
        let error = backend.sequence_end("intids");
        assert!(
            matches!(&error, Err(ZoeError::CorruptSequence { name }) if name == "intids"),
            "{error:?}"
        );
    }
    // A row stored under another name fails the checksum.
    insert_sequence_frame(&backend, "lexicon", &good);
    let error = backend.sequence_end("lexicon");
    assert!(
        matches!(&error, Err(ZoeError::CorruptSequence { name }) if name == "lexicon"),
        "{error:?}"
    );
}

#[test]
fn a_sequence_row_of_a_newer_format_is_a_format_error() {
    let backend = empty();
    let frame = format::encode_stored_frame(
        KeySpace::Sequences,
        b"intids",
        2,
        &format::encode_sequence(101),
    );
    insert_sequence_frame(&backend, "intids", &frame);
    let error = backend.sequence_end("intids");
    assert!(
        matches!(&error, Err(ZoeError::Format(message)) if message.contains("header byte 2")),
        "{error:?}"
    );
}
