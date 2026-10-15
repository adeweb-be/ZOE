//! `verify`: check a store without Python.
//!
//! The check walks every record key of the store in key order. It reads
//! each record, deletions too, and the blob record of each key, so each
//! frame passes its checksum or gives a finding. It reads and hashes each
//! blob file. It reads each transaction row and each sequence row. Of each
//! oid, the newest record is the current record. A current record gives
//! its class, from the class pickle alone, and its persistent references,
//! from `zoe_pickle::persistent_ids`. No unpickler runs.
//!
//! A reference is dangling when it names an oid without a current record.
//! A store error is a finding, never a failure of the call.
//!
//! The walk counts the oids that have at least one record, deletions
//! included. The store keeps this count in `object_count`, and a count
//! that differs is a finding. A backend that keeps no count gives none,
//! which is no finding.

use std::collections::{BTreeMap, BTreeSet};

use zoe_core::{Oid, PackReport, Store, Tid, ZoeError};
use zoe_pickle::{PersistentId, decode_class_name, persistent_ids};

/// The number of record keys that one read of the walk takes.
const KEY_CHUNK: usize = 1024;

/// A frame that does not read: a record, a blob record, a transaction row
/// or a sequence row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrameProblem {
    /// The oid of the record or of the blob record. A transaction row and
    /// a sequence row have none.
    pub oid: Option<Oid>,
    /// The tid of the frame, when the store gives it. A sequence row has
    /// none.
    pub tid: Option<Tid>,
    /// What is wrong. For a sequence row, the text names the sequence.
    pub error: String,
}

/// A blob file that does not match its blob record, or that is missing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlobProblem {
    /// The oid of the blob.
    pub oid: Oid,
    /// The tid of the blob.
    pub tid: Tid,
    /// What is wrong.
    pub error: String,
}

/// A persistent reference of a current record to an oid without a current
/// record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Dangling {
    /// The oid of the record that holds the reference.
    pub oid: Oid,
    /// The tid of that record.
    pub tid: Tid,
    /// The oid that the reference names.
    pub missing: Oid,
}

/// A current record whose references zoe-pickle cannot read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PickleProblem {
    /// The oid of the record.
    pub oid: Oid,
    /// The tid of the record.
    pub tid: Tid,
    /// What is wrong.
    pub error: String,
}

/// An object count that differs from the oids of the records.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CountProblem {
    /// The `object_count` that the store keeps.
    pub stored: u64,
    /// The oids that have at least one record, a deletion included.
    pub counted: u64,
}

/// The findings and the counts of `verify`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Verification {
    /// The transaction rows that read.
    pub transactions: u64,
    /// The sequence rows that read.
    pub sequences: u64,
    /// The record keys, deletions too.
    pub records: u64,
    /// The oids with a current record.
    pub objects: u64,
    /// The blob records.
    pub blobs: u64,
    /// The `object_count` that the store keeps. A backend that keeps no
    /// count gives none: that is no finding.
    pub object_count: Option<u64>,
    /// The object count when it differs from the oids of the records. The
    /// check runs only when the walk read every record key.
    pub count: Option<CountProblem>,
    /// The frames that do not read.
    pub checksums: Vec<FrameProblem>,
    /// The blob files that do not match their blob records.
    pub blob_files: Vec<BlobProblem>,
    /// The dangling references, in the order of the oid, then of the
    /// missing oid. A pair comes once.
    pub references: Vec<Dangling>,
    /// The current records that zoe-pickle cannot read.
    pub pickles: Vec<PickleProblem>,
    /// The class of each current record, `module.name`, with its count.
    /// A class pickle that names no class gives
    /// [`PackReport::UNKNOWN_CLASS`].
    pub classes: BTreeMap<String, u64>,
    /// The classes of `classes` that the list of known classes does not
    /// name. It is empty when `verify` gets no list.
    pub unknown_classes: Vec<String>,
}

impl Verification {
    /// Return the number of findings.
    pub fn problems(&self) -> usize {
        self.checksums.len()
            + self.blob_files.len()
            + self.references.len()
            + self.pickles.len()
            + self.unknown_classes.len()
            + usize::from(self.count.is_some())
    }
}

/// Check `store`. `known` lists the classes that the application provides,
/// as `module.name`. A class of the store that it does not name is a
/// finding.
///
/// For an exact report, no writer changes the store during the call. A
/// read-only open makes sure of this.
pub fn verify(store: &Store, known: Option<&BTreeSet<String>>) -> Verification {
    let mut report = Verification::default();
    rows(store, &mut report);
    sequences(store, &mut report);
    let mut walk = Walk::default();
    records(store, &mut report, &mut walk);
    report.object_count = store.object_count();
    if let (Some(stored), Some(counted)) = (report.object_count, walk.oids)
        && stored != counted
    {
        report.count = Some(CountProblem { stored, counted });
    }
    dangling(store, &mut report, walk);
    if let Some(known) = known {
        report.unknown_classes = report
            .classes
            .keys()
            .filter(|class| !known.contains(*class))
            .cloned()
            .collect();
    }
    report
}

/// Read every transaction row.
fn rows(store: &Store, report: &mut Verification) {
    for row in store.txn_rows() {
        match row {
            Ok(_) => report.transactions += 1,
            Err(error) => {
                let tid = match error {
                    ZoeError::CorruptTxn { tid } => Some(tid),
                    _ => None,
                };
                report.checksums.push(FrameProblem {
                    oid: None,
                    tid,
                    error: error.to_string(),
                });
            }
        }
    }
}

/// Read every sequence row.
fn sequences(store: &Store, report: &mut Verification) {
    let names = match store.sequence_names() {
        Ok(names) => names,
        Err(error) => {
            report.checksums.push(frame_problem(None, error));
            return;
        }
    };
    for name in names {
        let error = match store.sequence_end(&name) {
            Ok(Some(_)) => {
                report.sequences += 1;
                continue;
            }
            // The list names the row, so a read that does not find it
            // shows a damaged store.
            Ok(None) => format!("sequence row {name:?}: listed, but the read finds no row"),
            Err(error) => error.to_string(),
        };
        report.checksums.push(FrameProblem {
            oid: None,
            tid: None,
            error,
        });
    }
}

/// What the walk of the records keeps for the check of the references.
#[derive(Default)]
struct Walk {
    /// The oids with a current record, in ascending order. An oid whose
    /// newest record does not read is here too: it exists.
    current: Vec<Oid>,
    /// The references to an oid after the oid of the record, which the
    /// walk did not reach yet: the oid that the reference names, then the
    /// record. ZODB gives a new object its oid when it pickles the parent,
    /// so nearly every reference comes here: 24 bytes each.
    ahead: Vec<(Oid, Oid, Tid)>,
    /// The references that the walk found without a current record.
    missing: Vec<Dangling>,
    /// The oids that have at least one record, a deletion included. It is
    /// `None` when the walk stopped before the last record key.
    oids: Option<u64>,
}

impl Walk {
    fn has(&self, oid: Oid) -> bool {
        self.current.binary_search(&oid).is_ok()
    }
}

/// Walk every record key: check each record frame, each blob record and
/// each blob file, and read the current record of each oid.
fn records(store: &Store, report: &mut Verification, walk: &mut Walk) {
    let mut from = Oid(0);
    let mut oids = 0;
    loop {
        let keys = match store.record_keys(from, KEY_CHUNK) {
            Ok(keys) => keys,
            Err(error) => {
                // The walk cannot go on. `dangling` asks the store about
                // each reference that it did not see.
                report.checksums.push(frame_problem(None, error));
                return;
            }
        };
        let Some(&(last, _)) = keys.last() else {
            walk.oids = Some(oids);
            return;
        };
        for versions in keys.chunk_by(|a, b| a.0 == b.0) {
            oids += 1;
            oid_versions(store, versions, report, walk);
        }
        match last.0.checked_add(1) {
            Some(next) => from = Oid(next),
            None => {
                walk.oids = Some(oids);
                return;
            }
        }
    }
}

/// Check the records of one oid, oldest first, and read its current
/// record.
fn oid_versions(
    store: &Store,
    versions: &[(Oid, Tid)],
    report: &mut Verification,
    walk: &mut Walk,
) {
    let mut newest = None;
    for &(oid, tid) in versions {
        report.records += 1;
        newest = match store.load_serial(oid, tid) {
            Ok(data) => Some((tid, Some(data))),
            // The key exists, so this is a deletion.
            Err(ZoeError::NotFound { .. }) => None,
            Err(error) => {
                report
                    .checksums
                    .push(frame_problem(Some((oid, tid)), error));
                Some((tid, None))
            }
        };
        blob(store, oid, tid, report);
    }
    let Some(&(oid, _)) = versions.first() else {
        return;
    };
    let Some((tid, data)) = newest else {
        return;
    };
    walk.current.push(oid);
    report.objects += 1;
    if let Some(data) = data {
        current(oid, tid, &data, report, walk);
    }
}

/// Check the blob record of `oid` at `tid` and its blob file, if the
/// record has a blob.
fn blob(store: &Store, oid: Oid, tid: Tid, report: &mut Verification) {
    match store.load_blob(oid, tid) {
        Ok(None) => {}
        Ok(Some(_)) => {
            report.blobs += 1;
            if let Err(error) = store.verify_blob(oid, tid) {
                report.blob_files.push(BlobProblem {
                    oid,
                    tid,
                    error: blob_error(store, oid, tid, error),
                });
            }
        }
        Err(error) => report
            .checksums
            .push(frame_problem(Some((oid, tid)), error)),
    }
}

/// Return the text of `error`, the failed check of the blob file of `oid`
/// at `tid`. The text of a missing file says "missing". The text of a file
/// that does not match its blob record does not. The finding gives the oid
/// and the tid, so the text names no path: a path can hold any word.
fn blob_error(store: &Store, oid: Oid, tid: Tid, error: ZoeError) -> String {
    match (store.has_blob_file(oid, tid), error) {
        (Ok(false), _) => MISSING_BLOB_FILE.to_owned(),
        (Ok(true), ZoeError::CorruptBlob { .. }) => DAMAGED_BLOB_FILE.to_owned(),
        (_, error) => error.to_string(),
    }
}

/// The text of a blob file that is missing.
const MISSING_BLOB_FILE: &str = "the blob file is missing";

/// The text of a blob file that does not match its blob record.
const DAMAGED_BLOB_FILE: &str =
    "the blob file does not match the size and the BLAKE3 hash of its blob record";

/// Count the class of the current record `data` of `oid` at `tid`, and
/// check its references.
fn current(oid: Oid, tid: Tid, data: &[u8], report: &mut Verification, walk: &mut Walk) {
    let class = match decode_class_name(data) {
        Ok(Some((module, name))) => format!("{module}.{name}"),
        Ok(None) | Err(_) => PackReport::UNKNOWN_CLASS.to_owned(),
    };
    *report.classes.entry(class).or_default() += 1;

    let ids = match persistent_ids(data) {
        Ok(ids) => ids,
        Err(error) => {
            let error = format!("the references do not read: {error}");
            report.pickles.push(PickleProblem { oid, tid, error });
            return;
        }
    };
    for id in ids {
        let PersistentId::Oid(bytes) = id else {
            continue;
        };
        let Ok(bytes) = <[u8; 8]>::try_from(bytes.as_slice()) else {
            let error = format!("a reference has an oid of {} bytes, not 8", bytes.len());
            report.pickles.push(PickleProblem { oid, tid, error });
            continue;
        };
        let target = Oid(u64::from_be_bytes(bytes));
        if target > oid {
            walk.ahead.push((target, oid, tid));
        } else if !walk.has(target) {
            walk.missing.push(Dangling {
                oid,
                tid,
                missing: target,
            });
        }
    }
}

/// Find the dangling references. A reference that the walk cannot match
/// to a current record is dangling only when the store has no current
/// record for it either.
fn dangling(store: &Store, report: &mut Verification, mut walk: Walk) {
    let mut found = std::mem::take(&mut walk.missing);
    for &(target, oid, tid) in &walk.ahead {
        if !walk.has(target) {
            found.push(Dangling {
                oid,
                tid,
                missing: target,
            });
        }
    }
    found.sort_unstable();
    found.dedup_by(|a, b| (a.oid, a.missing) == (b.oid, b.missing));
    // A walk that stopped early did not see every oid. A load of the oid
    // tells.
    found.retain(|d| matches!(store.load(d.missing), Err(ZoeError::NotFound { .. })));
    report.references = found;
}

/// Return the finding for a frame that does not read.
fn frame_problem(key: Option<(Oid, Tid)>, error: ZoeError) -> FrameProblem {
    let (oid, tid) = match (key, &error) {
        (Some((oid, tid)), _) => (Some(oid), Some(tid)),
        (None, ZoeError::CorruptRecord { oid, tid } | ZoeError::CorruptBlob { oid, tid }) => {
            (Some(*oid), Some(*tid))
        }
        (None, ZoeError::CorruptTxn { tid }) => (None, Some(*tid)),
        (None, _) => (None, None),
    };
    FrameProblem {
        oid,
        tid,
        error: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use zoe_core::vfs::MemVfs;

    use super::*;

    /// A `PersistentMapping` record that refers to `refs`.
    fn mapping(refs: &[u64]) -> Vec<u8> {
        let mut pickle = b"\x80\x03cpersistent.mapping\nPersistentMapping\nq\x00.".to_vec();
        pickle.extend_from_slice(b"\x80\x03}X\x04\x00\x00\x00refs]");
        for oid in refs {
            pickle.extend_from_slice(b"(C\x08");
            pickle.extend_from_slice(&oid.to_be_bytes());
            pickle.extend_from_slice(b"cpersistent.mapping\nPersistentMapping\ntQa");
        }
        pickle.extend_from_slice(b"s.");
        pickle
    }

    fn commit(store: &Store, writes: &[(u64, Option<Vec<u8>>)]) -> Tid {
        let tid = Tid(store.last_transaction().0 + 1);
        let mut txn = store.tpc_begin_restore(tid, b"", b"", b"").unwrap();
        for (oid, data) in writes {
            txn.restore(Oid(*oid), data.as_deref()).unwrap();
        }
        txn.vote().unwrap();
        txn.finish().unwrap()
    }

    #[test]
    fn a_sound_store_has_no_finding() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        commit(
            &store,
            &[(0, Some(mapping(&[1]))), (1, Some(mapping(&[0])))],
        );
        commit(&store, &[(1, Some(mapping(&[])))]);
        let report = verify(&store, None);
        assert_eq!(report.problems(), 0, "{report:?}");
        assert_eq!(
            (report.transactions, report.records, report.objects),
            (2, 3, 2)
        );
        let class = "persistent.mapping.PersistentMapping";
        assert_eq!(report.classes, BTreeMap::from([(class.to_owned(), 2)]));
    }

    #[test]
    fn a_reference_to_a_deleted_or_absent_oid_dangles() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        // Oid 5 refers to 1, which comes before it, and to 9, after it.
        commit(
            &store,
            &[(1, Some(mapping(&[]))), (5, Some(mapping(&[1, 9, 9, 7])))],
        );
        let deleted = commit(&store, &[(1, None)]);
        let report = verify(&store, None);
        let missing: Vec<_> = report
            .references
            .iter()
            .map(|d| (d.oid, d.missing))
            .collect();
        assert_eq!(
            missing,
            [(Oid(5), Oid(1)), (Oid(5), Oid(7)), (Oid(5), Oid(9))]
        );
        assert!(report.references.iter().all(|d| d.tid != deleted));
        assert_eq!(report.objects, 1);
    }

    #[test]
    fn a_record_that_does_not_decode_is_a_finding() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let mut broken = mapping(&[]);
        broken.truncate(broken.len() - 1);
        commit(&store, &[(0, Some(broken))]);
        let report = verify(&store, None);
        assert_eq!(report.pickles.len(), 1, "{report:?}");
        assert_eq!(report.pickles[0].oid, Oid(0));
        // The class pickle reads, so the class counts.
        let class = "persistent.mapping.PersistentMapping";
        assert_eq!(report.classes.get(class), Some(&1));
    }

    #[test]
    fn a_class_pickle_without_a_class_counts_as_unknown() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        commit(&store, &[(0, Some(b"\x80\x03K\x01.\x80\x03N.".to_vec()))]);
        let known = BTreeSet::from(["app.Thing".to_owned()]);
        let report = verify(&store, Some(&known));
        let unknown = PackReport::UNKNOWN_CLASS.to_owned();
        assert_eq!(report.classes, BTreeMap::from([(unknown.clone(), 1)]));
        assert_eq!(report.unknown_classes, [unknown]);
    }

    #[test]
    fn a_blob_is_counted_and_checked() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let mut file = store.new_blob_file().unwrap();
        file.write_all(b"bytes").unwrap();
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        let pickle = b"\x80\x03cZODB.blob\nBlob\nq\x00.\x80\x03N.";
        txn.store_blob(Oid(0), Tid::ZERO, pickle, file).unwrap();
        txn.vote().unwrap();
        txn.finish().unwrap();
        let report = verify(&store, None);
        assert_eq!(report.blobs, 1);
        assert_eq!(report.problems(), 0, "{report:?}");
    }

    #[test]
    fn a_missing_blob_file_and_a_damaged_one_have_two_texts() {
        let dir =
            std::env::temp_dir().join(format!("zoe-verify-blob-files-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(&dir).unwrap();
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        let pickle = b"\x80\x03cZODB.blob\nBlob\nq\x00.\x80\x03N.";
        for oid in [Oid(0), Oid(1)] {
            let mut file = store.new_blob_file().unwrap();
            file.write_all(b"bytes").unwrap();
            txn.store_blob(oid, Tid::ZERO, pickle, file).unwrap();
        }
        txn.vote().unwrap();
        let tid = txn.finish().unwrap();
        let (missing, damaged) = (
            store.blob_path(Oid(0), tid).unwrap(),
            store.blob_path(Oid(1), tid).unwrap(),
        );
        std::fs::remove_file(&missing).unwrap();
        let mut permissions = std::fs::metadata(&damaged).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
        std::fs::set_permissions(&damaged, permissions).unwrap();
        std::fs::write(&damaged, b"bytez").unwrap();

        let report = verify(&store, None);
        let texts: Vec<_> = report
            .blob_files
            .iter()
            .map(|p| (p.oid, p.error.as_str()))
            .collect();
        assert_eq!(
            texts,
            [(Oid(0), MISSING_BLOB_FILE), (Oid(1), DAMAGED_BLOB_FILE)]
        );
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_sequence_rows_are_read_and_counted() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        store.sequence("intids").next().unwrap();
        store.sequence("lexicon").next().unwrap();
        let report = verify(&store, None);
        assert_eq!(report.sequences, 2);
        assert_eq!(report.problems(), 0, "{report:?}");
    }

    /// Verify a store with two sequence rows, after a flip of `bit` in the
    /// last byte of the name of one of them in the database file.
    fn verify_with_a_flipped_name(bit: u8) -> Verification {
        let dir = std::env::temp_dir().join(format!(
            "zoe-verify-sequence-{bit:#04x}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let name = "verify-marker-sequence";
        {
            let store = Store::open(&dir).unwrap();
            store.sequence(name).next().unwrap();
            store.sequence("intids").next().unwrap();
        }
        let db = dir.join("zoe.redb");
        let mut bytes = std::fs::read(&db).unwrap();
        let mut flipped = 0;
        for at in 0..bytes.len().saturating_sub(name.len()) {
            if bytes[at..].starts_with(name.as_bytes()) {
                bytes[at + name.len() - 1] ^= bit;
                flipped += 1;
            }
        }
        assert!(flipped > 0, "the name is not in the database file");
        std::fs::write(&db, bytes).unwrap();

        let store = Store::open_read_only(&dir).unwrap();
        let report = verify(&store, None);
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
        report
    }

    #[test]
    fn a_damaged_sequence_row_is_a_finding() {
        // The name stays UTF-8. The frame then reads under another name,
        // so it fails its checksum.
        let report = verify_with_a_flipped_name(0x01);
        assert_eq!(report.sequences, 1, "{report:?}");
        assert_eq!(report.checksums.len(), 1, "{report:?}");
        let problem = &report.checksums[0];
        assert_eq!((problem.oid, problem.tid), (None, None));
        assert!(
            problem.error.contains("verify-marker-sequencd"),
            "{problem:?}"
        );
    }

    #[test]
    fn a_sequence_name_that_is_not_utf8_is_a_finding() {
        // Bit 0x80 makes the last byte a lead byte without its continuation
        // bytes. redb panics on such a key, and the backend catches it. The
        // list of the names fails, so no sequence row counts.
        let report = verify_with_a_flipped_name(0x80);
        assert_eq!(report.sequences, 0, "{report:?}");
        assert_eq!(report.checksums.len(), 1, "{report:?}");
        let problem = &report.checksums[0];
        assert_eq!((problem.oid, problem.tid), (None, None));
        assert!(
            problem
                .error
                .contains("sequence row after \"intids\" does not decode"),
            "{problem:?}"
        );
    }

    #[test]
    fn the_object_count_matches_the_oids_of_the_records() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        commit(&store, &[(0, Some(mapping(&[]))), (1, Some(mapping(&[])))]);
        commit(&store, &[(1, None)]);
        let report = verify(&store, None);
        // The deletion is a record, so its oid counts.
        assert_eq!(report.object_count, Some(2));
        assert_eq!(report.count, None);
        assert_eq!(report.problems(), 0, "{report:?}");
    }

    #[test]
    fn an_object_count_that_differs_is_a_finding() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        commit(&store, &[(0, Some(mapping(&[]))), (1, Some(mapping(&[])))]);
        let tid = commit(&store, &[(2, Some(mapping(&[])))]);
        // The only record of oid 2 goes with its key of the object index,
        // and the count stays.
        store.remove_record_for_test(Oid(2), tid).unwrap();
        store.remove_index_key_for_test(tid, Oid(2)).unwrap();
        let report = verify(&store, None);
        assert_eq!(
            report.count,
            Some(CountProblem {
                stored: 3,
                counted: 2
            })
        );
        assert_eq!(report.problems(), 1, "{report:?}");
    }
}
