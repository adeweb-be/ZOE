//! The decoder: a record to a tree.
//!
//! The decoder runs the stack machine of pickle over the opcodes, but it
//! builds nodes instead of objects. It never imports or calls anything. Its
//! stack is a vector, so the depth of a value does not grow the call stack.
//!
//! Each stack entry keeps the offset where its bytes start. So the bytes of
//! a construct that the tree does not model are one known span, and the
//! decoder keeps them in an opaque node. During the run, an opaque node
//! holds no bytes: the decoder only records its span. A span that grows
//! over older spans replaces them. At the end, the decoder copies the bytes
//! of the spans that remain, so it copies each byte once at most.

use std::collections::HashMap;
use std::ops::Range;

use crate::error::Error;
use crate::opcode::*;
use crate::text;
use crate::tree::{
    Batch, Bool, ByteString, Bytes, Dict, Float, Get, Global, Int, List, MemoForm, Node, Opaque,
    Pickle, Put, Reduce, Tree, Tuple, Unicode, Width,
};

/// The node budget of [`decode_record`]: 1,048,576 nodes.
///
/// An opcode of one byte can make a node of 56 bytes. So without a limit,
/// the tree of a record can take some 56 times the size of the record. At
/// this budget, the tree takes about 60 MB. The stack of the decoder takes
/// about 90 MB more while it runs. ZODB splits a big BTree into buckets of
/// some hundreds of items, so most records need far fewer nodes. But ZODB
/// also writes and loads a list of a million items, or a dict of some
/// hundreds of thousands. This budget refuses them. A tool that must read
/// every record gives a larger budget to [`decode_record_with`].
pub const NODE_BUDGET: usize = 1 << 20;

/// The limits of one call of [`decode_record_with`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// The most nodes that the decoder makes for the record. The decoder
    /// counts each value, mark and memo put that it makes. Each opcode
    /// makes one node at most, so the count never gets above the length of
    /// the record. A budget equal to that length never refuses a record.
    pub node_budget: usize,
}

impl Default for Limits {
    /// The limits of [`decode_record`]: a budget of [`NODE_BUDGET`] nodes.
    fn default() -> Self {
        Limits {
            node_budget: NODE_BUDGET,
        }
    }
}

/// The callables that a modeled REDUCE can call.
const KNOWN_CONSTRUCTORS: [(&str, &str); 2] = [("builtins", "set"), ("builtins", "frozenset")];

/// Decodes a record: the class pickle, then the state pickle.
///
/// Both pickles share one memo, as ZODB writes them with one pickler. A
/// construct that the tree does not model becomes a [`Node::Opaque`]. The
/// decoder refuses bytes that Python's unpickler refuses. It also refuses a
/// record that needs more than [`NODE_BUDGET`] nodes, with
/// [`Error::Budget`].
pub fn decode_record(bytes: &[u8]) -> Result<Tree, Error> {
    decode_record_with(bytes, Limits::default())
}

/// Decodes a record as [`decode_record`] does, with the given limits.
///
/// A tool that must read every record, such as pack or a converter, gives
/// a node budget equal to the length of the record. Then only malformed
/// bytes give an error.
pub fn decode_record_with(bytes: &[u8], limits: Limits) -> Result<Tree, Error> {
    Decoder::new(bytes, limits.node_budget).record()
}

/// Returns the module and the name of the class of a record, as
/// [`Tree::class_name`] gives them, from the class pickle alone.
///
/// The decoder stops at the end of the class pickle and does not read the
/// state pickle. So the memory stays in proportion to the class pickle,
/// and a state pickle that does not decode does not matter. The node
/// budget is the length of the record, so no budget refuses the class
/// pickle. Returns `None` when the class pickle names no class.
pub fn decode_class_name(bytes: &[u8]) -> Result<Option<(String, String)>, Error> {
    let mut decoder = Decoder::new(bytes, bytes.len());
    let mut class = decoder.pickle()?;
    let mut spans = std::mem::take(&mut decoder.spans).into_iter();
    decoder.copy_spans(&mut class.root, &mut spans)?;
    let tree = Tree {
        class,
        state: Pickle {
            protocol: None,
            root: Node::None,
        },
    };
    Ok(tree
        .class_name()
        .map(|(module, name)| (module.to_owned(), name.to_owned())))
}

/// One stack entry.
struct Entry {
    /// The offset where the bytes of the entry start.
    start: usize,
    kind: Kind,
}

enum Kind {
    /// MARK. `junk` is POP or POP_MARK if bytes that these opcodes left come
    /// just before the mark.
    Mark { junk: Option<u8> },
    /// A value, and the memo puts after it, in order. The puts wrap the
    /// value when the entry leaves the stack. Until then, an opcode that
    /// fills a list or a dict reaches the value directly.
    Value {
        node: Node,
        puts: Vec<(u32, MemoForm)>,
    },
}

impl Entry {
    fn into_node(self) -> Option<Node> {
        match self.kind {
            Kind::Value { mut node, puts } => {
                for (index, form) in puts {
                    node = Node::Put(Put {
                        index,
                        form,
                        value: Box::new(node),
                    });
                }
                Some(node)
            }
            Kind::Mark { .. } => None,
        }
    }
}

/// The items that APPEND, APPENDS, SETITEM or SETITEMS add.
enum Items {
    List(Batch<Node>),
    Dict(Batch<(Node, Node)>),
}

/// The stack machine of one pickle.
#[derive(Default)]
struct Machine {
    stack: Vec<Entry>,
    /// The start of the bytes that POP or POP_MARK took off the stack, and
    /// that opcode. These bytes stay in the record, so the next entry takes
    /// them in, as an opaque node.
    junk: Option<(usize, u8)>,
}

/// What the decoder knows about a memo index.
struct Slot {
    /// The offset of the latest put of the index.
    at: usize,
    /// True if that put stores a known constructor.
    constructor: bool,
}

struct Decoder<'a> {
    bytes: &'a [u8],
    pos: usize,
    memo: HashMap<u32, Slot>,
    /// The spans of the opaque nodes, sorted and disjoint.
    spans: Vec<Range<usize>>,
    /// The node budget of the record.
    limit: usize,
    /// The nodes that the record can still make.
    budget: usize,
    /// The bytes that the decoder copies into opaque nodes.
    #[cfg(test)]
    copied: usize,
}

impl<'a> Decoder<'a> {
    fn new(bytes: &'a [u8], budget: usize) -> Self {
        Decoder {
            bytes,
            pos: 0,
            memo: HashMap::new(),
            spans: Vec::new(),
            limit: budget,
            budget,
            #[cfg(test)]
            copied: 0,
        }
    }

    fn record(mut self) -> Result<Tree, Error> {
        let mut class = self.pickle()?;
        let mut state = self.pickle()?;
        if self.pos != self.bytes.len() {
            return Err(self.error("bytes follow the state pickle"));
        }
        let mut spans = std::mem::take(&mut self.spans).into_iter();
        self.copy_spans(&mut class.root, &mut spans)?;
        self.copy_spans(&mut state.root, &mut spans)?;
        if spans.next().is_some() {
            return Err(self.error("an opaque span has no node"));
        }
        Ok(Tree { class, state })
    }

    /// Copies the bytes of the opaque nodes below `root` from their spans.
    /// The nodes come in pickle order, which is the order of the spans.
    fn copy_spans(
        &mut self,
        root: &mut Node,
        spans: &mut impl Iterator<Item = Range<usize>>,
    ) -> Result<(), Error> {
        let mut stack = vec![root];
        let mut children = Vec::new();
        while let Some(node) = stack.pop() {
            match node {
                Node::Opaque(opaque) => {
                    let span = spans
                        .next()
                        .ok_or_else(|| self.error("an opaque node has no span"))?;
                    opaque.bytes = self.bytes[span].to_vec();
                    #[cfg(test)]
                    {
                        self.copied += opaque.bytes.len();
                    }
                }
                other => {
                    other.children_mut(&mut children);
                    stack.extend(children.drain(..).rev());
                }
            }
        }
        Ok(())
    }

    fn pickle(&mut self) -> Result<Pickle, Error> {
        let mut protocol = None;
        if self.bytes.get(self.pos) == Some(&PROTO) {
            let at = self.pos;
            self.pos += 1;
            let value = self.u8()?;
            if value > HIGHEST_PROTOCOL {
                return Err(Error::Decode {
                    offset: at,
                    message: format!("protocol {value} is not supported"),
                });
            }
            protocol = Some(value);
        }
        let mut machine = Machine::default();
        loop {
            let start = self.pos;
            let opcode = self.u8()?;
            if opcode == STOP {
                let root = self.stop(machine, start)?;
                return Ok(Pickle { protocol, root });
            }
            self.step(&mut machine, opcode, start)?;
        }
    }

    /// Returns the value that the pickle returns at STOP, at offset `at`.
    fn stop(&mut self, mut machine: Machine, at: usize) -> Result<Node, Error> {
        let entry = machine.stack.pop();
        let entry = entry
            .filter(|entry| machine.stack.is_empty() && matches!(entry.kind, Kind::Value { .. }))
            .ok_or_else(|| self.error("the stack does not hold exactly one value at STOP"))?;
        let start = entry.start;
        let root = entry.into_node().unwrap_or(Node::None);
        Ok(match machine.junk {
            Some((_, opcode)) => self.opaque_span(start..at, inherit(&root, opcode)),
            None => root,
        })
    }

    /// Runs one opcode. `start` is the offset of the opcode.
    fn step(&mut self, m: &mut Machine, opcode: u8, start: usize) -> Result<(), Error> {
        let node = match opcode {
            MARK => return self.push_mark(m, start),
            POP => {
                let entry = m
                    .stack
                    .pop()
                    .ok_or_else(|| self.error("POP on an empty stack"))?;
                m.junk = Some((entry.start, m.junk.map_or(opcode, |(_, first)| first)));
                return Ok(());
            }
            POP_MARK => {
                let (mark, _, _) = self.pop_mark(m)?;
                m.junk = Some((mark, m.junk.map_or(opcode, |(_, first)| first)));
                return Ok(());
            }
            DUP => {
                if !matches!(
                    m.stack.last(),
                    Some(Entry {
                        kind: Kind::Value { .. },
                        ..
                    })
                ) {
                    return Err(self.error("DUP needs a value on the stack"));
                }
                self.opaque(start, opcode)
            }
            NONE => Node::None,
            NEWTRUE => Node::Bool(Bool::New(true)),
            NEWFALSE => Node::Bool(Bool::New(false)),
            INT => match self.line()? {
                line if text::int_bool(line).is_some() => Node::Bool(Bool::Text(ascii(line))),
                line if text::is_int(line, false) => Node::Int(Int::Text(ascii(line))),
                _ => return Err(self.error_at(start, "INT has no valid number")),
            },
            LONG => match self.line()? {
                line if text::is_int(line, true) => Node::Int(Int::LongText(ascii(line))),
                _ => return Err(self.error_at(start, "LONG has no valid number")),
            },
            BININT1 => Node::Int(Int::U8(self.u8()?)),
            BININT2 => Node::Int(Int::U16(u16::from_le_bytes(self.array()?))),
            BININT => Node::Int(Int::I32(i32::from_le_bytes(self.array()?))),
            LONG1 | LONG4 => {
                let (len, width) = self.length(opcode == LONG1, true)?;
                let bytes = self.take(len)?.to_vec();
                Node::Int(Int::Long { bytes, width })
            }
            FLOAT => match self.line()? {
                line if text::is_float(line) => Node::Float(Float::Text(ascii(line))),
                _ => return Err(self.error_at(start, "FLOAT has no valid number")),
            },
            BINFLOAT => Node::Float(Float::Binary(f64::from_be_bytes(self.array()?))),
            SHORT_BINBYTES | BINBYTES => {
                Node::Bytes(self.read_bytes(opcode == SHORT_BINBYTES, false)?)
            }
            SHORT_BINSTRING | BINSTRING => {
                let bytes = self.read_bytes(opcode == SHORT_BINSTRING, true)?;
                Node::ByteString(ByteString::Binary(bytes))
            }
            STRING => match self.line()? {
                line if text::is_quoted(line) => Node::ByteString(ByteString::Text(line.to_vec())),
                _ => return Err(self.error_at(start, "STRING is not quoted with valid escapes")),
            },
            BINUNICODE => {
                let (len, _) = self.length(false, false)?;
                let data = self.take(len)?;
                match std::str::from_utf8(data) {
                    Ok(text) => Node::Unicode(Unicode::Binary(text.to_owned())),
                    // Python writes a lone surrogate with `surrogatepass`.
                    // That is not UTF-8, so a Rust string cannot hold it.
                    Err(_) if text::is_utf8_with_surrogates(data) => self.opaque(start, opcode),
                    Err(_) => return Err(self.error_at(start, "BINUNICODE is not UTF-8")),
                }
            }
            UNICODE => match self.line()? {
                line if text::is_raw_unicode(line) => Node::Unicode(Unicode::Text(line.to_vec())),
                _ => return Err(self.error_at(start, "UNICODE has an escape that is not valid")),
            },
            EMPTY_TUPLE => tuple(Vec::new(), false),
            TUPLE1 | TUPLE2 | TUPLE3 => {
                let (first, items) = self.pop_values(m, usize::from(opcode - TUPLE1) + 1)?;
                return self.replace(m, first, tuple(items, false));
            }
            TUPLE => {
                let (mark, junk, items) = self.pop_mark(m)?;
                let node = self.unless_junk(mark, junk, || tuple(items, true));
                return self.replace(m, mark, node);
            }
            EMPTY_LIST => list(None),
            LIST => {
                let (mark, junk, items) = self.pop_mark(m)?;
                let node = self.unless_junk(mark, junk, || list(Some(items)));
                return self.replace(m, mark, node);
            }
            EMPTY_DICT => dict(None),
            DICT => {
                let (mark, junk, items) = self.pop_mark(m)?;
                let items = self.pairs(items, "DICT")?;
                let node = self.unless_junk(mark, junk, || dict(Some(items)));
                return self.replace(m, mark, node);
            }
            APPEND => {
                let (_, item) = self.pop_value(m)?;
                let batch = Batch {
                    items: vec![item],
                    marked: false,
                };
                return self.fill(m, opcode, Items::List(batch), None);
            }
            APPENDS => {
                let (_, junk, items) = self.pop_mark(m)?;
                let batch = Batch {
                    items,
                    marked: true,
                };
                return self.fill(m, opcode, Items::List(batch), junk);
            }
            SETITEM => {
                let (_, value) = self.pop_value(m)?;
                let (_, key) = self.pop_value(m)?;
                let batch = Batch {
                    items: vec![(key, value)],
                    marked: false,
                };
                return self.fill(m, opcode, Items::Dict(batch), None);
            }
            SETITEMS => {
                let (_, junk, items) = self.pop_mark(m)?;
                let batch = Batch {
                    items: self.pairs(items, "SETITEMS")?,
                    marked: true,
                };
                return self.fill(m, opcode, Items::Dict(batch), junk);
            }
            GLOBAL => {
                let module = self.name()?;
                let name = self.name()?;
                match (std::str::from_utf8(module), std::str::from_utf8(name)) {
                    (Ok(module), Ok(name)) => Node::Global(Global {
                        module: module.to_owned(),
                        name: name.to_owned(),
                    }),
                    _ => return Err(self.error_at(start, "GLOBAL names are not UTF-8")),
                }
            }
            INST => {
                let module = self.name()?;
                let name = self.name()?;
                if !module.is_ascii() || !name.is_ascii() {
                    return Err(self.error_at(start, "INST names are not ASCII"));
                }
                let (mark, _, _) = self.pop_mark(m)?;
                let node = self.opaque(mark, opcode);
                return self.replace(m, mark, node);
            }
            OBJ => {
                let (mark, _, items) = self.pop_mark(m)?;
                if items.is_empty() {
                    return Err(self.error("OBJ has no class"));
                }
                let node = self.opaque(mark, opcode);
                return self.replace(m, mark, node);
            }
            PERSID => {
                if !self.line()?.is_ascii() {
                    return Err(self.error_at(start, "PERSID is not ASCII"));
                }
                self.opaque(start, opcode)
            }
            BINPERSID => {
                let (first, pid) = self.pop_value(m)?;
                return self.replace(m, first, Node::Persistent(Box::new(pid)));
            }
            REDUCE => {
                let (_, args) = self.pop_value(m)?;
                let (first, callable) = self.pop_value(m)?;
                let node =
                    if self.is_constructor(&callable) && matches!(args.value(), Node::Tuple(_)) {
                        Node::Reduce(Reduce {
                            callable: Box::new(callable),
                            args: Box::new(args),
                        })
                    } else {
                        self.opaque(first, inherit(&callable, opcode))
                    };
                return self.replace(m, first, node);
            }
            NEWOBJ | BUILD => {
                self.pop_value(m)?;
                let (first, target) = self.pop_value(m)?;
                let node = self.opaque(first, inherit(&target, opcode));
                return self.replace(m, first, node);
            }
            EXT1 | EXT2 | EXT4 => {
                let code = match opcode {
                    EXT1 => i64::from(self.u8()?),
                    EXT2 => i64::from(u16::from_le_bytes(self.array()?)),
                    _ => i64::from(i32::from_le_bytes(self.array()?)),
                };
                if code <= 0 {
                    return Err(
                        self.error_at(start, format!("extension code {code} is not positive"))
                    );
                }
                self.opaque(start, opcode)
            }
            BINPUT | LONG_BINPUT | PUT => return self.put(m, opcode, start),
            BINGET | LONG_BINGET | GET => {
                let (index, form) = self.index(opcode)?;
                if !self.memo.contains_key(&index) {
                    return Err(self.error(format!("memo index {index} is not stored")));
                }
                Node::Get(Get { index, form })
            }
            _ => {
                return Err(Error::Decode {
                    offset: start,
                    message: format!("opcode 0x{opcode:02x} is not supported"),
                });
            }
        };
        self.push(m, start, node)
    }

    /// Pushes the value of an opcode that takes nothing off the stack. If
    /// junk comes before the opcode, the value takes it in and is opaque.
    fn push(&mut self, m: &mut Machine, start: usize, node: Node) -> Result<(), Error> {
        let (start, node) = match m.junk.take() {
            Some((junk, opcode)) => (junk, self.opaque(junk, inherit(&node, opcode))),
            None => (start, node),
        };
        self.spend()?;
        m.stack.push(Entry {
            start,
            kind: Kind::Value {
                node,
                puts: Vec::new(),
            },
        });
        Ok(())
    }

    fn push_mark(&mut self, m: &mut Machine, start: usize) -> Result<(), Error> {
        let (start, junk) = match m.junk.take() {
            Some((junk, opcode)) => (junk, Some(opcode)),
            None => (start, None),
        };
        self.spend()?;
        m.stack.push(Entry {
            start,
            kind: Kind::Mark { junk },
        });
        Ok(())
    }

    /// Pushes the value of an opcode that took entries off the stack.
    /// `start` is the start of the first of them. Junk comes after that
    /// start, so if there is junk, the value takes it in and is opaque.
    fn replace(&mut self, m: &mut Machine, start: usize, node: Node) -> Result<(), Error> {
        let node = match m.junk.take() {
            Some((_, opcode)) => self.opaque(start, inherit(&node, opcode)),
            None => node,
        };
        self.spend()?;
        m.stack.push(Entry {
            start,
            kind: Kind::Value {
                node,
                puts: Vec::new(),
            },
        });
        Ok(())
    }

    /// Returns the node that `make` builds, or an opaque node from `mark` if
    /// junk comes before the mark.
    fn unless_junk(&mut self, mark: usize, junk: Option<u8>, make: impl FnOnce() -> Node) -> Node {
        match junk {
            Some(opcode) => self.opaque(mark, opcode),
            None => make(),
        }
    }

    /// Adds a batch to the list or the dict on top of the stack. The top and
    /// the batch become one opaque node in two cases: the top is another
    /// value, for example an object that REDUCE makes, or junk is in the
    /// span.
    fn fill(
        &mut self,
        m: &mut Machine,
        opcode: u8,
        items: Items,
        mark_junk: Option<u8>,
    ) -> Result<(), Error> {
        let junk = m.junk.take().map(|(_, opcode)| opcode).or(mark_junk);
        let Some(Entry {
            start,
            kind: Kind::Value { node, puts },
        }) = m.stack.last_mut()
        else {
            return Err(self.error("no list or dict to fill"));
        };
        let modeled = junk.is_none()
            && match (&mut *node, items) {
                (Node::List(list), Items::List(batch)) => {
                    list.batches.push(batch);
                    true
                }
                (Node::Dict(dict), Items::Dict(batch)) => {
                    dict.batches.push(batch);
                    true
                }
                _ => false,
            };
        if !modeled {
            let opcode = inherit(node, junk.unwrap_or(opcode));
            *node = self.opaque(*start, opcode);
            puts.clear();
        }
        Ok(())
    }

    /// Runs BINPUT, LONG_BINPUT or PUT on the value on top of the stack.
    fn put(&mut self, m: &mut Machine, opcode: u8, start: usize) -> Result<(), Error> {
        let (index, form) = self.index(opcode)?;
        let junk = m.junk.take().map(|(_, opcode)| opcode);
        let Some(Entry {
            start: first,
            kind: Kind::Value { node, puts },
        }) = m.stack.last_mut()
        else {
            return Err(self.error("no value to put in the memo"));
        };
        let constructor = self.is_constructor(node);
        // The tree writes a put before the items of a list or a dict. A put
        // after the items cannot keep its place.
        if junk.is_some() || is_filled(node) {
            let opcode = inherit(node, junk.unwrap_or(opcode));
            *node = self.opaque(*first, opcode);
            puts.clear();
        } else {
            puts.push((index, form));
        }
        self.memo.insert(
            index,
            Slot {
                at: start,
                constructor,
            },
        );
        self.spend()
    }

    /// Returns a placeholder opaque node for the bytes from `start` to the
    /// current offset.
    fn opaque(&mut self, start: usize, opcode: u8) -> Node {
        self.opaque_span(start..self.pos, opcode)
    }

    /// Returns a placeholder opaque node for a span, and records the span.
    /// The span replaces the older spans inside it. Each span is the span of
    /// a stack entry, or of the entries that an opcode takes off. So a span
    /// never starts inside an older span and ends after it.
    fn opaque_span(&mut self, span: Range<usize>, opcode: u8) -> Node {
        while self
            .spans
            .last()
            .is_some_and(|last| last.start >= span.start)
        {
            self.spans.pop();
        }
        debug_assert!(self.spans.last().is_none_or(|last| last.end <= span.start));
        self.spans.push(span);
        Node::Opaque(Opaque {
            bytes: Vec::new(),
            opcode,
        })
    }

    /// Tells if an offset is inside an opaque node.
    fn hidden(&self, at: usize) -> bool {
        let after = self.spans.partition_point(|span| span.start <= at);
        after > 0 && self.spans[after - 1].end > at
    }

    /// Tells if a node is a known constructor, or a get of one whose put the
    /// tree can see.
    fn is_constructor(&self, node: &Node) -> bool {
        match node.value() {
            Node::Global(global) => {
                KNOWN_CONSTRUCTORS.contains(&(global.module.as_str(), global.name.as_str()))
            }
            Node::Get(get) => self
                .memo
                .get(&get.index)
                .is_some_and(|slot| slot.constructor && !self.hidden(slot.at)),
            _ => false,
        }
    }

    fn spend(&mut self) -> Result<(), Error> {
        self.budget = self.budget.checked_sub(1).ok_or(Error::Budget {
            offset: self.pos,
            budget: self.limit,
        })?;
        Ok(())
    }

    fn pop_value(&self, m: &mut Machine) -> Result<(usize, Node), Error> {
        match m.stack.pop() {
            Some(
                entry @ Entry {
                    kind: Kind::Value { .. },
                    ..
                },
            ) => Ok((entry.start, entry.into_node().unwrap_or(Node::None))),
            _ => Err(self.error("no value on the stack")),
        }
    }

    /// Pops `n` values. Returns the start of the first one and the values in
    /// stack order.
    fn pop_values(&self, m: &mut Machine, n: usize) -> Result<(usize, Vec<Node>), Error> {
        let at = m
            .stack
            .len()
            .checked_sub(n)
            .ok_or_else(|| self.error("the stack is too short"))?;
        let start = m.stack.get(at).map_or(self.pos, |entry| entry.start);
        m.stack
            .split_off(at)
            .into_iter()
            .map(Entry::into_node)
            .collect::<Option<Vec<_>>>()
            .map(|items| (start, items))
            .ok_or_else(|| self.error("a mark is in the way"))
    }

    /// Pops the values down to the last mark, then the mark. Returns the
    /// start of the mark, its junk and the values in stack order.
    fn pop_mark(&self, m: &mut Machine) -> Result<(usize, Option<u8>, Vec<Node>), Error> {
        let at = m
            .stack
            .iter()
            .rposition(|entry| matches!(entry.kind, Kind::Mark { .. }))
            .ok_or_else(|| self.error("no mark on the stack"))?;
        let items = m
            .stack
            .drain(at + 1..)
            .filter_map(Entry::into_node)
            .collect();
        match m.stack.pop() {
            Some(Entry {
                start,
                kind: Kind::Mark { junk },
            }) => Ok((start, junk, items)),
            _ => Err(self.error("no mark on the stack")),
        }
    }

    /// Pairs the items of DICT or SETITEMS.
    fn pairs(&self, items: Vec<Node>, opcode: &str) -> Result<Vec<(Node, Node)>, Error> {
        if !items.len().is_multiple_of(2) {
            return Err(self.error(format!("{opcode} has an odd number of items")));
        }
        let mut items = items.into_iter();
        let mut pairs = Vec::with_capacity(items.len() / 2);
        while let (Some(key), Some(value)) = (items.next(), items.next()) {
            pairs.push((key, value));
        }
        Ok(pairs)
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], Error> {
        let end = self
            .pos
            .checked_add(len)
            .filter(|end| *end <= self.bytes.len())
            .ok_or_else(|| self.error("the record ends too early"))?;
        let bytes = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(bytes)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], Error> {
        let mut array = [0; N];
        array.copy_from_slice(self.take(N)?);
        Ok(array)
    }

    fn u8(&mut self) -> Result<u8, Error> {
        Ok(self.take(1)?[0])
    }

    /// Reads a length: one byte if `short`, else four bytes, little-endian.
    /// If `signed`, Python reads the four bytes as a signed number and
    /// refuses a negative one, that is a length of 2**31 or more.
    fn length(&mut self, short: bool, signed: bool) -> Result<(usize, Width), Error> {
        if short {
            return Ok((usize::from(self.u8()?), Width::Short));
        }
        let at = self.pos;
        let len = u32::from_le_bytes(self.array()?);
        if signed && i32::try_from(len).is_err() {
            return Err(self.error_at(at, format!("length {len} is negative as a signed number")));
        }
        let len = usize::try_from(len).map_err(|_| self.error_at(at, "length too large"))?;
        Ok((len, Width::Long))
    }

    fn read_bytes(&mut self, short: bool, signed: bool) -> Result<Bytes, Error> {
        let (len, width) = self.length(short, signed)?;
        let data = self.take(len)?.to_vec();
        Ok(Bytes { data, width })
    }

    /// Reads the index of a put or a get.
    fn index(&mut self, opcode: u8) -> Result<(u32, MemoForm), Error> {
        match opcode {
            BINPUT | BINGET => Ok((u32::from(self.u8()?), MemoForm::Short)),
            LONG_BINPUT | LONG_BINGET => Ok((u32::from_le_bytes(self.array()?), MemoForm::Long)),
            _ => {
                let at = self.pos;
                let line = self.line()?;
                let index = text::index(line)
                    .ok_or_else(|| self.error_at(at, "the memo index is not a decimal number"))?;
                Ok((index, MemoForm::Text))
            }
        }
    }

    /// Reads up to the next newline. Returns the bytes before it.
    fn line(&mut self) -> Result<&'a [u8], Error> {
        let rest = &self.bytes[self.pos..];
        let len = rest
            .iter()
            .position(|byte| *byte == b'\n')
            .ok_or_else(|| self.error("a text argument has no newline"))?;
        self.pos += len + 1;
        Ok(&rest[..len])
    }

    /// Reads a module or a class name: a line that is not empty.
    fn name(&mut self) -> Result<&'a [u8], Error> {
        let at = self.pos;
        match self.line()? {
            [] => Err(self.error_at(at, "a name is empty")),
            name => Ok(name),
        }
    }

    fn error(&self, message: impl Into<String>) -> Error {
        self.error_at(self.pos, message)
    }

    fn error_at(&self, offset: usize, message: impl Into<String>) -> Error {
        Error::Decode {
            offset,
            message: message.into(),
        }
    }
}

/// Returns the opcode of a node if it is opaque, else `opcode`.
fn inherit(node: &Node, opcode: u8) -> u8 {
    match node.value() {
        Node::Opaque(opaque) => opaque.opcode,
        _ => opcode,
    }
}

/// Returns ASCII bytes as a string. The callers check the bytes first.
fn ascii(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn tuple(items: Vec<Node>, marked: bool) -> Node {
    Node::Tuple(Tuple { items, marked })
}

fn list(initial: Option<Vec<Node>>) -> Node {
    Node::List(List {
        initial,
        batches: Vec::new(),
    })
}

fn dict(initial: Option<Vec<(Node, Node)>>) -> Node {
    Node::Dict(Dict {
        initial,
        batches: Vec::new(),
    })
}

/// Tells if a node is a list or a dict that has batches already.
fn is_filled(node: &Node) -> bool {
    match node.value() {
        Node::List(list) => !list.batches.is_empty(),
        Node::Dict(dict) => !dict.batches.is_empty(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode_record;

    /// The class pickle of the test records: PROTO 3, GLOBAL m C, BINPUT 0.
    const CLASS: &[u8] = b"\x80\x03cm\nC\nq\x00.";

    fn record(body: &[u8]) -> Vec<u8> {
        [CLASS, b"\x80\x03", body, b"."].concat()
    }

    /// Decodes a record whose state pickle is PROTO 3, `body`, STOP. Checks
    /// that the record encodes to the same bytes. Returns the state.
    fn state(body: &[u8]) -> Node {
        let record = record(body);
        let tree = decode_record(&record).unwrap();
        assert_eq!(encode_record(&tree).unwrap(), record);
        tree.state().clone()
    }

    fn decode_error(body: &[u8]) -> String {
        decode_record(&record(body)).unwrap_err().to_string()
    }

    fn opaque(bytes: &[u8], opcode: u8) -> Node {
        Node::Opaque(Opaque {
            bytes: bytes.to_vec(),
            opcode,
        })
    }

    fn put(index: u32, form: MemoForm, value: Node) -> Node {
        Node::Put(Put {
            index,
            form,
            value: Box::new(value),
        })
    }

    fn int(value: u8) -> Node {
        Node::Int(Int::U8(value))
    }

    fn batch<T>(items: Vec<T>, marked: bool) -> Batch<T> {
        Batch { items, marked }
    }

    #[test]
    fn the_class_name_comes_from_the_class_pickle_alone() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus");
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_none_or(|ext| ext != "record") {
                continue;
            }
            let bytes = std::fs::read(&path).unwrap();
            let tree = decode_record(&bytes).unwrap();
            let expected = tree
                .class_name()
                .map(|(module, name)| (module.to_owned(), name.to_owned()));
            assert_eq!(decode_class_name(&bytes).unwrap(), expected, "{path:?}");
        }
        // A state pickle that does not decode does not matter.
        let class = (String::from("m"), String::from("C"));
        assert_eq!(decode_class_name(&record(b"\xff")).unwrap(), Some(class));
        assert!(decode_class_name(b"\x80\x03cm\n").is_err());
        assert_eq!(decode_class_name(b"\x80\x03N.\x80\x03}.").unwrap(), None);
    }

    #[test]
    fn atoms_keep_their_opcode() {
        let short = Width::Short;
        let long = Width::Long;
        let bytes = |data: &[u8], width| Bytes {
            data: data.to_vec(),
            width,
        };
        for (body, node) in [
            (&b"N"[..], Node::None),
            (b"\x88", Node::Bool(Bool::New(true))),
            (b"\x89", Node::Bool(Bool::New(false))),
            (b"I01\n", Node::Bool(Bool::Text("01".to_owned()))),
            (b"I00\n", Node::Bool(Bool::Text("00".to_owned()))),
            (b"I+1\n", Node::Bool(Bool::Text("+1".to_owned()))),
            (b"I-0\n", Node::Bool(Bool::Text("-0".to_owned()))),
            (b"I-1\n", Node::Int(Int::Text("-1".to_owned()))),
            (b"I+10\n", Node::Int(Int::Text("+10".to_owned()))),
            (b"K\x05", int(5)),
            (b"M\x00\x01", Node::Int(Int::U16(256))),
            (b"J\xff\xff\xff\xff", Node::Int(Int::I32(-1))),
            (
                b"I4294967296\n",
                Node::Int(Int::Text("4294967296".to_owned())),
            ),
            (
                b"\x8a\x02\xff\x00",
                Node::Int(Int::Long {
                    bytes: vec![0xff, 0],
                    width: short,
                }),
            ),
            (
                b"\x8b\x01\x00\x00\x00\x80",
                Node::Int(Int::Long {
                    bytes: vec![0x80],
                    width: long,
                }),
            ),
            (b"L-12L\n", Node::Int(Int::LongText("-12L".to_owned()))),
            (
                b"G?\xf8\x00\x00\x00\x00\x00\x00",
                Node::Float(Float::Binary(1.5)),
            ),
            (b"F-2.25\n", Node::Float(Float::Text("-2.25".to_owned()))),
            (b"C\x02ab", Node::Bytes(bytes(b"ab", short))),
            (b"B\x02\x00\x00\x00ab", Node::Bytes(bytes(b"ab", long))),
            (
                b"U\x02ab",
                Node::ByteString(ByteString::Binary(bytes(b"ab", short))),
            ),
            (
                b"T\x02\x00\x00\x00ab",
                Node::ByteString(ByteString::Binary(bytes(b"ab", long))),
            ),
            (
                b"S'a\\x00b'\n",
                Node::ByteString(ByteString::Text(b"'a\\x00b'".to_vec())),
            ),
            (
                b"X\x02\x00\x00\x00\xc3\xa9",
                Node::Unicode(Unicode::Binary("\u{e9}".to_owned())),
            ),
            (
                b"Vcl\\u00e9\n",
                Node::Unicode(Unicode::Text(b"cl\\u00e9".to_vec())),
            ),
            (
                b"cmod\nname\n",
                Node::Global(Global {
                    module: "mod".to_owned(),
                    name: "name".to_owned(),
                }),
            ),
        ] {
            assert_eq!(state(body), node, "{body:?}");
        }
    }

    #[test]
    fn containers_keep_their_batches_and_forms() {
        let list = state(b"]q\x01(K\x01K\x02eK\x03a");
        let expected = Node::List(List {
            initial: None,
            batches: vec![
                batch(vec![int(1), int(2)], true),
                batch(vec![int(3)], false),
            ],
        });
        assert_eq!(list, put(1, MemoForm::Short, expected));

        let dict = state(b"}(K\x01K\x02uK\x03K\x04s");
        let expected = Node::Dict(Dict {
            initial: None,
            batches: vec![
                batch(vec![(int(1), int(2))], true),
                batch(vec![(int(3), int(4))], false),
            ],
        });
        assert_eq!(dict, expected);

        assert_eq!(state(b")"), tuple(vec![], false));
        assert_eq!(state(b"(t"), tuple(vec![], true));
        assert_eq!(state(b"K\x01K\x02\x86"), tuple(vec![int(1), int(2)], false));
        assert_eq!(state(b"(K\x01t"), tuple(vec![int(1)], true));
    }

    #[test]
    fn protocol_0_containers_keep_their_initial_items() {
        let list = state(b"(K\x01lp0\nK\x02a");
        let expected = Node::List(List {
            initial: Some(vec![int(1)]),
            batches: vec![batch(vec![int(2)], false)],
        });
        assert_eq!(list, put(0, MemoForm::Text, expected));

        let dict = state(b"(K\x01K\x02dK\x03K\x04s");
        let expected = Node::Dict(Dict {
            initial: Some(vec![(int(1), int(2))]),
            batches: vec![batch(vec![(int(3), int(4))], false)],
        });
        assert_eq!(dict, expected);
        assert!(decode_error(b"(K\x01d").contains("DICT has an odd number"));
    }

    #[test]
    fn protocol_0_memo_opcodes_keep_their_text_form() {
        let node = state(b"(K\x07p1\ng1\ng0\nt");
        let get = |index| {
            Node::Get(Get {
                index,
                form: MemoForm::Text,
            })
        };
        let items = vec![put(1, MemoForm::Text, int(7)), get(1), get(0)];
        assert_eq!(node, tuple(items, true));
    }

    #[test]
    fn python_2_objects_are_opaque() {
        let obj = b"(cmod\nOld\nq\x01o";
        assert_eq!(state(obj), opaque(obj, OBJ));
        let inst = b"(K\x01imod\nOld\n";
        assert_eq!(state(inst), opaque(inst, INST));
        let built = b"(imod\nOld\nq\x01}q\x02b";
        assert_eq!(state(built), opaque(built, INST));
        assert_eq!(state(b"Pabc\n"), opaque(b"Pabc\n", PERSID));
    }

    #[test]
    fn both_pickles_share_one_memo() {
        let get = |index, form| Node::Get(Get { index, form });
        assert_eq!(state(b"h\x00"), get(0, MemoForm::Short));
        assert_eq!(state(b"j\x00\x00\x00\x00"), get(0, MemoForm::Long));
        assert_eq!(state(b"g0\n"), get(0, MemoForm::Text));
        assert!(decode_error(b"h\x05").contains("memo index 5 is not stored"));
    }

    #[test]
    fn a_persistent_reference_holds_its_persistent_id() {
        let node = state(b"C\x01\x07h\x00\x86Q");
        let Node::Persistent(pid) = &node else {
            panic!("{node:?}");
        };
        assert!(matches!(**pid, Node::Tuple(_)));
    }

    #[test]
    fn reduce_of_an_unknown_callable_is_opaque() {
        let body = b"cm\nf\nq\x01K\x01\x85R";
        assert_eq!(state(body), opaque(body, REDUCE));
        // A put after the opaque value stays outside it.
        let node = state(b"cm\nf\nq\x01K\x01\x85Rq\x02");
        assert_eq!(node, put(2, MemoForm::Short, opaque(body, REDUCE)));
    }

    #[test]
    fn reduce_of_set_is_modeled_also_through_a_get() {
        let node = state(b"(cbuiltins\nset\nq\x01)\x85Rh\x01)\x85Rt");
        let Node::Tuple(tuple) = &node else {
            panic!("{node:?}");
        };
        assert_eq!(tuple.items.len(), 2);
        assert!(
            tuple
                .items
                .iter()
                .all(|item| matches!(item, Node::Reduce(_)))
        );
    }

    #[test]
    fn reduce_through_a_get_of_a_hidden_put_is_opaque() {
        // The put of builtins.set is inside the bytes of an unknown call,
        // so the tree cannot show that the get names set.
        let node = state(b"(cm\nf\n(cbuiltins\nset\nq\x01t\x85Rh\x01)\x85Rt");
        let Node::Tuple(tuple) = &node else {
            panic!("{node:?}");
        };
        assert!(matches!(
            &tuple.items[..],
            [Node::Opaque(_), Node::Opaque(_)]
        ));
    }

    #[test]
    fn build_on_an_object_keeps_the_first_unmodeled_opcode() {
        let body = b"cm\nC\n)\x81q\x01}b";
        assert_eq!(state(body), opaque(body, NEWOBJ));
    }

    #[test]
    fn items_added_to_an_object_make_it_opaque() {
        let body = b"cm\nOrdered\n)R(K\x01K\x02u";
        assert_eq!(state(body), opaque(body, REDUCE));
        let body = b"cm\nOrdered\n)Rq\x01K\x01a";
        assert_eq!(state(body), opaque(body, REDUCE));
    }

    #[test]
    fn extension_codes_are_opaque() {
        assert_eq!(state(b"\x82\x05"), opaque(b"\x82\x05", EXT1));
        assert_eq!(state(b"\x83\x05\x01"), opaque(b"\x83\x05\x01", EXT2));
        let ext4 = b"\x84\x05\x00\x00\x00";
        assert_eq!(state(ext4), opaque(ext4, EXT4));
    }

    #[test]
    fn a_lone_surrogate_is_opaque() {
        let body = b"X\x03\x00\x00\x00\xed\xa0\x80";
        assert_eq!(state(body), opaque(body, BINUNICODE));
    }

    #[test]
    fn a_put_after_the_items_makes_the_list_opaque() {
        let body = b"](K\x01K\x02eq\x01";
        assert_eq!(state(body), opaque(body, BINPUT));
    }

    #[test]
    fn many_puts_on_one_list_keep_their_order() {
        let puts: Vec<u8> = (0..300_u32)
            .flat_map(|index| [&b"r"[..], &index.to_le_bytes()].concat())
            .collect();
        let items = b"N".repeat(300);
        let body = [&b"]"[..], &puts, b"(", &items, b"e"].concat();
        let node = state(&body);
        let mut value = &node;
        for index in (0..300).rev() {
            let Node::Put(put) = value else {
                panic!("{value:?}");
            };
            assert_eq!(put.index, index);
            value = &put.value;
        }
        assert!(matches!(value, Node::List(list) if list.batches[0].items.len() == 300));
    }

    #[test]
    fn dup_is_opaque() {
        let node = state(b"K\x012\x86");
        assert_eq!(node, tuple(vec![int(1), opaque(b"2", DUP)], false));
    }

    #[test]
    fn pop_leaves_its_bytes_to_the_next_value() {
        // The pickle of t = ([t],): the list holds a tuple that holds the
        // list. Python takes the list off with POP and gets the tuple.
        let body = b"]q\x01h\x01\x85q\x02a0h\x02";
        assert_eq!(state(body), opaque(body, POP));

        // The same in a dict: the key keeps its node.
        let node = state(b"}q\x01(X\x01\x00\x00\x00kq\x02]q\x03h\x03\x85q\x04a0h\x04u");
        let key = put(
            2,
            MemoForm::Short,
            Node::Unicode(Unicode::Binary("k".to_owned())),
        );
        let value = opaque(b"]q\x03h\x03\x85q\x04a0h\x04", POP);
        let dict = Node::Dict(Dict {
            initial: None,
            batches: vec![batch(vec![(key, value)], true)],
        });
        assert_eq!(node, put(1, MemoForm::Short, dict));
    }

    #[test]
    fn pop_mark_leaves_its_bytes_to_the_next_value() {
        let body = b"(K\x01K\x021h\x00";
        assert_eq!(state(body), opaque(body, POP_MARK));
    }

    #[test]
    fn junk_goes_into_the_value_that_covers_it() {
        // Junk before STOP: the root takes it in.
        assert_eq!(state(b"NK\x010"), opaque(b"NK\x010", POP));
        // POP takes a mark off.
        assert_eq!(state(b"N(0"), opaque(b"N(0", POP));
        // Junk before a mark: the tuple of that mark is opaque.
        let node = state(b"(K\x010(K\x02tt");
        assert_eq!(node, tuple(vec![opaque(b"K\x010(K\x02t", POP)], true));
        // Junk before APPEND: the list is opaque.
        assert_eq!(state(b"]K\x01N0a"), opaque(b"]K\x01N0a", POP));
        // Junk before a put: the put is inside the opaque node.
        let node = state(b"(NK\x010q\x01t");
        assert_eq!(node, tuple(vec![opaque(b"NK\x010q\x01", POP)], true));
    }

    #[test]
    fn a_pickle_without_proto_decodes() {
        let record = b"cm\nC\n.}.";
        let tree = decode_record(record).unwrap();
        assert_eq!(tree.state(), &dict(None));
        assert_eq!(encode_record(&tree).unwrap(), record);
    }

    #[test]
    fn bad_records_are_errors() {
        for (body, message) in [
            (&b"K"[..], "ends too early"),
            (b"\x8c\x01a", "opcode 0x8c is not supported"),
            (b"\x95\x00\x00\x00\x00\x00\x00\x00\x00N", "opcode 0x95"),
            (b"NN", "exactly one value"),
            (b"K\x01t", "no mark"),
            (b"}(K\x01u", "odd number"),
            (b"(", "exactly one value"),
            (b"(\x85", "a mark is in the way"),
            (b"K\x01a", "no list or dict"),
            (b"(K\x01e", "no list or dict"),
            (b"q\x01", "no value to put"),
            (b"0N", "POP on an empty stack"),
            (b"K\x011", "no mark"),
            (b"2", "DUP needs a value"),
            (b"(2", "DUP needs a value"),
            (b"(o", "OBJ has no class"),
            (b"imod\nOld\n", "no mark"),
            (b"T\x00\x00\x00\x80abc", "negative"),
            (b"\x8b\x00\x00\x00\x80\x01", "negative"),
            (b"X\x01\x00\x00\x00\xff", "not UTF-8"),
            (b"\x82\x00", "extension code 0"),
            (b"\x84\xff\xff\xff\xff", "extension code -1"),
            (b"I1", "no newline"),
            (b"I1x\n", "INT"),
            (b"I007\n", "INT"),
            (b"L1.5\n", "LONG"),
            (b"Fone\n", "FLOAT"),
            (b"Sabc\n", "STRING"),
            (b"S'a\\x0'\n", "STRING"),
            (b"V\\u12\n", "UNICODE"),
            (b"Np-1\n", "memo index"),
            (b"Np01\n", "memo index"),
            (b"P\xff\n", "PERSID"),
            (b"cm\n\n", "a name is empty"),
            (b"c\xff\nC\n", "not UTF-8"),
            (b"(imod\n\xc3\xa9\n", "not ASCII"),
        ] {
            let error = decode_error(body);
            assert!(error.contains(message), "{body:?}: {error}");
        }
        let trailing = [CLASS, b"N.", b"N"].concat();
        let error = decode_record(&trailing).unwrap_err().to_string();
        assert!(error.contains("bytes follow"), "{error}");
        assert!(decode_record(CLASS).is_err());
        let error = decode_record(&[CLASS, b"\x80\x06N."].concat()).unwrap_err();
        assert!(error.to_string().contains("protocol 6"), "{error}");
    }

    #[test]
    fn the_budget_limits_the_nodes_of_a_record() {
        // The class pickle makes 2 nodes (GLOBAL, BINPUT) and the state
        // makes 13: MARK, ten NONE, TUPLE.
        let bytes = record(&[&b"("[..], &[b'N'; 10], b"t"].concat());
        let limits = |node_budget| Limits { node_budget };
        assert!(decode_record_with(&bytes, limits(14)).is_ok());
        let error = decode_record_with(&bytes, limits(13)).unwrap_err();
        assert_eq!(
            error,
            Error::Budget {
                offset: bytes.len() - 1,
                budget: 13
            }
        );
        assert!(error.to_string().contains("more than 13 nodes"), "{error}");
    }

    #[test]
    fn a_budget_equal_to_the_length_never_refuses_a_record() {
        // One opcode of one byte per node: the most nodes for the length.
        let body = [&b"("[..], &[b'N'; NODE_BUDGET], b"t"].concat();
        let bytes = record(&body);
        let error = decode_record(&bytes).unwrap_err();
        assert!(
            matches!(
                error,
                Error::Budget {
                    budget: NODE_BUDGET,
                    ..
                }
            ),
            "{error}"
        );
        let limits = Limits {
            node_budget: bytes.len(),
        };
        let tree = decode_record_with(&bytes, limits).unwrap();
        assert_eq!(encode_record(&tree).unwrap(), bytes);
    }

    #[test]
    fn nested_opaque_spans_copy_each_byte_once() {
        // Each REDUCE calls the opaque value below it, so each opaque span
        // grows over the one before.
        let body = [&b"cm\nf\n"[..], &b")R".repeat(20_000)].concat();
        let bytes = record(&body);
        let mut decoder = Decoder::new(&bytes, NODE_BUDGET);
        let mut class = decoder.pickle().unwrap();
        let mut state = decoder.pickle().unwrap();
        let mut spans = std::mem::take(&mut decoder.spans).into_iter();
        decoder.copy_spans(&mut class.root, &mut spans).unwrap();
        decoder.copy_spans(&mut state.root, &mut spans).unwrap();
        assert_eq!(state.root, opaque(&body, REDUCE));
        assert_eq!(decoder.copied, body.len());
    }
}
