//! The generic tree of a record.
//!
//! A node keeps its value and the opcode form that wrote it. The encoder
//! needs both to write the same bytes again.
//!
//! No function here calls itself once per nesting level. The drop, the
//! clone, the comparison and the debug output of a node also use explicit
//! stacks (see `iterative.rs`). So a deep tree cannot overflow the call
//! stack.

use std::collections::HashMap;

use crate::encode::order::{Op, Order};
use crate::opcode::memo_puts;
use crate::text;

/// The decoded form of one record: the class pickle and the state pickle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tree {
    pub(crate) class: Pickle,
    pub(crate) state: Pickle,
}

/// One pickle of a record.
#[derive(Clone, Debug, PartialEq, Eq)]
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
    /// The name comes from the class pickle. ZODB writes it in one of four
    /// formats, numbered as in ZODB's `serialize.py`:
    ///
    /// 1. the class;
    /// 2. `(class, newargs)`;
    /// 3. `((module, name), None)`;
    /// 7. `((module, name), newargs)`.
    ///
    /// ZODB still reads three older formats. They have the shapes of the
    /// current formats, so this method reads them in the same way:
    ///
    /// - format 4, `((module, name), initargs)`, reads as format 7;
    /// - format 5, `(class, None)`, reads as format 2;
    /// - format 6, `(class, initargs)`, reads as format 2.
    ///
    /// The class is a GLOBAL. A persistent class is a persistent reference,
    /// and the record does not hold its name, so this method returns `None`
    /// for it. Another shape also gives `None`.
    pub fn class_name(&self) -> Option<(&str, &str)> {
        let root = self.resolve(self.class())?;
        let class = match root {
            Node::Tuple(tuple) => match &tuple.items[..] {
                [class, _] => self.resolve(class)?,
                _ => return None,
            },
            other => other,
        };
        match class {
            Node::Global(global) => Some((&global.module, &global.name)),
            Node::Tuple(pair) => match &pair.items[..] {
                [module, name] => Some((text(self.resolve(module)?)?, text(self.resolve(name)?)?)),
                _ => None,
            },
            _ => None,
        }
    }

    /// Returns the value that a node stands for.
    ///
    /// For a [`Node::Get`] of this tree, this is the value that the latest
    /// put of its memo index before it stores, as in Python's unpickler.
    /// The put can be in the class pickle or in the state pickle, because
    /// ZODB writes both pickles with one memo. If that put is inside an
    /// opaque node, the value is not known, and the result is `None`. A get
    /// that is not a node of this tree also gives `None`.
    ///
    /// For a [`Node::Put`], this is the value inside it. Other nodes stay as
    /// they are.
    ///
    /// For a get, the cost is one pass over the tree. To resolve many gets,
    /// use [`Tree::resolver`]: it makes one pass for all of them.
    pub fn resolve<'a>(&'a self, node: &'a Node) -> Option<&'a Node> {
        let node = node.value();
        if !matches!(node, Node::Get(_)) {
            return Some(node);
        }
        let mut found = None;
        self.replay(|get, value| {
            let target = std::ptr::eq(get, node);
            if target {
                found = value;
            }
            target
        });
        found
    }

    /// Returns a resolver that knows the value of each get of this tree.
    ///
    /// The resolver makes one pass over the tree. After that, each lookup
    /// takes constant time.
    pub fn resolver(&self) -> Resolver<'_> {
        let mut values = HashMap::new();
        self.replay(|get, value| {
            values.insert(std::ptr::from_ref(get).addr(), value);
            false
        });
        Resolver { values }
    }

    /// Runs the memo of the unpickler over both pickles, in pickle order.
    /// For each get, calls `visit` with the get and its value. Stops when
    /// `visit` returns true, or at a node that the encoder refuses.
    fn replay<'a>(&'a self, mut visit: impl FnMut(&'a Node, Option<&'a Node>) -> bool) {
        // An index gives the value of its latest put, or `None` if an
        // opaque node holds that put.
        let mut memo: HashMap<u32, Option<&Node>> = HashMap::new();
        for root in [self.class(), self.state()] {
            for op in Order::new(root) {
                let Ok(op) = op else {
                    return;
                };
                match op {
                    Op::Put(put, Node::Get(earlier)) => {
                        let value = memo.get(&earlier.index).copied().flatten();
                        memo.insert(put.index, value);
                    }
                    Op::Put(put, value) => {
                        memo.insert(put.index, Some(value));
                    }
                    Op::Leaf(leaf @ Node::Get(get)) => {
                        if visit(leaf, memo.get(&get.index).copied().flatten()) {
                            return;
                        }
                    }
                    Op::Leaf(Node::Opaque(opaque)) => {
                        for index in memo_puts(&opaque.bytes) {
                            memo.insert(index, None);
                        }
                    }
                    Op::Leaf(_) | Op::Byte(_) => {}
                }
            }
        }
    }
}

/// The values of all the gets of a tree, from one pass over it. See
/// [`Tree::resolver`].
#[derive(Clone, Debug)]
pub struct Resolver<'a> {
    /// The value of each get, by the address of the get node. The borrow of
    /// the tree keeps each address valid and unique.
    values: HashMap<usize, Option<&'a Node>>,
}

impl<'a> Resolver<'a> {
    /// Returns the value that a node stands for, as [`Tree::resolve`] does,
    /// in constant time.
    pub fn resolve(&self, node: &'a Node) -> Option<&'a Node> {
        let node = node.value();
        match node {
            Node::Get(_) => self
                .values
                .get(&std::ptr::from_ref(node).addr())
                .copied()
                .flatten(),
            other => Some(other),
        }
    }
}

/// Returns the text of a str node, if it can borrow it.
fn text(node: &Node) -> Option<&str> {
    match node {
        Node::Unicode(Unicode::Binary(text)) => Some(text),
        Node::Unicode(Unicode::Text(raw)) if raw.is_ascii() && !raw.contains(&b'\\') => {
            std::str::from_utf8(raw).ok()
        }
        Node::ByteString(ByteString::Binary(bytes)) => std::str::from_utf8(&bytes.data).ok(),
        Node::ByteString(ByteString::Text(raw)) => {
            let inner = raw.get(1..raw.len().checked_sub(1)?)?;
            if inner.contains(&b'\\') {
                return None;
            }
            std::str::from_utf8(inner).ok()
        }
        _ => None,
    }
}

/// One value of a pickle.
///
/// The drop, the clone, the comparison and the debug output of a node use
/// explicit stacks, so they work at any depth.
pub enum Node {
    /// NONE.
    None,
    /// A bool.
    Bool(Bool),
    /// An int.
    Int(Int),
    /// A float.
    Float(Float),
    /// SHORT_BINBYTES or BINBYTES: Python 3 `bytes`. In protocol 3,
    /// `zodbpickle.binary` also writes these opcodes.
    Bytes(Bytes),
    /// A Python 2 `str` of 8-bit bytes.
    ByteString(ByteString),
    /// A `str`.
    Unicode(Unicode),
    /// A tuple.
    Tuple(Tuple),
    /// A list.
    List(List),
    /// A dict.
    Dict(Dict),
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
    /// A value that the pickle stores in the memo.
    Put(Put),
    /// A reference to a value in the memo.
    Get(Get),
    /// A construct that the tree does not model. It keeps its bytes.
    Opaque(Opaque),
}

/// A bool, with the opcode that holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Bool {
    /// NEWTRUE or NEWFALSE (protocol 2 and later).
    New(bool),
    /// INT with a text of two characters that gives 0 or 1, as stored,
    /// without the newline. Python 2 writes `01` for True and `00` for
    /// False (protocols 0 and 1). The C unpickler of Python also reads
    /// `+1`, `+0` and `-0` as a bool.
    Text(String),
}

impl Bool {
    /// Returns the value of the bool.
    pub fn value(&self) -> bool {
        match self {
            Bool::New(value) => *value,
            Bool::Text(text) => text::int_bool(text.as_bytes()) == Some(true),
        }
    }
}

/// An int, with the opcode that holds it.
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
    /// INT: the decimal text, as stored, without the newline. Python 2
    /// writes an int that does not fit 32 bits in this form.
    Text(String),
    /// LONG: the decimal text, as stored, without the newline. Python 2
    /// writes a `long` in this form before protocol 2, for example
    /// `-1208925819614629174706176L`.
    LongText(String),
}

impl Int {
    /// Returns the value of the int, or `None` when it does not fit 128
    /// bits. A LONG1 or LONG4 of more than 16 bytes does not fit.
    pub fn to_i128(&self) -> Option<i128> {
        match self {
            Int::U8(value) => Some(i128::from(*value)),
            Int::U16(value) => Some(i128::from(*value)),
            Int::I32(value) => Some(i128::from(*value)),
            Int::Long { bytes, .. } => Int::long_to_i128(bytes),
            Int::Text(text) => text.trim().parse().ok(),
            Int::LongText(text) => text.trim().trim_end_matches('L').parse().ok(),
        }
    }

    /// Returns the int of the bytes of LONG1 or LONG4: two's complement,
    /// little-endian. Returns `None` for more than 16 bytes.
    pub fn long_to_i128(bytes: &[u8]) -> Option<i128> {
        if bytes.len() > 16 {
            return None;
        }
        let fill = if bytes.last().is_some_and(|byte| byte & 0x80 != 0) {
            0xff
        } else {
            0
        };
        let mut le = [fill; 16];
        le[..bytes.len()].copy_from_slice(bytes);
        Some(i128::from_le_bytes(le))
    }
}

/// A float, with the opcode that holds it.
#[derive(Clone, Debug)]
pub enum Float {
    /// BINFLOAT: a 64-bit float.
    Binary(f64),
    /// FLOAT: the text, as stored, without the newline.
    Text(String),
}

/// Two floats are equal if they have the same bits. So a NaN equals
/// itself, and 0.0 does not equal -0.0.
impl PartialEq for Float {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Float::Binary(a), Float::Binary(b)) => a.to_bits() == b.to_bits(),
            (Float::Text(a), Float::Text(b)) => a == b,
            _ => false,
        }
    }
}

impl Eq for Float {}

/// The width of the length argument of an opcode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Width {
    /// One byte: SHORT_BINBYTES, SHORT_BINSTRING, LONG1.
    Short,
    /// Four bytes: BINBYTES, BINSTRING, LONG4.
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

/// A Python 2 `str`, with the opcode that holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ByteString {
    /// SHORT_BINSTRING or BINSTRING.
    Binary(Bytes),
    /// STRING: the quoted text with escapes, as `repr` gives it, without
    /// the newline.
    Text(Vec<u8>),
}

/// A `str`, with the opcode that holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unicode {
    /// BINUNICODE: the text, valid UTF-8.
    Binary(String),
    /// UNICODE: the text in raw-unicode-escape, as stored, without the
    /// newline.
    Text(Vec<u8>),
}

/// A tuple, with the opcodes that build it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tuple {
    /// The items.
    pub items: Vec<Node>,
    /// True for MARK, the items, TUPLE. False for EMPTY_TUPLE, TUPLE1,
    /// TUPLE2 or TUPLE3.
    pub marked: bool,
}

/// A list, with the opcodes that build it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct List {
    /// The items for MARK, the items, LIST (protocol 0). `None` for
    /// EMPTY_LIST.
    pub initial: Option<Vec<Node>>,
    /// The items that APPEND or APPENDS add, one batch per opcode.
    pub batches: Vec<Batch<Node>>,
}

/// A dict, with the opcodes that build it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dict {
    /// The items for MARK, the items, DICT (protocol 0). `None` for
    /// EMPTY_DICT.
    pub initial: Option<Vec<(Node, Node)>>,
    /// The items that SETITEM or SETITEMS add, one batch per opcode.
    pub batches: Vec<Batch<(Node, Node)>>,
}

impl List {
    /// Returns the items of the list, in order: the initial items, then
    /// the items of each batch.
    pub fn items(&self) -> impl Iterator<Item = &Node> {
        let batches = self.batches.iter().flat_map(|batch| &batch.items);
        self.initial.iter().flatten().chain(batches)
    }
}

impl Dict {
    /// Returns the entries of the dict, in order: the initial entries, then
    /// the entries of each batch.
    pub fn entries(&self) -> impl Iterator<Item = &(Node, Node)> {
        let batches = self.batches.iter().flat_map(|batch| &batch.items);
        self.initial.iter().flatten().chain(batches)
    }
}

/// The items that one opcode adds to a list or a dict.
#[derive(Clone, Debug, PartialEq, Eq)]
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reduce {
    /// The constructor: a GLOBAL, or a memo reference to one.
    pub callable: Box<Node>,
    /// The arguments: a tuple.
    pub args: Box<Node>,
}

/// How a memo opcode writes its index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoForm {
    /// BINPUT or BINGET: one byte.
    Short,
    /// LONG_BINPUT or LONG_BINGET: four bytes.
    Long,
    /// PUT or GET: decimal text and a newline (protocol 0).
    Text,
}

/// A value and the memo index that the pickle stores it under.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Put {
    /// The memo index.
    pub index: u32,
    /// The opcode form of the put.
    pub form: MemoForm,
    /// The value. The put comes after the opcodes that create the value and
    /// before the opcodes that fill a list or a dict.
    pub value: Box<Node>,
}

/// A reference to a value in the memo.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Get {
    /// The memo index.
    pub index: u32,
    /// The opcode form of the get.
    pub form: MemoForm,
}

/// The bytes of a construct that the tree does not model.
///
/// The bytes are one contiguous part of a pickle that pushes one value.
/// They are whole opcodes, and they can contain memo puts and gets. The
/// encoder writes them unchanged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Opaque {
    /// The original bytes.
    pub bytes: Vec<u8>,
    /// A label for people: the opcode that consumed the span. Examples are
    /// REDUCE of an unknown callable, NEWOBJ, EXT1 and POP. When the span
    /// grows over an older opaque node, the label of that node can stay.
    /// The encoder does not read this field.
    pub opcode: u8,
}

impl Node {
    /// Returns the value inside memo puts. Other nodes stay as they are.
    pub fn value(&self) -> &Node {
        let mut node = self;
        while let Node::Put(put) = node {
            node = &put.value;
        }
        node
    }

    /// Returns an iterator over this node and all nodes below it, depth
    /// first. A node comes before its children. The children come in
    /// pickle order.
    pub fn walk(&self) -> Walk<'_> {
        Walk {
            stack: vec![self],
            children: Vec::new(),
        }
    }

    /// Tells if the node has no children.
    pub(crate) fn is_leaf(&self) -> bool {
        !matches!(
            self,
            Node::Tuple(_)
                | Node::List(_)
                | Node::Dict(_)
                | Node::Persistent(_)
                | Node::Reduce(_)
                | Node::Put(_)
        )
    }

    /// Adds the children of the node to `out`, in pickle order.
    pub(crate) fn children<'a>(&'a self, out: &mut Vec<&'a Node>) {
        match self {
            Node::Tuple(tuple) => out.extend(&tuple.items),
            Node::List(list) => out.extend(list.items()),
            Node::Dict(dict) => {
                for (key, value) in dict.entries() {
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

    /// Adds the children of the node to `out`, in pickle order, for change.
    pub(crate) fn children_mut<'a>(&'a mut self, out: &mut Vec<&'a mut Node>) {
        match self {
            Node::Tuple(tuple) => out.extend(&mut tuple.items),
            Node::List(list) => {
                out.extend(list.initial.iter_mut().flatten());
                out.extend(list.batches.iter_mut().flat_map(|batch| &mut batch.items));
            }
            Node::Dict(dict) => {
                let batches = dict.batches.iter_mut().flat_map(|batch| &mut batch.items);
                for (key, value) in dict.initial.iter_mut().flatten().chain(batches) {
                    out.push(key);
                    out.push(value);
                }
            }
            Node::Persistent(pid) => out.push(pid),
            Node::Reduce(reduce) => {
                out.push(&mut reduce.callable);
                out.push(&mut reduce.args);
            }
            Node::Put(put) => out.push(&mut put.value),
            _ => {}
        }
    }
}

/// A depth-first iterator over a node and the nodes below it.
pub struct Walk<'a> {
    stack: Vec<&'a Node>,
    children: Vec<&'a Node>,
}

impl<'a> Iterator for Walk<'a> {
    type Item = &'a Node;

    fn next(&mut self) -> Option<&'a Node> {
        let node = self.stack.pop()?;
        node.children(&mut self.children);
        self.stack.extend(self.children.drain(..).rev());
        Some(node)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(value: &str) -> Node {
        Node::Unicode(Unicode::Binary(value.to_owned()))
    }

    fn tuple(items: Vec<Node>) -> Node {
        Node::Tuple(Tuple {
            items,
            marked: false,
        })
    }

    #[test]
    fn walk_visits_every_node_in_pickle_order() {
        let node = Node::Dict(Dict {
            initial: Some(vec![(text("a"), tuple(vec![text("b"), text("c")]))]),
            batches: vec![Batch {
                items: vec![(text("d"), Node::None)],
                marked: true,
            }],
        });
        let seen: Vec<String> = node
            .walk()
            .map(|node| match node {
                Node::Unicode(Unicode::Binary(value)) => value.clone(),
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
        let mut node = Node::Bool(Bool::New(true));
        for index in 0..3 {
            node = Node::Put(Put {
                index,
                form: MemoForm::Short,
                value: Box::new(node),
            });
        }
        assert_eq!(node.value(), &Node::Bool(Bool::New(true)));
        assert_eq!(Node::None.value(), &Node::None);
    }

    /// Decodes a record whose class pickle is GLOBAL m C, BINPUT 0.
    fn decode(state: &[u8]) -> Tree {
        crate::decode_record(&[&b"\x80\x03cm\nC\nq\x00.\x80\x03"[..], state, b"."].concat())
            .unwrap()
    }

    /// Returns the values that the gets of a tree resolve to, in order.
    /// Checks that the resolver agrees with `Tree::resolve` on each get.
    fn resolved(tree: &Tree) -> Vec<Option<&Node>> {
        let resolver = tree.resolver();
        let gets = || {
            tree.state()
                .walk()
                .filter(|node| matches!(node, Node::Get(_)))
        };
        let one_pass: Vec<_> = gets().map(|get| resolver.resolve(get)).collect();
        let one_off: Vec<_> = gets().map(|get| tree.resolve(get)).collect();
        assert_eq!(one_pass, one_off);
        one_off
    }

    fn int(value: u8) -> Node {
        Node::Int(Int::U8(value))
    }

    #[test]
    fn a_get_resolves_to_the_latest_put_before_it_across_the_pickles() {
        let tree = decode(b"(h\x00K\x07q\x00h\x00t");
        let global = Node::Global(Global {
            module: "m".to_owned(),
            name: "C".to_owned(),
        });
        assert_eq!(resolved(&tree), [Some(&global), Some(&int(7))]);
    }

    #[test]
    fn a_get_of_a_get_resolves_where_its_put_is() {
        // Put 2 stores the value of put 1 at that time: 7, not 8.
        let tree = decode(b"(K\x07q\x01h\x01q\x02K\x08q\x01h\x02t");
        assert_eq!(resolved(&tree), [Some(&int(7)), Some(&int(7))]);
    }

    #[test]
    fn a_get_inside_a_list_resolves_to_the_list() {
        let tree = decode(b"]q\x01(h\x01e");
        let [Some(Node::List(list))] = resolved(&tree)[..] else {
            panic!("{tree:?}");
        };
        assert_eq!(list.batches.len(), 1);
    }

    #[test]
    fn a_put_inside_an_opaque_node_hides_its_value() {
        let tree = decode(b"(K\x07q\x01cm\nf\nq\x01)Rh\x01K\x08q\x01h\x01t");
        assert_eq!(resolved(&tree), [None, Some(&int(8))]);
    }

    #[test]
    fn resolve_gives_other_nodes_as_they_are() {
        let tree = decode(b"K\x07q\x01N\x86");
        let Node::Tuple(tuple) = tree.state() else {
            panic!("{tree:?}");
        };
        assert_eq!(tree.resolve(&tuple.items[0]), Some(&int(7)));
        assert_eq!(tree.resolve(&tuple.items[1]), Some(&Node::None));
        let stranger = Node::Get(Get {
            index: 1,
            form: MemoForm::Short,
        });
        assert_eq!(tree.resolve(&stranger), None);
    }

    #[test]
    fn the_resolver_resolves_every_get_of_a_large_record_in_one_pass() {
        // A list of persistent references: the class in each persistent id
        // is a get of the GLOBAL in the class pickle, as ZODB writes it.
        const REFERENCES: usize = 40_000;
        let mut state = b"](".to_vec();
        for oid in 0..REFERENCES as u64 {
            state.extend(b"C\x08");
            state.extend(oid.to_be_bytes());
            state.extend(b"h\x00\x86Q");
        }
        state.push(b'e');
        let tree = decode(&state);
        let global = Node::Global(Global {
            module: "m".to_owned(),
            name: "C".to_owned(),
        });
        let started = std::time::Instant::now();
        let resolver = tree.resolver();
        let resolved = tree
            .state()
            .walk()
            .filter(|node| matches!(node, Node::Get(_)))
            .filter(|get| resolver.resolve(get) == Some(&global))
            .count();
        let elapsed = started.elapsed();
        assert_eq!(resolved, REFERENCES);
        // One pass takes some tens of milliseconds, also in a debug build.
        // One pass per get takes minutes.
        assert!(elapsed.as_secs() < 10, "{elapsed:?}");
    }

    #[test]
    fn the_resolver_gives_none_for_a_get_of_another_tree() {
        let tree = decode(b"h\x00");
        let other = decode(b"h\x00");
        let resolver = tree.resolver();
        assert!(resolver.resolve(tree.state()).is_some());
        assert_eq!(resolver.resolve(other.state()), None);
        assert_eq!(resolver.resolve(&Node::None), Some(&Node::None));
    }

    #[test]
    fn class_name_reads_every_format() {
        let state = b"\x80\x03}.";
        for (format, class) in [
            (
                "4: ((module, name), initargs)",
                &b"\x80\x03X\x01\x00\x00\x00mX\x01\x00\x00\x00C\x86K\x01\x85\x86."[..],
            ),
            ("5: (class, None)", b"\x80\x03cm\nC\nN\x86."),
            ("6: (class, initargs)", b"\x80\x03cm\nC\nK\x01\x85\x86."),
            (
                "3, Python 2 strings",
                b"((U\x01mq\x00U\x01Cq\x01tq\x02Ntq\x03.",
            ),
            ("3, protocol 0", b"((S'm'\np0\nS'C'\np1\ntp2\nNtp3\n."),
            ("3, protocol 0 unicode", b"((Vm\np0\nVC\np1\ntp2\nNtp3\n."),
        ] {
            let tree = crate::decode_record(&[class, state].concat()).unwrap();
            assert_eq!(tree.class_name(), Some(("m", "C")), "format {format}");
        }
        // The module and the name are one interned str: the second is a get.
        let class = b"\x80\x03X\x01\x00\x00\x00mq\x00h\x00\x86q\x01N\x86q\x02.";
        let tree = crate::decode_record(&[&class[..], state].concat()).unwrap();
        assert_eq!(tree.class_name(), Some(("m", "m")));
    }

    #[test]
    fn class_name_is_none_for_a_persistent_class_or_another_shape() {
        let state = b"\x80\x03}.";
        for class in [
            &b"\x80\x03C\x08\x00\x00\x00\x00\x00\x00\x00\x01Q."[..],
            b"\x80\x03C\x08\x00\x00\x00\x00\x00\x00\x00\x01Q)\x86.",
            b"\x80\x03N.",
            b"\x80\x03cm\nC\nNN\x87.",
            b"\x80\x03K\x01K\x02\x86N\x86.",
            b"\x80\x03S'a\\x00'\nVC\n\x86N\x86.",
        ] {
            let tree = crate::decode_record(&[class, state].concat()).unwrap();
            assert_eq!(tree.class_name(), None, "{class:?}");
        }
    }

    #[test]
    fn a_bool_gives_its_value_in_each_form() {
        assert!(Bool::New(true).value());
        assert!(!Bool::New(false).value());
        for (text, value) in [("01", true), ("+1", true), ("00", false), ("-0", false)] {
            assert_eq!(Bool::Text(text.to_owned()).value(), value, "{text}");
        }
    }

    #[test]
    fn floats_compare_by_their_bits() {
        let nan = Float::Binary(f64::NAN);
        assert_eq!(nan, nan.clone());
        assert_ne!(Float::Binary(0.0), Float::Binary(-0.0));
        assert_ne!(Float::Binary(1.5), Float::Text("1.5".to_owned()));
    }
}
