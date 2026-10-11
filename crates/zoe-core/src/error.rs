//! The error type of the core.

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
    /// payload does not decode.
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
    /// payload does not decode, or a record has no transaction row, or the
    /// records of a transaction do not match the oid count of its row.
    #[error("corrupt transaction row: tid {tid:?}")]
    CorruptTxn {
        /// The transaction id of the row.
        tid: Tid,
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
    /// not after the last tid, or no tid or oid is left, or an oid is
    /// reserved.
    #[error("two-phase commit out of order: {0}")]
    Protocol(&'static str),

    /// A thread that did not begin the transaction called `store`,
    /// `restore`, `check_current_serial` or `vote`. The transaction refuses
    /// the call and does not change.
    #[error("the call came from a thread that did not begin the transaction")]
    WrongThread,

    /// A finish failed before, so the outcome of its commit is unknown.
    /// The store refuses every finish until it is reopened.
    #[error("a finish failed: reopen the store to commit again")]
    Halted,

    /// Another process holds the lock of the store.
    #[error("the store is locked by another process")]
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
    /// its store.
    #[error("the store is closed")]
    Closed,

    /// The store is not in a format that this code reads.
    #[error("unknown store format: {0}")]
    Format(String),

    /// The backend failed.
    #[error("backend error: {0}")]
    Backend(#[source] Box<dyn std::error::Error + Send + Sync>),

    /// An I/O operation failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
