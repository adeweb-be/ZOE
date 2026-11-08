//! [`verify_backup`]: compare the dumps of a backup directory with a
//! store, through a copy that the dumps restore.

use std::borrow::Borrow;
use std::collections::HashMap;
use std::fs;
use std::io::ErrorKind;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use zoe_core::{DurableRow, Oid, Store, TextChange, TextDelta, Tid, TxnInfo, ZoeError};

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
/// Each delta of the dumps must be the same in the store, unless the
/// store lacks it and its tid is at or before the pack watermark: a pack
/// removed it. The store must have no other delta up to the last tid of
/// the dumps. The live text of each key of the dumps at their last tid
/// must be the same in both, unless the store lacks it, the newest delta
/// of the key is at or before the watermark, and the store has no current
/// record of the catalog: a pack with garbage collection removed the
/// catalog. A difference names the catalog, the index, the rid and the
/// tid of a delta, never its text.
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
            Some(Ok(txn)) => {
                compare_txn(store, copy, &txn, &dumped, packed, &mut problems)?;
                compare_deltas(store, copy, dumped.tid, packed, &mut problems)?;
            }
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
        compare_live_texts(store, copy, last, watermark, &mut problems)?;
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

/// Return the name of the delta `delta` at `tid` in a difference: the
/// catalog, the index, the rid and the tid, never the text.
fn delta_name(delta: &TextDelta, tid: Tid) -> String {
    match delta.rid {
        Some(rid) => format!(
            "text delta of catalog {}, index {:?}, rid {rid} at {}",
            delta.catalog.hex(),
            delta.index,
            tid.hex()
        ),
        None => format!(
            "clear delta of catalog {}, index {:?} at {}",
            delta.catalog.hex(),
            delta.index,
            tid.hex()
        ),
    }
}

/// Return the order of the key of `delta` in `text_deltas`: the catalog,
/// the clear deltas first, the length of the index id, the index id, then
/// the rid.
fn delta_order(delta: &TextDelta) -> (Oid, bool, usize, &str, Option<i32>) {
    (
        delta.catalog,
        delta.rid.is_some(),
        delta.index.len(),
        delta.index.as_str(),
        delta.rid,
    )
}

/// Compare the deltas of the transaction `tid` in `store` with those of
/// `copy`. When `packed`, a delta that `store` lacks is no difference.
fn compare_deltas(
    store: &Store,
    copy: &Store,
    tid: Tid,
    packed: bool,
    problems: &mut Problems,
) -> Result<(), Error> {
    let ours = match store.text_deltas_at(tid) {
        Ok(deltas) => deltas,
        Err(error) => {
            problems.push(format!("the deltas at {}: {error}", tid.hex()));
            return Ok(());
        }
    };
    let mut ours = ours.into_iter().peekable();
    for dumped in copy.text_deltas_at(tid)? {
        while let Some(delta) = ours.next_if(|delta| delta_order(delta) < delta_order(&dumped)) {
            problems.push(format!(
                "{}: the store has a delta that the dumps do not have",
                delta_name(&delta, tid)
            ));
        }
        match ours.next_if(|delta| delta_order(delta) == delta_order(&dumped)) {
            Some(delta) if delta == dumped => {}
            Some(delta) => problems.push(format!("{}: the delta differs", delta_name(&delta, tid))),
            None if packed => {}
            None => problems.push(format!(
                "{}: the store lacks the delta",
                delta_name(&dumped, tid)
            )),
        }
    }
    for delta in ours {
        problems.push(format!(
            "{}: the store has a delta that the dumps do not have",
            delta_name(&delta, tid)
        ));
    }
    Ok(())
}

/// The keys of deltas that one read of `compare_live_texts` asks for.
const KEY_CHUNK: usize = 1024;

/// Compare the live text at `last` of each key of a text delta of `copy`
/// in `store` and in `copy`. A pack never changes a live text after its
/// tid, so a packed store gives the same live texts. A live text that
/// `store` lacks is no difference only when the newest delta of its key up
/// to `last` is at or before `watermark` and `store` has no current record
/// of the catalog at `last`: a pack with garbage collection removed the
/// catalog with its deltas. A catalog that no reader reaches but that has
/// a record after the pack tid keeps that record, so its lost texts are
/// differences.
fn compare_live_texts(
    store: &Store,
    copy: &Store,
    last: Tid,
    watermark: Tid,
    problems: &mut Problems,
) -> Result<(), Error> {
    let mut check = LiveCheck {
        store,
        copy,
        before: Tid(last.0.saturating_add(1)),
        last,
        watermark,
        gone: HashMap::new(),
    };
    let mut after: Option<TextChange> = None;
    // The key that the scan reads, and the tid of its newest delta up to
    // `last`.
    let mut key: Option<(TextChange, Tid)> = None;
    loop {
        let changes = copy.text_keys(after.as_ref(), KEY_CHUNK)?;
        let full = changes.len() == KEY_CHUNK;
        for change in &changes {
            if change.rid.is_none() || change.tid > last {
                continue;
            }
            match &mut key {
                Some((read, newest))
                    if (read.catalog, &read.index, read.rid)
                        == (change.catalog, &change.index, change.rid) =>
                {
                    *newest = change.tid;
                }
                _ => {
                    if let Some((read, newest)) = key.take() {
                        check.compare(&read, newest, problems)?;
                    }
                    key = Some((change.clone(), change.tid));
                }
            }
        }
        match changes.into_iter().last() {
            Some(change) if full => after = Some(change),
            _ => break,
        }
    }
    if let Some((read, newest)) = key {
        check.compare(&read, newest, problems)?;
    }
    Ok(())
}

/// The comparison of the live texts of `compare_live_texts`.
struct LiveCheck<'a> {
    store: &'a Store,
    copy: &'a Store,
    /// The tid after the last tid of the dumps.
    before: Tid,
    last: Tid,
    watermark: Tid,
    /// True for each catalog that has no current record in `store` at
    /// `last`.
    gone: HashMap<Oid, bool>,
}

impl LiveCheck<'_> {
    /// Compare the live text of the key of `change`. `newest` is the tid of
    /// the newest delta of the key in `copy` up to `last`.
    fn compare(
        &mut self,
        change: &TextChange,
        newest: Tid,
        problems: &mut Problems,
    ) -> Result<(), Error> {
        let (catalog, index) = (change.catalog, change.index.as_str());
        let rid = change.rid.expect("a text delta has a rid");
        let theirs = self.copy.text_before(catalog, index, rid, self.before)?;
        let name = format!(
            "the live text of catalog {}, index {index:?}, rid {rid} at {}",
            catalog.hex(),
            self.last.hex()
        );
        match self.store.text_before(catalog, index, rid, self.before) {
            Ok(ours) if ours == theirs => {}
            Ok(None) if newest <= self.watermark && self.gone(catalog)? => {}
            Ok(_) => problems.push(format!("{name} differs")),
            Err(error) => problems.push(format!("{name}: {error}")),
        }
        Ok(())
    }

    /// Tell if `store` has no current record of `catalog` at `last`.
    fn gone(&mut self, catalog: Oid) -> Result<bool, Error> {
        if let Some(&gone) = self.gone.get(&catalog) {
            return Ok(gone);
        }
        let gone = current(self.store, catalog, self.before)?.is_none();
        self.gone.insert(catalog, gone);
        Ok(gone)
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
