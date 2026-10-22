//! The write transaction of the redb backend.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use redb::ReadableTable;

use super::{
    BLOBS, META, META_LAST_TID, META_NEXT_OID, META_OBJECT_COUNT, META_PACK_WATERMARK, RECORDS,
    RedbBackend, SEQUENCES, TXN_OIDS, TXNS, backend, decode_object_count, has_records,
    has_records_from, kv, refused,
};
use crate::backend::{BlobRecord, NewPayload, NewRecord, Payload, TxnMeta, TxnStatus, WriteTxn};
use crate::error::{Result, ZoeError};
use crate::format::{self, Compression, KeySpace};
use crate::kv::{Flavour, Swept};
use crate::types::{Oid, Tid};

/// Return the frame of `payload` under `key` in the key space `space`, on
/// the main branch, with no previous serial. The blob records, the
/// transaction rows, the sequence rows and the namespace rows have such
/// frames.
pub(super) fn main_frame(space: KeySpace, key: &[u8], payload: &[u8]) -> Vec<u8> {
    format::encode_frame(space, key, format::MAIN_BRANCH, Tid::ZERO, payload)
}

/// One redb write transaction.
pub(super) struct RedbWrite<'a> {
    txn: redb::WriteTransaction,
    last_tid: &'a AtomicU64,
    object_count: &'a AtomicU64,
    compression: Compression,
    written_tid: Option<Tid>,
    max_oid: Option<Oid>,
    pack_watermark: Option<Tid>,
    /// Each oid that the write puts or removes a record of.
    touched: BTreeMap<Oid, Touched>,
    /// The next oid that `meta` held when the write began, read at the
    /// first put. No record has this oid or a greater one.
    fresh: Option<Oid>,
}

/// What a write did to the records of one oid.
#[derive(Clone, Copy)]
struct Touched {
    /// The oid had a record before the write touched it.
    before: bool,
    /// The write removed a record of the oid.
    removed: bool,
}

impl<'a> RedbWrite<'a> {
    /// Return a new write of `backend` in the redb write transaction
    /// `txn`.
    pub(super) fn new(txn: redb::WriteTransaction, backend: &'a RedbBackend) -> Self {
        Self {
            txn,
            last_tid: &backend.last_tid,
            object_count: &backend.object_count,
            compression: backend.compression,
            written_tid: None,
            max_oid: None,
            pack_watermark: None,
            touched: BTreeMap::new(),
            fresh: None,
        }
    }

    /// Note that the write touches `oid`, before it changes a record of
    /// the oid. The first note of an oid reads if the oid has a record.
    fn touch(
        touched: &mut BTreeMap<Oid, Touched>,
        records: &impl ReadableTable<&'static [u8; 16], &'static [u8]>,
        oid: Oid,
        removed: bool,
    ) -> Result<()> {
        Self::touch_put(touched, records, oid, Tid::ZERO, None, removed)
    }

    /// Note that the write touches `oid`, as `touch` does. `prev` is the
    /// serial of the previous record of the put, or `Tid::ZERO`. An oid at
    /// or after `fresh`, the next oid of `meta`, has no record, so the
    /// note reads nothing for it.
    fn touch_put(
        touched: &mut BTreeMap<Oid, Touched>,
        records: &impl ReadableTable<&'static [u8; 16], &'static [u8]>,
        oid: Oid,
        prev: Tid,
        fresh: Option<Oid>,
        removed: bool,
    ) -> Result<()> {
        let entry = match touched.entry(oid) {
            std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::btree_map::Entry::Vacant(entry) => {
                let before = match fresh {
                    Some(fresh) if oid >= fresh => false,
                    _ => has_records_from(records, oid, prev)?,
                };
                entry.insert(Touched {
                    before,
                    removed: false,
                })
            }
        };
        entry.removed |= removed;
        Ok(())
    }

    /// Return the next oid that `meta` held when the write began. It
    /// reads the row once. A row that does not decode gives `None`, and
    /// then each note reads the records.
    fn fresh(&mut self) -> Result<Option<Oid>> {
        if self.fresh.is_none() {
            let meta = self.txn.open_table(META).map_err(backend)?;
            let value = meta.get(META_NEXT_OID).map_err(backend)?;
            self.fresh = value.and_then(|v| format::decode_u64(v.value())).map(Oid);
        }
        Ok(self.fresh)
    }

    /// Return the object count after this write: the stored count, plus
    /// each touched oid that has a record now and had none before, less
    /// each touched oid that had a record before and has none now.
    fn count_after(&self, stored: u64) -> Result<u64> {
        let records = self.txn.open_table(RECORDS).map_err(backend)?;
        let mut count = stored;
        for (&oid, touched) in &self.touched {
            // A write that only put records of the oid leaves a record.
            let after = !touched.removed || has_records(&records, oid)?;
            // Only a damaged count can be lower than the oids that it
            // counts. `zoe verify` finds such a count, so the write does
            // not fail for it.
            match (touched.before, after) {
                (false, true) => count = count.saturating_add(1),
                (true, false) => count = count.saturating_sub(1),
                _ => {}
            }
        }
        Ok(count)
    }
}

impl WriteTxn for RedbWrite<'_> {
    fn put_record(&mut self, oid: Oid, tid: Tid, prev: Tid, data: &[u8]) -> Result<()> {
        let data = Cow::Borrowed(data);
        self.put_records(tid, &mut std::iter::once(Ok((oid, prev, data))))
    }

    fn put_records(
        &mut self,
        tid: Tid,
        new: &mut dyn Iterator<Item = Result<NewRecord<'_>>>,
    ) -> Result<()> {
        let mut payloads =
            new.map(|record| record.map(|(oid, prev, data)| (oid, prev, Payload::Pickle(data))));
        self.put_payloads(tid, &mut payloads)
    }

    fn put_payloads(
        &mut self,
        tid: Tid,
        new: &mut dyn Iterator<Item = Result<NewPayload<'_>>>,
    ) -> Result<()> {
        let fresh = self.fresh()?;
        // The two tables stay open for all the records of the call.
        let mut records = self.txn.open_table(RECORDS).map_err(backend)?;
        let mut index = self.txn.open_table(TXN_OIDS).map_err(backend)?;
        for record in new {
            let (oid, prev, payload) = record?;
            Self::touch_put(&mut self.touched, &records, oid, prev, fresh, false)?;
            let key = format::record_key(oid, tid);
            let (space, branch) = (KeySpace::Records, format::MAIN_BRANCH);
            let frame = match payload {
                Payload::Pickle(data) => {
                    format::encode_frame_with(space, &key, branch, prev, &data, self.compression)
                }
                Payload::Encoded(codec, stored) => {
                    format::encode_frame_payload(space, &key, branch, prev, codec, &stored)
                }
            };
            records.insert(&key, frame.as_slice()).map_err(backend)?;
            index
                .insert(&format::txn_oid_key(tid, oid), ())
                .map_err(backend)?;
            self.max_oid = self.max_oid.max(Some(oid));
        }
        Ok(())
    }

    fn put_blob(&mut self, oid: Oid, tid: Tid, blob: &BlobRecord) -> Result<()> {
        let key = format::record_key(oid, tid);
        let payload = format::blob::encode_blob(blob);
        let frame = main_frame(KeySpace::Blobs, &key, &payload);
        let mut blobs = self.txn.open_table(BLOBS).map_err(backend)?;
        blobs.insert(&key, frame.as_slice()).map_err(backend)?;
        self.max_oid = self.max_oid.max(Some(oid));
        Ok(())
    }

    fn put_txn(&mut self, meta: &TxnMeta) -> Result<()> {
        debug_assert_eq!(meta.status, TxnStatus::Committed);
        let key = format::txn_key(meta.tid);
        let payload = format::encode_txn(meta);
        let frame = main_frame(KeySpace::Txns, &key, &payload);
        let mut txns = self.txn.open_table(TXNS).map_err(backend)?;
        txns.insert(&key, frame.as_slice()).map_err(backend)?;
        self.written_tid = Some(meta.tid);
        Ok(())
    }

    fn remove_record(&mut self, oid: Oid, tid: Tid) -> Result<()> {
        let mut records = self.txn.open_table(RECORDS).map_err(backend)?;
        Self::touch(&mut self.touched, &records, oid, true)?;
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
            let frame = format::decode_frame(KeySpace::Txns, &key, old.value()).map_err(|e| {
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
        let frame = format::encode_frame(KeySpace::Txns, &key, branch, prev, &payload);
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

    fn put_sequence_end(&mut self, name: &str, end: u64) -> Result<()> {
        let payload = format::encode_sequence(end);
        let frame = main_frame(KeySpace::Sequences, name.as_bytes(), &payload);
        let mut sequences = self.txn.open_table(SEQUENCES).map_err(backend)?;
        sequences.insert(name, frame.as_slice()).map_err(backend)?;
        Ok(())
    }

    fn put_kv(&mut self, flavour: Flavour, row: &[u8], payload: &[u8]) -> Result<()> {
        kv::put(&self.txn, flavour, row, payload)
    }

    fn remove_kv(&mut self, flavour: Flavour, row: &[u8]) -> Result<()> {
        kv::remove(&self.txn, flavour, row)
    }

    fn put_kv_expiry(&mut self, key: &[u8]) -> Result<()> {
        kv::put_expiry(&self.txn, key)
    }

    fn sweep_kv(&mut self, now: u64, limit: usize) -> Result<Swept> {
        kv::sweep(&self.txn, now, limit)
    }

    fn commit(self: Box<Self>) -> Result<()> {
        // A write that changes no record writes no count: a sequence
        // reservation, a sweep step and the barrier of `sync`.
        let object_count = if self.touched.is_empty() {
            None
        } else {
            let stored = {
                let meta = self.txn.open_table(META).map_err(backend)?;
                let value = meta.get(META_OBJECT_COUNT).map_err(backend)?;
                decode_object_count(value.as_ref().map(|v| v.value()))?
            };
            Some(self.count_after(stored)?)
        };
        {
            let mut meta = self.txn.open_table(META).map_err(backend)?;
            if let Some(count) = object_count {
                meta.insert(META_OBJECT_COUNT, &format::encode_u64(count)[..])
                    .map_err(backend)?;
            }
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
        if let Some(count) = object_count {
            self.object_count.store(count, Ordering::Release);
        }
        if let Some(tid) = self.written_tid {
            self.last_tid.store(tid.0, Ordering::Release);
        }
        Ok(())
    }

    fn commit_deferred(mut self: Box<Self>) -> Result<()> {
        // redb does not sync a commit without durability. The next
        // durable commit makes it durable.
        self.txn
            .set_durability(redb::Durability::None)
            .map_err(backend)?;
        self.commit()
    }
}
