//! The drop, the clone, the comparison and the debug output of a node.
//!
//! The derived forms of these traits call themselves once per nesting
//! level, so a deep tree overflows the call stack and aborts the process.
//! The forms here keep the nodes that wait on a stack in the heap instead.

use std::fmt;
use std::mem;

use crate::tree::{Batch, Dict, List, Node, Put, Reduce, Tuple};

impl Drop for Node {
    fn drop(&mut self) {
        if self.is_leaf() {
            return;
        }
        // Move the children out before this node goes. Each child then
        // drops with no children of its own, so the drop never goes deep.
        let mut stack = Vec::new();
        take_children(self, &mut stack);
        while let Some(mut node) = stack.pop() {
            take_children(&mut node, &mut stack);
        }
    }
}

/// Moves the children of a node to `out`. The node keeps empty containers.
fn take_children(node: &mut Node, out: &mut Vec<Node>) {
    let mut take = |child: &mut Box<Node>| {
        if !matches!(**child, Node::None) {
            out.push(mem::replace(&mut **child, Node::None));
        }
    };
    match node {
        Node::Persistent(pid) => take(pid),
        Node::Reduce(reduce) => {
            take(&mut reduce.callable);
            take(&mut reduce.args);
        }
        Node::Put(put) => take(&mut put.value),
        Node::Tuple(tuple) => out.append(&mut tuple.items),
        Node::List(list) => {
            out.extend(list.initial.take().into_iter().flatten());
            for batch in &mut list.batches {
                out.append(&mut batch.items);
            }
        }
        Node::Dict(dict) => {
            let batches = dict
                .batches
                .iter_mut()
                .flat_map(|batch| batch.items.drain(..));
            for (key, value) in dict.initial.take().into_iter().flatten().chain(batches) {
                out.push(key);
                out.push(value);
            }
        }
        _ => {}
    }
}

impl Clone for Node {
    fn clone(&self) -> Node {
        enum Task<'a> {
            /// Copy a node and the nodes below it.
            Copy(&'a Node),
            /// Build a copy of a node from the copies of its children, which
            /// are the last `count` nodes of `done`.
            Build(&'a Node, usize),
        }
        let mut tasks = vec![Task::Copy(self)];
        let mut done: Vec<Node> = Vec::new();
        let mut children = Vec::new();
        while let Some(task) = tasks.pop() {
            match task {
                Task::Copy(node) => {
                    if let Some(leaf) = clone_leaf(node) {
                        done.push(leaf);
                        continue;
                    }
                    node.children(&mut children);
                    tasks.push(Task::Build(node, children.len()));
                    tasks.extend(children.drain(..).rev().map(Task::Copy));
                }
                Task::Build(node, count) => {
                    let copies = done.split_off(done.len() - count);
                    done.push(rebuild(node, copies));
                }
            }
        }
        done.pop().unwrap_or(Node::None)
    }
}

/// Returns a copy of a node that has no children, or `None` for another
/// node.
fn clone_leaf(node: &Node) -> Option<Node> {
    Some(match node {
        Node::None => Node::None,
        Node::Bool(value) => Node::Bool(value.clone()),
        Node::Int(value) => Node::Int(value.clone()),
        Node::Float(value) => Node::Float(value.clone()),
        Node::Bytes(value) => Node::Bytes(value.clone()),
        Node::ByteString(value) => Node::ByteString(value.clone()),
        Node::Unicode(value) => Node::Unicode(value.clone()),
        Node::Global(value) => Node::Global(value.clone()),
        Node::Get(value) => Node::Get(*value),
        Node::Opaque(value) => Node::Opaque(value.clone()),
        Node::Tuple(_)
        | Node::List(_)
        | Node::Dict(_)
        | Node::Persistent(_)
        | Node::Reduce(_)
        | Node::Put(_) => return None,
    })
}

/// Returns a copy of a node with the given children, in pickle order.
fn rebuild(node: &Node, children: Vec<Node>) -> Node {
    let mut children = children.into_iter();
    let mut next = || children.next().unwrap_or(Node::None);
    match node {
        Node::Tuple(tuple) => Node::Tuple(Tuple {
            items: (0..tuple.items.len()).map(|_| next()).collect(),
            marked: tuple.marked,
        }),
        Node::List(list) => Node::List(List {
            initial: list
                .initial
                .as_ref()
                .map(|items| items.iter().map(|_| next()).collect()),
            batches: list
                .batches
                .iter()
                .map(|batch| Batch {
                    items: batch.items.iter().map(|_| next()).collect(),
                    marked: batch.marked,
                })
                .collect(),
        }),
        Node::Dict(dict) => Node::Dict(Dict {
            initial: dict
                .initial
                .as_ref()
                .map(|items| items.iter().map(|_| (next(), next())).collect()),
            batches: dict
                .batches
                .iter()
                .map(|batch| Batch {
                    items: batch.items.iter().map(|_| (next(), next())).collect(),
                    marked: batch.marked,
                })
                .collect(),
        }),
        Node::Persistent(_) => Node::Persistent(Box::new(next())),
        Node::Reduce(_) => Node::Reduce(Reduce {
            callable: Box::new(next()),
            args: Box::new(next()),
        }),
        Node::Put(put) => Node::Put(Put {
            index: put.index,
            form: put.form,
            value: Box::new(next()),
        }),
        leaf => clone_leaf(leaf).unwrap_or(Node::None),
    }
}

impl PartialEq for Node {
    fn eq(&self, other: &Node) -> bool {
        let mut pairs = vec![(self, other)];
        let mut left = Vec::new();
        let mut right = Vec::new();
        while let Some((a, b)) = pairs.pop() {
            if !same_shape(a, b) {
                return false;
            }
            a.children(&mut left);
            b.children(&mut right);
            pairs.extend(left.drain(..).zip(right.drain(..)));
        }
        true
    }
}

impl Eq for Node {}

/// Compares two nodes without their children. Two nodes with the same
/// shape have the same number of children, in the same places.
fn same_shape(a: &Node, b: &Node) -> bool {
    fn lengths<T>(a: &Option<Vec<T>>, b: &Option<Vec<T>>) -> bool {
        a.as_ref().map(Vec::len) == b.as_ref().map(Vec::len)
    }
    fn batches<T>(a: &[Batch<T>], b: &[Batch<T>]) -> bool {
        a.len() == b.len()
            && a.iter()
                .zip(b)
                .all(|(a, b)| a.marked == b.marked && a.items.len() == b.items.len())
    }
    match (a, b) {
        (Node::None, Node::None) => true,
        (Node::Bool(a), Node::Bool(b)) => a == b,
        (Node::Int(a), Node::Int(b)) => a == b,
        (Node::Float(a), Node::Float(b)) => a == b,
        (Node::Bytes(a), Node::Bytes(b)) => a == b,
        (Node::ByteString(a), Node::ByteString(b)) => a == b,
        (Node::Unicode(a), Node::Unicode(b)) => a == b,
        (Node::Tuple(a), Node::Tuple(b)) => a.marked == b.marked && a.items.len() == b.items.len(),
        (Node::List(a), Node::List(b)) => {
            lengths(&a.initial, &b.initial) && batches(&a.batches, &b.batches)
        }
        (Node::Dict(a), Node::Dict(b)) => {
            lengths(&a.initial, &b.initial) && batches(&a.batches, &b.batches)
        }
        (Node::Global(a), Node::Global(b)) => a == b,
        (Node::Persistent(_), Node::Persistent(_)) | (Node::Reduce(_), Node::Reduce(_)) => true,
        (Node::Put(a), Node::Put(b)) => a.index == b.index && a.form == b.form,
        (Node::Get(a), Node::Get(b)) => a == b,
        (Node::Opaque(a), Node::Opaque(b)) => a == b,
        _ => false,
    }
}

/// The debug output has the form of a derived `Debug` on one line, also
/// for `{:#?}`.
impl fmt::Debug for Node {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        enum Piece<'a> {
            Node(&'a Node),
            Text(&'static str),
            /// Items with a comma between them. True for the first piece of
            /// the sequence.
            Nodes(&'a [Node], bool),
            Pairs(&'a [(Node, Node)], bool),
            ListBatches(&'a [Batch<Node>], bool),
            DictBatches(&'a [Batch<(Node, Node)>], bool),
            /// The end of a tuple or of a batch, with its mark flag.
            Marked(bool),
        }
        let mut pieces = vec![Piece::Node(self)];
        while let Some(piece) = pieces.pop() {
            match piece {
                Piece::Text(text) => f.write_str(text)?,
                Piece::Marked(marked) => write!(f, "], marked: {marked} }}")?,
                Piece::Nodes([], _)
                | Piece::Pairs([], _)
                | Piece::ListBatches([], _)
                | Piece::DictBatches([], _) => {}
                Piece::Nodes([node, rest @ ..], first) => {
                    if !first {
                        f.write_str(", ")?;
                    }
                    pieces.push(Piece::Nodes(rest, false));
                    pieces.push(Piece::Node(node));
                }
                Piece::Pairs([(key, value), rest @ ..], first) => {
                    f.write_str(if first { "(" } else { ", (" })?;
                    pieces.push(Piece::Pairs(rest, false));
                    pieces.push(Piece::Text(")"));
                    pieces.push(Piece::Node(value));
                    pieces.push(Piece::Text(", "));
                    pieces.push(Piece::Node(key));
                }
                Piece::ListBatches([batch, rest @ ..], first) => {
                    f.write_str(if first { "" } else { ", " })?;
                    f.write_str("Batch { items: [")?;
                    pieces.push(Piece::ListBatches(rest, false));
                    pieces.push(Piece::Marked(batch.marked));
                    pieces.push(Piece::Nodes(&batch.items, true));
                }
                Piece::DictBatches([batch, rest @ ..], first) => {
                    f.write_str(if first { "" } else { ", " })?;
                    f.write_str("Batch { items: [")?;
                    pieces.push(Piece::DictBatches(rest, false));
                    pieces.push(Piece::Marked(batch.marked));
                    pieces.push(Piece::Pairs(&batch.items, true));
                }
                Piece::Node(node) => match node {
                    Node::Tuple(tuple) => {
                        f.write_str("Tuple(Tuple { items: [")?;
                        pieces.push(Piece::Text(")"));
                        pieces.push(Piece::Marked(tuple.marked));
                        pieces.push(Piece::Nodes(&tuple.items, true));
                    }
                    Node::List(list) => {
                        f.write_str("List(List { initial: ")?;
                        pieces.push(Piece::Text("] })"));
                        pieces.push(Piece::ListBatches(&list.batches, true));
                        pieces.push(Piece::Text(", batches: ["));
                        match &list.initial {
                            Some(items) => {
                                pieces.push(Piece::Text("])"));
                                pieces.push(Piece::Nodes(items, true));
                                pieces.push(Piece::Text("Some(["));
                            }
                            None => pieces.push(Piece::Text("None")),
                        }
                    }
                    Node::Dict(dict) => {
                        f.write_str("Dict(Dict { initial: ")?;
                        pieces.push(Piece::Text("] })"));
                        pieces.push(Piece::DictBatches(&dict.batches, true));
                        pieces.push(Piece::Text(", batches: ["));
                        match &dict.initial {
                            Some(items) => {
                                pieces.push(Piece::Text("])"));
                                pieces.push(Piece::Pairs(items, true));
                                pieces.push(Piece::Text("Some(["));
                            }
                            None => pieces.push(Piece::Text("None")),
                        }
                    }
                    Node::Persistent(pid) => {
                        f.write_str("Persistent(")?;
                        pieces.push(Piece::Text(")"));
                        pieces.push(Piece::Node(pid));
                    }
                    Node::Reduce(reduce) => {
                        f.write_str("Reduce(Reduce { callable: ")?;
                        pieces.push(Piece::Text(" })"));
                        pieces.push(Piece::Node(&reduce.args));
                        pieces.push(Piece::Text(", args: "));
                        pieces.push(Piece::Node(&reduce.callable));
                    }
                    Node::Put(put) => {
                        write!(
                            f,
                            "Put(Put {{ index: {}, form: {:?}, value: ",
                            put.index, put.form
                        )?;
                        pieces.push(Piece::Text(" })"));
                        pieces.push(Piece::Node(&put.value));
                    }
                    Node::None => f.write_str("None")?,
                    Node::Bool(value) => write!(f, "Bool({value:?})")?,
                    Node::Int(value) => write!(f, "Int({value:?})")?,
                    Node::Float(value) => write!(f, "Float({value:?})")?,
                    Node::Bytes(value) => write!(f, "Bytes({value:?})")?,
                    Node::ByteString(value) => write!(f, "ByteString({value:?})")?,
                    Node::Unicode(value) => write!(f, "Unicode({value:?})")?,
                    Node::Global(value) => write!(f, "Global({value:?})")?,
                    Node::Get(value) => write!(f, "Get({value:?})")?,
                    Node::Opaque(value) => write!(f, "Opaque({value:?})")?,
                },
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::thread;

    use super::*;
    use crate::tree::{Bool, Get, Global, Int, MemoForm, Unicode};

    /// The stack of a thread that Rust spawns.
    const SMALL_STACK: usize = 2 << 20;

    /// The nesting depth of the deep trees: far more than a 2 MiB stack
    /// holds with one frame per level.
    const DEPTH: usize = 200_000;

    fn int(value: u8) -> Node {
        Node::Int(Int::U8(value))
    }

    fn sample() -> Node {
        let global = Node::Global(Global {
            module: "m".to_owned(),
            name: "f".to_owned(),
        });
        Node::Put(Put {
            index: 1,
            form: MemoForm::Long,
            value: Box::new(Node::Dict(Dict {
                initial: None,
                batches: vec![
                    Batch {
                        items: vec![
                            (int(1), Node::Persistent(Box::new(int(2)))),
                            (
                                Node::Unicode(Unicode::Binary("k".to_owned())),
                                Node::List(List {
                                    initial: Some(vec![Node::None]),
                                    batches: vec![Batch {
                                        items: vec![Node::Bool(Bool::New(true))],
                                        marked: false,
                                    }],
                                }),
                            ),
                        ],
                        marked: true,
                    },
                    Batch {
                        items: vec![(
                            Node::Get(Get {
                                index: 1,
                                form: MemoForm::Short,
                            }),
                            Node::Reduce(Reduce {
                                callable: Box::new(global),
                                args: Box::new(Node::Tuple(Tuple {
                                    items: vec![int(3)],
                                    marked: true,
                                })),
                            }),
                        )],
                        marked: false,
                    },
                ],
            })),
        })
    }

    /// Wraps a node in `depth` containers of each kind, in turn.
    fn deep(depth: usize) -> Node {
        let mut node = Node::None;
        for level in 0..depth {
            node = match level % 6 {
                0 => Node::Tuple(Tuple {
                    items: vec![node],
                    marked: false,
                }),
                1 => Node::List(List {
                    initial: None,
                    batches: vec![Batch {
                        items: vec![node],
                        marked: false,
                    }],
                }),
                2 => Node::Dict(Dict {
                    initial: Some(vec![(Node::None, node)]),
                    batches: Vec::new(),
                }),
                3 => Node::Persistent(Box::new(node)),
                4 => Node::Reduce(Reduce {
                    callable: Box::new(node),
                    args: Box::new(Node::None),
                }),
                _ => Node::Put(Put {
                    index: 0,
                    form: MemoForm::Short,
                    value: Box::new(node),
                }),
            };
        }
        node
    }

    fn on_a_small_stack(work: impl FnOnce() + Send + 'static) {
        thread::Builder::new()
            .stack_size(SMALL_STACK)
            .spawn(work)
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn clone_gives_an_equal_node() {
        let node = sample();
        assert_eq!(node.clone(), node);
        assert_eq!(format!("{:?}", node.clone()), format!("{node:?}"));
    }

    #[test]
    fn nodes_that_differ_in_one_place_are_not_equal() {
        let node = sample();
        let mut other = node.clone();
        let Node::Put(put) = &mut other else {
            panic!("{other:?}");
        };
        let Node::Dict(dict) = &mut *put.value else {
            panic!("{put:?}");
        };
        dict.batches[0].marked = false;
        assert_ne!(node, other);
        assert_ne!(Node::None, Node::Bool(Bool::New(false)));
        assert_ne!(int(1), int(2));
    }

    #[test]
    fn debug_has_the_form_of_a_derived_debug() {
        let node = Node::Tuple(Tuple {
            items: vec![
                int(1),
                Node::Dict(Dict {
                    initial: Some(vec![(Node::None, int(2))]),
                    batches: vec![Batch {
                        items: vec![(int(3), int(4))],
                        marked: true,
                    }],
                }),
            ],
            marked: false,
        });
        assert_eq!(
            format!("{node:?}"),
            "Tuple(Tuple { items: [Int(U8(1)), Dict(Dict { initial: Some([(None, Int(U8(2)))]), \
             batches: [Batch { items: [(Int(U8(3)), Int(U8(4)))], marked: true }] })], \
             marked: false })"
        );
    }

    #[test]
    fn a_deep_node_drops_clones_compares_and_prints_on_a_small_stack() {
        on_a_small_stack(|| {
            let node = deep(DEPTH);
            let copy = node.clone();
            assert!(copy == node);
            let text = format!("{copy:?}");
            assert!(text.len() > DEPTH);
            assert_eq!(node.walk().count(), copy.walk().count());
            drop(copy);
            drop(node);
        });
    }

    #[test]
    fn a_long_list_drops_on_a_small_stack() {
        on_a_small_stack(|| {
            let items = (0..DEPTH).map(|_| deep(6)).collect();
            drop(Node::Tuple(Tuple {
                items,
                marked: true,
            }));
        });
    }
}
