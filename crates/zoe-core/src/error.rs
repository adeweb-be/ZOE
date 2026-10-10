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

    /// The checksum of a stored record does not match its bytes.
    #[error("corrupt record: oid {oid:?}, tid {tid:?}")]
    CorruptRecord {
        /// The object id of the record.
        oid: Oid,
        /// The transaction id of the record.
        tid: Tid,
    },

    /// The checksum of a transaction row does not match its bytes, or its
    /// payload does not decode.
    #[error("corrupt transaction row: tid {tid:?}")]
    CorruptTxn {
        /// The transaction id of the row.
        tid: Tid,
    },

    /// Another transaction changed the object since the serial that this
    /// transaction read, or held its lock past the lock timeout.
    #[error("conflict: oid {oid:?}")]
    Conflict {
        /// The object id in conflict.
        oid: Oid,
    },

    /// A two-phase commit call came out of order, for example `finish`
    /// before `vote`.
    #[error("two-phase commit out of order: {0}")]
    Protocol(&'static str),

    /// Another process holds the lock of the store.
    #[error("the store is locked by another process")]
    Locked,

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
