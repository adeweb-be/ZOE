//! Plone-aware paths.
//!
//! A path starts at the Zope application, `root["Application"]`. Each
//! segment leads from one persistent object to the next:
//!
//! - in a BTree, through the key;
//! - in a mapping, such as a `PersistentMapping`, through the key;
//! - in a folder, through the item of its `_tree`, as in BTreeFolder2;
//! - else through the attribute of that name, such as `__annotations__`.
//!
//! The value of each step must be a persistent reference. Each step reads
//! the record of the next object as of the tid of the view.
//!
//! [`items`] lists the segments that lead on from one object, by the same
//! rules. The space screen uses them for the size of each item.

use std::collections::{HashMap, HashSet};

use zoe_core::Oid;
use zoe_pickle::{Node, Resolver, Tree};

use crate::state::{dict_entries, target};
use crate::text::{self, IntValue, text};
use crate::{Error, Result, Snapshot};

/// The most BTree nodes that one lookup reads. A damaged tree can hold a
/// cycle, so the lookup stops after this count.
const MAX_TREE_NODES: usize = 100_000;

/// Return the oid that `path` leads to from the Zope application in the
/// view `view`.
pub(crate) fn resolve(view: &Snapshot<'_>, path: &str) -> Result<Oid> {
    let no_object = |segment: &str| Error::NoObject {
        path: path.to_owned(),
        segment: segment.to_owned(),
    };
    let mut oid = step(view, Oid(0), "Application")?.ok_or_else(|| no_object("Application"))?;
    for segment in path.split('/').filter(|segment| !segment.is_empty()) {
        oid = step(view, oid, segment)?.ok_or_else(|| no_object(segment))?;
    }
    Ok(oid)
}

/// Return the oid that `name` leads to from the object `oid`.
fn step(view: &Snapshot<'_>, oid: Oid, name: &str) -> Result<Option<Oid>> {
    let tree = view.tree(oid)?;
    if let Some(kind) = btree_kind(&tree) {
        return btree_get(view, oid, tree, kind, name);
    }
    let resolver = tree.resolver();
    let Some(state) = resolver.resolve(tree.state()) else {
        return Ok(None);
    };
    if let Some(data) = mapping_data(&resolver, state) {
        return Ok(get(&resolver, data, name).and_then(|value| reference(&resolver, value)));
    }
    let items = attribute(&resolver, state, "_tree").and_then(|value| reference(&resolver, value));
    if let Some(items) = items {
        let items_tree = view.tree(items)?;
        if let Some(kind) = btree_kind(&items_tree)
            && let Some(found) = btree_get(view, items, items_tree, kind, name)?
        {
            return Ok(Some(found));
        }
    }
    Ok(attribute(&resolver, state, name).and_then(|value| reference(&resolver, value)))
}

/// Return the items of the object `oid` in the view `view`: each name that
/// a path segment can take from the object, with the oid that it leads
/// to, in the order of the records. The items follow the rules of a step:
///
/// - a BTree gives each key and its value, in key order;
/// - a mapping gives each key and its value;
/// - another object gives the items of its `_tree`, then each attribute
///   that holds a reference and that no item of `_tree` hides.
///
/// Each name is one segment: it is not empty and holds no `/`. A key of
/// another type than the keys that a segment matches is not an item. A
/// value that is not a reference is not an item. An object or a BTree node
/// that does not decode, or that has no record, gives no items. `read`
/// runs before the call reads each BTree node after the first, and its
/// error stops the call.
pub(crate) fn items(
    view: &Snapshot<'_>,
    oid: Oid,
    read: &mut dyn FnMut() -> Result<()>,
) -> Result<Vec<(String, Oid)>> {
    let mut items = Named::default();
    let Some(tree) = view.readable_tree(oid)? else {
        return Ok(Vec::new());
    };
    if let Some(kind) = btree_kind(&tree) {
        btree_items(view, oid, tree, kind, &mut items, read)?;
        return Ok(items.into_items());
    }
    let resolver = tree.resolver();
    let Some(state) = resolver.resolve(tree.state()) else {
        return Ok(Vec::new());
    };
    if let Some(data) = mapping_data(&resolver, state) {
        for (key, value) in dict_entries(data) {
            if let Some(name) = text(&resolver, key) {
                items.set(name, reference(&resolver, value));
            }
        }
        return Ok(items.into_items());
    }
    let folder = attribute(&resolver, state, "_tree").and_then(|value| reference(&resolver, value));
    if let Some(folder) = folder {
        read()?;
        if let Some(folder_tree) = view.readable_tree(folder)?
            && let Some(kind) = btree_kind(&folder_tree)
        {
            btree_items(view, folder, folder_tree, kind, &mut items, read)?;
        }
    }
    // A later entry of a name replaces an earlier one, as `attribute`
    // reads them: the dict, then the slots of an object with slots.
    let mut attributes = Named::default();
    let parts = match state {
        Node::Tuple(pair) => pair
            .items
            .iter()
            .filter_map(|part| resolver.resolve(part))
            .collect(),
        _ => vec![state],
    };
    for part in parts {
        for (key, value) in dict_entries(part) {
            if let Some(name) = text(&resolver, key) {
                attributes.set(name, reference(&resolver, value));
            }
        }
    }
    for (name, oid) in attributes.list {
        if !items.leads_on(&name) {
            items.set(name, oid);
        }
    }
    Ok(items.into_items())
}

/// Names and the oids that they lead to, in the order of the first entry
/// of each name. A value that is not a reference leads to no oid.
#[derive(Default)]
struct Named {
    list: Vec<(String, Option<Oid>)>,
    index: HashMap<String, usize>,
}

impl Named {
    /// Set the oid of `name`. A name that is in the list keeps its place.
    fn set(&mut self, name: String, oid: Option<Oid>) {
        match self.index.get(&name) {
            Some(&at) => self.list[at].1 = oid,
            None => {
                self.index.insert(name.clone(), self.list.len());
                self.list.push((name, oid));
            }
        }
    }

    /// Tell if `name` is in the list.
    fn contains(&self, name: &str) -> bool {
        self.index.contains_key(name)
    }

    /// Tell if `name` leads to an oid.
    fn leads_on(&self, name: &str) -> bool {
        self.index
            .get(name)
            .is_some_and(|&at| self.list[at].1.is_some())
    }

    /// Return each name that is one segment and that leads to an oid.
    fn into_items(self) -> Vec<(String, Oid)> {
        self.list
            .into_iter()
            .filter(|(name, _)| !name.is_empty() && !name.contains('/'))
            .filter_map(|(name, oid)| Some((name, oid?)))
            .collect()
    }
}

/// Add the keys and the values of the BTree whose root is the record
/// `oid`, decoded as `tree`, to `items`, in key order. The first entry of
/// a key wins, as in a lookup. Each node is read once, so a cycle in a
/// damaged tree ends.
fn btree_items(
    view: &Snapshot<'_>,
    oid: Oid,
    tree: Tree,
    (kind, int_keys): (Kind, bool),
    items: &mut Named,
    read: &mut dyn FnMut() -> Result<()>,
) -> Result<()> {
    let mut seen = HashSet::from([oid]);
    let mut first = Some((tree, kind));
    let mut pending: Vec<Oid> = Vec::new();
    loop {
        let (tree, kind) = match first.take() {
            Some(first) => first,
            None => {
                let Some(next) = pending.pop() else {
                    return Ok(());
                };
                if !seen.insert(next) {
                    continue;
                }
                read()?;
                match view.readable_tree(next)? {
                    Some(tree) => match btree_kind(&tree) {
                        Some((kind, _)) => (tree, kind),
                        None => continue,
                    },
                    None => continue,
                }
            }
        };
        let resolver = tree.resolver();
        match contents(&resolver, &tree, kind) {
            Some(Contents::Pairs(pairs)) => {
                for pair in pairs.chunks(2) {
                    let [k, value] = pair else {
                        continue;
                    };
                    let name = match Key::read(&resolver, k) {
                        Some(Key::Int(key)) if int_keys => key.to_string(),
                        Some(Key::Text(key)) if !int_keys => key,
                        _ => continue,
                    };
                    if !items.contains(&name) {
                        items.set(name, reference(&resolver, value));
                    }
                }
            }
            Some(Contents::Children(children)) => {
                let children: Vec<Oid> = children
                    .iter()
                    .step_by(2)
                    .filter_map(|child| reference(&resolver, child))
                    .collect();
                pending.extend(children.into_iter().rev());
            }
            None => {}
        }
    }
}

/// The two kinds of BTree node.
#[derive(Clone, Copy)]
enum Kind {
    /// A tree: inner nodes, and a small tree with one bucket inside.
    Tree,
    /// A bucket: keys and values.
    Bucket,
}

/// Tell if a record is a BTree node of the `BTrees` package that maps
/// keys to values. Also tell if its keys are ints: the first letter of
/// the class name gives the type of the keys, as in `IOBTree`.
fn btree_kind(tree: &Tree) -> Option<(Kind, bool)> {
    let (module, name) = tree.class_name()?;
    if !module.starts_with("BTrees.") {
        return None;
    }
    let int_keys = matches!(name.as_bytes().first(), Some(b'I' | b'L' | b'Q' | b'U'));
    if name.ends_with("BTree") {
        Some((Kind::Tree, int_keys))
    } else if name.ends_with("Bucket") {
        Some((Kind::Bucket, int_keys))
    } else {
        None
    }
}

/// A key of a BTree that a path segment can match.
#[derive(PartialEq, PartialOrd)]
enum Key {
    Text(String),
    Int(i128),
}

impl Key {
    /// Read a key node. Look through memo puts and gets.
    fn read<'t>(resolver: &Resolver<'t>, node: &'t Node) -> Option<Key> {
        match resolver.resolve(node)? {
            // A key beyond an i128 matches no segment, so its text is not
            // needed.
            Node::Int(int) => match text::int(int, 0) {
                IntValue::Small(value) => Some(Key::Int(value)),
                IntValue::Large { .. } => None,
            },
            _ => text(resolver, node).map(Key::Text),
        }
    }

    /// Tell if two keys have the same type, so that they compare.
    fn same_type(&self, other: &Key) -> bool {
        matches!(
            (self, other),
            (Key::Text(_), Key::Text(_)) | (Key::Int(_), Key::Int(_))
        )
    }
}

/// Return the oid of the value of `key` in the BTree whose root is the
/// record `oid`, decoded as `tree` of the kind `kind`. `contents` gives
/// the forms of the state.
fn btree_get(
    view: &Snapshot<'_>,
    oid: Oid,
    tree: Tree,
    (kind, int_keys): (Kind, bool),
    segment: &str,
) -> Result<Option<Oid>> {
    // A BTree with int keys holds a segment only if it is an int.
    let key = if int_keys {
        match segment.parse() {
            Ok(value) => Key::Int(value),
            Err(_) => return Ok(None),
        }
    } else {
        Key::Text(segment.to_owned())
    };
    let mut first = Some((tree, kind));
    let mut pending: Vec<Oid> = Vec::new();
    for _ in 0..MAX_TREE_NODES {
        let (tree, kind) = match first.take() {
            Some(first) => first,
            None => {
                let Some(next) = pending.pop() else {
                    return Ok(None);
                };
                let tree = view.tree(next)?;
                match btree_kind(&tree) {
                    Some((kind, _)) => (tree, kind),
                    None => continue,
                }
            }
        };
        let resolver = tree.resolver();
        match contents(&resolver, &tree, kind) {
            Some(Contents::Pairs(pairs)) => {
                for pair in pairs.chunks(2) {
                    if let [k, value] = pair
                        && Key::read(&resolver, k).as_ref() == Some(&key)
                    {
                        return Ok(reference(&resolver, value));
                    }
                }
            }
            Some(Contents::Children(children)) => {
                pending.extend(children_for(&resolver, children, &key).into_iter().rev());
            }
            None => {}
        }
    }
    Err(Error::NoObject {
        path: format!("BTree {}", crate::oid_repr(oid)),
        segment: segment.to_owned(),
    })
}

/// What a BTree node holds.
enum Contents<'t> {
    /// The keys and the values of a bucket: `(key, value, key, value, ...)`.
    Pairs(&'t [Node]),
    /// The children and the separating keys of an inner node:
    /// `(child, key, child, key, child, ...)`.
    Children(&'t [Node]),
}

/// Return what the BTree node `tree` of the kind `kind` holds, or `None`
/// for an empty tree or a state of another form.
///
/// The state of a tree is `((child, key, child, ...), first_bucket)`. A
/// small tree keeps the state of its one bucket inside:
/// `((((key, value, ...),),),)`. The state of a bucket is
/// `((key, value, ...), next_bucket)`, or `((key, value, ...),)` for the
/// last bucket. An empty tree has no state.
fn contents<'t>(resolver: &Resolver<'t>, tree: &'t Tree, kind: Kind) -> Option<Contents<'t>> {
    let Some(Node::Tuple(state)) = resolver.resolve(tree.state()) else {
        return None;
    };
    let items = state
        .items
        .first()
        .and_then(|items| resolver.resolve(items));
    let Some(Node::Tuple(items)) = items else {
        return None;
    };
    match (kind, &state.items[..], &items.items[..]) {
        (Kind::Bucket, _, _) => Some(Contents::Pairs(&items.items)),
        // A small tree: the state of its one bucket is inside.
        (Kind::Tree, [_], [bucket]) => match resolver.resolve(bucket) {
            Some(Node::Tuple(bucket)) => match bucket.items.first().map(|i| resolver.resolve(i)) {
                Some(Some(Node::Tuple(pairs))) => Some(Contents::Pairs(&pairs.items)),
                _ => None,
            },
            _ => None,
        },
        (Kind::Tree, _, children) => Some(Contents::Children(children)),
    }
}

/// Return the children of an inner tree node that can hold `key`. The
/// items are `(child, key, child, key, child, ...)`. When the keys have
/// the type of `key`, one child holds it. Otherwise every child can.
fn children_for<'t>(resolver: &Resolver<'t>, items: &'t [Node], key: &Key) -> Vec<Oid> {
    let children: Vec<Option<Oid>> = items
        .iter()
        .step_by(2)
        .map(|child| reference(resolver, child))
        .collect();
    let keys: Option<Vec<Key>> = items
        .iter()
        .skip(1)
        .step_by(2)
        .map(|k| Key::read(resolver, k).filter(|k| k.same_type(key)))
        .collect();
    match keys {
        Some(keys) => {
            // The child before the first separating key above `key`.
            let index = keys.iter().take_while(|k| *k <= key).count();
            children.get(index).copied().flatten().into_iter().collect()
        }
        None => children.into_iter().flatten().collect(),
    }
}

/// Return the dict of a mapping state: `{"data": {...}}`, as
/// `PersistentMapping` and `PersistentDict` write it, or
/// `{"_container": {...}}`, as older versions write it.
fn mapping_data<'t>(resolver: &Resolver<'t>, state: &'t Node) -> Option<&'t Node> {
    let [(key, value)] = dict_entries(state)[..] else {
        return None;
    };
    let key = text(resolver, key)?;
    let value = resolver.resolve(value)?;
    (matches!(key.as_str(), "data" | "_container") && matches!(value, Node::Dict(_)))
        .then_some(value)
}

/// Return the attribute `name` of an object state: a dict, or the pair
/// `(dict, slots)` of an object with slots.
fn attribute<'t>(resolver: &Resolver<'t>, state: &'t Node, name: &str) -> Option<&'t Node> {
    match state {
        Node::Tuple(pair) => pair
            .items
            .iter()
            .rev()
            .filter_map(|part| resolver.resolve(part))
            .find_map(|part| get(resolver, part, name)),
        _ => get(resolver, state, name),
    }
}

/// Return the value of the text key `key` in a dict node. The last entry
/// wins, as in Python.
fn get<'t>(resolver: &Resolver<'t>, dict: &'t Node, key: &str) -> Option<&'t Node> {
    dict_entries(dict)
        .into_iter()
        .rev()
        .find(|(k, _)| text(resolver, k).as_deref() == Some(key))
        .map(|(_, value)| value)
}

/// Return the oid of a node that holds a persistent reference.
fn reference<'t>(resolver: &Resolver<'t>, node: &'t Node) -> Option<Oid> {
    match resolver.resolve(node)? {
        Node::Persistent(pid) => target(resolver, pid).map(|target| target.oid),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use zoe_core::testing::TempDir;
    use zoe_core::{Store, Tid};

    use zoe_pickle::testing::{record, reference, text};

    use super::*;
    use crate::{Folder, fresh_dir};

    /// Write a store whose application holds an `IOBTree` with the key 7.
    fn int_tree_store() -> TempDir {
        let dir = fresh_dir("int-keys");
        let records = [
            record(
                "persistent.mapping",
                "PersistentMapping",
                &[
                    &b"}X\x04\x00\x00\x00data}X\x0b\x00\x00\x00Application"[..],
                    &reference(1, "m", "App"),
                    b"ss",
                ]
                .concat(),
            ),
            record(
                "m",
                "App",
                &[
                    &b"}X\x03\x00\x00\x00ids"[..],
                    &reference(2, "BTrees.IOBTree", "IOBTree"),
                    b"s",
                ]
                .concat(),
            ),
            record(
                "BTrees.IOBTree",
                "IOBTree",
                &[
                    &b"K\x07"[..],
                    &reference(3, "m", "Item"),
                    b"\x86\x85\x85\x85",
                ]
                .concat(),
            ),
            record("m", "Item", b"}"),
        ];
        let store = Store::open(&dir).unwrap();
        let mut txn = store.tpc_begin_restore(Tid(1), b"", b"", b"").unwrap();
        for (oid, data) in records.iter().enumerate() {
            txn.restore(Oid(oid as u64), Some(data)).unwrap();
        }
        txn.vote().unwrap();
        txn.finish().unwrap();
        dir
    }

    #[test]
    fn an_int_key_resolves_and_text_does_not() {
        let dir = int_tree_store();
        let folder = Folder::open(&dir).unwrap();
        let view = folder.as_of(folder.last_tid());
        assert_eq!(resolve(&view, "/ids/7").unwrap(), Oid(3));
        for path in ["/ids/8", "/ids/item", "/ids/7x"] {
            assert!(
                matches!(resolve(&view, path), Err(Error::NoObject { .. })),
                "{path}"
            );
        }
    }

    /// A dict state with the text keys and the pickled values of
    /// `entries`, in order.
    fn dict(entries: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut state = b"}".to_vec();
        for (key, value) in entries {
            state.extend(text(key));
            state.extend(value);
            state.extend(b"s");
        }
        state
    }

    /// The pairs of a bucket: a tuple of the keys and the pickled values.
    fn pairs(entries: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut state = b"(".to_vec();
        for (key, value) in entries {
            state.extend(text(key));
            state.extend(value);
        }
        state.extend(b"t");
        state
    }

    /// Write a store whose application holds a folder with a BTree of two
    /// buckets, a mapping, an `IOBTree` and a record that does not decode.
    fn items_store() -> TempDir {
        let dir = fresh_dir("items");
        let item = |oid| reference(oid, "m", "Item");
        let bucket = |oid| reference(oid, "BTrees.OOBTree", "OOBucket");
        let mapping = |state: &[u8]| record("persistent.mapping", "PersistentMapping", state);
        let mut records = vec![
            (
                0,
                mapping(&dict(&[("data", dict(&[("Application", item(1))]))])),
            ),
            (
                1,
                record(
                    "m",
                    "App",
                    &dict(&[
                        ("folder", item(2)),
                        ("map", item(3)),
                        ("ids", reference(4, "BTrees.IOBTree", "IOBTree")),
                        ("bad", item(5)),
                    ]),
                ),
            ),
            (
                2,
                record(
                    "m",
                    "Folder",
                    &dict(&[
                        ("_tree", reference(6, "BTrees.OOBTree", "OOBTree")),
                        ("a", item(21)),
                        ("b", item(27)),
                        ("c", item(22)),
                        ("x/y", item(23)),
                        ("", item(24)),
                        ("n", b"K\x05".to_vec()),
                    ]),
                ),
            ),
            // A mapping with the key `k1` twice: the last entry wins.
            (
                3,
                mapping(&dict(&[(
                    "data",
                    dict(&[("k1", item(30)), ("k2", text("v")), ("k1", item(31))]),
                )])),
            ),
            (
                4,
                record(
                    "BTrees.IOBTree",
                    "IOBTree",
                    &[&b"K\x07"[..], &item(40), b"\x86\x85\x85\x85"].concat(),
                ),
            ),
            (5, b"\x80\x03cm\nC\n.\x80\x03\xff.".to_vec()),
            // The tree: two buckets and the key `m` between them.
            (
                6,
                record(
                    "BTrees.OOBTree",
                    "OOBTree",
                    &[
                        &b"("[..],
                        &bucket(10),
                        &text("m"),
                        &bucket(11),
                        b"t",
                        &bucket(10),
                        b"\x86",
                    ]
                    .concat(),
                ),
            ),
            // The value of `b` is not a reference, so the attribute `b`
            // is not hidden.
            (
                10,
                record(
                    "BTrees.OOBTree",
                    "OOBucket",
                    &[
                        &pairs(&[("a", item(20)), ("b", b"K\x07".to_vec())])[..],
                        &bucket(11),
                        b"\x86",
                    ]
                    .concat(),
                ),
            ),
            (
                11,
                record(
                    "BTrees.OOBTree",
                    "OOBucket",
                    &[&pairs(&[("m", item(25)), ("q/r", item(26))])[..], b"\x85"].concat(),
                ),
            ),
        ];
        for oid in [20, 21, 22, 23, 24, 25, 26, 27, 30, 31, 40] {
            records.push((oid, record("m", "Item", b"}")));
        }
        let store = Store::open(&dir).unwrap();
        let mut txn = store.tpc_begin_restore(Tid(1), b"", b"", b"").unwrap();
        for (oid, data) in &records {
            txn.restore(Oid(*oid), Some(data)).unwrap();
        }
        txn.vote().unwrap();
        txn.finish().unwrap();
        dir
    }

    #[test]
    fn the_items_follow_the_steps_of_a_path() {
        let dir = items_store();
        let folder = Folder::open(&dir).unwrap();
        let view = folder.as_of(folder.last_tid());
        let reads = std::cell::Cell::new(0);
        let items_at = |path: &str| {
            let oid = resolve(&view, path).unwrap();
            let found = items(&view, oid, &mut || {
                reads.set(reads.get() + 1);
                Ok(())
            })
            .unwrap();
            // Each item resolves as a segment of the path.
            for (name, item) in &found {
                let at = format!("{path}/{name}");
                assert_eq!(resolve(&view, &at).unwrap(), *item, "{at}");
            }
            found
                .into_iter()
                .map(|(name, oid)| (name, oid.0))
                .collect::<Vec<_>>()
        };
        let named = |items: &[(&str, u64)]| -> Vec<(String, u64)> {
            items.iter().map(|(n, o)| ((*n).to_owned(), *o)).collect()
        };
        // The items of `_tree` in key order, then the attributes. The item
        // `a` hides the attribute `a`. A name with `/` and an empty name
        // are not segments.
        assert_eq!(
            items_at("/folder"),
            named(&[("a", 20), ("b", 27), ("m", 25), ("_tree", 6), ("c", 22)])
        );
        // The tree, and its two buckets.
        assert_eq!(reads.get(), 3);
        assert_eq!(items_at("/map"), named(&[("k1", 31)]));
        assert_eq!(items_at("/ids"), named(&[("7", 40)]));
        assert_eq!(
            items_at(""),
            named(&[("folder", 2), ("map", 3), ("ids", 4), ("bad", 5)])
        );
        // A record that does not decode has no items.
        let bad = resolve(&view, "/bad").unwrap();
        assert!(items(&view, bad, &mut || Ok(())).unwrap().is_empty());
        // An error of `read` stops the call.
        let folder_oid = resolve(&view, "/folder").unwrap();
        let stopped = items(&view, folder_oid, &mut || Err(Error::Stopped));
        assert!(matches!(stopped, Err(Error::Stopped)));
    }
}
