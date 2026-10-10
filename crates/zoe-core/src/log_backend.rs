//! An append-only log backend with an in-memory index. It is measurement
//! code for the record-store spike, not a production backend.
//!
//! The log is one file. It starts with a 16-byte magic. Then come the
//! entries, in commit order. An entry is a kind byte, the key of the row,
//! then the frame of `format`:
//!
//! | Kind | Key | Frame payload |
//! | --- | --- | --- |
//! | `R` | `record_key(oid, tid)`, 16 bytes | the pickle |
//! | `T` | `txn_key(tid)`, 8 bytes | the transaction row |
//!
//! A commit appends its records, then its transaction row, in one write,
//! and syncs the file once. The transaction row is the commit marker. One
//! write transaction can hold several transactions, each with its records
//! and then its row, as a group commit writes them.
//!
//! Open scans the whole log, checks every frame, and builds the index
//! `oid -> [(tid, offset, length)]`. A transaction counts only when all
//! its records and its row are whole and their count matches. The scan
//! stops at the first entry that fails, and open truncates the file
//! there. Only an unacknowledged commit can be after that point, because
//! a commit returns after its sync.

use std::any::Any;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, RwLock};

use redb::StorageBackend;

use crate::backend::{
    Backend, Bytes, PackStats, Record, RecordMeta, TxnIter, TxnMeta, TxnStatus, WriteTxn,
};
use crate::error::{Result, ZoeError};
use crate::format;
use crate::types::{Oid, Tid};
use crate::vfs::Vfs;

/// The name of the log file in the store directory.
pub const LOG: &str = "zoe.log";

/// The name under which a new log is written before it is renamed.
const NEW_LOG: &str = "zoe.log.new";

/// The first bytes of the log file.
const MAGIC: &[u8; 16] = b"ZOE log format 1";

/// The kind byte of a record entry.
const RECORD: u8 = b'R';

/// The kind byte of a transaction-row entry.
const TXN: u8 = b'T';

/// How many bytes the scan reads at a time.
const SCAN_CHUNK: usize = 4 << 20;

/// Where one frame is in the log.
#[derive(Clone, Copy, Debug)]
struct Slot {
    tid: Tid,
    /// The offset of the frame, after the kind byte and the key.
    offset: u64,
    /// The length of the frame.
    len: u32,
}

/// The in-memory index. It holds only durable entries.
#[derive(Default)]
struct Index {
    /// The versions of each oid, in tid order.
    records: HashMap<Oid, Vec<Slot>>,
    /// The transaction rows, in tid order.
    txns: Vec<Slot>,
}

/// A store in one append-only log file.
pub struct LogBackend {
    file: Box<dyn StorageBackend>,
    index: RwLock<Index>,
    /// The end of the log. Its lock lets one write transaction at a time
    /// append.
    end: Mutex<u64>,
    last_tid: AtomicU64,
    next_oid: Oid,
    /// The directory. It holds the single-writer lock while the log is
    /// open.
    _vfs: Box<dyn Any + Send + Sync>,
}

impl LogBackend {
    /// Open the log in `vfs`, and create it if necessary.
    ///
    /// A new log is written under a temporary name and renamed into place
    /// after it is durable.
    pub fn open<V: Vfs>(vfs: V) -> Result<Self> {
        if !vfs.exists(LOG)? {
            if vfs.exists(NEW_LOG)? {
                vfs.remove(NEW_LOG)?;
            }
            let file = vfs.open(NEW_LOG)?;
            file.write(0, MAGIC)?;
            file.sync_data()?;
            drop(file);
            vfs.rename(NEW_LOG, LOG)?;
            vfs.sync_dir()?;
        }
        let file: Box<dyn StorageBackend> = Box::new(vfs.open(LOG)?);
        let scan = scan(file.as_ref())?;
        if scan.valid_end < file.len()? {
            file.set_len(scan.valid_end)?;
            file.sync_data()?;
        }
        Ok(Self {
            file,
            index: RwLock::new(scan.index),
            end: Mutex::new(scan.valid_end),
            last_tid: AtomicU64::new(scan.last_tid.0),
            next_oid: scan.next_oid,
            _vfs: Box::new(vfs),
        })
    }

    /// Return the next oid that no committed record uses.
    pub fn next_oid(&self) -> Oid {
        self.next_oid
    }

    /// Return the length of the log in bytes.
    pub fn len(&self) -> u64 {
        *self.lock_end()
    }

    /// Tell if the log holds no entry.
    pub fn is_empty(&self) -> bool {
        self.len() == MAGIC.len() as u64
    }

    fn lock_end(&self) -> MutexGuard<'_, u64> {
        self.end.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn read_index(&self) -> std::sync::RwLockReadGuard<'_, Index> {
        self.index.read().unwrap_or_else(|e| e.into_inner())
    }

    /// Read and check the frame of the record of `oid` in `slot`.
    fn record(&self, oid: Oid, slot: Slot) -> Result<Record> {
        let mut bytes = vec![0; slot.len as usize];
        self.file.read(slot.offset, &mut bytes)?;
        let key = format::record_key(oid, slot.tid);
        let frame = format::decode_frame(&key, &bytes)
            .map_err(|_| ZoeError::CorruptRecord { oid, tid: slot.tid })?;
        Ok(Record {
            tid: slot.tid,
            prev: frame.prev,
            data: frame.payload.to_vec(),
        })
    }

    /// Return the slot of the newest version of `oid` that `keep` accepts.
    fn find(&self, oid: Oid, keep: impl Fn(Tid) -> bool) -> Option<Slot> {
        let index = self.read_index();
        let versions = index.records.get(&oid)?;
        versions.iter().rev().find(|slot| keep(slot.tid)).copied()
    }
}

impl Backend for LogBackend {
    fn load_before(&self, oid: Oid, tid: Tid) -> Result<Option<Record>> {
        match self.find(oid, |t| t < tid) {
            Some(slot) => self.record(oid, slot).map(Some),
            None => Ok(None),
        }
    }

    fn load_serial(&self, oid: Oid, tid: Tid) -> Result<Option<Bytes>> {
        match self.find(oid, |t| t == tid) {
            Some(slot) => Ok(Some(self.record(oid, slot)?.data)),
            None => Ok(None),
        }
    }

    fn last_tid(&self) -> Tid {
        Tid(self.last_tid.load(Ordering::Acquire))
    }

    fn begin_write(&self) -> Result<Box<dyn WriteTxn + '_>> {
        Ok(Box::new(LogWrite {
            backend: self,
            end: self.lock_end(),
            buf: Vec::new(),
            records: Vec::new(),
            txns: Vec::new(),
            open_records: 0,
        }))
    }

    fn iter_txns(&self, from: Tid, to: Tid) -> TxnIter<'_> {
        let slots: Vec<Slot> = self
            .read_index()
            .txns
            .iter()
            .filter(|slot| from <= slot.tid && slot.tid <= to)
            .copied()
            .collect();
        Box::new(slots.into_iter().map(move |slot| {
            let mut bytes = vec![0; slot.len as usize];
            self.file.read(slot.offset, &mut bytes)?;
            let corrupt = || ZoeError::CorruptTxn { tid: slot.tid };
            let frame =
                format::decode_frame(&format::txn_key(slot.tid), &bytes).map_err(|_| corrupt())?;
            format::decode_txn(slot.tid, frame.payload).map_err(|_| corrupt())
        }))
    }

    fn history(&self, oid: Oid, limit: usize) -> Result<Vec<RecordMeta>> {
        let versions: Vec<Slot> = self
            .read_index()
            .records
            .get(&oid)
            .map(|v| v.iter().rev().take(limit).copied().collect())
            .unwrap_or_default();
        versions
            .into_iter()
            .map(|slot| {
                let record = self.record(oid, slot)?;
                Ok(RecordMeta {
                    tid: record.tid,
                    prev: record.prev,
                    size: record.data.len() as u64,
                })
            })
            .collect()
    }

    fn pack(&self, _before: Tid, _reachable: &dyn Fn(Oid, Tid) -> bool) -> Result<PackStats> {
        Err(ZoeError::Backend("the log backend has no pack".into()))
    }

    fn sync(&self) -> Result<()> {
        // Every commit is durable when it returns.
        Ok(())
    }
}

/// One write transaction of the log. It holds the end of the log, so no
/// other write transaction appends until it ends.
struct LogWrite<'a> {
    backend: &'a LogBackend,
    end: MutexGuard<'a, u64>,
    /// The entries to append, in order.
    buf: Vec<u8>,
    /// The staged records: oid and slot, with offsets relative to `buf`.
    records: Vec<(Oid, Slot)>,
    /// The staged transaction rows, with offsets relative to `buf`.
    txns: Vec<Slot>,
    /// The number of staged records that no transaction row covers yet.
    open_records: usize,
}

impl LogWrite<'_> {
    /// Append one entry to `buf` and return its slot, relative to `buf`.
    fn push(&mut self, kind: u8, key: &[u8], tid: Tid, frame: &[u8]) -> Slot {
        self.buf.push(kind);
        self.buf.extend_from_slice(key);
        let offset = self.buf.len() as u64;
        self.buf.extend_from_slice(frame);
        Slot {
            tid,
            offset,
            len: u32::try_from(frame.len()).expect("a frame fits in 4 GiB"),
        }
    }
}

impl WriteTxn for LogWrite<'_> {
    fn put_record(&mut self, oid: Oid, tid: Tid, prev: Tid, data: &[u8]) -> Result<()> {
        let last = self.txns.last().map_or(self.backend.last_tid(), |t| t.tid);
        if tid <= last {
            return Err(ZoeError::Protocol("a record tid is not after the last one"));
        }
        let key = format::record_key(oid, tid);
        let frame = format::encode_frame(&key, format::MAIN_BRANCH, prev, data);
        let slot = self.push(RECORD, &key, tid, &frame);
        self.records.push((oid, slot));
        self.open_records += 1;
        Ok(())
    }

    fn put_txn(&mut self, meta: &TxnMeta) -> Result<()> {
        debug_assert_eq!(meta.status, TxnStatus::Committed);
        let open = &self.records[self.records.len() - self.open_records..];
        if open.iter().any(|(_, slot)| slot.tid != meta.tid) {
            return Err(ZoeError::Protocol("a record has another tid"));
        }
        if meta.oid_count != open.len() as u64 {
            return Err(ZoeError::Protocol("the oid count is not the record count"));
        }
        let key = format::txn_key(meta.tid);
        let payload = format::encode_txn(meta);
        let frame = format::encode_frame(&key, format::MAIN_BRANCH, Tid::ZERO, &payload);
        let slot = self.push(TXN, &key, meta.tid, &frame);
        self.txns.push(slot);
        self.open_records = 0;
        Ok(())
    }

    fn commit(mut self: Box<Self>) -> Result<()> {
        let Some(&last) = self.txns.last() else {
            return Err(ZoeError::Protocol("commit without a transaction row"));
        };
        if self.open_records > 0 {
            return Err(ZoeError::Protocol("records after the last transaction row"));
        }
        let base = *self.end;
        let file = &self.backend.file;
        file.write(base, &self.buf)?;
        file.sync_data()?;

        let mut index = self
            .backend
            .index
            .write()
            .unwrap_or_else(|e| e.into_inner());
        for (oid, slot) in self.records.drain(..) {
            let slot = Slot {
                offset: base + slot.offset,
                ..slot
            };
            index.records.entry(oid).or_default().push(slot);
        }
        for txn in self.txns.drain(..) {
            index.txns.push(Slot {
                offset: base + txn.offset,
                ..txn
            });
        }
        drop(index);
        *self.end = base + self.buf.len() as u64;
        self.backend.last_tid.store(last.tid.0, Ordering::Release);
        Ok(())
    }
}

/// What the scan of a log finds.
struct Scan {
    index: Index,
    /// The end of the last whole transaction.
    valid_end: u64,
    last_tid: Tid,
    next_oid: Oid,
}

/// A reader that keeps a window of the file in memory.
struct Reader<'f> {
    file: &'f dyn StorageBackend,
    file_len: u64,
    /// The bytes from `start`.
    window: Vec<u8>,
    start: u64,
}

impl Reader<'_> {
    /// Return the `n` bytes at `offset`, or `None` past the end of the
    /// file. `offset` never goes back before an earlier call.
    fn get(&mut self, offset: u64, n: usize) -> Result<Option<&[u8]>> {
        let end = offset + n as u64;
        if end > self.file_len {
            return Ok(None);
        }
        if offset < self.start || end > self.start + self.window.len() as u64 {
            let len = (n.max(SCAN_CHUNK) as u64).min(self.file_len - offset) as usize;
            self.window.resize(len, 0);
            self.file.read(offset, &mut self.window)?;
            self.start = offset;
        }
        let from = (offset - self.start) as usize;
        Ok(Some(&self.window[from..from + n]))
    }
}

/// Scan the log and rebuild the index.
fn scan(file: &dyn StorageBackend) -> Result<Scan> {
    let mut reader = Reader {
        file,
        file_len: file.len()?,
        window: Vec::new(),
        start: 0,
    };
    if reader.get(0, MAGIC.len())? != Some(&MAGIC[..]) {
        return Err(ZoeError::Format("not a ZOE log".to_owned()));
    }

    let mut index = Index::default();
    let mut pending: Vec<(Oid, Slot)> = Vec::new();
    let mut offset = MAGIC.len() as u64;
    let mut valid_end = offset;
    let mut last_tid = Tid::ZERO;
    let mut max_oid = 0;
    while let Some(entry) = next_entry(&mut reader, offset)? {
        offset = entry.slot.offset + u64::from(entry.slot.len);
        match entry.oid {
            Some(oid) => {
                if entry.slot.tid <= last_tid
                    || pending
                        .first()
                        .is_some_and(|(_, s)| s.tid != entry.slot.tid)
                {
                    break;
                }
                pending.push((oid, entry.slot));
            }
            None => {
                let whole = entry.slot.tid > last_tid
                    && pending.iter().all(|(_, s)| s.tid == entry.slot.tid)
                    && entry.oid_count == pending.len() as u64;
                if !whole {
                    break;
                }
                for (oid, slot) in pending.drain(..) {
                    max_oid = max_oid.max(oid.0);
                    index.records.entry(oid).or_default().push(slot);
                }
                index.txns.push(entry.slot);
                last_tid = entry.slot.tid;
                valid_end = offset;
            }
        }
    }
    Ok(Scan {
        index,
        valid_end,
        last_tid,
        next_oid: Oid(max_oid + 1),
    })
}

/// One entry that the scan read and checked.
struct Entry {
    /// The oid of a record, or `None` for a transaction row.
    oid: Option<Oid>,
    slot: Slot,
    /// The oid count of a transaction row.
    oid_count: u64,
}

/// Read and check the entry at `offset`. Return `None` when the bytes
/// there are not a whole, valid entry.
fn next_entry(reader: &mut Reader<'_>, offset: u64) -> Result<Option<Entry>> {
    let Some(&[kind]) = reader.get(offset, 1)? else {
        return Ok(None);
    };
    let key_len = match kind {
        RECORD => 16,
        TXN => 8,
        _ => return Ok(None),
    };
    let frame_offset = offset + 1 + key_len as u64;
    let Some(header) = reader.get(frame_offset, format::HEADER_LEN)? else {
        return Ok(None);
    };
    let payload_len = u32::from_le_bytes(header[16..20].try_into().unwrap()) as usize;
    let slots_len = usize::from(u16::from_le_bytes(header[20..22].try_into().unwrap()));
    let frame_len = format::HEADER_LEN + slots_len + payload_len;
    let Some(bytes) = reader.get(offset + 1, key_len + frame_len)? else {
        return Ok(None);
    };
    let (key, frame) = bytes.split_at(key_len);
    let Ok(decoded) = format::decode_frame(key, frame) else {
        return Ok(None);
    };
    let len = frame_len as u32;
    if kind == RECORD {
        let (oid, tid) = format::split_record_key(key.try_into().unwrap());
        return Ok(Some(Entry {
            oid: Some(oid),
            slot: Slot {
                tid,
                offset: frame_offset,
                len,
            },
            oid_count: 0,
        }));
    }
    let tid = Tid(u64::from_be_bytes(key.try_into().unwrap()));
    let Ok(row) = format::decode_txn(tid, decoded.payload) else {
        return Ok(None);
    };
    Ok(Some(Entry {
        oid: None,
        slot: Slot {
            tid,
            offset: frame_offset,
            len,
        },
        oid_count: row.oid_count,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use crate::vfs::MemVfs;
    use crate::{SystemClock, ZoeError};

    fn open_store(vfs: MemVfs) -> Store {
        let log = LogBackend::open(vfs).unwrap();
        let next = log.next_oid();
        Store::from_backend(Box::new(log), next, SystemClock)
    }

    fn commit(store: &Store, oids: &[(Oid, Tid)], data: &[u8]) -> Tid {
        let mut txn = store.tpc_begin(b"user", b"", b"").unwrap();
        for &(oid, serial) in oids {
            txn.store(oid, serial, data).unwrap();
        }
        txn.vote().unwrap();
        txn.finish().unwrap()
    }

    #[test]
    fn commits_load_after_reopen() {
        let vfs = MemVfs::new();
        let (a, b, first, second) = {
            let store = open_store(vfs.clone());
            let (a, b) = (store.new_oid().unwrap(), store.new_oid().unwrap());
            let first = commit(&store, &[(a, Tid::ZERO), (b, Tid::ZERO)], b"one");
            let second = commit(&store, &[(a, first)], b"two");
            assert_eq!(store.load(a).unwrap(), (b"two".to_vec(), second));
            (a, b, first, second)
        };
        let store = open_store(vfs);
        assert_eq!(store.load(a).unwrap(), (b"two".to_vec(), second));
        assert_eq!(store.load(b).unwrap(), (b"one".to_vec(), first));
        assert_eq!(store.last_transaction(), second);
        assert!(store.new_oid().unwrap() > b);

        let log = LogBackend::open(MemVfs::new()).unwrap();
        assert!(log.is_empty());
    }

    #[test]
    fn history_and_the_transaction_log_are_read_back() {
        let vfs = MemVfs::new();
        let store = open_store(vfs.clone());
        let oid = store.new_oid().unwrap();
        let first = commit(&store, &[(oid, Tid::ZERO)], b"one");
        let second = commit(&store, &[(oid, first)], b"three");
        drop(store);

        let log = LogBackend::open(vfs).unwrap();
        let history = log.history(oid, 10).unwrap();
        assert_eq!(
            history,
            vec![
                RecordMeta {
                    tid: second,
                    prev: first,
                    size: 5
                },
                RecordMeta {
                    tid: first,
                    prev: Tid::ZERO,
                    size: 3
                },
            ]
        );
        let rows: Vec<_> = log.iter_txns(Tid::ZERO, Tid::MAX).collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].as_ref().unwrap().tid, second);
        assert_eq!(log.load_serial(oid, first).unwrap(), Some(b"one".to_vec()));
        assert_eq!(log.load_before(oid, second).unwrap().unwrap().tid, first);
    }

    #[test]
    fn a_torn_tail_is_dropped_at_open() {
        let vfs = MemVfs::new();
        let (oid, tid) = {
            let store = open_store(vfs.clone());
            let oid = store.new_oid().unwrap();
            (oid, commit(&store, &[(oid, Tid::ZERO)], b"kept"))
        };
        let file = vfs.file(LOG);
        let len = file.len().unwrap();
        file.write(len, b"Rgarbage").unwrap();

        let store = open_store(vfs.clone());
        assert_eq!(vfs.file(LOG).len().unwrap(), len);
        assert_eq!(store.load(oid).unwrap(), (b"kept".to_vec(), tid));
        let next = commit(&store, &[(oid, tid)], b"next");
        drop(store);
        assert_eq!(open_store(vfs).load(oid).unwrap(), (b"next".to_vec(), next));
    }

    /// Every crash image opens, and keeps every acknowledged commit whole.
    #[test]
    fn an_acknowledged_commit_survives_every_crash_image() {
        let vfs = MemVfs::new();
        let (oids, commits) = {
            let store = open_store(vfs.clone());
            let oids = [store.new_oid().unwrap(), store.new_oid().unwrap()];
            let first = commit(&store, &oids.map(|oid| (oid, Tid::ZERO)), b"first");
            vfs.mark("first");
            let second = commit(&store, &oids.map(|oid| (oid, first)), b"second");
            vfs.mark("second");
            (oids, [("first", first), ("second", second)])
        };

        let images = vfs.crash_images();
        assert!(images.len() > 10);
        for image in &images {
            let store = open_store(image.vfs());
            let acknowledged = commits.iter().rfind(|(mark, _)| image.is_after(mark));
            for oid in oids {
                let found = match store.load(oid) {
                    Ok((data, tid)) => commits.iter().find(|(_, t)| *t == tid).map(|c| (c, data)),
                    Err(ZoeError::NotFound { .. }) => None,
                    Err(e) => panic!("{image:?}: {e}"),
                };
                if let Some(((mark, _), data)) = &found {
                    assert_eq!(data, mark.as_bytes(), "{image:?}");
                }
                if let Some((mark, tid)) = acknowledged {
                    let kept = found.is_some_and(|((_, t), _)| t >= tid);
                    assert!(kept, "{image:?} lost the {mark} commit");
                }
            }
        }
    }

    /// One write transaction holds two transactions, as a group commit
    /// writes them. Both load after a reopen.
    #[test]
    fn a_group_of_transactions_commits_in_one_write() {
        let vfs = MemVfs::new();
        let log = LogBackend::open(vfs.clone()).unwrap();
        let row = |tid: Tid| TxnMeta {
            tid,
            user: Vec::new(),
            description: Vec::new(),
            extension: Vec::new(),
            oid_count: 1,
            status: TxnStatus::Committed,
        };
        let mut write = log.begin_write().unwrap();
        write.put_record(Oid(1), Tid(5), Tid::ZERO, b"a").unwrap();
        write.put_txn(&row(Tid(5))).unwrap();
        write.put_record(Oid(2), Tid(6), Tid::ZERO, b"b").unwrap();
        assert!(write.put_txn(&row(Tid(7))).is_err());
        write.put_txn(&row(Tid(6))).unwrap();
        write.commit().unwrap();
        drop(log);

        let log = LogBackend::open(vfs).unwrap();
        assert_eq!(log.last_tid(), Tid(6));
        assert_eq!(
            log.load_serial(Oid(1), Tid(5)).unwrap(),
            Some(b"a".to_vec())
        );
        assert_eq!(
            log.load_serial(Oid(2), Tid(6)).unwrap(),
            Some(b"b".to_vec())
        );
        assert_eq!(log.iter_txns(Tid::ZERO, Tid::MAX).count(), 2);
    }

    #[test]
    fn a_file_that_is_not_a_log_is_refused() {
        let vfs = MemVfs::new();
        vfs.file(LOG).write(0, b"something else entirely").unwrap();
        assert!(matches!(
            LogBackend::open(vfs).err(),
            Some(ZoeError::Format(_))
        ));
    }
}
