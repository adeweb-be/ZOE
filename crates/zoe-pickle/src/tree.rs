//! The generic tree of a record.
//!
//! A node keeps its value and the opcode that wrote it. The encoder needs
//! both to write the same bytes again.

/// The decoded form of one record: the class pickle and the state pickle.
#[derive(Clone, Debug, PartialEq)]
pub struct Tree {
    pub(crate) class: Pickle,
    pub(crate) state: Pickle,
}

/// One pickle of a record.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Pickle {
    /// The argument of the PROTO opcode, if the pickle starts with one.
    pub(crate) protocol: Option<u8>,
    /// The value that the pickle returns at STOP.
    pub(crate) root: Node,
}

impl Tree {
    /// Returns the root of the class pickle.
    pub fn class(&self) -> &Node {
        &self.class.root
    }

    /// Returns the root of the state pickle.
    pub fn state(&self) -> &Node {
        &self.state.root
    }

    /// Returns the module and the name of the class of the record.
    ///
    /// The name comes from the class pickle. The class pickle is a GLOBAL,
    /// or a tuple whose first item is a GLOBAL (the class and the arguments
    /// of `__getnewargs__`). Other forms give `None`.
    pub fn class_name(&self) -> Option<(&str, &str)> {
        let root = self.class().value();
        let global = match root {
            Node::Tuple(tuple) => tuple.items.first()?.value(),
            other => other,
        };
        match global {
            Node::Global(global) => Some((&global.module, &global.name)),
            _ => None,
        }
    }

    /// Returns the value that a node stands for.
    ///
    /// For a [`Node::Get`], this is the value that the matching
    /// [`Node::Put`] stores. The put can be in the class pickle or in the
    /// state pickle, because ZODB writes both pickles with one memo. A put
    /// inside an opaque node is not visible, so its gets give `None`.
    /// For a [`Node::Put`], this is the value inside it. Other nodes stay as
    /// they are.
    pub fn resolve<'a>(&'a self, node: &'a Node) -> Option<&'a Node> {
        let Node::Get(get) = node.value() else {
            return Some(node.value());
        };
        self.class()
            .walk()
            .chain(self.state().walk())
            .find_map(|candidate| match candidate {
                Node::Put(put) if put.index == get.index => Some(put.value.value()),
                _ => None,
            })
    }
}

/// One value of a pickle.
#[derive(Clone, Debug, PartialEq)]
pub enum Node {
    /// NONE.
    None,
    /// NEWTRUE or NEWFALSE.
    Bool(bool),
    /// An integer.
    Int(Int),
    /// BINFLOAT: a 64-bit float.
    Float(f64),
    /// SHORT_BINBYTES or BINBYTES: Python 3 `bytes`. In protocol 3,
    /// `zodbpickle.binary` also writes these opcodes.
    Bytes(Bytes),
    /// SHORT_BINSTRING or BINSTRING: a Python 2 `str` of 8-bit bytes.
    ByteString(Bytes),
    /// BINUNICODE: a `str`, valid UTF-8.
    Unicode(String),
    /// A tuple.
    Tuple(Tuple),
    /// A list: EMPTY_LIST, then one batch per APPEND or APPENDS.
    List(Vec<Batch<Node>>),
    /// A dict: EMPTY_DICT, then one batch per SETITEM or SETITEMS.
    Dict(Vec<Batch<(Node, Node)>>),
    /// GLOBAL: a reference to a class or a function by module and name.
    Global(Global),
    /// BINPERSID: a reference to another persistent object. The node holds
    /// the persistent id. ZODB writes `(oid, class)`, or the oid alone for
    /// a class with `__getnewargs__`, or `['w', (oid,)]` for a weak
    /// reference.
    Persistent(Box<Node>),
    /// REDUCE of a constructor that the tree knows: `builtins.set` or
    /// `builtins.frozenset`.
    Reduce(Reduce),
    /// A value that the pickle stores in the memo (BINPUT or LONG_BINPUT).
    Put(Put),
    /// BINGET or LONG_BINGET: a reference to a value in the memo.
    Get(Get),
    /// A construct that the tree does not model. It keeps its bytes.
    Opaque(Opaque),
}

/// An integer, with the opcode that holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Int {
    /// BININT1: 0 to 255.
    U8(u8),
    /// BININT2: 0 to 65,535.
    U16(u16),
    /// BININT: a signed 32-bit integer.
    I32(i32),
    /// LONG1 or LONG4: an integer of any size.
    Long {
        /// The value in two's complement, little-endian, as stored.
        bytes: Vec<u8>,
        /// Short for LONG1, long for LONG4.
        width: Width,
    },
}

/// The width of the length or index argument of an opcode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Width {
    /// One byte: SHORT_BINBYTES, SHORT_BINSTRING, LONG1, BINPUT, BINGET.
    Short,
    /// Four bytes: BINBYTES, BINSTRING, LONG4, LONG_BINPUT, LONG_BINGET.
    Long,
}

/// A byte string, with the width of its length.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bytes {
    /// The bytes.
    pub data: Vec<u8>,
    /// The width of the length argument.
    pub width: Width,
}

/// A tuple, with the opcodes that build it.
#[derive(Clone, Debug, PartialEq)]
pub struct Tuple {
    /// The items.
    pub items: Vec<Node>,
    /// True for MARK, the items, TUPLE. False for EMPTY_TUPLE, TUPLE1,
    /// TUPLE2 or TUPLE3.
    pub marked: bool,
}

/// The items that one opcode adds to a list or a dict.
#[derive(Clone, Debug, PartialEq)]
pub struct Batch<T> {
    /// The items, in order.
    pub items: Vec<T>,
    /// True for MARK, the items, APPENDS or SETITEMS. False for one item
    /// and APPEND or SETITEM.
    pub marked: bool,
}

/// A reference to a class or a function.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Global {
    /// The module, for example `persistent.mapping`.
    pub module: String,
    /// The name in the module, for example `PersistentMapping`.
    pub name: String,
}

/// A call of a known constructor with a tuple of arguments.
#[derive(Clone, Debug, PartialEq)]
pub struct Reduce {
    /// The constructor: a GLOBAL, or a memo reference to one.
    pub callable: Box<Node>,
    /// The arguments: a tuple.
    pub args: Box<Node>,
}

/// A value and the memo index that the pickle stores it under.
#[derive(Clone, Debug, PartialEq)]
pub struct Put {
    /// The memo index.
    pub index: u32,
    /// Short for BINPUT, long for LONG_BINPUT.
    pub width: Width,
    /// The value. The put comes after the opcode that creates the value and
    /// before the opcodes that fill a list or a dict.
    pub value: Box<Node>,
}

/// A reference to a value in the memo.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Get {
    /// The memo index.
    pub index: u32,
    /// Short for BINGET, long for LONG_BINGET.
    pub width: Width,
}

/// The bytes of a construct that the tree does not model.
///
/// The bytes are one contiguous part of a pickle that pushes one value.
/// They can contain memo puts and gets. The encoder writes them unchanged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Opaque {
    /// The original bytes.
    pub bytes: Vec<u8>,
    /// The first opcode in the bytes that the tree does not model, for
    /// example REDUCE of an unknown callable, NEWOBJ or EXT1.
    pub opcode: u8,
}

impl Node {
    /// Returns the value inside memo puts. Other nodes stay as they are.
    pub fn value(&self) -> &Node {
        match self {
            Node::Put(put) => put.value.value(),
            other => other,
        }
    }

    pub(crate) fn value_mut(&mut self) -> &mut Node {
        match self {
            Node::Put(put) => put.value.value_mut(),
            other => other,
        }
    }

    /// Returns an iterator over this node and all nodes below it, depth
    /// first, in pickle order.
    pub fn walk(&self) -> Walk<'_> {
        Walk { stack: vec![self] }
    }

    fn push_children<'a>(&'a self, out: &mut Vec<&'a Node>) {
        match self {
            Node::Tuple(tuple) => out.extend(&tuple.items),
            Node::List(batches) => out.extend(batches.iter().flat_map(|b| &b.items)),
            Node::Dict(batches) => {
                for (key, value) in batches.iter().flat_map(|b| &b.items) {
                    out.push(key);
                    out.push(value);
                }
            }
            Node::Persistent(pid) => out.push(pid),
            Node::Reduce(reduce) => {
                out.push(&reduce.callable);
                out.push(&reduce.args);
            }
            Node::Put(put) => out.push(&put.value),
            _ => {}
        }
    }
}

/// A depth-first iterator over a node and the nodes below it.
pub struct Walk<'a> {
    stack: Vec<&'a Node>,
}

impl<'a> Iterator for Walk<'a> {
    type Item = &'a Node;

    fn next(&mut self) -> Option<&'a Node> {
        let node = self.stack.pop()?;
        let first = self.stack.len();
        node.push_children(&mut self.stack);
        self.stack[first..].reverse();
        Some(node)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(value: &str) -> Node {
        Node::Unicode(value.to_owned())
    }

    #[test]
    fn walk_visits_every_node_in_pickle_order() {
        let node = Node::Dict(vec![Batch {
            items: vec![
                (
                    text("a"),
                    Node::Tuple(Tuple {
                        items: vec![text("b"), text("c")],
                        marked: false,
                    }),
                ),
                (text("d"), Node::None),
            ],
            marked: true,
        }]);
        let seen: Vec<String> = node
            .walk()
            .map(|node| match node {
                Node::Unicode(value) => value.clone(),
                Node::Tuple(_) => "tuple".to_owned(),
                Node::Dict(_) => "dict".to_owned(),
                Node::None => "none".to_owned(),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(seen, ["dict", "a", "tuple", "b", "c", "d", "none"]);
    }

    #[test]
    fn value_looks_through_memo_puts() {
        let node = Node::Put(Put {
            index: 1,
            width: Width::Short,
            value: Box::new(Node::Bool(true)),
        });
        assert_eq!(node.value(), &Node::Bool(true));
        assert_eq!(Node::None.value(), &Node::None);
    }
}
