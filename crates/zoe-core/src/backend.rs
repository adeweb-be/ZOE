//! The storage trait that backends implement.
//!
//! The core, not the backend, implements locking, two-phase commit,
//! conflict resolution, invalidations and blobs. A backend stores and
//! reads bytes under the keys of the data model.

use crate::error::Result;
use crate::types::{Oid, Tid};

/// The bytes of a pickle or of a metadata field.
pub type Bytes = Vec<u8>;

/// An iterator over the transaction log, in tid order.
pub type TxnIter<'a> = Box<dyn Iterator<Item = Result<TxnMeta>> + 'a>;

/// One version of one object: a row of the `records` key space.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    /// The tid that wrote this version. It is the serial of the record.
    pub tid: Tid,
    /// The serial of the previous version of the object.
    pub prev: Tid,
    /// The pickle bytes. An empty value marks a deletion by pack or by
    /// `deleteObject`.
    pub data: Bytes,
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

    /// Return the tid of the last committed transaction.
    fn last_tid(&self) -> Tid;

    /// Open a write transaction. The core opens one for each `tpc_finish`.
    fn begin_write(&self) -> Result<Box<dyn WriteTxn + '_>>;

    /// Iterate over the transactions from `from` to `to`.
    fn iter_txns(&self, from: Tid, to: Tid) -> TxnIter<'_>;

    /// Return at most `limit` versions of `oid`, newest first.
    fn history(&self, oid: Oid, limit: usize) -> Result<Vec<RecordMeta>>;

    /// Remove the versions older than `before` that `reachable` rejects.
    fn pack(&self, before: Tid, reachable: &dyn Fn(Oid, Tid) -> bool) -> Result<PackStats>;

    /// Make all committed writes durable. Group commit uses this barrier.
    fn sync(&self) -> Result<()>;
}

/// One backend write transaction. It holds the records, the transaction
/// row and the meta update of one commit.
pub trait WriteTxn {
    /// Stage the version of `oid` that `tid` writes. `prev` is the serial
    /// of the previous version.
    fn put_record(&mut self, oid: Oid, tid: Tid, prev: Tid, data: &[u8]) -> Result<()>;

    /// Stage the transaction row.
    fn put_txn(&mut self, meta: &TxnMeta) -> Result<()>;

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

        fn put_txn(&mut self, _: &TxnMeta) -> Result<()> {
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

        fn pack(&self, _: Tid, _: &dyn Fn(Oid, Tid) -> bool) -> Result<PackStats> {
            Ok(PackStats::default())
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
