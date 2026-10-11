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
use crate::text::text;
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
/// keys to values.
fn btree_kind(tree: &Tree) -> Option<Kind> {
    let (module, name) = tree.class_name()?;
    if !module.starts_with("BTrees.") {
        return None;
    }
    if name.ends_with("BTree") {
        Some(Kind::Tree)
    } else if name.ends_with("Bucket") {
        Some(Kind::Bucket)
    } else {
        None
    }
}

/// Return the oid of the value of `key` in the BTree whose root is the
/// record `oid`, decoded as `tree` of the kind `kind`.
///
/// The state of a tree is `((child, key, child, ...), first_bucket)`. A
/// small tree keeps its one bucket inside: `(((key, value, ...),),)`. The
/// state of a bucket is `((key, value, ...), next_bucket)`. An empty tree
/// has no state.
fn btree_get(folder: &Folder, oid: Oid, tree: Tree, kind: Kind, key: &str) -> Result<Option<Oid>> {
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
                    Some(kind) => (tree, kind),
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
                pending.extend(children_for(&resolver, children, key).into_iter().rev());
                None
            }
        };
        if let Some(pairs) = pairs {
            for pair in pairs.chunks(2) {
                if let [k, value] = pair
                    && text(&resolver, k).as_deref() == Some(key)
                {
                    return Ok(reference(&resolver, value));
                }
            }
        }
    }
    Err(Error::NoObject {
        path: format!("BTree {}", crate::oid_repr(oid)),
        segment: key.to_owned(),
    })
}

/// Return the children of an inner tree node that can hold `key`. The
/// items are `(child, key, child, key, child, ...)`. When the keys are
/// text, one child holds `key`. Otherwise every child can hold it.
fn children_for<'t>(resolver: &Resolver<'t>, items: &'t [Node], key: &str) -> Vec<Oid> {
    let children: Vec<Option<Oid>> = items
        .iter()
        .step_by(2)
        .map(|child| reference(resolver, child))
        .collect();
    let keys: Option<Vec<String>> = items
        .iter()
        .skip(1)
        .step_by(2)
        .map(|k| text(resolver, k))
        .collect();
    match keys {
        Some(keys) => {
            // The child before the first separating key above `key`.
            let index = keys.iter().take_while(|k| k.as_str() <= key).count();
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
