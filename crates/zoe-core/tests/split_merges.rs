//! The split merge of ADR 0048 in the vote: the three rules, the order of
//! key references, the cases without an answer, and the switch
//! `split_merges` of the merge policy in each mode.
//!
//! The records are those of ZODB, built with the pickler of
//! `zoe_pickle::python`: a node is `((child, key, child, ...), first)`, a
//! bucket `(items,)` or `(items, next)`, and a reference `(oid, class)`.
//! After each merge, a walk of the committed tree checks its separators,
//! the order of its keys and the chain of its next buckets. The model of
//! the spike on real BTrees is the Python test `test_zoe_split_merges.py`.

use std::collections::BTreeSet;
use std::time::SystemTime;

use zoe_core::vfs::MemVfs;
use zoe_core::{Mode, Oid, Options, Reason, Resolved, Store, SystemClock, Tid, Txn, ZoeError};
use zoe_pickle::btrees::{BucketState, TreeState, bucket_state, new_bucket_state, tree_state};
use zoe_pickle::python::{Heap, Instance, Made, Pickler, Value};

/// A family of BTrees of the tests: the module and the prefix of its
/// classes. With `references`, the value of each key is a key reference to
/// the object whose oid is the key; else it is 1.
#[derive(Clone, Copy)]
struct Family {
    module: &'static str,
    prefix: &'static str,
    references: bool,
}

/// Keys and values of C ints.
const II: Family = Family {
    module: "BTrees.IIBTree",
    prefix: "II",
    references: false,
};

/// Object keys and int values, as `ids` of the intid utility.
const OI: Family = Family {
    module: "BTrees.OIBTree",
    prefix: "OI",
    references: false,
};

/// Int keys and key references as values, as `refs` of the intid utility.
const IO: Family = Family {
    module: "BTrees.IOBTree",
    prefix: "IO",
    references: true,
};

impl Family {
    fn bucket(self) -> String {
        format!("{}Bucket", self.prefix)
    }

    fn tree(self) -> String {
        format!("{}BTree", self.prefix)
    }
}

/// A key of a test tree.
#[derive(Clone, Copy, Debug)]
enum Key {
    /// An int.
    Int(i128),
    /// A key reference of five.intid to the object of this oid.
    Ref(u64),
    /// A key reference of zope.keyreference to the object of this oid.
    Zope(u64),
    /// A key reference of five.intid to an object of another database.
    Remote(u64),
    /// An object of a class that the order does not know.
    Opaque(i128),
}

/// Returns the value of `key` in `heap`.
fn key(heap: &mut Heap, key: Key) -> Value {
    let key_reference = |heap: &mut Heap, module: &str, oid: u64, database: Option<&str>| {
        let target = heap.new_class("m", "Document");
        let bytes = heap.new_bytes(&oid.to_be_bytes());
        let data = match database {
            None => heap.new_tuple(vec![bytes, target]),
            Some(database) => {
                let kind = heap.new_str("m");
                let database = heap.new_str(database);
                let args = heap.new_tuple(vec![database, bytes, target]);
                heap.new_list(vec![kind, args])
            }
        };
        let object = heap.new_reference(data).unwrap();
        let name = heap.new_str("object");
        let state = heap.new_dict(vec![(name, object)]);
        let class = heap.new_class(module, "KeyReferenceToPersistent");
        let args = heap.new_tuple(Vec::new());
        heap.new_instance(Instance {
            made: Made::New,
            class,
            args,
            state: Some(state),
        })
    };
    match key {
        Key::Int(int) => Value::Int(int),
        Key::Ref(oid) => key_reference(heap, "five.intid.keyreference", oid, None),
        Key::Zope(oid) => key_reference(heap, "zope.keyreference.persistent", oid, None),
        Key::Remote(oid) => key_reference(heap, "five.intid.keyreference", oid, Some("other")),
        Key::Opaque(int) => {
            let class = heap.new_class("m", "Key");
            let args = heap.new_tuple(vec![Value::Int(int)]);
            heap.new_instance(Instance {
                made: Made::New,
                class,
                args,
                state: None,
            })
        }
    }
}

/// Returns the int of a key of a committed tree: an int, or the oid of the
/// object of a key reference.
fn key_int(heap: &Heap, key: Value) -> i128 {
    if let Value::Int(int) = key {
        return int;
    }
    let instance = heap.instance(key).expect("a key reference");
    let state = heap.dict(instance.state.unwrap()).unwrap();
    let object = state[0].1;
    let oid = &heap.reference(object).unwrap().oid;
    i128::from(u64::from_be_bytes(oid[..].try_into().unwrap()))
}

/// Returns a reference to `oid` of the class `name` of `family`.
fn reference(heap: &mut Heap, family: Family, oid: Oid, name: &str) -> Value {
    let bytes = heap.new_bytes(&oid.0.to_be_bytes());
    let class = heap.new_class(family.module, name);
    let data = heap.new_tuple(vec![bytes, class]);
    heap.new_reference(data).unwrap()
}

fn record(heap: &Heap, class: Value, state: Value) -> Vec<u8> {
    let mut pickler = Pickler::new(heap);
    pickler.dump(class).unwrap();
    pickler.dump(state).unwrap();
    pickler.into_bytes()
}

/// Returns the items of `keys`, each with its value in `family`.
fn items(heap: &mut Heap, family: Family, keys: &[Key]) -> Vec<Value> {
    let mut items = Vec::new();
    for &k in keys {
        items.push(key(heap, k));
        let value = match (family.references, k) {
            (true, Key::Int(int)) => key(heap, Key::Ref(int as u64)),
            _ => Value::Int(1),
        };
        items.push(value);
    }
    items
}

/// Returns the record of a bucket of `family` of `keys`, with its next
/// bucket.
fn bucket(family: Family, keys: &[Key], next: Option<Oid>) -> Vec<u8> {
    let mut heap = Heap::with_instances();
    let class = heap.new_class(family.module, &family.bucket());
    let items = items(&mut heap, family, keys);
    let next = next.map(|oid| reference(&mut heap, family, oid, &family.bucket()));
    let state = new_bucket_state(&mut heap, items, next);
    record(&heap, class, state)
}

/// Returns the record of a node of `family`: its first child, then each
/// separator and its child, of the class `child`, and its first bucket.
fn node(family: Family, first: Oid, children: &[(Key, Oid)], child: &str, bucket: Oid) -> Vec<u8> {
    let mut heap = Heap::with_instances();
    let class = heap.new_class(family.module, &family.tree());
    let mut items = vec![reference(&mut heap, family, first, child)];
    for &(separator, oid) in children {
        items.push(key(&mut heap, separator));
        items.push(reference(&mut heap, family, oid, child));
    }
    let children = heap.new_tuple(items);
    let first = reference(&mut heap, family, bucket, &family.bucket());
    let state = heap.new_tuple(vec![children, first]);
    record(&heap, class, state)
}

/// Returns the record of a node of `family` whose children are buckets.
fn leaf_node(family: Family, first: Oid, children: &[(Key, Oid)]) -> Vec<u8> {
    node(family, first, children, &family.bucket(), first)
}

/// Returns the record of a BTree of `family` that holds one bucket of
/// `keys` inline.
fn inline(family: Family, keys: &[Key]) -> Vec<u8> {
    let mut heap = Heap::with_instances();
    let class = heap.new_class(family.module, &family.tree());
    let items = items(&mut heap, family, keys);
    let bucket = new_bucket_state(&mut heap, items, None);
    let inner = heap.new_tuple(vec![bucket]);
    let state = heap.new_tuple(vec![inner]);
    record(&heap, class, state)
}

/// Returns `ints` as int keys.
fn ints(ints: &[i128]) -> Vec<Key> {
    ints.iter().map(|&int| Key::Int(int)).collect()
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

/// Returns the record of a policy that sets `split_merges` to `mode`, when
/// it is given, and the mode of tier 1 of each class of `tier1`.
fn policy(mode: Option<&str>, tier1: &[(&str, &str)]) -> Vec<u8> {
    let mut heap = Heap::new();
    let class = heap.new_class("zoe.policy", "MergePolicy");
    let mut entries = Vec::new();
    for (name, mode) in tier1 {
        let entry = vec![(heap.new_str("tier1"), heap.new_str(mode))];
        let entry = heap.new_dict(entry);
        entries.push((heap.new_str(name), entry));
    }
    let classes = heap.new_dict(entries);
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

/// Opens a store. With `mode`, its policy sets `split_merges` to it.
fn open(mode: Option<&str>) -> Store {
    open_with(mode, &[])
}

/// Opens a store whose policy sets `split_merges` to `mode` and the mode of
/// tier 1 of each class of `tier1`.
fn open_with(mode: Option<&str>, tier1: &[(&str, &str)]) -> Store {
    let options = Options {
        structural_merges: zoe_core::StructuralMerges {
            resolution_log: true,
            ..Default::default()
        },
        ..Options::default()
    };
    let store = Store::open_full(MemVfs::new(), SystemClock, options).unwrap();
    let oid = new_oid(&store);
    let tid = commit(&store, &[(oid, Tid::ZERO, policy(mode, tier1))]);
    commit(&store, &[(Oid(0), Tid::ZERO, root(oid))]);
    assert!(tid > Tid::ZERO);
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

/// Begins a transaction that stores `writes`, each with the serial that
/// it read.
fn writing<'s>(store: &'s Store, writes: &[(Oid, Tid, Vec<u8>)]) -> Txn<&'s Store> {
    let mut txn = store.tpc_begin(b"user", b"split", b"").unwrap();
    for (oid, serial, data) in writes {
        txn.store(*oid, *serial, data).unwrap();
    }
    txn
}

/// Commits `writes` in one transaction.
fn commit(store: &Store, writes: &[(Oid, Tid, Vec<u8>)]) -> Tid {
    let mut txn = writing(store, writes);
    txn.vote().unwrap();
    txn.finish().unwrap()
}

/// A tree of the tests: its root and its buckets in order, each with its
/// keys. The tree is committed at `tid`.
struct Tree {
    family: Family,
    root: Oid,
    buckets: Vec<Oid>,
    tid: Tid,
}

/// Commits a tree of `family` whose root holds the buckets of `keys`,
/// in one transaction.
fn tree(store: &Store, family: Family, keys: &[&[Key]]) -> Tree {
    let root = new_oid(store);
    let buckets: Vec<Oid> = keys.iter().map(|_| new_oid(store)).collect();
    let mut writes = Vec::new();
    let mut children = Vec::new();
    for (index, keys) in keys.iter().enumerate() {
        let next = buckets.get(index + 1).copied();
        writes.push((buckets[index], Tid::ZERO, bucket(family, keys, next)));
        if index > 0 {
            children.push((keys[0], buckets[index]));
        }
    }
    writes.insert(
        0,
        (root, Tid::ZERO, leaf_node(family, buckets[0], &children)),
    );
    let tid = commit(store, &writes);
    Tree {
        family,
        root,
        buckets,
        tid,
    }
}

/// Returns the buckets of the committed tree of `root` in order, with its
/// keys as ints. Checks that each key of a child is at or above its
/// separator and below the next one, that the keys ascend, and that each
/// bucket names the next one.
fn walk(store: &Store, root: Oid) -> (Vec<Oid>, Vec<i128>) {
    let mut buckets = Vec::new();
    let mut keys = Vec::new();
    let mut nexts = Vec::new();
    // Each entry: an oid, and the bounds of its keys.
    let mut stack = vec![(root, None::<i128>, None::<i128>)];
    while let Some((oid, low, high)) = stack.pop() {
        let (data, _) = store.load(oid).unwrap();
        let mut heap = Heap::with_instances();
        let tree = zoe_pickle::decode_record(&data).unwrap();
        let (class, state) = heap.load(&tree).unwrap();
        let (_, name) = heap.class(class).unwrap();
        let mut found = Vec::new();
        if name.ends_with("Bucket") {
            let BucketState { items, next } = bucket_state(&heap, state, true).unwrap();
            buckets.push(oid);
            nexts.push(next.map(|next| oid_of(&heap, next)));
            for pair in items.chunks(2) {
                found.push(key_int(&heap, pair[0]));
            }
        } else {
            match tree_state(&heap, state).unwrap() {
                TreeState::Inline(bucket) => {
                    let BucketState { items, .. } = bucket_state(&heap, bucket, true).unwrap();
                    for pair in items.chunks(2) {
                        found.push(key_int(&heap, pair[0]));
                    }
                }
                TreeState::Split => {
                    let &[children, _] = heap.tuple(state).unwrap() else {
                        panic!("a node of another shape");
                    };
                    let children = heap.tuple(children).unwrap();
                    let mut entries = Vec::new();
                    for (index, &child) in children.iter().enumerate().step_by(2) {
                        let below = (index > 0).then(|| key_int(&heap, children[index - 1]));
                        let above = children
                            .get(index + 1)
                            .map(|&separator| key_int(&heap, separator));
                        entries.push((oid_of(&heap, child), below.or(low), above.or(high)));
                    }
                    // The stack takes the children in reverse order, so the
                    // walk visits them in order.
                    stack.extend(entries.into_iter().rev());
                }
                TreeState::Empty => {}
            }
        }
        for &k in &found {
            assert!(
                low.is_none_or(|low| k >= low),
                "{k} below {low:?} in {oid:?}"
            );
            assert!(
                high.is_none_or(|high| k < high),
                "{k} at or above {high:?} in {oid:?}"
            );
        }
        keys.extend(found);
    }
    assert!(keys.windows(2).all(|pair| pair[0] < pair[1]), "{keys:?}");
    let expected: Vec<Option<Oid>> = buckets.iter().skip(1).map(|&oid| Some(oid)).collect();
    if !buckets.is_empty() {
        assert_eq!(
            nexts[..nexts.len() - 1],
            expected[..],
            "the chain of next buckets"
        );
        assert_eq!(
            nexts.last(),
            Some(&None),
            "the last bucket has no next bucket"
        );
    }
    (buckets, keys)
}

fn oid_of(heap: &Heap, value: Value) -> Oid {
    let oid = &heap.reference(value).unwrap().oid;
    Oid(u64::from_be_bytes(oid[..].try_into().unwrap()))
}

/// Returns the oids of `oids` as a set.
fn set(oids: &[Oid]) -> BTreeSet<Oid> {
    oids.iter().copied().collect()
}

/// The tree of most tests: two buckets, [10, 20, 30] and [40, 50, 60].
fn two_buckets(store: &Store) -> Tree {
    tree(store, II, &[&ints(&[10, 20, 30]), &ints(&[40, 50, 60])])
}

/// Splits the bucket `index` of `tree` in one transaction: the bucket keeps
/// `left`, and a new bucket gets `right`. Returns the new bucket.
fn split(store: &Store, tree: &Tree, index: usize, left: &[i128], right: &[i128]) -> Oid {
    let new = new_oid(store);
    let writes = split_writes(tree, index, new, left, right);
    commit(store, &writes);
    new
}

/// Returns the writes of a split of the bucket `index` of `tree` into
/// itself and `new`, read at the tid of the tree.
fn split_writes(
    tree: &Tree,
    index: usize,
    new: Oid,
    left: &[i128],
    right: &[i128],
) -> Vec<(Oid, Tid, Vec<u8>)> {
    let family = tree.family;
    let anchor = tree.buckets[index];
    let next = tree.buckets.get(index + 1).copied();
    let mut children: Vec<(Key, Oid)> = tree
        .buckets
        .iter()
        .enumerate()
        .skip(1)
        .map(|(position, &oid)| (Key::Int(SEPARATORS[position]), oid))
        .collect();
    // The new child goes right after its anchor.
    let at = children
        .iter()
        .position(|&(_, oid)| oid == anchor)
        .map_or(0, |position| position + 1);
    children.insert(at, (Key::Int(right[0]), new));
    vec![
        (
            tree.root,
            tree.tid,
            leaf_node(family, tree.buckets[0], &children),
        ),
        (anchor, tree.tid, bucket(family, &ints(left), Some(new))),
        (new, Tid::ZERO, bucket(family, &ints(right), next)),
    ]
}

/// The first key of each bucket of the tree of [`two_buckets`]: the
/// separator of each bucket but the first.
const SEPARATORS: [i128; 2] = [10, 40];

#[test]
fn both_sides_split_one_bucket_and_the_keys_of_its_range_merge() {
    let store = open(Some("on"));
    let tree = two_buckets(&store);
    let y = split(&store, &tree, 1, &[40, 50], &[60, 70]);
    let x = new_oid(&store);
    let mut txn = writing(&store, &split_writes(&tree, 1, x, &[40, 45], &[50, 60]));
    assert_eq!(txn.vote_resolving().unwrap(), []);
    let resolved = set(&txn.natively_resolved());
    assert_eq!(resolved, set(&[tree.root, tree.buckets[1], x]));
    for resolution in txn.resolutions() {
        assert_eq!(
            (resolution.split, resolution.outcome, resolution.applied),
            (Some(3), Resolved::Merged, true)
        );
    }
    let lines = txn.resolution_log_lines(SystemTime::now(), None);
    assert!(
        lines.iter().all(|line| line.contains("\"split\":3")),
        "{lines:?}"
    );
    txn.finish().unwrap();
    let (buckets, keys) = walk(&store, tree.root);
    // The keys at and above the separator of the other side go to the new
    // bucket of the transaction, which takes the place of the other one.
    assert_eq!(buckets, [tree.buckets[0], tree.buckets[1], x]);
    assert!(!buckets.contains(&y));
    assert_eq!(keys, [10, 20, 30, 40, 45, 50, 60, 70]);
    let (data, _) = store.load(x).unwrap();
    assert_eq!(data, bucket(II, &ints(&[60, 70]), None));
}

#[test]
fn shadow_mode_tells_the_merge_and_the_vote_fails_as_without_it() {
    let store = open(Some("shadow"));
    let tree = two_buckets(&store);
    split(&store, &tree, 1, &[40, 50], &[60, 70]);
    let x = new_oid(&store);
    let mut txn = writing(&store, &split_writes(&tree, 1, x, &[40, 45], &[50, 60]));
    let voted = txn.vote();
    assert!(matches!(voted, Err(ZoeError::Conflict { oid }) if oid == tree.root));
    let [resolution] = txn.resolutions() else {
        panic!("{:?}", txn.resolutions());
    };
    assert_eq!(resolution.outcome, Resolved::Refused(Reason::BTrees(11)));
    assert_eq!(resolution.shadow, Some(Resolved::Merged));
    assert_eq!(resolution.split, Some(3));
}

#[test]
fn without_the_switch_the_vote_keeps_the_stock_behaviour() {
    for mode in [None, Some("off")] {
        let store = open(mode);
        let tree = two_buckets(&store);
        split(&store, &tree, 1, &[40, 50], &[60, 70]);
        let x = new_oid(&store);
        let mut txn = writing(&store, &split_writes(&tree, 1, x, &[40, 45], &[50, 60]));
        let voted = txn.vote();
        assert!(matches!(voted, Err(ZoeError::Conflict { oid }) if oid == tree.root));
        assert!(
            txn.resolutions().iter().all(|r| r.split.is_none()),
            "{mode:?}"
        );
    }
}

#[test]
fn the_rules_run_only_where_tier_1_runs() {
    let store = open_with(Some("on"), &[("BTrees.IIBTree.IIBTree", "off")]);
    let tree = two_buckets(&store);
    split(&store, &tree, 1, &[40, 50], &[60, 70]);
    let x = new_oid(&store);
    let mut txn = writing(&store, &split_writes(&tree, 1, x, &[40, 45], &[50, 60]));
    assert!(matches!(txn.vote(), Err(ZoeError::Conflict { .. })));
    assert!(txn.resolutions().iter().all(|r| r.split.is_none()));
}

#[test]
fn a_split_of_a_bucket_that_the_other_side_changed_without_a_split_merges() {
    let store = open(Some("on"));
    let tree = two_buckets(&store);
    // The other side inserts 55 into the second bucket, without a split.
    commit(
        &store,
        &[(
            tree.buckets[1],
            tree.tid,
            bucket(II, &ints(&[40, 50, 55, 60]), None),
        )],
    );
    let x = new_oid(&store);
    let mut txn = writing(&store, &split_writes(&tree, 1, x, &[40, 45], &[50, 60]));
    txn.vote().unwrap();
    // The node is not stale: its record stands.
    assert_eq!(set(&txn.natively_resolved()), set(&[tree.buckets[1], x]));
    txn.finish().unwrap();
    let (buckets, keys) = walk(&store, tree.root);
    assert_eq!(buckets, [tree.buckets[0], tree.buckets[1], x]);
    assert_eq!(keys, [10, 20, 30, 40, 45, 50, 55, 60]);
}

#[test]
fn splits_of_different_children_merge_in_the_node() {
    let store = open(Some("on"));
    let tree = two_buckets(&store);
    let y = split(&store, &tree, 0, &[10, 15], &[20, 30]);
    let x = new_oid(&store);
    let mut txn = writing(&store, &split_writes(&tree, 1, x, &[40, 45], &[50, 60]));
    txn.vote().unwrap();
    assert_eq!(txn.natively_resolved(), [tree.root]);
    assert_eq!(txn.resolutions()[0].split, Some(2));
    txn.finish().unwrap();
    let (buckets, keys) = walk(&store, tree.root);
    assert_eq!(buckets, [tree.buckets[0], y, tree.buckets[1], x]);
    assert_eq!(keys, [10, 15, 20, 30, 40, 45, 50, 60]);
}

#[test]
fn a_read_current_node_passes_when_another_child_split() {
    let store = open(Some("on"));
    let tree = two_buckets(&store);
    let y = split(&store, &tree, 0, &[10, 15], &[20, 30]);
    // The transaction inserts 45 into the second bucket, without a split,
    // and marks the node read-current, as BTrees does.
    let mut txn = writing(
        &store,
        &[(
            tree.buckets[1],
            tree.tid,
            bucket(II, &ints(&[40, 45, 50, 60]), None),
        )],
    );
    txn.check_current_serial(tree.root, tree.tid).unwrap();
    assert_eq!(txn.vote_resolving().unwrap(), []);
    // The transaction did not store the node: nothing goes back to the
    // client as resolved.
    assert_eq!(txn.natively_resolved(), []);
    let [resolution] = txn.resolutions() else {
        panic!("{:?}", txn.resolutions());
    };
    assert_eq!(
        (
            resolution.oid,
            resolution.split,
            resolution.outcome,
            resolution.applied
        ),
        (tree.root, Some(1), Resolved::Merged, true)
    );
    let [line] = &txn.resolution_log_lines(SystemTime::now(), None)[..] else {
        panic!("one line");
    };
    assert!(line.contains("\"outcome\":\"passed\""), "{line}");
    assert!(line.contains("\"split\":1"), "{line}");
    txn.finish().unwrap();
    let (buckets, keys) = walk(&store, tree.root);
    assert_eq!(buckets, [tree.buckets[0], y, tree.buckets[1]]);
    assert_eq!(keys, [10, 15, 20, 30, 40, 45, 50, 60]);
}

#[test]
fn a_node_of_rule_1_passes_the_vote_after_the_caller_resolved() {
    let store = open(Some("on"));
    let tree = two_buckets(&store);
    split(&store, &tree, 0, &[10, 15], &[20, 30]);
    // An object that no tier merges conflicts too: the caller resolves it.
    let other = new_oid(&store);
    let thing = |n: i128| {
        let mut heap = Heap::new();
        let class = heap.new_class("m", "Thing");
        record(&heap, class, Value::Int(n))
    };
    let read = commit(&store, &[(other, Tid::ZERO, thing(1))]);
    let committed = commit(&store, &[(other, read, thing(2))]);
    let mut txn = writing(
        &store,
        &[
            (
                tree.buckets[1],
                tree.tid,
                bucket(II, &ints(&[40, 45, 50, 60]), None),
            ),
            (other, read, thing(3)),
        ],
    );
    txn.check_current_serial(tree.root, tree.tid).unwrap();
    let conflicts = txn.vote_resolving().unwrap();
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].0, other);
    txn.store(other, committed, &thing(4)).unwrap();
    txn.vote().unwrap();
    txn.finish().unwrap();
    assert_eq!(walk(&store, tree.root).1, [10, 15, 20, 30, 40, 45, 50, 60]);
}

#[test]
fn a_split_of_a_root_that_held_its_bucket_inline_merges() {
    let store = open(Some("on"));
    let root = new_oid(&store);
    let read = commit(
        &store,
        &[(root, Tid::ZERO, inline(II, &ints(&[10, 20, 30])))],
    );
    // The other side splits the inline bucket into two new buckets.
    let (a1, y1) = (new_oid(&store), new_oid(&store));
    commit(
        &store,
        &[
            (root, read, leaf_node(II, a1, &[(Key::Int(30), y1)])),
            (a1, Tid::ZERO, bucket(II, &ints(&[10, 20]), Some(y1))),
            (y1, Tid::ZERO, bucket(II, &ints(&[30, 40]), None)),
        ],
    );
    let (a, x) = (new_oid(&store), new_oid(&store));
    let mut txn = writing(
        &store,
        &[
            (root, read, leaf_node(II, a, &[(Key::Int(20), x)])),
            (a, Tid::ZERO, bucket(II, &ints(&[10]), Some(x))),
            (x, Tid::ZERO, bucket(II, &ints(&[20, 25, 30]), None)),
        ],
    );
    txn.vote().unwrap();
    assert_eq!(set(&txn.natively_resolved()), set(&[root, a, x]));
    txn.finish().unwrap();
    let (buckets, keys) = walk(&store, root);
    // The separator of the other side applies.
    assert_eq!(buckets, [a, x]);
    assert_eq!(keys, [10, 20, 25, 30, 40]);
    assert_eq!(store.load(x).unwrap().0, bucket(II, &ints(&[30, 40]), None));
}

#[test]
fn a_split_of_an_inline_bucket_that_the_other_side_changed_inline_merges() {
    let store = open(Some("on"));
    let root = new_oid(&store);
    let read = commit(
        &store,
        &[(root, Tid::ZERO, inline(II, &ints(&[10, 20, 30])))],
    );
    commit(&store, &[(root, read, inline(II, &ints(&[5, 10, 20, 30])))]);
    let (a, x) = (new_oid(&store), new_oid(&store));
    let mut txn = writing(
        &store,
        &[
            (root, read, leaf_node(II, a, &[(Key::Int(20), x)])),
            (a, Tid::ZERO, bucket(II, &ints(&[10]), Some(x))),
            (x, Tid::ZERO, bucket(II, &ints(&[20, 25, 30]), None)),
        ],
    );
    txn.vote().unwrap();
    txn.finish().unwrap();
    let (buckets, keys) = walk(&store, root);
    // The separator of the transaction applies.
    assert_eq!(buckets, [a, x]);
    assert_eq!(keys, [5, 10, 20, 25, 30]);
    assert_eq!(
        store.load(a).unwrap().0,
        bucket(II, &ints(&[5, 10]), Some(x))
    );
}

#[test]
fn a_refusal_inside_the_range_refuses_with_its_reason() {
    let store = open(Some("on"));
    let tree = two_buckets(&store);
    split(&store, &tree, 1, &[40, 50], &[60, 70]);
    // The transaction inserts 70 too.
    let x = new_oid(&store);
    let mut txn = writing(&store, &split_writes(&tree, 1, x, &[40, 50], &[60, 70]));
    let voted = txn.vote_resolving();
    // The vote fails on the bucket whose range refused.
    let anchor = tree.buckets[1];
    assert!(matches!(voted, Err(ZoeError::Conflict { oid }) if oid == anchor));
    let [resolution] = txn.resolutions() else {
        panic!("{:?}", txn.resolutions());
    };
    assert_eq!(
        (
            resolution.oid,
            resolution.split,
            resolution.outcome,
            resolution.applied
        ),
        (anchor, Some(3), Resolved::Refused(Reason::BTrees(6)), true)
    );
    assert!(txn.conflict_serials().is_some());
}

/// Runs a split of the second bucket of [`two_buckets`] by the transaction
/// against the writes of the other side that `other` gives. Returns the
/// outcome of the vote.
fn against(other: impl FnOnce(&Store, &Tree) -> Vec<(Oid, Tid, Vec<u8>)>) -> zoe_core::Result<()> {
    let store = open(Some("on"));
    let tree = two_buckets(&store);
    let writes = other(&store, &tree);
    commit(&store, &writes);
    let x = new_oid(&store);
    let mut txn = writing(&store, &split_writes(&tree, 1, x, &[40, 45], &[50, 60]));
    let voted = txn.vote();
    assert!(
        txn.resolutions().iter().all(|r| r.split.is_none()),
        "{:?}",
        txn.resolutions()
    );
    voted
}

#[test]
fn a_removal_of_a_child_gives_no_answer() {
    let voted = against(|_, tree| {
        vec![
            (tree.root, tree.tid, leaf_node(II, tree.buckets[0], &[])),
            (
                tree.buckets[0],
                tree.tid,
                bucket(II, &ints(&[10, 20, 30, 40, 50, 60]), None),
            ),
        ]
    });
    assert!(matches!(voted, Err(ZoeError::Conflict { .. })));
}

#[test]
fn a_changed_separator_gives_no_answer() {
    let voted = against(|_, tree| {
        let children = [(Key::Int(35), tree.buckets[1])];
        vec![(
            tree.root,
            tree.tid,
            leaf_node(II, tree.buckets[0], &children),
        )]
    });
    assert!(matches!(voted, Err(ZoeError::Conflict { .. })));
}

#[test]
fn two_splits_of_one_bucket_on_one_side_give_no_answer() {
    let voted = against(|store, tree| {
        let (y1, y2) = (new_oid(store), new_oid(store));
        let children = [
            (Key::Int(40), tree.buckets[1]),
            (Key::Int(50), y1),
            (Key::Int(60), y2),
        ];
        vec![
            (
                tree.root,
                tree.tid,
                leaf_node(II, tree.buckets[0], &children),
            ),
            (
                tree.buckets[1],
                tree.tid,
                bucket(II, &ints(&[40]), Some(y1)),
            ),
            (y1, Tid::ZERO, bucket(II, &ints(&[50]), Some(y2))),
            (y2, Tid::ZERO, bucket(II, &ints(&[60, 70]), None)),
        ]
    });
    assert!(matches!(voted, Err(ZoeError::Conflict { .. })));
}

#[test]
fn a_delete_in_the_range_gives_no_answer() {
    let voted = against(|_, tree| {
        vec![(
            tree.buckets[1],
            tree.tid,
            bucket(II, &ints(&[40, 60]), None),
        )]
    });
    assert!(matches!(voted, Err(ZoeError::Conflict { .. })));
}

#[test]
fn a_read_current_node_whose_path_child_split_gives_no_answer() {
    let store = open(Some("on"));
    let tree = two_buckets(&store);
    split(&store, &tree, 1, &[40, 50], &[60, 70]);
    // The transaction read the node and the second bucket as current.
    let other = new_oid(&store);
    let mut txn = writing(&store, &[(other, Tid::ZERO, inline(II, &ints(&[1])))]);
    txn.check_current_serial(tree.root, tree.tid).unwrap();
    txn.check_current_serial(tree.buckets[1], tree.tid).unwrap();
    let voted = txn.vote();
    assert!(matches!(voted, Err(ZoeError::ReadConflict { oid }) if oid == tree.root));
    assert!(txn.resolutions().is_empty());
}

#[test]
fn a_read_current_node_without_a_path_child_gives_no_answer() {
    let store = open(Some("on"));
    let tree = two_buckets(&store);
    split(&store, &tree, 0, &[10, 15], &[20, 30]);
    let other = new_oid(&store);
    let mut txn = writing(&store, &[(other, Tid::ZERO, inline(II, &ints(&[1])))]);
    txn.check_current_serial(tree.root, tree.tid).unwrap();
    let voted = txn.vote();
    assert!(matches!(voted, Err(ZoeError::ReadConflict { oid }) if oid == tree.root));
}

#[test]
fn a_read_current_node_in_shadow_mode_tells_that_it_would_pass() {
    let store = open(Some("shadow"));
    let tree = two_buckets(&store);
    split(&store, &tree, 0, &[10, 15], &[20, 30]);
    let mut txn = writing(
        &store,
        &[(
            tree.buckets[1],
            tree.tid,
            bucket(II, &ints(&[40, 45, 50, 60]), None),
        )],
    );
    txn.check_current_serial(tree.root, tree.tid).unwrap();
    let voted = txn.vote();
    assert!(matches!(voted, Err(ZoeError::ReadConflict { oid }) if oid == tree.root));
    let [resolution] = txn.resolutions() else {
        panic!("{:?}", txn.resolutions());
    };
    assert_eq!(resolution.mode, Mode::Shadow);
    assert_eq!(
        (resolution.split, resolution.shadow),
        (Some(1), Some(Resolved::Merged))
    );
    let line = &txn.resolution_log_lines(SystemTime::now(), None)[0];
    assert!(
        line.contains("\"shadow\":{\"outcome\":\"passed\""),
        "{line}"
    );
}

#[test]
fn two_levels_of_nodes_merge_node_by_node() {
    let store = open(Some("on"));
    // A root of two nodes, each of two buckets.
    let (root, n1, n2) = (new_oid(&store), new_oid(&store), new_oid(&store));
    let b: Vec<Oid> = (0..4).map(|_| new_oid(&store)).collect();
    let keys = [[10, 20], [30, 40], [50, 60], [70, 80]];
    let mut writes = Vec::new();
    for index in 0..4 {
        let next = b.get(index + 1).copied();
        writes.push((b[index], Tid::ZERO, bucket(II, &ints(&keys[index]), next)));
    }
    writes.push((n1, Tid::ZERO, leaf_node(II, b[0], &[(Key::Int(30), b[1])])));
    writes.push((n2, Tid::ZERO, leaf_node(II, b[2], &[(Key::Int(70), b[3])])));
    let tree_name = II.tree();
    writes.push((
        root,
        Tid::ZERO,
        node(II, n1, &[(Key::Int(50), n2)], &tree_name, b[0]),
    ));
    let tid = commit(&store, &writes);
    // The other side splits the node n1 into n1 and n3, and the last bucket
    // into b3 and y.
    let (n3, y) = (new_oid(&store), new_oid(&store));
    commit(
        &store,
        &[
            (
                root,
                tid,
                node(
                    II,
                    n1,
                    &[(Key::Int(30), n3), (Key::Int(50), n2)],
                    &tree_name,
                    b[0],
                ),
            ),
            (n1, tid, leaf_node(II, b[0], &[])),
            (n3, Tid::ZERO, leaf_node(II, b[1], &[])),
            (
                n2,
                tid,
                leaf_node(II, b[2], &[(Key::Int(70), b[3]), (Key::Int(80), y)]),
            ),
            (b[3], tid, bucket(II, &ints(&[70]), Some(y))),
            (y, Tid::ZERO, bucket(II, &ints(&[80, 90]), None)),
        ],
    );
    // The transaction splits the last bucket too. It marks the root
    // read-current and stores n2.
    let x = new_oid(&store);
    let mut txn = writing(
        &store,
        &[
            (
                n2,
                tid,
                leaf_node(II, b[2], &[(Key::Int(70), b[3]), (Key::Int(75), x)]),
            ),
            (b[3], tid, bucket(II, &ints(&[70]), Some(x))),
            (x, Tid::ZERO, bucket(II, &ints(&[75, 80]), None)),
        ],
    );
    txn.check_current_serial(root, tid).unwrap();
    txn.vote().unwrap();
    assert_eq!(set(&txn.natively_resolved()), set(&[n2, b[3], x]));
    let rules: BTreeSet<_> = txn.resolutions().iter().map(|r| r.split).collect();
    assert_eq!(rules, BTreeSet::from([Some(1), Some(3)]));
    txn.finish().unwrap();
    let (buckets, keys) = walk(&store, root);
    assert_eq!(buckets, [b[0], b[1], b[2], b[3], x]);
    assert_eq!(keys, [10, 20, 30, 40, 50, 60, 70, 75, 80, 90]);
}

/// Runs two splits of the last bucket of a tree of `OI` whose keys are
/// made by `make`. Returns the outcome of the vote and the keys of the tree
/// after it.
fn object_keys(make: fn(u64) -> Key) -> (zoe_core::Result<()>, Vec<i128>) {
    let store = open(Some("on"));
    let keys = |oids: &[u64]| oids.iter().map(|&oid| make(oid)).collect::<Vec<_>>();
    let tree = tree(&store, OI, &[&keys(&[10, 20, 30]), &keys(&[40, 50, 60])]);
    let y = new_oid(&store);
    let other = vec![
        (
            tree.root,
            tree.tid,
            leaf_node(
                OI,
                tree.buckets[0],
                &[(make(40), tree.buckets[1]), (make(60), y)],
            ),
        ),
        (
            tree.buckets[1],
            tree.tid,
            bucket(OI, &keys(&[40, 50]), Some(y)),
        ),
        (y, Tid::ZERO, bucket(OI, &keys(&[60, 70]), None)),
    ];
    commit(&store, &other);
    let x = new_oid(&store);
    let ours = vec![
        (
            tree.root,
            tree.tid,
            leaf_node(
                OI,
                tree.buckets[0],
                &[(make(40), tree.buckets[1]), (make(50), x)],
            ),
        ),
        (
            tree.buckets[1],
            tree.tid,
            bucket(OI, &keys(&[40, 45]), Some(x)),
        ),
        (x, Tid::ZERO, bucket(OI, &keys(&[50, 60]), None)),
    ];
    let mut txn = writing(&store, &ours);
    let voted = txn.vote();
    if voted.is_err() {
        return (voted, Vec::new());
    }
    txn.finish().unwrap();
    (voted, walk(&store, tree.root).1)
}

#[test]
fn key_references_of_one_class_order_by_the_oids_of_their_objects() {
    for make in [Key::Ref as fn(u64) -> Key, Key::Zope] {
        let (voted, keys) = object_keys(make);
        voted.unwrap();
        assert_eq!(keys, [10, 20, 30, 40, 45, 50, 60, 70]);
    }
}

#[test]
fn key_references_as_values_compare_by_the_oids_of_their_objects() {
    let store = open(Some("on"));
    let tree = tree(&store, IO, &[&ints(&[10, 20, 30]), &ints(&[40, 50, 60])]);
    split(&store, &tree, 1, &[40, 50], &[60, 70]);
    let x = new_oid(&store);
    let mut txn = writing(&store, &split_writes(&tree, 1, x, &[40, 45], &[50, 60]));
    txn.vote().unwrap();
    txn.finish().unwrap();
    assert_eq!(walk(&store, tree.root).1, [10, 20, 30, 40, 45, 50, 60, 70]);
}

#[test]
fn keys_that_the_order_does_not_take_give_no_answer() {
    // Key references to another database.
    let (voted, _) = object_keys(Key::Remote);
    assert!(matches!(voted, Err(ZoeError::Conflict { .. })));
    // Objects of a class that the order does not know.
    let (voted, _) = object_keys(|oid| Key::Opaque(i128::from(oid)));
    assert!(matches!(voted, Err(ZoeError::Conflict { .. })));
    // Key references of two classes.
    let (voted, _) = object_keys(|oid| {
        if oid % 20 == 0 {
            Key::Zope(oid)
        } else {
            Key::Ref(oid)
        }
    });
    assert!(matches!(voted, Err(ZoeError::Conflict { .. })));
}

/// Runs a transaction that `ours` makes against the writes of the other
/// side that `other` makes, on the tree of [`two_buckets`]. `ours` gives
/// the writes of the transaction and the oids that it marks read-current.
/// Returns the tree, the outcome of the vote and the entries of the vote.
fn sides(
    other: impl FnOnce(&Store, &Tree) -> Vec<(Oid, Tid, Vec<u8>)>,
    ours: impl FnOnce(&Store, &Tree) -> (Vec<(Oid, Tid, Vec<u8>)>, Vec<Oid>),
) -> (Tree, zoe_core::Result<()>, Vec<zoe_core::Resolution>) {
    let store = open(Some("on"));
    let tree = two_buckets(&store);
    let writes = other(&store, &tree);
    commit(&store, &writes);
    let (writes, read) = ours(&store, &tree);
    let mut txn = writing(&store, &writes);
    for oid in read {
        txn.check_current_serial(oid, tree.tid).unwrap();
    }
    let voted = txn.vote();
    let resolutions = txn.resolutions().to_vec();
    (tree, voted, resolutions)
}

#[test]
fn a_refusal_fails_on_the_stale_bucket_when_the_node_did_not_change() {
    let store = open(Some("on"));
    let tree = two_buckets(&store);
    // The other side inserts 70 into the second bucket, without a split.
    let current = commit(
        &store,
        &[(
            tree.buckets[1],
            tree.tid,
            bucket(II, &ints(&[40, 50, 60, 70]), None),
        )],
    );
    // The transaction splits it and inserts 70 too.
    let x = new_oid(&store);
    let mut txn = writing(&store, &split_writes(&tree, 1, x, &[40, 50], &[60, 70]));
    let voted = txn.vote();
    let anchor = tree.buckets[1];
    assert!(matches!(voted, Err(ZoeError::Conflict { oid }) if oid == anchor));
    assert_eq!(txn.conflict_serials(), Some((current, tree.tid)));
    let [resolution] = txn.resolutions() else {
        panic!("{:?}", txn.resolutions());
    };
    assert_eq!(
        (
            resolution.oid,
            &resolution.class[..],
            resolution.split,
            resolution.outcome
        ),
        (
            anchor,
            "BTrees.IIBTree.IIBucket",
            Some(3),
            Resolved::Refused(Reason::BTrees(6))
        )
    );
}

#[test]
fn splits_of_different_child_nodes_merge_at_the_root() {
    let store = open(Some("on"));
    // A root of two nodes, each of two buckets.
    let (root, n1, n2) = (new_oid(&store), new_oid(&store), new_oid(&store));
    let b: Vec<Oid> = (0..4).map(|_| new_oid(&store)).collect();
    let keys = [[10, 20], [30, 40], [50, 60], [70, 80]];
    let mut writes = Vec::new();
    for index in 0..4 {
        let next = b.get(index + 1).copied();
        writes.push((b[index], Tid::ZERO, bucket(II, &ints(&keys[index]), next)));
    }
    writes.push((n1, Tid::ZERO, leaf_node(II, b[0], &[(Key::Int(30), b[1])])));
    writes.push((n2, Tid::ZERO, leaf_node(II, b[2], &[(Key::Int(70), b[3])])));
    let tree_name = II.tree();
    writes.push((
        root,
        Tid::ZERO,
        node(II, n1, &[(Key::Int(50), n2)], &tree_name, b[0]),
    ));
    let tid = commit(&store, &writes);
    // The other side splits the node n1 into n1 and n3.
    let n3 = new_oid(&store);
    commit(
        &store,
        &[
            (
                root,
                tid,
                node(
                    II,
                    n1,
                    &[(Key::Int(30), n3), (Key::Int(50), n2)],
                    &tree_name,
                    b[0],
                ),
            ),
            (n1, tid, leaf_node(II, b[0], &[])),
            (n3, Tid::ZERO, leaf_node(II, b[1], &[])),
        ],
    );
    // The transaction splits the node n2 into n2 and n4.
    let n4 = new_oid(&store);
    let mut txn = writing(
        &store,
        &[
            (
                root,
                tid,
                node(
                    II,
                    n1,
                    &[(Key::Int(50), n2), (Key::Int(70), n4)],
                    &tree_name,
                    b[0],
                ),
            ),
            (n2, tid, leaf_node(II, b[2], &[])),
            (n4, Tid::ZERO, leaf_node(II, b[3], &[])),
        ],
    );
    txn.vote().unwrap();
    assert_eq!(txn.natively_resolved(), [root]);
    assert_eq!(txn.resolutions()[0].split, Some(2));
    txn.finish().unwrap();
    let (data, _) = store.load(root).unwrap();
    assert_eq!(
        data,
        node(
            II,
            n1,
            &[(Key::Int(30), n3), (Key::Int(50), n2), (Key::Int(70), n4)],
            &tree_name,
            b[0],
        )
    );
    let (buckets, keys) = walk(&store, root);
    assert_eq!(buckets, b);
    assert_eq!(keys, [10, 20, 30, 40, 50, 60, 70, 80]);
}

/// The writes of a split of the second bucket of [`two_buckets`] by the
/// other side: it keeps [40, 50], and a new bucket gets [60, 70].
fn other_split(store: &Store, tree: &Tree) -> Vec<(Oid, Tid, Vec<u8>)> {
    let y = new_oid(store);
    split_writes(tree, 1, y, &[40, 50], &[60, 70])
}

#[test]
fn a_path_child_that_the_transaction_stored_and_the_other_side_split_conflicts() {
    let (tree, voted, resolutions) = sides(other_split, |_, tree| {
        // A plain insert of 45 into the second bucket, under the node that
        // the transaction read.
        let writes = vec![(
            tree.buckets[1],
            tree.tid,
            bucket(II, &ints(&[40, 45, 50, 60]), None),
        )];
        (writes, vec![tree.root])
    });
    let anchor = tree.buckets[1];
    assert!(matches!(voted, Err(ZoeError::Conflict { oid }) if oid == anchor));
    assert!(
        resolutions.iter().all(|r| r.split.is_none()),
        "{resolutions:?}"
    );
}

#[test]
fn two_splits_of_one_bucket_by_the_transaction_give_no_answer() {
    let (tree, voted, resolutions) = sides(other_split, |store, tree| {
        let (x1, x2) = (new_oid(store), new_oid(store));
        let children = [
            (Key::Int(40), tree.buckets[1]),
            (Key::Int(50), x1),
            (Key::Int(60), x2),
        ];
        let writes = vec![
            (
                tree.root,
                tree.tid,
                leaf_node(II, tree.buckets[0], &children),
            ),
            (
                tree.buckets[1],
                tree.tid,
                bucket(II, &ints(&[40, 45]), Some(x1)),
            ),
            (x1, Tid::ZERO, bucket(II, &ints(&[50]), Some(x2))),
            (x2, Tid::ZERO, bucket(II, &ints(&[60]), None)),
        ];
        (writes, Vec::new())
    });
    assert!(matches!(voted, Err(ZoeError::Conflict { oid }) if oid == tree.root));
    assert!(
        resolutions.iter().all(|r| r.split.is_none()),
        "{resolutions:?}"
    );
}

#[test]
fn a_delete_by_the_transaction_in_the_range_gives_no_answer() {
    let (tree, voted, resolutions) = sides(other_split, |store, tree| {
        // The transaction splits the second bucket and deletes 50.
        let x = new_oid(store);
        (split_writes(tree, 1, x, &[40, 45], &[60]), Vec::new())
    });
    assert!(matches!(voted, Err(ZoeError::Conflict { oid }) if oid == tree.root));
    assert!(
        resolutions.iter().all(|r| r.split.is_none()),
        "{resolutions:?}"
    );
}

#[test]
fn a_refusal_on_a_bucket_whose_tier_1_is_off_fails_as_without_the_switch() {
    // Tier 1 runs on the node, and not on its buckets.
    let store = open_with(Some("on"), &[("BTrees.IIBTree.IIBucket", "off")]);
    let tree = two_buckets(&store);
    // The other side inserts 70 into the second bucket, without a split.
    let other = bucket(II, &ints(&[40, 50, 60, 70]), None);
    let current = commit(&store, &[(tree.buckets[1], tree.tid, other.clone())]);
    // The transaction splits it and inserts 70 too.
    let x = new_oid(&store);
    let mut txn = writing(&store, &split_writes(&tree, 1, x, &[40, 50], &[60, 70]));
    let voted = txn.vote();
    let anchor = tree.buckets[1];
    assert!(matches!(voted, Err(ZoeError::Conflict { oid }) if oid == anchor));
    assert_eq!(txn.conflict_serials(), Some((current, tree.tid)));
    // The rules and tier 1 leave no entry: the bucket goes to the caller.
    assert!(txn.resolutions().is_empty(), "{:?}", txn.resolutions());
    txn.abort();
    // No update is lost: the store keeps the commit of the other side.
    assert_eq!(store.load(anchor).unwrap(), (other, current));
    assert_eq!(walk(&store, tree.root).1, [10, 20, 30, 40, 50, 60, 70]);
}
