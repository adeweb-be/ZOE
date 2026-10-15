//! The layout `log`: a commit appends its records to a write-ahead log and
//! syncs only the log. A checkpoint thread folds the records of many
//! commits at once into the redb tables of the core. A load reads the
//! in-memory table of the records that are not folded yet, then the tree.
use std::collections::{BTreeMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use redb::{Database, ReadableDatabase, TableDefinition};
use xxhash_rust::xxh3::{Xxh3, xxh3_64};

use crate::frame::{
    TAG_RECORDS, TAG_TXNS, decode_frame, encode_frame, encode_payload, frame_of_payload,
    record_key, txn_oid_key, txn_payload,
};
use crate::io::{CountedFile, Io};

const RECORDS: TableDefinition<&[u8; 16], &[u8]> = TableDefinition::new("records");
const TXN_OIDS: TableDefinition<&[u8; 16], ()> = TableDefinition::new("txn_oids");
const TXNS: TableDefinition<&[u8; 8], &[u8]> = TableDefinition::new("txns");
const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");

const MAGIC: u32 = 0x5a4c_4f47;
/// A log segment rotates past this size.
const SEGMENT_BYTES: u64 = 256 << 20;

/// The records of one commit, before it has a tid.
pub struct Member {
    /// `(oid, prev serial, data)`.
    pub records: Vec<(u64, u64, Vec<u8>)>,
    /// The codec and the payload of each record, when the thread of the
    /// member encoded them before it joined the group.
    pub encoded: Option<Vec<(u8, Vec<u8>)>>,
}

impl Member {
    /// Encode the payloads on this thread, so the leader only frames them.
    pub fn precompress(mut self, compress: bool) -> Self {
        self.encoded = Some(
            self.records
                .iter()
                .map(|(_, _, data)| {
                    let (codec, payload) = encode_payload(data, compress);
                    (codec, payload.into_owned())
                })
                .collect(),
        );
        self
    }
}

/// One folded-later commit.
struct Batch {
    tid: u64,
    records: Vec<([u8; 16], Arc<[u8]>)>,
    oids: Vec<u64>,
    txn_row: Vec<u8>,
    bytes: u64,
}

struct Slot {
    tid: Mutex<Option<u64>>,
    cv: Condvar,
}

struct Segment {
    file: File,
    path: PathBuf,
    bytes: u64,
    first_tid: u64,
}

struct Log {
    current: Segment,
    /// The closed segments, with their last tid.
    closed: Vec<(PathBuf, u64)>,
    number: u64,
}

#[derive(Default, Debug, Clone)]
pub struct CpStats {
    pub folds: u64,
    pub commits: u64,
    pub records: u64,
    pub record_bytes: u64,
    pub tree_bytes: u64,
    pub fold_nanos: u64,
    pub longest_fold_nanos: u64,
}

struct CpState {
    wanted: bool,
    stop: bool,
}

pub struct LogStore {
    dir: PathBuf,
    compress: bool,
    log: Mutex<Log>,
    sequencer: Mutex<()>,
    queue: Mutex<Vec<(Member, Arc<Slot>)>>,
    memtable: RwLock<BTreeMap<[u8; 16], Arc<[u8]>>>,
    pub mem_bytes: AtomicU64,
    pending: Mutex<VecDeque<Batch>>,
    pub pending_bytes: AtomicU64,
    pub last_tid: AtomicU64,
    pub folded_tid: AtomicU64,
    pub next_oid: AtomicU64,
    db: Database,
    pub tree_io: Arc<Io>,
    cp: Mutex<CpState>,
    cp_cv: Condvar,
    folded_cv: Condvar,
    pub stats: Mutex<CpStats>,
    pub threshold: u64,
    pub max_pending: u64,
    pub stalls: AtomicU64,
    pub stall_nanos: AtomicU64,
    pub sync_nanos: AtomicU64,
    pub syncs: AtomicU64,
    pub log_bytes: AtomicU64,
    pub batches: AtomicU64,
    checkpointer: Mutex<Option<JoinHandle<()>>>,
}

fn segment_path(dir: &Path, number: u64) -> PathBuf {
    dir.join(format!("wal-{number:08}.log"))
}

fn open_segment(dir: &Path, number: u64, first_tid: u64) -> Segment {
    let path = segment_path(dir, number);
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .read(true)
        .open(&path)
        .unwrap();
    let bytes = file.metadata().unwrap().len();
    Segment {
        file,
        path,
        bytes,
        first_tid,
    }
}

impl LogStore {
    /// Open or create the store in `dir`. Replay the log segments after
    /// the folded tid. Return the store and the replay numbers: bytes,
    /// records, time.
    pub fn open(dir: &Path, compress: bool, threshold: u64, max_pending: u64) -> (Arc<LogStore>, (u64, u64, Duration)) {
        fs::create_dir_all(dir).unwrap();
        let tree_io = Arc::new(Io::default());
        let db_path = dir.join("zoe.redb");
        let existed = db_path.exists();
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&db_path)
            .unwrap();
        let backend = CountedFile {
            file: redb::backends::FileBackend::new(file).unwrap(),
            io: Some(tree_io.clone()),
        };
        let db = redb::Builder::new().create_with_backend(backend).unwrap();
        let (folded, next_oid) = if existed {
            let txn = db.begin_read().unwrap();
            let meta = txn.open_table(META).unwrap();
            let read = |key: &str| -> u64 {
                meta.get(key)
                    .unwrap()
                    .map(|v| u64::from_be_bytes(v.value().try_into().unwrap()))
                    .unwrap_or(0)
            };
            (read("last_tid"), read("next_oid").max(1))
        } else {
            let txn = db.begin_write().unwrap();
            {
                txn.open_table(RECORDS).unwrap();
                txn.open_table(TXN_OIDS).unwrap();
                txn.open_table(TXNS).unwrap();
                let mut meta = txn.open_table(META).unwrap();
                meta.insert("last_tid", &0u64.to_be_bytes()[..]).unwrap();
                meta.insert("next_oid", &1u64.to_be_bytes()[..]).unwrap();
            }
            txn.commit().unwrap();
            (0, 1)
        };
        // Replay the segments.
        let start = Instant::now();
        let mut segments: Vec<PathBuf> = fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("wal-") && n.ends_with(".log"))
            })
            .collect();
        segments.sort();
        let mut memtable = BTreeMap::new();
        let mut pending = VecDeque::new();
        let mut mem_bytes = 0u64;
        let mut last_tid = folded;
        let mut replayed_bytes = 0u64;
        let mut replayed_records = 0u64;
        let mut closed = Vec::new();
        let mut next_oid_seen = next_oid;
        for path in &segments {
            let mut bytes = Vec::new();
            File::open(path).unwrap().read_to_end(&mut bytes).unwrap();
            replayed_bytes += bytes.len() as u64;
            let mut at = 0usize;
            let mut segment_last = 0u64;
            while at + 4 <= bytes.len() {
                let Some((batch, end)) = parse_batch(&bytes[at..]) else {
                    break;
                };
                at += end;
                segment_last = batch.tid;
                if batch.tid > folded {
                    for (key, frame) in &batch.records {
                        mem_bytes += frame.len() as u64;
                        memtable.insert(*key, frame.clone());
                    }
                    for &oid in &batch.oids {
                        next_oid_seen = next_oid_seen.max(oid + 1);
                    }
                    replayed_records += batch.records.len() as u64;
                    last_tid = last_tid.max(batch.tid);
                    pending.push_back(batch);
                }
            }
            closed.push((path.clone(), segment_last));
        }
        let number = segments.len() as u64;
        let replay = (replayed_bytes, replayed_records, start.elapsed());
        let pending_bytes: u64 = pending.iter().map(|b| b.bytes).sum();
        let store = Arc::new(LogStore {
            dir: dir.to_path_buf(),
            compress,
            log: Mutex::new(Log {
                current: open_segment(dir, number, last_tid + 1),
                closed,
                number,
            }),
            sequencer: Mutex::new(()),
            queue: Mutex::new(Vec::new()),
            memtable: RwLock::new(memtable),
            mem_bytes: AtomicU64::new(mem_bytes),
            pending: Mutex::new(pending),
            pending_bytes: AtomicU64::new(pending_bytes),
            last_tid: AtomicU64::new(last_tid),
            folded_tid: AtomicU64::new(folded),
            next_oid: AtomicU64::new(next_oid_seen),
            db,
            tree_io,
            cp: Mutex::new(CpState {
                wanted: pending_bytes >= threshold,
                stop: false,
            }),
            cp_cv: Condvar::new(),
            folded_cv: Condvar::new(),
            stats: Mutex::new(CpStats::default()),
            threshold,
            max_pending,
            stalls: AtomicU64::new(0),
            stall_nanos: AtomicU64::new(0),
            sync_nanos: AtomicU64::new(0),
            syncs: AtomicU64::new(0),
            log_bytes: AtomicU64::new(0),
            batches: AtomicU64::new(0),
            checkpointer: Mutex::new(None),
        });
        let worker = store.clone();
        let handle = std::thread::Builder::new()
            .name("checkpoint".into())
            .spawn(move || worker.checkpoint_loop())
            .unwrap();
        *store.checkpointer.lock().unwrap() = Some(handle);
        (store, replay)
    }

    pub fn new_oids(&self, n: usize) -> Vec<u64> {
        let first = self.next_oid.fetch_add(n as u64, Ordering::SeqCst);
        (first..first + n as u64).collect()
    }

    /// Commit `member`. Return its tid and the time spent in this call.
    pub fn finish(&self, member: Member) -> (u64, Duration) {
        let start = Instant::now();
        let slot = Arc::new(Slot {
            tid: Mutex::new(None),
            cv: Condvar::new(),
        });
        self.queue.lock().unwrap().push((member, slot.clone()));
        // Lead the next batch, or wait for the leader that takes this
        // member. A member never blocks on the sequencer: a blocked member
        // could not see that another leader gave it its tid.
        let guard = loop {
            {
                let done = slot.tid.lock().unwrap();
                if let Some(tid) = *done {
                    return (tid, start.elapsed());
                }
                if let Ok(guard) = self.sequencer.try_lock() {
                    break guard;
                }
                let (done, _) = slot.cv.wait_timeout(done, Duration::from_micros(50)).unwrap();
                if let Some(tid) = *done {
                    return (tid, start.elapsed());
                }
            }
        };
        let batch: Vec<(Member, Arc<Slot>)> = std::mem::take(&mut *self.queue.lock().unwrap());
        if batch.is_empty() {
            drop(guard);
            let mut done = slot.tid.lock().unwrap();
            while done.is_none() {
                done = slot.cv.wait(done).unwrap();
            }
            return (done.unwrap(), start.elapsed());
        }
        // Back-pressure: wait while the checkpoint is behind.
        if self.pending_bytes.load(Ordering::SeqCst) > self.max_pending {
            let stall = Instant::now();
            let mut cp = self.cp.lock().unwrap();
            cp.wanted = true;
            self.cp_cv.notify_all();
            while self.pending_bytes.load(Ordering::SeqCst) > self.max_pending {
                cp = self.folded_cv.wait(cp).unwrap();
            }
            self.stalls.fetch_add(1, Ordering::SeqCst);
            self.stall_nanos
                .fetch_add(stall.elapsed().as_nanos() as u64, Ordering::SeqCst);
        }
        let mut last = self.last_tid.load(Ordering::SeqCst);
        let mut bytes: Vec<u8> = Vec::new();
        let mut batches: Vec<Batch> = Vec::with_capacity(batch.len());
        let mut tids = Vec::with_capacity(batch.len());
        for (member, _) in &batch {
            last += 1;
            let tid = last;
            tids.push(tid);
            let mut records = Vec::with_capacity(member.records.len());
            let mut oids = Vec::with_capacity(member.records.len());
            let mut batch_bytes = 0u64;
            let txn_row = encode_frame(
                TAG_TXNS,
                &tid.to_be_bytes(),
                0,
                &txn_payload(member.records.len() as u64, b"user", b"walproto"),
                false,
            );
            let at = bytes.len();
            bytes.extend_from_slice(&MAGIC.to_le_bytes());
            bytes.extend_from_slice(&tid.to_le_bytes());
            bytes.extend_from_slice(&(member.records.len() as u32).to_le_bytes());
            bytes.extend_from_slice(&(txn_row.len() as u32).to_le_bytes());
            bytes.extend_from_slice(&txn_row);
            for (i, (oid, prev, data)) in member.records.iter().enumerate() {
                let key = record_key(*oid, tid);
                let frame: Arc<[u8]> = match &member.encoded {
                    Some(encoded) => {
                        let (codec, payload) = &encoded[i];
                        frame_of_payload(TAG_RECORDS, &key, *prev, *codec, payload).into()
                    }
                    None => encode_frame(TAG_RECORDS, &key, *prev, data, self.compress).into(),
                };
                bytes.extend_from_slice(&oid.to_le_bytes());
                bytes.extend_from_slice(&(frame.len() as u32).to_le_bytes());
                bytes.extend_from_slice(&frame);
                batch_bytes += frame.len() as u64;
                records.push((key, frame));
                oids.push(*oid);
            }
            let sum = xxh3_64(&bytes[at..]);
            bytes.extend_from_slice(&sum.to_le_bytes());
            batches.push(Batch {
                tid,
                records,
                oids,
                txn_row,
                bytes: batch_bytes,
            });
        }
        // Append and sync the log.
        {
            let mut log = self.log.lock().unwrap();
            if log.current.bytes > SEGMENT_BYTES {
                let number = log.number + 1;
                let next = open_segment(&self.dir, number, tids[0]);
                let old = std::mem::replace(&mut log.current, next);
                log.number = number;
                log.closed.push((old.path, tids[0] - 1));
            }
            log.current.file.write_all(&bytes).unwrap();
            log.current.bytes += bytes.len() as u64;
            let sync = Instant::now();
            log.current.file.sync_data().unwrap();
            self.sync_nanos
                .fetch_add(sync.elapsed().as_nanos() as u64, Ordering::SeqCst);
            self.syncs.fetch_add(1, Ordering::SeqCst);
            self.log_bytes.fetch_add(bytes.len() as u64, Ordering::SeqCst);
            self.batches.fetch_add(1, Ordering::SeqCst);
        }
        // Publish: the table, then the tid.
        {
            let mut table = self.memtable.write().unwrap();
            let mut added = 0u64;
            for batch in &batches {
                for (key, frame) in &batch.records {
                    added += frame.len() as u64;
                    table.insert(*key, frame.clone());
                }
            }
            self.mem_bytes.fetch_add(added, Ordering::SeqCst);
        }
        let pending_bytes = {
            let mut pending = self.pending.lock().unwrap();
            let mut added = 0u64;
            for batch in batches {
                added += batch.bytes;
                pending.push_back(batch);
            }
            self.pending_bytes.fetch_add(added, Ordering::SeqCst) + added
        };
        self.last_tid.store(last, Ordering::SeqCst);
        if pending_bytes >= self.threshold {
            let mut cp = self.cp.lock().unwrap();
            cp.wanted = true;
            self.cp_cv.notify_all();
        }
        drop(guard);
        let mut mine = None;
        for ((_, other), tid) in batch.iter().zip(tids) {
            *other.tid.lock().unwrap() = Some(tid);
            other.cv.notify_all();
            if Arc::ptr_eq(other, &slot) {
                mine = Some(tid);
            }
        }
        let tid = match mine {
            Some(tid) => tid,
            None => {
                let mut guard = slot.tid.lock().unwrap();
                while guard.is_none() {
                    guard = slot.cv.wait(guard).unwrap();
                }
                guard.unwrap()
            }
        };
        (tid, start.elapsed())
    }

    /// Fold every pending commit into the tree and wait for it.
    pub fn force_checkpoint(&self) {
        {
            let mut cp = self.cp.lock().unwrap();
            cp.wanted = true;
            self.cp_cv.notify_all();
        }
        let mut cp = self.cp.lock().unwrap();
        while self.pending_bytes.load(Ordering::SeqCst) > 0 || !self.pending.lock().unwrap().is_empty() {
            cp = self.folded_cv.wait_timeout(cp, Duration::from_millis(20)).unwrap().0;
        }
    }

    fn checkpoint_loop(self: Arc<Self>) {
        loop {
            {
                let mut cp = self.cp.lock().unwrap();
                while !cp.wanted && !cp.stop {
                    cp = self.cp_cv.wait(cp).unwrap();
                }
                if cp.stop {
                    return;
                }
                cp.wanted = false;
            }
            let batches: Vec<Batch> = self.pending.lock().unwrap().drain(..).collect();
            if batches.is_empty() {
                continue;
            }
            let start = Instant::now();
            let before = self.tree_io.written.load(Ordering::SeqCst);
            let last = batches.last().unwrap().tid;
            let mut records_n = 0u64;
            let mut record_bytes = 0u64;
            {
                let txn = self.db.begin_write().unwrap();
                {
                    let mut records = txn.open_table(RECORDS).unwrap();
                    let mut index = txn.open_table(TXN_OIDS).unwrap();
                    let mut txns = txn.open_table(TXNS).unwrap();
                    for batch in &batches {
                        for ((key, frame), oid) in batch.records.iter().zip(&batch.oids) {
                            records.insert(key, &frame[..]).unwrap();
                            index.insert(&txn_oid_key(batch.tid, *oid), ()).unwrap();
                            records_n += 1;
                            record_bytes += frame.len() as u64;
                        }
                        txns.insert(&batch.tid.to_be_bytes(), &batch.txn_row[..]).unwrap();
                    }
                    let mut meta = txn.open_table(META).unwrap();
                    meta.insert("last_tid", &last.to_be_bytes()[..]).unwrap();
                    meta.insert(
                        "next_oid",
                        &self.next_oid.load(Ordering::SeqCst).to_be_bytes()[..],
                    )
                    .unwrap();
                }
                txn.commit().unwrap();
            }
            self.folded_tid.store(last, Ordering::SeqCst);
            {
                let mut table = self.memtable.write().unwrap();
                let mut removed = 0u64;
                for batch in &batches {
                    for (key, frame) in &batch.records {
                        if table.remove(key).is_some() {
                            removed += frame.len() as u64;
                        }
                    }
                }
                self.mem_bytes.fetch_sub(removed, Ordering::SeqCst);
            }
            // Remove the segments that the fold covers.
            {
                let mut log = self.log.lock().unwrap();
                let (gone, kept): (Vec<_>, Vec<_>) =
                    log.closed.drain(..).partition(|(_, segment_last)| *segment_last <= last);
                log.closed = kept;
                for (path, _) in gone {
                    let _ = fs::remove_file(path);
                }
                let _ = log.current.first_tid;
            }
            let nanos = start.elapsed().as_nanos() as u64;
            let after = self.tree_io.written.load(Ordering::SeqCst);
            {
                let mut stats = self.stats.lock().unwrap();
                stats.folds += 1;
                stats.commits += batches.len() as u64;
                stats.records += records_n;
                stats.record_bytes += record_bytes;
                stats.tree_bytes += after - before;
                stats.fold_nanos += nanos;
                stats.longest_fold_nanos = stats.longest_fold_nanos.max(nanos);
            }
            // The numbers are complete before a waiter can see the fold.
            let folded_bytes: u64 = batches.iter().map(|b| b.bytes).sum();
            let _cp = self.cp.lock().unwrap();
            self.pending_bytes.fetch_sub(folded_bytes, Ordering::SeqCst);
            self.folded_cv.notify_all();
        }
    }

    /// Load the record of `oid` before `tid`: the table first, then the
    /// tree. Return the length of the data.
    pub fn load_before(&self, oid: u64, tid: u64) -> Option<usize> {
        let start = record_key(oid, 0);
        let end = record_key(oid, tid);
        {
            let table = self.memtable.read().unwrap();
            if let Some((key, frame)) = table.range(start..end).next_back() {
                return Some(decode_frame(TAG_RECORDS, key, frame).len());
            }
        }
        let txn = self.db.begin_read().unwrap();
        let records = txn.open_table(RECORDS).unwrap();
        let mut range = records.range::<&[u8; 16]>(&start..&end).unwrap();
        let entry = range.next_back()?.unwrap();
        Some(decode_frame(TAG_RECORDS, entry.0.value(), entry.1.value()).len())
    }

    pub fn memtable_len(&self) -> usize {
        self.memtable.read().unwrap().len()
    }

    /// Stop the checkpoint thread, after it folded everything when `fold`
    /// is true, and at once otherwise: the log then holds what the next
    /// open replays.
    pub fn close(&self, fold: bool) {
        if fold {
            self.force_checkpoint();
        }
        {
            let mut cp = self.cp.lock().unwrap();
            cp.stop = true;
            self.cp_cv.notify_all();
        }
        if let Some(handle) = self.checkpointer.lock().unwrap().take() {
            handle.join().unwrap();
        }
    }
}

/// Parse one batch record at the start of `bytes`. Return it and its
/// length, or `None` for a torn or damaged record.
fn parse_batch(bytes: &[u8]) -> Option<(Batch, usize)> {
    let u32_at = |at: usize| -> Option<u32> {
        Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
    };
    let u64_at = |at: usize| -> Option<u64> {
        Some(u64::from_le_bytes(bytes.get(at..at + 8)?.try_into().ok()?))
    };
    if u32_at(0)? != MAGIC {
        return None;
    }
    let tid = u64_at(4)?;
    let n = u32_at(12)? as usize;
    let txn_len = u32_at(16)? as usize;
    let mut at = 20;
    let txn_row = bytes.get(at..at + txn_len)?.to_vec();
    at += txn_len;
    let mut records = Vec::with_capacity(n);
    let mut oids = Vec::with_capacity(n);
    let mut total = 0u64;
    for _ in 0..n {
        let oid = u64_at(at)?;
        let len = u32_at(at + 8)? as usize;
        at += 12;
        let frame: Arc<[u8]> = bytes.get(at..at + len)?.into();
        at += len;
        total += len as u64;
        records.push((record_key(oid, tid), frame));
        oids.push(oid);
    }
    let sum = u64_at(at)?;
    let mut h = Xxh3::new();
    h.update(&bytes[..at]);
    if h.digest() != sum {
        return None;
    }
    Some((
        Batch {
            tid,
            records,
            oids,
            txn_row,
            bytes: total,
        },
        at + 8,
    ))
}

