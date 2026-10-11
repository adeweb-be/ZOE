//! The redb backend.
//!
//! Four tables hold the key spaces of the data model: `records`, `blobs`,
//! `txns` and `meta`. `format` says how keys and values are written.
//! `initialise` creates all four, so a new store has an empty `blobs`
//! table. A store written before the `blobs` table has none until its
//! first blob commit creates it. Until then it reads as a store without
//! blob records.

use std::collections::BTreeMap;
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
    /// The last tid is the greater of `last_tid` in `meta` and the tid of
    /// the last transaction row. So a damaged `last_tid` cannot give a tid
    /// a second time.
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
            let stored = read(META_LAST_TID)?
                .as_deref()
                .and_then(format::decode_u64)
                .ok_or_else(|| ZoeError::Format("no last tid".to_owned()))?;
            let txns = txn.open_table(TXNS).map_err(backend)?;
            let last_row = txns.last().map_err(backend)?;
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
            for entry in txns.range::<&[u8; 8]>(&start..=&end).map_err(backend)? {
                let (key, value) = entry.map_err(backend)?;
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
        for entry in records
            .range::<&[u8; 16]>(&start..=&end)
            .map_err(backend)?
            .rev()
            .take(limit)
        {
            let (key, value) = entry.map_err(backend)?;
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
                Some(Self::frame(oid, key.value(), value.value())?.0)
            }
            None => None,
        };
        Ok((before, next))
    }

    fn txn_oids(&self, from: Tid, to: Tid) -> Result<BTreeMap<Tid, Vec<Oid>>> {
        let txn = self.db.begin_read().map_err(backend)?;
        let records = txn.open_table(RECORDS).map_err(backend)?;
        let mut oids: BTreeMap<Tid, Vec<Oid>> = BTreeMap::new();
        // No key space maps a tid to its oids, so this scan reads every
        // key. It does not decode the frames: each later read of a record
        // verifies it.
        for entry in records.iter().map_err(backend)? {
            let (key, _) = entry.map_err(backend)?;
            let (oid, tid) = format::split_record_key(key.value());
            if (from..=to).contains(&tid) {
                oids.entry(tid).or_default().push(oid);
            }
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
            Some(entry) => format::split_record_key(entry.map_err(backend)?.0.value()).0,
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
        for entry in records.range::<&[u8; 16]>(&start..).map_err(backend)? {
            let (key, _) = entry.map_err(backend)?;
            let (oid, tid) = format::split_record_key(key.value());
            if keys.len() >= limit && keys.last().is_some_and(|&(last, _)| last != oid) {
                break;
            }
            keys.push((oid, tid));
        }
        Ok(keys)
    }

    fn txn_records(&self, tid: Tid, oids: &[Oid]) -> Result<Option<TxnRecords>> {
        let txn = self.db.begin_read().map_err(backend)?;
        let txns = txn.open_table(TXNS).map_err(backend)?;
        let key = format::txn_key(tid);
        let row = match txns.get(&key).map_err(backend)? {
            Some(value) => Self::txn_row(&key, value.value())?,
            None => return Ok(None),
        };
        let records = txn.open_table(RECORDS).map_err(backend)?;
        let mut found = Vec::new();
        for &oid in oids {
            let key = format::record_key(oid, tid);
            if let Some(value) = records.get(&key).map_err(backend)? {
                found.push((oid, Self::record(oid, &key, value.value())?.data));
            }
        }
        Ok(Some((row, found)))
    }

    fn sync(&self) -> Result<()> {
        // Every commit is durable when it returns, so there is nothing to
        // flush.
        Ok(())
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
        let payload = format::encode_txn(meta);
        let frame = format::encode_frame(&key, format::MAIN_BRANCH, Tid::ZERO, &payload);
        let mut txns = self.txn.open_table(TXNS).map_err(backend)?;
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
    use redb::backends::InMemoryBackend;
    use xxhash_rust::xxh3::Xxh3;

    use super::*;

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
}
