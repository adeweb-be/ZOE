//! Acceptance test of the space screen of Loupe (F15).
//!
//! `Folder::space(largest)` measures the store as of its last tid:
//!
//! - `by_class`: for each class, the number of current records and their
//!   bytes;
//! - `blobs`: the number of current blob files and their bytes;
//! - `largest`: the `largest` biggest current records, biggest first;
//! - `growth`: for each transaction, its tid and the bytes of the records
//!   that it wrote, in tid order.
//!
//! `Folder::path_size(path)` gives the bytes of the current records of
//! every object that the object at `path` reaches through its references,
//! itself included, each object once. `Folder::reclaimable(at)` gives what
//! a pack at `at` would remove: the report of `Store::pack_dry_run`.
//!
//! It reads the store only. The window shows these as a treemap, tables
//! and a timeline.

mod common;

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use zoe_core::Store;
use zoe_loupe::Folder;

use common::{class_name, files, fresh_dir, write_store};

#[test]
fn the_space_by_class_matches_the_current_records() {
    let dir = fresh_dir("space-class");
    let fixture = write_store(&dir);
    let last = fixture.last_tid();
    let mut expected: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    for oid in fixture.created().keys() {
        if let Some((_, data)) = fixture.revision(*oid, last) {
            if data.is_empty() {
                continue;
            }
            let entry = expected.entry(class_name(data)).or_default();
            entry.0 += 1;
            entry.1 += data.len() as u64;
        }
    }

    let before = files(&dir);
    let folder = Folder::open(&dir).unwrap();
    let space = folder.space(3).unwrap();
    let found: BTreeMap<String, (u64, u64)> = space
        .by_class
        .iter()
        .map(|(class, c)| (class.clone(), (c.count, c.bytes)))
        .collect();
    assert_eq!(found, expected);

    // The largest records, biggest first.
    assert_eq!(space.largest.len(), 3);
    for pair in space.largest.windows(2) {
        assert!(pair[0].size >= pair[1].size);
    }
    let biggest = fixture
        .created()
        .keys()
        .filter_map(|oid| fixture.revision(*oid, last))
        .map(|(_, data)| data.len() as u64)
        .max()
        .unwrap();
    assert_eq!(space.largest[0].size, biggest);

    // The growth has one entry per transaction.
    assert_eq!(space.growth.len(), fixture.txns.len());
    for (point, txn) in space.growth.iter().zip(&fixture.txns) {
        assert_eq!(point.tid, txn.tid);
        let written: u64 = txn.records.iter().map(|(_, d)| d.len() as u64).sum();
        assert_eq!(point.bytes, written);
    }
    drop(folder);
    assert_eq!(files(&dir), before, "the space screen changed the store");
}

#[test]
fn the_space_of_a_path_counts_what_it_reaches_once() {
    let dir = fresh_dir("space-path");
    let fixture = write_store(&dir);
    let folder = Folder::open(&dir).unwrap();

    // The objects that the folder reaches, by a walk of the references.
    let (start, _) = fixture.at_path("/plone/folder");
    let mut seen = BTreeSet::new();
    let mut queue = VecDeque::from([start]);
    let mut bytes = 0;
    while let Some(oid) = queue.pop_front() {
        if !seen.insert(oid) {
            continue;
        }
        let object = folder.object(oid).unwrap();
        bytes += object.size;
        queue.extend(object.references.iter().map(|r| r.oid));
    }

    assert_eq!(folder.path_size("/plone/folder").unwrap(), bytes);
    let site = folder.path_size("/plone").unwrap();
    let doc = folder.path_size("/plone/folder/doc").unwrap();
    assert!(site >= bytes && bytes >= doc);
}

#[test]
fn what_a_pack_would_reclaim_is_the_dry_run_of_the_core() {
    let dir = fresh_dir("space-pack");
    let fixture = write_store(&dir);
    let at = fixture.last_tid();
    let expected = {
        let store = Store::open_read_only(&dir).unwrap();
        store.pack_dry_run(at).unwrap()
    };
    let folder = Folder::open(&dir).unwrap();
    assert_eq!(folder.reclaimable(at).unwrap(), expected);
}
