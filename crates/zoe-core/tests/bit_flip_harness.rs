//! The bit-flip harness: a wider search than the acceptance test
//! `bit_flips`.
//!
//! It builds a store whose tables have branch pages. Each flip damages one
//! bit. Then every read path runs: `load`, `load_before` and its next
//! serial, `load_serial`, `record_info`, `history`, the transaction
//! iterator, `record_iternext` and the blob reads. An oracle compares each
//! result with the exact answer that the committed data gives, and puts it
//! in one class:
//!
//! - ok: the exact answer;
//! - error: the read fails with an error;
//! - hidden: committed data is missing, as `NotFound`, `None`, or a
//!   missing history entry, transaction, record or current record;
//! - stale: an older record comes as the current one, or a next serial
//!   is missing or skips a record;
//! - wrong: data that the store did not commit, or committed data under
//!   another oid, tid or snapshot.
//!
//! A flip can also make the open fail, panic, abort the process or hang a
//! read. A flip that hits a stored frame or a blob file gives no wrong, no
//! stale and no hidden read. A flip in the pages of redb gives no wrong
//! read on the default seeds. Its stale and hidden reads, panics, aborts
//! and hangs are counted and printed, not asserted: redb does not check
//! its pages on a read. `crates/zoe-core/CLAUDE.md` gives the measured
//! rates and the known limits.
//!
//! The flips in the raw database file run in child processes of this test,
//! so that an abort stops only the child. The parent counts the abort and
//! starts a new child at the next flip.
//!
//! It also flips bits in a zstd payload and seals the frame again, so that
//! the decoder gets the damage past the checksum.
//!
//! Every flip comes from a seeded generator, and the counts and a failure
//! give the seed and the build profile. `ZOE_FLIP_SEED` sets another seed,
//! and `ZOE_FLIPS` sets the number of flips of each run, for a longer
//! search.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io::{BufRead, BufReader, Read, Write};
use std::panic::{self, AssertUnwindSafe};
use std::process::{Command, Stdio};
use std::sync::{Arc, Once, mpsc};
use std::thread;
use std::time::Duration;

use redb::StorageBackend;
use xxhash_rust::xxh3::{Xxh3, xxh3_64};
use zoe_core::testing::{self, Key};
use zoe_core::vfs::MemVfs;
use zoe_core::{Codec, Compression, Oid, Options, Store, SystemClock, Tid, ZoeError};

/// The default seed of the flips.
const SEED: u64 = 0x2545_f491_4f6c_dd1d;

/// The default number of flips of each run.
const FLIPS: usize = 150;

/// The name of the threads that run the reads of a damaged store. A panic
/// there is a refusal, so the panic hook stays silent for them.
const CHECK_THREAD: &str = "flip check";

/// The longest time that the reads of one flip take before they count as
/// a hang. It leaves room for a slow build, such as one under
/// ThreadSanitizer.
const HANG: Duration = Duration::from_secs(60);

/// The variable that makes a test run as a child: it holds the first flip
/// and the hash of the database file.
const CHILD: &str = "ZOE_FLIP_CHILD";

/// The start of each line that a child writes for its parent.
const TAG: &str = "zoe-flip";

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

/// Return the build profile, for the counts.
fn profile() -> &'static str {
    if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    }
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

/// A record of the model: its tid and its pickle, `None` for a deletion.
type Version<'a> = (Tid, &'a Option<Vec<u8>>);

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

    /// Return the records of `oid` whose tid is in `tids`, in tid order.
    fn versions(&self, oid: Oid, tids: impl std::ops::RangeBounds<Tid>) -> Vec<Version<'_>> {
        use std::ops::Bound;
        let start = match tids.start_bound() {
            Bound::Unbounded => Bound::Included((oid, Tid::ZERO)),
            bound => bound.map(|&tid| (oid, tid)),
        };
        let end = match tids.end_bound() {
            Bound::Unbounded => Bound::Included((oid, Tid::MAX)),
            bound => bound.map(|&tid| (oid, tid)),
        };
        self.records
            .range((start, end))
            .map(|(&(_, tid), data)| (tid, data))
            .collect()
    }

    /// Return the newest record of `oid`.
    fn newest(&self, oid: Oid) -> Option<Version<'_>> {
        self.versions(oid, ..).pop()
    }

    /// Return the newest record of `oid` before `tid`.
    fn before(&self, oid: Oid, tid: Tid) -> Option<Version<'_>> {
        self.versions(oid, ..tid).pop()
    }

    /// Return the tid of the oldest record of `oid` at or after `tid`.
    fn next(&self, oid: Oid, tid: Tid) -> Option<Tid> {
        self.versions(oid, tid..).first().map(|&(tid, _)| tid)
    }

    /// Return the oids whose newest record is not a deletion.
    fn live(&self) -> Vec<Oid> {
        let oids = self.oids().into_iter();
        oids.filter(|&oid| self.newest(oid).is_some_and(|(_, data)| data.is_some()))
            .collect()
    }
}

/// The class of the result of one read, from the best to the worst.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Class {
    Ok,
    Error,
    Hidden,
    Stale,
    Wrong,
}

impl Class {
    const ALL: [Class; 5] = [
        Self::Ok,
        Self::Error,
        Self::Hidden,
        Self::Stale,
        Self::Wrong,
    ];

    fn index(self) -> usize {
        self as usize
    }
}

/// The classes of the reads of one flip.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Reads {
    /// The number of reads in each class, in the order of `Class::ALL`.
    counts: [usize; 5],
    /// What each wrong, stale or hidden read was, for a failure message.
    notes: Vec<String>,
}

impl Reads {
    /// Count one read of `class`. `what` names the read.
    fn add(&mut self, class: Class, what: impl FnOnce() -> String) {
        self.counts[class.index()] += 1;
        if class >= Class::Hidden && self.notes.len() < 8 {
            self.notes.push(format!("{class:?}: {}", what()));
        }
    }

    fn has(&self, class: Class) -> bool {
        self.counts[class.index()] > 0
    }
}

/// What one flip gave.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Outcome {
    /// The store opened, and the reads gave these classes.
    Read(Reads),
    /// The store refused to open.
    Refused,
    /// The open or a read panicked after the reads that it counted.
    Panic(Reads),
    /// The reads did not end in time.
    Hang,
    /// The process aborted.
    Abort,
}

impl Outcome {
    fn reads(&self) -> Option<&Reads> {
        match self {
            Self::Read(reads) | Self::Panic(reads) => Some(reads),
            _ => None,
        }
    }
}

/// The counts of a run of flips.
#[derive(Default)]
struct Tally {
    flips: usize,
    /// The flips with at least one read in each class.
    classes: [usize; 5],
    /// The reads in each class.
    reads: [usize; 5],
    refused: usize,
    panics: usize,
    hangs: usize,
    aborts: usize,
    /// The flips that gave a read in a class that the run forbids.
    failures: Vec<String>,
    /// The first flips that gave a stale or a hidden read, as examples.
    examples: Vec<String>,
}

impl Tally {
    /// Count the outcome of a flip. A read in a class of `forbidden` is a
    /// failure.
    fn add(&mut self, what: &str, outcome: &Outcome, forbidden: &[Class]) {
        self.flips += 1;
        match outcome {
            Outcome::Refused => self.refused += 1,
            Outcome::Panic(_) => self.panics += 1,
            Outcome::Hang => self.hangs += 1,
            Outcome::Abort => self.aborts += 1,
            Outcome::Read(_) => {}
        }
        let Some(reads) = outcome.reads() else {
            return;
        };
        for class in Class::ALL {
            self.reads[class.index()] += reads.counts[class.index()];
            if reads.has(class) {
                self.classes[class.index()] += 1;
            }
        }
        if forbidden.iter().any(|&class| reads.has(class)) {
            self.failures.push(format!("{what}: {:?}", reads.notes));
        } else if !reads.notes.is_empty() && self.examples.len() < 4 {
            self.examples.push(format!("{what}: {:?}", reads.notes));
        }
    }
}

impl fmt::Display for Tally {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [ok, error, hidden, stale, wrong] = self.classes;
        write!(
            f,
            "{} flips: {wrong} wrong, {stale} stale, {hidden} hidden, {error} with an error, \
             {ok} with an exact read, {} refused, {} panic, {} abort, {} hang; reads {:?} \
             as ok, error, hidden, stale, wrong",
            self.flips, self.refused, self.panics, self.aborts, self.hangs, self.reads
        )?;
        for example in &self.examples {
            write!(f, "\n  {example}")?;
        }
        Ok(())
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

/// Open the store in `vfs` with zstd at level 3. So the store writes
/// records of both codecs.
fn open_vfs(vfs: MemVfs) -> zoe_core::Result<Store> {
    let options = Options {
        compression: Compression::Zstd { level: 3 },
        ..Options::default()
    };
    Store::open_full(vfs, SystemClock, options)
}

/// Commit 100 restore transactions on 40 oids in `vfs`: records in several
/// revisions, compressed records, deletions and blobs. The tables are large
/// enough to have branch pages. The tids and the
/// data depend on nothing else, so the database file is the same at each
/// run.
fn build(vfs: &MemVfs) -> Model {
    let store = open_vfs(vfs.clone()).unwrap();
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
    let store = open_vfs(vfs.clone()).unwrap();
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

/// Return the tid that `error` names, when it names one.
fn named_tid(error: &ZoeError) -> Option<Tid> {
    match error {
        ZoeError::CorruptTxn { tid }
        | ZoeError::CorruptRecord { tid, .. }
        | ZoeError::CorruptBlob { tid, .. } => Some(*tid),
        _ => None,
    }
}

/// Run every read path of `store`, compare each result with the exact
/// answer that `model` gives, and count its class in `reads`.
fn classify(store: &Store, model: &Model, reads: &mut Reads) {
    for oid in model.oids() {
        classify_oid(store, model, oid, reads);
    }
    for (&(oid, tid), bytes) in &model.blobs {
        let class = match store.open_blob(oid, tid) {
            Ok(mut reader) => {
                let mut read = Vec::new();
                match reader.read_to_end(&mut read) {
                    Ok(_) if read == *bytes => Class::Ok,
                    Ok(_) => Class::Wrong,
                    Err(_) => Class::Error,
                }
            }
            Err(ZoeError::NotFound { .. }) => Class::Hidden,
            Err(_) => Class::Error,
        };
        reads.add(class, || format!("the blob of {oid:?} at {tid:?}"));
    }
    reads.add(iterator_class(store, model, None, None), || {
        "the iterator".to_owned()
    });
    for &tid in model.txns.keys() {
        let class = iterator_class(store, model, Some(tid), Some(tid));
        reads.add(class, || format!("the iterator at {tid:?}"));
    }
    reads.add(iternext_class(store, model), || {
        "a walk of record_iternext".to_owned()
    });
    let live = model.live();
    for oid in model.oids() {
        let class = iternext_from_class(store, model, &live, oid);
        reads.add(class, || format!("record_iternext from {oid:?}"));
    }
}

/// Run the reads of the records of `oid` and count their classes.
fn classify_oid(store: &Store, model: &Model, oid: Oid, reads: &mut Reads) {
    let (newest, current) = model.newest(oid).expect("the oid has a record");
    let class = match store.load(oid) {
        Ok((data, serial)) if !model.has(oid, serial, &data) => Class::Wrong,
        Ok((_, serial)) if serial == newest => Class::Ok,
        Ok(_) => Class::Stale,
        Err(ZoeError::NotFound { .. }) if current.is_none() => Class::Ok,
        Err(ZoeError::NotFound { .. }) => Class::Hidden,
        Err(_) => Class::Error,
    };
    reads.add(class, || format!("load of {oid:?}"));

    let tids: Vec<Tid> = model
        .versions(oid, ..)
        .iter()
        .map(|&(tid, _)| tid)
        .collect();
    let bounds = tids.iter().flat_map(|&tid| [tid, Tid(tid.0 + 1)]);
    for bound in bounds.chain([Tid::MAX]) {
        let class = load_before_class(store, model, oid, bound);
        reads.add(class, || format!("load_before of {oid:?} before {bound:?}"));
    }
    for &tid in &tids {
        let deleted = model.records[&(oid, tid)].is_none();
        let class = match store.load_serial(oid, tid) {
            Ok(data) if model.has(oid, tid, &data) => Class::Ok,
            Ok(_) => Class::Wrong,
            Err(ZoeError::NotFound { .. }) if deleted => Class::Ok,
            Err(ZoeError::NotFound { .. }) => Class::Hidden,
            Err(_) => Class::Error,
        };
        reads.add(class, || format!("load_serial of {oid:?} at {tid:?}"));
        let class = match store.record_info(oid, tid) {
            Ok(info) if Some(info.data_len) == model.len(oid, tid) => Class::Ok,
            Ok(_) => Class::Wrong,
            Err(ZoeError::NotFound { .. }) => Class::Hidden,
            Err(_) => Class::Error,
        };
        reads.add(class, || format!("record_info of {oid:?} at {tid:?}"));
    }

    let class = match store.history(oid, 1_000) {
        Ok(history) => {
            let newest_first = history.windows(2).all(|pair| pair[0].tid > pair[1].tid);
            let committed = history.iter().all(|entry| {
                model.len(oid, entry.tid) == Some(entry.size)
                    && model.txns.get(&entry.tid).is_some_and(|meta| {
                        meta.user == entry.user && meta.description == entry.description
                    })
            });
            if !newest_first || !committed {
                Class::Wrong
            } else if history.len() < tids.len() {
                Class::Hidden
            } else {
                Class::Ok
            }
        }
        Err(ZoeError::NotFound { .. }) => Class::Hidden,
        Err(_) => Class::Error,
    };
    reads.add(class, || format!("history of {oid:?}"));
}

/// Return the class of `load_before(oid, bound)`.
fn load_before_class(store: &Store, model: &Model, oid: Oid, bound: Tid) -> Class {
    let expected = model.before(oid, bound);
    let next = model.next(oid, bound);
    match store.load_before(oid, bound) {
        Ok(Some(found)) => {
            let next_inside = found
                .next
                .is_none_or(|tid| tid >= bound && model.records.contains_key(&(oid, tid)));
            if found.serial >= bound || !model.has(oid, found.serial, &found.data) || !next_inside {
                Class::Wrong
            } else if Some(found.serial) != expected.map(|(tid, _)| tid) || found.next != next {
                // An older record as the current one, or a next serial that
                // is missing or skips a record.
                Class::Stale
            } else {
                Class::Ok
            }
        }
        Ok(None) if expected.is_none() => Class::Ok,
        Ok(None) => Class::Hidden,
        Err(ZoeError::NotFound { .. }) if expected.is_some_and(|(_, data)| data.is_none()) => {
            Class::Ok
        }
        Err(ZoeError::NotFound { .. }) => Class::Hidden,
        Err(_) => Class::Error,
    }
}

/// Return the class of the transaction iterator from `start` to `stop`.
/// It is the worst class of its items. A transaction of the range that no
/// item gives and that no error names is hidden.
fn iterator_class(store: &Store, model: &Model, start: Option<Tid>, stop: Option<Tid>) -> Class {
    let range = start.unwrap_or(Tid::ZERO)..=stop.unwrap_or(Tid::MAX);
    let Ok(txns) = store.iterator(start, stop) else {
        return Class::Error;
    };
    let mut class = Class::Ok;
    let mut given = BTreeSet::new();
    let mut last = None;
    let mut unnamed = false;
    // An iterator gives at most one item for each row and each tid of the
    // records.
    for item in txns.take(model.txns.len() + model.records.len() + 1) {
        let txn = match item {
            Ok(txn) => txn,
            Err(error) => {
                class = class.max(Class::Error);
                match named_tid(&error) {
                    Some(tid) => {
                        given.insert(tid);
                    }
                    None => unnamed = true,
                }
                continue;
            }
        };
        let expected = model.records.keys().filter(|&&(_, tid)| tid == txn.tid);
        let committed = model.txns.get(&txn.tid).is_some_and(|meta| {
            meta.user == txn.user
                && meta.description == txn.description
                && meta.extension == txn.extension
        }) && txn.records.iter().all(|record| {
            record.tid == txn.tid && model.records.get(&(record.oid, txn.tid)) == Some(&record.data)
        }) && txn.records.windows(2).all(|pair| pair[0].oid < pair[1].oid)
            && last.is_none_or(|last| txn.tid > last)
            && range.contains(&txn.tid);
        class = class.max(if !committed {
            Class::Wrong
        } else if txn.records.len() < expected.count() {
            Class::Hidden
        } else {
            Class::Ok
        });
        given.insert(txn.tid);
        last = Some(txn.tid);
    }
    if !unnamed && model.txns.range(range).any(|(tid, _)| !given.contains(tid)) {
        class = class.max(Class::Hidden);
    }
    class
}

/// Return the class of a walk of `record_iternext` over every current
/// record. A current record that the walk passes is hidden.
fn iternext_class(store: &Store, model: &Model) -> Class {
    let live = model.live();
    let mut class = Class::Ok;
    let mut walked: Vec<Oid> = Vec::new();
    let mut failed = false;
    let mut next = None;
    for _ in 0..=live.len() {
        let found = match store.record_iternext(next) {
            Ok(Some(found)) => found,
            Ok(None) => break,
            Err(_) => {
                failed = true;
                break;
            }
        };
        let in_order = walked.last().is_none_or(|&last| found.oid > last)
            && next.is_none_or(|from| found.oid >= from);
        if !model.has(found.oid, found.tid, &found.data) || !in_order {
            class = class.max(Class::Wrong);
        } else if model.newest(found.oid).map(|(tid, _)| tid) != Some(found.tid) {
            class = class.max(Class::Stale);
        }
        walked.push(found.oid);
        match found.next {
            Some(oid) if oid > found.oid => next = Some(oid),
            Some(_) => {
                class = class.max(Class::Wrong);
                break;
            }
            None => break,
        }
    }
    if failed {
        class = class.max(Class::Error);
    }
    // After an error, the walk cannot see the oids after the last one.
    let reached = walked.last().copied();
    let passed = live
        .iter()
        .any(|oid| !walked.contains(oid) && (!failed || reached.is_some_and(|last| *oid < last)));
    if passed {
        class = class.max(Class::Hidden);
    }
    class
}

/// Return the class of `record_iternext(Some(from))`. `live` holds the
/// oids whose newest record is not a deletion.
fn iternext_from_class(store: &Store, model: &Model, live: &[Oid], from: Oid) -> Class {
    let expected = live.iter().copied().find(|&oid| oid >= from);
    let found = match store.record_iternext(Some(from)) {
        Ok(Some(found)) => found,
        Ok(None) if expected.is_none() => return Class::Ok,
        Ok(None) => return Class::Hidden,
        Err(_) => return Class::Error,
    };
    let next_known = found
        .next
        .is_none_or(|next| next > found.oid && model.newest(next).is_some());
    if found.oid < from || !model.has(found.oid, found.tid, &found.data) || !next_known {
        return Class::Wrong;
    }
    let mut class = Class::Ok;
    // An older record as the current one: of a deleted oid or of a live
    // oid, or a next oid whose newest record is a deletion.
    let current = model.newest(found.oid).map(|(tid, _)| tid) == Some(found.tid);
    let next_live = found.next.is_none_or(|next| live.contains(&next));
    if !live.contains(&found.oid) || !current || !next_live {
        class = class.max(Class::Stale);
    }
    // A live oid that the read passes, as the record or as the next oid.
    let after = live.iter().copied().find(|&oid| oid > found.oid);
    let passed_next = after.is_some_and(|after| found.next.is_none_or(|next| next > after));
    if expected.is_some_and(|oid| found.oid > oid) || passed_next {
        class = class.max(Class::Hidden);
    }
    class
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
            // The intact store gives the exact answer to every read.
            let store = open_vfs(filesystem(&files, &files["zoe.redb"])).unwrap();
            let mut reads = Reads::default();
            classify(&store, &model, &mut reads);
            assert_eq!(reads.counts[1..], [0; 4], "{:?}", reads.notes);
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

/// Return `bytes` with the bit `bit` of the byte `byte` flipped.
fn flip(bytes: &[u8], byte: usize, bit: usize) -> Vec<u8> {
    let mut flipped = bytes.to_vec();
    flipped[byte] ^= 1 << bit;
    flipped
}

/// Run `check` on a check thread. A check that does not end in time is a
/// hang, and its thread runs on until the process ends.
fn check_in_time(check: impl FnOnce() -> Outcome + Send + 'static) -> Outcome {
    quiet_check_panics();
    let (done, wait) = mpsc::channel();
    thread::Builder::new()
        .name(CHECK_THREAD.to_owned())
        .spawn(move || {
            let _ = done.send(check());
        })
        .unwrap();
    match wait.recv_timeout(HANG) {
        Ok(outcome) => outcome,
        Err(mpsc::RecvTimeoutError::Timeout) => Outcome::Hang,
        Err(mpsc::RecvTimeoutError::Disconnected) => Outcome::Panic(Reads::default()),
    }
}

/// Open the store of `fixture` with the bit `bit` of the byte `at` of its
/// database file flipped, and run every read.
fn raw_outcome(fixture: &Fixture, at: usize, bit: usize) -> Outcome {
    let flipped = flip(fixture.database(), at, bit);
    let (files, model) = (Arc::clone(&fixture.files), Arc::clone(&fixture.model));
    check_in_time(move || {
        let mut reads = Reads::default();
        let opened = panic::catch_unwind(AssertUnwindSafe(|| {
            let store = open_vfs(filesystem(&files, &flipped)).ok()?;
            classify(&store, &model, &mut reads);
            // The close of the database can panic too.
            drop(store);
            Some(())
        }));
        match opened {
            Ok(Some(())) => Outcome::Read(reads),
            Ok(None) => Outcome::Refused,
            Err(_) => Outcome::Panic(reads),
        }
    })
}

/// Write `outcome` as the words of a line of a child.
fn encode(outcome: &Outcome) -> String {
    let counts = |reads: &Reads| reads.counts.map(|count| count.to_string()).join(" ");
    match outcome {
        Outcome::Read(reads) => format!("read {}", counts(reads)),
        Outcome::Panic(reads) => format!("panic {}", counts(reads)),
        Outcome::Refused => "refused".to_owned(),
        Outcome::Hang => "hang".to_owned(),
        Outcome::Abort => "abort".to_owned(),
    }
}

/// Read the outcome that `encode` wrote, with the notes of its reads.
fn decode<'a>(mut words: impl Iterator<Item = &'a str>, notes: Vec<String>) -> Outcome {
    let kind = words.next();
    let mut counts = [0; 5];
    for (count, word) in counts.iter_mut().zip(words) {
        *count = word.parse().expect("a count");
    }
    let reads = Reads { counts, notes };
    match kind {
        Some("read") => Outcome::Read(reads),
        Some("panic") => Outcome::Panic(reads),
        Some("refused") => Outcome::Refused,
        Some("hang") => Outcome::Hang,
        Some(kind) => panic!("unknown outcome {kind}"),
        None => panic!("no outcome"),
    }
}

/// Run the flips from `from` as a child: write a line before each flip,
/// then a line for each note and one for the outcome.
fn run_child(fixture: &Fixture, flips: &[(usize, usize)], from: usize) {
    for (n, &(at, bit)) in flips.iter().enumerate().skip(from) {
        println!("{TAG} start {n}");
        let outcome = raw_outcome(fixture, at, bit);
        for note in outcome.reads().map_or(&[][..], |reads| &reads.notes) {
            println!("{TAG} note {n} {note}");
        }
        println!("{TAG} done {n} {}", encode(&outcome));
    }
}

/// What the parent reads from the lines of its children, one child after
/// the other.
struct Reader {
    outcomes: Vec<Option<Outcome>>,
    /// The flip that the current child started and did not finish.
    started: Option<usize>,
    /// The notes of the flip that the current child runs.
    notes: Vec<String>,
}

impl Reader {
    fn new(count: usize) -> Self {
        Self {
            outcomes: vec![None; count],
            started: None,
            notes: Vec::new(),
        }
    }

    /// Read one line of the current child. Text can come before the tag:
    /// libtest writes `test … ` before the first line of a child.
    fn line(&mut self, line: &str) {
        let Some(at) = line.find(TAG) else {
            return;
        };
        let mut words = line[at + TAG.len()..].split_whitespace();
        let (word, n) = (
            words.next(),
            words.next().and_then(|n| n.parse::<usize>().ok()),
        );
        match (word, n) {
            (Some("start"), Some(n)) => self.started = Some(n),
            (Some("note"), Some(_)) => self.notes.push(words.collect::<Vec<_>>().join(" ")),
            (Some("done"), Some(n)) => {
                self.outcomes[n] = Some(decode(words, std::mem::take(&mut self.notes)));
                self.started = None;
            }
            _ => panic!("a child wrote {line:?}"),
        }
    }

    /// End the current child. `success` tells if it exited with success,
    /// and `failure` describes its exit. Return the flip after the one
    /// where the child aborted, or `None` when the child did all its flips.
    fn end(&mut self, success: bool, failure: impl FnOnce() -> String) -> Option<usize> {
        self.notes.clear();
        match self.started.take() {
            Some(n) => {
                self.outcomes[n] = Some(Outcome::Abort);
                Some(n + 1)
            }
            None => {
                assert!(success, "a child failed out of a flip: {}", failure());
                None
            }
        }
    }

    fn outcomes(self) -> Vec<Outcome> {
        let outcomes = self.outcomes.into_iter().enumerate();
        outcomes
            .map(|(n, outcome)| outcome.unwrap_or_else(|| panic!("no outcome for flip {n}")))
            .collect()
    }
}

/// Run the flips of the test `test` in child processes of this test, and
/// return the outcome of each flip. A child that stops during a flip
/// aborted in it, and the next child starts at the flip after it.
fn run_children(test: &str, fixture: &Fixture, count: usize) -> Vec<Outcome> {
    let hash = xxh3_64(fixture.database());
    let mut reader = Reader::new(count);
    let mut from = 0;
    while from < count {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([test, "--exact", "--nocapture", "--test-threads=1"])
            .env(CHILD, format!("{from} {hash:x}"))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stderr = child.stderr.take().unwrap();
        let errors = thread::spawn(move || {
            let mut text = String::new();
            let _ = BufReader::new(stderr).read_to_string(&mut text);
            text
        });
        for line in BufReader::new(child.stdout.take().unwrap()).lines() {
            reader.line(&line.unwrap());
        }
        let status = child.wait().unwrap();
        let errors = errors.join().unwrap();
        match reader.end(status.success(), || format!("{status}\n{errors}")) {
            Some(next) => from = next,
            None => break,
        }
    }
    reader.outcomes()
}

#[test]
fn the_reader_takes_a_late_tag_and_two_aborts_in_a_row() {
    let test = "test flips_anywhere_in_the_database_file_never_return_wrong_data ...";
    let mut reader = Reader::new(4);
    // The first child aborts in flip 1, and the second child aborts in its
    // first flip, flip 2. Each first line has the text of libtest before
    // the tag.
    let children: [(&[String], bool, Option<usize>); 3] = [
        (
            &[
                "running 1 test".to_owned(),
                format!("{test} {TAG} start 0"),
                format!("{TAG} done 0 refused"),
                format!("{TAG} start 1"),
            ],
            false,
            Some(2),
        ),
        (
            &["running 1 test".to_owned(), format!("{test} {TAG} start 2")],
            false,
            Some(3),
        ),
        (
            &[
                "running 1 test".to_owned(),
                format!("{test} {TAG} start 3"),
                format!("{TAG} note 3 Hidden: a read"),
                format!("{TAG} done 3 read 1 0 1 0 0"),
                "ok".to_owned(),
            ],
            true,
            None,
        ),
    ];
    for (lines, success, next) in children {
        lines.iter().for_each(|line| reader.line(line));
        assert_eq!(reader.end(success, String::new), next);
    }
    let reads = Reads {
        counts: [1, 0, 1, 0, 0],
        notes: vec!["Hidden: a read".to_owned()],
    };
    assert_eq!(
        reader.outcomes(),
        [
            Outcome::Refused,
            Outcome::Abort,
            Outcome::Abort,
            Outcome::Read(reads),
        ]
    );
}

/// Flip seeded bits of the database file of the fixture at the offsets
/// that `places` gives, in child processes, and count the classes. A wrong
/// read fails the test. `test` is the name of the calling test.
fn raw_run(kind: &str, test: &str, places: impl Fn(&Fixture) -> Vec<usize>) {
    let fixture = Fixture::new();
    let places = places(&fixture);
    let seed = seed();
    let mut rng = Rng::new(seed);
    let flips: Vec<(usize, usize)> = (0..flips())
        .map(|_| (places[rng.below(places.len())], rng.below(8)))
        .collect();
    if let Ok(spec) = std::env::var(CHILD) {
        let (from, hash) = spec.split_once(' ').expect("a first flip and a hash");
        let built = format!("{:x}", xxh3_64(fixture.database()));
        assert_eq!(built, hash, "the child built another database file");
        run_child(&fixture, &flips, from.parse().expect("a first flip"));
        return;
    }
    let outcomes = run_children(test, &fixture, flips.len());
    let mut tally = Tally::default();
    for (n, (&(at, bit), outcome)) in flips.iter().zip(&outcomes).enumerate() {
        let what = format!("flip {n}: byte {at} bit {bit}");
        tally.add(&what, outcome, &[Class::Wrong]);
    }
    let run = format!("flips {kind}, seed {seed:#x}, {} build", profile());
    eprintln!("bit {run}: {tally}");
    assert!(
        tally.failures.is_empty(),
        "{} {run} gave wrong data. ZOE_FLIP_SEED={seed:#x} runs them again.\n{}",
        tally.failures.len(),
        tally.failures.join("\n")
    );
}

#[test]
fn flips_anywhere_in_the_database_file_never_return_wrong_data() {
    raw_run(
        "anywhere in the database file",
        "flips_anywhere_in_the_database_file_never_return_wrong_data",
        |fixture| {
            let database = fixture.database();
            (0..database.len())
                .filter(|&at| database[at] != 0)
                .collect()
        },
    );
}

/// The flips miss every stored frame, so they hit the pages and the keys
/// of redb.
#[test]
fn flips_in_the_index_of_the_database_file_never_return_wrong_data() {
    raw_run(
        "in the index of the database file",
        "flips_in_the_index_of_the_database_file_never_return_wrong_data",
        |fixture| {
            let database = fixture.database();
            let inside = frame_bytes(database, &fixture.frames);
            let index: Vec<usize> = (0..database.len())
                .filter(|&at| database[at] != 0 && !inside.contains(&at))
                .collect();
            assert!(!index.is_empty());
            index
        },
    );
}

/// Where a flip in a stored frame or a blob file goes.
enum Target {
    /// A stored frame, through the test hook.
    Frame(Key),
    /// A blob file of the filesystem.
    BlobFile(String),
}

/// Write `bytes` to `target` in `store`, whose filesystem is `vfs`.
fn write_target(store: &Store, vfs: &MemVfs, target: &Target, bytes: &[u8]) {
    match target {
        Target::Frame(key) => testing::write_frame(store, *key, bytes).unwrap(),
        Target::BlobFile(name) => {
            let file = vfs.file(name);
            file.set_len(bytes.len() as u64).unwrap();
            file.write(0, bytes).unwrap();
        }
    }
}

/// A flip in a stored frame, of a record of either codec, of a transaction
/// row or of a blob record, or in a blob file, makes the reads of it fail.
/// No read in the whole store gives wrong, stale or hidden data, and each
/// flip gives at least one error.
#[test]
fn flips_in_stored_frames_and_blob_files_are_errors_and_hide_nothing() {
    let fixture = Fixture::new();
    let model = &fixture.model;
    let vfs = filesystem(&fixture.files, fixture.database());
    let store = open_vfs(vfs.clone()).unwrap();
    let keys = model
        .records
        .keys()
        .map(|&(oid, tid)| Key::Record(oid, tid))
        .chain(model.blobs.keys().map(|&(oid, tid)| Key::Blob(oid, tid)))
        .chain(model.txns.keys().map(|&tid| Key::Txn(tid)));
    let mut targets: Vec<(Target, Vec<u8>)> = keys
        .map(|key| {
            (
                Target::Frame(key),
                testing::read_frame(&store, key).unwrap(),
            )
        })
        .collect();
    targets.extend(
        fixture
            .files
            .iter()
            .filter(|(name, _)| name.ends_with(".blob"))
            .map(|(name, bytes)| (Target::BlobFile(name.clone()), bytes.clone())),
    );
    assert_eq!(
        targets.len(),
        model.records.len() + 2 * model.blobs.len() + model.txns.len()
    );

    let seed = seed();
    let mut rng = Rng::new(seed);
    let mut tally = Tally::default();
    for n in 0..flips() {
        let (target, bytes) = &targets[rng.below(targets.len())];
        let (byte, bit) = (rng.below(bytes.len()), rng.below(8));
        write_target(&store, &vfs, target, &flip(bytes, byte, bit));
        let mut reads = Reads::default();
        classify(&store, model, &mut reads);
        write_target(&store, &vfs, target, bytes);
        let what = match target {
            Target::Frame(key) => format!("flip {n}: byte {byte} bit {bit} of {key:?}"),
            Target::BlobFile(name) => format!("flip {n}: byte {byte} bit {bit} of {name}"),
        };
        assert!(reads.has(Class::Error), "{what} went unseen");
        tally.add(
            &what,
            &Outcome::Read(reads),
            &[Class::Wrong, Class::Stale, Class::Hidden],
        );
    }
    let run = format!(
        "flips in stored frames, seed {seed:#x}, {} build",
        profile()
    );
    eprintln!("bit {run}: {tally}");
    assert!(
        tally.failures.is_empty(),
        "{} {run} gave wrong, stale or hidden data. ZOE_FLIP_SEED={seed:#x} runs them again.\n{}",
        tally.failures.len(),
        tally.failures.join("\n")
    );
}

/// The tag of the key space `records`, the first byte of the checksum
/// input of a record.
const RECORDS_TAG: u8 = 1;

/// Seal `frame` again: write the checksum of its bytes as a record under
/// `key`. The frame has no typed-field slots.
fn seal(key: &[u8], frame: &mut [u8]) {
    let mut hasher = Xxh3::new();
    hasher.update(&[RECORDS_TAG]);
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
    let store = open_vfs(MemVfs::new()).unwrap();
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
