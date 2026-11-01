//! The helpers that the unit tests of the crate share.

use zoe_core::{Oid, Store, Tid};

/// Commit a restore transaction of `writes` at the tid after the last tid
/// of `store`. A write is an oid and its pickle, or `None` for a
/// deletion. Return the tid.
pub(crate) fn commit(store: &Store, writes: &[(u64, Option<Vec<u8>>)]) -> Tid {
    let tid = Tid(store.last_transaction().0 + 1);
    let mut txn = store.tpc_begin_restore(tid, b"", b"", b"").unwrap();
    for (oid, data) in writes {
        txn.restore(Oid(*oid), data.as_deref()).unwrap();
    }
    txn.vote().unwrap();
    txn.finish().unwrap()
}
