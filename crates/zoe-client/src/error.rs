//! The errors of the client.

use std::io;

use zoe_core::{Oid, Tid};

/// An error of a call of the client.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The server answered with an error. `class` is the class name of the
    /// exception, as `ZODB.POSException.POSKeyError`. `message` shows the
    /// value of the exception.
    #[error("{class}: {message}")]
    Server {
        /// The module and the name of the exception class.
        class: String,
        /// The args or the attributes of the exception, as text.
        message: String,
    },

    /// A store of the transaction has a stale serial: a
    /// `ZODB.POSException.ConflictError`, or a conflict that the vote
    /// listed. `serials` is `(committed, read)`, or `None` when the server
    /// did not give them, as after a lock-wait timeout.
    #[error("a conflict on the oid {oid:?}, serials {serials:?}")]
    Conflict {
        /// The oid of the conflict.
        oid: Oid,
        /// The committed serial and the serial that the transaction read.
        serials: Option<(Tid, Tid)>,
    },

    /// The connection to the server is lost or closed. Every call that
    /// waits for a reply fails with it, and so does every later call.
    #[error("the connection to the server is lost")]
    Disconnected,

    /// The server sent a frame that the protocol does not allow.
    #[error("the server breaks the protocol: {0}")]
    Protocol(String),

    /// An argument that the client cannot send.
    #[error("the client cannot send this: {0}")]
    Invalid(String),

    /// The receiver of invalidations missed this number of invalidations,
    /// because it read too slowly.
    #[error("{0} invalidations were missed")]
    Lagged(u64),

    /// The connection to the server failed to open.
    #[error(transparent)]
    Io(#[from] io::Error),
}
