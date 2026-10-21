//! The transactions of the store, as `explain_conflict` and the prompts
//! give them.
//!
//! A transaction shows its tid, its user, its description and the objects
//! that it wrote: the oid, the class and the size of each record. A
//! deletion has size 0 and no class. The class comes from the class
//! pickle alone, as in Loupe's space screen. No state and no pickle bytes
//! go out.
//!
//! The store keeps no record of the conflicts. So `explain_conflict` reads
//! the history only: for each object of the transaction, the revisions
//! just before and just after it, from Loupe's `history`. These are the
//! other writers of the same objects.
//!
//! Each list has a limit, and the JSON gives the full count next to it.

use std::collections::BTreeMap;

use serde_json::{Value, json};
use zoe_core::{Oid, PackReport, Tid, TxnInfo, ZoeError};
use zoe_loupe::{Folder, HISTORY_LIMIT, Revision, oid_repr, tid_repr};
use zoe_pickle::decode_class_key;

use crate::ops::ToolError;

/// The most objects of a transaction that `explain_conflict` lists.
pub(crate) const OBJECT_LIMIT: usize = 100;

/// The most transactions that a summary of changes lists.
pub(crate) const SUMMARY_LIMIT: usize = 50;

/// The most objects of one transaction that a summary of changes lists.
pub(crate) const SUMMARY_OBJECT_LIMIT: usize = 16;

/// The most revisions of an object that `why_conflict` gives.
pub(crate) const REVISION_LIMIT: usize = 32;

/// The class key of the deletions in a summary. No class key of a record
/// looks like it, because a class key is `module.name` or
/// [`PackReport::UNKNOWN_CLASS`].
const DELETED: &str = "<deleted>";

/// The most bytes of a user or a description, as in Loupe.
const MAX_NOTE: usize = 1 << 12;

/// Return the user or the description `bytes` as text, as Loupe shows it.
/// Bytes that are not UTF-8 become U+FFFD. A text longer than 4 KiB shows
/// its start and ends with `…`.
fn note(bytes: &[u8]) -> String {
    let cut = bytes.len() > MAX_NOTE;
    let mut text = String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_NOTE)]).into_owned();
    if cut {
        text.push('…');
    }
    text
}

/// One record of a transaction: the oid, the class and the size.
fn object_json(oid: Oid, data: Option<&[u8]>) -> Value {
    json!({
        "oid": oid_repr(oid),
        "class": data.and_then(|data| decode_class_key(data).ok().flatten()),
        "size": data.map_or(0, <[u8]>::len),
    })
}

/// Return the transaction of `tid`. A tid without a transaction is an
/// error.
fn transaction(folder: &Folder, tid: Tid) -> Result<TxnInfo, ToolError> {
    let mut transactions = folder
        .store()
        .iterator(Some(tid), Some(tid))
        .map_err(zoe_loupe::Error::from)?;
    match transactions.next() {
        Some(txn) => Ok(txn.map_err(zoe_loupe::Error::from)?),
        None => Err(ToolError::NoTransaction(tid_repr(tid))),
    }
}

/// The revisions of an object just before and just after a transaction.
#[derive(Debug, PartialEq, Eq)]
struct Neighbours {
    /// The revision just before the transaction, or `None`.
    before: Option<Revision>,
    /// The revision just after the transaction, or `None`.
    after: Option<Revision>,
    /// The history stops before it reaches the revision before the
    /// transaction. Then a neighbour that is `None` is not known.
    cut: bool,
}

/// Return the revisions just before and just after `tid` in `revisions`,
/// a history of one object, newest first. The history holds at most
/// `limit` revisions.
///
/// When the history does not hold `tid`, both neighbours are not known.
/// When `tid` is the last revision of a full history, the revision before
/// it is not known. In these two cases, `cut` is true.
fn neighbours(revisions: &[Revision], tid: Tid, limit: usize) -> Neighbours {
    let Some(at) = revisions.iter().position(|revision| revision.tid == tid) else {
        return Neighbours {
            before: None,
            after: None,
            cut: true,
        };
    };
    Neighbours {
        before: revisions.get(at + 1).cloned(),
        after: at.checked_sub(1).map(|newer| revisions[newer].clone()),
        cut: at + 1 == revisions.len() && revisions.len() >= limit,
    }
}

/// Return the transaction of `tid` with each object that it wrote, and for
/// each object the revisions just before and just after it. List at most
/// [`OBJECT_LIMIT`] objects, in oid order. `records` gives their number.
///
/// Loupe's history holds the newest [`HISTORY_LIMIT`] revisions only. When
/// it does not reach the revision before `tid`, the object has
/// `history_cut`, and a neighbour that is null is not known.
pub(crate) fn explain(folder: &Folder, tid: Tid) -> Result<Value, ToolError> {
    let txn = transaction(folder, tid)?;
    let mut objects = Vec::new();
    for record in txn.records.iter().take(OBJECT_LIMIT) {
        let mut object = object_json(record.oid, record.data.as_deref());
        let revisions = folder.history(record.oid)?;
        let found = neighbours(&revisions, tid, HISTORY_LIMIT);
        object["before"] = serde_json::to_value(found.before)?;
        object["after"] = serde_json::to_value(found.after)?;
        if found.cut {
            object["history_cut"] = Value::Bool(true);
        }
        objects.push(object);
    }
    Ok(json!({
        "tid": tid_repr(tid),
        "user": note(&txn.user),
        "description": note(&txn.description),
        "records": txn.records.len(),
        "objects": objects,
    }))
}

/// Return the transactions after `since`, oldest first: at most
/// [`SUMMARY_LIMIT`] of them. `more` tells if the store has more.
///
/// Each transaction gives its tid, user and description, the number of
/// its records, the number of records of each class (deletions under
/// `<deleted>`), and at most
/// [`SUMMARY_OBJECT_LIMIT`] of its objects.
pub(crate) fn changes_since(folder: &Folder, since: Tid) -> Result<Value, ToolError> {
    let mut transactions = Vec::new();
    let mut more = false;
    if let Some(start) = since.0.checked_add(1) {
        let store = folder.store();
        let iterator = store
            .iterator(Some(Tid(start)), None)
            .map_err(zoe_loupe::Error::from)?;
        for txn in iterator {
            if transactions.len() == SUMMARY_LIMIT {
                more = true;
                break;
            }
            transactions.push(summary(&txn.map_err(zoe_loupe::Error::from)?));
        }
    }
    Ok(json!({
        "since": tid_repr(since),
        "last_tid": tid_repr(folder.last_tid()),
        "transactions": transactions,
        "more": more,
    }))
}

/// Return the summary of one transaction.
fn summary(txn: &TxnInfo) -> Value {
    let mut classes: BTreeMap<String, u64> = BTreeMap::new();
    let mut objects = Vec::new();
    for record in &txn.records {
        let data = record.data.as_deref();
        let class = match data {
            Some(data) => decode_class_key(data)
                .ok()
                .flatten()
                .unwrap_or_else(|| PackReport::UNKNOWN_CLASS.to_owned()),
            None => DELETED.to_owned(),
        };
        *classes.entry(class).or_default() += 1;
        if objects.len() < SUMMARY_OBJECT_LIMIT {
            objects.push(object_json(record.oid, data));
        }
    }
    json!({
        "tid": tid_repr(txn.tid),
        "user": note(&txn.user),
        "description": note(&txn.description),
        "records": txn.records.len(),
        "classes": classes,
        "objects": objects,
    })
}

/// Return the newest revisions of `oid`, at most [`REVISION_LIMIT`] of
/// them, with the class of its current record. `revisions` gives the
/// number of revisions in Loupe's history, at most [`HISTORY_LIMIT`].
pub(crate) fn recent_writers(folder: &Folder, oid: Oid) -> Result<Value, ToolError> {
    let mut revisions = folder.history(oid)?;
    let count = revisions.len();
    revisions.truncate(REVISION_LIMIT);
    // A deletion has no current record, so it has no class.
    let class = match folder.store().load(oid) {
        Ok((data, _)) => decode_class_key(&data).ok().flatten(),
        Err(ZoeError::NotFound { .. }) => None,
        Err(error) => return Err(zoe_loupe::Error::from(error).into()),
    };
    Ok(json!({
        "oid": oid_repr(oid),
        "class": class,
        "revisions": count,
        "newest": serde_json::to_value(revisions)?,
    }))
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use zoe_core::Store;

    use super::*;

    #[test]
    fn a_long_note_shows_its_start() {
        assert_eq!(note(b"admin"), "admin");
        assert_eq!(note(b"a\xffb"), "a\u{fffd}b");
        let long = note(&[b'x'; MAX_NOTE + 1]);
        assert_eq!(long.len(), MAX_NOTE + '…'.len_utf8());
        assert!(long.ends_with('…'));
    }

    #[test]
    fn a_record_gives_its_class_from_the_class_pickle() {
        let record = b"\x80\x03cm\nDoc\nq\x00.\x80\x03}.";
        let object = object_json(Oid(1), Some(record));
        assert_eq!(object["oid"], "0x01");
        assert_eq!(object["class"], "m.Doc");
        assert_eq!(object["size"], record.len());
        let deletion = object_json(Oid(2), None);
        assert_eq!(deletion["class"], Value::Null);
        assert_eq!(deletion["size"], 0);
    }

    /// A record of the class `m.Doc` whose state is the text `title`.
    fn document(title: &str) -> Vec<u8> {
        let length = u32::try_from(title.len()).unwrap().to_le_bytes();
        [
            &b"\x80\x03cm\nDoc\n.\x80\x03X"[..],
            &length,
            title.as_bytes(),
            b".",
        ]
        .concat()
    }

    /// A store in a new folder. Three transactions write oid 1: `create`
    /// by `admin`, then `rename` and `retitle` by `editor`. `create` also
    /// writes oid 0 and oid 2, and `retitle` deletes oid 2. Return the
    /// folder and the three tids.
    fn store(name: &str) -> (PathBuf, [Tid; 3]) {
        let dir = std::env::temp_dir().join(format!("zoe-mcp-txn-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(&dir).unwrap();
        let mut txn = store.tpc_begin(b"admin", b"create", b"").unwrap();
        for oid in 0..=2 {
            txn.store(Oid(oid), Tid(0), &document("first")).unwrap();
        }
        txn.vote().unwrap();
        let first = txn.finish().unwrap();
        let mut txn = store.tpc_begin(b"editor", b"rename", b"").unwrap();
        txn.store(Oid(1), first, &document("second")).unwrap();
        txn.vote().unwrap();
        let second = txn.finish().unwrap();
        let mut txn = store.tpc_begin(b"editor", b"retitle", b"").unwrap();
        txn.store(Oid(1), second, &document("third")).unwrap();
        txn.delete(Oid(2), first).unwrap();
        txn.vote().unwrap();
        let third = txn.finish().unwrap();
        drop(store);
        (dir, [first, second, third])
    }

    /// Add `n` transactions to the store in `dir`, each one a new revision
    /// of oid 0 that writes `width` other new oids too. Return the folder.
    fn add(dir: &Path, n: usize, width: u64) -> Folder {
        let store = Store::open(dir).unwrap();
        for i in 0..n {
            let current = store.load_before(Oid(0), Tid::MAX).unwrap().unwrap();
            let mut txn = store.tpc_begin(b"bulk", b"more", b"").unwrap();
            txn.store(Oid(0), current.serial, &document("more"))
                .unwrap();
            for oid in 0..width {
                let oid = Oid(1000 + (i as u64) * width + oid);
                txn.store(oid, Tid(0), &document("new")).unwrap();
            }
            txn.vote().unwrap();
            txn.finish().unwrap();
        }
        drop(store);
        Folder::open(dir).unwrap()
    }

    #[test]
    fn explain_gives_the_objects_and_their_neighbours() {
        let (dir, [first, second, third]) = store("explain");
        let folder = Folder::open(&dir).unwrap();
        let explained = explain(&folder, second).unwrap();
        assert_eq!(explained["tid"], tid_repr(second));
        assert_eq!(explained["user"], "editor");
        assert_eq!(explained["description"], "rename");
        assert_eq!(explained["records"], 1);
        let object = &explained["objects"][0];
        assert_eq!(object["oid"], "0x01");
        assert_eq!(object["class"], "m.Doc");
        assert_eq!(object["before"]["tid"], tid_repr(first));
        assert_eq!(object["before"]["description"], "create");
        assert_eq!(object["after"]["tid"], tid_repr(third));
        assert_eq!(object["after"]["user"], "editor");
        assert!(object.get("history_cut").is_none(), "{object}");
        // No state and no pickle bytes go out.
        assert!(!explained.to_string().contains("second"), "{explained}");

        // The first transaction has no revision before it. The deletion
        // of oid 2 comes after it.
        let explained = explain(&folder, first).unwrap();
        let objects = explained["objects"].as_array().unwrap();
        assert_eq!(objects.len(), 3);
        assert_eq!(objects[2]["before"], Value::Null);
        assert_eq!(objects[2]["after"]["tid"], tid_repr(third));
        assert_eq!(objects[2]["after"]["size"], 0);

        // A deletion has size 0 and no class, and the last revision has
        // nothing after it.
        let explained = explain(&folder, third).unwrap();
        let deletion = &explained["objects"][1];
        assert_eq!(deletion["oid"], "0x02");
        assert_eq!(deletion["size"], 0);
        assert_eq!(deletion["class"], Value::Null);
        assert_eq!(deletion["after"], Value::Null);
    }

    #[test]
    fn a_tid_without_a_transaction_is_an_error() {
        let (dir, [_, _, third]) = store("no-txn");
        let folder = Folder::open(&dir).unwrap();
        let after = Tid(third.0 + 1);
        let error = explain(&folder, after).unwrap_err();
        assert!(matches!(error, ToolError::NoTransaction(_)), "{error:?}");
        assert_eq!(
            error.to_string(),
            format!("tid {}: no transaction", tid_repr(after))
        );
    }

    #[test]
    fn explain_lists_at_most_the_object_limit() {
        let (dir, _) = store("object-limit");
        let folder = add(&dir, 1, OBJECT_LIMIT as u64);
        let explained = explain(&folder, folder.last_tid()).unwrap();
        assert_eq!(explained["records"], OBJECT_LIMIT + 1);
        assert_eq!(explained["objects"].as_array().unwrap().len(), OBJECT_LIMIT);
    }

    #[test]
    fn the_changes_since_a_tid_are_the_transactions_after_it() {
        let (dir, [first, second, third]) = store("since");
        let folder = Folder::open(&dir).unwrap();
        let changes = changes_since(&folder, first).unwrap();
        assert_eq!(changes["since"], tid_repr(first));
        assert_eq!(changes["last_tid"], tid_repr(third));
        assert_eq!(changes["more"], false);
        let transactions = changes["transactions"].as_array().unwrap();
        let tids: Vec<&Value> = transactions.iter().map(|t| &t["tid"]).collect();
        assert_eq!(tids, [&json!(tid_repr(second)), &json!(tid_repr(third))]);
        let retitle = &transactions[1];
        assert_eq!(retitle["records"], 2);
        assert_eq!(retitle["classes"], json!({"<deleted>": 1, "m.Doc": 1}));
        assert_eq!(retitle["objects"][1]["oid"], "0x02");

        let none = changes_since(&folder, third).unwrap();
        assert_eq!(none["transactions"], json!([]));
        let none = changes_since(&folder, Tid(u64::MAX)).unwrap();
        assert_eq!(none["transactions"], json!([]));
    }

    #[test]
    fn a_summary_has_limits_and_tells_that_more_follow() {
        let (dir, [_, _, third]) = store("summary-limit");
        let folder = add(&dir, SUMMARY_LIMIT + 1, SUMMARY_OBJECT_LIMIT as u64);
        let changes = changes_since(&folder, third).unwrap();
        assert_eq!(changes["more"], true);
        let transactions = changes["transactions"].as_array().unwrap();
        assert_eq!(transactions.len(), SUMMARY_LIMIT);
        assert_eq!(transactions[0]["records"], SUMMARY_OBJECT_LIMIT + 1);
        let objects = transactions[0]["objects"].as_array().unwrap();
        assert_eq!(objects.len(), SUMMARY_OBJECT_LIMIT);
        assert_eq!(
            transactions[0]["classes"]["m.Doc"],
            SUMMARY_OBJECT_LIMIT + 1
        );
    }

    #[test]
    fn the_recent_writers_are_the_newest_revisions() {
        let (dir, [_, second, third]) = store("writers");
        let folder = Folder::open(&dir).unwrap();
        let writers = recent_writers(&folder, Oid(1)).unwrap();
        assert_eq!(writers["oid"], "0x01");
        assert_eq!(writers["class"], "m.Doc");
        assert_eq!(writers["revisions"], 3);
        assert_eq!(writers["newest"][0]["tid"], tid_repr(third));
        assert_eq!(writers["newest"][1]["tid"], tid_repr(second));

        // A deleted object has its history and no class.
        let deleted = recent_writers(&folder, Oid(2)).unwrap();
        assert_eq!(deleted["class"], Value::Null);
        assert_eq!(deleted["revisions"], 2);

        drop(folder);
        let folder = add(&dir, REVISION_LIMIT, 0);
        let writers = recent_writers(&folder, Oid(0)).unwrap();
        assert_eq!(writers["revisions"], REVISION_LIMIT + 1);
        let newest = writers["newest"].as_array().unwrap();
        assert_eq!(newest.len(), REVISION_LIMIT);
        assert_eq!(newest[0]["tid"], tid_repr(folder.last_tid()));
    }

    #[test]
    fn an_oid_without_a_record_has_no_writers() {
        let (dir, _) = store("no-writers");
        let folder = Folder::open(&dir).unwrap();
        let error = recent_writers(&folder, Oid(9)).unwrap_err();
        assert_eq!(error.to_string(), "oid 0x09: no record");
    }

    /// A revision of `tid`.
    fn revision(tid: u64) -> Revision {
        Revision {
            tid: Tid(tid),
            user: String::new(),
            description: String::new(),
            size: 1,
        }
    }

    #[test]
    fn the_neighbours_are_the_revisions_around_the_tid() {
        // Newest first.
        let history = [revision(9), revision(7), revision(5)];
        let found = neighbours(&history, Tid(7), 4);
        assert_eq!(found.before, Some(revision(5)));
        assert_eq!(found.after, Some(revision(9)));
        assert!(!found.cut);
        // The oldest revision of a history that is not full has nothing
        // before it: the transaction created the object.
        let found = neighbours(&history, Tid(5), 4);
        assert_eq!(found.before, None);
        assert_eq!(found.after, Some(revision(7)));
        assert!(!found.cut);
    }

    #[test]
    fn a_full_history_does_not_know_what_comes_before_its_last_revision() {
        let history = [revision(9), revision(7), revision(5)];
        let found = neighbours(&history, Tid(5), 3);
        assert_eq!(found.before, None);
        assert_eq!(found.after, Some(revision(7)));
        assert!(found.cut);
        // A revision before the last one still has a known neighbour.
        assert!(!neighbours(&history, Tid(7), 3).cut);
    }

    #[test]
    fn a_tid_that_the_history_does_not_hold_has_no_known_neighbours() {
        let history = [revision(9), revision(7)];
        let found = neighbours(&history, Tid(3), 2);
        assert_eq!(
            found,
            Neighbours {
                before: None,
                after: None,
                cut: true
            }
        );
    }

    #[test]
    fn the_class_of_the_writers_comes_from_the_class_pickle() {
        let dir = std::env::temp_dir().join(format!("zoe-mcp-txn-class-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(&dir).unwrap();
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.store(Oid(0), Tid(0), &document("root")).unwrap();
        // The class pickle decodes, the state does not.
        txn.store(Oid(1), Tid(0), b"\x80\x03cm\nDoc\n.not a pickle")
            .unwrap();
        txn.vote().unwrap();
        let tid = txn.finish().unwrap();
        drop(store);
        let folder = Folder::open(&dir).unwrap();
        assert!(folder.object(Oid(1)).unwrap().error.is_some());
        let writers = recent_writers(&folder, Oid(1)).unwrap();
        assert_eq!(writers["class"], "m.Doc");
        let explained = explain(&folder, tid).unwrap();
        assert_eq!(explained["objects"][1]["class"], "m.Doc");
    }
}
