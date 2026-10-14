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
mod lock;
mod pack;
mod read;
mod redb_backend;
mod sequence;
mod store;
#[cfg(feature = "testing")]
#[doc(hidden)]
pub mod testing;
mod types;
pub mod vfs;

pub use backend::{
    Backend, BlobRecord, Bytes, NewRecord, PackStats, Record, RecordEncoding, RecordMeta, TxnIter,
    TxnMeta, TxnRecords, TxnStatus, WriteTxn,
};
pub use blob::{BlobFile, BlobReader};
pub use clock::{Clock, SystemClock, Waiter, timestamp};
pub use commit::{Finished, Txn};
pub use error::{Result, ZoeError};
pub use format::{Codec, Compression};
pub use guard::SizeLimit;
pub use pack::{ClassTotals, PackReport};
pub use read::{CurrentRecord, HistoryEntry, LoadedRecord, RecordInfo, Transactions, TxnInfo};
pub use sequence::Sequence;
pub use store::{Options, Store};
pub use types::{Oid, Tid};
