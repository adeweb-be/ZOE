//! The `Backend` trait of the redb backend: the reads, the barrier
//! `sync`, the start of a write transaction and the test hooks.

use std::collections::BTreeMap;
use std::ops::Bound::{self, Included, Unbounded};
use std::sync::atomic::Ordering;

use redb::ReadableTable;

#[cfg(feature = "testing")]
use super::KV_EXPIRY;
use super::write::RedbWrite;
use super::{
    BLOBS, Bounds, Db, META, META_PACK_WATERMARK, RECORDS, RedbBackend, SEQUENCES, TXN_OIDS, TXNS,
    backend, corrupt_index, corrupt_record, corrupt_txn, inside, kv, newest, open_optional,
    refused,
};
use crate::backend::{
    Backend, BlobRecord, Bytes, Record, RecordEncoding, RecordMeta, TxnIter, TxnMeta, TxnRecords,
    WriteTxn,
};
use crate::error::{Result, ZoeError};
use crate::format::{self, KeySpace};
use crate::kv::Flavour;
use crate::types::{Oid, Tid};

impl Backend for RedbBackend {
    fn load_before(&self, oid: Oid, tid: Tid) -> Result<Option<Record>> {
        let txn = self.db.begin_read().map_err(backend)?;
        let records = txn.open_table(RECORDS).map_err(backend)?;
        let start = format::record_key(oid, Tid::ZERO);
        let end = format::record_key(oid, tid);
        newest(&records, oid, &start..&end, |key, value| {
            Self::record(oid, key, value)
        })
    }

    fn current_serials(&self, oids: &mut dyn Iterator<Item = Oid>) -> Result<Vec<Option<Tid>>> {
        // One read transaction serves every oid. The key of the newest
        // record gives its tid, so no value is read or decoded.
        let txn = self.db.begin_read().map_err(backend)?;
        let records = txn.open_table(RECORDS).map_err(backend)?;
        oids.map(|oid| {
            let start = format::record_key(oid, Tid::ZERO);
            let end = format::record_key(oid, Tid::MAX);
            newest(&records, oid, &start..&end, |key, _| {
                Ok(format::split_record_key(key).1)
            })
        })
        .collect()
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
        let Some(blobs) = open_optional(&txn, BLOBS)? else {
            return Ok(None);
        };
        let key = format::record_key(oid, tid);
        let Some(value) = blobs.get(&key).map_err(backend)? else {
            return Ok(None);
        };
        let blob = format::decode_frame(KeySpace::Blobs, &key, value.value())
            .and_then(|frame| format::blob::decode_blob(&frame.data))
            .map_err(|e| {
                let row = format!("blob record of oid {oid:?}, tid {tid:?}");
                refused(e, &row, ZoeError::CorruptBlob { oid, tid })
            })?;
        Ok(Some(blob))
    }

    fn has_blobs(&self) -> Result<bool> {
        let txn = self.db.begin_read().map_err(backend)?;
        match open_optional(&txn, BLOBS)? {
            Some(blobs) => Ok(blobs.first().map_err(backend)?.is_some()),
            // A store written before the blob table has no blob record.
            None => Ok(false),
        }
    }

    fn last_tid(&self) -> Tid {
        Tid(self.last_tid.load(Ordering::Acquire))
    }

    fn begin_write(&self) -> Result<Box<dyn WriteTxn + '_>> {
        let Db::ReadWrite(db) = &self.db else {
            return Err(ZoeError::ReadOnly);
        };
        let txn = db.begin_write().map_err(backend)?;
        Ok(Box::new(RedbWrite::new(txn, self)))
    }

    fn iter_txns(&self, from: Tid, to: Tid) -> TxnIter<'_> {
        let (start, end) = (format::txn_key(from), format::txn_key(to));
        let range = self
            .db
            .begin_read()
            .map_err(backend)
            .and_then(|txn| txn.open_table(TXNS).map_err(backend))
            .and_then(|txns| txns.range::<&[u8; 8]>(&start..=&end).map_err(backend));
        let mut range = match range {
            Ok(range) => range,
            Err(e) => return Box::new(std::iter::once(Err(e))),
        };
        // The range keeps its snapshot. It reads one row at a time, so the
        // memory does not grow with the range. Each key is after the key
        // before it. A failed scan gives its error and ends the iterator.
        let mut bounds = Bounds::new(Included(start), Included(end));
        let mut failed = false;
        Box::new(std::iter::from_fn(move || {
            if failed {
                return None;
            }
            let row = range.next()?.map_err(backend).and_then(|(key, value)| {
                bounds.next(key.value(), corrupt_txn)?;
                Ok(Self::txn_row(key.value(), value.value()))
            });
            Some(row.unwrap_or_else(|e| {
                failed = true;
                Err(e)
            }))
        }))
    }

    fn txns_before(&self, before: Tid, limit: usize) -> Result<Vec<TxnMeta>> {
        if before == Tid::ZERO || limit == 0 {
            return Ok(Vec::new());
        }
        let (start, end) = (
            format::txn_key(Tid::ZERO),
            format::txn_key(Tid(before.0 - 1)),
        );
        let txn = self.db.begin_read().map_err(backend)?;
        let txns = txn.open_table(TXNS).map_err(backend)?;
        let mut rows = Vec::new();
        // Each key is before the key before it.
        let mut bounds = Bounds::new(Included(start), Included(end));
        for entry in txns
            .range::<&[u8; 8]>(&start..=&end)
            .map_err(backend)?
            .rev()
            .take(limit)
        {
            let (key, value) = entry.map_err(backend)?;
            bounds.next_back(key.value(), corrupt_txn)?;
            rows.push(Self::txn_row(key.value(), value.value())?);
        }
        Ok(rows)
    }

    fn history(&self, oid: Oid, limit: usize) -> Result<Vec<RecordMeta>> {
        let txn = self.db.begin_read().map_err(backend)?;
        let records = txn.open_table(RECORDS).map_err(backend)?;
        let start = format::record_key(oid, Tid::ZERO);
        let end = format::record_key(oid, Tid::MAX);
        let mut history = Vec::new();
        // Each key is before the key before it.
        let mut bounds = Bounds::new(Included(start), Included(end));
        for entry in records
            .range::<&[u8; 16]>(&start..=&end)
            .map_err(backend)?
            .rev()
            .take(limit)
        {
            let (key, value) = entry.map_err(backend)?;
            bounds.next_back(key.value(), corrupt_record(oid))?;
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
        let before = newest(&records, oid, &start..&split, |key, value| {
            Self::record(oid, key, value)
        })?;
        // The key of the next record gives its serial. Its frame is
        // verified and not decoded: the load returns no byte of it.
        let next = match records
            .range::<&[u8; 16]>(&split..=&end)
            .map_err(backend)?
            .next()
        {
            Some(entry) => {
                let (key, value) = entry.map_err(backend)?;
                inside(key.value(), split..=end, corrupt_record(oid))?;
                Some(Self::verified(oid, key.value(), value.value())?)
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
        let mut bounds = Bounds::new(Included(start), Included(end));
        for entry in index.range::<&[u8; 16]>(&start..=&end).map_err(backend)? {
            let (key, _) = entry.map_err(backend)?;
            bounds.next(key.value(), corrupt_index)?;
            let (tid, oid) = format::split_txn_oid_key(key.value());
            oids.entry(tid).or_default().push(oid);
        }
        Ok(oids)
    }

    fn indexed_tids(&self, from: Tid, to: Tid, limit: usize) -> Result<Vec<Tid>> {
        let mut tids: Vec<Tid> = Vec::new();
        if from > to || limit == 0 {
            return Ok(tids);
        }
        let txn = self.db.begin_read().map_err(backend)?;
        let index = txn.open_table(TXN_OIDS).map_err(backend)?;
        // The scan keeps only the tids. Each key is inside the range and
        // after the key before it, as in `txn_oids`.
        let start = format::txn_oid_key(from, Oid(0));
        let end = format::txn_oid_key(to, Oid(u64::MAX));
        let mut bounds = Bounds::new(Included(start), Included(end));
        for entry in index.range::<&[u8; 16]>(&start..=&end).map_err(backend)? {
            let (key, _) = entry.map_err(backend)?;
            bounds.next(key.value(), corrupt_index)?;
            let (tid, _) = format::split_txn_oid_key(key.value());
            if tids.last() != Some(&tid) {
                if tids.len() == limit {
                    break;
                }
                tids.push(tid);
            }
        }
        Ok(tids)
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
                inside(&key, start.., corrupt_record(from))?;
                format::split_record_key(&key).0
            }
            None => return Ok(None),
        };
        let first = format::record_key(oid, Tid::ZERO);
        let last = format::record_key(oid, Tid::MAX);
        newest(&records, oid, &first..=&last, |key, value| {
            Ok((oid, Self::record(oid, key, value)?))
        })
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

    fn object_count(&self) -> Option<u64> {
        self.counted
            .then(|| self.object_count.load(Ordering::Acquire))
    }

    fn record_keys(&self, from: Oid, limit: usize) -> Result<Vec<(Oid, Tid)>> {
        let txn = self.db.begin_read().map_err(backend)?;
        let records = txn.open_table(RECORDS).map_err(backend)?;
        let start = format::record_key(from, Tid::ZERO);
        let mut keys: Vec<(Oid, Tid)> = Vec::new();
        // Each key is at or after the start, and after the key before it.
        let mut bounds = Bounds::new(Included(start), Unbounded);
        for entry in records.range::<&[u8; 16]>(&start..).map_err(backend)? {
            let (key, _) = entry.map_err(backend)?;
            let (oid, tid) = format::split_record_key(key.value());
            bounds.next(key.value(), corrupt_record(oid))?;
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
        let mut bounds = Bounds::new(Included(start), Included(end));
        for entry in index.range::<&[u8; 16]>(&start..=&end).map_err(backend)? {
            let (key, _) = entry.map_err(backend)?;
            bounds.next(key.value(), corrupt_index)?;
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
        let Db::ReadWrite(db) = &self.db else {
            return Ok(());
        };
        // An empty write transaction with the default durability syncs
        // the file. So the commits without durability before it become
        // durable too.
        db.begin_write().map_err(backend)?.commit().map_err(backend)
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

    #[cfg(feature = "testing")]
    fn write_kv_stored(
        &self,
        flavour: Option<Flavour>,
        row: &[u8],
        bytes: Option<&[u8]>,
    ) -> Result<()> {
        let Db::ReadWrite(db) = &self.db else {
            return Err(ZoeError::ReadOnly);
        };
        let txn = db.begin_write().map_err(backend)?;
        match flavour {
            Some(flavour) => {
                let mut table = txn.open_table(kv::table(flavour)).map_err(backend)?;
                match bytes {
                    Some(bytes) => table.insert(row, bytes).map(drop),
                    None => table.remove(row).map(drop),
                }
                .map_err(backend)?;
            }
            None => {
                let mut index = txn.open_table(KV_EXPIRY).map_err(backend)?;
                match bytes {
                    Some(_) => index.insert(row, ()).map(drop),
                    None => index.remove(row).map(drop),
                }
                .map_err(backend)?;
            }
        }
        txn.commit().map_err(backend)
    }

    #[cfg(feature = "testing")]
    fn write_meta_stored(&self, key: &str, value: Option<&[u8]>) -> Result<()> {
        let Db::ReadWrite(db) = &self.db else {
            return Err(ZoeError::ReadOnly);
        };
        let txn = db.begin_write().map_err(backend)?;
        {
            let mut meta = txn.open_table(META).map_err(backend)?;
            match value {
                Some(value) => {
                    meta.insert(key, value).map_err(backend)?;
                }
                None => {
                    if meta.remove(key).map_err(backend)?.is_none() {
                        return Err(ZoeError::Backend(format!("no meta row {key:?}").into()));
                    }
                }
            }
        }
        txn.commit().map_err(backend)
    }

    #[cfg(feature = "testing")]
    fn remove_index_key_for_test(&self, tid: Tid, oid: Oid) -> Result<()> {
        self.remove_for_test(TXN_OIDS, &format::txn_oid_key(tid, oid), oid)
    }

    #[cfg(feature = "testing")]
    fn remove_record_for_test(&self, oid: Oid, tid: Tid) -> Result<()> {
        self.remove_for_test(RECORDS, &format::record_key(oid, tid), oid)
    }

    fn sequence_end(&self, name: &str) -> Result<Option<u64>> {
        let txn = self.db.begin_read().map_err(backend)?;
        let Some(sequences) = open_optional(&txn, SEQUENCES)? else {
            return Ok(None);
        };
        let Some(value) = sequences.get(name).map_err(backend)? else {
            return Ok(None);
        };
        let end = format::decode_frame(KeySpace::Sequences, name.as_bytes(), value.value())
            .and_then(|frame| format::decode_sequence(&frame.data))
            .map_err(|e| {
                let corrupt = ZoeError::CorruptSequence {
                    name: name.to_owned(),
                };
                refused(e, &format!("sequence row {name:?}"), corrupt)
            })?;
        Ok(Some(end))
    }

    fn sequence_names(&self) -> Result<Vec<String>> {
        let txn = self.db.begin_read().map_err(backend)?;
        let Some(sequences) = open_optional(&txn, SEQUENCES)? else {
            return Ok(Vec::new());
        };
        let mut names = Vec::new();
        // redb decodes a `&str` key with `from_utf8(..).unwrap()`. A flipped
        // bit can make a stored name invalid UTF-8, and then the decode
        // panics. The catch changes that panic into a corruption error.
        let scan = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
            for entry in sequences.iter().map_err(backend)? {
                let (key, _) = entry.map_err(backend)?;
                names.push(key.value().to_owned());
            }
            Ok(())
        }));
        match scan {
            Ok(scanned) => scanned.map(|()| names),
            Err(_) => {
                let at = match names.last() {
                    Some(name) => format!("after {name:?}"),
                    None => "at the start".to_owned(),
                };
                Err(backend(redb::StorageError::Corrupted(format!(
                    "the key of a sequence row {at} does not decode"
                ))))
            }
        }
    }

    fn kv_row(&self, flavour: Flavour, row: &[u8]) -> Result<Option<Bytes>> {
        kv::row(&self.db, flavour, row)
    }

    fn kv_rows(
        &self,
        flavour: Flavour,
        from: Bound<&[u8]>,
        to: Bound<&[u8]>,
        limit: usize,
    ) -> Result<Vec<(Bytes, Bytes)>> {
        kv::rows(&self.db, flavour, from, to, limit)
    }

    fn kv_expiry_has(&self, key: &[u8]) -> Result<bool> {
        kv::expiry_has(&self.db, key)
    }

    fn has_kv(&self, flavour: Flavour) -> Result<bool> {
        kv::has(&self.db, flavour)
    }
}
