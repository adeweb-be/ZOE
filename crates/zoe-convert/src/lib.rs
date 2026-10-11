//! Converters: `Data.fs` to ZOE, RelStorage to ZOE, ZOE to `Data.fs` and
//! ZOE to ZOE. Each conversion verifies its result and writes a report.

use std::path::PathBuf;

use zoe_core::{Tid, ZoeError};

mod filestorage;

/// An error that stops a conversion.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The file does not start with the magic of a FileStorage file.
    #[error("{}: not a FileStorage file: the magic is \"{magic}\", not \"FS30\"", path.display())]
    NotDataFs {
        /// The path of the file.
        path: PathBuf,
        /// The first bytes of the file, escaped.
        magic: String,
    },

    /// The file breaks the FileStorage format.
    #[error("{}: corrupt at offset {pos}: {reason}", path.display())]
    Corrupt {
        /// The path of the file.
        path: PathBuf,
        /// The offset of the transaction or the record that breaks the
        /// format.
        pos: u64,
        /// What is wrong.
        reason: String,
    },

    /// The `.layout` file of the blob directory names a layout that is not
    /// bushy or lawn.
    #[error("{}: unknown blob layout \"{layout}\"", path.display())]
    Layout {
        /// The path of the blob directory.
        path: PathBuf,
        /// The name in the `.layout` file.
        layout: String,
    },

    /// The store is not empty, and its last tid is not a tid of the
    /// source. So the store does not hold a part of the source, and the
    /// conversion copies nothing.
    #[error(
        "the store does not continue the source: its last tid {last:?} is not a tid of the source"
    )]
    Diverged {
        /// The last tid of the store.
        last: Tid,
    },

    /// A read of the source failed.
    #[error("{}: {source}", path.display())]
    Io {
        /// The path of the file or the directory.
        path: PathBuf,
        /// The error.
        #[source]
        source: std::io::Error,
    },

    /// The store failed.
    #[error(transparent)]
    Store(#[from] ZoeError),
}
