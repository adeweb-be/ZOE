//! [`backup`]: write the next dump of a store into a backup directory.

use std::fs::{self, File};
use std::path::{Path, PathBuf};

use zoe_core::{Store, Tid};

use super::chain::{chain, dump_name, parse_name};
use super::dump::write_dump;
use crate::Error;
use crate::fsutil::{create_dirs, io_error, sibling, sync_dir, try_lock};

/// The suffix of a dump file that a backup still writes.
pub(super) const TEMP_SUFFIX: &str = ".tmp";

/// What a backup wrote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Backup {
    /// The dump file that the backup wrote. It is `None` when the store
    /// has no transaction after the dumps.
    pub dump: Option<PathBuf>,
    /// The last tid of the dump before this one. It is `Tid::ZERO` for the
    /// full dump.
    pub after: Tid,
    /// The last tid of the dump: the last tid of the store at the start of
    /// the backup.
    pub last: Tid,
    /// The transactions of the dump.
    pub transactions: u64,
    /// The records of the dump, deletions too.
    pub records: u64,
    /// The blobs of the dump.
    pub blobs: u64,
    /// The bytes of the blobs of the dump.
    pub blob_bytes: u64,
    /// The sequences of the dump.
    pub sequences: u64,
    /// The durable rows of the KV namespaces of the dump, the generation
    /// rows included.
    pub durable_rows: u64,
}

/// Write a dump of `store` into the backup directory `dir`, and return
/// what it wrote.
///
/// The first dump of `dir` is the full dump. A later dump holds the
/// transactions after the last tid of the dumps, up to the last tid of the
/// store at the start of the call. The call reads the end of each sequence
/// after it reads that last tid. So each value of a sequence that a
/// transaction of the dump holds is below its end in the dump. When the
/// dumps end at the last tid of the store, the call writes nothing.
///
/// The call creates `dir` when it is missing. It takes a lock on `dir`, so
/// two backups never write into one directory together. It removes the
/// temporary dump files that a stopped backup left. It never changes a dump
/// file. A dump that does not follow the dump before it fails the call with
/// `Dump`. When the last tid of the dumps is not a tid of the store, the
/// call fails with `BackupDiverged` and writes nothing. When the store was
/// packed after that tid, it fails with `BackupPacked` and writes nothing.
/// A pack at that tid removes only history that the dumps hold, so the
/// backup goes on. Both errors ask for a new full dump in a new directory.
///
/// For an exact dump, no writer changes the store during the call. A
/// read-only open makes sure of this.
pub fn backup(store: &Store, dir: &Path) -> Result<Backup, Error> {
    create_dirs(dir).map_err(|source| io_error(dir, source))?;
    let _lock = lock_dir(dir)?;
    remove_temp_files(dir)?;
    let dumps = chain(dir)?;
    let after = dumps.last().map_or(Tid::ZERO, |dump| dump.last);
    let last = store.last_transaction();
    let mut report = Backup {
        dump: None,
        after,
        last,
        transactions: 0,
        records: 0,
        blobs: 0,
        blob_bytes: 0,
        sequences: 0,
        durable_rows: 0,
    };
    if !dumps.is_empty() {
        if last == after {
            return Ok(report);
        }
        continues(store, dir, after, last)?;
    }

    let path = dir.join(dump_name(after, last));
    let temp = sibling(&path, TEMP_SUFFIX)?;
    let written = write_dump(store, &temp, after, last, &mut report)
        .and_then(|()| fs::rename(&temp, &path).map_err(|source| io_error(&path, source)))
        .and_then(|()| sync_dir(dir));
    if written.is_err() {
        let _ = fs::remove_file(&temp);
    }
    written?;
    report.dump = Some(path);
    Ok(report)
}

/// Check that the store continues the dumps that end at `after`: `after`
/// is a tid of the store, before its last tid `last`, and no pack of the
/// store went past `after`.
///
/// The pack watermark does not tell one store from another, so a missing
/// transaction always refuses: a store restored from a part of the dumps,
/// or another store, never continues them.
fn continues(store: &Store, dir: &Path, after: Tid, last: Tid) -> Result<(), Error> {
    let diverged = || Error::BackupDiverged {
        path: dir.to_owned(),
        last: after,
    };
    if after == Tid::ZERO {
        return Ok(());
    }
    if last < after {
        return Err(diverged());
    }
    match store.iterator(Some(after), Some(after))?.next() {
        Some(Ok(txn)) if txn.tid == after => {}
        Some(Err(error)) => return Err(error.into()),
        _ => return Err(diverged()),
    }
    // A pack after `after` changed history that the next dump must hold.
    // A pack at `after` removed only history that the dumps hold.
    let watermark = store.pack_watermark()?;
    if watermark > after {
        return Err(Error::BackupPacked {
            path: dir.to_owned(),
            last: after,
            watermark,
        });
    }
    Ok(())
}

/// Take the lock of the backup directory `dir`: an exclusive `flock` on a
/// descriptor of the directory itself. The lock adds no file.
pub(super) fn lock_dir(dir: &Path) -> Result<File, Error> {
    let file = File::open(dir).map_err(|source| io_error(dir, source))?;
    try_lock(file, dir)
}

/// Remove the temporary dump files of `dir`, which a stopped backup left.
/// The caller holds the lock of `dir`.
fn remove_temp_files(dir: &Path) -> Result<(), Error> {
    let entries = fs::read_dir(dir).map_err(|source| io_error(dir, source))?;
    for entry in entries {
        let entry = entry.map_err(|source| io_error(dir, source))?;
        let temp = entry
            .file_name()
            .to_str()
            .and_then(|name| name.strip_suffix(TEMP_SUFFIX))
            .is_some_and(|name| parse_name(name).is_some());
        if temp {
            let path = entry.path();
            fs::remove_file(&path).map_err(|source| io_error(&path, source))?;
        }
    }
    Ok(())
}
