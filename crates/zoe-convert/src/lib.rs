//! Converters: `Data.fs` to ZOE and ZOE to `Data.fs`. Each conversion
//! verifies its result and writes a report.
//!
//! [`from_datafs`] converts a `Data.fs` and its blob directory into a ZOE
//! store. It reads the file as a stream, restores each transaction under
//! its own tid, and resumes after the last tid of the store.
//!
//! [`to_datafs`] converts a ZOE store into a `Data.fs` and a blob
//! directory that ZODB's FileStorage opens. It writes the file under a
//! temporary name and renames it at the end, and resumes after the last tid
//! of the file.
//!
//! [`verify`] checks a store without Python: the checksums of its frames,
//! its blob files, its persistent references and its classes.
//!
//! [`records_of_classes`] finds the current records of some classes, from
//! their class pickles, without Python.
//!
//! [`backup`] writes a dump of a store into a backup directory: a full
//! dump the first time, then the transactions after the last dump, with
//! their blobs and the ends of the sequences. [`restore`] rebuilds the
//! store from the dumps, and [`verify_backup`] compares them with a store.

use std::path::PathBuf;

use zoe_core::{Tid, ZoeError};

mod backup;
mod blobs;
mod classes;
mod compare;
mod datafs;
mod filestorage;
mod fsutil;
mod report;
#[cfg(test)]
mod testing;
mod to_datafs;
mod verify;
mod walk;
mod writer;

pub use backup::{Backup, BackupCheck, Restored, backup, restore, verify_backup};
pub use classes::{ClassRecord, records_of_classes};
pub use datafs::from_datafs;
pub use report::{ProblemKind, Report};
pub use to_datafs::to_datafs;
pub use verify::{
    BlobProblem, CountProblem, Dangling, FrameProblem, IdForm, PickleProblem, Verification,
    missing_blob_dir, verify,
};

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
        "the store does not continue the source: its last tid {} is not a tid of the source",
        .last.hex()
    )]
    Diverged {
        /// The last tid of the store.
        last: Tid,
    },

    /// The `Data.fs` that a conversion appends to is not empty, and its
    /// last tid is not a tid of the store. So the file does not hold a part
    /// of the store, and the conversion writes nothing.
    #[error(
        "{}: the Data.fs does not continue the store: its last tid {} is not a tid of the store",
        path.display(),
        .last.hex()
    )]
    DataFsDiverged {
        /// The path of the file.
        path: PathBuf,
        /// The last tid of the file.
        last: Tid,
    },

    /// The verification found that the `Data.fs` does not match the store.
    /// The conversion keeps the old file and removes the file that it
    /// wrote.
    #[error(
        "{}: the Data.fs does not match the store: {}",
        path.display(),
        .problems.first().map_or("", String::as_str)
    )]
    Mismatch {
        /// The path of the file.
        path: PathBuf,
        /// The differences, one per line, with the cap of
        /// `Report.problems`.
        problems: Vec<String>,
    },

    /// A field of a transaction is longer than a `Data.fs` holds. The
    /// header of a transaction record holds the length of the user, the
    /// description and the extension in 2 bytes each.
    #[error(
        "transaction {}: the {field} has {len} bytes, and a Data.fs holds at most 65535",
        .tid.hex()
    )]
    TooLong {
        /// The tid of the transaction.
        tid: Tid,
        /// The name of the field: `user`, `description` or `extension`.
        field: &'static str,
        /// The length of the field.
        len: usize,
    },

    /// Another process holds the lock of the `Data.fs`, for example a
    /// FileStorage that has the file open for writing. Or another backup
    /// holds the lock of the backup directory.
    #[error("{}: another process holds the lock", path.display())]
    Locked {
        /// The path of the lock file, or of the backup directory.
        path: PathBuf,
    },

    /// The blob directory of the output has the lawn layout. The converter
    /// writes only the bushy layout.
    #[error("{}: the blob directory has the lawn layout, not bushy", path.display())]
    NotBushy {
        /// The path of the blob directory.
        path: PathBuf,
    },

    /// A dump file is damaged, or does not follow the dump before it in
    /// its backup directory.
    #[error("{}: {reason}", path.display())]
    Dump {
        /// The path of the dump file.
        path: PathBuf,
        /// What is wrong.
        reason: String,
    },

    /// The backup directory holds no dump.
    #[error("{}: no dump", path.display())]
    NoDump {
        /// The path of the backup directory.
        path: PathBuf,
    },

    /// The target of a restore holds a store or other files, or is not a
    /// directory. The restore writes nothing.
    #[error("{}: {reason}", path.display())]
    Target {
        /// The path of the target.
        path: PathBuf,
        /// What is wrong.
        reason: &'static str,
    },

    /// The last tid of the dumps is not a tid of the store. So the store
    /// does not continue the dumps: they are dumps of another store, or of
    /// another history of this store. The backup writes nothing.
    #[error(
        "{}: the store does not continue the dumps: their last tid {} is not a tid of the store; \
         write a new full dump into a new directory",
        path.display(),
        .last.hex()
    )]
    BackupDiverged {
        /// The path of the backup directory.
        path: PathBuf,
        /// The last tid of the dumps.
        last: Tid,
    },

    /// The store was packed after the last tid of the dumps. The pack
    /// changed history that the next dump must hold, so the backup writes
    /// nothing.
    #[error(
        "{}: the store was packed at tid {}, after the last tid {} of the dumps; \
         write a new full dump into a new directory",
        path.display(),
        .watermark.hex(),
        .last.hex()
    )]
    BackupPacked {
        /// The path of the backup directory.
        path: PathBuf,
        /// The last tid of the dumps.
        last: Tid,
        /// The pack watermark of the store.
        watermark: Tid,
    },

    /// A read or a write of a file failed.
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
