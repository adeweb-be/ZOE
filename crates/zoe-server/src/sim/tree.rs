//! The tree of the split writers (`Simulation::split_merges`).
//!
//! The store holds one `BTrees.IIBTree.IIBTree` with small buckets, and a
//! merge policy that sets `split_merges` to `on`. The writers insert
//! growing int keys into it, so most inserts go to its last bucket. An
//! insert into a full bucket splits it: the bucket keeps the lower half,
//! a new bucket takes the upper half, and the node gets the new bucket as
//! a child. Two writers that read one state of a full bucket split it
//! both. The vote of the second one then merges with rule 3 of ADR 0049:
//! it writes the bucket and the new bucket of the other side, and
//! resolves the node and its own new bucket without a record.
//!
//! The writers store the records as ZODB writes them for a BTree whose
//! node holds bucket children: a node is `((child, key, child, ...),
//! first)`, a bucket `(items,)` or `(items, next)`, and a reference to a
//! bucket `(oid, class)`.
//!
//! After each recovery and at the end of a run, `check` walks the tree:
//! the keys of each child are inside its separators, the keys ascend, and
//! each bucket names the next one. Every key of an acknowledged commit is
//! in the tree, every bucket of the store is in the tree, and the
//! transaction index of each commit of a writer agrees with its records.

use std::collections::BTreeSet;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use zoe_core::vfs::MemVfs;
use zoe_core::{Clock, Oid, Options, Store, Tid, ZoeError};
use zoe_pickle::btrees::{BucketState, TreeState, bucket_state, tree_state};
use zoe_pickle::python::{Heap, Pickler, Value};

use super::world::{Acked, TreeCommit};

/// The most keys of a bucket. An insert that makes more splits it.
pub(crate) const MAX_KEYS: usize = 4;

/// The first key of the writers. The planted buckets hold lower keys.
const FIRST_KEY: i128 = 100;

/// The module of the family of the tree.
const MODULE: &str = "BTrees.IIBTree";

/// The class pickle of a bucket and of a node.
const BUCKET_CLASS: &[u8] = b"\x80\x03cBTrees.IIBTree\nIIBucket\nq\x00.";
const NODE_CLASS: &[u8] = b"\x80\x03cBTrees.IIBTree\nIIBTree\nq\x00.";

/// The global of a bucket, for a persistent reference.
const BUCKET_GLOBAL: &[u8] = b"cBTrees.IIBTree\nIIBucket\n";

/// A bucket: its items in key order, and its next bucket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Bucket {
    pub(crate) items: Vec<(i128, i128)>,
    pub(crate) next: Option<u64>,
}

/// A node whose children are buckets: its first child, each later child
/// with its separator, and its first bucket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Node {
    pub(crate) first: u64,
    pub(crate) children: Vec<(i128, u64)>,
    pub(crate) first_bucket: u64,
}

impl Node {
    /// Return the child whose range holds `key`.
    pub(crate) fn child_for(&self, key: i128) -> u64 {
        self.children
            .iter()
            .rev()
            .find(|(separator, _)| *separator <= key)
            .map_or(self.first, |(_, oid)| *oid)
    }

    /// Put `new` with the separator `separator` right after the child
    /// `anchor`.
    pub(crate) fn insert_after(&mut self, anchor: u64, separator: i128, new: u64) {
        let at = self
            .children
            .iter()
            .position(|(_, oid)| *oid == anchor)
            .map_or(0, |position| position + 1);
        self.children.insert(at, (separator, new));
    }
}

impl Bucket {
    /// Insert `key` with `value`, in key order. A key that the bucket
    /// holds takes the new value.
    pub(crate) fn insert(&mut self, key: i128, value: i128) {
        match self.items.binary_search_by_key(&key, |(k, _)| *k) {
            Ok(at) => self.items[at].1 = value,
            Err(at) => self.items.insert(at, (key, value)),
        }
    }

    /// Split the bucket in two halves, as BTrees does: the bucket keeps
    /// the lower half and names `new` as its next bucket, and the new
    /// bucket takes the upper half and the next bucket of the bucket.
    pub(crate) fn split(self, new: u64) -> (Bucket, Bucket) {
        let mut items = self.items;
        let upper = items.split_off(items.len() / 2);
        let left = Bucket {
            items,
            next: Some(new),
        };
        let right = Bucket {
            items: upper,
            next: self.next,
        };
        (left, right)
    }
}

/// Return the key of the transaction `t` of the client `id` among
/// `clients`. Each key is unique, and the keys grow with `t`.
pub(crate) fn key(t: usize, clients: usize, id: usize) -> i128 {
    FIRST_KEY + (t * clients + id) as i128
}

/// Write `int` as a pickle int: `K`, `M` or `J`.
fn int(out: &mut Vec<u8>, int: i128) {
    match int {
        0..=0xff => out.extend([b'K', int as u8]),
        0x100..=0xffff => {
            out.push(b'M');
            out.extend((int as u16).to_le_bytes());
        }
        _ => {
            let int = i32::try_from(int).expect("a key of the tree fits in an i32");
            out.push(b'J');
            out.extend(int.to_le_bytes());
        }
    }
}

/// Write a persistent reference to the bucket `oid`: `(oid, class)`.
fn reference(out: &mut Vec<u8>, oid: u64) {
    out.extend([b'C', 8]);
    out.extend(oid.to_be_bytes());
    out.extend(BUCKET_GLOBAL);
    out.extend([0x86, b'Q']);
}

/// Return the record of `bucket`.
pub(crate) fn bucket_record(bucket: &Bucket) -> Vec<u8> {
    let mut out = BUCKET_CLASS.to_vec();
    out.extend(b"\x80\x03(");
    for &(key, value) in &bucket.items {
        int(&mut out, key);
        int(&mut out, value);
    }
    out.push(b't');
    match bucket.next {
        Some(next) => {
            reference(&mut out, next);
            out.push(0x86);
        }
        None => out.push(0x85),
    }
    out.push(b'.');
    out
}

/// Return the record of `node`.
pub(crate) fn node_record(node: &Node) -> Vec<u8> {
    let mut out = NODE_CLASS.to_vec();
    out.extend(b"\x80\x03(");
    reference(&mut out, node.first);
    for &(separator, oid) in &node.children {
        int(&mut out, separator);
        reference(&mut out, oid);
    }
    out.push(b't');
    reference(&mut out, node.first_bucket);
    out.extend([0x86, b'.']);
    out
}

/// A record of the tree, as `parse` reads it.
enum Parsed {
    Bucket(Bucket),
    Node(Node),
}

/// Return the oid of a persistent reference.
fn oid_of(heap: &Heap, value: Value) -> Option<u64> {
    let oid = &heap.reference(value)?.oid;
    Some(u64::from_be_bytes(oid[..].try_into().ok()?))
}

fn int_of(value: Value) -> Option<i128> {
    match value {
        Value::Int(int) => Some(int),
        _ => None,
    }
}

/// Read a record of the tree: a bucket, or a node whose children are
/// buckets. `None` for another record or shape.
fn parse(data: &[u8]) -> Option<Parsed> {
    let tree = zoe_pickle::decode_record(data).ok()?;
    let mut heap = Heap::new();
    let (class, state) = heap.load(&tree).ok()?;
    let (module, name) = heap.class(class)?;
    if module != MODULE {
        return None;
    }
    match name {
        "IIBucket" => {
            let BucketState { items, next } = bucket_state(&heap, state, true)?;
            let mut pairs = Vec::new();
            for pair in items.chunks(2) {
                pairs.push((int_of(pair[0])?, int_of(pair[1])?));
            }
            let next = match next {
                Some(next) => Some(oid_of(&heap, next)?),
                None => None,
            };
            Some(Parsed::Bucket(Bucket { items: pairs, next }))
        }
        "IIBTree" => {
            if tree_state(&heap, state)? != TreeState::Split {
                return None;
            }
            let &[children, first_bucket] = heap.tuple(state)? else {
                return None;
            };
            let children = heap.tuple(children)?;
            let first = oid_of(&heap, *children.first()?)?;
            let mut rest = Vec::new();
            for pair in children[1..].chunks(2) {
                let &[separator, child] = pair else {
                    return None;
                };
                rest.push((int_of(separator)?, oid_of(&heap, child)?));
            }
            Some(Parsed::Node(Node {
                first,
                children: rest,
                first_bucket: oid_of(&heap, first_bucket)?,
            }))
        }
        _ => None,
    }
}

/// Read a bucket record.
pub(crate) fn bucket(data: &[u8]) -> Option<Bucket> {
    match parse(data)? {
        Parsed::Bucket(bucket) => Some(bucket),
        Parsed::Node(_) => None,
    }
}

/// Read a node record.
pub(crate) fn node(data: &[u8]) -> Option<Node> {
    match parse(data)? {
        Parsed::Node(node) => Some(node),
        Parsed::Bucket(_) => None,
    }
}

/// The clock of the store that `plant` writes: the start of the run, and
/// no time passes.
struct Planted;

impl Clock for Planted {
    fn now(&self) -> SystemTime {
        UNIX_EPOCH + super::disk::START
    }

    fn monotonic(&self) -> Duration {
        Duration::ZERO
    }
}

/// The root of ZODB, a `PersistentMapping` that names the merge policy at
/// `policy`, as ZODB writes it.
fn root_record(policy: Oid) -> Vec<u8> {
    [
        &b"\x80\x03cpersistent.mapping\nPersistentMapping\nq\x00.\x80\x03}q\x01X\x04\x00\x00\x00\
           dataq\x02}q\x03X\x10\x00\x00\x00zoe.merge_policyq\x04C\x08"[..],
        &policy.0.to_be_bytes(),
        b"q\x05czoe.policy\nMergePolicy\nq\x06\x86q\x07Qss.",
    ]
    .concat()
}

/// The record of a merge policy that sets `split_merges` to `on`.
fn policy_record() -> Vec<u8> {
    let mut heap = Heap::new();
    let class = heap.new_class("zoe.policy", "MergePolicy");
    let classes = heap.new_dict(Vec::new());
    let items = vec![
        (heap.new_str("version"), Value::Int(1)),
        (heap.new_str("classes"), classes),
        (heap.new_str("split_merges"), heap.new_str("on")),
    ];
    let state = heap.new_dict(items);
    let mut pickler = Pickler::new(&heap);
    pickler.dump(class).expect("a class pickles");
    pickler.dump(state).expect("a dict pickles");
    pickler.into_bytes()
}

/// Return a new oid of `store` other than the root.
fn new_oid(store: &Store) -> zoe_core::Result<Oid> {
    loop {
        let oid = store.new_oid()?;
        if oid != Oid(0) {
            return Ok(oid);
        }
    }
}

/// Write the merge policy, the root of ZODB and the tree to `disk`, before
/// the server opens it. The tree is a node with two buckets of lower keys
/// than those of the writers. Return the oid of the node.
pub(crate) fn plant(disk: &MemVfs) -> zoe_core::Result<u64> {
    let store = Store::open_full(disk.clone(), Planted, Options::default())?;
    let policy = new_oid(&store)?;
    let root = new_oid(&store)?;
    let (a, b) = (new_oid(&store)?.0, new_oid(&store)?.0);
    let first = Bucket {
        items: vec![(10, 0), (20, 0)],
        next: Some(b),
    };
    let second = Bucket {
        items: vec![(30, 0), (40, 0)],
        next: None,
    };
    let node = Node {
        first: a,
        children: vec![(30, b)],
        first_bucket: a,
    };
    let writes = [
        (Oid(0), root_record(policy)),
        (policy, policy_record()),
        (root, node_record(&node)),
        (Oid(a), bucket_record(&first)),
        (Oid(b), bucket_record(&second)),
    ];
    let mut txn = store.tpc_begin(b"", b"plant the tree", b"")?;
    for (oid, data) in &writes {
        txn.store(*oid, Tid::ZERO, data)?;
    }
    txn.vote()?;
    txn.finish()?;
    Ok(root.0)
}

/// What `check` finds on a store.
#[derive(Debug, Default)]
pub(crate) struct Checked {
    /// Each break, as text.
    pub(crate) faults: Vec<String>,
    /// The keys of the tree, when its walk ends.
    pub(crate) keys: Option<BTreeSet<i128>>,
    /// The oids that acknowledged commits stored and that their votes
    /// resolved without a record.
    pub(crate) dropped: usize,
}

/// Walk the tree of the node `root` in `store`. Return its buckets and its
/// keys, or the first break.
fn walk(store: &Store, root: u64) -> Result<(BTreeSet<u64>, Vec<i128>), String> {
    let load = |oid: u64| {
        store
            .load(Oid(oid))
            .map(|(data, _)| data)
            .map_err(|error| format!("oid {oid} of the tree does not load: {error}"))
    };
    let Some(node) = node(&load(root)?) else {
        return Err(format!(
            "the root {root} of the tree is not a node of buckets"
        ));
    };
    let mut children = vec![(node.first, None)];
    children.extend(
        node.children
            .iter()
            .map(|&(separator, oid)| (oid, Some(separator))),
    );
    if node.first_bucket != node.first {
        return Err(format!(
            "the first bucket {} of the node is not its first child {}",
            node.first_bucket, node.first
        ));
    }
    let mut buckets = BTreeSet::new();
    let mut keys: Vec<i128> = Vec::new();
    for (index, &(oid, low)) in children.iter().enumerate() {
        let high = children
            .get(index + 1)
            .and_then(|&(_, separator)| separator);
        if !buckets.insert(oid) {
            return Err(format!("the bucket {oid} is twice in the node"));
        }
        let Some(bucket) = bucket(&load(oid)?) else {
            return Err(format!("the child {oid} of the node is not a bucket"));
        };
        if bucket.items.is_empty() {
            return Err(format!("the bucket {oid} is empty"));
        }
        let next = children.get(index + 1).map(|&(oid, _)| oid);
        if bucket.next != next {
            return Err(format!(
                "the bucket {oid} names the next bucket {:?}, not {next:?}",
                bucket.next
            ));
        }
        for &(key, _) in &bucket.items {
            if low.is_some_and(|low| key < low) || high.is_some_and(|high| key >= high) {
                return Err(format!(
                    "the key {key} of the bucket {oid} is outside {low:?}..{high:?}"
                ));
            }
            if keys.last().is_some_and(|&last| last >= key) {
                return Err(format!("the key {key} of the bucket {oid} does not ascend"));
            }
            keys.push(key);
        }
    }
    Ok((buckets, keys))
}

/// Check the tree of the node `root` in `store` against the acknowledged
/// commits `acked`:
///
/// - the walk of the tree finds the separators, the order of the keys and
///   the chain of the next buckets correct;
/// - the tree holds the key of each acknowledged commit of a writer;
/// - each bucket with a current record is in the tree;
/// - for each acknowledged commit of a writer whose vote resolved oids,
///   the transaction index of its tid lists each stored oid that the vote
///   did not resolve and each oid that the vote wrote and the transaction
///   did not store, and no other oid than the stored and resolved ones;
///   each resolved oid that the index does not list has no record at that
///   tid; and when the vote wrote an oid that the transaction did not
///   store, the index does not list the new bucket of the transaction.
pub(crate) fn check(store: &Store, root: u64, acked: &[Acked]) -> Checked {
    let mut checked = Checked::default();
    let (buckets, keys) = match walk(store, root) {
        Ok(walked) => walked,
        Err(fault) => {
            checked.faults.push(fault);
            return checked;
        }
    };
    let keys: BTreeSet<i128> = keys.into_iter().collect();
    for acked in acked {
        if let Some(commit) = &acked.tree
            && !keys.contains(&commit.key)
        {
            checked.faults.push(format!(
                "the key {} of the commit {:016x} is not in the tree",
                commit.key, acked.tid
            ));
        }
    }
    let mut next = None;
    loop {
        match store.record_iternext(next) {
            Ok(Some(record)) => {
                let class = zoe_pickle::decode_class_name(&record.data).ok().flatten();
                let is_bucket = class
                    .as_ref()
                    .is_some_and(|(module, name)| module == MODULE && name == "IIBucket");
                if is_bucket && !buckets.contains(&record.oid.0) {
                    checked.faults.push(format!(
                        "the bucket {} of tid {:016x} is not in the tree",
                        record.oid.0, record.tid.0
                    ));
                }
                match record.next {
                    Some(oid) => next = Some(oid),
                    None => break,
                }
            }
            Ok(None) => break,
            Err(error) => {
                checked
                    .faults
                    .push(format!("the current records do not read: {error}"));
                break;
            }
        }
    }
    for acked in acked {
        if let Some(commit) = &acked.tree
            && !commit.resolved.is_empty()
        {
            check_index(store, acked.tid, commit, &mut checked);
        }
    }
    checked.keys = Some(keys);
    checked
}

/// Check the transaction index of the commit `tid` of a writer whose vote
/// resolved oids, as `check` says.
fn check_index(store: &Store, tid: u64, commit: &TreeCommit, checked: &mut Checked) {
    let faults = &mut checked.faults;
    let listed: BTreeSet<u64> = match store
        .iterator(Some(Tid(tid)), Some(Tid(tid)))
        .map(|transactions| transactions.collect::<Vec<_>>())
    {
        Ok(transactions) => match transactions.as_slice() {
            [Ok(transaction)] => transaction.records.iter().map(|r| r.oid.0).collect(),
            [Err(error)] => {
                faults.push(format!("the commit {tid:016x} does not read: {error}"));
                return;
            }
            _ => {
                faults.push(format!(
                    "the iterator gives {} transactions at {tid:016x}",
                    transactions.len()
                ));
                return;
            }
        },
        Err(error) => {
            faults.push(format!("the commit {tid:016x} does not read: {error}"));
            return;
        }
    };
    let stored: BTreeSet<u64> = commit.stored.iter().copied().collect();
    let resolved: BTreeSet<u64> = commit.resolved.iter().copied().collect();
    for oid in stored.difference(&resolved) {
        if !listed.contains(oid) {
            faults.push(format!(
                "the index of {tid:016x} does not list the stored oid {oid}"
            ));
        }
    }
    let unstored: Vec<u64> = resolved.difference(&stored).copied().collect();
    for oid in &unstored {
        if !listed.contains(oid) {
            faults.push(format!(
                "the index of {tid:016x} does not list the written oid {oid}"
            ));
        }
    }
    for oid in &listed {
        if !stored.contains(oid) && !resolved.contains(oid) {
            faults.push(format!(
                "the index of {tid:016x} lists the oid {oid}, which the commit did not write"
            ));
        }
    }
    for &oid in stored.intersection(&resolved) {
        if listed.contains(&oid) {
            continue;
        }
        checked.dropped += 1;
        match store.load_serial(Oid(oid), Tid(tid)) {
            Err(ZoeError::NotFound { .. }) => {}
            Ok(_) => faults.push(format!(
                "the dropped oid {oid} has a record at {tid:016x} that the index does not list"
            )),
            Err(error) => faults.push(format!(
                "the dropped oid {oid} at {tid:016x} does not read: {error}"
            )),
        }
    }
    if !unstored.is_empty() {
        for oid in commit.created.iter().filter(|oid| resolved.contains(oid)) {
            if listed.contains(oid) {
                faults.push(format!(
                    "the index of {tid:016x} lists the new bucket {oid}, which rule 3 dropped"
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Plant the tree on a new disk, and open the store again.
    fn planted() -> (Store, u64) {
        let disk = MemVfs::new();
        let root = plant(&disk).unwrap();
        let store = Store::open_full(disk, Planted, Options::default()).unwrap();
        (store, root)
    }

    /// Commit `writes` over the current serials, and return the tid.
    fn commit(store: &Store, writes: &[(u64, Vec<u8>)]) -> u64 {
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        for (oid, data) in writes {
            let serial = store
                .load(Oid(*oid))
                .map_or(Tid::ZERO, |(_, serial)| serial);
            txn.store(Oid(*oid), serial, data).unwrap();
        }
        txn.vote().unwrap();
        txn.finish().unwrap().0
    }

    fn acked(tid: u64, key: i128, stored: &[u64], created: &[u64], resolved: &[u64]) -> Acked {
        Acked {
            tid,
            records: Vec::new(),
            blobs: Vec::new(),
            tree: Some(Box::new(TreeCommit {
                key,
                stored: stored.to_vec(),
                created: created.to_vec(),
                resolved: resolved.to_vec(),
            })),
        }
    }

    fn bucket_of(items: &[i128], next: Option<u64>) -> Vec<u8> {
        bucket_record(&Bucket {
            items: items.iter().map(|&key| (key, 0)).collect(),
            next,
        })
    }

    #[test]
    fn the_records_of_the_writers_read_back() {
        let bucket = Bucket {
            items: vec![(10, 0), (300, 7), (70_000, -1)],
            next: Some(0x0102_0304_0506_0708),
        };
        assert_eq!(super::bucket(&bucket_record(&bucket)), Some(bucket));
        let last = Bucket {
            items: vec![(1, 2)],
            next: None,
        };
        assert_eq!(super::bucket(&bucket_record(&last)), Some(last));
        let node = Node {
            first: 3,
            children: vec![(30, 4), (70_000, 9)],
            first_bucket: 3,
        };
        assert_eq!(super::node(&node_record(&node)), Some(node.clone()));
        assert_eq!(node.child_for(29), 3);
        assert_eq!(node.child_for(30), 4);
        assert_eq!(node.child_for(80_000), 9);
    }

    #[test]
    fn a_split_keeps_the_lower_half_and_names_the_new_bucket() {
        let mut bucket = Bucket {
            items: vec![(10, 0), (20, 0), (30, 0), (40, 0)],
            next: Some(8),
        };
        bucket.insert(25, 1);
        let (left, right) = bucket.split(9);
        assert_eq!(left.items, vec![(10, 0), (20, 0)]);
        assert_eq!(left.next, Some(9));
        assert_eq!(right.items, vec![(25, 1), (30, 0), (40, 0)]);
        assert_eq!(right.next, Some(8));
    }

    #[test]
    fn the_planted_tree_passes_and_a_missing_key_fails() {
        let (store, root) = planted();
        let checked = check(&store, root, &[acked(1, 40, &[], &[], &[])]);
        assert_eq!(checked.faults, Vec::<String>::new());
        assert_eq!(
            checked.keys.unwrap().into_iter().collect::<Vec<_>>(),
            [10, 20, 30, 40]
        );
        let faults = check(&store, root, &[acked(1, 100, &[], &[], &[])]).faults;
        assert_eq!(faults.len(), 1, "{faults:?}");
        assert!(faults[0].contains("key 100"), "{faults:?}");
    }

    #[test]
    fn a_bucket_outside_the_tree_fails() {
        let (store, root) = planted();
        let orphan = store.new_oid().unwrap().0;
        commit(&store, &[(orphan, bucket_of(&[100], None))]);
        let faults = check(&store, root, &[]).faults;
        assert_eq!(faults.len(), 1, "{faults:?}");
        assert!(
            faults[0].contains(&format!("bucket {orphan} ")),
            "{faults:?}"
        );
    }

    #[test]
    fn a_broken_chain_and_a_key_outside_its_child_fail() {
        let (store, root) = planted();
        let node = node(&store.load(Oid(root)).unwrap().0).unwrap();
        let (a, b) = (node.first, node.children[0].1);
        commit(&store, &[(a, bucket_of(&[10, 20], None))]);
        let faults = check(&store, root, &[]).faults;
        assert!(faults[0].contains("names the next bucket"), "{faults:?}");
        commit(&store, &[(a, bucket_of(&[10, 20], Some(b)))]);
        commit(&store, &[(b, bucket_of(&[25, 40], None))]);
        let faults = check(&store, root, &[]).faults;
        assert!(faults[0].contains("key 25"), "{faults:?}");
    }

    #[test]
    fn the_index_of_a_vote_that_wrote_y_must_drop_n_and_x() {
        // The transaction stored the node N, the bucket A and its new
        // bucket X; the vote resolved N, A and X, and wrote the bucket B
        // of the other side.
        let (store, root) = planted();
        let node = node(&store.load(Oid(root)).unwrap().0).unwrap();
        let (a, b) = (node.first, node.children[0].1);
        let x = store.new_oid().unwrap().0;
        let stored = [root, a, x];
        let resolved = [root, a, x, b];
        // The commit holds A and B alone: N and X are dropped.
        let tid = commit(
            &store,
            &[
                (a, bucket_of(&[10, 20, 25], Some(b))),
                (b, bucket_of(&[30, 40], None)),
            ],
        );
        let checked = check(&store, root, &[acked(tid, 25, &stored, &[x], &resolved)]);
        assert_eq!(checked.faults, Vec::<String>::new());
        assert_eq!(checked.dropped, 2);
        // A commit that holds X too breaks the rule.
        let split = Node {
            first: a,
            children: vec![(20, x), (30, b)],
            first_bucket: a,
        };
        let tid = commit(
            &store,
            &[
                (root, node_record(&split)),
                (a, bucket_of(&[10], Some(x))),
                (x, bucket_of(&[20, 25], Some(b))),
                (b, bucket_of(&[30, 40], None)),
            ],
        );
        let faults = check(&store, root, &[acked(tid, 25, &stored, &[x], &resolved)]).faults;
        assert_eq!(faults.len(), 1, "{faults:?}");
        assert!(faults[0].contains("rule 3 dropped"), "{faults:?}");
        // An index that lacks a stored oid that the vote did not resolve,
        // and lists an oid that the transaction did not write.
        let faults = check(&store, root, &[acked(tid, 25, &[a, 77], &[], &[b])]).faults;
        assert_eq!(faults.len(), 3, "{faults:?}");
    }
}
