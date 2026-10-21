//! The space of a store: what its records, its blobs and its paths take,
//! how it grew, and what a pack would remove.
//!
//! Each measure reads the store only. A walk of a large store takes time,
//! so each walk calls a progress function after each item that it reads.
//! The function can stop the walk: the call then fails with `Stopped`.
//!
//! The size of a path is the bytes of the current records of every object
//! that the object at the path reaches through its strong references,
//! itself included, each object once. The references are the ones that
//! [`Object`](crate::Object) gives: none for a record that does not
//! decode. A reference to an oid without a current record adds nothing.
//!
//! The items of a path share that size. One walk from the object, breadth
//! first, gives each object that it reaches to one owner: an item owns
//! itself, and each other object takes the owner of the object that the
//! walk reached it from first. The object itself is the owner of the
//! start. So the items and the object add up to the size of the path, and
//! each object counts once.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap, HashMap, HashSet, VecDeque};
use std::ops::ControlFlow;

use serde::Serialize;
use zoe_core::{Oid, PackReport, Store, Tid, ZoeError};
use zoe_pickle::decode_class_key;

use crate::{Error, Result, Snapshot, serialize_oid, serialize_tid, strong_references};

/// The most items of a path that [`PathSpace`] lists one by one. The
/// others add up in [`PathSpace::others`].
pub const ITEM_LIMIT: usize = 256;

/// The walk that a long call runs, for its progress.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Walk {
    /// The walk of the current records.
    Records,
    /// The walk of the transactions.
    Transactions,
    /// The walk of the references from a path.
    Objects,
}

/// The progress of a long call: the walk that runs and the number of items
/// that it has read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Progress {
    /// The walk that runs.
    pub walk: Walk,
    /// The number of items that the walk has read: records, transactions,
    /// or objects and BTree nodes.
    pub done: u64,
}

/// A number of records or objects and their bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Totals {
    /// The number of records or objects.
    pub count: u64,
    /// The bytes of their records or files.
    pub bytes: u64,
}

impl Totals {
    fn add(&mut self, bytes: u64) {
        self.count += 1;
        self.bytes += bytes;
    }
}

/// One of the largest current records.
///
/// In JSON, `oid` is a ZODB-form string.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Large {
    /// The oid of the object.
    #[serde(serialize_with = "serialize_oid")]
    pub oid: Oid,
    /// The module and the name of the class, from the class pickle, or
    /// `None` when the class pickle gives no name.
    pub class: Option<String>,
    /// The size of the record in bytes.
    pub size: u64,
}

/// What one transaction wrote.
///
/// In JSON, `tid` is a ZODB-form string.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Growth {
    /// The tid of the transaction.
    #[serde(serialize_with = "serialize_tid")]
    pub tid: Tid,
    /// The bytes of the records that the transaction wrote. A deletion
    /// adds 0.
    pub bytes: u64,
}

/// The space of a store as of its last transaction.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Space {
    /// For each class, the number of current records and their bytes. The
    /// key is `module.name` from the class pickle, or
    /// [`PackReport::UNKNOWN_CLASS`] when the class pickle gives no name.
    /// A deletion is not a current record.
    pub by_class: BTreeMap<String, Totals>,
    /// The number of current blob files and their bytes.
    pub blobs: Totals,
    /// The largest current records, biggest first. Of two records of one
    /// size, the lower oid comes first.
    pub largest: Vec<Large>,
    /// What each transaction wrote, in tid order.
    pub growth: Vec<Growth>,
}

/// One item of a path, with the objects that it reaches first.
///
/// In JSON, `oid` is a ZODB-form string.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PathItem {
    /// The name of the item: one segment of a path.
    pub name: String,
    /// The oid that the item leads to.
    #[serde(serialize_with = "serialize_oid")]
    pub oid: Oid,
    /// The class of the item, from its class pickle.
    pub class: Option<String>,
    /// The objects that the item reaches first and their bytes.
    pub reach: Totals,
}

/// The space of a path and of its items.
///
/// In JSON, `oid` is a ZODB-form string.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PathSpace {
    /// The path, with one `/` before each segment, or `/` for the Zope
    /// application.
    pub path: String,
    /// The oid of the object at the path.
    #[serde(serialize_with = "serialize_oid")]
    pub oid: Oid,
    /// The class of the object, from its class pickle.
    pub class: Option<String>,
    /// The objects that the object reaches, itself included, each once,
    /// and their bytes: the size of the path.
    pub reach: Totals,
    /// The object itself and the objects that it reaches and that no item
    /// reaches first.
    pub own: Totals,
    /// The largest items, biggest first, at most [`ITEM_LIMIT`] of them.
    /// Of two items of one size, the first in the records comes first.
    pub items: Vec<PathItem>,
    /// The items past [`ITEM_LIMIT`]: their number in `count`, and the
    /// objects that they reach first in `reach`.
    pub others: Others,
}

/// The items of a path past [`ITEM_LIMIT`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Others {
    /// The number of these items.
    pub count: u64,
    /// The objects that they reach first and their bytes.
    pub reach: Totals,
}

/// What a pack would remove, as the window shows it: the report of the dry
/// run of the core and the tid of the pack.
///
/// In JSON, `tid` is a ZODB-form string.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Reclaim {
    /// The tid that the pack packs at.
    #[serde(serialize_with = "serialize_tid")]
    pub tid: Tid,
    /// The number of records that the pack would remove.
    pub records: u64,
    /// The bytes of these records.
    pub bytes: u64,
    /// The number of objects that would lose their last record.
    pub objects: u64,
    /// The number of blob files that the pack would remove.
    pub blobs: u64,
    /// The bytes of these blob files.
    pub blob_bytes: u64,
    /// The removed records by class: their number in `count` and their
    /// bytes. The keys are the keys of the report.
    pub by_class: BTreeMap<String, Totals>,
}

impl Reclaim {
    /// Return the report `report` of a pack at `tid` as the window shows
    /// it.
    pub fn new(tid: Tid, report: &PackReport) -> Self {
        Reclaim {
            tid,
            records: report.records,
            bytes: report.bytes,
            objects: report.objects,
            blobs: report.blobs,
            blob_bytes: report.blob_bytes,
            by_class: report
                .by_class
                .iter()
                .map(|(class, totals)| {
                    let totals = Totals {
                        count: totals.records,
                        bytes: totals.bytes,
                    };
                    (class.clone(), totals)
                })
                .collect(),
        }
    }
}

/// Call `progress` and turn a stop into the error `Stopped`.
fn report(
    progress: &mut impl FnMut(Progress) -> ControlFlow<()>,
    walk: Walk,
    done: u64,
) -> Result<()> {
    match progress(Progress { walk, done }) {
        ControlFlow::Continue(()) => Ok(()),
        ControlFlow::Break(()) => Err(Error::Stopped),
    }
}

/// A record that `space` keeps among the largest: the key of the heap,
/// from its size and its oid, then its class.
type Kept = (Reverse<(u64, Reverse<Oid>)>, Option<String>);

/// Measure the current records, the current blobs and the transactions of
/// `store`. Keep the `largest` largest records.
pub(crate) fn space(
    store: &Store,
    largest: usize,
    mut progress: impl FnMut(Progress) -> ControlFlow<()>,
) -> Result<Space> {
    let mut space = Space::default();
    // The `largest` largest records so far, the smallest on top. Of two
    // records of one size, the higher oid is smaller.
    let mut kept: BinaryHeap<Kept> = BinaryHeap::new();
    let mut next = None;
    let mut done = 0;
    while let Some(record) = store.record_iternext(next)? {
        let size = record.data.len() as u64;
        let class = decode_class_key(&record.data).ok().flatten();
        let key = class.as_deref().unwrap_or(PackReport::UNKNOWN_CLASS);
        match space.by_class.get_mut(key) {
            Some(totals) => totals.add(size),
            None => {
                let mut totals = Totals::default();
                totals.add(size);
                space.by_class.insert(key.to_owned(), totals);
            }
        }
        if let Some(bytes) = blob_size(store, record.oid, record.tid)? {
            space.blobs.add(bytes);
        }
        kept.push((Reverse((size, Reverse(record.oid))), class));
        if kept.len() > largest {
            kept.pop();
        }
        done += 1;
        report(&mut progress, Walk::Records, done)?;
        match record.next {
            Some(oid) => next = Some(oid),
            None => break,
        }
    }
    // The sorted vector starts with the smallest key, which is the largest
    // record.
    space.largest = kept
        .into_sorted_vec()
        .into_iter()
        .map(|(Reverse((size, Reverse(oid))), class)| Large { oid, class, size })
        .collect();
    let mut done = 0;
    for txn in store.iterator(None, None)? {
        let txn = txn?;
        let bytes = txn
            .records
            .iter()
            .map(|record| record.data.as_ref().map_or(0, |data| data.len() as u64))
            .sum();
        space.growth.push(Growth {
            tid: txn.tid,
            bytes,
        });
        done += 1;
        report(&mut progress, Walk::Transactions, done)?;
    }
    Ok(space)
}

/// Return the size of the blob file that `tid` wrote for `oid`, or `None`
/// when the record has no blob. A blob record without its file is
/// `CorruptBlob`, as for a read of the blob.
fn blob_size(store: &Store, oid: Oid, tid: Tid) -> Result<Option<u64>> {
    let path = match store.blob_path(oid, tid) {
        Ok(path) => path,
        Err(ZoeError::NotFound { .. }) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    match std::fs::metadata(&path) {
        Ok(metadata) => Ok(Some(metadata.len())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Err(ZoeError::CorruptBlob { oid, tid }.into())
        }
        Err(e) => Err(ZoeError::Io(e).into()),
    }
}

/// Walk the references from `start` in `view`, breadth first, each object
/// once, and add each object that the walk reaches to the totals of its
/// owner. `start` has the owner 0. An oid in `owners` owns itself: its
/// owner is the index there. Each other object takes the owner of the
/// object that the walk reached it from first. `read` runs after each
/// object that the walk reads, and its error stops the walk.
fn walk(
    view: &Snapshot<'_>,
    start: Oid,
    owners: &HashMap<Oid, usize>,
    totals: &mut [Totals],
    read: &mut dyn FnMut() -> Result<()>,
) -> Result<()> {
    let mut seen = HashSet::from([start]);
    let mut queue = VecDeque::from([(start, 0)]);
    while let Some((oid, owner)) = queue.pop_front() {
        let Some((data, _)) = view.record(oid)? else {
            continue;
        };
        totals[owner].add(data.len() as u64);
        for next in walk_references(&data) {
            if seen.insert(next) {
                let owner = owners.get(&next).copied().unwrap_or(owner);
                queue.push_back((next, owner));
            }
        }
        read()?;
    }
    Ok(())
}

/// Return the oids of the strong references of `record`, as
/// [`Object`](crate::Object) gives them: none when zoe-pickle refuses the
/// record or its references.
fn walk_references(record: &[u8]) -> Vec<Oid> {
    // `persistent_ids` runs only after `decode_record` accepts the record,
    // so its own decode stays inside the default node budget. The tree of
    // the first decode is dropped at once.
    if zoe_pickle::decode_record(record).is_err() {
        return Vec::new();
    }
    strong_references(record).unwrap_or_default()
}

/// Return the size of the path `path` in the view `view`.
pub(crate) fn path_size(view: Snapshot<'_>, path: &str) -> Result<u64> {
    let oid = crate::path::resolve(&view, path)?;
    let mut totals = [Totals::default()];
    walk(&view, oid, &HashMap::new(), &mut totals, &mut || Ok(()))?;
    Ok(totals[0].bytes)
}

/// Return the space of the path `path` and of its items in the view
/// `view`.
pub(crate) fn path_space(
    view: Snapshot<'_>,
    path: &str,
    mut progress: impl FnMut(Progress) -> ControlFlow<()>,
) -> Result<PathSpace> {
    let oid = crate::path::resolve(&view, path)?;
    let mut done = 0;
    let mut read = || {
        done += 1;
        report(&mut progress, Walk::Objects, done)
    };
    let found = crate::path::items(&view, oid, &mut read)?;
    // The owner 0 is the object itself, and the owner of each item is its
    // place in `found`, plus 1. Of two names of one oid, the first owns
    // it.
    let mut owners = HashMap::new();
    for (at, (_, item)) in found.iter().enumerate() {
        owners.entry(*item).or_insert(at + 1);
    }
    let mut totals = vec![Totals::default(); found.len() + 1];
    walk(&view, oid, &owners, &mut totals, &mut read)?;
    let own = totals[0];
    let mut reach = own;
    // An item that the walk does not reach, such as a weak reference or a
    // second name of one oid, takes no space and is not listed.
    let mut items: Vec<(String, Oid, Totals)> = found
        .into_iter()
        .zip(&totals[1..])
        .filter(|(_, totals)| totals.count > 0)
        .map(|((name, item), totals)| {
            reach.count += totals.count;
            reach.bytes += totals.bytes;
            (name, item, *totals)
        })
        .collect();
    // A stable sort keeps the order of the records among equal sizes.
    items.sort_by_key(|(_, _, totals)| Reverse(totals.bytes));
    let mut others = Others::default();
    for (_, _, totals) in items.iter().skip(ITEM_LIMIT) {
        others.count += 1;
        others.reach.count += totals.count;
        others.reach.bytes += totals.bytes;
    }
    items.truncate(ITEM_LIMIT);
    let class_at = |oid| -> Result<Option<String>> {
        Ok(view
            .record(oid)?
            .and_then(|(data, _)| decode_class_key(&data).ok().flatten()))
    };
    let items = items
        .into_iter()
        .map(|(name, oid, reach)| {
            Ok(PathItem {
                name,
                oid,
                class: class_at(oid)?,
                reach,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(PathSpace {
        path: normal_path(path),
        oid,
        class: class_at(oid)?,
        reach,
        own,
        items,
        others,
    })
}

/// Return `path` with one `/` before each segment, or `/` for no segment.
fn normal_path(path: &str) -> String {
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    format!("/{}", segments.join("/"))
}

impl Snapshot<'_> {
    /// Return the record of `oid` and its serial, or `None` when `oid` has
    /// no record in the view or its record is a deletion.
    fn record(&self, oid: Oid) -> Result<Option<(Vec<u8>, Tid)>> {
        match self.load(oid) {
            Ok(found) => Ok(Some(found)),
            Err(Error::NoRecordAsOf { .. } | Error::Store(ZoeError::NotFound { .. })) => Ok(None),
            Err(error) => Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::path::PathBuf;

    use zoe_pickle::testing::{record, text};

    use super::*;
    use crate::Folder;

    /// A persistent reference to `oid` with the class `m.C`.
    fn reference(oid: u64) -> Vec<u8> {
        zoe_pickle::testing::reference(oid, "m", "C")
    }

    /// A weak reference to `oid`: `['w', (oid,)]`.
    fn weak(oid: u64) -> Vec<u8> {
        [
            &b"("[..],
            &text("w"),
            b"C\x08",
            &oid.to_be_bytes(),
            b"\x85lQ",
        ]
        .concat()
    }

    /// A dict state with the text keys and the pickled values of `entries`.
    fn dict(entries: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut state = b"}".to_vec();
        for (key, value) in entries {
            state.extend(text(key));
            state.extend(value);
            state.extend(b"s");
        }
        state
    }

    /// An object of the class `m.Item` with the attributes `entries`.
    fn item(entries: &[(&str, Vec<u8>)]) -> Vec<u8> {
        record("m", "Item", &dict(entries))
    }

    /// A small `OOBTree` that maps each name of `entries` to a reference.
    fn tree(entries: &[(&str, u64)]) -> Vec<u8> {
        let mut state = b"(".to_vec();
        for (key, oid) in entries {
            state.extend(text(key));
            state.extend(reference(*oid));
        }
        state.extend(b"t\x85\x85\x85");
        record("BTrees.OOBTree", "OOBTree", &state)
    }

    /// Return a new directory for one test.
    fn fresh(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("zoe-loupe-space-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// The records of one transaction by oid. `None` is a deletion.
    type Records = Vec<(u64, Option<Vec<u8>>)>;

    /// Write a store with one restore transaction for each item of
    /// `txns`: its tid and its records by oid. `None` is a deletion.
    fn store_with(name: &str, txns: &[(u64, Records)]) -> PathBuf {
        let dir = fresh(name);
        let store = Store::open(&dir).unwrap();
        for (tid, records) in txns {
            let mut txn = store.tpc_begin_restore(Tid(*tid), b"", b"", b"").unwrap();
            for (oid, data) in records {
                txn.restore(Oid(*oid), data.as_deref()).unwrap();
            }
            txn.vote().unwrap();
            txn.finish().unwrap();
        }
        dir
    }

    /// The root mapping, with the application at oid 1.
    fn root() -> Vec<u8> {
        record(
            "persistent.mapping",
            "PersistentMapping",
            &dict(&[("data", dict(&[("Application", reference(1))]))]),
        )
    }

    fn go_on(_: Progress) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    #[test]
    fn the_space_counts_current_records_by_class() {
        let small = item(&[]);
        let large = item(&[("title", text(&"x".repeat(100)))]);
        let other = record("m", "Other", b"}");
        // A persistent class: the class pickle is a persistent id.
        let unknown = [&b"\x80\x03"[..], &reference(9), b".\x80\x03}."].concat();
        let dir = store_with(
            "class",
            &[
                (
                    0x10,
                    vec![(1, Some(large.clone())), (2, Some(small.clone()))],
                ),
                (
                    0x20,
                    vec![(3, Some(other.clone())), (4, Some(unknown.clone()))],
                ),
                // The deletion of 2 and a new record of 1.
                (0x30, vec![(2, None), (1, Some(small.clone()))]),
                (0x40, vec![(5, Some(large.clone()))]),
            ],
        );
        let folder = Folder::open(&dir).unwrap();
        let space = folder.space(10).unwrap();
        let size = |data: &[u8]| data.len() as u64;
        let expected = BTreeMap::from([
            (
                "m.Item".to_owned(),
                Totals {
                    count: 2,
                    bytes: size(&small) + size(&large),
                },
            ),
            (
                "m.Other".to_owned(),
                Totals {
                    count: 1,
                    bytes: size(&other),
                },
            ),
            (
                PackReport::UNKNOWN_CLASS.to_owned(),
                Totals {
                    count: 1,
                    bytes: size(&unknown),
                },
            ),
        ]);
        assert_eq!(space.by_class, expected);
        assert_eq!(space.blobs, Totals::default());
        // Biggest first; of two records of one size, the lower oid first.
        let largest: Vec<(u64, Option<&str>, u64)> = space
            .largest
            .iter()
            .map(|l| (l.oid.0, l.class.as_deref(), l.size))
            .collect();
        let mut expected = vec![
            (5, Some("m.Item"), size(&large)),
            (1, Some("m.Item"), size(&small)),
            (3, Some("m.Other"), size(&other)),
            (4, None, size(&unknown)),
        ];
        expected.sort_by_key(|(oid, _, size)| (Reverse(*size), *oid));
        assert_eq!(largest, expected);
        // A deletion adds 0 to the growth.
        let growth: Vec<(u64, u64)> = space.growth.iter().map(|g| (g.tid.0, g.bytes)).collect();
        assert_eq!(
            growth,
            [
                (0x10, size(&large) + size(&small)),
                (0x20, size(&other) + size(&unknown)),
                (0x30, size(&small)),
                (0x40, size(&large)),
            ]
        );
        assert_eq!(folder.space(1).unwrap().largest.len(), 1);
        assert!(folder.space(0).unwrap().largest.is_empty());
    }

    #[test]
    fn the_space_counts_the_current_blob_files() {
        let dir = fresh("blobs");
        {
            let store = Store::open(&dir).unwrap();
            for (oid, bytes) in [(1, &b"twelve bytes"[..]), (2, b"four")] {
                let mut file = store.new_blob_file().unwrap();
                file.write_all(bytes).unwrap();
                let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
                txn.store_blob(Oid(oid), Tid::ZERO, &item(&[]), file)
                    .unwrap();
                txn.vote().unwrap();
                txn.finish().unwrap();
            }
            // A new record of 2 without a blob: its blob is not current.
            let serial = store.load(Oid(2)).unwrap().1;
            let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
            txn.store(Oid(2), serial, &item(&[])).unwrap();
            txn.vote().unwrap();
            txn.finish().unwrap();
        }
        let folder = Folder::open(&dir).unwrap();
        let space = folder.space(10).unwrap();
        assert_eq!(
            space.blobs,
            Totals {
                count: 1,
                bytes: 12
            }
        );
    }

    #[test]
    fn the_progress_function_sees_each_walk_and_can_stop_it() {
        let dir = store_with(
            "progress",
            &[
                (0x10, vec![(0, Some(root())), (1, Some(item(&[])))]),
                (0x20, vec![(2, Some(item(&[])))]),
            ],
        );
        let folder = Folder::open(&dir).unwrap();
        let mut seen = Vec::new();
        folder
            .space_with(1, |progress| {
                seen.push(progress);
                ControlFlow::Continue(())
            })
            .unwrap();
        let at = |walk, done| Progress { walk, done };
        assert_eq!(
            seen,
            [
                at(Walk::Records, 1),
                at(Walk::Records, 2),
                at(Walk::Records, 3),
                at(Walk::Transactions, 1),
                at(Walk::Transactions, 2),
            ]
        );
        let stopped = folder.space_with(1, |_| ControlFlow::Break(()));
        assert!(matches!(stopped, Err(Error::Stopped)), "{stopped:?}");
        let stopped = folder.path_space("/", |_| ControlFlow::Break(()));
        assert!(matches!(stopped, Err(Error::Stopped)), "{stopped:?}");
    }

    /// A site at `/site` with two items in its `_tree` that share an
    /// object, annotations, and a weak reference.
    fn site() -> (PathBuf, BTreeMap<u64, Vec<u8>>) {
        let records = BTreeMap::from([
            (0, root()),
            (1, item(&[("site", reference(2))])),
            (
                2,
                item(&[
                    ("_tree", reference(3)),
                    ("__annotations__", reference(6)),
                    ("title", text("Site")),
                    ("link", weak(9)),
                ]),
            ),
            (3, tree(&[("a", 4), ("b", 5)])),
            (
                4,
                item(&[("shared", reference(7)), ("child", reference(8))]),
            ),
            (5, item(&[("shared", reference(7)), ("text", text("b"))])),
            (6, item(&[("key", text("value"))])),
            (7, item(&[("text", text(&"s".repeat(40)))])),
            (8, item(&[])),
            (9, item(&[("text", text("weak"))])),
        ]);
        let txn = records
            .iter()
            .map(|(oid, data)| (*oid, Some(data.clone())))
            .collect();
        (store_with("site", &[(0x10, txn)]), records)
    }

    #[test]
    fn the_items_of_a_path_share_its_size_each_object_once() {
        let (dir, records) = site();
        let folder = Folder::open(&dir).unwrap();
        let bytes =
            |oids: &[u64]| -> u64 { oids.iter().map(|oid| records[oid].len() as u64).sum() };
        let totals = |oids: &[u64]| Totals {
            count: oids.len() as u64,
            bytes: bytes(oids),
        };

        let space = folder.path_space("/site/", go_on).unwrap();
        assert_eq!(space.path, "/site");
        assert_eq!(space.oid, Oid(2));
        assert_eq!(space.class.as_deref(), Some("m.Item"));
        // The weak reference keeps nothing, so 9 is not in the path.
        assert_eq!(space.reach, totals(&[2, 3, 4, 5, 6, 7, 8]));
        assert_eq!(space.reach.bytes, folder.path_size("/site").unwrap());
        assert_eq!(space.own, totals(&[2]));
        // The walk goes breadth first: `a` reaches the shared object 7
        // before `b`.
        let mut found: Vec<(&str, u64, Totals)> = space
            .items
            .iter()
            .map(|i| (i.name.as_str(), i.oid.0, i.reach))
            .collect();
        found.sort_by_key(|(name, _, _)| *name);
        assert_eq!(
            found,
            [
                ("__annotations__", 6, totals(&[6])),
                ("_tree", 3, totals(&[3])),
                ("a", 4, totals(&[4, 7, 8])),
                ("b", 5, totals(&[5])),
            ]
        );
        for pair in space.items.windows(2) {
            assert!(pair[0].reach.bytes >= pair[1].reach.bytes);
        }
        assert_eq!(space.others, Others::default());

        // The application: the site is its one item.
        let app = folder.path_space("/", go_on).unwrap();
        assert_eq!(app.path, "/");
        assert_eq!(app.own, totals(&[1]));
        assert_eq!(app.items.len(), 1);
        assert_eq!(app.items[0].reach, space.reach);
        assert_eq!(app.reach.bytes, folder.path_size("").unwrap());
        assert!(matches!(
            folder.path_space("/site/none", go_on),
            Err(Error::NoObject { .. })
        ));
    }

    #[test]
    fn items_past_the_limit_add_up_in_others() {
        let count = ITEM_LIMIT + 4;
        let names: Vec<String> = (0..count).map(|i| format!("item-{i:04}")).collect();
        let entries: Vec<(&str, u64)> = names
            .iter()
            .enumerate()
            .map(|(i, name)| (name.as_str(), 4 + i as u64))
            .collect();
        let mut txn = vec![
            (0, Some(root())),
            (1, Some(item(&[("folder", reference(2))]))),
            (2, Some(item(&[("_tree", reference(3))]))),
            (3, Some(tree(&entries))),
        ];
        // The first item is the smallest, so it goes past the limit.
        for (i, (_, oid)) in entries.iter().enumerate() {
            let title = if i == 0 {
                String::new()
            } else {
                "x".repeat(50)
            };
            txn.push((*oid, Some(item(&[("title", text(&title))]))));
        }
        let dir = store_with("others", &[(0x10, txn)]);
        let folder = Folder::open(&dir).unwrap();
        let space = folder.path_space("/folder", go_on).unwrap();
        // `_tree` is an item too.
        assert_eq!(space.items.len(), ITEM_LIMIT);
        assert_eq!(space.others.count, count as u64 + 1 - ITEM_LIMIT as u64);
        assert!(space.items.iter().all(|i| i.name != "item-0000"));
        // The BTree is the largest item. Of two items of one size, the
        // first in the records comes first.
        assert_eq!(space.items[0].name, "_tree");
        assert_eq!(space.items[1].name, "item-0001");
        assert_eq!(space.items[2].name, "item-0002");
        let listed: u64 = space.items.iter().map(|i| i.reach.bytes).sum();
        assert_eq!(
            space.own.bytes + listed + space.others.reach.bytes,
            space.reach.bytes
        );
        assert_eq!(space.reach.bytes, folder.path_size("/folder").unwrap());
        assert_eq!(space.reach.count, count as u64 + 2);
    }

    #[test]
    fn what_a_pack_would_reclaim_shows_the_report() {
        let mut report = PackReport {
            records: 3,
            bytes: 30,
            objects: 1,
            blobs: 1,
            blob_bytes: 7,
            ..PackReport::default()
        };
        report.by_class.insert(
            "m.C".to_owned(),
            zoe_core::ClassTotals {
                records: 3,
                bytes: 30,
            },
        );
        let reclaim = Reclaim::new(Tid(0x0100), &report);
        assert_eq!(
            serde_json::to_value(&reclaim).unwrap(),
            serde_json::json!({
                "tid": "0x0100",
                "records": 3,
                "bytes": 30,
                "objects": 1,
                "blobs": 1,
                "blob_bytes": 7,
                "by_class": {"m.C": {"count": 3, "bytes": 30}},
            })
        );
    }

    #[test]
    fn the_space_serializes_with_zodb_ids() {
        let space = Space {
            by_class: BTreeMap::from([("m.C".to_owned(), Totals { count: 1, bytes: 9 })]),
            blobs: Totals::default(),
            largest: vec![Large {
                oid: Oid(1),
                class: None,
                size: 9,
            }],
            growth: vec![Growth {
                tid: Tid(0x0100),
                bytes: 9,
            }],
        };
        assert_eq!(
            serde_json::to_value(&space).unwrap(),
            serde_json::json!({
                "by_class": {"m.C": {"count": 1, "bytes": 9}},
                "blobs": {"count": 0, "bytes": 0},
                "largest": [{"oid": "0x01", "class": null, "size": 9}],
                "growth": [{"tid": "0x0100", "bytes": 9}],
            })
        );
        let progress = Progress {
            walk: Walk::Transactions,
            done: 2,
        };
        assert_eq!(
            serde_json::to_value(progress).unwrap(),
            serde_json::json!({"walk": "transactions", "done": 2})
        );
    }
}
