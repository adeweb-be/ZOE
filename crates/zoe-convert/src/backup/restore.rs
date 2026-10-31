//! [`restore`]: rebuild a store from the dumps of a backup directory.

use std::collections::BTreeSet;
use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use zoe_core::{Store, Tid};

use super::chain::{DumpFile, chain};
use super::dump::{DumpReader, Entry, Head, check_hash};
use crate::Error;
use crate::fsutil::{create_dirs, exists, io_error, parent, sibling, sync_dir};

/// The suffix of the work directory of a restore, beside the target.
pub(super) const WORK_SUFFIX: &str = ".restore";

/// The database file in the directory of a store. A restore refuses a
/// target with any entry of this name, also a directory or a link.
pub(super) const DATABASE: &str = "zoe.redb";

/// What a restore read and wrote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Restored {
    /// The dump files.
    pub dumps: u64,
    /// The transactions.
    pub transactions: u64,
    /// The records, deletions too.
    pub records: u64,
    /// The blobs.
    pub blobs: u64,
    /// The sequences.
    pub sequences: u64,
    /// The durable rows of the KV namespaces of the last dump, the
    /// generation rows included.
    pub durable_rows: u64,
    /// The last tid of the last dump.
    pub last_tid: Tid,
}

/// Restore the store in the backup directory `dir` into `target`, and
/// return what the restore read and wrote.
///
/// The restore reads the dumps in order and commits each transaction
/// under its own tid, with its records and its blobs. It checks the BLAKE3
/// hash of each blob. Each sequence restarts at or above its end in the
/// dumps.
///
/// `target` must not exist, or be an empty directory on the filesystem of
/// its parent: a directory that holds a store or other files, or a mount
/// point, fails the call with `Target`. The call checks the hash of every
/// dump file before it writes anything, in a pass that reads each file as
/// a stream. A damaged dump file, or a dump that does not follow the dump
/// before it, fails the call with `Dump`. A directory without a dump fails
/// with `NoDump`. A file whose hash matches but that breaks the layout
/// fails the call while it builds the store.
///
/// The restore builds the store in a work directory beside `target`: the
/// name of `target` with the suffix `.restore`. It renames the work
/// directory to `target` at the end. When the call fails, it removes the
/// work directory. A work directory that exists fails the call, because
/// another restore can be writing into it.
pub fn restore(dir: &Path, target: &Path) -> Result<Restored, Error> {
    check_target(target)?;
    let dumps = chain(dir)?;
    if dumps.is_empty() {
        return Err(Error::NoDump {
            path: dir.to_owned(),
        });
    }
    for dump in &dumps {
        check_hash(&dump.path)?;
    }

    let parent = parent(target);
    create_dirs(parent).map_err(|source| io_error(parent, source))?;
    let work = sibling(target, WORK_SUFFIX)?;
    match fs::create_dir(&work) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            return Err(Error::Target {
                path: work,
                reason: "exists: another restore writes into it, or a restore stopped; \
                         remove it when no restore runs",
            });
        }
        Err(error) => return Err(io_error(&work, error)),
    }
    let restored = build(&dumps, &work)
        .and_then(|report| {
            fs::rename(&work, target).map_err(|source| io_error(target, source))?;
            Ok(report)
        })
        .and_then(|report| sync_dir(parent).map(|()| report));
    if restored.is_err() {
        let _ = fs::remove_dir_all(&work);
    }
    restored
}

/// Build the store in `work` from `dumps`, which the caller checked.
fn build(dumps: &[DumpFile], work: &Path) -> Result<Restored, Error> {
    let store = Store::open(work)?;
    let mut report = Restored {
        dumps: 0,
        transactions: 0,
        records: 0,
        blobs: 0,
        sequences: 0,
        durable_rows: 0,
        last_tid: Tid::ZERO,
    };
    let mut names = BTreeSet::new();
    // The durable rows of the last dump: each dump holds every row.
    let mut rows = Vec::new();
    for dump in dumps {
        let mut reader = DumpReader::open(dump)?;
        rows.clear();
        loop {
            match reader.next()? {
                Entry::Sequence { name, end } => {
                    store.sequence(&name).restore_end(end)?;
                    names.insert(name);
                }
                Entry::Durable(row) => rows.push(row),
                Entry::Transaction(head) => restore_txn(&store, &mut reader, &head, &mut report)?,
                Entry::Record { .. } => {
                    return Err(reader.damaged("a record outside a transaction".to_owned()));
                }
                Entry::End => break,
            }
        }
        report.dumps += 1;
        report.last_tid = dump.last;
    }
    report.sequences = names.len() as u64;
    // One backend write holds them all.
    store.kv_restore_durable(&rows)?;
    report.durable_rows = rows.len() as u64;
    Ok(report)
}

/// Restore the transaction `head` and its records from `reader`.
fn restore_txn(
    store: &Store,
    reader: &mut DumpReader,
    head: &Head,
    report: &mut Restored,
) -> Result<(), Error> {
    let mut txn =
        store.tpc_begin_restore(head.tid, &head.user, &head.description, &head.extension)?;
    for _ in 0..head.records {
        let Entry::Record { oid, data, blob } = reader.next()? else {
            return Err(reader.damaged("a transaction without all its records".to_owned()));
        };
        match (data, blob) {
            (None, _) => txn.restore(oid, None)?,
            (Some(data), None) => txn.restore(oid, Some(&data))?,
            (Some(data), Some(_)) => {
                let mut file = store.new_blob_file()?;
                reader.blob(&mut file)?;
                txn.restore_blob(oid, &data, file)?;
                report.blobs += 1;
            }
        }
        report.records += 1;
    }
    txn.vote()?;
    txn.finish()?;
    report.transactions += 1;
    Ok(())
}

/// Refuse a target that holds a store or other files. A target that does
/// not exist, or an empty directory, passes.
fn check_target(target: &Path) -> Result<(), Error> {
    let refuse = |reason| {
        Err(Error::Target {
            path: target.to_owned(),
            reason,
        })
    };
    match fs::symlink_metadata(target) {
        Ok(meta) if !meta.is_dir() => refuse("not a directory"),
        Ok(_) if exists(&target.join(DATABASE))? => {
            refuse("holds a store; a restore never writes into a store")
        }
        // The restore builds the store beside the target and renames it,
        // and a rename never crosses a filesystem.
        Ok(meta) if meta.dev() != parent_device(target)? => refuse(
            "a mount point; a restore builds the store beside its target, so give a directory inside it",
        ),
        Ok(_) => {
            let mut entries = fs::read_dir(target).map_err(|source| io_error(target, source))?;
            match entries.next() {
                None => Ok(()),
                Some(_) => {
                    refuse("not empty; a restore writes only into a new or an empty directory")
                }
            }
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_error(target, error)),
    }
}

/// Return the device of the parent directory of `path`.
fn parent_device(path: &Path) -> Result<u64, Error> {
    let parent = parent(path);
    fs::metadata(parent)
        .map(|meta| meta.dev())
        .map_err(|source| io_error(parent, source))
}
