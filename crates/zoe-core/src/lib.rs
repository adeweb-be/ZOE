//! The ZOE storage core.
//!
//! This crate is synchronous. It has no tokio and no Python inside.
//! Both skins call it: the in-process storage (`zoe-py`) and the server
//! (`zoe-server`). The core owns correctness: the data model, two-phase
//! commit, per-OID locks, conflict handling, pack and blobs. A backend
//! owns bytes, behind the [`Backend`] trait.

mod backend;
mod blob;
mod clock;
mod commit;
mod error;
mod format;
mod guard;
mod kv;
mod lock;
pub mod merge;
mod pack;
mod read;
mod redb_backend;
mod sequence;
mod store;
#[cfg(feature = "testing")]
#[doc(hidden)]
pub mod testing;
mod types;
mod undo;
pub mod vfs;

pub use backend::{
    Backend, BlobRecord, Bytes, NewPayload, NewRecord, PackStats, Payload, Record, RecordEncoding,
    RecordMeta, TxnIter, TxnMeta, TxnRecords, TxnStatus, WriteTxn,
};
pub use blob::{BlobFile, BlobReader};
pub use clock::{Clock, SystemClock, Waiter, civil_from_days, civil_from_tid, timestamp};
pub use commit::{Encoded, Finished, IntentLocks, Observer, Txn};
pub use error::{Result, ZoeError};
pub use format::{Codec, Compression};
pub use guard::SizeLimit;
pub use kv::{
    DurableRow, Flavour, KvCheck, KvFinding, MAX_TTL_SECONDS, SCAN_BYTES, SCAN_LIMIT,
    SWEEP_INTERVAL, SWEEP_LIMIT, Swept,
};
pub use lock::LockCancel;
pub use merge::{Mode, Reason, Resolution, Resolved, StructuralMerges};
pub use pack::{ClassTotals, PackReport};
pub use read::{CurrentRecord, HistoryEntry, LoadedRecord, RecordInfo, Transactions, TxnInfo};
pub use sequence::Sequence;
pub use store::{Options, Store};
pub use types::{Oid, Tid};
pub use undo::{UndoConflict, Undone, undo_id, undo_tid};

/// Lock `mutex`. A thread that panicked while it held the mutex does not
/// stop the others: they take the data as that thread left it.
pub(crate) fn lock<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
