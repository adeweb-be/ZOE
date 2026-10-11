//! The redb backend.
//!
//! Five tables hold the key spaces of the data model: `records`,
//! `txn_oids`, `blobs`, `txns` and `meta`. `format` says how keys and
//! values are written. `txn_oids` is the transaction index: `put_record`
//! writes the index key of each record in the write transaction of the
//! record, and `remove_record` removes the key with the record.
//! `initialise` creates all five, so a new store has an empty
//! `blobs` table. A store written before the `blobs` table has none until
//! its first blob commit creates it. Until then it reads as a store
//! without blob records.

use std::collections::BTreeMap;
use std::ops::Bound::{Excluded, Included, Unbounded};
use std::ops::RangeBounds;
use std::sync::atomic::{AtomicU64, Ordering};

use redb::{
    Database, ReadOnlyDatabase, ReadTransaction, ReadableDatabase, ReadableTable, TableDefinition,
};

use crate::backend::{
    Backend, BlobRecord, Bytes, PackStats, Record, RecordEncoding, RecordMeta, TxnIter, TxnMeta,
    TxnRecords, TxnStatus, WriteTxn,
};
use crate::error::{Result, ZoeError};
use crate::format::{self, Compression};
use crate::types::{Oid, Tid};

const RECORDS: TableDefinition<&[u8; 16], &[u8]> = TableDefinition::new("records");
const TXN_OIDS: TableDefinition<&[u8; 16], ()> = TableDefinition::new("txn_oids");
const BLOBS: TableDefinition<&[u8; 16], &[u8]> = TableDefinition::new("blobs");
const TXNS: TableDefinition<&[u8; 8], &[u8]> = TableDefinition::new("txns");
const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");

const META_FORMAT: &str = "format";
const META_LAST_TID: &str = "last_tid";
const META_NEXT_OID: &str = "next_oid";
const META_PACK_WATERMARK: &str = "pack_watermark";

/// Wrap a redb error.
fn backend(error: impl Into<redb::Error>) -> ZoeError {
    ZoeError::Backend(Box::new(error.into()))
}

/// Return the error for the frame of `row` that `decode_frame` refused.
/// A frame of a newer format is a format error. Any other refusal is
/// `corrupt`.
fn refused(error: format::FrameError, row: &str, corrupt: ZoeError) -> ZoeError {
    match error {
        format::FrameError::Unsupported { offset, value } => ZoeError::Format(format!(
            "{row}: header byte {offset} is {value:#04x}, which this build cannot read"
        )),
        _ => corrupt,
    }
}

/// Check that `key`, which a scan of `records` gave to a read of `oid`, is
/// inside `range`. A key outside it raises `CorruptRecord` with `oid` and
/// the tid of the key.
///
/// redb seeks each end of a scan through the branch pages, and it checks
/// only the bound of the other end. So a damaged branch page can move the
/// seek and give a key outside the range. The frame of that key is intact
/// and passes its checksum, so only this check stops it.
fn inside(oid: Oid, key: &[u8; 16], range: impl RangeBounds<[u8; 16]>) -> Result<()> {
    if range.contains(key) {
        return Ok(());
    }
    let (_, tid) = format::split_record_key(key);
    Err(ZoeError::CorruptRecord { oid, tid })
}

/// Check that `key`, which a scan of `txns` gave, is inside `range`, as
/// `inside` does for `records`. A key outside it raises `CorruptTxn` with
/// the tid of the key.
fn inside_txns(key: &[u8; 8], range: impl RangeBounds<[u8; 8]>) -> Result<()> {
    if range.contains(key) {
        return Ok(());
    }
    let tid = Tid(u64::from_be_bytes(*key));
    Err(ZoeError::CorruptTxn { tid })
}

/// Check that `key`, which a scan of the transaction index gave, is inside
/// `range`, as `inside` does for `records`. A key outside it raises
/// `CorruptTxn` with the tid of the key.
fn inside_index(key: &[u8; 16], range: impl RangeBounds<[u8; 16]>) -> Result<()> {
    if range.contains(key) {
        return Ok(());
    }
    let (tid, _) = format::split_txn_oid_key(key);
    Err(ZoeError::CorruptTxn { tid })
}

/// The redb database of a store.
enum Db {
    /// The database is open for reads and writes.
    ReadWrite(Database),
    /// The database file is open read-only. Every write fails.
    ReadOnly(ReadOnlyDatabase),
}

impl Db {
    fn begin_read(&self) -> std::result::Result<ReadTransaction, redb::TransactionError> {
        match self {
            Db::ReadWrite(db) => db.begin_read(),
            Db::ReadOnly(db) => db.begin_read(),
        }
    }
}

/// A store in one redb database.
pub(crate) struct RedbBackend {
    db: Db,
    last_tid: AtomicU64,
    /// How new records are written.
    compression: Compression,
}

impl RedbBackend {
    /// Write the tables and the `meta` rows of an empty store.
    pub(crate) fn initialise(db: &Database) -> Result<()> {
        let txn = db.begin_write().map_err(backend)?;
        {
            txn.open_table(RECORDS).map_err(backend)?;
            txn.open_table(TXN_OIDS).map_err(backend)?;
            txn.open_table(BLOBS).map_err(backend)?;
            txn.open_table(TXNS).map_err(backend)?;
            let mut meta = txn.open_table(META).map_err(backend)?;
            meta.insert(META_FORMAT, format::FORMAT_NAME)
                .map_err(backend)?;
            meta.insert(META_LAST_TID, &format::encode_u64(0)[..])
                .map_err(backend)?;
            meta.insert(META_NEXT_OID, &format::encode_u64(1)[..])
                .map_err(backend)?;
        }
        txn.commit().map_err(backend)
    }

    /// Open a store that `initialise` wrote, and check its format.
    pub(crate) fn open(db: Database) -> Result<Self> {
        Self::open_db(Db::ReadWrite(db))
    }

    /// Open a store that `initialise` wrote, for reads only, and check its
    /// format. `begin_write` fails with `ReadOnly`.
    pub(crate) fn open_read_only(db: ReadOnlyDatabase) -> Result<Self> {
        Self::open_db(Db::ReadOnly(db))
    }

    /// Open the store in `db` and check its format.
    ///
    /// A store without the transaction index is not in this format. Its
    /// records have no index keys, so the open fails with `Format`.
    ///
    /// The last tid is the greater of `last_tid` in `meta` and the tid of
    /// the last transaction row. So a damaged `last_tid` cannot give a tid
    /// a second time. The row counts only when its frame passes the
    /// checksum under its key. So a damaged key cannot raise the last tid.
    fn open_db(db: Db) -> Result<Self> {
        let last_tid = {
            let txn = db.begin_read().map_err(backend)?;
            let meta = txn.open_table(META).map_err(backend)?;
            let read = |key: &str| -> Result<Option<Vec<u8>>> {
                Ok(meta.get(key).map_err(backend)?.map(|v| v.value().to_vec()))
            };
            let format_name = read(META_FORMAT)?.unwrap_or_default();
            if format_name != format::FORMAT_NAME {
                return Err(ZoeError::Format(
                    String::from_utf8_lossy(&format_name).into_owned(),
                ));
            }
            match txn.open_table(TXN_OIDS) {
                Ok(_) => {}
                Err(redb::TableError::TableDoesNotExist(_)) => {
                    return Err(ZoeError::Format("no transaction index".to_owned()));
                }
                Err(e) => return Err(backend(e)),
            }
            let stored = read(META_LAST_TID)?
                .as_deref()
                .and_then(format::decode_u64)
                .ok_or_else(|| ZoeError::Format("no last tid".to_owned()))?;
            let txns = txn.open_table(TXNS).map_err(backend)?;
            let last_row = txns.last().map_err(backend)?.filter(|(key, value)| {
                // A frame of a newer format passed the checksum too.
                matches!(
                    format::decode_frame(key.value(), value.value()),
                    Ok(_) | Err(format::FrameError::Unsupported { .. })
                )
            });
            stored.max(last_row.map_or(0, |(key, _)| u64::from_be_bytes(*key.value())))
        };
        Ok(Self {
            db,
            last_tid: AtomicU64::new(last_tid),
            compression: Compression::default(),
        })
    }

    /// Write new records with `compression`. The backend reads records of
    /// every codec, whatever this setting.
    pub(crate) fn with_compression(self, compression: Compression) -> Self {
        Self {
            compression,
            ..self
        }
    }

    /// Return the next oid that no committed record uses.
    pub(crate) fn next_oid(&self) -> Result<Oid> {
        let txn = self.db.begin_read().map_err(backend)?;
        let meta = txn.open_table(META).map_err(backend)?;
        let value = meta.get(META_NEXT_OID).map_err(backend)?;
        value
            .and_then(|v| format::decode_u64(v.value()))
            .map(Oid)
            .ok_or_else(|| ZoeError::Format("no next oid".to_owned()))
    }

    /// Read the record frame stored under `key` for `oid`.
    fn record(oid: Oid, key: &[u8; 16], bytes: &[u8]) -> Result<Record> {
        let (tid, frame) = Self::frame(oid, key, bytes)?;
        Ok(Record {
            tid,
            prev: frame.prev,
            data: frame.data.into_owned(),
        })
    }

    /// Decode and verify the transaction row stored under `key`.
    fn txn_row(key: &[u8; 8], bytes: &[u8]) -> Result<TxnMeta> {
        let tid = Tid(u64::from_be_bytes(*key));
        let corrupt = ZoeError::CorruptTxn { tid };
        format::decode_frame(key, bytes)
            .and_then(|frame| format::decode_txn(tid, &frame.data))
            .map_err(|e| refused(e, &format!("transaction row {tid:?}"), corrupt))
    }

    /// Decode and verify the record frame stored under `key` for `oid`.
    /// Return the tid of the key and the frame.
    fn frame<'a>(oid: Oid, key: &[u8; 16], bytes: &'a [u8]) -> Result<(Tid, format::Frame<'a>)> {
        let (_, tid) = format::split_record_key(key);
        let frame = format::decode_frame(key, bytes).map_err(|e| {
            let row = format!("record of oid {oid:?}, tid {tid:?}");
            refused(e, &row, ZoeError::CorruptRecord { oid, tid })
        })?;
        Ok((tid, frame))
    }

    /// Remove `key` from `table` in a write transaction of its own. Fail
    /// with `NotFound` for `oid` when the key does not exist.
    fn remove_for_test<V: redb::Value + 'static>(
        &self,
        table: TableDefinition<&[u8; 16], V>,
        key: &[u8; 16],
        oid: Oid,
    ) -> Result<()> {
        let Db::ReadWrite(db) = &self.db else {
            return Err(ZoeError::ReadOnly);
        };
        let txn = db.begin_write().map_err(backend)?;
        let removed = txn
            .open_table(table)
            .map_err(backend)?
            .remove(key)
            .map_err(backend)?
            .is_some();
        if !removed {
            return Err(ZoeError::NotFound { oid });
        }
        txn.commit().map_err(backend)
    }
}

impl Backend for RedbBackend {
    fn load_before(&self, oid: Oid, tid: Tid) -> Result<Option<Record>> {
        let txn = self.db.begin_read().map_err(backend)?;
        let records = txn.open_table(RECORDS).map_err(backend)?;
        let start = format::record_key(oid, Tid::ZERO);
        let end = format::record_key(oid, tid);
        let mut range = records.range::<&[u8; 16]>(&start..&end).map_err(backend)?;
        match range.next_back() {
            Some(entry) => {
                let (key, value) = entry.map_err(backend)?;
                inside(oid, key.value(), start..end)?;
                Self::record(oid, key.value(), value.value()).map(Some)
            }
            None => Ok(None),
        }
    }

    fn load_serial(&self, oid: Oid, tid: Tid) -> Result<Option<Bytes>> {
        let txn = self.db.begin_read().map_err(backend)?;
        let records = txn.open_table(RECORDS).map_err(backend)?;
        let key = format::record_key(oid, tid);
        match records.get(&key).map_err(backend)? {
            Some(value) => Ok(Some(Self::record(oid, &key, value.value())?.data)),
            None => Ok(None),
        }
    }

    fn record_info(&self, oid: Oid, tid: Tid) -> Result<Option<RecordEncoding>> {
        let txn = self.db.begin_read().map_err(backend)?;
        let records = txn.open_table(RECORDS).map_err(backend)?;
        let key = format::record_key(oid, tid);
        let Some(value) = records.get(&key).map_err(backend)? else {
            return Ok(None);
        };
        let (_, frame) = Self::frame(oid, &key, value.value())?;
        Ok(Some(RecordEncoding {
            codec: frame.codec,
            stored_len: frame.stored_len as u64,
            data_len: frame.data.len() as u64,
        }))
    }

    fn load_blob(&self, oid: Oid, tid: Tid) -> Result<Option<BlobRecord>> {
        let txn = self.db.begin_read().map_err(backend)?;
        let blobs = match txn.open_table(BLOBS) {
            Ok(blobs) => blobs,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(backend(e)),
        };
        let key = format::record_key(oid, tid);
        let Some(value) = blobs.get(&key).map_err(backend)? else {
            return Ok(None);
        };
        let blob = format::decode_frame(&key, value.value())
            .and_then(|frame| format::blob::decode_blob(&frame.data))
            .map_err(|e| {
                let row = format!("blob record of oid {oid:?}, tid {tid:?}");
                refused(e, &row, ZoeError::CorruptBlob { oid, tid })
            })?;
        Ok(Some(blob))
    }

    fn last_tid(&self) -> Tid {
        Tid(self.last_tid.load(Ordering::Acquire))
    }

    fn begin_write(&self) -> Result<Box<dyn WriteTxn + '_>> {
        let Db::ReadWrite(db) = &self.db else {
            return Err(ZoeError::ReadOnly);
        };
        Ok(Box::new(RedbWrite {
            txn: db.begin_write().map_err(backend)?,
            last_tid: &self.last_tid,
            compression: self.compression,
            written_tid: None,
            max_oid: None,
            pack_watermark: None,
        }))
    }

    fn iter_txns(&self, from: Tid, to: Tid) -> TxnIter<'_> {
        let read = || -> Result<Vec<Result<TxnMeta>>> {
            let txn = self.db.begin_read().map_err(backend)?;
            let txns = txn.open_table(TXNS).map_err(backend)?;
            let (start, end) = (format::txn_key(from), format::txn_key(to));
            let mut rows = Vec::new();
            // Each key is after the key before it.
            let mut after = Included(start);
            for entry in txns.range::<&[u8; 8]>(&start..=&end).map_err(backend)? {
                let (key, value) = entry.map_err(backend)?;
                inside_txns(key.value(), (after, Included(end)))?;
                after = Excluded(*key.value());
                rows.push(Self::txn_row(key.value(), value.value()));
            }
            Ok(rows)
        };
        match read() {
            Ok(rows) => Box::new(rows.into_iter()),
            Err(e) => Box::new(std::iter::once(Err(e))),
        }
    }

    fn history(&self, oid: Oid, limit: usize) -> Result<Vec<RecordMeta>> {
        let txn = self.db.begin_read().map_err(backend)?;
        let records = txn.open_table(RECORDS).map_err(backend)?;
        let start = format::record_key(oid, Tid::ZERO);
        let end = format::record_key(oid, Tid::MAX);
        let mut history = Vec::new();
        // Each key is before the key before it.
        let mut before = Included(end);
        for entry in records
            .range::<&[u8; 16]>(&start..=&end)
            .map_err(backend)?
            .rev()
            .take(limit)
        {
            let (key, value) = entry.map_err(backend)?;
            inside(oid, key.value(), (Included(start), before))?;
            before = Excluded(*key.value());
            let record = Self::record(oid, key.value(), value.value())?;
            history.push(RecordMeta {
                tid: record.tid,
                prev: record.prev,
                size: record.data.len() as u64,
            });
        }
        Ok(history)
    }

    fn load_before_next(&self, oid: Oid, tid: Tid) -> Result<(Option<Record>, Option<Tid>)> {
        let txn = self.db.begin_read().map_err(backend)?;
        let records = txn.open_table(RECORDS).map_err(backend)?;
        let start = format::record_key(oid, Tid::ZERO);
        let split = format::record_key(oid, tid);
        let end = format::record_key(oid, Tid::MAX);
        let before = match records
            .range::<&[u8; 16]>(&start..&split)
            .map_err(backend)?
            .next_back()
        {
            Some(entry) => {
                let (key, value) = entry.map_err(backend)?;
                inside(oid, key.value(), start..split)?;
                Some(Self::record(oid, key.value(), value.value())?)
            }
            None => None,
        };
        let next = match records
            .range::<&[u8; 16]>(&split..=&end)
            .map_err(backend)?
            .next()
        {
            Some(entry) => {
                let (key, value) = entry.map_err(backend)?;
                inside(oid, key.value(), split..=end)?;
                Some(Self::frame(oid, key.value(), value.value())?.0)
            }
            None => None,
        };
        Ok((before, next))
    }

    fn txn_oids(&self, from: Tid, to: Tid) -> Result<BTreeMap<Tid, Vec<Oid>>> {
        let mut oids: BTreeMap<Tid, Vec<Oid>> = BTreeMap::new();
        if from > to {
            return Ok(oids);
        }
        let txn = self.db.begin_read().map_err(backend)?;
        let index = txn.open_table(TXN_OIDS).map_err(backend)?;
        // The scan reads only the index keys of the range, and no record.
        // Each key is inside the range and after the key before it, so no
        // oid comes twice.
        let start = format::txn_oid_key(from, Oid(0));
        let end = format::txn_oid_key(to, Oid(u64::MAX));
        let mut after = Included(start);
        for entry in index.range::<&[u8; 16]>(&start..=&end).map_err(backend)? {
            let (key, _) = entry.map_err(backend)?;
            inside_index(key.value(), (after, Included(end)))?;
            after = Excluded(*key.value());
            let (tid, oid) = format::split_txn_oid_key(key.value());
            oids.entry(tid).or_default().push(oid);
        }
        Ok(oids)
    }

    fn current_from(&self, from: Oid) -> Result<Option<(Oid, Record)>> {
        let txn = self.db.begin_read().map_err(backend)?;
        let records = txn.open_table(RECORDS).map_err(backend)?;
        let start = format::record_key(from, Tid::ZERO);
        let oid = match records
            .range::<&[u8; 16]>(&start..)
            .map_err(backend)?
            .next()
        {
            Some(entry) => {
                let key = *entry.map_err(backend)?.0.value();
                inside(from, &key, start..)?;
                format::split_record_key(&key).0
            }
            None => return Ok(None),
        };
        let first = format::record_key(oid, Tid::ZERO);
        let last = format::record_key(oid, Tid::MAX);
        let mut versions = records
            .range::<&[u8; 16]>(&first..=&last)
            .map_err(backend)?;
        match versions.next_back() {
            Some(entry) => {
                let (key, value) = entry.map_err(backend)?;
                inside(oid, key.value(), first..=last)?;
                Ok(Some((oid, Self::record(oid, key.value(), value.value())?)))
            }
            None => Ok(None),
        }
    }

    fn pack(&self, _before: Tid, _reachable: &dyn Fn(Oid, Tid) -> bool) -> Result<PackStats> {
        Err(ZoeError::Backend(
            "pack goes through Store::pack and the removal methods".into(),
        ))
    }

    fn pack_watermark(&self) -> Result<Tid> {
        let txn = self.db.begin_read().map_err(backend)?;
        let meta = txn.open_table(META).map_err(backend)?;
        match meta.get(META_PACK_WATERMARK).map_err(backend)? {
            None => Ok(Tid::ZERO),
            Some(value) => format::decode_u64(value.value())
                .map(Tid)
                .ok_or_else(|| ZoeError::Format("pack watermark is not 8 bytes".to_owned())),
        }
    }

    fn record_keys(&self, from: Oid, limit: usize) -> Result<Vec<(Oid, Tid)>> {
        let txn = self.db.begin_read().map_err(backend)?;
        let records = txn.open_table(RECORDS).map_err(backend)?;
        let start = format::record_key(from, Tid::ZERO);
        let mut keys: Vec<(Oid, Tid)> = Vec::new();
        // Each key is at or after the start, and after the key before it.
        let mut after = Included(start);
        for entry in records.range::<&[u8; 16]>(&start..).map_err(backend)? {
            let (key, _) = entry.map_err(backend)?;
            let (oid, tid) = format::split_record_key(key.value());
            inside(oid, key.value(), (after, Unbounded))?;
            after = Excluded(*key.value());
            if keys.len() >= limit && keys.last().is_some_and(|&(last, _)| last != oid) {
                break;
            }
            keys.push((oid, tid));
        }
        Ok(keys)
    }

    fn txn_records(&self, tid: Tid) -> Result<Option<TxnRecords>> {
        let txn = self.db.begin_read().map_err(backend)?;
        let index = txn.open_table(TXN_OIDS).map_err(backend)?;
        let start = format::txn_oid_key(tid, Oid(0));
        let end = format::txn_oid_key(tid, Oid(u64::MAX));
        // Each key has the tid `tid` and comes after the key before it, so
        // no oid of another transaction comes in, and no oid comes twice.
        let mut oids = Vec::new();
        let mut after = Included(start);
        for entry in index.range::<&[u8; 16]>(&start..=&end).map_err(backend)? {
            let (key, _) = entry.map_err(backend)?;
            inside_index(key.value(), (after, Included(end)))?;
            after = Excluded(*key.value());
            oids.push(format::split_txn_oid_key(key.value()).1);
        }
        let txns = txn.open_table(TXNS).map_err(backend)?;
        let key = format::txn_key(tid);
        let row = match txns.get(&key).map_err(backend)? {
            Some(value) => Self::txn_row(&key, value.value())?,
            // A pack removes a row in the write that removes its last
            // index key.
            None if oids.is_empty() => return Ok(None),
            None => return Err(ZoeError::CorruptTxn { tid }),
        };
        let records = txn.open_table(RECORDS).map_err(backend)?;
        let mut found = Vec::with_capacity(oids.len());
        for oid in oids {
            let key = format::record_key(oid, tid);
            let value = records
                .get(&key)
                .map_err(backend)?
                .ok_or(ZoeError::CorruptTxn { tid })?;
            found.push((oid, Self::record(oid, &key, value.value())?.data));
        }
        Ok(Some((row, found)))
    }

    fn sync(&self) -> Result<()> {
        // Every commit is durable when it returns, so there is nothing to
        // flush.
        Ok(())
    }

    #[cfg(feature = "testing")]
    fn read_stored(&self, key: crate::testing::Key) -> Result<Option<Bytes>> {
        use crate::testing::Key;
        let txn = self.db.begin_read().map_err(backend)?;
        let value = match key {
            Key::Record(oid, tid) | Key::Blob(oid, tid) => {
                let table = if matches!(key, Key::Record(..)) {
                    RECORDS
                } else {
                    BLOBS
                };
                let table = txn.open_table(table).map_err(backend)?;
                let value = table.get(&format::record_key(oid, tid)).map_err(backend)?;
                value.map(|v| v.value().to_vec())
            }
            Key::Txn(tid) => {
                let table = txn.open_table(TXNS).map_err(backend)?;
                let value = table.get(&format::txn_key(tid)).map_err(backend)?;
                value.map(|v| v.value().to_vec())
            }
        };
        Ok(value)
    }

    #[cfg(feature = "testing")]
    fn write_stored(&self, key: crate::testing::Key, bytes: &[u8]) -> Result<()> {
        use crate::testing::Key;
        let Db::ReadWrite(db) = &self.db else {
            return Err(ZoeError::ReadOnly);
        };
        let txn = db.begin_write().map_err(backend)?;
        match key {
            Key::Record(oid, tid) | Key::Blob(oid, tid) => {
                let table = if matches!(key, Key::Record(..)) {
                    RECORDS
                } else {
                    BLOBS
                };
                let mut table = txn.open_table(table).map_err(backend)?;
                table
                    .insert(&format::record_key(oid, tid), bytes)
                    .map_err(backend)?;
            }
            Key::Txn(tid) => {
                let mut table = txn.open_table(TXNS).map_err(backend)?;
                table
                    .insert(&format::txn_key(tid), bytes)
                    .map_err(backend)?;
            }
        }
        txn.commit().map_err(backend)
    }

    fn remove_index_key_for_test(&self, tid: Tid, oid: Oid) -> Result<()> {
        self.remove_for_test(TXN_OIDS, &format::txn_oid_key(tid, oid), oid)
    }

    fn remove_record_for_test(&self, oid: Oid, tid: Tid) -> Result<()> {
        self.remove_for_test(RECORDS, &format::record_key(oid, tid), oid)
    }
}

/// One redb write transaction.
struct RedbWrite<'a> {
    txn: redb::WriteTransaction,
    last_tid: &'a AtomicU64,
    compression: Compression,
    written_tid: Option<Tid>,
    max_oid: Option<Oid>,
    pack_watermark: Option<Tid>,
}

impl WriteTxn for RedbWrite<'_> {
    fn put_record(&mut self, oid: Oid, tid: Tid, prev: Tid, data: &[u8]) -> Result<()> {
        let key = format::record_key(oid, tid);
        let frame =
            format::encode_frame_with(&key, format::MAIN_BRANCH, prev, data, self.compression);
        let mut records = self.txn.open_table(RECORDS).map_err(backend)?;
        records.insert(&key, frame.as_slice()).map_err(backend)?;
        let mut index = self.txn.open_table(TXN_OIDS).map_err(backend)?;
        index
            .insert(&format::txn_oid_key(tid, oid), ())
            .map_err(backend)?;
        self.max_oid = self.max_oid.max(Some(oid));
        Ok(())
    }

    fn put_blob(&mut self, oid: Oid, tid: Tid, blob: &BlobRecord) -> Result<()> {
        let key = format::record_key(oid, tid);
        let payload = format::blob::encode_blob(blob);
        let frame = format::encode_frame(&key, format::MAIN_BRANCH, Tid::ZERO, &payload);
        let mut blobs = self.txn.open_table(BLOBS).map_err(backend)?;
        blobs.insert(&key, frame.as_slice()).map_err(backend)?;
        self.max_oid = self.max_oid.max(Some(oid));
        Ok(())
    }

    fn put_txn(&mut self, meta: &TxnMeta) -> Result<()> {
        debug_assert_eq!(meta.status, TxnStatus::Committed);
        let key = format::txn_key(meta.tid);
        let payload = format::encode_txn(meta);
        let frame = format::encode_frame(&key, format::MAIN_BRANCH, Tid::ZERO, &payload);
        let mut txns = self.txn.open_table(TXNS).map_err(backend)?;
        txns.insert(&key, frame.as_slice()).map_err(backend)?;
        self.written_tid = Some(meta.tid);
        Ok(())
    }

    fn remove_record(&mut self, oid: Oid, tid: Tid) -> Result<()> {
        let mut records = self.txn.open_table(RECORDS).map_err(backend)?;
        records
            .remove(&format::record_key(oid, tid))
            .map_err(backend)?;
        let mut index = self.txn.open_table(TXN_OIDS).map_err(backend)?;
        let removed = index
            .remove(&format::txn_oid_key(tid, oid))
            .map_err(backend)?
            .is_some();
        // A record without its index key is a damaged store. The removal
        // fails, so the write does not erase the evidence.
        if !removed {
            return Err(ZoeError::CorruptTxn { tid });
        }
        Ok(())
    }

    fn remove_blob(&mut self, oid: Oid, tid: Tid) -> Result<()> {
        let mut blobs = self.txn.open_table(BLOBS).map_err(backend)?;
        blobs
            .remove(&format::record_key(oid, tid))
            .map_err(backend)?;
        Ok(())
    }

    fn replace_txn(&mut self, meta: &TxnMeta) -> Result<()> {
        let key = format::txn_key(meta.tid);
        let mut txns = self.txn.open_table(TXNS).map_err(backend)?;
        // The new frame keeps the branch id and the previous serial of the
        // row. This code writes no typed-field slots, so it refuses to
        // rewrite a row that has some.
        let (branch, prev) = {
            let tid = meta.tid;
            let old = txns
                .get(&key)
                .map_err(backend)?
                .ok_or(ZoeError::CorruptTxn { tid })?;
            let frame = format::decode_frame(&key, old.value()).map_err(|e| {
                refused(
                    e,
                    &format!("transaction row {tid:?}"),
                    ZoeError::CorruptTxn { tid },
                )
            })?;
            if !frame.slots.is_empty() {
                return Err(ZoeError::Format(format!(
                    "transaction row {tid:?} has typed-field slots, which this build cannot rewrite"
                )));
            }
            (frame.branch, frame.prev)
        };
        let payload = format::encode_txn(meta);
        let frame = format::encode_frame(&key, branch, prev, &payload);
        txns.insert(&key, frame.as_slice()).map_err(backend)?;
        Ok(())
    }

    fn remove_txn(&mut self, tid: Tid) -> Result<()> {
        let mut txns = self.txn.open_table(TXNS).map_err(backend)?;
        txns.remove(&format::txn_key(tid)).map_err(backend)?;
        Ok(())
    }

    fn put_pack_watermark(&mut self, tid: Tid) -> Result<()> {
        self.pack_watermark = Some(tid);
        Ok(())
    }

    fn commit(self: Box<Self>) -> Result<()> {
        {
            let mut meta = self.txn.open_table(META).map_err(backend)?;
            if let Some(tid) = self.pack_watermark {
                meta.insert(META_PACK_WATERMARK, &format::encode_u64(tid.0)[..])
                    .map_err(backend)?;
            }
            if let Some(tid) = self.written_tid {
                meta.insert(META_LAST_TID, &format::encode_u64(tid.0)[..])
                    .map_err(backend)?;
            }
            if let Some(oid) = self.max_oid {
                let stored = meta
                    .get(META_NEXT_OID)
                    .map_err(backend)?
                    .and_then(|v| format::decode_u64(v.value()))
                    .unwrap_or(1);
                let next = stored.max(oid.0 + 1);
                meta.insert(META_NEXT_OID, &format::encode_u64(next)[..])
                    .map_err(backend)?;
            }
        }
        self.txn.commit().map_err(backend)?;
        if let Some(tid) = self.written_tid {
            self.last_tid.store(tid.0, Ordering::Release);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use redb::StorageBackend;
    use redb::backends::InMemoryBackend;
    use xxhash_rust::xxh3::Xxh3;

    use super::*;
    use crate::store::Store;
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

    /// Return the frame of the row `key` with the codec byte `codec` and a
    /// valid checksum.
    fn frame_with_codec(key: &[u8], codec: u8, payload: &[u8]) -> Vec<u8> {
        let mut frame = format::encode_frame(key, format::MAIN_BRANCH, Tid::ZERO, payload);
        frame[2] = codec;
        let mut hasher = Xxh3::new();
        hasher.update(key);
        hasher.update(&frame[..24]);
        hasher.update(payload);
        let checksum = hasher.digest().to_le_bytes();
        frame[24..32].copy_from_slice(&checksum);
        frame
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

    /// A pickle that zstd makes much smaller.
    fn compressible() -> Vec<u8> {
        b"X\x05\x00\x00\x00titleq\x02X\x0b\x00\x00\x00descriptionq\x03".repeat(100)
    }

    /// Commit the record of `oid` at `tid` with `data`.
    fn commit_record(backend: &RedbBackend, oid: Oid, tid: Tid, data: &[u8]) {
        let mut write = backend.begin_write().unwrap();
        write.put_record(oid, tid, Tid::ZERO, data).unwrap();
        write.commit().unwrap();
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
        let frame = format::encode_frame(&key, 7, Tid(3), &format::encode_txn(&row));
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
        let frame = format::decode_frame(&key, stored.value()).unwrap();
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
        let mut frame = format::encode_frame(&key, format::MAIN_BRANCH, Tid::ZERO, &payload);
        frame[20..22].copy_from_slice(&2u16.to_le_bytes());
        frame.splice(format::HEADER_LEN..format::HEADER_LEN, [1, 2]);
        let mut hasher = Xxh3::new();
        hasher.update(&key);
        hasher.update(&frame[..24]);
        hasher.update(&frame[format::HEADER_LEN..]);
        let checksum = hasher.digest().to_le_bytes();
        frame[24..32].copy_from_slice(&checksum);
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
}
