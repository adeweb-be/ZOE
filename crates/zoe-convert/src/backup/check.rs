//! [`verify_backup`]: compare the dumps of a backup directory with a
//! store, through a copy that the dumps restore.

use std::borrow::Borrow;
use std::fs;
use std::io::ErrorKind;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use zoe_core::{DurableRow, Oid, Store, Tid, TxnInfo, ZoeError};

use super::restore::{Restored, restore};
use crate::Error;
use crate::compare::{RecordDiff, diff_records, txn_records};
use crate::fsutil::io_error;
use crate::report::Problems;
use crate::walk::{OidVersions, each_durable_row};

/// The start of the name of the copy that `verify_backup` restores, before
/// the id of its process.
pub(super) const CHECK_PREFIX: &str = "zoe-backup-check-";

/// The result of [`verify_backup`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackupCheck {
    /// What the restore of the dumps read.
    pub restored: Restored,
    /// The differences between the dumps and the store, one per line. At
    /// most 100 lines, and a last line counts the others.
    pub differences: Vec<String>,
}

/// Restore the dumps of the backup directory `dir` into a new directory in
/// `scratch`, open the store with `open`, compare the two, and remove that
/// directory.
///
/// The call builds the copy before it calls `open`. So the store stays
/// free while the copy is built, and a dump that does not restore fails
/// the call before the open. The copy is a throwaway store: the call
/// builds it with [`restore`] and removes it at the end, also when the
/// call fails. The call writes nothing outside `scratch`. It drops the
/// store of `open` before it returns.
///
/// The comparison covers the transactions of the store up to the last tid
/// of the dumps, so a commit after the last dump is no difference. Each
/// transaction and each record must be the same, with the same blob record:
/// the size and the BLAKE3 hash of its blob. A pack removes records at or
/// before its tid, and the dumps keep them. So at or before the pack
/// watermark of the store, a record or a transaction that the store lacks
/// is no difference. A pack after the last tid of the dumps is a
/// difference: it changed history that the next dump must hold. A pack at
/// that tid is no difference. The current record of each oid at the last
/// tid of the dumps must be the same too, so a store whose history
/// diverged differs, unless a pack removed that record. In the store, the
/// end of each sequence of the dumps must be at or above its end in the
/// dumps.
///
/// A dump that does not restore fails the call with its error, and an
/// open that fails fails the call with the error of `open`. The call
/// first removes the directories in `scratch` that a check in a process
/// that is gone left.
pub fn verify_backup<S: Borrow<Store>, E: From<Error>>(
    dir: &Path,
    scratch: &Path,
    open: impl FnOnce() -> Result<S, E>,
) -> Result<BackupCheck, E> {
    static CHECKS: AtomicU64 = AtomicU64::new(0);
    remove_stale_checks(scratch);
    let n = CHECKS.fetch_add(1, Ordering::Relaxed);
    let copy = scratch.join(format!("{CHECK_PREFIX}{}-{n}", std::process::id()));
    let checked = restore(dir, &copy).map_err(E::from).and_then(|restored| {
        let opened = Store::open_read_only(&copy).map_err(|e| E::from(Error::from(e)))?;
        let store = open()?;
        let differences = compare(store.borrow(), &opened, restored.last_tid).map_err(E::from)?;
        Ok(BackupCheck {
            restored,
            differences,
        })
    });
    match fs::remove_dir_all(&copy) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) if checked.is_ok() => return Err(io_error(&copy, error).into()),
        Err(_) => {}
    }
    checked
}

/// Remove each directory of a check in `scratch` whose process is gone: a
/// copy, or the work directory of its restore. The name of such a
/// directory holds the id of its process. A process that `/proc` lists
/// keeps its directories.
fn remove_stale_checks(scratch: &Path) {
    let Ok(entries) = fs::read_dir(scratch) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .and_then(|name| name.strip_prefix(CHECK_PREFIX))
            .and_then(|rest| rest.split('-').next())
            .and_then(|pid| pid.parse::<u32>().ok())
        else {
            continue;
        };
        if pid != std::process::id() && !Path::new("/proc").join(pid.to_string()).exists() {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

/// Return the differences between `store` and the restored dumps `copy`,
/// up to the last tid `last` of the dumps.
fn compare(store: &Store, copy: &Store, last: Tid) -> Result<Vec<String>, Error> {
    let watermark = store.pack_watermark()?;
    let mut problems = Problems::default();
    let mut ours = store.iterator(None, Some(last))?.peekable();
    for dumped in copy.iterator(None, None)? {
        let dumped = dumped?;
        // A transaction of the store before this one is not in the dumps.
        while let Some(txn) = ours.next_if(|txn| !txn.as_ref().is_ok_and(|t| t.tid >= dumped.tid)) {
            problems.push(match txn {
                Ok(txn) => not_in_dumps(txn.tid),
                Err(error) => format!("the store does not read: {error}"),
            });
        }
        let packed = dumped.tid <= watermark;
        match ours.next_if(|txn| txn.as_ref().is_ok_and(|t| t.tid == dumped.tid)) {
            Some(Ok(txn)) => compare_txn(store, copy, &txn, &dumped, packed, &mut problems)?,
            // A pack removed the transaction.
            _ if packed => {}
            _ => problems.push(format!("the store lacks transaction {}", dumped.tid.hex())),
        }
    }
    for txn in ours {
        problems.push(match txn {
            Ok(txn) => not_in_dumps(txn.tid),
            Err(error) => format!("the store does not read: {error}"),
        });
    }

    if watermark > last {
        problems.summary(format!(
            "the store was packed at tid {}, after the last tid {} of the dumps; \
             write a new full dump into a new directory",
            watermark.hex(),
            last.hex()
        ));
    } else {
        compare_current(store, copy, last, watermark, &mut problems)?;
    }

    for name in copy.sequence_names()? {
        let Some(end) = copy.sequence_end(&name)? else {
            continue;
        };
        match store.sequence_end(&name) {
            Ok(Some(have)) if have >= end => {}
            Ok(Some(have)) => problems.push(format!(
                "sequence {name:?}: the store ends at {have}, below its end {end} in the dumps"
            )),
            Ok(None) => problems.push(format!("sequence {name:?}: the store has no row")),
            Err(error) => problems.push(format!("sequence {name:?}: {error}")),
        }
    }
    // The durable rows of the store can change only with a commit, so the
    // comparison holds when the store has no commit after the dumps.
    if store.last_transaction() == last {
        compare_durable(store, copy, &mut problems)?;
    }
    Ok(problems.finish())
}

/// Compare the durable rows of `store` with those of `copy`. A difference
/// names the namespace and the xxh3-64 hash of the key, never the key.
fn compare_durable(store: &Store, copy: &Store, problems: &mut Problems) -> Result<(), Error> {
    let all = |store: &Store| -> Result<Vec<DurableRow>, Error> {
        let mut rows = Vec::new();
        each_durable_row(store, |row| {
            rows.push(row);
            Ok(())
        })?;
        Ok(rows)
    };
    let name = |row: &DurableRow| {
        format!(
            "durable row of namespace {:?}, the key of xxh3-64 {:#018x}",
            row.name,
            xxhash_rust::xxh3::xxh3_64(&row.key)
        )
    };
    let ours = all(store)?;
    let theirs = all(copy)?;
    let mut ours = ours.into_iter().peekable();
    for dumped in theirs {
        while let Some(row) =
            ours.next_if(|row| (&row.name, &row.key) < (&dumped.name, &dumped.key))
        {
            problems.push(format!("{}: not in the dumps", name(&row)));
        }
        match ours.next_if(|row| (&row.name, &row.key) == (&dumped.name, &dumped.key)) {
            Some(row) if row.payload == dumped.payload => {}
            Some(row) => problems.push(format!("{}: another value in the dumps", name(&row))),
            None => problems.push(format!("{}: the store lacks it", name(&dumped))),
        }
    }
    for row in ours {
        problems.push(format!("{}: not in the dumps", name(&row)));
    }
    Ok(())
}

/// Compare the current record of each oid at `last` in `store` and in
/// `copy`. A record that `copy` has and `store` lacks is no difference
/// when its serial is at or before `watermark`: a pack removed it.
fn compare_current(
    store: &Store,
    copy: &Store,
    last: Tid,
    watermark: Tid,
    problems: &mut Problems,
) -> Result<(), Error> {
    let before = Tid(last.0.saturating_add(1));
    let (mut ours, mut theirs) = (OidVersions::new(store), OidVersions::new(copy));
    let (mut mine, mut restored) = (ours.next_oid()?, theirs.next_oid()?);
    loop {
        let oid = match (mine, restored) {
            (None, None) => break,
            (Some(oid), None) | (None, Some(oid)) => oid,
            (Some(a), Some(b)) => a.min(b),
        };
        if mine == Some(oid) {
            mine = ours.next_oid()?;
        }
        if restored == Some(oid) {
            restored = theirs.next_oid()?;
        }
        let at = format!("oid {} at {}", oid.hex(), last.hex());
        let current_mine = match current(store, oid, before) {
            Ok(record) => record,
            Err(error) => {
                problems.push(format!("{at}: {error}"));
                continue;
            }
        };
        match (current_mine, current(copy, oid, before)?) {
            (a, b) if a == b => {}
            (None, Some((serial, _))) if serial <= watermark => {}
            _ => problems.push(format!("{at}: the current record differs")),
        }
    }
    Ok(())
}

/// Return the serial and the pickle of the current record of `oid` before
/// `before`, or `None` for a deletion or no record.
fn current(store: &Store, oid: Oid, before: Tid) -> zoe_core::Result<Option<(Tid, Vec<u8>)>> {
    match store.load_before(oid, before) {
        Ok(Some(record)) => Ok(Some((record.serial, record.data))),
        Ok(None) | Err(ZoeError::NotFound { .. }) => Ok(None),
        Err(error) => Err(error),
    }
}

fn not_in_dumps(tid: Tid) -> String {
    format!(
        "the store has transaction {}, which the dumps do not have",
        tid.hex()
    )
}

/// Compare the transaction `ours` of `store` with the transaction `dumped`
/// of `copy`. When `packed`, a record that `store` lacks is no difference.
fn compare_txn(
    store: &Store,
    copy: &Store,
    ours: &TxnInfo,
    dumped: &TxnInfo,
    packed: bool,
    problems: &mut Problems,
) -> Result<(), Error> {
    let at = ours.tid.hex();
    let meta = |txn: &TxnInfo| {
        (
            txn.user.clone(),
            txn.description.clone(),
            txn.extension.clone(),
        )
    };
    if meta(ours) != meta(dumped) {
        problems.push(format!(
            "transaction {at}: the user, the description or the extension differs"
        ));
    }
    for diff in diff_records(txn_records(ours), txn_records(dumped)) {
        match diff {
            RecordDiff::SecondOnly(oid) => {
                if !packed {
                    problems.push(format!(
                        "oid {} at {at}: the store lacks the record",
                        oid.hex()
                    ));
                }
            }
            RecordDiff::FirstOnly(oid) => problems.push(format!(
                "oid {} at {at}: the store has a record that the dumps do not have",
                oid.hex()
            )),
            RecordDiff::Both(oid, data, other) if other != data => {
                problems.push(format!("oid {} at {at}: the record differs", oid.hex()));
            }
            RecordDiff::Both(_, None, _) => {}
            RecordDiff::Both(oid, Some(_), _) => {
                match (
                    store.load_blob(oid, ours.tid),
                    copy.load_blob(oid, ours.tid)?,
                ) {
                    (Ok(mine), restored) if mine == restored => {}
                    (Ok(_), _) => {
                        problems.push(format!("oid {} at {at}: the blob differs", oid.hex()));
                    }
                    (Err(error), _) => problems.push(format!("oid {} at {at}: {error}", oid.hex())),
                }
            }
        }
    }
    Ok(())
}
