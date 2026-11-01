//! Rule 1 of the split merge (ADR 0048) and the read conflicts of the vote.
//!
//! - The other side replaced a child of a node by a new child with the same
//!   separator and the same separator after it, as a merge of rule 3 does.
//!   When that child is not a path child of the transaction, rule 1 lets
//!   the read-current node through, in mode `on`. Mode `shadow` tells that
//!   it would pass. Without the switch, the vote keeps the stock behaviour.
//! - A read-current node that fails the vote in mode `on` gives a line of
//!   the resolution log: the oid, the class, the serials and the reason
//!   that rule 1 gave.
//!
//! The records are those of ZODB, built with the pickler of
//! `zoe_pickle::python`: a node is `((child, key, child, ...), first)`, a
//! bucket `(items,)` or `(items, next)`, and a reference `(oid, class)`.

use std::time::SystemTime;

use zoe_core::testing::{commit_txn, new_oid, record, root};
use zoe_core::vfs::MemVfs;
use zoe_core::{Mode, Oid, Options, Resolution, Resolved, Store, SystemClock, Tid, Txn, ZoeError};
use zoe_pickle::btrees::{BucketState, TreeState, bucket_state, new_bucket_state, tree_state};
use zoe_pickle::python::{Heap, Value};

const MODULE: &str = "BTrees.IIBTree";
const BUCKET: &str = "IIBucket";
const TREE: &str = "IIBTree";
const TREE_CLASS: &str = "BTrees.IIBTree.IIBTree";

/// Returns a reference to `oid` of the class `name`.
fn reference(heap: &mut Heap, oid: Oid, name: &str) -> Value {
    let bytes = heap.new_bytes(&oid.0.to_be_bytes());
    let class = heap.new_class(MODULE, name);
    let data = heap.new_tuple(vec![bytes, class]);
    heap.new_reference(data).unwrap()
}

/// Returns the record of an `IIBucket` of `keys`, each with the value 1,
/// with its next bucket.
fn bucket(keys: &[i128], next: Option<Oid>) -> Vec<u8> {
    let mut heap = Heap::with_instances();
    let class = heap.new_class(MODULE, BUCKET);
    let items = keys
        .iter()
        .flat_map(|&key| [Value::Int(key), Value::Int(1)])
        .collect();
    let next = next.map(|oid| reference(&mut heap, oid, BUCKET));
    let state = new_bucket_state(&mut heap, items, next);
    record(&heap, class, state)
}

/// Returns the record of an `IIBTree` node whose children are buckets: its
/// first child, then each separator and its child.
fn node(first: Oid, children: &[(i128, Oid)]) -> Vec<u8> {
    let mut heap = Heap::with_instances();
    let class = heap.new_class(MODULE, TREE);
    let mut items = vec![reference(&mut heap, first, BUCKET)];
    for &(separator, oid) in children {
        items.push(Value::Int(separator));
        items.push(reference(&mut heap, oid, BUCKET));
    }
    let children = heap.new_tuple(items);
    let first = reference(&mut heap, first, BUCKET);
    let state = heap.new_tuple(vec![children, first]);
    record(&heap, class, state)
}

/// Returns the record of a policy that sets `split_merges` to `mode`, when
/// it is given.
fn policy(mode: Option<&str>) -> Vec<u8> {
    let mut heap = Heap::new();
    let class = heap.new_class("zoe.policy", "MergePolicy");
    let classes = heap.new_dict(Vec::new());
    let mut items = vec![
        (heap.new_str("version"), Value::Int(1)),
        (heap.new_str("classes"), classes),
    ];
    if let Some(mode) = mode {
        items.push((heap.new_str("split_merges"), heap.new_str(mode)));
    }
    let state = heap.new_dict(items);
    record(&heap, class, state)
}

/// Opens a store with the resolution log on. With `mode`, its policy sets
/// `split_merges` to it.
fn open(mode: Option<&str>) -> Store {
    let options = Options {
        structural_merges: zoe_core::StructuralMerges {
            resolution_log: true,
            ..Default::default()
        },
        ..Options::default()
    };
    let store = Store::open_full(MemVfs::new(), SystemClock, options).unwrap();
    let oid = new_oid(&store);
    commit_txn(writing(&store, &[(oid, Tid::ZERO, policy(mode))]));
    commit_txn(writing(&store, &[(Oid(0), Tid::ZERO, root(oid))]));
    store
}

/// Begins a transaction that stores `writes`, each with the serial that
/// it read.
fn writing<'s>(store: &'s Store, writes: &[(Oid, Tid, Vec<u8>)]) -> Txn<&'s Store> {
    let mut txn = store.tpc_begin(b"user", b"split", b"").unwrap();
    for (oid, serial, data) in writes {
        txn.store(*oid, *serial, data).unwrap();
    }
    txn
}

fn oid_of(heap: &Heap, value: Value) -> Oid {
    let oid = &heap.reference(value).unwrap().oid;
    Oid(u64::from_be_bytes(oid[..].try_into().unwrap()))
}

/// Returns the buckets of the committed tree of the node `root` in order,
/// and its keys. Checks that each key of a child is at or above its
/// separator and below the next one, that the keys ascend, and that each
/// bucket names the next one.
fn walk(store: &Store, root: Oid) -> (Vec<Oid>, Vec<i128>) {
    let load = |oid: Oid| {
        let (data, _) = store.load(oid).unwrap();
        let mut heap = Heap::with_instances();
        let tree = zoe_pickle::decode_record(&data).unwrap();
        let (_, state) = heap.load(&tree).unwrap();
        (heap, state)
    };
    let (heap, state) = load(root);
    assert!(matches!(
        tree_state(&heap, state).unwrap(),
        TreeState::Split
    ));
    let &[children, _] = heap.tuple(state).unwrap() else {
        panic!("a node of another shape");
    };
    let children = heap.tuple(children).unwrap();
    let mut buckets = Vec::new();
    let mut nexts = Vec::new();
    let mut keys = Vec::new();
    for (index, &child) in children.iter().enumerate().step_by(2) {
        let low = (index > 0).then(|| {
            let Value::Int(low) = children[index - 1] else {
                panic!("a separator that is not an int");
            };
            low
        });
        let high = children.get(index + 1).map(|&separator| {
            let Value::Int(high) = separator else {
                panic!("a separator that is not an int");
            };
            high
        });
        let oid = oid_of(&heap, child);
        let (bucket_heap, bucket) = load(oid);
        let BucketState { items, next } = bucket_state(&bucket_heap, bucket, true).unwrap();
        buckets.push(oid);
        nexts.push(next.map(|next| oid_of(&bucket_heap, next)));
        for pair in items.chunks(2) {
            let Value::Int(key) = pair[0] else {
                panic!("a key that is not an int");
            };
            assert!(low.is_none_or(|low| key >= low), "{key} below {low:?}");
            assert!(
                high.is_none_or(|high| key < high),
                "{key} at or above {high:?}"
            );
            keys.push(key);
        }
    }
    assert!(keys.windows(2).all(|pair| pair[0] < pair[1]), "{keys:?}");
    let mut expected: Vec<Option<Oid>> = buckets.iter().skip(1).map(|&oid| Some(oid)).collect();
    expected.push(None);
    assert_eq!(nexts, expected, "the chain of next buckets");
    (buckets, keys)
}

/// The tree of the tests, as the transaction read it: a node of three
/// buckets, `a` [10, 20], `y` [30, 40] and `b` [50, 60], with the
/// separators 30 and 50.
struct Tree {
    root: Oid,
    a: Oid,
    y: Oid,
    b: Oid,
    /// The tid that the transaction read.
    tid: Tid,
}

fn three_buckets(store: &Store) -> Tree {
    let (root, a, y, b) = (
        new_oid(store),
        new_oid(store),
        new_oid(store),
        new_oid(store),
    );
    let tid = commit_txn(writing(
        store,
        &[
            (root, Tid::ZERO, node(a, &[(30, y), (50, b)])),
            (a, Tid::ZERO, bucket(&[10, 20], Some(y))),
            (y, Tid::ZERO, bucket(&[30, 40], Some(b))),
            (b, Tid::ZERO, bucket(&[50, 60], None)),
        ],
    ));
    Tree { root, a, y, b, tid }
}

/// The other side replaces `y` by a new bucket `x` with the same separator
/// and the same separator after it, as a merge of rule 3 writes it: the
/// node holds `x` in the place of `y`, `a` names `x` as its next bucket,
/// and `x` holds the keys of `y` and 35. `y` leaves the tree. Returns `x`
/// and the tid of the commit.
fn replace_y(store: &Store, tree: &Tree) -> (Oid, Tid) {
    let x = new_oid(store);
    let tid = commit_txn(writing(
        store,
        &[
            (tree.root, tree.tid, node(tree.a, &[(30, x), (50, tree.b)])),
            (tree.a, tree.tid, bucket(&[10, 20], Some(x))),
            (x, Tid::ZERO, bucket(&[30, 35, 40], Some(tree.b))),
        ],
    ));
    (x, tid)
}

/// The transaction inserts 55 into `b`, without a split, and marks the node
/// read-current, as BTrees does. `b` is its one path child.
fn insert_into_b<'s>(store: &'s Store, tree: &Tree) -> Txn<&'s Store> {
    let mut txn = writing(store, &[(tree.b, tree.tid, bucket(&[50, 55, 60], None))]);
    txn.check_current_serial(tree.root, tree.tid).unwrap();
    txn
}

/// Returns the one entry of the vote for `oid`.
fn entry_of(txn: &Txn<&Store>, oid: Oid) -> Resolution {
    let entries: Vec<&Resolution> = txn.resolutions().iter().filter(|r| r.oid == oid).collect();
    let [entry] = entries[..] else {
        panic!("one entry for {oid:?}: {:?}", txn.resolutions());
    };
    entry.clone()
}

/// Returns the one line of the resolution log for `oid`.
fn line_of(txn: &Txn<&Store>, oid: Oid) -> String {
    let needle = format!("\"oid\":\"0x{:016x}\"", oid.0);
    let lines: Vec<String> = txn
        .resolution_log_lines(SystemTime::now(), None)
        .into_iter()
        .filter(|line| line.contains(&needle))
        .collect();
    let [line] = &lines[..] else {
        panic!("one line for {oid:?}: {lines:?}");
    };
    line.clone()
}

#[test]
fn a_replaced_child_off_the_path_passes_rule_1_in_mode_on() {
    let store = open(Some("on"));
    let tree = three_buckets(&store);
    let (x, _) = replace_y(&store, &tree);
    let mut txn = insert_into_b(&store, &tree);
    assert_eq!(txn.vote_resolving().unwrap(), []);
    // The transaction did not store the node: nothing goes back to the
    // client as resolved.
    assert_eq!(txn.natively_resolved(), []);
    let entry = entry_of(&txn, tree.root);
    assert_eq!(
        (entry.split, entry.outcome, entry.applied),
        (Some(1), Resolved::Merged, true)
    );
    let line = line_of(&txn, tree.root);
    assert!(line.contains("\"outcome\":\"passed\""), "{line}");
    assert!(line.contains("\"split\":1"), "{line}");
    txn.finish().unwrap();
    let (buckets, keys) = walk(&store, tree.root);
    assert_eq!(buckets, [tree.a, x, tree.b]);
    assert_eq!(keys, [10, 20, 30, 35, 40, 50, 55, 60]);
}

#[test]
fn a_replaced_last_child_off_the_path_passes_rule_1_in_mode_on() {
    // The keys grow: the other side replaced the last bucket, and the
    // transaction wrote the first one.
    let store = open(Some("on"));
    let tree = three_buckets(&store);
    let x = new_oid(&store);
    commit_txn(writing(
        &store,
        &[
            (tree.root, tree.tid, node(tree.a, &[(30, tree.y), (50, x)])),
            (tree.y, tree.tid, bucket(&[30, 40], Some(x))),
            (x, Tid::ZERO, bucket(&[50, 60, 70], None)),
        ],
    ));
    let mut txn = writing(
        &store,
        &[(tree.a, tree.tid, bucket(&[10, 15, 20], Some(tree.y)))],
    );
    txn.check_current_serial(tree.root, tree.tid).unwrap();
    txn.vote().unwrap();
    let entry = entry_of(&txn, tree.root);
    assert_eq!((entry.split, entry.outcome), (Some(1), Resolved::Merged));
    txn.finish().unwrap();
    let (buckets, keys) = walk(&store, tree.root);
    assert_eq!(buckets, [tree.a, tree.y, x]);
    assert_eq!(keys, [10, 15, 20, 30, 40, 50, 60, 70]);
}

#[test]
fn a_replaced_child_off_the_path_in_shadow_mode_tells_that_it_would_pass() {
    let store = open(Some("shadow"));
    let tree = three_buckets(&store);
    replace_y(&store, &tree);
    let mut txn = insert_into_b(&store, &tree);
    let voted = txn.vote();
    assert!(
        matches!(voted, Err(ZoeError::ReadConflict { oid }) if oid == tree.root),
        "{voted:?}"
    );
    let entry = entry_of(&txn, tree.root);
    assert_eq!(entry.mode, Mode::Shadow);
    assert_eq!(
        (entry.split, entry.shadow),
        (Some(1), Some(Resolved::Merged))
    );
    let line = line_of(&txn, tree.root);
    assert!(
        line.contains("\"shadow\":{\"outcome\":\"passed\""),
        "{line}"
    );
}

#[test]
fn a_replaced_child_off_the_path_without_the_switch_keeps_the_stock_behaviour() {
    for mode in [None, Some("off")] {
        let store = open(mode);
        let tree = three_buckets(&store);
        replace_y(&store, &tree);
        let mut txn = insert_into_b(&store, &tree);
        let voted = txn.vote();
        assert!(
            matches!(voted, Err(ZoeError::ReadConflict { oid }) if oid == tree.root),
            "{mode:?}: {voted:?}"
        );
        assert!(
            txn.resolutions().iter().all(|r| r.split.is_none()),
            "{mode:?}: {:?}",
            txn.resolutions()
        );
    }
}

/// Checks the line of the resolution log of a read-current node that
/// failed the vote in mode `on`: its oid, its class, its serials, the rule
/// and the reason of the no-answer of rule 1.
fn check_read_conflict_line(txn: &Txn<&Store>, oid: Oid, read: Tid, current: Tid) {
    let entry = entry_of(txn, oid);
    assert_eq!(
        (entry.split, &entry.class[..], entry.read, entry.committed),
        (Some(1), TREE_CLASS, read, current),
        "{entry:?}"
    );
    let Resolved::Undecided(why) = entry.outcome else {
        panic!("rule 1 gives no answer: {entry:?}");
    };
    assert!(!why.is_empty());
    let line = line_of(txn, oid);
    for field in [
        format!("\"class\":\"{TREE_CLASS}\""),
        format!("\"read\":\"{}\"", read.hex()),
        format!("\"committed\":\"{}\"", current.hex()),
        "\"split\":1".to_owned(),
        "\"outcome\":\"undecided\"".to_owned(),
        format!("\"reason\":\"{why}\""),
    ] {
        assert!(line.contains(&field), "{field} in {line}");
    }
}

#[test]
fn a_replaced_child_on_the_path_fails_and_the_vote_logs_why() {
    let store = open(Some("on"));
    let tree = three_buckets(&store);
    let (_, current) = replace_y(&store, &tree);
    // The transaction inserts 45 into `y`, which left the tree. Its write
    // to `y` is not stale. A pass of the node would lose the insert.
    let mut txn = writing(
        &store,
        &[(tree.y, tree.tid, bucket(&[30, 40, 45], Some(tree.b)))],
    );
    txn.check_current_serial(tree.root, tree.tid).unwrap();
    let voted = txn.vote();
    assert!(
        matches!(voted, Err(ZoeError::ReadConflict { oid }) if oid == tree.root),
        "{voted:?}"
    );
    check_read_conflict_line(&txn, tree.root, tree.tid, current);
    txn.abort();
    // No update is lost: the store keeps the tree of the other side.
    assert_eq!(walk(&store, tree.root).1, [10, 20, 30, 35, 40, 50, 60]);
}

#[test]
fn a_replaced_child_with_another_separator_fails_and_the_vote_logs_why() {
    let store = open(Some("on"));
    let tree = three_buckets(&store);
    let x = new_oid(&store);
    let current = commit_txn(writing(
        &store,
        &[
            (tree.root, tree.tid, node(tree.a, &[(25, x), (50, tree.b)])),
            (tree.a, tree.tid, bucket(&[10, 20], Some(x))),
            (x, Tid::ZERO, bucket(&[25, 30, 40], Some(tree.b))),
        ],
    ));
    let mut txn = insert_into_b(&store, &tree);
    let voted = txn.vote();
    assert!(
        matches!(voted, Err(ZoeError::ReadConflict { oid }) if oid == tree.root),
        "{voted:?}"
    );
    check_read_conflict_line(&txn, tree.root, tree.tid, current);
}

#[test]
fn a_read_current_node_without_a_path_child_fails_and_the_vote_logs_why() {
    let store = open(Some("on"));
    let tree = three_buckets(&store);
    let (_, current) = replace_y(&store, &tree);
    // The transaction writes an object outside the tree, and marks the node
    // read-current.
    let other = new_oid(&store);
    let mut txn = writing(&store, &[(other, Tid::ZERO, bucket(&[1], None))]);
    txn.check_current_serial(tree.root, tree.tid).unwrap();
    let voted = txn.vote();
    assert!(
        matches!(voted, Err(ZoeError::ReadConflict { oid }) if oid == tree.root),
        "{voted:?}"
    );
    check_read_conflict_line(&txn, tree.root, tree.tid, current);
}

#[test]
fn a_read_current_node_whose_path_child_split_fails_and_the_vote_logs_why() {
    let store = open(Some("on"));
    let tree = three_buckets(&store);
    // The other side splits `b`.
    let y2 = new_oid(&store);
    let current = commit_txn(writing(
        &store,
        &[
            (
                tree.root,
                tree.tid,
                node(tree.a, &[(30, tree.y), (50, tree.b), (60, y2)]),
            ),
            (tree.b, tree.tid, bucket(&[50], Some(y2))),
            (y2, Tid::ZERO, bucket(&[60, 70], None)),
        ],
    ));
    // The transaction marks the node and `b` read-current.
    let other = new_oid(&store);
    let mut txn = writing(&store, &[(other, Tid::ZERO, bucket(&[1], None))]);
    txn.check_current_serial(tree.root, tree.tid).unwrap();
    txn.check_current_serial(tree.b, tree.tid).unwrap();
    let voted = txn.vote();
    assert!(
        matches!(voted, Err(ZoeError::ReadConflict { oid }) if oid == tree.root),
        "{voted:?}"
    );
    check_read_conflict_line(&txn, tree.root, tree.tid, current);
}
