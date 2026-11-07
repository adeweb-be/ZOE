//! Dumps of a store: [`backup`], [`restore`] and [`verify_backup`].
//!
//! A backup directory holds a chain of dump files. The first one is the
//! full dump: every transaction of the store up to its last tid. Each later
//! one is an incremental dump: the transactions after the last tid of the
//! dump before it. A dump also holds the blob of each blob record, the end
//! of each sequence, every row of the durable KV namespaces with their
//! generation rows (ADR 0043), and the text deltas of each transaction
//! (ADR 0060, section 6). ADR 0030 gives the layout, and ADR 0060 its
//! version 3. A restore reads only version 3, and writes the durable rows
//! of the last dump of the chain. A backup and a restore refuse a dump of
//! version 2.
//!
//! A backup reads the store through its transaction iterator. It writes a
//! dump file once, under a temporary name, syncs it and renames it into
//! place. So it never changes a dump file that the directory holds.
//!
//! A restore checks every dump file before it writes anything. It builds
//! the store in a work directory beside the target, and renames the work
//! directory to the target at the end. So a restore that fails leaves
//! nothing at the target.

use std::path::Path;

use crate::Error;

// The module of the backup command has the name of its parent.
#[allow(clippy::module_inception)]
mod backup;
mod chain;
mod check;
mod dump;
mod restore;
#[cfg(test)]
mod tests;

pub use backup::{Backup, backup};
pub use check::{BackupCheck, verify_backup};
pub use restore::{Restored, restore};

/// Return the `Dump` error of the dump file `path`.
fn damaged(path: &Path, reason: String) -> Error {
    Error::Dump {
        path: path.to_owned(),
        reason,
    }
}
