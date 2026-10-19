//! The order merge of ADR 0052 in the vote: the order list of a folder of
//! plone.folder and its positions change together.
//!
//! The records are those that plone.folder writes, built with the pickler
//! of `zoe_pickle::python`: a folder whose state names its annotations
//! under `__annotations__`, the annotations, an `OOBTree` with its bucket
//! inline that names the order and the positions, the order, a
//! `persistent.list.PersistentList` of ids, and the positions, an
//! `OIBTree` of id to index. A creation appends its id to the order,
//! inserts it into the positions with the index `len(order) - 1`, sets
//! `modification_date` of the folder. After each
//! commit, the test reads the store: the order holds each id once, and the
//! positions are `{id: index}` of the order.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use zoe_core::vfs::MemVfs;
use zoe_core::{
    Clock, Mode, Observer, Oid, Options, Reason, Resolved, Store, SystemClock, Tid, Txn, Waiter,
    ZoeError,
};
use zoe_pickle::btrees::{TreeState, bucket_state, new_bucket_state, tree_state};
use zoe_pickle::python::{Heap, Object, Pickler, Value};

// The records.

/// Returns a reference to `oid` of the class `module.name`.
fn reference(heap: &mut Heap, oid: Oid, module: &str, name: &str) -> Value {
    let bytes = heap.new_bytes(&oid.0.to_be_bytes());
    let class = heap.new_class(module, name);
    let data = heap.new_tuple(vec![bytes, class]);
    heap.new_reference(data).unwrap()
}

fn record(heap: &Heap, class: Value, state: Value) -> Vec<u8> {
    let mut pickler = Pickler::new(heap);
    pickler.dump(class).unwrap();
    pickler.dump(state).unwrap();
    pickler.into_bytes()
}

/// The record of a folder whose annotations are `annotations`.
fn folder(annotations: Oid, date: i128) -> Vec<u8> {
    let mut heap = Heap::with_instances();
    let class = heap.new_class("m", "Folder");
    let a = reference(&mut heap, annotations, "BTrees.OOBTree", "OOBTree");
    let items = vec![
        (heap.new_str("__annotations__"), a),
        (heap.new_str("modification_date"), Value::Int(date)),
    ];
    let state = heap.new_dict(items);
    record(&heap, class, state)
}

/// The record of the annotations that name `order` and `pos`.
fn annotations(order: Oid, pos: Oid) -> Vec<u8> {
    let mut heap = Heap::with_instances();
    let class = heap.new_class("BTrees.OOBTree", "OOBTree");
    let items = vec![
        heap.new_str("plone.folder.ordered.order"),
        reference(&mut heap, order, "persistent.list", "PersistentList"),
        heap.new_str("plone.folder.ordered.pos"),
        reference(&mut heap, pos, "BTrees.OIBTree", "OIBTree"),
    ];
    let bucket = new_bucket_state(&mut heap, items, None);
    let inner = heap.new_tuple(vec![bucket]);
    let state = heap.new_tuple(vec![inner]);
    record(&heap, class, state)
}

/// The record of an order of `ids`, of the class `module.name`.
fn list_of(module: &str, name: &str, ids: &[Value], heap: &mut Heap) -> Vec<u8> {
    let class = heap.new_class(module, name);
    let data = heap.new_list(ids.to_vec());
    let key = heap.new_str("data");
    let state = heap.new_dict(vec![(key, data)]);
    record(heap, class, state)
}

/// The record of the order `ids`.
fn list(ids: &[String]) -> Vec<u8> {
    let mut heap = Heap::with_instances();
    let values: Vec<Value> = ids.iter().map(|id| heap.new_str(id)).collect();
    list_of("persistent.list", "PersistentList", &values, &mut heap)
}

fn pos_items(heap: &mut Heap, items: &[(String, i128)]) -> Vec<Value> {
    let mut values = Vec::new();
    for (id, index) in items {
        values.push(heap.new_str(id));
        values.push(Value::Int(*index));
    }
    values
}

/// The record of positions that hold their bucket inline.
fn pos_inline(items: &[(String, i128)]) -> Vec<u8> {
    let mut heap = Heap::with_instances();
    let class = heap.new_class("BTrees.OIBTree", "OIBTree");
    let values = pos_items(&mut heap, items);
    let bucket = new_bucket_state(&mut heap, values, None);
    let inner = heap.new_tuple(vec![bucket]);
    let state = heap.new_tuple(vec![inner]);
    record(&heap, class, state)
}

/// The record of a bucket of positions.
fn pos_bucket(items: &[(String, i128)], next: Option<Oid>) -> Vec<u8> {
    let mut heap = Heap::with_instances();
    let class = heap.new_class("BTrees.OIBTree", "OIBucket");
    let values = pos_items(&mut heap, items);
    let next = next.map(|next| reference(&mut heap, next, "BTrees.OIBTree", "OIBucket"));
    let state = new_bucket_state(&mut heap, values, next);
    record(&heap, class, state)
}

/// The record of a node of positions: its children, each with the key of
/// its separator (none for the first), of the class `child`, and the oid of
/// its first bucket.
fn pos_node(children: &[(Option<&str>, Oid)], child: &str, first: Oid) -> Vec<u8> {
    let mut heap = Heap::with_instances();
    let class = heap.new_class("BTrees.OIBTree", "OIBTree");
    let mut items = Vec::new();
    for (separator, oid) in children {
        if let Some(separator) = separator {
            items.push(heap.new_str(separator));
        }
        items.push(reference(&mut heap, *oid, "BTrees.OIBTree", child));
    }
    let children = heap.new_tuple(items);
    let first = reference(&mut heap, first, "BTrees.OIBTree", "OIBucket");
    let state = heap.new_tuple(vec![children, first]);
    record(&heap, class, state)
}

/// The root of ZODB, a `PersistentMapping` that names the policy at
/// `policy`, as ZODB writes it.
fn root(policy: Oid) -> Vec<u8> {
    [
        &b"\x80\x03cpersistent.mapping\nPersistentMapping\nq\x00.\x80\x03}q\x01X\x04\x00\x00\x00\
           dataq\x02}q\x03X\x10\x00\x00\x00zoe.merge_policyq\x04C\x08"[..],
        &policy.0.to_be_bytes(),
        b"q\x05czoe.policy\nMergePolicy\nq\x06\x86q\x07Qss.",
    ]
    .concat()
}

/// The settings of a policy of the tests.
#[derive(Clone, Copy)]
struct Settings<'a> {
    folder_orders: Option<&'a str>,
    /// The mode of tier 1 of `BTrees.OIBTree.OIBucket`.
    bucket_tier1: Option<&'a str>,
    /// A tier 2 entry for `persistent.list.PersistentList` that appends
    /// to `data`, in this mode.
    list_tier2: Option<&'a str>,
    /// The switch `split_merges`.
    split_merges: bool,
}

const ON: Settings<'static> = Settings {
    folder_orders: Some("on"),
    bucket_tier1: None,
    list_tier2: None,
    split_merges: true,
};

/// The record of a policy: the split merge on, the folder class merges
/// `modification_date` with `max`, and the settings.
fn policy(settings: Settings<'_>) -> Vec<u8> {
    let mut heap = Heap::new();
    let class = heap.new_class("zoe.policy", "MergePolicy");
    let s = |heap: &mut Heap, text: &str| heap.new_str(text);
    let fields = vec![(s(&mut heap, "modification_date"), s(&mut heap, "max"))];
    let fields = heap.new_dict(fields);
    let entry = vec![
        (s(&mut heap, "tier2"), s(&mut heap, "on")),
        (s(&mut heap, "default"), s(&mut heap, "strict")),
        (s(&mut heap, "fields"), fields),
    ];
    let entry = heap.new_dict(entry);
    let mut entries = vec![(s(&mut heap, "m.Folder"), entry)];
    if let Some(mode) = settings.bucket_tier1 {
        let entry = vec![(s(&mut heap, "tier1"), s(&mut heap, mode))];
        let entry = heap.new_dict(entry);
        entries.push((s(&mut heap, "BTrees.OIBTree.OIBucket"), entry));
    }
    if let Some(mode) = settings.list_tier2 {
        let fields = vec![(s(&mut heap, "data"), s(&mut heap, "append"))];
        let fields = heap.new_dict(fields);
        let entry = vec![
            (s(&mut heap, "tier2"), s(&mut heap, mode)),
            (s(&mut heap, "fields"), fields),
        ];
        let entry = heap.new_dict(entry);
        entries.push((s(&mut heap, "persistent.list.PersistentList"), entry));
    }
    let classes = heap.new_dict(entries);
    let mut items = vec![
        (s(&mut heap, "version"), Value::Int(1)),
        (s(&mut heap, "classes"), classes),
    ];
    if settings.split_merges {
        items.push((s(&mut heap, "split_merges"), s(&mut heap, "on")));
    }
    if let Some(mode) = settings.folder_orders {
        items.push((s(&mut heap, "folder_orders"), s(&mut heap, mode)));
    }
    let state = heap.new_dict(items);
    record(&heap, class, state)
}

fn open(settings: Settings<'_>) -> Store {
    open_on(SystemClock, settings)
}

fn open_on(clock: impl Clock + 'static, settings: Settings<'_>) -> Store {
    let options = Options {
        structural_merges: zoe_core::StructuralMerges {
            resolution_log: true,
            ..Default::default()
        },
        ..Options::default()
    };
    let store = Store::open_full(MemVfs::new(), clock, options).unwrap();
    let oid = new_oid(&store);
    let mut txn = store.tpc_begin(b"user", b"policy", b"").unwrap();
    txn.store(oid, Tid::ZERO, &policy(settings)).unwrap();
    txn.vote().unwrap();
    txn.finish().unwrap();
    let mut txn = store.tpc_begin(b"user", b"root", b"").unwrap();
    txn.store(Oid(0), Tid::ZERO, &root(oid)).unwrap();
    txn.vote().unwrap();
    txn.finish().unwrap();
    store
}

fn new_oid(store: &Store) -> Oid {
    loop {
        let oid = store.new_oid().unwrap();
        if oid != Oid(0) {
            return oid;
        }
    }
}

// The site and its snapshots.

/// The records of the folder of a test.
#[derive(Clone, Copy, Debug)]
struct Site {
    folder: Oid,
    annotations: Oid,
    order: Oid,
    pos: Oid,
}

/// A bucket of positions, as a snapshot reads it.
#[derive(Clone, Debug)]
struct Bucket {
    oid: Oid,
    serial: Tid,
    items: Vec<(String, i128)>,
}

/// The positions, as a snapshot reads them.
#[derive(Clone, Debug)]
enum Pos {
    Inline(Vec<(String, i128)>),
    Node(Vec<Bucket>),
}

/// What a transaction reads of the folder.
#[derive(Clone, Debug)]
struct Snap {
    folder: Tid,
    date: i128,
    order: Vec<String>,
    order_serial: Tid,
    pos: Pos,
    pos_serial: Tid,
}

/// Returns the id of number `n`.
fn id(n: usize) -> String {
    format!("d{n:03}")
}

/// Returns the positions of `order`.
fn positions(order: &[String]) -> Vec<(String, i128)> {
    let mut items: Vec<(String, i128)> = order
        .iter()
        .enumerate()
        .map(|(index, id)| (id.clone(), index as i128))
        .collect();
    items.sort();
    items
}

/// Commits a folder of `ids`. With `bucket`, the positions are a node of
/// buckets of at most `bucket` items each; else they hold their bucket
/// inline.
fn site(store: &Store, ids: &[String], bucket: Option<usize>) -> Site {
    let site = Site {
        folder: new_oid(store),
        annotations: new_oid(store),
        order: new_oid(store),
        pos: new_oid(store),
    };
    let mut writes = vec![
        (site.folder, folder(site.annotations, 0)),
        (site.annotations, annotations(site.order, site.pos)),
        (site.order, list(ids)),
    ];
    let items = positions(ids);
    match bucket {
        None => writes.push((site.pos, pos_inline(&items))),
        Some(size) => {
            let chunks: Vec<_> = items.chunks(size).collect();
            let oids: Vec<Oid> = chunks.iter().map(|_| new_oid(store)).collect();
            let children: Vec<(Option<&str>, Oid)> = chunks
                .iter()
                .zip(&oids)
                .enumerate()
                .map(|(index, (chunk, &oid))| ((index > 0).then(|| chunk[0].0.as_str()), oid))
                .collect();
            writes.push((site.pos, pos_node(&children, "OIBucket", oids[0])));
            for (index, chunk) in chunks.iter().enumerate() {
                writes.push((oids[index], pos_bucket(chunk, oids.get(index + 1).copied())));
            }
        }
    }
    let mut txn = store.tpc_begin(b"user", b"site", b"").unwrap();
    for (oid, data) in &writes {
        txn.store(*oid, Tid::ZERO, data).unwrap();
    }
    txn.vote().unwrap();
    txn.finish().unwrap();
    site
}

/// Loads `oid` with a heap of instances: its class, its state and the
/// serial.
fn load(store: &Store, oid: Oid) -> (Heap, Value, Value, Tid) {
    let (data, serial) = store.load(oid).unwrap();
    let mut heap = Heap::with_instances();
    let tree = zoe_pickle::decode_record(&data).unwrap();
    let (class, state) = heap.load(&tree).unwrap();
    (heap, class, state, serial)
}

fn oid_of(heap: &Heap, value: Value) -> Oid {
    let reference = heap.reference(value).unwrap();
    Oid(u64::from_be_bytes(reference.oid[..].try_into().unwrap()))
}

fn items_of(heap: &Heap, bucket: Value) -> Vec<(String, i128)> {
    let state = bucket_state(heap, bucket, true).unwrap();
    state
        .items
        .chunks(2)
        .map(|pair| {
            let Value::Int(index) = pair[1] else {
                panic!("a position that is not an int")
            };
            (heap.str(pair[0]).unwrap().to_owned(), index)
        })
        .collect()
}

/// Reads the snapshot of `site` from the store.
fn snap(store: &Store, site: Site) -> Snap {
    let (heap, _, state, folder_serial) = load(store, site.folder);
    let date = heap
        .dict(state)
        .unwrap()
        .iter()
        .find(|(key, _)| heap.str(*key) == Some("modification_date"))
        .map(|&(_, value)| match value {
            Value::Int(date) => date,
            _ => panic!("a date that is not an int"),
        })
        .unwrap();
    let (heap, _, state, order_serial) = load(store, site.order);
    let data = heap.dict(state).unwrap()[0].1;
    let Some(Object::List(values)) = heap.object(data) else {
        panic!("an order that is not a list")
    };
    let order = values
        .iter()
        .map(|&value| heap.str(value).unwrap().to_owned())
        .collect();
    let (heap, _, state, pos_serial) = load(store, site.pos);
    let pos = match tree_state(&heap, state).unwrap() {
        TreeState::Inline(bucket) => Pos::Inline(items_of(&heap, bucket)),
        TreeState::Split => Pos::Node(buckets(store, &heap, state)),
        TreeState::Empty => Pos::Inline(Vec::new()),
    };
    Snap {
        folder: folder_serial,
        date,
        order,
        order_serial,
        pos,
        pos_serial,
    }
}

/// Returns the buckets under the node state `state`, in order, through
/// the child nodes.
fn buckets(store: &Store, heap: &Heap, state: Value) -> Vec<Bucket> {
    let &[children, _] = heap.tuple(state).unwrap() else {
        panic!("a node of another shape")
    };
    let mut out = Vec::new();
    for &child in heap.tuple(children).unwrap().iter().step_by(2) {
        let oid = oid_of(heap, child);
        let (child_heap, class, child_state, serial) = load(store, oid);
        if child_heap.class(class).unwrap().1 == "OIBucket" {
            out.push(Bucket {
                oid,
                serial,
                items: items_of(&child_heap, child_state),
            });
        } else {
            out.extend(buckets(store, &child_heap, child_state));
        }
    }
    out
}

/// Asserts that the order of `site` holds each id once and that the
/// positions agree with it. Returns the order.
fn assert_agrees(store: &Store, site: Site) -> Vec<String> {
    let snap = snap(store, site);
    let mut sorted = snap.order.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        snap.order.len(),
        "an id twice: {:?}",
        snap.order
    );
    let found: Vec<(String, i128)> = match &snap.pos {
        Pos::Inline(items) => items.clone(),
        Pos::Node(buckets) => buckets.iter().flat_map(|b| b.items.clone()).collect(),
    };
    let expected = positions(&snap.order);
    let mut found_sorted = found.clone();
    found_sorted.sort();
    assert_eq!(
        found_sorted, expected,
        "positions that disagree with the order"
    );
    snap.order
}

// The writes of a transaction.

/// What a transaction does to the store.
#[derive(Clone, Debug)]
enum Write {
    Store(Oid, Tid, Vec<u8>),
    ReadCurrent(Oid, Tid),
}

/// The writes of a creation of `ids` in `site` from `snap`, as
/// plone.folder makes them, with no split.
fn create(site: Site, snap: &Snap, ids: &[String]) -> Vec<Write> {
    let mut order = snap.order.clone();
    let mut new_items = Vec::new();
    for id in ids {
        new_items.push((id.clone(), order.len() as i128));
        order.push(id.clone());
    }
    let mut writes = vec![
        Write::Store(
            site.folder,
            snap.folder,
            folder(site.annotations, snap.date + 1),
        ),
        Write::Store(site.order, snap.order_serial, list(&order)),
    ];
    match &snap.pos {
        Pos::Inline(items) => {
            let mut items = items.clone();
            items.extend(new_items);
            items.sort();
            writes.push(Write::Store(site.pos, snap.pos_serial, pos_inline(&items)));
        }
        Pos::Node(buckets) => {
            let mut buckets = buckets.clone();
            let mut touched = Vec::new();
            for item in new_items {
                let at = buckets
                    .iter()
                    .rposition(|b| b.items[0].0 <= item.0)
                    .unwrap_or(0);
                buckets[at].items.push(item);
                buckets[at].items.sort();
                if !touched.contains(&at) {
                    touched.push(at);
                }
            }
            for at in touched {
                let next = buckets.get(at + 1).map(|b| b.oid);
                let b = &buckets[at];
                writes.push(Write::Store(b.oid, b.serial, pos_bucket(&b.items, next)));
            }
            writes.push(Write::ReadCurrent(site.pos, snap.pos_serial));
        }
    }
    writes
}

/// The writes of a creation of `ids` that splits the last bucket of the
/// positions into it and the new bucket `new` at `at` items, or the bucket
/// inline in the root into two new buckets.
fn create_split(site: Site, snap: &Snap, ids: &[String], new: &[Oid], at: usize) -> Vec<Write> {
    let mut writes = create(site, snap, ids);
    writes.retain(|write| match write {
        Write::Store(oid, _, _) | Write::ReadCurrent(oid, _) => {
            *oid == site.folder || *oid == site.order
        }
    });
    let mut order = snap.order.clone();
    order.extend(ids.iter().cloned());
    let all = positions(&order);
    match &snap.pos {
        Pos::Inline(_) => {
            let (low, high) = all.split_at(at);
            writes.push(Write::Store(
                site.pos,
                snap.pos_serial,
                pos_node(
                    &[(None, new[0]), (Some(&high[0].0), new[1])],
                    "OIBucket",
                    new[0],
                ),
            ));
            writes.push(Write::Store(
                new[0],
                Tid::ZERO,
                pos_bucket(low, Some(new[1])),
            ));
            writes.push(Write::Store(new[1], Tid::ZERO, pos_bucket(high, None)));
        }
        Pos::Node(buckets) => {
            let last = buckets.last().unwrap();
            let mut items = last.items.clone();
            for (id, index) in &all {
                if ids.contains(id) {
                    items.push((id.clone(), *index));
                }
            }
            items.sort();
            let (low, high) = items.split_at(at);
            let mut children: Vec<(Option<&str>, Oid)> = buckets
                .iter()
                .enumerate()
                .map(|(index, b)| ((index > 0).then(|| b.items[0].0.as_str()), b.oid))
                .collect();
            children.push((Some(&high[0].0), new[0]));
            writes.push(Write::Store(
                site.pos,
                snap.pos_serial,
                pos_node(&children, "OIBucket", buckets[0].oid),
            ));
            writes.push(Write::Store(
                last.oid,
                last.serial,
                pos_bucket(low, Some(new[0])),
            ));
            writes.push(Write::Store(new[0], Tid::ZERO, pos_bucket(high, None)));
        }
    }
    writes
}

/// Begins a transaction of `writes`. With `server_writes`, its client
/// takes records that it did not store, as the native client and
/// `zoe.Storage`.
fn begin<'s>(store: &'s Store, writes: &[Write], server_writes: bool) -> Txn<&'s Store> {
    let mut txn = store.tpc_begin(b"user", b"create", b"").unwrap();
    txn.set_server_writes(server_writes);
    for write in writes {
        match write {
            Write::Store(oid, serial, data) => txn.store(*oid, *serial, data).unwrap(),
            Write::ReadCurrent(oid, serial) => txn.check_current_serial(*oid, *serial).unwrap(),
        }
    }
    txn
}

/// Commits `writes` with `server_writes`.
fn commit(store: &Store, writes: &[Write]) -> Tid {
    let mut txn = begin(store, writes, true);
    txn.vote().unwrap();
    txn.finish().unwrap()
}

/// Votes and commits `writes`; returns the oids that the vote resolved.
fn merge(store: &Store, writes: &[Write]) -> Vec<Oid> {
    let mut txn = begin(store, writes, true);
    txn.vote().unwrap();
    let resolved = txn.natively_resolved();
    txn.finish().unwrap();
    resolved
}

/// Votes `writes`, which must fail with a conflict on `oid`; returns the
/// entries of the vote.
fn conflict(store: &Store, writes: &[Write], oid: Oid) -> Vec<zoe_core::Resolution> {
    conflict_with(store, writes, oid, true)
}

fn conflict_with(
    store: &Store,
    writes: &[Write],
    oid: Oid,
    server_writes: bool,
) -> Vec<zoe_core::Resolution> {
    let mut txn = begin(store, writes, server_writes);
    match txn.vote() {
        Err(ZoeError::Conflict { oid: failed }) => {
            assert_eq!(failed, oid, "the oid of the conflict")
        }
        other => panic!("expected a conflict on {oid:?}, got {other:?}"),
    }
    txn.resolutions().to_vec()
}

/// Returns the entry of the order list of a vote.
fn list_entry(entries: &[zoe_core::Resolution], site: Site) -> Option<zoe_core::Resolution> {
    entries
        .iter()
        .find(|entry| entry.oid == site.order && entry.order == Some("list"))
        .cloned()
}

fn ids(range: std::ops::Range<usize>) -> Vec<String> {
    range.map(id).collect()
}

// The merges.

#[test]
fn appends_on_both_sides_merge_in_an_inline_positions_tree() {
    let store = open(ON);
    let site = site(&store, &ids(0..3), None);
    let old = snap(&store, site);
    // The other side commits two creations after the snapshot.
    commit(&store, &create(site, &old, &[id(3)]));
    let between = snap(&store, site);
    commit(&store, &create(site, &between, &[id(4)]));
    let resolved = merge(&store, &create(site, &old, &[id(5)]));
    assert!(resolved.contains(&site.order), "{resolved:?}");
    assert!(resolved.contains(&site.pos), "{resolved:?}");
    assert_eq!(assert_agrees(&store, site), ids(0..6));
}

#[test]
fn two_ids_of_the_transaction_follow_the_committed_ids() {
    let store = open(ON);
    let site = site(&store, &ids(0..3), None);
    let old = snap(&store, site);
    commit(&store, &create(site, &old, &[id(3)]));
    merge(&store, &create(site, &old, &[id(4), id(5)]));
    assert_eq!(assert_agrees(&store, site), ids(0..6));
}

#[test]
fn appends_into_one_bucket_merge_with_tier_1_and_renumber() {
    let store = open(ON);
    let site = site(&store, &ids(0..10), Some(4));
    let old = snap(&store, site);
    commit(&store, &create(site, &old, &[id(10)]));
    let resolved = merge(&store, &create(site, &old, &[id(11)]));
    let Pos::Node(buckets) = snap(&store, site).pos else {
        panic!("a node")
    };
    assert!(resolved.contains(&buckets.last().unwrap().oid));
    assert_eq!(assert_agrees(&store, site), ids(0..12));
}

#[test]
fn an_id_in_a_bucket_that_the_other_side_did_not_write_is_staged_again() {
    let store = open(ON);
    let site = site(&store, &ids(0..10), Some(4));
    let old = snap(&store, site);
    let Pos::Node(buckets) = &old.pos else {
        panic!("a node")
    };
    let first = buckets[0].oid;
    commit(&store, &create(site, &old, &[id(10)]));
    // An id that sorts into the first bucket, which the other side did
    // not write: that record of the transaction is not stale.
    let mut txn = begin(&store, &create(site, &old, &["a-first".to_owned()]), true);
    txn.vote().unwrap();
    assert!(txn.natively_resolved().contains(&first));
    let entry = txn
        .resolutions()
        .iter()
        .find(|entry| entry.oid == first)
        .cloned()
        .unwrap();
    assert_eq!(entry.order, Some("pos"));
    assert_eq!(entry.read, entry.committed);
    let lines = txn.resolution_log_lines(std::time::SystemTime::now(), None);
    assert!(
        lines.iter().any(|line| line.contains("\"order\":\"list\"")),
        "{lines:?}"
    );
    assert!(
        lines.iter().any(|line| line.contains("\"order\":\"pos\"")),
        "{lines:?}"
    );
    txn.finish().unwrap();
    let mut expected = ids(0..11);
    expected.push("a-first".to_owned());
    assert_eq!(assert_agrees(&store, site), expected);
}

#[test]
fn a_split_of_one_bucket_by_both_sides_puts_the_ids_in_the_bucket_of_the_other_side() {
    let store = open(ON);
    let site = site(&store, &ids(0..6), Some(3));
    let old = snap(&store, site);
    let theirs = [new_oid(&store)];
    commit(&store, &create_split(site, &old, &[id(6)], &theirs, 2));
    let ours = [new_oid(&store)];
    let resolved = merge(&store, &create_split(site, &old, &[id(7)], &ours, 2));
    assert!(resolved.contains(&theirs[0]), "{resolved:?}");
    assert_eq!(assert_agrees(&store, site), ids(0..8));
}

#[test]
fn a_split_of_one_bucket_by_both_sides_without_server_writes_conflicts() {
    let store = open(ON);
    let site = site(&store, &ids(0..6), Some(3));
    let old = snap(&store, site);
    commit(
        &store,
        &create_split(site, &old, &[id(6)], &[new_oid(&store)], 2),
    );
    let writes = create_split(site, &old, &[id(7)], &[new_oid(&store)], 2);
    let Pos::Node(buckets) = &old.pos else {
        panic!("a node")
    };
    let mut txn = begin(&store, &writes, false);
    assert!(txn.vote().is_err());
    drop(txn);
    let _ = buckets;
    assert_eq!(assert_agrees(&store, site), ids(0..7));
}

#[test]
fn a_split_of_the_inline_bucket_by_both_sides_merges() {
    let store = open(ON);
    let site = site(&store, &ids(0..4), None);
    let old = snap(&store, site);
    let theirs = [new_oid(&store), new_oid(&store)];
    commit(&store, &create_split(site, &old, &[id(4)], &theirs, 2));
    let ours = [new_oid(&store), new_oid(&store)];
    merge(&store, &create_split(site, &old, &[id(5)], &ours, 2));
    assert_eq!(assert_agrees(&store, site), ids(0..6));
}

#[test]
fn a_split_by_the_transaction_alone_renumbers_its_new_bucket() {
    let store = open(ON);
    let site = site(&store, &ids(0..6), Some(3));
    let old = snap(&store, site);
    // The other side inserts into the last bucket without a split.
    commit(&store, &create(site, &old, &[id(6)]));
    let ours = [new_oid(&store)];
    let resolved = merge(&store, &create_split(site, &old, &[id(7)], &ours, 2));
    assert!(resolved.contains(&ours[0]), "{resolved:?}");
    assert_eq!(assert_agrees(&store, site), ids(0..8));
}

#[test]
fn a_rename_on_create_in_an_inline_tree_merges() {
    let store = open(ON);
    let site = site(&store, &ids(0..3), None);
    let old = snap(&store, site);
    commit(&store, &create(site, &old, &[id(3)]));
    // The rename rebuilds the positions: in an inline tree, the root holds
    // the old items and the new id.
    merge(&store, &create(site, &old, &["renamed".to_owned()]));
    let mut expected = ids(0..4);
    expected.push("renamed".to_owned());
    assert_eq!(assert_agrees(&store, site), expected);
}

/// The writes of a rename on create in a folder whose positions are a
/// node: `pos.clear()` and the rebuild store a new root over new buckets,
/// and drop the old ones without a write.
fn rename_on_create(store: &Store, site: Site, snap: &Snap, new_id: &str) -> Vec<Write> {
    let mut writes = create(site, snap, &[new_id.to_owned()]);
    writes.retain(|write| matches!(write, Write::Store(oid, _, _) if *oid == site.folder || *oid == site.order));
    let mut order = snap.order.clone();
    order.push(new_id.to_owned());
    let all = positions(&order);
    let (low, high) = all.split_at(all.len() / 2);
    let x = [new_oid(store), new_oid(store)];
    writes.push(Write::Store(
        site.pos,
        snap.pos_serial,
        pos_node(&[(None, x[0]), (Some(&high[0].0), x[1])], "OIBucket", x[0]),
    ));
    writes.push(Write::Store(x[0], Tid::ZERO, pos_bucket(low, Some(x[1]))));
    writes.push(Write::Store(x[1], Tid::ZERO, pos_bucket(high, None)));
    writes
}

#[test]
fn a_rename_on_create_in_buckets_gives_no_answer() {
    let store = open(ON);
    let site = site(&store, &ids(0..6), Some(3));
    let old = snap(&store, site);
    commit(&store, &create(site, &old, &[id(6)]));
    let writes = rename_on_create(&store, site, &old, "renamed");
    let entries = conflict(&store, &writes, site.order);
    let entry = list_entry(&entries, site).expect("a line of the order");
    assert!(matches!(entry.outcome, Resolved::Undecided(_)), "{entry:?}");
    assert_eq!(assert_agrees(&store, site), ids(0..7));
}

#[test]
fn a_node_that_drops_a_child_that_the_transaction_did_not_store_gives_no_answer() {
    let store = open(ON);
    let site = site(&store, &ids(0..6), Some(3));
    let old = snap(&store, site);
    commit(&store, &create(site, &old, &[id(6)]));
    let Pos::Node(buckets) = &old.pos else {
        panic!("a node")
    };
    // The transaction inserts into the last bucket, which holds the
    // equation, and stores a root that drops the first bucket.
    let mut writes = create(site, &old, &[id(7)]);
    writes.retain(|write| !matches!(write, Write::ReadCurrent(..)));
    let last = buckets.last().unwrap().oid;
    writes.push(Write::Store(
        site.pos,
        old.pos_serial,
        pos_node(&[(None, last)], "OIBucket", last),
    ));
    let entries = conflict(&store, &writes, site.order);
    assert_eq!(
        list_entry(&entries, site).unwrap().outcome,
        Resolved::Undecided("a node of the positions that drops a child")
    );
    assert_eq!(assert_agrees(&store, site), ids(0..7));
}

#[test]
fn a_change_of_another_position_gives_no_answer() {
    let store = open(ON);
    let site = site(&store, &ids(0..6), Some(3));
    let old = snap(&store, site);
    commit(&store, &create(site, &old, &[id(6)]));
    let Pos::Node(buckets) = &old.pos else {
        panic!("a node")
    };
    let mut writes = create(site, &old, &[id(7)]);
    // The transaction also changes the position of an id of the first
    // bucket: the node check passes, the equation does not.
    let first = &buckets[0];
    let mut items = first.items.clone();
    items[0].1 = 99;
    writes.push(Write::Store(
        first.oid,
        first.serial,
        pos_bucket(&items, Some(buckets[1].oid)),
    ));
    let entries = conflict(&store, &writes, site.order);
    let entry = list_entry(&entries, site).expect("a line of the order");
    assert_eq!(
        entry.outcome,
        Resolved::Undecided("positions that changed by more than the appends")
    );
}

#[test]
fn both_sides_adding_one_id_refuse() {
    let store = open(ON);
    let site = site(&store, &ids(0..3), None);
    let old = snap(&store, site);
    commit(&store, &create(site, &old, &[id(3)]));
    let mut txn = begin(&store, &create(site, &old, &[id(3)]), true);
    let voted = txn.vote();
    assert!(voted.is_err());
    let entries = txn.resolutions().to_vec();
    // `_tree` refuses two inserts of one key in a site; here the positions
    // or the order refuse.
    assert!(
        entries
            .iter()
            .any(|entry| matches!(entry.outcome, Resolved::Refused(_))),
        "{entries:?}"
    );
    if let Some(entry) = list_entry(&entries, site) {
        assert_eq!(entry.outcome, Resolved::Refused(Reason::OrderSameId));
    }
}

#[test]
fn a_delete_on_the_committed_side_gives_no_answer() {
    let store = open(ON);
    let site = site(&store, &ids(0..3), None);
    let old = snap(&store, site);
    // The other side deletes the first id: notifyRemoved rebuilds pos.
    let order = ids(1..3);
    commit(
        &store,
        &[
            Write::Store(site.folder, old.folder, folder(site.annotations, 7)),
            Write::Store(site.order, old.order_serial, list(&order)),
            Write::Store(site.pos, old.pos_serial, pos_inline(&positions(&order))),
        ],
    );
    let entries = conflict(&store, &create(site, &old, &[id(3)]), site.order);
    let entry = list_entry(&entries, site).expect("a line of the order");
    assert_eq!(
        entry.outcome,
        Resolved::Undecided("an order that is not appends on both sides")
    );
}

#[test]
fn a_move_by_the_transaction_gives_no_answer() {
    let store = open(ON);
    let site = site(&store, &ids(0..3), None);
    let old = snap(&store, site);
    commit(&store, &create(site, &old, &[id(3)]));
    let mut order = old.order.clone();
    order.swap(0, 1);
    order.push(id(4));
    let writes = vec![
        Write::Store(site.folder, old.folder, folder(site.annotations, 9)),
        Write::Store(site.order, old.order_serial, list(&order)),
        Write::Store(site.pos, old.pos_serial, pos_inline(&positions(&order))),
    ];
    let entries = conflict(&store, &writes, site.order);
    assert!(list_entry(&entries, site).is_some());
}

#[test]
fn an_item_that_is_not_a_str_gives_no_answer() {
    let store = open(ON);
    let site = site(&store, &ids(0..3), None);
    let old = snap(&store, site);
    commit(&store, &create(site, &old, &[id(3)]));
    let mut writes = create(site, &old, &[id(4)]);
    let mut heap = Heap::with_instances();
    let mut values: Vec<Value> = old.order.iter().map(|id| heap.new_str(id)).collect();
    values.push(Value::Int(4));
    let bad = list_of("persistent.list", "PersistentList", &values, &mut heap);
    for write in &mut writes {
        if let Write::Store(oid, _, data) = write
            && *oid == site.order
        {
            *data = bad.clone();
        }
    }
    let entries = conflict(&store, &writes, site.order);
    assert_eq!(
        list_entry(&entries, site).unwrap().outcome,
        Resolved::Undecided("an item of the order that is not a str")
    );
}

#[test]
fn an_order_of_another_class_on_the_committed_side_gives_no_answer() {
    let store = open(ON);
    let site = site(&store, &ids(0..3), None);
    let old = snap(&store, site);
    let mut heap = Heap::with_instances();
    let values: Vec<Value> = ids(0..4).iter().map(|id| heap.new_str(id)).collect();
    let other = list_of("m", "OtherList", &values, &mut heap);
    let mut theirs = create(site, &old, &[id(3)]);
    for write in &mut theirs {
        if let Write::Store(oid, _, data) = write
            && *oid == site.order
        {
            *data = other.clone();
        }
    }
    commit(&store, &theirs);
    let entries = conflict(&store, &create(site, &old, &[id(4)]), site.order);
    assert_eq!(
        list_entry(&entries, site).unwrap().outcome,
        Resolved::Undecided("a record of the order of another class")
    );
}

#[test]
fn an_order_that_no_stored_record_names_gives_no_line() {
    let store = open(ON);
    let site = site(&store, &ids(0..3), None);
    let old = snap(&store, site);
    commit(&store, &create(site, &old, &[id(3)]));
    let mut writes = create(site, &old, &[id(4)]);
    writes.retain(|write| !matches!(write, Write::Store(oid, _, _) if *oid == site.folder));
    let entries = conflict(&store, &writes, site.order);
    assert!(list_entry(&entries, site).is_none(), "{entries:?}");
}

#[test]
fn annotations_of_the_transaction_that_name_another_order_give_no_answer() {
    let store = open(ON);
    let site = site(&store, &ids(0..3), None);
    let old = snap(&store, site);
    commit(&store, &create(site, &old, &[id(3)]));
    let (_, annotations_serial) = store.load(site.annotations).unwrap();
    let mut writes = create(site, &old, &[id(4)]);
    let other = new_oid(&store);
    writes.push(Write::Store(
        site.annotations,
        annotations_serial,
        annotations(other, site.pos),
    ));
    let entries = conflict(&store, &writes, site.order);
    assert_eq!(
        list_entry(&entries, site).unwrap().outcome,
        Resolved::Undecided("annotations of the transaction that name another order")
    );
}

#[test]
fn a_transaction_that_appended_nothing_gives_no_answer() {
    let store = open(ON);
    let site = site(&store, &ids(0..3), None);
    let old = snap(&store, site);
    commit(&store, &create(site, &old, &[id(3)]));
    let writes = vec![
        Write::Store(site.folder, old.folder, folder(site.annotations, 9)),
        Write::Store(site.order, old.order_serial, list(&old.order)),
    ];
    let entries = conflict(&store, &writes, site.order);
    assert_eq!(
        list_entry(&entries, site).unwrap().outcome,
        Resolved::Undecided("a transaction that appended nothing")
    );
}

#[test]
fn with_tier_1_of_the_buckets_off_the_merge_does_not_run() {
    let store = open(Settings {
        bucket_tier1: Some("off"),
        ..ON
    });
    let site = site(&store, &ids(0..3), None);
    let old = snap(&store, site);
    commit(&store, &create(site, &old, &[id(3)]));
    let entries = conflict(&store, &create(site, &old, &[id(4)]), site.order);
    assert!(list_entry(&entries, site).is_none(), "{entries:?}");
}

// The switch.

#[test]
fn without_the_switch_the_order_conflicts() {
    for folder_orders in [None, Some("off")] {
        let store = open(Settings {
            folder_orders,
            ..ON
        });
        let site = site(&store, &ids(0..3), None);
        let old = snap(&store, site);
        commit(&store, &create(site, &old, &[id(3)]));
        let entries = conflict(&store, &create(site, &old, &[id(4)]), site.order);
        assert!(list_entry(&entries, site).is_none(), "{entries:?}");
    }
}

#[test]
fn in_shadow_mode_the_vote_conflicts_and_the_line_tells_the_outcome() {
    let store = open(Settings {
        folder_orders: Some("shadow"),
        ..ON
    });
    let site = site(&store, &ids(0..3), None);
    let old = snap(&store, site);
    commit(&store, &create(site, &old, &[id(3)]));
    let entries = conflict(&store, &create(site, &old, &[id(4)]), site.order);
    let entry = list_entry(&entries, site).unwrap();
    assert_eq!(entry.mode, Mode::Shadow);
    assert_eq!(
        entry.outcome,
        Resolved::Undecided("an order in shadow mode")
    );
    assert_eq!(entry.shadow, Some(Resolved::Merged));
    assert!(!entry.applied);
    assert_eq!(assert_agrees(&store, site), ids(0..4));
}

// Tier 2 and an order.

/// The writes of a creation of `new` whose transaction also changes the
/// position of the first id: the order is a plain append, but the
/// positions break the equation, so the order merge gives no answer.
fn create_with_another_change(site: Site, snap: &Snap, new: &str) -> Vec<Write> {
    let mut order = snap.order.clone();
    order.push(new.to_owned());
    let mut items = positions(&order);
    items[0].1 = 99;
    vec![
        Write::Store(
            site.folder,
            snap.folder,
            folder(site.annotations, snap.date + 1),
        ),
        Write::Store(site.order, snap.order_serial, list(&order)),
        Write::Store(site.pos, snap.pos_serial, pos_inline(&items)),
    ]
}

#[test]
fn tier_2_never_merges_an_order() {
    let cases = [
        Settings {
            list_tier2: Some("on"),
            ..ON
        },
        Settings {
            folder_orders: Some("shadow"),
            list_tier2: Some("on"),
            ..ON
        },
        // With tier 1 of the buckets off the merge does not run, and the
        // vote still knows the order.
        Settings {
            bucket_tier1: Some("off"),
            list_tier2: Some("on"),
            ..ON
        },
    ];
    for settings in cases {
        let store = open(settings);
        let site = site(&store, &ids(0..3), None);
        let old = snap(&store, site);
        commit(&store, &create(site, &old, &[id(3)]));
        // Tier 2 would append to the order without the positions.
        let mut txn = begin(
            &store,
            &create_with_another_change(site, &old, &id(4)),
            true,
        );
        assert!(txn.vote().is_err(), "the vote passed");
        let entries = txn.resolutions().to_vec();
        drop(txn);
        assert!(
            entries
                .iter()
                .all(|entry| !(entry.oid == site.order && entry.tier == 2)),
            "{entries:?}"
        );
        assert_eq!(assert_agrees(&store, site), ids(0..4));
    }
}

#[test]
fn a_list_that_is_not_an_order_keeps_its_entry_of_tier_2() {
    let store = open(Settings {
        list_tier2: Some("on"),
        ..ON
    });
    let oid = new_oid(&store);
    let mut txn = store.tpc_begin(b"user", b"list", b"").unwrap();
    txn.store(oid, Tid::ZERO, &list(&ids(0..2))).unwrap();
    txn.vote().unwrap();
    let read = txn.finish().unwrap();
    let mut txn = store.tpc_begin(b"user", b"a", b"").unwrap();
    txn.store(oid, read, &list(&ids(0..3))).unwrap();
    txn.vote().unwrap();
    txn.finish().unwrap();
    let mut txn = store.tpc_begin(b"user", b"b", b"").unwrap();
    let mut appended = ids(0..2);
    appended.push(id(9));
    txn.store(oid, read, &list(&appended)).unwrap();
    txn.vote().unwrap();
    assert!(txn.natively_resolved().contains(&oid));
    txn.finish().unwrap();
}

// Concurrency, undo, depth.

#[test]
fn two_votes_at_once_on_one_folder_both_commit() {
    let store = open(ON);
    let site = site(&store, &ids(0..3), None);
    let old = snap(&store, site);
    commit(&store, &create(site, &old, &[id(3)]));
    let first = create(site, &old, &[id(4)]);
    let second = create(site, &old, &[id(5)]);
    std::thread::scope(|scope| {
        let a = scope.spawn(|| merge(&store, &first));
        let b = scope.spawn(|| merge(&store, &second));
        a.join().unwrap();
        b.join().unwrap();
    });
    let order = assert_agrees(&store, site);
    assert_eq!(order.len(), 6);
    assert_eq!(order[..4], ids(0..4)[..]);
}

#[test]
fn an_undo_of_a_merged_creation_gives_back_the_order_and_the_positions_before_it() {
    let store = open(ON);
    let site = site(&store, &ids(0..3), None);
    let old = snap(&store, site);
    commit(&store, &create(site, &old, &[id(3)]));
    let mut txn = begin(&store, &create(site, &old, &[id(4)]), true);
    txn.vote().unwrap();
    let merged = txn.finish().unwrap();
    assert_eq!(assert_agrees(&store, site), ids(0..5));
    let mut undo = store.tpc_begin(b"user", b"undo", b"").unwrap();
    let undone = undo.undo(merged).unwrap();
    assert!(undone.conflicts.is_empty(), "{:?}", undone.conflicts);
    undo.vote().unwrap();
    undo.finish().unwrap();
    assert_eq!(assert_agrees(&store, site), ids(0..4));
}

#[test]
fn a_positions_tree_of_three_levels_merges() {
    let store = open(ON);
    let order = ids(0..8);
    let site = Site {
        folder: new_oid(&store),
        annotations: new_oid(&store),
        order: new_oid(&store),
        pos: new_oid(&store),
    };
    let items = positions(&order);
    let n = [new_oid(&store), new_oid(&store)];
    let b: Vec<Oid> = (0..4).map(|_| new_oid(&store)).collect();
    let mut txn = store.tpc_begin(b"user", b"site", b"").unwrap();
    txn.store(site.folder, Tid::ZERO, &folder(site.annotations, 0))
        .unwrap();
    txn.store(
        site.annotations,
        Tid::ZERO,
        &annotations(site.order, site.pos),
    )
    .unwrap();
    txn.store(site.order, Tid::ZERO, &list(&order)).unwrap();
    txn.store(
        site.pos,
        Tid::ZERO,
        &pos_node(&[(None, n[0]), (Some(&items[4].0), n[1])], "OIBTree", b[0]),
    )
    .unwrap();
    txn.store(
        n[0],
        Tid::ZERO,
        &pos_node(&[(None, b[0]), (Some(&items[2].0), b[1])], "OIBucket", b[0]),
    )
    .unwrap();
    txn.store(
        n[1],
        Tid::ZERO,
        &pos_node(&[(None, b[2]), (Some(&items[6].0), b[3])], "OIBucket", b[2]),
    )
    .unwrap();
    for (index, chunk) in items.chunks(2).enumerate() {
        txn.store(
            b[index],
            Tid::ZERO,
            &pos_bucket(chunk, b.get(index + 1).copied()),
        )
        .unwrap();
    }
    txn.vote().unwrap();
    txn.finish().unwrap();
    let old = snap(&store, site);
    let (_, n1) = store.load(n[1]).unwrap();
    let theirs = with_node(create(site, &old, &[id(8)]), n[1], n1);
    commit(&store, &theirs);
    let ours = with_node(create(site, &old, &[id(9)]), n[1], n1);
    merge(&store, &ours);
    assert_eq!(assert_agrees(&store, site), ids(0..10));
}

/// Adds the read-current mark of the child node `node`, as BTrees marks
/// each node of the path of a write.
fn with_node(mut writes: Vec<Write>, node: Oid, serial: Tid) -> Vec<Write> {
    writes.push(Write::ReadCurrent(node, serial));
    writes
}

#[test]
fn a_resolving_vote_merges_the_order() {
    let store = open(ON);
    let site = site(&store, &ids(0..3), None);
    let old = snap(&store, site);
    commit(&store, &create(site, &old, &[id(3)]));
    let mut txn = begin(&store, &create(site, &old, &[id(4)]), true);
    // The merge leaves no conflict to the caller, so the resolving vote is
    // the vote.
    let conflicts = txn.vote_resolving().unwrap();
    assert!(
        conflicts.is_empty(),
        "{:?}",
        conflicts.iter().map(|(oid, _)| *oid).collect::<Vec<_>>()
    );
    assert!(txn.natively_resolved().contains(&site.order));
    txn.finish().unwrap();
    assert_eq!(assert_agrees(&store, site), ids(0..5));
}

#[test]
fn rounds_from_one_snapshot_past_splits_keep_the_order_and_the_positions() {
    let store = open(ON);
    let site = site(&store, &ids(0..1), None);
    let mut next = 1;
    for _round in 0..6 {
        let old = snap(&store, site);
        for _turn in 0..4 {
            let writes = create_auto(&store, site, &old, &[id(next)]);
            next += 1;
            merge(&store, &writes);
        }
        let order = assert_agrees(&store, site);
        assert_eq!(order.len(), next);
    }
}

/// The writes of a creation that splits a bucket of the positions when it
/// passes 6 items, as BTrees splits at its maximum.
fn create_auto(store: &Store, site: Site, snap: &Snap, ids: &[String]) -> Vec<Write> {
    const MAX: usize = 6;
    match &snap.pos {
        Pos::Inline(items) if items.len() + ids.len() > MAX => {
            let new = [new_oid(store), new_oid(store)];
            create_split(site, snap, ids, &new, (items.len() + ids.len()) / 2)
        }
        Pos::Node(buckets) if buckets.last().unwrap().items.len() + ids.len() > MAX => {
            let len = buckets.last().unwrap().items.len() + ids.len();
            create_split(site, snap, ids, &[new_oid(store)], len / 2)
        }
        _ => create(site, snap, ids),
    }
}

#[test]
fn the_merged_records_name_no_other_change() {
    // The merged order is the committed order with the ids of the
    // transaction, and the merged positions differ from the committed
    // ones only by those ids.
    let store = open(ON);
    let site = site(&store, &ids(0..3), None);
    let old = snap(&store, site);
    commit(&store, &create(site, &old, &[id(3)]));
    let committed = snap(&store, site);
    merge(&store, &create(site, &old, &[id(4)]));
    let after = snap(&store, site);
    assert_eq!(after.order[..4], committed.order[..]);
    let (Pos::Inline(before), Pos::Inline(now)) = (committed.pos, after.pos) else {
        panic!("inline positions")
    };
    let before: BTreeMap<_, _> = before.into_iter().collect();
    let now: BTreeMap<_, _> = now.into_iter().collect();
    for (id, index) in &before {
        assert_eq!(now.get(id), Some(index));
    }
    assert_eq!(now.get(&id(4)), Some(&4));
}

// More changes that give no answer.

/// The writes of a transaction that writes `order` and its positions, as
/// a sort, a rename or a delete of plone.folder does.
fn rewrite(site: Site, snap: &Snap, order: &[String]) -> Vec<Write> {
    vec![
        Write::Store(
            site.folder,
            snap.folder,
            folder(site.annotations, snap.date + 1),
        ),
        Write::Store(site.order, snap.order_serial, list(order)),
        Write::Store(site.pos, snap.pos_serial, pos_inline(&positions(order))),
    ]
}

#[test]
fn a_sort_on_either_side_gives_no_answer() {
    for sort_first in [true, false] {
        let store = open(ON);
        let site = site(&store, &ids(0..3), None);
        let old = snap(&store, site);
        let mut sorted = old.order.clone();
        sorted.reverse();
        let (theirs, ours) = if sort_first {
            (rewrite(site, &old, &sorted), create(site, &old, &[id(3)]))
        } else {
            (create(site, &old, &[id(3)]), rewrite(site, &old, &sorted))
        };
        commit(&store, &theirs);
        let entries = conflict(&store, &ours, site.order);
        let entry = list_entry(&entries, site).expect("a line of the order");
        assert_eq!(
            entry.outcome,
            Resolved::Undecided("an order that is not appends on both sides")
        );
    }
}

#[test]
fn a_rename_of_an_existing_item_gives_no_answer() {
    let store = open(ON);
    let site = site(&store, &ids(0..3), None);
    let old = snap(&store, site);
    commit(&store, &create(site, &old, &[id(3)]));
    let mut renamed = old.order.clone();
    renamed[1] = "renamed".to_owned();
    let entries = conflict(&store, &rewrite(site, &old, &renamed), site.order);
    assert_eq!(
        list_entry(&entries, site).unwrap().outcome,
        Resolved::Undecided("an order that is not appends on both sides")
    );
}

#[test]
fn a_delete_by_the_transaction_gives_no_answer() {
    let store = open(ON);
    let site = site(&store, &ids(0..3), None);
    let old = snap(&store, site);
    commit(&store, &create(site, &old, &[id(3)]));
    let entries = conflict(&store, &rewrite(site, &old, &ids(1..3)), site.order);
    assert_eq!(
        list_entry(&entries, site).unwrap().outcome,
        Resolved::Undecided("an order that is not appends on both sides")
    );
}

/// Returns the record of an order of `ids` of the class `m.OtherList`.
fn other_list(ids: &[String]) -> Vec<u8> {
    let mut heap = Heap::with_instances();
    let values: Vec<Value> = ids.iter().map(|id| heap.new_str(id)).collect();
    list_of("m", "OtherList", &values, &mut heap)
}

#[test]
fn an_order_of_another_class_as_the_old_record_gives_no_answer() {
    let store = open(ON);
    let site = site(&store, &ids(0..3), None);
    // The record that both sides read is of another class.
    let (_, serial) = store.load(site.order).unwrap();
    let mut txn = begin(
        &store,
        &[Write::Store(site.order, serial, other_list(&ids(0..3)))],
        true,
    );
    txn.vote().unwrap();
    txn.finish().unwrap();
    let old = snap(&store, site);
    commit(&store, &create(site, &old, &[id(3)]));
    let entries = conflict(&store, &create(site, &old, &[id(4)]), site.order);
    assert_eq!(
        list_entry(&entries, site).unwrap().outcome,
        Resolved::Undecided("a record of the order of another class")
    );
}

#[test]
fn an_order_of_another_class_as_the_new_record_is_not_an_order() {
    let store = open(ON);
    let site = site(&store, &ids(0..3), None);
    let old = snap(&store, site);
    commit(&store, &create(site, &old, &[id(3)]));
    let mut writes = create(site, &old, &[id(4)]);
    let mut order = old.order.clone();
    order.push(id(4));
    for write in &mut writes {
        if let Write::Store(oid, _, data) = write
            && *oid == site.order
        {
            *data = other_list(&order);
        }
    }
    let entries = conflict(&store, &writes, site.order);
    assert!(list_entry(&entries, site).is_none(), "{entries:?}");
}

#[test]
fn a_rename_on_create_on_the_committed_side_in_buckets_gives_no_answer() {
    let store = open(ON);
    let site = site(&store, &ids(0..6), Some(3));
    let old = snap(&store, site);
    commit(&store, &rename_on_create(&store, site, &old, "renamed"));
    // The rebuild gave the root new children: rule 1 of the split merge
    // gives no answer, and the vote fails on the root that the transaction
    // marked read-current.
    let mut txn = begin(&store, &create(site, &old, &[id(6)]), true);
    match txn.vote() {
        Err(ZoeError::ReadConflict { oid }) => assert_eq!(oid, site.pos),
        other => panic!("expected a read conflict on the positions, got {other:?}"),
    }
    drop(txn);
    let mut expected = ids(0..6);
    expected.push("renamed".to_owned());
    assert_eq!(assert_agrees(&store, site), expected);
}

// A commit in flight.

/// A clock of the system that counts the waits on the waiters that it
/// gives, so a test tells when a vote waits.
#[derive(Clone, Default)]
struct Counting {
    waits: Arc<AtomicUsize>,
}

impl Clock for Counting {
    fn now(&self) -> SystemTime {
        SystemClock.now()
    }

    fn waiter(&self) -> Arc<dyn Waiter> {
        Arc::new(Counted {
            inner: SystemClock.waiter(),
            waits: Arc::clone(&self.waits),
        })
    }
}

struct Counted {
    inner: Arc<dyn Waiter>,
    waits: Arc<AtomicUsize>,
}

impl Waiter for Counted {
    fn wait(&self, timeout: Duration) {
        self.waits.fetch_add(1, Ordering::SeqCst);
        self.inner.wait(timeout);
    }

    fn wake(&self) {
        self.inner.wake();
    }
}

/// An observer that stops the finish after the sync of its batch, while
/// the gate is armed, until the test opens it. The commit of that finish
/// stays in flight.
#[derive(Default)]
struct Gate {
    armed: AtomicBool,
    held: AtomicBool,
    open: AtomicBool,
}

impl Observer for Gate {
    fn batch_synced(&self, _: usize) {
        if self.armed.load(Ordering::SeqCst) {
            self.held.store(true, Ordering::SeqCst);
            while !self.open.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(1));
            }
        }
    }
}

fn wait_until(done: impl Fn() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(start.elapsed() < Duration::from_secs(10), "timed out");
        thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn a_record_of_the_positions_merged_onto_a_commit_in_flight_gives_no_answer() {
    let clock = Counting::default();
    // Without the split merge, whose plan waits for a commit in flight
    // before it reads the store, tier 1 merges onto the commit in flight.
    let store = open_on(
        clock.clone(),
        Settings {
            split_merges: false,
            ..ON
        },
    );
    let gate = Arc::new(Gate::default());
    assert!(store.set_observer(gate.clone()));
    let site = site(&store, &ids(0..3), None);
    let old = snap(&store, site);
    // The other side appends, so the order of the creation is stale.
    commit(&store, &create(site, &old, &[id(3)]));
    // A transaction writes the positions without the order, from the
    // current snapshot, and holds their lock after its vote.
    let now = snap(&store, site);
    let Pos::Inline(items) = &now.pos else {
        panic!("inline positions")
    };
    let mut items = items.clone();
    items.push(("zz".to_owned(), 9));
    let mut holder = begin(
        &store,
        &[Write::Store(site.pos, now.pos_serial, pos_inline(&items))],
        true,
    );
    holder.vote().unwrap();
    let writes = create(site, &old, &[id(4)]);
    let waits = clock.waits.load(Ordering::SeqCst);
    thread::scope(|scope| {
        let store = &store;
        let creation = scope.spawn(move || {
            let mut txn = begin(store, &writes, true);
            let voted = txn.vote();
            (voted, txn.resolutions().to_vec())
        });
        // The creation waits for the lock of the positions, so the finish
        // of the holder hands it off, and stops with its commit in flight.
        wait_until(|| clock.waits.load(Ordering::SeqCst) > waits);
        gate.armed.store(true, Ordering::SeqCst);
        let finish = scope.spawn(move || holder.finish());
        // Open the gate when the creation voted, or when it waits again,
        // so that a wrong path does not hang the test.
        let waited = clock.waits.load(Ordering::SeqCst);
        wait_until(|| {
            creation.is_finished()
                || (gate.held.load(Ordering::SeqCst)
                    && clock.waits.load(Ordering::SeqCst) > waited + 1)
        });
        gate.open.store(true, Ordering::SeqCst);
        finish.join().unwrap().unwrap();
        let (voted, entries) = creation.join().unwrap();
        match voted {
            Err(ZoeError::Conflict { oid }) => assert_eq!(oid, site.order),
            other => panic!("expected a conflict on the order, got {other:?}"),
        }
        assert_eq!(
            list_entry(&entries, site).unwrap().outcome,
            Resolved::Undecided("a record of the positions merged onto a commit in flight")
        );
    });
    assert!(store.lock_table_is_empty());
}
