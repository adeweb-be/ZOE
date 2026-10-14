//! The error type of the core.

use std::path::PathBuf;

use crate::guard::SizeLimit;
use crate::types::{Oid, Tid};

/// The result type of the core.
pub type Result<T> = std::result::Result<T, ZoeError>;

/// An error of the core.
#[derive(Debug, thiserror::Error)]
pub enum ZoeError {
    /// The store has no record of the object.
    #[error("no record: oid {oid:?}")]
    NotFound {
        /// The object id that has no record.
        oid: Oid,
    },

    /// The checksum of a stored record does not match its bytes, or its
    /// payload does not decode, or a scan for the oid gave the key of the
    /// tid outside the scan or out of order. The last case is a damaged
    /// index of the database.
    #[error("corrupt record: oid {oid:?}, tid {tid:?}")]
    CorruptRecord {
        /// The object id of the record.
        oid: Oid,
        /// The transaction id of the record.
        tid: Tid,
    },

    /// A blob file does not match the size and the BLAKE3 hash in its blob
    /// record, or the file is missing, or the blob record is damaged.
    #[error("corrupt blob: oid {oid:?}, tid {tid:?}")]
    CorruptBlob {
        /// The object id of the blob.
        oid: Oid,
        /// The transaction id of the blob.
        tid: Tid,
    },

    /// The checksum of a transaction row does not match its bytes, or its
    /// payload does not decode, or a record or an index key has no
    /// transaction row, or an index key has no record, or the index keys of
    /// a transaction do not match the oid count of its row, or a scan of
    /// the rows or of the index gave a key outside the scan or out of
    /// order.
    #[error("corrupt transaction row: tid {tid:?}")]
    CorruptTxn {
        /// The transaction id of the row.
        tid: Tid,
    },

    /// The checksum of a sequence row does not match the name and the
    /// bytes, or its payload does not decode.
    #[error("corrupt sequence row: {name:?}")]
    CorruptSequence {
        /// The name of the sequence.
        name: String,
    },

    /// Another transaction changed the object since the serial that this
    /// transaction read, or held its lock past the lock timeout.
    /// `Txn::conflict_serials` gives the serials of a stale serial.
    #[error("conflict: oid {oid:?}")]
    Conflict {
        /// The object id in conflict.
        oid: Oid,
    },

    /// Another transaction changed an object that this transaction read as
    /// current, since the serial that this transaction read. This is the
    /// `ReadConflictError` of ZODB. `Txn::conflict_serials` gives the
    /// serials.
    #[error("read conflict: oid {oid:?}")]
    ReadConflict {
        /// The object id that this transaction read as current.
        oid: Oid,
    },

    /// A lock request is not in ascending oid order. The lock manager
    /// refuses it and takes no lock.
    #[error("lock request out of oid order: oid {oid:?}")]
    LockOrder {
        /// The first oid that is not greater than the oid before it.
        oid: Oid,
    },

    /// The store refuses a call. A two-phase commit call came out of order,
    /// for example `finish` before `vote`, or a restore gave a tid that is
    /// not after the last tid, or no tid, oid or sequence value is left, or
    /// an oid is reserved.
    #[error("two-phase commit out of order: {0}")]
    Protocol(&'static str),

    /// A `store` or a `store_blob` is over a size limit of the store. The
    /// transaction refuses the call and stages nothing. The message names
    /// the configuration key of the limit, and holds no pickle bytes.
    #[error(
        "store of oid {oid:?} refused: {} of {size} bytes, over {limit} ({max} bytes)",
        .limit.subject()
    )]
    TooLarge {
        /// The oid of the refused call.
        oid: Oid,
        /// The limit that refused the call.
        limit: SizeLimit,
        /// The size that the call gives, in bytes: the pickle, the sum of
        /// the pickles of the transaction, or the blob.
        size: u64,
        /// The limit, in bytes.
        max: u64,
    },

    /// A thread that did not begin the transaction called `store`,
    /// `store_blob`, `restore`, `check_current_serial` or `vote`. The
    /// transaction refuses the call and does not change.
    #[error("the call came from a thread that did not begin the transaction")]
    WrongThread,

    /// A finish failed before, so the outcome of its commit is unknown.
    /// The store refuses every finish until it is reopened.
    #[error("a finish failed: reopen the store to commit again")]
    Halted,

    /// Another open store holds the lock of the store directory, or of the
    /// blob directory.
    #[error("the store or its blob directory is locked by another open store")]
    Locked,

    /// The store is open for reads only. It refuses every write.
    #[error("the store is open for reads only")]
    ReadOnly,

    /// The store was not closed cleanly. A read-only open cannot rebuild
    /// the free-space map of its database. Open the store once with a
    /// writer: the writer rebuilds it.
    #[error(
        "the store was not closed cleanly: open it once with a writer, which rebuilds the free-space map of its database"
    )]
    NotClosedCleanly,

    /// The store is closed. A transaction iterator or a blob file outlived
    /// its store. `Store::pack_until` also gives it when its stop ends the
    /// pack between two batches.
    #[error("the store is closed")]
    Closed,

    /// The store has blob records, but its blob directory has no layout
    /// file. The first blob of a store creates the layout file before any
    /// blob record, so this is not the blob directory of the store: the
    /// blob directory option is missing or wrong, or the directory is not
    /// mounted. A writer open refuses the store, so that it does not start
    /// a second blob directory.
    #[error(
        "the store has blobs, but the blob directory {} has no layout file: open the store with its blob directory",
        path.display()
    )]
    MissingBlobDir {
        /// The blob directory that the open found without a layout file.
        path: PathBuf,
    },

    /// The blob directory has no layout file, and it holds files of
    /// another program. A writer open refuses it, so that the recovery of
    /// the blob store never removes those files. A blob directory without
    /// a layout file can hold only an empty `tmp/`, or a `tmp/` with only
    /// the new layout file that a stopped first blob left.
    #[error(
        "the blob directory {} has no layout file and holds other files: give an empty or a missing directory",
        path.display()
    )]
    NotBlobDir {
        /// The blob directory.
        path: PathBuf,
    },

    /// The store is not in a format that this code reads. Pack also gives
    /// it for a record whose persistent references it cannot read: the
    /// text then names the oid and the tid of the record.
    #[error("unknown store format: {0}")]
    Format(String),

    /// The backend failed.
    #[error("backend error: {0}")]
    Backend(#[source] Box<dyn std::error::Error + Send + Sync>),

    /// An I/O operation failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
