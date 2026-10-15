//! The errors of the client.

use std::io;

use rmpv::Value;
use zoe_core::{Oid, Tid};

/// An error of a call of the client.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The server answered with an error. `class` is the class name of the
    /// exception, as `ZODB.POSException.POSKeyError`. `message` shows the
    /// value of the exception. `value` is that value as the server sent
    /// it: the array of the args, or the map of the attributes.
    #[error("{class}: {message}")]
    Server {
        /// The module and the name of the exception class.
        class: String,
        /// The args or the attributes of the exception, as text.
        message: String,
        /// The args or the attributes of the exception, as the server
        /// sent them. So a bytes argument, such as an oid, stays bytes.
        value: Value,
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
        /// The class of the object, when the error of the server names it
        /// in `class_name`, as a stock server does after its own
        /// resolution failed.
        class_name: Option<String>,
    },

    /// An object that the transaction read is no longer current: a
    /// `ZODB.POSException.ReadConflictError` of the vote, after
    /// `Transaction::check_current_serial`. `serials` is `(committed,
    /// read)`, or `None` when the server did not give them.
    #[error("a read conflict on the oid {oid:?}, serials {serials:?}")]
    ReadConflict {
        /// The oid of the conflict.
        oid: Oid,
        /// The committed serial and the serial that the transaction read.
        serials: Option<(Tid, Tid)>,
    },

    /// The call needs an extension that the server did not accept on this
    /// connection. The client sent nothing.
    #[error("the server does not serve this: {0}")]
    Unsupported(String),

    /// The connection to the server is lost or closed. Every call that
    /// waits for a reply fails with it, and so does every later call.
    #[error("the connection to the server is lost")]
    Disconnected,

    /// The server sent a frame that the protocol does not allow.
    #[error("the server breaks the protocol: {0}")]
    Protocol(String),

    /// The server speaks another protocol than `M5`, as a ZEO 4 server
    /// (`Z4`) or a ZEO 5 server without msgpack (`Z5`). The client sent
    /// nothing. As for `ZEO.ClientStorage`, the open counts it as a failed
    /// connection.
    #[error("the server speaks the protocol {}", String::from_utf8_lossy(.0))]
    OtherProtocol(Vec<u8>),

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
