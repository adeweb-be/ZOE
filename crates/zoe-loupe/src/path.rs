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
//! The value of each step must be a persistent reference.

use zoe_core::Oid;
use zoe_pickle::{Node, Resolver, Tree};

use crate::state::{dict_entries, target};
use crate::text::{self, IntValue, text};
use crate::{Error, Folder, Result};

/// The most BTree nodes that one lookup reads. A damaged tree can hold a
/// cycle, so the lookup stops after this count.
const MAX_TREE_NODES: usize = 100_000;

/// Return the oid that `path` leads to from the Zope application.
pub(crate) fn resolve(folder: &Folder, path: &str) -> Result<Oid> {
    let no_object = |segment: &str| Error::NoObject {
        path: path.to_owned(),
        segment: segment.to_owned(),
    };
    let mut oid = step(folder, Oid(0), "Application")?.ok_or_else(|| no_object("Application"))?;
    for segment in path.split('/').filter(|segment| !segment.is_empty()) {
        oid = step(folder, oid, segment)?.ok_or_else(|| no_object(segment))?;
    }
    Ok(oid)
}

/// Return the oid that `name` leads to from the object `oid`.
fn step(folder: &Folder, oid: Oid, name: &str) -> Result<Option<Oid>> {
    let tree = folder.tree(oid)?;
    if let Some(kind) = btree_kind(&tree) {
        return btree_get(folder, oid, tree, kind, name);
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
        let items_tree = folder.tree(items)?;
        if let Some(kind) = btree_kind(&items_tree)
            && let Some(found) = btree_get(folder, items, items_tree, kind, name)?
        {
            return Ok(Some(found));
        }
    }
    Ok(attribute(&resolver, state, name).and_then(|value| reference(&resolver, value)))
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
/// record `oid`, decoded as `tree` of the kind `kind`.
///
/// The state of a tree is `((child, key, child, ...), first_bucket)`. A
/// small tree keeps its one bucket inside: `(((key, value, ...),),)`. The
/// state of a bucket is `((key, value, ...), next_bucket)`. An empty tree
/// has no state.
fn btree_get(
    folder: &Folder,
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
                let tree = folder.tree(next)?;
                match btree_kind(&tree) {
                    Some((kind, _)) => (tree, kind),
                    None => continue,
                }
            }
        };
        let resolver = tree.resolver();
        let state = resolver.resolve(tree.state());
        let Some(Node::Tuple(state)) = state else {
            continue;
        };
        let items = state
            .items
            .first()
            .and_then(|items| resolver.resolve(items));
        let Some(Node::Tuple(items)) = items else {
            continue;
        };
        let pairs = match (kind, &state.items[..], &items.items[..]) {
            (Kind::Bucket, _, _) => Some(&items.items[..]),
            // A small tree: the state of its one bucket is inside.
            (Kind::Tree, [_], [bucket]) => match resolver.resolve(bucket) {
                Some(Node::Tuple(bucket)) => {
                    match bucket.items.first().map(|i| resolver.resolve(i)) {
                        Some(Some(Node::Tuple(pairs))) => Some(&pairs.items[..]),
                        _ => None,
                    }
                }
                _ => None,
            },
            (Kind::Tree, _, children) => {
                pending.extend(children_for(&resolver, children, &key).into_iter().rev());
                None
            }
        };
        if let Some(pairs) = pairs {
            for pair in pairs.chunks(2) {
                if let [k, value] = pair
                    && Key::read(&resolver, k).as_ref() == Some(&key)
                {
                    return Ok(reference(&resolver, value));
                }
            }
        }
    }
    Err(Error::NoObject {
        path: format!("BTree {}", crate::oid_repr(oid)),
        segment: segment.to_owned(),
    })
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
    use zoe_core::{Store, Tid};

    use super::*;

    /// A record of the class `module.name` with the state pickle `state`.
    fn record(module: &str, name: &str, state: &[u8]) -> Vec<u8> {
        let class = format!("c{module}\n{name}\n.");
        [b"\x80\x03", class.as_bytes(), b"\x80\x03", state, b"."].concat()
    }

    /// A persistent reference to `oid` with the class `module.name`.
    fn reference(oid: u64, module: &str, name: &str) -> Vec<u8> {
        let class = format!("c{module}\n{name}\n");
        [
            &b"C\x08"[..],
            &oid.to_be_bytes(),
            class.as_bytes(),
            b"\x86Q",
        ]
        .concat()
    }

    /// Write a store whose application holds an `IOBTree` with the key 7.
    fn int_tree_store() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("zoe-loupe-int-keys-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
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
        assert_eq!(resolve(&folder, "/ids/7").unwrap(), Oid(3));
        for path in ["/ids/8", "/ids/item", "/ids/7x"] {
            assert!(
                matches!(resolve(&folder, path), Err(Error::NoObject { .. })),
                "{path}"
            );
        }
    }
}
