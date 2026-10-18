//! The split merge of ADR 0048: tier 1 merges the splits of a BTree node.
//!
//! A node is a BTree or a tree set of a family of tier 1 whose state holds
//! children: `(children, firstbucket)`. `children` holds the first child,
//! then each separator and its child. A split of a child A adds a new
//! child X right after A, with the first key of X as its separator. A is
//! the anchor of the split. A root that holds one bucket inline splits into
//! a node of two new buckets.
//!
//! Three rules decide the records of one node in the vote:
//!
//! 1. A node that the transaction marked read-current and did not store
//!    passes when the other side changed it only by splits and split no
//!    child that the transaction stored or marked read-current
//!    ([`read_node`]).
//! 2. The splits of different children merge: the merged node is the
//!    committed node with the new children of the transaction right after
//!    their anchors ([`written_node`]).
//! 3. A split of one bucket merges the keys of its range with the rules of
//!    `bucket_merge`. The merged keys go to the bucket and to the new bucket
//!    of the transaction, at the separator of the committed side when it
//!    split, else at the separator of the transaction. The new bucket of
//!    the transaction takes the place of the new bucket of the committed
//!    side ([`written_node`]).
//!
//! The rules write only records that the transaction stored. They read the
//! records through a [`Source`]. They give no answer for a node that
//! changed by anything but splits, for a delete in a range that they merge,
//! for two splits of one child on one side, and for a state that the model
//! does not hold. Two key references of one class compare by the oids of
//! their objects.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use zoe_pickle::Limits;
use zoe_pickle::btrees::{
    BucketState, Class, Family, Shape, TreeState, bucket_state, classify, new_bucket_state,
    tree_state,
};
use zoe_pickle::python::{Heap, Pickler, Value};

use super::btrees::{compare_keys, merge_range};
use super::{Outcome, Reason};
use crate::error::{Result, ZoeError};
use crate::types::{Oid, Tid};

/// The records that the rules read.
pub(crate) trait Source {
    /// Returns the serial and the pickle that the transaction stored for
    /// `oid`, or `None` when it stored none, restored it or deleted it.
    fn staged(&self, oid: Oid) -> Result<Option<(Tid, Vec<u8>)>>;

    /// Tells if the transaction stored, restored or deleted `oid`.
    fn stores(&self, oid: Oid) -> bool;

    /// Returns the serial and the pickle of the current record of `oid`, or
    /// `None` when it has none or is deleted.
    fn current(&self, oid: Oid) -> Result<Option<(Tid, Vec<u8>)>>;

    /// Returns the pickle of the record of `oid` at `serial`.
    fn at(&self, oid: Oid, serial: Tid) -> Result<Option<Vec<u8>>>;

    /// Tells if the transaction marked `oid` read-current.
    fn read_current(&self, oid: Oid) -> bool;
}

/// A record that a rule writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Written {
    /// The oid of the record.
    pub(crate) oid: Oid,
    /// Its class, as `module.name`.
    pub(crate) class: String,
    /// The serial that the transaction read: `Tid::ZERO` for a new object.
    pub(crate) read: Tid,
    /// The serial of the current record, which the merged record goes
    /// over: `Tid::ZERO` for a new object.
    pub(crate) current: Tid,
    /// The merged record.
    pub(crate) pickle: Vec<u8>,
    /// The keys that the new side changed in the range of a bucket, as
    /// text, at most `MAX_KEYS`.
    pub(crate) keys: Vec<String>,
    /// The number of keys that the new side changed.
    pub(crate) changed: usize,
}

/// What the rules give for one node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Split {
    /// Rule 2 or 3: the records to write, the node first when it changed.
    Merged { rule: u8, records: Vec<Written> },
    /// Rule 1: the read-current node passes.
    Passed,
    /// Rule 3: `bucket_merge` refuses the merge of a range.
    Refused { rule: u8, reason: Reason },
    /// No answer: the vote goes on as without the rules. The text says why.
    Undecided(&'static str),
}

/// Returns the class of `record` as `module.name` when it is a node class
/// of tier 1: a BTree or a tree set of a family. Reads the class pickle
/// alone.
pub(crate) fn node_class(record: &[u8]) -> Option<String> {
    let (module, name) = zoe_pickle::decode_class_name(record).ok()??;
    match classify(&module, &name)? {
        Class::Family(Shape::Tree | Shape::TreeSet, _) => Some(format!("{module}.{name}")),
        _ => None,
    }
}

/// Tells if `record` is a bucket or a set of a family of tier 1. Reads the
/// class pickle alone.
pub(crate) fn is_bucket(record: &[u8]) -> bool {
    let Ok(Some((module, name))) = zoe_pickle::decode_class_name(record) else {
        return false;
    };
    matches!(
        classify(&module, &name),
        Some(Class::Family(Shape::Bucket | Shape::Set, _))
    )
}

/// Runs rules 2 and 3 on the node `oid`, which the transaction stored.
pub(crate) fn written_node(source: &dyn Source, oid: Oid) -> Result<Split> {
    Group::new(source).answer(|group| group.written(oid))
}

/// Runs rule 1 on the node `oid`, which the transaction marked
/// read-current at `read` and did not store.
pub(crate) fn read_node(source: &dyn Source, oid: Oid, read: Tid) -> Result<Split> {
    Group::new(source).answer(|group| group.read(oid, read))
}

/// Why a rule stops before its end: an answer, or an error of the store.
enum Stop {
    Answer(Split),
    Store(ZoeError),
}

impl From<ZoeError> for Stop {
    fn from(error: ZoeError) -> Self {
        Stop::Store(error)
    }
}

impl From<Outcome> for Stop {
    /// A refusal of a merge of tier 1 inside a rule is a refusal of rule
    /// 3, the only rule that merges keys.
    fn from(outcome: Outcome) -> Self {
        Stop::Answer(match outcome {
            Outcome::Refused(reason) => Split::Refused { rule: 3, reason },
            Outcome::Undecided(why) => Split::Undecided(why),
            Outcome::Merged(_) => Split::Undecided("a merge where a refusal was due"),
        })
    }
}

/// The result of a step of a rule.
type Step<T> = std::result::Result<T, Stop>;

/// Returns the stop of no answer, for `why`.
fn no<T>(why: &'static str) -> Step<T> {
    Err(Stop::Answer(Split::Undecided(why)))
}

/// A child of a node: its separator, none for the first child, and its
/// reference.
#[derive(Clone, Copy, Debug)]
struct Child {
    separator: Option<Value>,
    reference: Value,
    oid: Oid,
}

/// The children of a node and the reference of its first bucket.
#[derive(Clone, Debug)]
struct Node {
    children: Vec<Child>,
    first: Value,
    first_oid: Oid,
}

/// The state of a BTree or a tree set.
enum Tree {
    /// One bucket inline: the state of the bucket.
    Inline(Value),
    /// A node.
    Node(Node),
}

/// A bucket: its items and its next bucket.
struct Leaf {
    items: Vec<Value>,
    next: Option<Value>,
    next_oid: Option<Oid>,
}

/// The records of one node that the rules read, in one heap, so that each
/// persistent reference is one object.
struct Group<'s> {
    source: &'s dyn Source,
    heap: Heap,
    /// The bytes of the records that the group loaded, for the step budget.
    len: usize,
    family: Family,
    /// True for a BTree, false for a tree set.
    mapping: bool,
}

impl<'s> Group<'s> {
    fn new(source: &'s dyn Source) -> Self {
        Group {
            source,
            heap: Heap::with_instances(),
            len: 0,
            family: Family {
                keys: zoe_pickle::btrees::KeyType::Object,
                values: zoe_pickle::btrees::ValueType::Object,
            },
            mapping: true,
        }
    }

    /// Runs `rule` and gives its answer, or the error of the store.
    fn answer(mut self, rule: impl FnOnce(&mut Self) -> Step<Split>) -> Result<Split> {
        match rule(&mut self) {
            Ok(split) | Err(Stop::Answer(split)) => Ok(split),
            Err(Stop::Store(error)) => Err(error),
        }
    }

    /// Reads the family of the node class of `record`, and returns its
    /// class as `module.name`.
    fn node_family(&mut self, record: &[u8]) -> Step<String> {
        let Ok(Some((module, name))) = zoe_pickle::decode_class_name(record) else {
            return no("a class pickle that does not decode");
        };
        let Some(Class::Family(shape @ (Shape::Tree | Shape::TreeSet), family)) =
            classify(&module, &name)
        else {
            return no("a record that is not a node of tier 1");
        };
        self.family = family;
        self.mapping = shape == Shape::Tree;
        Ok(format!("{module}.{name}"))
    }

    /// Decodes and loads a record. Each budget is the length of the
    /// records that the group loaded, so a merge takes a number of steps in
    /// proportion to its records.
    fn load(&mut self, record: &[u8]) -> Step<(Value, Value)> {
        let limits = Limits {
            node_budget: record.len(),
        };
        let Ok(tree) = zoe_pickle::decode_record_with(record, limits) else {
            return no("a record that does not decode");
        };
        self.len = self.len.saturating_add(record.len());
        self.heap.set_budget(super::budget(self.len));
        let (class, state) = self
            .heap
            .load(&tree)
            .map_err(|unsupported| Stop::Answer(Split::Undecided(unsupported.0)))?;
        // ZODB writes the class of a bucket and of a BTree as a GLOBAL
        // alone.
        if self.heap.class(class).is_none() {
            return no("a class pickle of another format");
        }
        Ok((class, state))
    }

    /// Compares two keys of the family, with the order of key references.
    fn compare(&self, a: Value, b: Value) -> Step<Ordering> {
        compare_keys(&self.heap, self.family.keys, true, a, b).map_err(|outcome| match outcome {
            // A separator that Python cannot compare is not a refusal of
            // `bucket_merge`.
            Outcome::Refused(_) => {
                Stop::Answer(Split::Undecided("keys that Python cannot compare"))
            }
            other => Stop::from(other),
        })
    }

    /// Returns the oid of a persistent reference to an object of the same
    /// database.
    fn oid(&self, value: Value) -> Step<Oid> {
        let Some(reference) = self.heap.reference(value) else {
            return no("a child that is not a persistent reference");
        };
        if reference.database.is_some() || reference.weak {
            return no("a reference to another database, or a weak reference");
        }
        let Ok(oid) = <[u8; 8]>::try_from(&reference.oid[..]) else {
            return no("an oid that is not 8 bytes");
        };
        Ok(Oid(u64::from_be_bytes(oid)))
    }

    /// Reads the state of a BTree or a tree set.
    fn tree(&self, state: Value) -> Step<Tree> {
        match tree_state(&self.heap, state) {
            Some(TreeState::Inline(bucket)) => Ok(Tree::Inline(bucket)),
            Some(TreeState::Split) => {
                let Some(&[children, first]) = self.heap.tuple(state) else {
                    return no("a node state of another shape");
                };
                let Some(items) = self.heap.tuple(children) else {
                    return no("a node state of another shape");
                };
                if items.len().is_multiple_of(2) {
                    return no("a node state of another shape");
                }
                let mut node = Node {
                    children: Vec::with_capacity(items.len() / 2 + 1),
                    first,
                    first_oid: self.oid(first)?,
                };
                for (index, &reference) in items.iter().enumerate().step_by(2) {
                    let separator = match index {
                        0 => None,
                        _ => match self.family.keys.stored(items[index - 1]) {
                            Some(separator) => Some(separator),
                            None => return no("a separator that the family does not take"),
                        },
                    };
                    node.children.push(Child {
                        separator,
                        reference,
                        oid: self.oid(reference)?,
                    });
                }
                Ok(Tree::Node(node))
            }
            Some(TreeState::Empty) => no("an empty tree"),
            None => no("a tree state of another shape"),
        }
    }

    /// Reads the state of a bucket or a set.
    fn bucket(&self, state: Value) -> Step<Leaf> {
        let Some(BucketState { items, next }) = bucket_state(&self.heap, state, self.mapping)
        else {
            return no("a bucket state of another shape");
        };
        let next_oid = next.map(|next| self.oid(next)).transpose()?;
        Ok(Leaf {
            items,
            next,
            next_oid,
        })
    }

    /// Loads the record of a bucket of the family of the node. Returns its
    /// class value, its class as `module.name` and its state.
    fn leaf(&mut self, record: &[u8]) -> Step<(Value, String, Leaf)> {
        let (class, state) = self.load(record)?;
        let Some((module, name)) = self.heap.class(class) else {
            return no("a class pickle of another format");
        };
        let shape = if self.mapping {
            Shape::Bucket
        } else {
            Shape::Set
        };
        match classify(module, name) {
            Some(Class::Family(found, family)) if found == shape && family == self.family => {}
            _ => return no("a child that is not a bucket of the family of its node"),
        }
        let name = format!("{module}.{name}");
        Ok((class, name, self.bucket(state)?))
    }

    /// Returns the record of a new object of the transaction: an oid that
    /// it stored with the serial `Tid::ZERO`.
    fn new_object(&self, oid: Oid) -> Step<Vec<u8>> {
        match self.source.staged(oid)? {
            Some((Tid::ZERO, record)) => Ok(record),
            _ => no("a new child that is not a new object of the transaction"),
        }
    }

    /// Returns the serial and the pickle of the current record of `oid`.
    fn current(&self, oid: Oid) -> Step<(Tid, Vec<u8>)> {
        match self.source.current(oid)? {
            Some(current) => Ok(current),
            None => no("a child without a current record"),
        }
    }

    /// Returns the pickle of `node`, of the class `class`.
    fn pickle(&self, class: Value, state: Value) -> Step<Vec<u8>> {
        let mut pickler = Pickler::new(&self.heap);
        match pickler.dump(class).and_then(|()| pickler.dump(state)) {
            Ok(()) => Ok(pickler.into_bytes()),
            Err(unsupported) => no(unsupported.0),
        }
    }

    /// Returns the new children of `after` against `before`, by the index of
    /// their anchor in `before`, when `after` changed only by splits: it
    /// holds each child of `before` in the same order with the same
    /// separator, and each other child comes right after its anchor or
    /// after another new child of the same anchor. Returns `None` when
    /// `after` changed otherwise.
    fn splits(&self, before: &Node, after: &Node) -> Step<Option<BTreeMap<usize, Vec<Child>>>> {
        if before.first_oid != after.first_oid {
            return Ok(None);
        }
        let known: BTreeSet<Oid> = before.children.iter().map(|child| child.oid).collect();
        let mut seen = BTreeSet::new();
        if known.len() != before.children.len()
            || !after.children.iter().all(|child| seen.insert(child.oid))
        {
            return Ok(None);
        }
        let mut added: BTreeMap<usize, Vec<Child>> = BTreeMap::new();
        let mut next = 0;
        for (index, child) in before.children.iter().enumerate() {
            let Some(other) = after.children.get(next) else {
                return Ok(None);
            };
            if other.oid != child.oid {
                return Ok(None);
            }
            let same = match (child.separator, other.separator) {
                (None, None) => true,
                (Some(a), Some(b)) => self.compare(a, b)? == Ordering::Equal,
                _ => false,
            };
            if !same {
                return Ok(None);
            }
            next += 1;
            while let Some(new) = after.children.get(next)
                && !known.contains(&new.oid)
            {
                added.entry(index).or_default().push(*new);
                next += 1;
            }
        }
        Ok((next == after.children.len()).then_some(added))
    }

    /// Merges the range of one bucket with the rules of `bucket_merge`, and
    /// cuts the merged items at `separator`. Returns the items below the
    /// separator, the items at and above it, the keys that the new side
    /// changed and their number. Neither half may be empty.
    fn merge(
        &mut self,
        old: &[Value],
        committed: &[Value],
        new: &[Value],
        separator: Value,
    ) -> Step<Cut> {
        let (mut items, keys, changed) = merge_range(
            &mut self.heap,
            self.family,
            self.mapping,
            old,
            committed,
            new,
        )?;
        let width = if self.mapping { 2 } else { 1 };
        let mut cut = items.len();
        for (index, item) in items.chunks(width).enumerate() {
            if self.compare(item[0], separator)? != Ordering::Less {
                cut = index * width;
                break;
            }
        }
        if cut == 0 || cut == items.len() {
            return no("a split merge that leaves a bucket empty");
        }
        let high = items.split_off(cut);
        Ok(Cut {
            low: items,
            high,
            keys,
            changed,
        })
    }

    /// Rules 2 and 3 on the node `oid`, which the transaction stored.
    fn written(&mut self, oid: Oid) -> Step<Split> {
        let Some((read, new)) = self.source.staged(oid)? else {
            return no("a node that the transaction did not store");
        };
        if read == Tid::ZERO {
            return no("a new node");
        }
        let Some(old) = self.source.at(oid, read)? else {
            return no("the record that the transaction read is gone");
        };
        let Some((current, committed)) = self.source.current(oid)? else {
            return no("a node without a current record");
        };
        let name = self.node_family(&new)?;
        // ZODB's resolver loads the new record, then the old one, then the
        // committed one.
        let (class, new) = self.load(&new)?;
        let (_, old) = self.load(&old)?;
        let committed = if current == read {
            old
        } else {
            self.load(&committed)?.1
        };
        let (new, old, committed) = (self.tree(new)?, self.tree(old)?, self.tree(committed)?);
        let node = Merging {
            oid,
            name,
            class,
            read,
            current,
        };
        match old {
            Tree::Inline(bucket) => self.inline(&node, bucket, committed, new),
            Tree::Node(old) => self.node(&node, &old, committed, new),
        }
    }

    /// Rule 3 on a root that held its one bucket inline in the record that
    /// the transaction read, and that the transaction split into two new
    /// buckets.
    fn inline(&mut self, node: &Merging, bucket: Value, committed: Tree, new: Tree) -> Step<Split> {
        if node.current == node.read {
            return no("a root that the other side did not change");
        }
        let Tree::Node(new) = new else {
            return no("a root that the transaction did not split");
        };
        let [a, x] = new.children[..] else {
            return no("two splits of one bucket by the transaction");
        };
        if new.first_oid != a.oid {
            return no("a first bucket that is not the first child");
        }
        let (a_record, x_record) = (self.new_object(a.oid)?, self.new_object(x.oid)?);
        let (a_class, a_name, a_new) = self.leaf(&a_record)?;
        let (x_class, x_name, x_new) = self.leaf(&x_record)?;
        if a_new.next_oid != Some(x.oid) || x_new.next.is_some() {
            return no("next buckets that are not those of one split");
        }
        let old = self.bucket(bucket)?;
        if old.next.is_some() {
            return no("an inline bucket with a next bucket");
        }
        let (committed, separator) = match committed {
            Tree::Inline(bucket) => {
                let bucket = self.bucket(bucket)?;
                if bucket.next.is_some() {
                    return no("an inline bucket with a next bucket");
                }
                (bucket.items, x.separator)
            }
            Tree::Node(committed) => {
                let [c, y] = committed.children[..] else {
                    return no("two splits of one bucket by the other side");
                };
                if committed.first_oid != c.oid {
                    return no("a first bucket that is not the first child");
                }
                let (_, c_record) = self.current(c.oid)?;
                let (_, y_record) = self.current(y.oid)?;
                let (_, _, c_leaf) = self.leaf(&c_record)?;
                let (_, _, y_leaf) = self.leaf(&y_record)?;
                if c_leaf.next_oid != Some(y.oid) || y_leaf.next.is_some() {
                    return no("next buckets that are not those of one split");
                }
                let mut items = c_leaf.items;
                items.extend(y_leaf.items);
                (items, y.separator)
            }
        };
        let Some(separator) = separator else {
            return no("a second child without a separator");
        };
        let mut ours = a_new.items;
        ours.extend(x_new.items);
        let Cut {
            low,
            high,
            keys,
            changed,
        } = self.merge(&old.items, &committed, &ours, separator)?;
        let a_state = new_bucket_state(&mut self.heap, low, a_new.next);
        let x_state = new_bucket_state(&mut self.heap, high, None);
        let children = self
            .heap
            .new_tuple(vec![a.reference, separator, x.reference]);
        let root = self.heap.new_tuple(vec![children, new.first]);
        let records = vec![
            Written {
                oid: node.oid,
                class: node.name.clone(),
                read: node.read,
                current: node.current,
                pickle: self.pickle(node.class, root)?,
                keys: keys.clone(),
                changed,
            },
            Written {
                oid: a.oid,
                class: a_name,
                read: Tid::ZERO,
                current: Tid::ZERO,
                pickle: self.pickle(a_class, a_state)?,
                keys: keys.clone(),
                changed,
            },
            Written {
                oid: x.oid,
                class: x_name,
                read: Tid::ZERO,
                current: Tid::ZERO,
                pickle: self.pickle(x_class, x_state)?,
                keys,
                changed,
            },
        ];
        Ok(Split::Merged { rule: 3, records })
    }

    /// Rules 2 and 3 on a node that held children in the record that the
    /// transaction read.
    fn node(&mut self, node: &Merging, old: &Node, committed: Tree, new: Tree) -> Step<Split> {
        let (Tree::Node(committed), Tree::Node(new)) = (committed, new) else {
            return no("a node that a side made inline");
        };
        let Some(theirs) = self.splits(old, &committed)? else {
            return no("a node that the other side changed by more than splits");
        };
        let Some(ours) = self.splits(old, &new)? else {
            return no("a node that the transaction changed by more than splits");
        };
        if ours.is_empty() {
            return no("a node that the transaction did not split");
        }
        let in_committed: BTreeMap<Oid, Child> = committed
            .children
            .iter()
            .map(|child| (child.oid, *child))
            .collect();
        let mut merged = Vec::with_capacity(committed.children.len() + ours.len());
        let mut records = Vec::new();
        let mut rule = 2;
        for (index, anchor) in old.children.iter().enumerate() {
            merged.push(in_committed[&anchor.oid]);
            let added = theirs.get(&index);
            let Some(split) = ours.get(&index) else {
                merged.extend(added.into_iter().flatten().copied());
                continue;
            };
            for child in split {
                self.new_object(child.oid)?;
            }
            let Some((anchor_read, anchor_new)) = self.source.staged(anchor.oid)? else {
                return no("an anchor that the transaction did not store");
            };
            let anchor_current = self.current(anchor.oid)?;
            let changed = anchor_current.0 != anchor_read;
            let (x, y) = match (added.map(Vec::as_slice), &split[..]) {
                // Rule 2: the other side did not change the anchor.
                (None, _) if !changed => {
                    merged.extend(split.iter().copied());
                    continue;
                }
                // Rule 3: the other side changed the anchor without a split.
                (None, &[x]) => (x, None),
                // Rule 3: both sides split the anchor once.
                (Some(&[y]), &[x]) if changed => (x, Some(y)),
                _ => return no("two splits of one child on one side"),
            };
            rule = 3;
            let anchor = Anchor {
                child: *anchor,
                read: anchor_read,
                record: anchor_new,
                current: anchor_current,
            };
            let separator = match y {
                Some(y) => y.separator,
                None => x.separator,
            };
            let Some(separator) = separator else {
                return no("a new child without a separator");
            };
            records.extend(self.split_bucket(&anchor, x, y, separator)?);
            merged.push(Child {
                separator: Some(separator),
                ..x
            });
        }
        if node.current != node.read {
            let mut items = Vec::with_capacity(merged.len() * 2);
            for child in &merged {
                items.extend(child.separator);
                items.push(child.reference);
            }
            let children = self.heap.new_tuple(items);
            let state = self.heap.new_tuple(vec![children, committed.first]);
            let (keys, changed) = records.first().map_or((Vec::new(), 0), |record: &Written| {
                (record.keys.clone(), record.changed)
            });
            records.insert(
                0,
                Written {
                    oid: node.oid,
                    class: node.name.clone(),
                    read: node.read,
                    current: node.current,
                    pickle: self.pickle(node.class, state)?,
                    keys,
                    changed,
                },
            );
        }
        if records.is_empty() {
            return no("a node whose splits need no merge");
        }
        Ok(Split::Merged { rule, records })
    }

    /// Rule 3 on the bucket `anchor`, which the transaction split into the
    /// anchor and `x`, and which the other side split into the anchor and
    /// `y`, or changed without a split. Returns the records of the anchor
    /// and of `x`.
    fn split_bucket(
        &mut self,
        anchor: &Anchor,
        x: Child,
        y: Option<Child>,
        separator: Value,
    ) -> Step<[Written; 2]> {
        let x_record = self.new_object(x.oid)?;
        let Some(old_record) = self.source.at(anchor.child.oid, anchor.read)? else {
            return no("the record that the transaction read is gone");
        };
        let (a_class, a_name, a_new) = self.leaf(&anchor.record)?;
        let (x_class, x_name, x_new) = self.leaf(&x_record)?;
        let (_, _, a_old) = self.leaf(&old_record)?;
        let (_, _, a_committed) = self.leaf(&anchor.current.1)?;
        let next = a_old.next_oid;
        if a_new.next_oid != Some(x.oid) || x_new.next_oid != next {
            return no("next buckets that are not those of one split");
        }
        let mut committed = a_committed.items;
        match y {
            Some(y) => {
                let (_, y_record) = self.current(y.oid)?;
                let (_, _, y_leaf) = self.leaf(&y_record)?;
                if a_committed.next_oid != Some(y.oid) || y_leaf.next_oid != next {
                    return no("next buckets that are not those of one split");
                }
                committed.extend(y_leaf.items);
            }
            None => {
                if a_committed.next_oid != next {
                    return no("a next bucket that the other side changed");
                }
            }
        }
        let mut ours = a_new.items;
        ours.extend(x_new.items);
        let Cut {
            low,
            high,
            keys,
            changed,
        } = self.merge(&a_old.items, &committed, &ours, separator)?;
        let a_state = new_bucket_state(&mut self.heap, low, a_new.next);
        let x_state = new_bucket_state(&mut self.heap, high, x_new.next);
        Ok([
            Written {
                oid: anchor.child.oid,
                class: a_name,
                read: anchor.read,
                current: anchor.current.0,
                pickle: self.pickle(a_class, a_state)?,
                keys: keys.clone(),
                changed,
            },
            Written {
                oid: x.oid,
                class: x_name,
                read: Tid::ZERO,
                current: Tid::ZERO,
                pickle: self.pickle(x_class, x_state)?,
                keys,
                changed,
            },
        ])
    }

    /// Rule 1 on the node `oid`, which the transaction marked read-current
    /// at `read` and did not store.
    fn read(&mut self, oid: Oid, read: Tid) -> Step<Split> {
        let Some((current, committed)) = self.source.current(oid)? else {
            return no("a node without a current record");
        };
        if current == read {
            return no("a node that did not change");
        }
        self.node_family(&committed)?;
        let Some(old) = self.source.at(oid, read)? else {
            return no("the record that the transaction read is gone");
        };
        let (_, committed) = self.load(&committed)?;
        let (_, old) = self.load(&old)?;
        let (Tree::Node(old), Tree::Node(committed)) = (self.tree(old)?, self.tree(committed)?)
        else {
            return no("a node that holds its bucket inline");
        };
        let Some(theirs) = self.splits(&old, &committed)? else {
            return no("a node that the other side changed by more than splits");
        };
        let mut path = 0;
        for (index, child) in old.children.iter().enumerate() {
            if self.source.stores(child.oid) || self.source.read_current(child.oid) {
                if theirs.contains_key(&index) {
                    return no("a path child that the other side split");
                }
                path += 1;
            }
        }
        if path == 0 {
            return no("a node without a path child");
        }
        Ok(Split::Passed)
    }
}

/// The merged items of the range of a bucket, cut at a separator.
struct Cut {
    /// The items below the separator.
    low: Vec<Value>,
    /// The items at and above the separator.
    high: Vec<Value>,
    /// The keys that the new side changed, as text.
    keys: Vec<String>,
    /// The number of keys that the new side changed.
    changed: usize,
}

/// The node that rules 2 and 3 merge.
struct Merging {
    oid: Oid,
    /// The class of the node, as `module.name`.
    name: String,
    /// The class value of the node, from its new record.
    class: Value,
    /// The serial that the transaction read.
    read: Tid,
    /// The serial of the current record.
    current: Tid,
}

/// An anchor of a split of the transaction.
struct Anchor {
    child: Child,
    /// The serial that the transaction read.
    read: Tid,
    /// The record that the transaction stored.
    record: Vec<u8>,
    /// The serial and the pickle of the current record.
    current: (Tid, Vec<u8>),
}
