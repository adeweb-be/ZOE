//! The order merge of ADR 0052: the order list of a folder of plone.folder
//! and its positions change together.
//!
//! A folder of plone.folder keeps its order in its attribute annotations,
//! an `OOBTree` under `__annotations__`: the order list L, a
//! `persistent.list.PersistentList` whose state is `{'data': [id, ...]}`,
//! under `plone.folder.ordered.order`, and the positions P, an `OIBTree`
//! of id to index, under `plone.folder.ordered.pos`. A creation appends its
//! id to L and inserts it into P with the index `len(order) - 1`.
//!
//! [`plan`] decides one stale order L of a vote. It finds the folder among
//! the records that the transaction stored, through the annotations. It
//! merges only appends on both sides: the old list is a prefix of the
//! committed list and of the new list. The merged list is the committed
//! list, then the ids N that the transaction appended. It checks one
//! equation over the buckets of P that the transaction stored: the union
//! of their new states is the union of their old states plus exactly
//! `{N[i]: len(old) + i}`. A stored node of P must keep each child that the
//! transaction did not store.
//!
//! [`renumber`] then gives each id N[i] the index `len(committed) + i` in
//! the records of P that the vote stages, and checks its own result.
//!
//! The merge never guesses. A state that the model does not hold, a change
//! that is not an append, or a record that does not decode gives no
//! answer, and the conflict goes on as without the merge.

use std::collections::{BTreeMap, BTreeSet};

use zoe_pickle::Limits;
use zoe_pickle::btrees::{TreeState, bucket_state, new_bucket_state, new_tree_state, tree_state};
use zoe_pickle::python::{Heap, Object, Pickler, Value};

use super::{Reason, budget, tier1_class};
use crate::error::{Result, ZoeError};
use crate::types::{Oid, Tid};

/// The class of the order list, as `module.name`.
pub(crate) const LIST_CLASS: &str = "persistent.list.PersistentList";
/// The key of the order in the annotations of a folder.
const ORDER_KEY: &str = "plone.folder.ordered.order";
/// The key of the positions in the annotations of a folder.
const POS_KEY: &str = "plone.folder.ordered.pos";
/// The attribute of a folder that holds its annotations.
const ANNOTATIONS: &str = "__annotations__";
/// The class of the annotations.
const ANNOTATIONS_CLASS: (&str, &str) = ("BTrees.OOBTree", "OOBTree");
/// The class of the root and the nodes of the positions.
const POS_TREE: (&str, &str) = ("BTrees.OIBTree", "OIBTree");
/// The class of the buckets of the positions.
const POS_BUCKET: (&str, &str) = ("BTrees.OIBTree", "OIBucket");
/// The classes of the records of the positions, as `module.name`.
pub(crate) const POS_CLASSES: [&str; 2] = ["BTrees.OIBTree.OIBTree", "BTrees.OIBTree.OIBucket"];

/// Tells if `record` is of the class of the order list. Reads the class
/// pickle alone.
pub(crate) fn is_list(record: &[u8]) -> bool {
    zoe_pickle::decode_class_key(record)
        .ok()
        .flatten()
        .as_deref()
        == Some(LIST_CLASS)
}

/// Tells if the class pickle of `record` names `class`.
fn is_class(record: &[u8], class: (&str, &str)) -> bool {
    matches!(
        zoe_pickle::decode_class_name(record),
        Ok(Some((module, name))) if module == class.0 && name == class.1
    )
}

/// The records that the merge reads.
pub(crate) trait Source {
    /// Returns the serial that the transaction read and the pickle that it
    /// stored for `oid`, or `None` when it stored none, restored it or
    /// deleted it.
    fn staged(&self, oid: Oid) -> Result<Option<(Tid, Vec<u8>)>>;

    /// Returns the serial and the pickle of the current record of `oid`, or
    /// `None` when it has none or is deleted.
    fn current(&self, oid: Oid) -> Result<Option<(Tid, Vec<u8>)>>;

    /// Returns the pickle of the record of `oid` at `serial`.
    fn at(&self, oid: Oid, serial: Tid) -> Result<Option<Vec<u8>>>;

    /// Returns the serial at which the transaction marked `oid`
    /// read-current, if it did.
    fn read_current(&self, oid: Oid) -> Option<Tid>;

    /// Returns the oids that the transaction stored, in order.
    fn stored(&self) -> Vec<Oid>;
}

/// The merge of one order, before the renumbering.
#[derive(Clone, Debug)]
pub(crate) struct Order {
    /// The oid of the order list.
    pub(crate) list: Oid,
    /// The serial that the transaction read of the list.
    pub(crate) read: Tid,
    /// The serial of the current record of the list.
    pub(crate) current: Tid,
    /// The merged record of the list.
    pub(crate) pickle: Vec<u8>,
    /// The ids that the transaction appended, in order.
    pub(crate) ids: Vec<String>,
    /// The length of the committed list: the index of the first id of
    /// `ids` in the merged list.
    pub(crate) start: usize,
    /// The oids of the records of the positions: the root, the nodes on
    /// the path of the transaction and their children in each state.
    pub(crate) positions: BTreeSet<Oid>,
    /// The records of the positions that the transaction stored and that
    /// hold values: the buckets, and the root when it holds its bucket
    /// inline.
    pub(crate) stored: BTreeSet<Oid>,
}

/// What the merge decides for one stale list.
#[derive(Clone, Debug)]
pub(crate) enum Planned {
    /// No record that the transaction stored names the list as the order
    /// of a folder.
    NotOrder,
    /// The list is the order of a folder, and the merge goes on.
    Merge(Order),
    /// The list is the order of a folder, and both sides added an id.
    Refused { reason: Reason, ids: Vec<String> },
    /// The list is the order of a folder, and the merge gives no answer.
    Undecided { why: &'static str, ids: Vec<String> },
}

/// Why a step of the merge stops.
enum Stop {
    /// No answer, for this reason.
    No(&'static str),
    /// An error of the store.
    Store(ZoeError),
}

impl From<ZoeError> for Stop {
    fn from(error: ZoeError) -> Self {
        Stop::Store(error)
    }
}

type Step<T> = std::result::Result<T, Stop>;

fn no<T>(why: &'static str) -> Step<T> {
    Err(Stop::No(why))
}

/// A heap that loads records within the step budget of their bytes.
struct Reader {
    heap: Heap,
    len: usize,
}

impl Reader {
    fn new() -> Self {
        Reader {
            heap: Heap::with_instances(),
            len: 0,
        }
    }

    /// Decodes and loads `record`: its class and its state.
    fn load(&mut self, record: &[u8]) -> Step<(Value, Value)> {
        let limits = Limits {
            node_budget: record.len(),
        };
        let Ok(tree) = zoe_pickle::decode_record_with(record, limits) else {
            return no("a record that does not decode");
        };
        self.len = self.len.saturating_add(record.len());
        self.heap.set_budget(budget(self.len));
        self.heap
            .load(&tree)
            .map_err(|unsupported| Stop::No(unsupported.0))
    }

    /// Returns the oid of a persistent reference to an object of the same
    /// database.
    fn oid(&self, value: Value) -> Step<Oid> {
        let Some(reference) = self.heap.reference(value) else {
            return no("a value that is not a persistent reference");
        };
        if reference.database.is_some() || reference.weak {
            return no("a reference to another database, or a weak reference");
        }
        let Ok(oid) = <[u8; 8]>::try_from(&reference.oid[..]) else {
            return no("an oid that is not 8 bytes");
        };
        Ok(Oid(u64::from_be_bytes(oid)))
    }

    /// Returns the value of the str key `key` of the dict `state`.
    fn field(&self, state: Value, key: &str) -> Option<Value> {
        self.heap
            .dict(state)?
            .iter()
            .find(|(name, _)| self.heap.str(*name) == Some(key))
            .map(|&(_, value)| value)
    }

    /// Returns the items of a bucket state of the positions, as id and
    /// index.
    fn items(&self, bucket: Value) -> Step<Vec<(String, i128)>> {
        let Some(state) = bucket_state(&self.heap, bucket, true) else {
            return no("a bucket state of another shape");
        };
        let mut items = Vec::with_capacity(state.items.len() / 2);
        for pair in state.items.chunks(2) {
            let (Some(id), Value::Int(index)) = (self.heap.str(pair[0]), pair[1]) else {
                return no("positions that are not of str to int");
            };
            items.push((id.to_owned(), index));
        }
        Ok(items)
    }
}

/// Reads the ids of a record of the order list: `{'data': [str, ...]}`.
fn list_ids(record: &[u8]) -> Step<Vec<String>> {
    let mut reader = Reader::new();
    let (_, state) = reader.load(record)?;
    let Some(items) = reader.heap.dict(state) else {
        return no("a list state that is not a dict");
    };
    let [(key, data)] = items else {
        return no("a list state with another key than data");
    };
    if reader.heap.str(*key) != Some("data") {
        return no("a list state with another key than data");
    }
    let Some(Object::List(values)) = reader.heap.object(*data) else {
        return no("a list state whose data is not a list");
    };
    values
        .iter()
        .map(|&value| match reader.heap.str(value) {
            Some(id) => Ok(id.to_owned()),
            None => no("an item of the order that is not a str"),
        })
        .collect()
}

/// Returns the merged record of the list: the class pickle of `new` and
/// `{'data': ids}`.
fn list_record(new: &[u8], ids: &[String]) -> Step<Vec<u8>> {
    let mut reader = Reader::new();
    let (class, state) = reader.load(new)?;
    let Some(&[(key, _)]) = reader.heap.dict(state) else {
        return no("a list state with another key than data");
    };
    let items = ids.iter().map(|id| reader.heap.new_str(id)).collect();
    let list = reader.heap.new_list(items);
    let state = reader.heap.new_dict(vec![(key, list)]);
    pickle(&reader.heap, class, state)
}

/// Returns the pickle of `class` and `state`.
fn pickle(heap: &Heap, class: Value, state: Value) -> Step<Vec<u8>> {
    let mut pickler = Pickler::new(heap);
    match pickler.dump(class).and_then(|()| pickler.dump(state)) {
        Ok(()) => Ok(pickler.into_bytes()),
        Err(unsupported) => no(unsupported.0),
    }
}

/// Decides the stale order `list` of a transaction. The transaction stored
/// it and read it at a serial that is not the current one.
pub(crate) fn plan(source: &dyn Source, list: Oid) -> Result<Planned> {
    let positions = match find_folder(source, list) {
        Ok(Some(positions)) => positions,
        Ok(None) => return Ok(Planned::NotOrder),
        Err(Stop::Store(error)) => return Err(error),
        Err(Stop::No(why)) => {
            return Ok(Planned::Undecided {
                why,
                ids: Vec::new(),
            });
        }
    };
    match merge(source, list, positions) {
        Ok(planned) => Ok(planned),
        Err(Stop::Store(error)) => Err(error),
        Err(Stop::No(why)) => Ok(Planned::Undecided {
            why,
            ids: Vec::new(),
        }),
    }
}

/// Finds the folder whose annotations name `list` as its order, among the
/// records that the transaction stored. Returns the oid of its positions,
/// or `None` when no stored record names the list.
fn find_folder(source: &dyn Source, list: Oid) -> Step<Option<Oid>> {
    for oid in source.stored() {
        if oid == list {
            continue;
        }
        let Some((_, record)) = source.staged(oid)? else {
            continue;
        };
        if tier1_class(&record).is_some() || is_list(&record) {
            continue;
        }
        // A record whose bytes do not hold the name of the attribute does
        // not name annotations: the pickler writes a str once in a record,
        // and the memo refers to it after. So only such a record decodes.
        if !holds(&record, ANNOTATIONS.as_bytes()) {
            continue;
        }
        let Some(annotations) = annotations_of(&record) else {
            continue;
        };
        let Some((_, current)) = source.current(annotations)? else {
            continue;
        };
        let Some((order, pos)) = names(&current) else {
            continue;
        };
        if order != list {
            continue;
        }
        // The transaction can store the annotations too: both records
        // must name the same order and the same positions.
        if let Some((_, staged)) = source.staged(annotations)?
            && names(&staged) != Some((order, pos))
        {
            return no("annotations of the transaction that name another order");
        }
        return Ok(Some(pos));
    }
    Ok(None)
}

/// Tells if `bytes` holds `part`.
fn holds(bytes: &[u8], part: &[u8]) -> bool {
    bytes.windows(part.len()).any(|window| window == part)
}

/// Returns the oid of the annotations that the state of `record` names
/// under `__annotations__`, or `None`.
fn annotations_of(record: &[u8]) -> Option<Oid> {
    let mut reader = Reader::new();
    let (_, state) = reader.load(record).ok()?;
    let value = reader.field(state, ANNOTATIONS)?;
    reader.oid(value).ok()
}

/// Returns the oids of the order and of the positions that the record of
/// annotations `record` names, or `None` when it names no order, or is not
/// an `OOBTree` that holds its bucket inline.
fn names(record: &[u8]) -> Option<(Oid, Oid)> {
    if !is_class(record, ANNOTATIONS_CLASS) {
        return None;
    }
    let mut reader = Reader::new();
    let (_, state) = reader.load(record).ok()?;
    let Some(TreeState::Inline(bucket)) = tree_state(&reader.heap, state) else {
        return None;
    };
    let items = bucket_state(&reader.heap, bucket, true)?.items;
    let mut order = None;
    let mut pos = None;
    for pair in items.chunks(2) {
        match reader.heap.str(pair[0]) {
            Some(ORDER_KEY) => order = Some(reader.oid(pair[1]).ok()?),
            Some(POS_KEY) => pos = Some(reader.oid(pair[1]).ok()?),
            _ => {}
        }
    }
    Some((order?, pos?))
}

/// Merges the list and checks the changes of the positions.
fn merge(source: &dyn Source, list: Oid, pos: Oid) -> Step<Planned> {
    let Some((read, new)) = source.staged(list)? else {
        return no("an order that the transaction did not store");
    };
    let Some((current, committed)) = source.current(list)? else {
        return no("an order without a current record");
    };
    let Some(old) = source.at(list, read)? else {
        return no("the record of the order that the transaction read is gone");
    };
    if !(is_list(&old) && is_list(&committed) && is_list(&new)) {
        return no("a record of the order of another class");
    }
    let o = list_ids(&old)?;
    let c = list_ids(&committed)?;
    let n = list_ids(&new)?;
    if !c.starts_with(&o) || !n.starts_with(&o) {
        return no("an order that is not appends on both sides");
    }
    let ids = n[o.len()..].to_vec();
    if ids.is_empty() {
        return no("a transaction that appended nothing");
    }
    let held: BTreeSet<&String> = o.iter().collect();
    let theirs: BTreeSet<&String> = c[o.len()..].iter().collect();
    let ours: BTreeSet<&String> = ids.iter().collect();
    if held.len() != o.len() || theirs.len() != c.len() - o.len() || ours.len() != ids.len() {
        return no("an id twice in the order");
    }
    if ours.iter().chain(theirs.iter()).any(|id| held.contains(id)) {
        return no("an appended id that the old order holds");
    }
    if ours.iter().any(|id| theirs.contains(id)) {
        return Ok(Planned::Refused {
            reason: Reason::OrderSameId,
            ids,
        });
    }
    let (positions, stored) = check_positions(source, pos, o.len(), &ids)?;
    let mut merged = c.clone();
    merged.extend(ids.iter().cloned());
    Ok(Planned::Merge(Order {
        list,
        read,
        current,
        pickle: list_record(&new, &merged)?,
        start: c.len(),
        ids,
        positions,
        stored,
    }))
}

/// The states of a record of the positions.
struct States {
    /// True when the transaction stored the record.
    stored: bool,
    old: Option<Value>,
    new: Option<Value>,
    committed: Option<Value>,
}

/// Checks the equation and the node check of ADR 0052 (section 2, step 4)
/// on the positions `pos`, for the ids `ids` that the transaction appended
/// to an order of `len` ids. Returns the oids of the records of the
/// positions, and the records that the transaction stored and that hold
/// values.
fn check_positions(
    source: &dyn Source,
    pos: Oid,
    len: usize,
    ids: &[String],
) -> Step<(BTreeSet<Oid>, BTreeSet<Oid>)> {
    let mut reader = Reader::new();
    let mut positions = BTreeSet::from([pos]);
    let mut stored = BTreeSet::new();
    let mut new_items: Vec<(String, i128)> = Vec::new();
    let mut old_items: Vec<(String, i128)> = Vec::new();
    let mut todo = vec![(pos, true)];
    let mut visited = BTreeSet::new();
    while let Some((oid, root)) = todo.pop() {
        if !visited.insert(oid) {
            continue;
        }
        let states = node_states(source, &mut reader, oid, root)?;
        let mut children = BTreeMap::new();
        let mut old_children = Vec::new();
        let mut new_children = BTreeSet::new();
        for (which, state) in [(0, states.old), (1, states.new), (2, states.committed)] {
            let Some(state) = state else {
                continue;
            };
            match tree_state(&reader.heap, state) {
                Some(TreeState::Inline(bucket)) if root => {
                    // Only a root holds its bucket inline: the root is then
                    // a record with values.
                    match which {
                        0 => old_items.extend(reader.items(bucket)?),
                        1 => new_items.extend(reader.items(bucket)?),
                        _ => {}
                    }
                    if which < 2 && states.stored {
                        stored.insert(oid);
                    }
                }
                Some(TreeState::Split) => {
                    for child in node_children(&reader, state)? {
                        children.insert(child, ());
                        match which {
                            0 => old_children.push(child),
                            1 => {
                                new_children.insert(child);
                            }
                            _ => {}
                        }
                    }
                }
                Some(TreeState::Empty) => {}
                _ => return no("a state of the positions of another shape"),
            }
        }
        // A stored node keeps each child that the transaction did not
        // store: else the keys of that child leave the tree unseen.
        if states.stored && states.new.is_some() {
            for child in &old_children {
                if !new_children.contains(child) && source.staged(*child)?.is_none() {
                    return no("a node of the positions that drops a child");
                }
            }
        }
        for child in children.into_keys() {
            positions.insert(child);
            if let Some((serial, record)) = source.staged(child)? {
                if is_class(&record, POS_TREE) {
                    todo.push((child, false));
                } else if is_class(&record, POS_BUCKET) {
                    stored.insert(child);
                    let (_, state) = reader.load(&record)?;
                    new_items.extend(reader.items(state)?);
                    if serial != Tid::ZERO {
                        let Some(old) = source.at(child, serial)? else {
                            return no("a bucket of the positions whose read record is gone");
                        };
                        if !is_class(&old, POS_BUCKET) {
                            return no("a bucket of the positions of another class");
                        }
                        let (_, state) = reader.load(&old)?;
                        old_items.extend(reader.items(state)?);
                    }
                } else {
                    return no("a record of the positions of another class");
                }
            } else if let Some(read) = source.read_current(child) {
                let Some(record) = source.at(child, read)? else {
                    return no("a node of the positions whose read record is gone");
                };
                if is_class(&record, POS_TREE) {
                    todo.push((child, false));
                }
            }
        }
    }
    // The equation: the new states are the old states plus exactly the
    // ids of the transaction at their old positions.
    let old_map = unique(old_items)?;
    let new_map = unique(new_items)?;
    if new_map.len() != old_map.len() + ids.len() {
        return no("positions that changed by more than the appends");
    }
    for (id, index) in &old_map {
        if new_map.get(id) != Some(index) {
            return no("positions that changed by more than the appends");
        }
    }
    for (offset, id) in ids.iter().enumerate() {
        if old_map.contains_key(id) || new_map.get(id) != Some(&((len + offset) as i128)) {
            return no("positions that changed by more than the appends");
        }
    }
    Ok((positions, stored))
}

/// Returns the items as a map, or no answer when a key comes twice.
fn unique(items: Vec<(String, i128)>) -> Step<BTreeMap<String, i128>> {
    let len = items.len();
    let map: BTreeMap<String, i128> = items.into_iter().collect();
    if map.len() != len {
        return no("an id twice in the positions");
    }
    Ok(map)
}

/// Reads the states of the node `oid` of the positions: the old one, the
/// new one when the transaction stored it, and the committed one.
fn node_states(source: &dyn Source, reader: &mut Reader, oid: Oid, root: bool) -> Step<States> {
    let mut load = |record: &[u8]| -> Step<Value> {
        if !is_class(record, POS_TREE) {
            return no("positions that are not an OIBTree");
        }
        Ok(reader.load(record)?.1)
    };
    let (stored, old, new) = match source.staged(oid)? {
        Some((serial, record)) => {
            let new = load(&record)?;
            let old = match serial {
                Tid::ZERO if root => return no("positions that the transaction created"),
                Tid::ZERO => None,
                serial => match source.at(oid, serial)? {
                    Some(old) => Some(load(&old)?),
                    None => return no("a node of the positions whose read record is gone"),
                },
            };
            (true, old, Some(new))
        }
        None => match source.read_current(oid) {
            Some(read) => match source.at(oid, read)? {
                Some(old) => (false, Some(load(&old)?), None),
                None => return no("a node of the positions whose read record is gone"),
            },
            None => return no("positions that the transaction did not reach"),
        },
    };
    let committed = match source.current(oid)? {
        Some((_, record)) => Some(load(&record)?),
        None => None,
    };
    Ok(States {
        stored,
        old,
        new,
        committed,
    })
}

/// Returns the oids of the children of a node state of the positions.
fn node_children(reader: &Reader, state: Value) -> Step<Vec<Oid>> {
    let Some(&[children, _first]) = reader.heap.tuple(state) else {
        return no("a node state of another shape");
    };
    let Some(items) = reader.heap.tuple(children) else {
        return no("a node state of another shape");
    };
    items
        .iter()
        .step_by(2)
        .map(|&child| reader.oid(child))
        .collect()
}

/// A record of the positions that the renumbering changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Renumbered {
    pub(crate) oid: Oid,
    pub(crate) pickle: Vec<u8>,
    /// The ids that the record holds, which the renumbering changed.
    pub(crate) ids: Vec<String>,
}

/// Gives each id N[i] of `order` the index `start + i` in `records`, the
/// records of the positions that the vote stages. Returns the records that
/// it changed. Each id must be in exactly one record; the result is read
/// back and checked (the self-check of ADR 0052, section 2, step 7).
pub(crate) fn renumber(
    order: &Order,
    records: &[(Oid, Vec<u8>)],
) -> std::result::Result<Vec<Renumbered>, &'static str> {
    let mut target = BTreeMap::new();
    for (offset, id) in order.ids.iter().enumerate() {
        let index = i32::try_from(order.start + offset).map_err(|_| "a position too large")?;
        target.insert(id.as_str(), i128::from(index));
    }
    let mut found = BTreeMap::new();
    let mut changed = Vec::new();
    for (oid, record) in records {
        match renumber_record(record, &target) {
            Ok(Some((pickle, ids))) => {
                for id in &ids {
                    *found.entry(id.clone()).or_insert(0) += 1;
                }
                changed.push(Renumbered {
                    oid: *oid,
                    pickle,
                    ids,
                });
            }
            Ok(None) => {}
            Err(Stop::No(why)) => return Err(why),
            Err(Stop::Store(_)) => return Err("a record of the positions that does not read"),
        }
    }
    if found.len() != target.len() || found.values().any(|&count| count != 1) {
        return Err("an id of the transaction that is not once in the positions");
    }
    self_check(&changed, &target, records)?;
    Ok(changed)
}

/// Returns the record with the new indexes of the ids of `target`, and
/// those ids, or `None` when it holds none of them. A node holds no
/// values.
fn renumber_record(
    record: &[u8],
    target: &BTreeMap<&str, i128>,
) -> Step<Option<(Vec<u8>, Vec<String>)>> {
    let mut reader = Reader::new();
    let bucket = is_class(record, POS_BUCKET);
    if !bucket && !is_class(record, POS_TREE) {
        return no("a record of the positions of another class");
    }
    let (class, state) = reader.load(record)?;
    let bucket_value = if bucket {
        state
    } else {
        match tree_state(&reader.heap, state) {
            Some(TreeState::Inline(inline)) => inline,
            Some(TreeState::Split | TreeState::Empty) => return Ok(None),
            None => return no("a state of the positions of another shape"),
        }
    };
    let Some(parts) = bucket_state(&reader.heap, bucket_value, true) else {
        return no("a bucket state of another shape");
    };
    let mut items = parts.items;
    let mut ids = Vec::new();
    for pair in items.chunks_mut(2) {
        let Some(id) = reader.heap.str(pair[0]) else {
            return no("positions that are not of str to int");
        };
        if let Some(&index) = target.get(id) {
            ids.push(id.to_owned());
            pair[1] = Value::Int(index);
        }
    }
    if ids.is_empty() {
        return Ok(None);
    }
    let new_bucket = new_bucket_state(&mut reader.heap, items, parts.next);
    let state = if bucket {
        new_bucket
    } else {
        new_tree_state(&mut reader.heap, new_bucket)
    };
    Ok(Some((pickle(&reader.heap, class, state)?, ids)))
}

/// Reads the changed records back: each id of `target` once, with its
/// index, and every other item as in the record before the renumbering.
fn self_check(
    changed: &[Renumbered],
    target: &BTreeMap<&str, i128>,
    records: &[(Oid, Vec<u8>)],
) -> std::result::Result<(), &'static str> {
    const FAILED: &str = "a self-check that failed";
    let read = |record: &[u8]| -> Option<Vec<(String, i128)>> {
        let mut reader = Reader::new();
        let (_, state) = reader.load(record).ok()?;
        let bucket = match tree_state(&reader.heap, state) {
            Some(TreeState::Inline(inline)) if is_class(record, POS_TREE) => inline,
            _ => state,
        };
        reader.items(bucket).ok()
    };
    let mut seen = 0;
    for renumbered in changed {
        let before = records
            .iter()
            .find(|(oid, _)| *oid == renumbered.oid)
            .and_then(|(_, record)| read(record))
            .ok_or(FAILED)?;
        let after = read(&renumbered.pickle).ok_or(FAILED)?;
        if before.len() != after.len() {
            return Err(FAILED);
        }
        for ((id, old), (again, new)) in before.iter().zip(&after) {
            if id != again {
                return Err(FAILED);
            }
            match target.get(id.as_str()) {
                Some(&index) if *new == index => seen += 1,
                Some(_) => return Err(FAILED),
                None if new == old => {}
                None => return Err(FAILED),
            }
        }
    }
    if seen != target.len() {
        return Err(FAILED);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::record;

    fn list(items: &[&str]) -> Vec<u8> {
        let mut heap = Heap::new();
        let class = heap.new_class("persistent.list", "PersistentList");
        let values = items.iter().map(|item| heap.new_str(item)).collect();
        let data = heap.new_list(values);
        let key = heap.new_str("data");
        let state = heap.new_dict(vec![(key, data)]);
        record(&heap, class, state)
    }

    fn inline(items: &[(&str, i128)]) -> Vec<u8> {
        let mut heap = Heap::new();
        let class = heap.new_class("BTrees.OIBTree", "OIBTree");
        let mut values = Vec::new();
        for (id, index) in items {
            values.push(heap.new_str(id));
            values.push(Value::Int(*index));
        }
        let bucket = new_bucket_state(&mut heap, values, None);
        let state = new_tree_state(&mut heap, bucket);
        record(&heap, class, state)
    }

    fn order(ids: &[&str], start: usize) -> Order {
        Order {
            list: Oid(1),
            read: Tid(1),
            current: Tid(2),
            pickle: Vec::new(),
            ids: ids.iter().map(|id| (*id).to_owned()).collect(),
            start,
            positions: BTreeSet::from([Oid(2)]),
            stored: BTreeSet::from([Oid(2)]),
        }
    }

    fn items_of(record: &[u8]) -> Vec<(String, i128)> {
        let mut reader = Reader::new();
        let (_, state) = reader.load(record).ok().unwrap();
        let Some(TreeState::Inline(bucket)) = tree_state(&reader.heap, state) else {
            panic!("an inline tree")
        };
        reader.items(bucket).ok().unwrap()
    }

    #[test]
    fn the_ids_of_an_order_decode_and_another_item_gives_no_answer() {
        assert_eq!(list_ids(&list(&["a", "b"])).ok().unwrap(), ["a", "b"]);
        let mut heap = Heap::new();
        let class = heap.new_class("persistent.list", "PersistentList");
        let data = heap.new_list(vec![Value::Int(1)]);
        let key = heap.new_str("data");
        let state = heap.new_dict(vec![(key, data)]);
        assert!(matches!(
            list_ids(&record(&heap, class, state)),
            Err(Stop::No("an item of the order that is not a str"))
        ));
        assert!(is_list(&list(&[])));
    }

    #[test]
    fn the_renumbering_gives_each_id_its_merged_index() {
        // The old order held one id; each side appended one id at index 1.
        let merged = inline(&[("a", 0), ("b", 1), ("c", 1)]);
        let renumbered = renumber(&order(&["c"], 2), &[(Oid(2), merged)]).unwrap();
        assert_eq!(renumbered.len(), 1);
        assert_eq!(renumbered[0].ids, ["c"]);
        assert_eq!(
            items_of(&renumbered[0].pickle),
            [
                ("a".to_owned(), 0),
                ("b".to_owned(), 1),
                ("c".to_owned(), 2)
            ]
        );
    }

    #[test]
    fn an_id_that_no_record_holds_gives_no_answer() {
        let merged = inline(&[("a", 0), ("b", 1)]);
        assert_eq!(
            renumber(&order(&["c"], 2), &[(Oid(2), merged)]),
            Err("an id of the transaction that is not once in the positions")
        );
    }

    #[test]
    fn an_id_in_two_records_gives_no_answer() {
        let one = inline(&[("c", 1)]);
        let two = inline(&[("c", 1)]);
        assert_eq!(
            renumber(&order(&["c"], 2), &[(Oid(2), one), (Oid(3), two)]),
            Err("an id of the transaction that is not once in the positions")
        );
    }

    #[test]
    fn a_key_twice_in_a_union_gives_no_answer() {
        let items = vec![("a".to_owned(), 0), ("a".to_owned(), 1)];
        assert!(matches!(
            unique(items),
            Err(Stop::No("an id twice in the positions"))
        ));
    }
}
