//! The error type of the core.

use crate::types::{Oid, Tid};

/// The result type of the core.
pub type Result<T> = std::result::Result<T, ZoeError>;

/// An error of the core.
#[derive(Debug, thiserror::Error)]
pub enum ZoeError {
    /// The checksum of a stored record does not match its bytes.
    #[error("corrupt record: oid {oid:?}, tid {tid:?}")]
    CorruptRecord {
        /// The object id of the record.
        oid: Oid,
        /// The transaction id of the record.
        tid: Tid,
    },

    /// The backend failed.
    #[error("backend error: {0}")]
    Backend(#[source] Box<dyn std::error::Error + Send + Sync>),

    /// An I/O operation failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
