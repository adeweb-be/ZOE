//! The redb backend.
//!
//! Six tables hold the key spaces of the object graph: `records`,
//! `txn_oids`, `blobs`, `txns`, `sequences` and `meta`. Three more hold
//! the KV namespaces: `kv_ephemeral`, `kv_durable` and `kv_expiry`. The
//! first write of a flavour creates its tables, so a store that never
//! writes a namespace has none of them. `format` says how keys and
//! values are written. `txn_oids` is the transaction index: `put_record`
//! writes the index key of each record in the write transaction of the
//! record, and `remove_record` removes the key with the record.
//! `initialise` creates all six, so a new store has an empty `blobs`
//! table and an empty `sequences` table. A store written before one of
//! these tables has none until its first blob commit or its first
//! reservation creates it. Until then it reads as a store without blob
//! records or without sequences.
//!
//! The row `object_count` of `meta` counts the oids that have a record.
//! Each write that puts or removes records updates it, and the first
//! writer open of a store of format 1 writes it.
//!
//! `read` holds the `Backend` trait of the backend, `write` its write
//! transaction, and `kv` the tables of the KV namespaces.

use std::ops::Bound::{self, Excluded};
use std::ops::RangeBounds;
use std::sync::atomic::AtomicU64;

use redb::{
    Database, ReadOnlyDatabase, ReadTransaction, ReadableDatabase, ReadableTable, TableDefinition,
};

use crate::backend::{Record, TxnMeta};
use crate::error::{Result, ZoeError};
use crate::format::{self, Compression, KeySpace};
use crate::types::{Oid, Tid};

mod kv;
mod read;
#[cfg(test)]
mod tests;
mod write;

const RECORDS: TableDefinition<&[u8; 16], &[u8]> = TableDefinition::new("records");
const TXN_OIDS: TableDefinition<&[u8; 16], ()> = TableDefinition::new("txn_oids");
const BLOBS: TableDefinition<&[u8; 16], &[u8]> = TableDefinition::new("blobs");
const TXNS: TableDefinition<&[u8; 8], &[u8]> = TableDefinition::new("txns");
const SEQUENCES: TableDefinition<&str, &[u8]> = TableDefinition::new("sequences");
const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");
const KV_EPHEMERAL: TableDefinition<&[u8], &[u8]> = TableDefinition::new("kv_ephemeral");
const KV_DURABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("kv_durable");
const KV_EXPIRY: TableDefinition<&[u8], ()> = TableDefinition::new("kv_expiry");

const META_FORMAT: &str = "format";
const META_LAST_TID: &str = "last_tid";
const META_NEXT_OID: &str = "next_oid";
const META_PACK_WATERMARK: &str = "pack_watermark";
const META_OBJECT_COUNT: &str = "object_count";

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

/// Check that `key`, which a scan gave, is inside `range`. A key outside
/// it raises the error that `corrupt` makes from the key.
///
/// redb seeks each end of a scan through the branch pages, and it checks
/// only the bound of the other end. So a damaged branch page can move the
/// seek and give a key outside the range. The frame of that key is intact
/// and passes its checksum, so only this check stops it.
fn inside<const N: usize>(
    key: &[u8; N],
    range: impl RangeBounds<[u8; N]>,
    corrupt: impl FnOnce(&[u8; N]) -> ZoeError,
) -> Result<()> {
    if range.contains(key) {
        return Ok(());
    }
    Err(corrupt(key))
}

/// Return the error of a key of `records` outside the scan of a read of
/// `oid`: `CorruptRecord` with `oid` and the tid of the key.
fn corrupt_record(oid: Oid) -> impl FnOnce(&[u8; 16]) -> ZoeError {
    move |key| {
        let (_, tid) = format::split_record_key(key);
        ZoeError::CorruptRecord { oid, tid }
    }
}

/// Return the error of a key of `txns` outside its scan: `CorruptTxn` with
/// the tid of the key.
fn corrupt_txn(key: &[u8; 8]) -> ZoeError {
    let tid = Tid(u64::from_be_bytes(*key));
    ZoeError::CorruptTxn { tid }
}

/// Return the error of a key of the transaction index outside its scan:
/// `CorruptTxn` with the tid of the key.
fn corrupt_index(key: &[u8; 16]) -> ZoeError {
    let (tid, _) = format::split_txn_oid_key(key);
    ZoeError::CorruptTxn { tid }
}

/// The bounds of a scan, for the check of each key that it gives. Each key
/// must be inside the range of the scan and after the key before it, in
/// the order of the scan.
struct Bounds<const N: usize> {
    low: Bound<[u8; N]>,
    high: Bound<[u8; N]>,
}

impl<const N: usize> Bounds<N> {
    /// Return the bounds of a scan from `low` to `high`.
    fn new(low: Bound<[u8; N]>, high: Bound<[u8; N]>) -> Self {
        Self { low, high }
    }

    /// Check `key`, the next key of a forward scan, as `inside` does. Then
    /// the next key must come after it.
    fn next(&mut self, key: &[u8; N], corrupt: impl FnOnce(&[u8; N]) -> ZoeError) -> Result<()> {
        inside(key, (self.low, self.high), corrupt)?;
        self.low = Excluded(*key);
        Ok(())
    }

    /// Check `key`, the next key of a backward scan, as `inside` does. Then
    /// the next key must come before it.
    fn next_back(
        &mut self,
        key: &[u8; N],
        corrupt: impl FnOnce(&[u8; N]) -> ZoeError,
    ) -> Result<()> {
        inside(key, (self.low, self.high), corrupt)?;
        self.high = Excluded(*key);
        Ok(())
    }
}

/// Read the newest record of `oid` in `range`: the last key of a scan of
/// `records`. Give its key and its stored bytes to `read`, and return what
/// `read` returns. Return `None` when the scan gives no key. A key outside
/// `range` raises `CorruptRecord` with `oid` and the tid of the key.
fn newest<'k, T>(
    records: &impl ReadableTable<&'static [u8; 16], &'static [u8]>,
    oid: Oid,
    range: impl RangeBounds<&'k [u8; 16]> + RangeBounds<[u8; 16]> + Clone,
    read: impl FnOnce(&[u8; 16], &[u8]) -> Result<T>,
) -> Result<Option<T>> {
    let mut scan = records
        .range::<&[u8; 16]>(range.clone())
        .map_err(backend)?;
    let Some(entry) = scan.next_back() else {
        return Ok(None);
    };
    let (key, value) = entry.map_err(backend)?;
    inside(key.value(), range, corrupt_record(oid))?;
    read(key.value(), value.value()).map(Some)
}

/// Open the table `table` of `txn` for reads. Return `None` when the store
/// has no such table: a missing table is empty.
fn open_optional<K: redb::Key + 'static, V: redb::Value + 'static>(
    txn: &ReadTransaction,
    table: TableDefinition<K, V>,
) -> Result<Option<redb::ReadOnlyTable<K, V>>> {
    match txn.open_table(table) {
        Ok(table) => Ok(Some(table)),
        Err(redb::TableError::TableDoesNotExist(_)) => Ok(None),
        Err(e) => Err(backend(e)),
    }
}

/// Tell if `oid` has at least one record in `records`. `prev` is the
/// serial of a record of `oid` that can exist, or `Tid::ZERO`. A point read
/// of that record answers first, and the scan runs only when it finds
/// none.
fn has_records_from(
    records: &impl ReadableTable<&'static [u8; 16], &'static [u8]>,
    oid: Oid,
    prev: Tid,
) -> Result<bool> {
    if prev != Tid::ZERO
        && records
            .get(&format::record_key(oid, prev))
            .map_err(backend)?
            .is_some()
    {
        return Ok(true);
    }
    has_records(records, oid)
}

/// Tell if `oid` has at least one record in `records`.
fn has_records(
    records: &impl ReadableTable<&'static [u8; 16], &'static [u8]>,
    oid: Oid,
) -> Result<bool> {
    let start = format::record_key(oid, Tid::ZERO);
    let end = format::record_key(oid, Tid::MAX);
    let mut scan = records.range::<&[u8; 16]>(&start..=&end).map_err(backend)?;
    match scan.next() {
        None => Ok(false),
        Some(entry) => {
            let (key, _) = entry.map_err(backend)?;
            inside(key.value(), start..=end, corrupt_record(oid))?;
            Ok(true)
        }
    }
}

/// Return the number of distinct oids of the keys of `records`. The scan
/// reads every key. Each key must come after the key before it, else the
/// count fails with `CorruptRecord`.
fn count_objects(records: &impl ReadableTable<&'static [u8; 16], &'static [u8]>) -> Result<u64> {
    let mut count = 0;
    let mut previous: Option<[u8; 16]> = None;
    for entry in records.iter().map_err(backend)? {
        let (key, _) = entry.map_err(backend)?;
        let key = *key.value();
        let (oid, tid) = format::split_record_key(&key);
        match previous {
            Some(before) if key <= before => return Err(ZoeError::CorruptRecord { oid, tid }),
            Some(before) if format::split_record_key(&before).0 == oid => {}
            _ => count += 1,
        }
        previous = Some(key);
    }
    Ok(count)
}

/// Decode the row `object_count` of `meta`. A store of the current format
/// without the row, or with a row that is not 8 bytes, is a `Format`
/// error.
fn decode_object_count(value: Option<&[u8]>) -> Result<u64> {
    value
        .and_then(format::decode_u64)
        .ok_or_else(|| ZoeError::Format("the object count is missing or is not 8 bytes".to_owned()))
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
    /// The object count of the last write that changed records. Only
    /// writes under the commit sequencer change records, so they store
    /// it in their order.
    object_count: AtomicU64,
    /// False only for a read-only open of a store of format 1, which has
    /// no count.
    counted: bool,
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
            txn.open_table(SEQUENCES).map_err(backend)?;
            let mut meta = txn.open_table(META).map_err(backend)?;
            meta.insert(META_FORMAT, format::FORMAT_NAME)
                .map_err(backend)?;
            meta.insert(META_LAST_TID, &format::encode_u64(0)[..])
                .map_err(backend)?;
            meta.insert(META_NEXT_OID, &format::encode_u64(1)[..])
                .map_err(backend)?;
            meta.insert(META_OBJECT_COUNT, &format::encode_u64(0)[..])
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
    ///
    /// A store of the current format must have `object_count`. A store of
    /// format 1 has none: a writer open upgrades it (`upgrade`), and a
    /// read-only open reads it without a count.
    fn open_db(db: Db) -> Result<Self> {
        let (last_tid, object_count) = {
            let txn = db.begin_read().map_err(backend)?;
            let meta = txn.open_table(META).map_err(backend)?;
            let read = |key: &str| -> Result<Option<Vec<u8>>> {
                Ok(meta.get(key).map_err(backend)?.map(|v| v.value().to_vec()))
            };
            let format_name = read(META_FORMAT)?.unwrap_or_default();
            let object_count = if format_name == format::FORMAT_NAME {
                Some(decode_object_count(read(META_OBJECT_COUNT)?.as_deref())?)
            } else if format_name == format::FORMAT_1_NAME {
                None
            } else {
                return Err(ZoeError::Format(
                    String::from_utf8_lossy(&format_name).into_owned(),
                ));
            };
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
                    format::decode_frame(KeySpace::Txns, key.value(), value.value()),
                    Ok(_) | Err(format::FrameError::Unsupported { .. })
                )
            });
            let last_tid =
                stored.max(last_row.map_or(0, |(key, _)| u64::from_be_bytes(*key.value())));
            (last_tid, object_count)
        };
        let object_count = match (object_count, &db) {
            (Some(count), _) => Some(count),
            (None, Db::ReadWrite(db)) => Some(Self::upgrade(db)?),
            (None, Db::ReadOnly(_)) => None,
        };
        Ok(Self {
            db,
            last_tid: AtomicU64::new(last_tid),
            object_count: AtomicU64::new(object_count.unwrap_or(0)),
            counted: object_count.is_some(),
            compression: Compression::default(),
        })
    }

    /// Upgrade a store of format 1 to the current format: count the
    /// distinct oids of `records`, then write `object_count` and the
    /// format name in one write transaction, with a sync. Return the
    /// count.
    ///
    /// The count reads every key of `records`, so this open does not hold
    /// N7. A crash before the commit leaves format 1, and the next writer
    /// open counts again.
    fn upgrade(db: &Database) -> Result<u64> {
        let txn = db.begin_write().map_err(backend)?;
        let count = {
            let records = txn.open_table(RECORDS).map_err(backend)?;
            let count = count_objects(&records)?;
            let mut meta = txn.open_table(META).map_err(backend)?;
            meta.insert(META_OBJECT_COUNT, &format::encode_u64(count)[..])
                .map_err(backend)?;
            meta.insert(META_FORMAT, format::FORMAT_NAME)
                .map_err(backend)?;
            count
        };
        txn.commit().map_err(backend)?;
        Ok(count)
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
        format::decode_frame(KeySpace::Txns, key, bytes)
            .and_then(|frame| format::decode_txn(tid, &frame.data))
            .map_err(|e| refused(e, &format!("transaction row {tid:?}"), corrupt))
    }

    /// Verify the record frame stored under `key` for `oid` without
    /// decoding it. Return the tid of the key.
    fn verified(oid: Oid, key: &[u8; 16], bytes: &[u8]) -> Result<Tid> {
        let (_, tid) = format::split_record_key(key);
        format::verify_frame(KeySpace::Records, key, bytes).map_err(|e| {
            let row = format!("record of oid {oid:?}, tid {tid:?}");
            refused(e, &row, ZoeError::CorruptRecord { oid, tid })
        })?;
        Ok(tid)
    }

    /// Decode and verify the record frame stored under `key` for `oid`.
    /// Return the tid of the key and the frame.
    fn frame<'a>(oid: Oid, key: &[u8; 16], bytes: &'a [u8]) -> Result<(Tid, format::Frame<'a>)> {
        let (_, tid) = format::split_record_key(key);
        let frame = format::decode_frame(KeySpace::Records, key, bytes).map_err(|e| {
            let row = format!("record of oid {oid:?}, tid {tid:?}");
            refused(e, &row, ZoeError::CorruptRecord { oid, tid })
        })?;
        Ok((tid, frame))
    }

    /// Remove `key` from `table` in a write transaction of its own. Fail
    /// with `NotFound` for `oid` when the key does not exist.
    #[cfg(feature = "testing")]
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
