//! The storage trait that backends implement.
//!
//! The core, not the backend, implements locking, two-phase commit,
//! conflict resolution, invalidations and blobs. A backend stores and
//! reads bytes under the keys of the data model.

use std::collections::BTreeMap;

use crate::error::{Result, ZoeError};
use crate::format::Codec;
use crate::types::{Oid, Tid};

/// The bytes of a pickle or of a metadata field.
pub type Bytes = Vec<u8>;

/// An iterator over the transaction log, in tid order.
pub type TxnIter<'a> = Box<dyn Iterator<Item = Result<TxnMeta>> + 'a>;

/// The row of a transaction and its records, as `(oid, pickle)`, from one
/// snapshot.
pub type TxnRecords = (TxnMeta, Vec<(Oid, Bytes)>);

/// One version of one object: a row of the `records` key space.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    /// The tid that wrote this version. It is the serial of the record.
    pub tid: Tid,
    /// The serial of the previous version of the object.
    pub prev: Tid,
    /// The pickle bytes. An empty value marks a deletion by
    /// `deleteObject`. Pack removes records and writes no deletion.
    pub data: Bytes,
}

/// The blob record of one blob file: a row of the `blobs` key space. The
/// file itself is in the blob store, at the path of its `(oid, tid)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlobRecord {
    /// The BLAKE3 hash of the bytes of the blob file.
    pub hash: [u8; 32],
    /// The size of the blob file in bytes.
    pub size: u64,
}

/// The metadata of one version of one object, for `history`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordMeta {
    /// The tid that wrote this version.
    pub tid: Tid,
    /// The serial of the previous version of the object.
    pub prev: Tid,
    /// The size of the pickle in bytes.
    pub size: u64,
}

/// How one version of one object is stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecordEncoding {
    /// The codec of the stored payload.
    pub codec: Codec,
    /// The length of the payload as stored.
    pub stored_len: u64,
    /// The length of the pickle.
    pub data_len: u64,
}

/// The state of a transaction in the transaction log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxnStatus {
    /// The finish record of the transaction is not written yet.
    Committing,
    /// The transaction is complete and durable.
    Committed,
}

/// A row of the `txns` key space: one transaction of the log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TxnMeta {
    /// The tid of the transaction.
    pub tid: Tid,
    /// The user name that ZODB gives to the transaction.
    pub user: Bytes,
    /// The description that ZODB gives to the transaction.
    pub description: Bytes,
    /// The pickled extension data of the transaction.
    pub extension: Bytes,
    /// The number of objects that the transaction wrote.
    pub oid_count: u64,
    /// The state of the transaction.
    pub status: TxnStatus,
}

/// What a pack removed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PackStats {
    /// The number of record versions that the pack removed.
    pub records_removed: u64,
    /// The number of bytes that the pack removed.
    pub bytes_removed: u64,
}

/// A durable store of records and transactions.
///
/// The trait is dyn-compatible, so that the configuration can select the
/// backend when the store opens.
pub trait Backend: Send + Sync {
    /// Return the newest version of `oid` with a tid lower than `tid`.
    fn load_before(&self, oid: Oid, tid: Tid) -> Result<Option<Record>>;

    /// Return the pickle that `tid` wrote for `oid`.
    fn load_serial(&self, oid: Oid, tid: Tid) -> Result<Option<Bytes>>;

    /// Return how the record that `tid` wrote for `oid` is stored.
    fn record_info(&self, oid: Oid, tid: Tid) -> Result<Option<RecordEncoding>>;

    /// Return the blob record that `tid` wrote for `oid`.
    fn load_blob(&self, oid: Oid, tid: Tid) -> Result<Option<BlobRecord>>;

    /// Return the tid of the last committed transaction.
    fn last_tid(&self) -> Tid;

    /// Open a write transaction. The core opens one for each `tpc_finish`.
    fn begin_write(&self) -> Result<Box<dyn WriteTxn + '_>>;

    /// Iterate over the transactions from `from` to `to`.
    fn iter_txns(&self, from: Tid, to: Tid) -> TxnIter<'_>;

    /// Return at most `limit` versions of `oid`, newest first.
    fn history(&self, oid: Oid, limit: usize) -> Result<Vec<RecordMeta>>;

    /// Return the newest version of `oid` with a tid lower than `tid`, and
    /// the serial of the oldest version of `oid` with a tid at or after
    /// `tid`. One snapshot gives both.
    fn load_before_next(&self, oid: Oid, tid: Tid) -> Result<(Option<Record>, Option<Tid>)>;

    /// Return the oids that each transaction from `from` to `to` wrote,
    /// both bounds included. The oids of a transaction are in ascending
    /// order. One snapshot gives the whole map.
    ///
    /// The map comes from the transaction index. The call reads only the
    /// index keys of the range and no record, so its cost is the size of
    /// the result. A key outside the range, or a key that does not come
    /// after the key before it, is `CorruptTxn`. The iterator takes only
    /// the tids of the map: `txn_records` reads the oids of each
    /// transaction with their records.
    fn txn_oids(&self, from: Tid, to: Tid) -> Result<BTreeMap<Tid, Vec<Oid>>>;

    /// Return the first oid at or after `from` that has a version, with
    /// its newest version.
    fn current_from(&self, from: Oid) -> Result<Option<(Oid, Record)>>;

    /// Remove the versions older than `before` that `reachable` rejects.
    ///
    /// The core does not call this method: pack removes records through
    /// the removal methods of `WriteTxn`, in batches.
    fn pack(&self, before: Tid, reachable: &dyn Fn(Oid, Tid) -> bool) -> Result<PackStats>;

    /// Return the tid of the last pack, from `pack_watermark` in `meta`.
    /// A store that was never packed gives `Tid::ZERO`.
    fn pack_watermark(&self) -> Result<Tid>;

    /// Return the keys of the records of the oids from `from` up, in key
    /// order. The list stops after the first oid that brings it to `limit`
    /// keys or more, so it holds all the keys of each oid in it. One
    /// snapshot gives the list.
    fn record_keys(&self, from: Oid, limit: usize) -> Result<Vec<(Oid, Tid)>>;

    /// Return the transaction row of `tid` and the records that the
    /// transaction index gives for `tid`, in oid order, in one snapshot.
    /// Return `None` when the row does not exist and no index key of `tid`
    /// is left.
    ///
    /// An index key without its record is `CorruptTxn`. Index keys of a
    /// `tid` that has no row are `CorruptTxn` too, and so is a key of
    /// another tid or a key that does not come after the key before it.
    /// The caller compares the records with the oid count of the row.
    fn txn_records(&self, tid: Tid) -> Result<Option<TxnRecords>>;

    /// Make all committed writes durable. Group commit uses this barrier.
    fn sync(&self) -> Result<()>;

    /// Return the bytes stored under `key`, as stored. Only the test hook
    /// `testing` calls it. A backend without the hook gives a `Backend`
    /// error.
    #[cfg(feature = "testing")]
    fn read_stored(&self, key: crate::testing::Key) -> Result<Option<Bytes>> {
        Err(ZoeError::Backend(
            format!("no test hook to read {key:?}").into(),
        ))
    }

    /// Store `bytes` under `key`, with no check, in a write transaction of
    /// its own. Only the test hook `testing` calls it. A backend without
    /// the hook gives a `Backend` error.
    #[cfg(feature = "testing")]
    fn write_stored(&self, key: crate::testing::Key, bytes: &[u8]) -> Result<()> {
        let _ = bytes;
        Err(ZoeError::Backend(
            format!("no test hook to write {key:?}").into(),
        ))
    }

    /// Remove the key `(tid, oid)` of the transaction index and keep the
    /// record. A test uses it to damage a store. Fail with `NotFound` when
    /// the key does not exist.
    #[doc(hidden)]
    fn remove_index_key_for_test(&self, _tid: Tid, _oid: Oid) -> Result<()> {
        Err(ZoeError::Backend("this backend has no test hooks".into()))
    }

    /// Remove the record of `oid` at `tid` and keep its key in the
    /// transaction index. A test uses it to damage a store. Fail with
    /// `NotFound` when the record does not exist.
    #[doc(hidden)]
    fn remove_record_for_test(&self, _oid: Oid, _tid: Tid) -> Result<()> {
        Err(ZoeError::Backend("this backend has no test hooks".into()))
    }
}

/// One backend write transaction. It holds the records, their keys in
/// the transaction index, the blob records, the transaction row and the
/// meta update of one commit.
pub trait WriteTxn {
    /// Stage the version of `oid` that `tid` writes, and its key
    /// `(tid, oid)` in the transaction index. `prev` is the serial of the
    /// previous version.
    fn put_record(&mut self, oid: Oid, tid: Tid, prev: Tid, data: &[u8]) -> Result<()>;

    /// Stage the blob record of the version of `oid` that `tid` writes.
    /// It has no key of its own in the transaction index: the record of
    /// the same version has it.
    fn put_blob(&mut self, oid: Oid, tid: Tid, blob: &BlobRecord) -> Result<()>;

    /// Stage the transaction row.
    fn put_txn(&mut self, meta: &TxnMeta) -> Result<()>;

    /// Stage the removal of the record of `oid` that `tid` wrote, and of
    /// its key `(tid, oid)` in the transaction index. Pack uses this. A
    /// missing index key is `CorruptTxn`, so the write does not erase the
    /// evidence of a damaged store.
    fn remove_record(&mut self, oid: Oid, tid: Tid) -> Result<()>;

    /// Stage the removal of the blob record of `oid` that `tid` wrote.
    /// Pack uses this.
    fn remove_blob(&mut self, oid: Oid, tid: Tid) -> Result<()>;

    /// Stage a new value of the existing transaction row of `meta.tid`.
    /// Unlike `put_txn`, it does not change the last tid. Pack uses this
    /// to lower the oid count of a row.
    fn replace_txn(&mut self, meta: &TxnMeta) -> Result<()>;

    /// Stage the removal of the transaction row of `tid`. Pack uses this
    /// when no record of the transaction is left.
    fn remove_txn(&mut self, tid: Tid) -> Result<()>;

    /// Stage `pack_watermark` in `meta`: the tid of the last pack.
    fn put_pack_watermark(&mut self, tid: Tid) -> Result<()>;

    /// Commit the staged writes. The writes are durable when this returns.
    fn commit(self: Box<Self>) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A backend that stores nothing. It shows that the traits work as
    /// trait objects.
    struct NullBackend;

    struct NullWrite;

    impl WriteTxn for NullWrite {
        fn put_record(&mut self, _: Oid, _: Tid, _: Tid, _: &[u8]) -> Result<()> {
            Ok(())
        }

        fn put_blob(&mut self, _: Oid, _: Tid, _: &BlobRecord) -> Result<()> {
            Ok(())
        }

        fn put_txn(&mut self, _: &TxnMeta) -> Result<()> {
            Ok(())
        }

        fn remove_record(&mut self, _: Oid, _: Tid) -> Result<()> {
            Ok(())
        }

        fn remove_blob(&mut self, _: Oid, _: Tid) -> Result<()> {
            Ok(())
        }

        fn replace_txn(&mut self, _: &TxnMeta) -> Result<()> {
            Ok(())
        }

        fn remove_txn(&mut self, _: Tid) -> Result<()> {
            Ok(())
        }

        fn put_pack_watermark(&mut self, _: Tid) -> Result<()> {
            Ok(())
        }

        fn commit(self: Box<Self>) -> Result<()> {
            Ok(())
        }
    }

    impl Backend for NullBackend {
        fn load_before(&self, _: Oid, _: Tid) -> Result<Option<Record>> {
            Ok(None)
        }

        fn load_serial(&self, _: Oid, _: Tid) -> Result<Option<Bytes>> {
            Ok(None)
        }

        fn record_info(&self, _: Oid, _: Tid) -> Result<Option<RecordEncoding>> {
            Ok(None)
        }

        fn load_blob(&self, _: Oid, _: Tid) -> Result<Option<BlobRecord>> {
            Ok(None)
        }

        fn last_tid(&self) -> Tid {
            Tid(0)
        }

        fn begin_write(&self) -> Result<Box<dyn WriteTxn + '_>> {
            Ok(Box::new(NullWrite))
        }

        fn iter_txns(&self, _: Tid, _: Tid) -> TxnIter<'_> {
            Box::new(std::iter::empty())
        }

        fn history(&self, _: Oid, _: usize) -> Result<Vec<RecordMeta>> {
            Ok(Vec::new())
        }

        fn load_before_next(&self, _: Oid, _: Tid) -> Result<(Option<Record>, Option<Tid>)> {
            Ok((None, None))
        }

        fn txn_oids(&self, _: Tid, _: Tid) -> Result<BTreeMap<Tid, Vec<Oid>>> {
            Ok(BTreeMap::new())
        }

        fn current_from(&self, _: Oid) -> Result<Option<(Oid, Record)>> {
            Ok(None)
        }

        fn pack(&self, _: Tid, _: &dyn Fn(Oid, Tid) -> bool) -> Result<PackStats> {
            Ok(PackStats::default())
        }

        fn pack_watermark(&self) -> Result<Tid> {
            Ok(Tid::ZERO)
        }

        fn record_keys(&self, _: Oid, _: usize) -> Result<Vec<(Oid, Tid)>> {
            Ok(Vec::new())
        }

        fn txn_records(&self, _: Tid) -> Result<Option<TxnRecords>> {
            Ok(None)
        }

        fn sync(&self) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn backend_is_dyn_compatible() {
        let backend: Box<dyn Backend> = Box::new(NullBackend);
        let mut txn = backend.begin_write().unwrap();
        txn.put_record(Oid(1), Tid(2), Tid(0), b"pickle").unwrap();
        txn.commit().unwrap();
        assert_eq!(backend.load_before(Oid(1), Tid(3)).unwrap(), None);
        assert_eq!(backend.iter_txns(Tid(0), Tid(3)).count(), 0);
    }
}
