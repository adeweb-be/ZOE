//! The bit-flip harness: a wider search than the acceptance test
//! `bit_flips`.
//!
//! It builds a store whose tables have branch pages. It flips bits in the
//! raw database file and runs every read path: `load`, `load_before`,
//! `load_serial`, `record_info`, `history`, the transaction iterator,
//! `record_iternext` and the blob reads. The store may refuse to open,
//! panic, or fail a read. A read must never return data that the store did
//! not commit. It also flips bits in a zstd payload and seals the frame
//! again, so that the decoder gets the damage past the checksum.
//!
//! Every flip comes from a seeded generator. A failure gives the seed.
//! `ZOE_FLIP_SEED` sets another seed, and `ZOE_FLIPS` sets the number of
//! flips of each run, for a longer search.
//!
//! A read that does not return in a minute fails the run. A longer
//! search can also meet a flip that aborts the process: redb panics while
//! it closes the damaged database, then panics again while it unwinds. The
//! default flips do neither.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::panic::{self, AssertUnwindSafe};
use std::sync::{Arc, Once, mpsc};
use std::thread;
use std::time::Duration;

use redb::StorageBackend;
use xxhash_rust::xxh3::Xxh3;
use zoe_core::testing::{self, Key};
use zoe_core::vfs::MemVfs;
use zoe_core::{Codec, Oid, Store, Tid, ZoeError};

/// The default seed of the flips.
const SEED: u64 = 0x2545_f491_4f6c_dd1d;

/// The default number of flips of each run.
const FLIPS: usize = 150;

/// The name of the threads that run the reads of a damaged store. A panic
/// there is a refusal, so the panic hook stays silent for them.
const CHECK_THREAD: &str = "flip check";

/// The tid of the first transaction. Its high byte is not 0, as with every
/// tid that the clock gives.
const FIRST_TID: u64 = 0x03f0_0000_0000_0000;

const BLOB_PICKLE: &[u8] = b"\x80\x03cZODB.blob\nBlob\nq\x00.\x80\x03N.";

/// A seeded xorshift generator.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        // A xorshift generator stays at 0 forever.
        Self(seed.max(1))
    }

    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % n as u64) as usize
    }
}

/// Return the seed of the flips: `ZOE_FLIP_SEED`, in hex with `0x` or in
/// decimal, or `SEED`.
fn seed() -> u64 {
    let Ok(text) = std::env::var("ZOE_FLIP_SEED") else {
        return SEED;
    };
    let parsed = match text.strip_prefix("0x") {
        Some(hex) => u64::from_str_radix(hex, 16),
        None => text.parse(),
    };
    parsed.unwrap_or_else(|_| panic!("ZOE_FLIP_SEED {text:?} is not a number"))
}

/// Return the number of flips of each run: `ZOE_FLIPS`, or `FLIPS`.
fn flips() -> usize {
    std::env::var("ZOE_FLIPS").map_or(FLIPS, |text| {
        text.parse()
            .unwrap_or_else(|_| panic!("ZOE_FLIPS {text:?} is not a number"))
    })
}

/// Keep the panics of the check threads off the output. Other threads keep
/// the default hook.
fn quiet_check_panics() {
    static QUIET: Once = Once::new();
    QUIET.call_once(|| {
        let default = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            if thread::current().name() != Some(CHECK_THREAD) {
                default(info);
            }
        }));
    });
}

/// Run `check` on a check thread, and fail when it takes longer than a
/// minute, so that a read that never returns fails the test. A panic in
/// `check` gives `None`. The limit leaves room for a slow build, such as
/// one under ThreadSanitizer.
fn within_time<T: Send + 'static>(
    what: &str,
    check: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    let (done, wait) = mpsc::channel();
    thread::Builder::new()
        .name(CHECK_THREAD.to_owned())
        .spawn(move || {
            let _ = done.send(check());
        })
        .unwrap();
    match wait.recv_timeout(Duration::from_secs(60)) {
        Ok(result) => Some(result),
        Err(mpsc::RecvTimeoutError::Timeout) => panic!("{what}: a read did not return"),
        Err(mpsc::RecvTimeoutError::Disconnected) => None,
    }
}

/// The metadata of a committed transaction.
#[derive(Debug, PartialEq, Eq)]
struct TxnMeta {
    user: Vec<u8>,
    description: Vec<u8>,
    extension: Vec<u8>,
}

/// What the store committed.
#[derive(Default)]
struct Model {
    /// Each record with its pickle, `None` for a deletion.
    records: BTreeMap<(Oid, Tid), Option<Vec<u8>>>,
    txns: BTreeMap<Tid, TxnMeta>,
    /// The bytes of each blob.
    blobs: BTreeMap<(Oid, Tid), Vec<u8>>,
}

impl Model {
    /// Tell if the store committed `data` for `oid` at `tid`.
    fn has(&self, oid: Oid, tid: Tid, data: &[u8]) -> bool {
        self.records.get(&(oid, tid)) == Some(&Some(data.to_vec()))
    }

    /// Return the length of the pickle of `oid` at `tid`, 0 for a deletion.
    fn len(&self, oid: Oid, tid: Tid) -> Option<u64> {
        let data = self.records.get(&(oid, tid))?;
        Some(data.as_ref().map_or(0, Vec::len) as u64)
    }

    fn oids(&self) -> BTreeSet<Oid> {
        self.records.keys().map(|&(oid, _)| oid).collect()
    }

    fn tids_of(&self, oid: Oid) -> Vec<Tid> {
        let range = (oid, Tid::ZERO)..=(oid, Tid::MAX);
        self.records
            .range(range)
            .map(|(&(_, tid), _)| tid)
            .collect()
    }
}

/// A pickle with `len` bytes of data that differ for each record.
fn pickle(oid: Oid, tid: Tid, len: usize) -> Vec<u8> {
    let mut data = b"\x80\x03capp.content\nItem\nq\x00.\x80\x03X".to_vec();
    let seed = oid.0.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ tid.0;
    data.extend((0..len).map(|i| (seed >> (i % 8 * 8)) as u8 ^ i as u8));
    data.push(b'.');
    data
}

/// A pickle that zstd shrinks, so that the store keeps it compressed.
fn compressible(oid: Oid, tid: Tid) -> Vec<u8> {
    let mut data = pickle(oid, tid, 8);
    for i in 0..60u32 {
        data.extend_from_slice(b"X\x0b\x00\x00\x00descriptionq\x02X\x05\x00\x00\x00title");
        data.extend_from_slice(&i.to_le_bytes());
    }
    data
}

/// The bytes of the blob of `oid` at `tid`.
fn blob_bytes(oid: Oid, tid: Tid) -> Vec<u8> {
    pickle(oid, tid, 300 + (oid.0 as usize % 7) * 100)
}

/// Commit 100 restore transactions on 40 oids in `vfs`: records in several
/// revisions, compressed records, deletions and blobs. The tables are large
/// enough to have branch pages. The tids and the
/// data depend on nothing else, so the database file is the same at each
/// run.
fn build(vfs: &MemVfs) -> Model {
    let store = Store::open_vfs(vfs.clone()).unwrap();
    let mut model = Model::default();
    let mut current: BTreeMap<Oid, Tid> = BTreeMap::new();
    let mut rng = Rng::new(0x0123_4567_89ab_cdef);
    for n in 0..100u64 {
        let tid = Tid(FIRST_TID + (n + 1) * 0x100);
        let meta = TxnMeta {
            user: format!("user {}", n % 3).into_bytes(),
            description: format!("transaction {n}").into_bytes(),
            extension: format!("extension {}", n % 5).into_bytes(),
        };
        let mut txn = store
            .tpc_begin_restore(tid, &meta.user, &meta.description, &meta.extension)
            .unwrap();
        let count = 1 + rng.below(5);
        let oids: BTreeSet<Oid> = (0..count).map(|_| Oid(1 + rng.below(40) as u64)).collect();
        for oid in oids {
            let data = match rng.below(20) {
                0 if current.contains_key(&oid) => {
                    txn.restore(oid, None).unwrap();
                    None
                }
                1 => {
                    let bytes = blob_bytes(oid, tid);
                    let mut file = store.new_blob_file().unwrap();
                    file.write_all(&bytes).unwrap();
                    let serial = current.get(&oid).copied().unwrap_or(Tid::ZERO);
                    txn.store_blob(oid, serial, BLOB_PICKLE, file).unwrap();
                    model.blobs.insert((oid, tid), bytes);
                    Some(BLOB_PICKLE.to_vec())
                }
                2..=4 => {
                    let data = compressible(oid, tid);
                    txn.restore(oid, Some(&data)).unwrap();
                    Some(data)
                }
                _ => {
                    let data = pickle(oid, tid, 40 + rng.below(260));
                    txn.restore(oid, Some(&data)).unwrap();
                    Some(data)
                }
            };
            model.records.insert((oid, tid), data);
            current.insert(oid, tid);
        }
        txn.vote().unwrap();
        assert_eq!(txn.finish().unwrap(), tid);
        model.txns.insert(tid, meta);
    }
    assert!(model.records.values().any(Option::is_none), "no deletion");
    assert!(!model.blobs.is_empty(), "no blob");
    let (&(oid, tid), _) = model
        .records
        .iter()
        .find(|&(&(oid, tid), data)| data.as_ref() == Some(&compressible(oid, tid)))
        .expect("no compressed record");
    assert_eq!(store.record_info(oid, tid).unwrap().codec, Codec::Zstd);
    model
}

/// Return the stored frames of every row of `model` in the store of `vfs`.
fn frames(vfs: &MemVfs, model: &Model) -> Vec<Vec<u8>> {
    let store = Store::open_vfs(vfs.clone()).unwrap();
    let keys = model
        .records
        .keys()
        .map(|&(oid, tid)| Key::Record(oid, tid))
        .chain(model.blobs.keys().map(|&(oid, tid)| Key::Blob(oid, tid)))
        .chain(model.txns.keys().map(|&tid| Key::Txn(tid)));
    keys.map(|key| testing::read_frame(&store, key).unwrap())
        .collect()
}

/// Return the offsets of the bytes of `file` that are inside a copy of one
/// of `frames`. The checksum of a frame, at bytes 24 to 31, finds it.
fn frame_bytes(file: &[u8], frames: &[Vec<u8>]) -> BTreeSet<usize> {
    let by_sum: BTreeMap<[u8; 8], &Vec<u8>> = frames
        .iter()
        .map(|frame| (frame[24..32].try_into().unwrap(), frame))
        .collect();
    let mut inside = BTreeSet::new();
    for at in 24..file.len().saturating_sub(8) {
        let sum: [u8; 8] = file[at..at + 8].try_into().unwrap();
        if let Some(frame) = by_sum.get(&sum) {
            let start = at - 24;
            if file[start..].starts_with(frame) {
                inside.extend(start..start + frame.len());
            }
        }
    }
    inside
}

/// Return the files of `vfs`.
fn files(vfs: &MemVfs) -> BTreeMap<String, Vec<u8>> {
    vfs.names()
        .into_iter()
        .map(|name| {
            let file = vfs.file(&name);
            let mut bytes = vec![0; file.len().unwrap() as usize];
            file.read(0, &mut bytes).unwrap();
            (name, bytes)
        })
        .collect()
}

/// Return a filesystem with `files`, the database file replaced by
/// `database`.
fn filesystem(files: &BTreeMap<String, Vec<u8>>, database: &[u8]) -> MemVfs {
    let vfs = MemVfs::new();
    for (name, bytes) in files {
        let bytes = if name == "zoe.redb" { database } else { bytes };
        let file = vfs.file(name);
        file.set_len(bytes.len() as u64).unwrap();
        file.write(0, bytes).unwrap();
    }
    vfs
}

/// Run every read path of `store`, and add to `wrong` each read that gave
/// data that `model` does not hold. A read that fails is correct.
fn wrong_reads(store: &Store, model: &Model, wrong: &mut Vec<String>) {
    for oid in model.oids() {
        let tids = model.tids_of(oid);
        if let Ok((data, serial)) = store.load(oid)
            && !model.has(oid, serial, &data)
        {
            wrong.push(format!("load of {oid:?} gave {serial:?}"));
        }
        let bounds = tids.iter().flat_map(|&tid| [tid, Tid(tid.0 + 1)]);
        for before in bounds.chain([Tid::MAX]) {
            if let Ok(Some(found)) = store.load_before(oid, before) {
                let next = found
                    .next
                    .is_none_or(|next| next >= before && model.records.contains_key(&(oid, next)));
                if found.serial >= before || !model.has(oid, found.serial, &found.data) || !next {
                    wrong.push(format!(
                        "load_before of {oid:?} before {before:?} gave {:?}, next {:?}",
                        found.serial, found.next
                    ));
                }
            }
        }
        for &tid in &tids {
            if let Ok(data) = store.load_serial(oid, tid)
                && !model.has(oid, tid, &data)
            {
                wrong.push(format!("load_serial of {oid:?} at {tid:?}"));
            }
            if let Ok(info) = store.record_info(oid, tid)
                && Some(info.data_len) != model.len(oid, tid)
            {
                wrong.push(format!("record_info of {oid:?} at {tid:?}: {info:?}"));
            }
        }
        if let Ok(history) = store.history(oid, 1_000) {
            let newest_first = history.windows(2).all(|pair| pair[0].tid > pair[1].tid);
            let committed = history.iter().all(|entry| {
                model.len(oid, entry.tid) == Some(entry.size)
                    && model.txns.get(&entry.tid).is_some_and(|meta| {
                        meta.user == entry.user && meta.description == entry.description
                    })
            });
            if !newest_first || !committed {
                wrong.push(format!("history of {oid:?}: {history:?}"));
            }
        }
    }
    for (&(oid, tid), bytes) in &model.blobs {
        if let Ok(mut reader) = store.open_blob(oid, tid) {
            let mut read = Vec::new();
            if reader.read_to_end(&mut read).is_ok() && read != *bytes {
                wrong.push(format!("the blob of {oid:?} at {tid:?}"));
            }
        }
    }
    if let Ok(txns) = store.iterator(None, None) {
        for txn in txns.flatten() {
            // A panic here would count as a refusal, so the check compares
            // and does not assert.
            let records: Vec<(Oid, Tid, Option<Vec<u8>>)> = txn
                .records
                .iter()
                .map(|record| (record.oid, record.tid, record.data.clone()))
                .collect();
            let expected: Vec<(Oid, Tid, Option<Vec<u8>>)> = model
                .records
                .iter()
                .filter(|&(&(_, tid), _)| tid == txn.tid)
                .map(|(&(oid, tid), data)| (oid, tid, data.clone()))
                .collect();
            let meta = TxnMeta {
                user: txn.user,
                description: txn.description,
                extension: txn.extension,
            };
            if model.txns.get(&txn.tid) != Some(&meta) || records != expected {
                wrong.push(format!("the iterator at {:?}", txn.tid));
            }
        }
    }
    let mut next = None;
    for _ in 0..=model.oids().len() {
        let Ok(Some(found)) = store.record_iternext(next) else {
            break;
        };
        if !model.has(found.oid, found.tid, &found.data) {
            wrong.push(format!(
                "record_iternext gave {:?} at {:?}",
                found.oid, found.tid
            ));
        }
        match found.next {
            Some(oid) => next = Some(oid),
            None => break,
        }
    }
}

/// Flip `flips()` seeded bits of the database file of `fixture` at the
/// offsets `places`, and check every read of each damaged store.
fn run(kind: &str, fixture: &Fixture, places: &[usize]) {
    quiet_check_panics();
    let (seed, count) = (seed(), flips());
    let database = fixture.database();
    let mut rng = Rng::new(seed);
    let mut failures = Vec::new();
    for n in 0..count {
        let (at, bit) = (places[rng.below(places.len())], rng.below(8));
        let mut flipped = database.to_vec();
        flipped[at] ^= 1 << bit;
        let what = format!("{kind} flip {n} of seed {seed:#x}: byte {at} bit {bit}");
        let (files, model) = (Arc::clone(&fixture.files), Arc::clone(&fixture.model));
        let wrong = within_time(&what, move || {
            let mut wrong = Vec::new();
            // A panic is a refusal: it returns no data.
            let _ = panic::catch_unwind(AssertUnwindSafe(|| {
                if let Ok(store) = Store::open_vfs(filesystem(&files, &flipped)) {
                    wrong_reads(&store, &model, &mut wrong);
                }
            }));
            wrong
        });
        if let Some(wrong) = wrong
            && !wrong.is_empty()
        {
            failures.push(format!("{what}: {wrong:?}"));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {count} flips gave wrong data. ZOE_FLIP_SEED={seed:#x} runs them again.\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// The store that `build` writes.
struct Fixture {
    /// The files of the store, by name.
    files: Arc<BTreeMap<String, Vec<u8>>>,
    model: Arc<Model>,
    /// The stored frame of each row.
    frames: Vec<Vec<u8>>,
}

impl Fixture {
    fn new() -> Self {
        let vfs = MemVfs::new();
        let model = build(&vfs);
        let frames = frames(&vfs, &model);
        let files = files(&vfs);
        {
            // The intact store reads back as it was committed.
            let store = Store::open_vfs(filesystem(&files, &files["zoe.redb"])).unwrap();
            let mut wrong = Vec::new();
            wrong_reads(&store, &model, &mut wrong);
            assert!(wrong.is_empty(), "{wrong:?}");
        }
        Self {
            files: Arc::new(files),
            model: Arc::new(model),
            frames,
        }
    }

    fn database(&self) -> &[u8] {
        &self.files["zoe.redb"]
    }
}

#[test]
fn flips_anywhere_in_the_database_file_never_return_wrong_data() {
    let fixture = Fixture::new();
    let database = fixture.database();
    let used: Vec<usize> = (0..database.len())
        .filter(|&at| database[at] != 0)
        .collect();
    run("a", &fixture, &used);
}

/// The flips miss every stored frame, so they hit the pages and the keys
/// of redb. A damaged index can give a read the row of another key.
#[test]
fn flips_in_the_index_of_the_database_file_never_return_wrong_data() {
    let fixture = Fixture::new();
    let database = fixture.database();
    let inside = frame_bytes(database, &fixture.frames);
    let index: Vec<usize> = (0..database.len())
        .filter(|&at| database[at] != 0 && !inside.contains(&at))
        .collect();
    assert!(!index.is_empty());
    run("an index", &fixture, &index);
}

/// Seal `frame` again: write the checksum of its bytes under `key`. The
/// frame has no typed-field slots.
fn seal(key: &[u8], frame: &mut [u8]) {
    let mut hasher = Xxh3::new();
    hasher.update(key);
    hasher.update(&frame[..24]);
    hasher.update(&frame[32..]);
    frame[24..32].copy_from_slice(&hasher.digest().to_le_bytes());
}

/// The record key of `oid` at `tid`: both big-endian.
fn record_key(oid: Oid, tid: Tid) -> [u8; 16] {
    let mut key = [0; 16];
    key[..8].copy_from_slice(&oid.0.to_be_bytes());
    key[8..].copy_from_slice(&tid.0.to_be_bytes());
    key
}

/// Run every read of the record of `oid` at `tid`, which is current. Each
/// result holds the data. `record_info` and `history` give only a length,
/// so their result holds that number of zero bytes.
fn read_current(
    store: &Store,
    oid: Oid,
    tid: Tid,
) -> Vec<(&'static str, Result<Vec<u8>, ZoeError>)> {
    let length = |len: u64| vec![0; len as usize];
    vec![
        ("load", store.load(oid).map(|(data, _)| data)),
        (
            "load_before",
            store
                .load_before(oid, Tid(tid.0 + 1))
                .map(|found| found.map(|found| found.data).unwrap_or_default()),
        ),
        ("load_serial", store.load_serial(oid, tid)),
        (
            "record_info",
            store
                .record_info(oid, tid)
                .map(|info| length(info.data_len)),
        ),
        (
            "history",
            store
                .history(oid, 10)
                .map(|history| length(history[0].size)),
        ),
        (
            "iterator",
            store
                .iterator(Some(tid), Some(tid))
                .and_then(|txns| txns.collect::<Result<Vec<_>, _>>())
                .map(|txns| txns[0].records[0].data.clone().unwrap_or_default()),
        ),
        (
            "record_iternext",
            store
                .record_iternext(Some(oid))
                .map(|found| found.map(|found| found.data).unwrap_or_default()),
        ),
    ]
}

/// A zstd payload with a flipped bit and a valid checksum reaches the
/// decoder. Every read then raises `CorruptRecord`, or every read gives
/// the same data: zstd frames carry no checksum of their own, so a flip in
/// a literal can decode. A read never panics and never gives another
/// error.
#[test]
fn a_damaged_zstd_payload_under_a_valid_checksum_raises_corrupt_record_or_decodes() {
    let store = Store::open_vfs(MemVfs::new()).unwrap();
    let (oid, tid) = (Oid(1), Tid(FIRST_TID));
    let data = compressible(oid, tid);
    let mut txn = store.tpc_begin_restore(tid, b"", b"zstd", b"").unwrap();
    txn.restore(oid, Some(&data)).unwrap();
    txn.vote().unwrap();
    txn.finish().unwrap();
    assert_eq!(store.record_info(oid, tid).unwrap().codec, Codec::Zstd);

    let frame = testing::read_frame(&store, Key::Record(oid, tid)).unwrap();
    assert_eq!(frame[20..22], [0, 0], "the frame has no typed-field slots");
    let key = record_key(oid, tid);
    let seed = seed();
    let mut rng = Rng::new(seed);
    // Every bit of the zstd frame header and of the first block header,
    // where the lengths are, and one seeded bit of each later byte.
    let flips = (32..frame.len()).flat_map(|byte| {
        let bits: Vec<usize> = if byte < 32 + 24 {
            (0..8).collect()
        } else {
            vec![rng.below(8)]
        };
        bits.into_iter().map(move |bit| (byte, bit))
    });
    let (mut corrupt, mut decoded) = (0, 0);
    for (byte, bit) in flips.collect::<Vec<_>>() {
        let mut damaged = frame.clone();
        damaged[byte] ^= 1 << bit;
        seal(&key, &mut damaged);
        testing::write_frame(&store, Key::Record(oid, tid), &damaged).unwrap();
        let what = format!("byte {byte} bit {bit} of the zstd frame, seed {seed:#x}");
        let reads = read_current(&store, oid, tid);
        let mut data: Option<&Vec<u8>> = None;
        for (read, result) in &reads {
            match result {
                Err(ZoeError::CorruptRecord { oid: o, tid: t }) if (*o, *t) == (oid, tid) => {}
                Ok(read_data) => {
                    let same = match (*read, data) {
                        (_, None) => true,
                        ("record_info" | "history", Some(data)) => read_data.len() == data.len(),
                        (_, Some(data)) => read_data == data,
                    };
                    assert!(same, "{what}: {read} gave other data");
                    if !matches!(*read, "record_info" | "history") {
                        data = Some(read_data);
                    }
                }
                Err(error) => panic!("{what}: {read} gave {error:?}"),
            }
        }
        let failed = reads.iter().filter(|(_, result)| result.is_err()).count();
        assert!(
            failed == 0 || failed == reads.len(),
            "{what}: some reads failed and some gave data: {reads:?}"
        );
        if failed == 0 {
            decoded += 1;
        } else {
            corrupt += 1;
        }
    }
    assert!(corrupt > 0, "no flip broke the zstd frame");
    eprintln!("zstd flips: {corrupt} raised CorruptRecord, {decoded} decoded");

    testing::write_frame(&store, Key::Record(oid, tid), &frame).unwrap();
    assert_eq!(store.load(oid).unwrap(), (data, tid));
}
