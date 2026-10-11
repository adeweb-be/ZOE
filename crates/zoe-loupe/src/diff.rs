//! The diff of the decoded states of two records.
//!
//! The diff walks the JSON states of the two records, as `state` writes
//! them, and the decoded trees under them at the same time. It goes into a
//! dict with text keys one key at a time, and into a tuple or a list one
//! item at a time. It compares each other pair of values in the trees, and
//! gives one change for each pair that is not equal. A key or an item that
//! only one state has is also a change.
//!
//! Two values are equal when Python finds them equal, with the type of each
//! value counted:
//!
//! - A dict compares by key, so the order of its keys is not a change. When
//!   a key occurs twice, the last value counts, as in Python. A set compares
//!   by item.
//! - Memo puts and gets, and the opcode that writes a value, are not
//!   changes. Only the values count.
//! - A reference compares by oid, not by the object that it leads to.
//! - The type of a value counts: `1`, `1.0` and `True` are three values, and
//!   a tuple is not a list. An int beyond 128 bits that two opcodes write in
//!   two forms is a change.
//! - An opaque node compares by its bytes, because the tree does not model
//!   it.
//!
//! A change has the JSON pointer of the value in the two states, and the
//! JSON of the value before and after it. A side that does not have the
//! value has no JSON.
//!
//! The JSON states have the limits of `state`. The comparison reads the
//! trees, so a change inside a value that the JSON cuts or leaves out still
//! shows, as a change of that value. The diff has limits too:
//!
//! - A pointer of a change has at most `MAX_POINTER` bytes. A dict or a list
//!   with a longer pointer below it compares as one value.
//! - The pointers of the changes take at most `POINTER_BUDGET` bytes in all.
//!   Past that, the diff is one change of the whole state.
//! - The comparisons of one diff take at most `WORK_BUDGET` steps. Past that,
//!   two values compare by their JSON, as the pane shows them.

use std::collections::{HashMap, HashSet};

use serde::Serialize;
use serde_json::{Map, Value};
use zoe_pickle::{Float, Int, Node, Reduce, Resolver, Tree, Unicode};

use crate::state::{
    self, MAX_POINTER, POINTER_BUDGET, dict_entries, list_items, push_segment, target,
};
use crate::text::{self, IntValue};

/// The most steps that the comparisons of one diff take. A step is one
/// pair of nodes, or one byte of a value that a comparison reads.
const WORK_BUDGET: usize = 1 << 26;

/// One change between two states of an object.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Change {
    /// The JSON pointer of the value in the two states. The whole state is
    /// `""`.
    pub path: String,
    /// The value before, as JSON. It is `None` when the state before does
    /// not have the value, and JSON then leaves the field out.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<Value>,
    /// The value after, as JSON. It is `None` when the state after does
    /// not have the value, and JSON then leaves the field out.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<Value>,
}

/// Return the changes from the state of `before` to the state of `after`.
pub(crate) fn diff(before: &Tree, after: &Tree) -> Vec<Change> {
    diff_within(before, after, WORK_BUDGET)
}

/// Return the changes from the state of `before` to the state of `after`.
/// The comparisons take at most `work` steps.
fn diff_within<'t>(before: &'t Tree, after: &'t Tree, work: usize) -> Vec<Change> {
    let (a, b) = (before.resolver(), after.resolver());
    let (json_a, _) = state::convert(&a, before.state());
    let (json_b, _) = state::convert(&b, after.state());
    let mut differ = Differ {
        a: &a,
        b: &b,
        work,
        pointers: POINTER_BUDGET,
        overflow: false,
        compared: HashMap::new(),
        changes: Vec::new(),
    };
    differ.walk(
        Side {
            json: &json_a,
            node: before.state(),
        },
        Side {
            json: &json_b,
            node: after.state(),
        },
        &mut String::new(),
    );
    if differ.overflow {
        return vec![Change {
            path: String::new(),
            before: Some(json_a),
            after: Some(json_b),
        }];
    }
    differ.changes
}

/// A value in one state: its JSON and its node in the tree.
#[derive(Clone, Copy)]
struct Side<'j, 't> {
    json: &'j Value,
    node: &'t Node,
}

struct Differ<'r, 't> {
    /// The resolver of the tree before.
    a: &'r Resolver<'t>,
    /// The resolver of the tree after.
    b: &'r Resolver<'t>,
    /// The steps that the comparisons can still take.
    work: usize,
    /// The bytes of pointers that the changes can still take.
    pointers: usize,
    /// True when the changes need more than the pointer budget.
    overflow: bool,
    /// The result of each comparison, by the addresses of the two nodes.
    compared: HashMap<(usize, usize), bool>,
    changes: Vec<Change>,
}

impl<'t> Differ<'_, 't> {
    /// Compare the value `a` before with the value `b` after. `at` is the
    /// JSON pointer of the two values.
    fn walk(&mut self, a: Side<'_, 't>, b: Side<'_, 't>, at: &mut String) {
        if self.overflow {
            return;
        }
        match (
            a.json,
            b.json,
            self.a.resolve(a.node),
            self.b.resolve(b.node),
        ) {
            (
                Value::Object(x),
                Value::Object(y),
                Some(na @ Node::Dict(_)),
                Some(nb @ Node::Dict(_)),
            ) if plain(x) && plain(y) && fits(at, x.keys().chain(y.keys())) => {
                return self.dict((x, na), (y, nb), at);
            }
            (Value::Array(x), Value::Array(y), Some(na), Some(nb))
                if matches!(
                    (na, nb),
                    (Node::Tuple(_), Node::Tuple(_)) | (Node::List(_), Node::List(_))
                ) && fits(
                    at,
                    [x.len().max(y.len()).saturating_sub(1).to_string()].iter(),
                ) =>
            {
                return self.items((x, na), (y, nb), at);
            }
            _ => {}
        }
        if !self.same(a, b) {
            self.change(at, Some(a.json), Some(b.json));
        }
    }

    /// Compare two dicts with text keys, key by key.
    fn dict(
        &mut self,
        (x, na): (&Map<String, Value>, &'t Node),
        (y, nb): (&Map<String, Value>, &'t Node),
        at: &mut String,
    ) {
        let (values_a, values_b) = (values(self.a, na), values(self.b, nb));
        let len = at.len();
        for (key, json) in x {
            push_segment(at, key);
            match (y.get(key), values_a.get(key), values_b.get(key)) {
                (Some(other), Some(&node_a), Some(&node_b)) => self.walk(
                    Side { json, node: node_a },
                    Side {
                        json: other,
                        node: node_b,
                    },
                    at,
                ),
                (Some(other), _, _) => {
                    if json != other {
                        self.change(at, Some(json), Some(other));
                    }
                }
                (None, _, _) => self.change(at, Some(json), None),
            }
            at.truncate(len);
        }
        for (key, json) in y {
            if !x.contains_key(key) {
                push_segment(at, key);
                self.change(at, None, Some(json));
                at.truncate(len);
            }
        }
    }

    /// Compare two tuples or two lists, item by item.
    fn items(
        &mut self,
        (x, na): (&[Value], &'t Node),
        (y, nb): (&[Value], &'t Node),
        at: &mut String,
    ) {
        let (items_a, items_b) = (sequence_items(na), sequence_items(nb));
        let len = at.len();
        for index in 0..x.len().max(y.len()) {
            push_segment(at, &index.to_string());
            match (x.get(index), y.get(index)) {
                (Some(json_a), Some(json_b)) => match (items_a.get(index), items_b.get(index)) {
                    (Some(&node_a), Some(&node_b)) => self.walk(
                        Side {
                            json: json_a,
                            node: node_a,
                        },
                        Side {
                            json: json_b,
                            node: node_b,
                        },
                        at,
                    ),
                    _ => {
                        if json_a != json_b {
                            self.change(at, Some(json_a), Some(json_b));
                        }
                    }
                },
                (Some(json_a), None) => self.change(at, Some(json_a), None),
                (None, Some(json_b)) => self.change(at, None, Some(json_b)),
                (None, None) => {}
            }
            at.truncate(len);
        }
    }

    /// Add a change at `at`, when the pointer budget pays for its pointer.
    fn change(&mut self, at: &str, before: Option<&Value>, after: Option<&Value>) {
        match self.pointers.checked_sub(at.len()) {
            Some(left) => {
                self.pointers = left;
                self.changes.push(Change {
                    path: at.to_owned(),
                    before: before.cloned(),
                    after: after.cloned(),
                });
            }
            None => self.overflow = true,
        }
    }

    /// Tell if two values are equal. Compare the trees, or the JSON when
    /// the work budget is spent.
    fn same(&mut self, a: Side<'_, 't>, b: Side<'_, 't>) -> bool {
        let node_a = self.a.resolve(a.node).unwrap_or(a.node.value());
        let node_b = self.b.resolve(b.node).unwrap_or(b.node.value());
        let key = (address(node_a), address(node_b));
        if let Some(&known) = self.compared.get(&key) {
            return known;
        }
        let equal = self
            .equal(a.node, b.node)
            .unwrap_or_else(|| a.json == b.json);
        self.compared.insert(key, equal);
        equal
    }

    /// Take `steps` from the work budget. Return `None` when the budget
    /// cannot pay for them.
    fn spend(&mut self, steps: usize) -> Option<()> {
        self.work = self.work.checked_sub(steps)?;
        Some(())
    }

    /// Tell if the values of the nodes `a` and `b` are equal. Return `None`
    /// when the work budget runs out.
    ///
    /// The comparison keeps a stack of the pairs that it must compare, so a
    /// deep value does not recurse. A pair that it met before needs no
    /// second look: it is equal, or its comparison is still open, as in a
    /// cycle.
    fn equal(&mut self, a: &'t Node, b: &'t Node) -> Option<bool> {
        let mut pending = vec![(a, b)];
        let mut seen = HashSet::new();
        while let Some((a, b)) = pending.pop() {
            self.spend(1)?;
            let (a, b) = match (self.a.resolve(a), self.b.resolve(b)) {
                (Some(a), Some(b)) => (a, b),
                // The value of a get is in an opaque node. Only its memo
                // index is known.
                (None, None) => match (a.value(), b.value()) {
                    (Node::Get(x), Node::Get(y)) if x.index == y.index => continue,
                    _ => return Some(false),
                },
                _ => return Some(false),
            };
            if seen.insert((address(a), address(b))) && !self.shallow(a, b, &mut pending)? {
                return Some(false);
            }
        }
        Some(true)
    }

    /// Compare the nodes `a` and `b` without their children. Push the pairs
    /// of children that must be equal too. Return `None` when the work
    /// budget runs out.
    fn shallow(
        &mut self,
        a: &'t Node,
        b: &'t Node,
        pending: &mut Vec<(&'t Node, &'t Node)>,
    ) -> Option<bool> {
        let equal = match (a, b) {
            (Node::None, Node::None) => true,
            (Node::Bool(x), Node::Bool(y)) => x.value() == y.value(),
            (Node::Int(x), Node::Int(y)) => self.ints(x, y)?,
            (Node::Float(x), Node::Float(y)) => floats(x, y),
            (Node::Unicode(x), Node::Unicode(y)) => {
                self.spend(text::unicode_len(x) + text::unicode_len(y))?;
                match (x, y) {
                    (Unicode::Binary(x), Unicode::Binary(y)) => x == y,
                    _ => text::unicode(x) == text::unicode(y),
                }
            }
            (Node::ByteString(x), Node::ByteString(y)) => {
                self.spend(text::byte_string_len(x) + text::byte_string_len(y))?;
                text::byte_string(x) == text::byte_string(y)
            }
            (Node::Bytes(x), Node::Bytes(y)) => {
                self.spend(x.data.len())?;
                x.data == y.data
            }
            (Node::Global(x), Node::Global(y)) => {
                self.spend(x.module.len() + x.name.len())?;
                x == y
            }
            (Node::Opaque(x), Node::Opaque(y)) => {
                self.spend(x.bytes.len())?;
                x.bytes == y.bytes
            }
            (Node::Tuple(x), Node::Tuple(y)) => pair(x.items.iter(), y.items.iter(), pending),
            (Node::List(_), Node::List(_)) => pair(
                list_items(a).into_iter(),
                list_items(b).into_iter(),
                pending,
            ),
            (Node::Dict(_), Node::Dict(_)) => self.dicts(a, b, pending)?,
            (Node::Persistent(x), Node::Persistent(y)) => {
                match (target(self.a, x), target(self.b, y)) {
                    (Some(x), Some(y)) => x.oid == y.oid && x.weak == y.weak,
                    (None, None) => {
                        pending.push((x.as_ref(), y.as_ref()));
                        true
                    }
                    _ => false,
                }
            }
            (Node::Reduce(x), Node::Reduce(y)) => self.sets(x, y, pending)?,
            _ => false,
        };
        Some(equal)
    }

    /// Compare two ints by value. A large int reads whole, so the call pays
    /// for its size.
    fn ints(&mut self, x: &Int, y: &Int) -> Option<bool> {
        self.spend(int_size(x) + int_size(y))?;
        Some(match (text::int(x, usize::MAX), text::int(y, usize::MAX)) {
            (IntValue::Small(x), IntValue::Small(y)) => x == y,
            // The text is hex for LONG1 and LONG4, and decimal for INT and
            // LONG.
            (IntValue::Large { start: x, .. }, IntValue::Large { start: y, .. }) => x == y,
            _ => false,
        })
    }

    /// Compare two dicts by key. Push the pairs of values of each key.
    fn dicts(
        &mut self,
        a: &'t Node,
        b: &'t Node,
        pending: &mut Vec<(&'t Node, &'t Node)>,
    ) -> Option<bool> {
        let (entries_a, entries_b) = (dict_entries(a), dict_entries(b));
        let (resolver_a, resolver_b) = (self.a, self.b);
        let keyed_a = self.by_key(resolver_a, &entries_a)?;
        let keyed_b = self.by_key(resolver_b, &entries_b)?;
        if let (Some(keyed_a), Some(keyed_b)) = (keyed_a, keyed_b) {
            if keyed_a.len() != keyed_b.len() {
                return Some(false);
            }
            for (key, value) in keyed_a {
                match keyed_b.get(&key) {
                    Some(&other) => pending.push((value, other)),
                    None => return Some(false),
                }
            }
            return Some(true);
        }
        // A key has no canonical form: compare the entries in order.
        if entries_a.len() != entries_b.len() {
            return Some(false);
        }
        for ((key_a, value_a), (key_b, value_b)) in entries_a.into_iter().zip(entries_b) {
            pending.push((key_a, key_b));
            pending.push((value_a, value_b));
        }
        Some(true)
    }

    /// Return the values of a dict by the canonical form of their keys. The
    /// last entry of a key wins, as in Python. Return `Some(None)` when a
    /// key has no canonical form, and `None` when the work budget runs out.
    fn by_key(
        &mut self,
        resolver: &Resolver<'t>,
        entries: &[&'t (Node, Node)],
    ) -> Option<Option<HashMap<Vec<u8>, &'t Node>>> {
        let mut keyed = HashMap::with_capacity(entries.len());
        for &entry in entries {
            let (key, value) = entry;
            let Some(form) = self.canonical(resolver, key)? else {
                return Some(None);
            };
            keyed.insert(form, value);
        }
        Some(Some(keyed))
    }

    /// Compare two sets: REDUCE of `builtins.set` or `builtins.frozenset`.
    /// The items compare as a set, in any order. Arguments of another shape
    /// compare as a tuple.
    fn sets(
        &mut self,
        x: &'t Reduce,
        y: &'t Reduce,
        pending: &mut Vec<(&'t Node, &'t Node)>,
    ) -> Option<bool> {
        let (resolver_a, resolver_b) = (self.a, self.b);
        match (
            resolver_a.resolve(&x.callable),
            resolver_b.resolve(&y.callable),
        ) {
            (Some(Node::Global(callable_a)), Some(Node::Global(callable_b)))
                if callable_a == callable_b => {}
            _ => return Some(false),
        }
        if let (Some(items_a), Some(items_b)) = (
            set_items(resolver_a, &x.args),
            set_items(resolver_b, &y.args),
        ) && let (Some(forms_a), Some(forms_b)) = (
            self.set_forms(resolver_a, items_a)?,
            self.set_forms(resolver_b, items_b)?,
        ) {
            return Some(forms_a == forms_b);
        }
        pending.push((x.args.as_ref(), y.args.as_ref()));
        Some(true)
    }

    /// Return the canonical forms of the items of a set, sorted, without
    /// repeats. Return `Some(None)` when an item has no canonical form, and
    /// `None` when the work budget runs out.
    fn set_forms(
        &mut self,
        resolver: &Resolver<'t>,
        items: Vec<&'t Node>,
    ) -> Option<Option<Vec<Vec<u8>>>> {
        let mut forms = Vec::with_capacity(items.len());
        for item in items {
            let Some(form) = self.canonical(resolver, item)? else {
                return Some(None);
            };
            forms.push(form);
        }
        forms.sort_unstable();
        forms.dedup();
        Some(Some(forms))
    }

    /// Return the canonical form of a dict key or a set item. Two keys are
    /// equal when their forms are equal. The form holds the type and the
    /// value of the key, and of each item of a tuple. Return `Some(None)`
    /// for a value that has no form here, such as a list, and `None` when
    /// the work budget runs out.
    fn canonical(&mut self, resolver: &Resolver<'t>, key: &'t Node) -> Option<Option<Vec<u8>>> {
        let mut form = Vec::new();
        let mut pending = vec![key];
        while let Some(node) = pending.pop() {
            self.spend(1)?;
            let Some(node) = resolver.resolve(node) else {
                return Some(None);
            };
            match node {
                Node::None => form.push(b'N'),
                Node::Bool(value) => form.extend([b'B', u8::from(value.value())]),
                Node::Int(int) => {
                    self.spend(int_size(int))?;
                    match text::int(int, usize::MAX) {
                        IntValue::Small(value) => {
                            form.push(b'I');
                            form.extend(value.to_le_bytes());
                        }
                        IntValue::Large { start, .. } => framed(&mut form, b'L', start.as_bytes()),
                    }
                }
                Node::Float(float) => match float_value(float) {
                    Some(value) => {
                        // 0.0 and -0.0 are one key, as in Python.
                        let value = if value == 0.0 { 0.0 } else { value };
                        form.push(b'F');
                        form.extend(value.to_bits().to_le_bytes());
                    }
                    None => return Some(None),
                },
                Node::Unicode(value) => {
                    self.spend(text::unicode_len(value))?;
                    framed(&mut form, b'S', text::unicode(value).as_bytes());
                }
                Node::ByteString(value) => {
                    self.spend(text::byte_string_len(value))?;
                    framed(&mut form, b'C', &text::byte_string(value));
                }
                Node::Bytes(value) => {
                    self.spend(value.data.len())?;
                    framed(&mut form, b'Y', &value.data);
                }
                Node::Global(global) => {
                    self.spend(global.module.len() + global.name.len())?;
                    framed(&mut form, b'G', global.module.as_bytes());
                    framed(&mut form, b'.', global.name.as_bytes());
                }
                Node::Opaque(opaque) => {
                    self.spend(opaque.bytes.len())?;
                    framed(&mut form, b'O', &opaque.bytes);
                }
                Node::Persistent(pid) => match target(resolver, pid) {
                    Some(target) => {
                        form.push(if target.weak { b'W' } else { b'P' });
                        form.extend(target.oid.0.to_le_bytes());
                    }
                    None => return Some(None),
                },
                Node::Tuple(tuple) => {
                    form.push(b'T');
                    form.extend((tuple.items.len() as u64).to_le_bytes());
                    pending.extend(tuple.items.iter().rev());
                }
                _ => return Some(None),
            }
        }
        Some(Some(form))
    }
}

/// Tell if a JSON object is a dict with text keys, not a tagged value.
/// Each tag starts with `$`, and no key of such a dict does.
fn plain(object: &Map<String, Value>) -> bool {
    !object.keys().any(|key| key.starts_with('$'))
}

/// Tell if the pointer `at` with each one of `segments` added fits
/// `MAX_POINTER`.
fn fits<'k>(at: &str, mut segments: impl Iterator<Item = &'k String>) -> bool {
    segments.all(|segment| {
        // `~` and `/` take two bytes in a pointer.
        let escapes = segment.bytes().filter(|&b| b == b'~' || b == b'/').count();
        at.len() + 1 + segment.len() + escapes <= MAX_POINTER
    })
}

/// Return the values of a dict with text keys, by key. The last entry of a
/// key wins, as in the JSON state.
fn values<'t>(resolver: &Resolver<'t>, dict: &'t Node) -> HashMap<String, &'t Node> {
    dict_entries(dict)
        .into_iter()
        .filter_map(|(key, value)| Some((text::text(resolver, key)?, value)))
        .collect()
}

/// Return the items of a tuple or a list.
fn sequence_items(node: &Node) -> Vec<&Node> {
    match node {
        Node::Tuple(tuple) => tuple.items.iter().collect(),
        _ => list_items(node),
    }
}

/// Push the pairs of items of two sequences. Tell if the sequences have
/// the same length.
fn pair<'t>(
    x: impl ExactSizeIterator<Item = &'t Node>,
    y: impl ExactSizeIterator<Item = &'t Node>,
    pending: &mut Vec<(&'t Node, &'t Node)>,
) -> bool {
    if x.len() != y.len() {
        return false;
    }
    pending.extend(x.zip(y));
    true
}

/// Return the items of a set from the arguments of its REDUCE: `()`, or
/// `(items,)` with a list or a tuple of items.
fn set_items<'t>(resolver: &Resolver<'t>, args: &'t Node) -> Option<Vec<&'t Node>> {
    let Node::Tuple(args) = resolver.resolve(args)? else {
        return None;
    };
    match &args.items[..] {
        [] => Some(Vec::new()),
        [items] => match resolver.resolve(items)? {
            Node::Tuple(tuple) => Some(tuple.items.iter().collect()),
            list @ Node::List(_) => Some(list_items(list)),
            _ => None,
        },
        _ => None,
    }
}

/// Compare two floats by value. A NaN equals a NaN here, so that a value
/// that stays the same is no change.
fn floats(x: &Float, y: &Float) -> bool {
    match (float_value(x), float_value(y)) {
        (Some(x), Some(y)) => x == y || (x.is_nan() && y.is_nan()),
        _ => x == y,
    }
}

/// Return the value of a float node, if its text reads as a float.
fn float_value(float: &Float) -> Option<f64> {
    match float {
        Float::Binary(value) => Some(*value),
        Float::Text(text) => text.parse().ok(),
    }
}

/// Return the size of an int in the record.
fn int_size(int: &Int) -> usize {
    match int {
        Int::Long { bytes, .. } => bytes.len(),
        Int::Text(text) | Int::LongText(text) => text.len(),
        Int::U8(_) | Int::U16(_) | Int::I32(_) => 1,
    }
}

/// Add a tag, the length of `bytes` and `bytes` to a canonical form.
fn framed(form: &mut Vec<u8>, tag: u8, bytes: &[u8]) {
    form.push(tag);
    form.extend((bytes.len() as u64).to_le_bytes());
    form.extend_from_slice(bytes);
}

/// Return the address of a node.
fn address(node: &Node) -> usize {
    std::ptr::from_ref(node).addr()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// Decode a record whose class pickle is GLOBAL m C, BINPUT 0, and
    /// whose state pickle is `state`.
    fn record(state: &[u8]) -> Tree {
        let record = [&b"\x80\x03cm\nC\nq\x00.\x80\x03"[..], state, b"."].concat();
        zoe_pickle::decode_record(&record).unwrap()
    }

    /// Return the changes between two state pickles, sorted by path.
    fn changes(before: &[u8], after: &[u8]) -> Vec<Change> {
        let mut changes = diff(&record(before), &record(after));
        changes.sort_by(|a, b| a.path.cmp(&b.path));
        changes
    }

    /// The pickle of a str with BINUNICODE.
    fn text(value: &str) -> Vec<u8> {
        [
            &b"X"[..],
            &(value.len() as u32).to_le_bytes(),
            value.as_bytes(),
        ]
        .concat()
    }

    /// The pickle of a dict that holds `inner` under a key of `size` bytes.
    fn under_key(size: usize, inner: &[u8]) -> Vec<u8> {
        [&b"}"[..], &text(&"k".repeat(size)), inner, b"s"].concat()
    }

    /// The pickle of a persistent reference to `oid`, with the class m.C
    /// or without a class.
    fn reference(oid: u8, with_class: bool) -> Vec<u8> {
        let oid = [&b"C\x08\0\0\0\0\0\0\0"[..], &[oid]].concat();
        if with_class {
            [&oid[..], b"h\x00\x86Q"].concat()
        } else {
            [&oid[..], b"Q"].concat()
        }
    }

    #[test]
    fn key_order_memo_puts_and_opcode_width_are_no_change() {
        let memo = [
            &b"}q\x01("[..],
            &text("a"),
            b"q\x02K\x07",
            &text("b"),
            b"q\x03",
            &text("x"),
            b"u",
        ]
        .concat();
        let plain = [
            &b"}"[..],
            &text("b"),
            &text("x"),
            b"s",
            &text("a"),
            b"J\x07\x00\x00\x00s",
        ]
        .concat();
        assert!(changes(&memo, &plain).is_empty());

        // An int in its four forms, a str in its two forms and a float in
        // its two forms.
        let forms = b"(K\x07X\x01\x00\x00\x00aG?\xf8\x00\x00\x00\x00\x00\x00t";
        let other = b"(\x8a\x01\x07Vb\nF1.5\nt";
        assert_eq!(
            changes(forms, other),
            [Change {
                path: "/1".to_owned(),
                before: Some(json!("a")),
                after: Some(json!("b")),
            }]
        );
        assert!(changes(b"(K\x07t", b"(I7\nt").is_empty());
        assert!(changes(b"(K\x07t", b"(L7L\nt").is_empty());
    }

    #[test]
    fn a_key_that_one_side_has_is_a_change_with_one_value() {
        let before = [&b"}"[..], &text("a"), b"K\x01s"].concat();
        let after = [&b"}"[..], &text("b"), b"K\x01s"].concat();
        assert_eq!(
            changes(&before, &after),
            [
                Change {
                    path: "/a".to_owned(),
                    before: Some(json!(1)),
                    after: None,
                },
                Change {
                    path: "/b".to_owned(),
                    before: None,
                    after: Some(json!(1)),
                },
            ]
        );
        // The last value of a key counts, as in Python.
        let twice = [&b"}("[..], &text("a"), b"K\x02", &text("a"), b"K\x01u"].concat();
        let once = [&b"}"[..], &text("a"), b"K\x01s"].concat();
        assert!(changes(&twice, &once).is_empty());
    }

    #[test]
    fn a_list_compares_by_index_and_a_tuple_is_not_a_list() {
        assert_eq!(
            changes(b"](K\x01K\x02e", b"](K\x01K\x03K\x04e"),
            [
                Change {
                    path: "/1".to_owned(),
                    before: Some(json!(2)),
                    after: Some(json!(3)),
                },
                Change {
                    path: "/2".to_owned(),
                    before: None,
                    after: Some(json!(4)),
                },
            ]
        );
        let changed = changes(b"(K\x01K\x02t", b"](K\x01K\x02e");
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].path, "");
        assert_eq!(changed[0].before, changed[0].after);
    }

    #[test]
    fn a_dict_with_other_keys_compares_by_key() {
        let before = b"}(K\x01K\x0aK\x02K\x14u";
        let after = b"}(K\x02K\x14K\x01K\x0au";
        assert!(changes(before, after).is_empty());
        let changed = changes(before, b"}(K\x02K\x14K\x01K\x0bu");
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].path, "");
    }

    #[test]
    fn a_set_compares_by_item() {
        let set = |items: &[u8]| [&b"cbuiltins\nset\n]("[..], items, b"e\x85R"].concat();
        assert!(changes(&set(b"K\x01K\x02"), &set(b"K\x02K\x01")).is_empty());
        assert_eq!(changes(&set(b"K\x01K\x02"), &set(b"K\x01K\x03")).len(), 1);
    }

    #[test]
    fn a_reference_compares_by_oid() {
        assert!(changes(&reference(5, true), &reference(5, false)).is_empty());
        let changed = changes(&reference(5, true), &reference(6, true));
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].after.as_ref().unwrap()["$ref"], json!("0x06"));
    }

    #[test]
    fn a_value_that_the_json_cuts_still_compares_whole() {
        let long = "a".repeat((1 << 16) + 10);
        let mut other = long.clone();
        other.replace_range(other.len() - 1.., "b");
        assert!(changes(&text(&long), &text(&long)).is_empty());
        let changed = changes(&text(&long), &text(&other));
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].path, "");
        // The JSON shows the same start on both sides.
        assert_eq!(changed[0].before, changed[0].after);

        // Past the work budget, the values compare by their JSON.
        let tree_a = record(&text(&long));
        let tree_b = record(&text(&other));
        assert!(diff_within(&tree_a, &tree_b, 0).is_empty());
    }

    #[test]
    fn a_value_that_the_json_leaves_out_still_compares_whole() {
        let deep = |leaf: &[u8]| [b"(".repeat(110), leaf.to_vec(), b"t".repeat(110)].concat();
        assert!(changes(&deep(b"K\x01"), &deep(b"K\x01")).is_empty());
        let changed = changes(&deep(b"K\x01"), &deep(b"K\x02"));
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].before, Some(json!({"$elided": "too deep"})));
        assert_eq!(changed[0].path, "/0".repeat(101));
    }

    #[test]
    fn a_change_below_a_long_pointer_shows_at_a_holder() {
        let nested = |leaf: &[u8]| (0..10).fold(leaf.to_vec(), |inner, _| under_key(200, &inner));
        let changed = changes(&nested(b"K\x01"), &nested(b"K\x02"));
        assert_eq!(changed.len(), 1);
        let path = &changed[0].path;
        assert!(path.len() <= MAX_POINTER, "{}", path.len());
        assert_eq!(*path, format!("/{}", "k".repeat(200)).repeat(5));
        assert!(changed[0].before.as_ref().unwrap().is_object());
    }

    #[test]
    fn past_the_pointer_budget_the_diff_is_one_change_of_the_state() {
        // A list of ints under four keys of 250 bytes: each change has a
        // pointer of about 1,010 bytes.
        let list = |item: &[u8]| {
            let mut list = b"](".to_vec();
            for _ in 0..POINTER_BUDGET / 1000 + 100 {
                list.extend(item);
            }
            list.push(b'e');
            (0..4).fold(list, |inner, _| under_key(250, &inner))
        };
        let (before, after) = (record(&list(b"K\x01")), record(&list(b"K\x02")));
        let changed = diff(&before, &after);
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].path, "");
        assert_eq!(changed[0].before, Some(state::state(&before).0));
        assert_eq!(changed[0].after, Some(state::state(&after).0));
    }

    #[test]
    fn a_cycle_compares_by_its_values() {
        // A list that holds itself, with other memo indexes.
        let one = b"]q\x01(K\x01h\x01e";
        let two = b"]q\x05(K\x01h\x05e";
        assert!(changes(one, two).is_empty());
        // The list changes, and so does the list inside it.
        let paths: Vec<String> = changes(one, b"]q\x05(K\x02h\x05e")
            .into_iter()
            .map(|change| change.path)
            .collect();
        assert_eq!(paths, ["/0", "/1"]);
    }

    #[test]
    fn a_change_without_a_side_leaves_out_its_field() {
        let change = Change {
            path: "/a".to_owned(),
            before: None,
            after: Some(Value::Null),
        };
        assert_eq!(
            serde_json::to_value(change).unwrap(),
            json!({"path": "/a", "after": null})
        );
    }
}
