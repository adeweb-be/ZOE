//! The decoder: a record to a tree.
//!
//! The decoder runs the stack machine of pickle over the opcodes, but it
//! builds nodes instead of objects. It never imports or calls anything.
//! Each stack entry keeps the offset where its bytes start. So the bytes of
//! a construct that the tree does not model are one known span, and the
//! decoder can keep them in an opaque node.

use std::collections::HashMap;

use crate::error::Error;
use crate::opcode::*;
use crate::tree::{
    Batch, Bytes, Get, Global, Int, Node, Opaque, Pickle, Put, Reduce, Tree, Tuple, Width,
};

/// The callables that a modeled REDUCE can call.
const KNOWN_CONSTRUCTORS: [(&str, &str); 2] = [("builtins", "set"), ("builtins", "frozenset")];

/// Decodes a record: the class pickle, then the state pickle.
///
/// Both pickles share one memo, as ZODB writes them with one pickler. A
/// construct that the tree does not model becomes a [`Node::Opaque`].
pub fn decode_record(bytes: &[u8]) -> Result<Tree, Error> {
    let mut decoder = Decoder {
        bytes,
        pos: 0,
        memo: HashMap::new(),
    };
    let class = decoder.pickle()?;
    let state = decoder.pickle()?;
    if decoder.pos != bytes.len() {
        return Err(decoder.error("bytes follow the state pickle"));
    }
    Ok(Tree { class, state })
}

/// One stack entry: a value, or a mark if `node` is `None`.
struct Entry {
    /// The offset where the bytes of the entry start.
    start: usize,
    node: Option<Node>,
}

/// The items that APPEND, APPENDS, SETITEM or SETITEMS add.
enum Items {
    List(Batch<Node>),
    Dict(Batch<(Node, Node)>),
}

struct Decoder<'a> {
    bytes: &'a [u8],
    pos: usize,
    /// The memo indices that a put stored. The value is the global that
    /// the put stored, if it is one. REDUCE uses it to name a callable
    /// that comes from a get.
    memo: HashMap<u32, Option<Global>>,
}

impl<'a> Decoder<'a> {
    fn pickle(&mut self) -> Result<Pickle, Error> {
        let mut protocol = None;
        if self.bytes.get(self.pos) == Some(&PROTO) {
            self.pos += 1;
            protocol = Some(self.u8()?);
        }
        let mut stack = Vec::new();
        loop {
            let start = self.pos;
            let opcode = self.u8()?;
            if opcode == STOP {
                break;
            }
            self.step(opcode, start, &mut stack)?;
        }
        match stack.pop() {
            Some(Entry {
                node: Some(root), ..
            }) if stack.is_empty() => Ok(Pickle { protocol, root }),
            _ => Err(self.error("the stack does not hold exactly one value at STOP")),
        }
    }

    /// Runs one opcode. `start` is the offset of the opcode.
    fn step(&mut self, opcode: u8, start: usize, stack: &mut Vec<Entry>) -> Result<(), Error> {
        let (start, node) = match opcode {
            MARK => (start, None),
            NONE => (start, Some(Node::None)),
            NEWTRUE => (start, Some(Node::Bool(true))),
            NEWFALSE => (start, Some(Node::Bool(false))),
            BININT1 => (start, Some(Node::Int(Int::U8(self.u8()?)))),
            BININT2 => {
                let value = u16::from_le_bytes(self.array()?);
                (start, Some(Node::Int(Int::U16(value))))
            }
            BININT => {
                let value = i32::from_le_bytes(self.array()?);
                (start, Some(Node::Int(Int::I32(value))))
            }
            LONG1 | LONG4 => {
                let (len, width) = self.length(opcode == LONG1)?;
                let bytes = self.take(len)?.to_vec();
                (start, Some(Node::Int(Int::Long { bytes, width })))
            }
            BINFLOAT => (start, Some(Node::Float(f64::from_be_bytes(self.array()?)))),
            SHORT_BINBYTES | BINBYTES => {
                let bytes = self.bytes(opcode == SHORT_BINBYTES)?;
                (start, Some(Node::Bytes(bytes)))
            }
            SHORT_BINSTRING | BINSTRING => {
                let bytes = self.bytes(opcode == SHORT_BINSTRING)?;
                (start, Some(Node::ByteString(bytes)))
            }
            BINUNICODE => {
                let (len, _) = self.length(false)?;
                let node = match std::str::from_utf8(self.take(len)?) {
                    Ok(text) => Node::Unicode(text.to_owned()),
                    // Python writes a lone surrogate with `surrogatepass`.
                    // That is not UTF-8, so a Rust string cannot hold it.
                    Err(_) => self.opaque(start, opcode, None),
                };
                (start, Some(node))
            }
            EMPTY_TUPLE => (start, Some(tuple(Vec::new(), false))),
            TUPLE1 | TUPLE2 | TUPLE3 => {
                let (first, items) = self.pop_values(stack, usize::from(opcode - TUPLE1) + 1)?;
                (first, Some(tuple(items, false)))
            }
            TUPLE => {
                let (first, items) = self.pop_mark(stack)?;
                (first, Some(tuple(items, true)))
            }
            EMPTY_LIST => (start, Some(Node::List(Vec::new()))),
            EMPTY_DICT => (start, Some(Node::Dict(Vec::new()))),
            APPEND => {
                let (_, item) = self.pop_value(stack)?;
                let batch = Batch {
                    items: vec![item],
                    marked: false,
                };
                return self.fill(stack, opcode, Items::List(batch));
            }
            APPENDS => {
                let (_, items) = self.pop_mark(stack)?;
                let batch = Batch {
                    items,
                    marked: true,
                };
                return self.fill(stack, opcode, Items::List(batch));
            }
            SETITEM => {
                let (_, value) = self.pop_value(stack)?;
                let (_, key) = self.pop_value(stack)?;
                let batch = Batch {
                    items: vec![(key, value)],
                    marked: false,
                };
                return self.fill(stack, opcode, Items::Dict(batch));
            }
            SETITEMS => {
                let (_, items) = self.pop_mark(stack)?;
                if items.len() % 2 != 0 {
                    return Err(self.error("SETITEMS has an odd number of items"));
                }
                let batch = Batch {
                    items: pairs(items),
                    marked: true,
                };
                return self.fill(stack, opcode, Items::Dict(batch));
            }
            GLOBAL => {
                let module = self.line()?;
                let name = self.line()?;
                let node = match (std::str::from_utf8(module), std::str::from_utf8(name)) {
                    (Ok(module), Ok(name)) => Node::Global(Global {
                        module: module.to_owned(),
                        name: name.to_owned(),
                    }),
                    _ => self.opaque(start, opcode, None),
                };
                (start, Some(node))
            }
            BINPERSID => {
                let (first, pid) = self.pop_value(stack)?;
                (first, Some(Node::Persistent(Box::new(pid))))
            }
            REDUCE => {
                let (_, args) = self.pop_value(stack)?;
                let (first, callable) = self.pop_value(stack)?;
                let node = if self.is_known_constructor(&callable)
                    && matches!(args.value(), Node::Tuple(_))
                {
                    Node::Reduce(Reduce {
                        callable: Box::new(callable),
                        args: Box::new(args),
                    })
                } else {
                    self.opaque(first, opcode, Some(&callable))
                };
                (first, Some(node))
            }
            NEWOBJ | BUILD => {
                self.pop_value(stack)?;
                let (first, target) = self.pop_value(stack)?;
                (first, Some(self.opaque(first, opcode, Some(&target))))
            }
            EXT1 | EXT2 | EXT4 => {
                let len = match opcode {
                    EXT1 => 1,
                    EXT2 => 2,
                    _ => 4,
                };
                self.take(len)?;
                (start, Some(self.opaque(start, opcode, None)))
            }
            BINPUT | LONG_BINPUT => {
                let (index, width) = self.index(opcode == BINPUT)?;
                let (first, value) = self.pop_value(stack)?;
                let global = match value.value() {
                    Node::Global(global) => Some(global.clone()),
                    _ => None,
                };
                self.memo.insert(index, global);
                // The tree writes a put before the items of a list or a
                // dict. A put after the items cannot keep its place.
                let node = if is_filled(&value) {
                    self.opaque(first, opcode, Some(&value))
                } else {
                    Node::Put(Put {
                        index,
                        width,
                        value: Box::new(value),
                    })
                };
                (first, Some(node))
            }
            BINGET | LONG_BINGET => {
                let (index, width) = self.index(opcode == BINGET)?;
                if !self.memo.contains_key(&index) {
                    return Err(self.error(format!("memo index {index} is not stored")));
                }
                (start, Some(Node::Get(Get { index, width })))
            }
            _ => {
                return Err(Error::Decode {
                    offset: start,
                    message: format!("opcode 0x{opcode:02x} is not supported"),
                });
            }
        };
        stack.push(Entry { start, node });
        Ok(())
    }

    /// Adds a batch to the list or the dict on top of the stack. If the top
    /// is another value, for example an object that REDUCE makes, the top
    /// and the batch become one opaque node.
    fn fill(&self, stack: &mut [Entry], opcode: u8, items: Items) -> Result<(), Error> {
        let Some(Entry {
            start,
            node: Some(target),
        }) = stack.last_mut()
        else {
            return Err(self.error("no list or dict to fill"));
        };
        match (target.value_mut(), items) {
            (Node::List(batches), Items::List(batch)) => batches.push(batch),
            (Node::Dict(batches), Items::Dict(batch)) => batches.push(batch),
            _ => {
                let node = self.opaque(*start, opcode, Some(target));
                *target = node;
            }
        }
        Ok(())
    }

    /// Returns the bytes from `start` to the current offset as an opaque
    /// node. If `first`, the first value in the span, is opaque, the node
    /// keeps its opcode.
    fn opaque(&self, start: usize, opcode: u8, first: Option<&Node>) -> Node {
        let opcode = match first.map(Node::value) {
            Some(Node::Opaque(inner)) => inner.opcode,
            _ => opcode,
        };
        Node::Opaque(Opaque {
            bytes: self.bytes[start..self.pos].to_vec(),
            opcode,
        })
    }

    fn is_known_constructor(&self, callable: &Node) -> bool {
        let global = match callable.value() {
            Node::Global(global) => Some(global),
            Node::Get(get) => self.memo.get(&get.index).and_then(Option::as_ref),
            _ => None,
        };
        global.is_some_and(|global| {
            KNOWN_CONSTRUCTORS.contains(&(global.module.as_str(), global.name.as_str()))
        })
    }

    fn pop_value(&self, stack: &mut Vec<Entry>) -> Result<(usize, Node), Error> {
        match stack.pop() {
            Some(Entry {
                start,
                node: Some(node),
            }) => Ok((start, node)),
            _ => Err(self.error("no value on the stack")),
        }
    }

    /// Pops `n` values. Returns the start of the first one and the values in
    /// stack order.
    fn pop_values(&self, stack: &mut Vec<Entry>, n: usize) -> Result<(usize, Vec<Node>), Error> {
        let at = stack
            .len()
            .checked_sub(n)
            .ok_or_else(|| self.error("the stack is too short"))?;
        let entries = stack.split_off(at);
        let start = entries.first().map_or(self.pos, |entry| entry.start);
        let items = entries
            .into_iter()
            .map(|entry| entry.node)
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| self.error("a mark is in the way"))?;
        Ok((start, items))
    }

    /// Pops the values down to the last mark, then the mark. Returns the
    /// start of the mark and the values in stack order.
    fn pop_mark(&self, stack: &mut Vec<Entry>) -> Result<(usize, Vec<Node>), Error> {
        let mark = stack
            .iter()
            .rposition(|entry| entry.node.is_none())
            .ok_or_else(|| self.error("no mark on the stack"))?;
        let items = stack
            .drain(mark + 1..)
            .filter_map(|entry| entry.node)
            .collect();
        let start = stack.pop().map_or(self.pos, |entry| entry.start);
        Ok((start, items))
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
    fn length(&mut self, short: bool) -> Result<(usize, Width), Error> {
        if short {
            return Ok((usize::from(self.u8()?), Width::Short));
        }
        let len = u32::from_le_bytes(self.array()?);
        Ok((len as usize, Width::Long))
    }

    fn bytes(&mut self, short: bool) -> Result<Bytes, Error> {
        let (len, width) = self.length(short)?;
        let data = self.take(len)?.to_vec();
        Ok(Bytes { data, width })
    }

    fn index(&mut self, short: bool) -> Result<(u32, Width), Error> {
        if short {
            return Ok((u32::from(self.u8()?), Width::Short));
        }
        Ok((u32::from_le_bytes(self.array()?), Width::Long))
    }

    /// Reads up to the next newline. Returns the bytes before it.
    fn line(&mut self) -> Result<&'a [u8], Error> {
        let rest = &self.bytes[self.pos..];
        let len = rest
            .iter()
            .position(|byte| *byte == b'\n')
            .ok_or_else(|| self.error("GLOBAL has no newline"))?;
        self.pos += len + 1;
        Ok(&rest[..len])
    }

    fn error(&self, message: impl Into<String>) -> Error {
        Error::Decode {
            offset: self.pos,
            message: message.into(),
        }
    }
}

fn tuple(items: Vec<Node>, marked: bool) -> Node {
    Node::Tuple(Tuple { items, marked })
}

fn pairs(items: Vec<Node>) -> Vec<(Node, Node)> {
    let mut items = items.into_iter();
    let mut pairs = Vec::with_capacity(items.len() / 2);
    while let (Some(key), Some(value)) = (items.next(), items.next()) {
        pairs.push((key, value));
    }
    pairs
}

/// Tells if a node is a list or a dict that has items already.
fn is_filled(node: &Node) -> bool {
    match node.value() {
        Node::List(batches) => !batches.is_empty(),
        Node::Dict(batches) => !batches.is_empty(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode_record;

    /// The class pickle of the test records: PROTO 3, GLOBAL m C, BINPUT 0.
    const CLASS: &[u8] = b"\x80\x03cm\nC\nq\x00.";

    /// Decodes a record whose state pickle is PROTO 3, `body`, STOP. Checks
    /// that the record encodes to the same bytes. Returns the state.
    fn state(body: &[u8]) -> Node {
        let record = [CLASS, b"\x80\x03", body, b"."].concat();
        let tree = decode_record(&record).unwrap();
        assert_eq!(encode_record(&tree).unwrap(), record);
        tree.state().clone()
    }

    fn decode_error(body: &[u8]) -> String {
        let record = [CLASS, b"\x80\x03", body, b"."].concat();
        decode_record(&record).unwrap_err().to_string()
    }

    fn opaque(bytes: &[u8], opcode: u8) -> Node {
        Node::Opaque(Opaque {
            bytes: bytes.to_vec(),
            opcode,
        })
    }

    fn put(index: u32, value: Node) -> Node {
        Node::Put(Put {
            index,
            width: Width::Short,
            value: Box::new(value),
        })
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
            (b"\x88", Node::Bool(true)),
            (b"\x89", Node::Bool(false)),
            (b"K\x05", Node::Int(Int::U8(5))),
            (b"M\x00\x01", Node::Int(Int::U16(256))),
            (b"J\xff\xff\xff\xff", Node::Int(Int::I32(-1))),
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
            (b"G?\xf8\x00\x00\x00\x00\x00\x00", Node::Float(1.5)),
            (b"C\x02ab", Node::Bytes(bytes(b"ab", short))),
            (b"B\x02\x00\x00\x00ab", Node::Bytes(bytes(b"ab", long))),
            (b"U\x02ab", Node::ByteString(bytes(b"ab", short))),
            (b"T\x02\x00\x00\x00ab", Node::ByteString(bytes(b"ab", long))),
            (b"X\x02\x00\x00\x00\xc3\xa9", Node::Unicode("é".to_owned())),
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
    fn containers_keep_their_batches_and_tuple_forms() {
        let int = |value| Node::Int(Int::U8(value));
        let list = state(b"]q\x01(K\x01K\x02eK\x03a");
        let batches = vec![
            Batch {
                items: vec![int(1), int(2)],
                marked: true,
            },
            Batch {
                items: vec![int(3)],
                marked: false,
            },
        ];
        assert_eq!(list, put(1, Node::List(batches)));

        let dict = state(b"}(K\x01K\x02uK\x03K\x04s");
        let batches = vec![
            Batch {
                items: vec![(int(1), int(2))],
                marked: true,
            },
            Batch {
                items: vec![(int(3), int(4))],
                marked: false,
            },
        ];
        assert_eq!(dict, Node::Dict(batches));

        assert_eq!(state(b")"), tuple(vec![], false));
        assert_eq!(state(b"(t"), tuple(vec![], true));
        assert_eq!(state(b"K\x01K\x02\x86"), tuple(vec![int(1), int(2)], false));
        assert_eq!(state(b"(K\x01t"), tuple(vec![int(1)], true));
    }

    #[test]
    fn both_pickles_share_one_memo() {
        let get = Node::Get(Get {
            index: 0,
            width: Width::Short,
        });
        assert_eq!(state(b"h\x00"), get);
        assert_eq!(
            state(b"j\x00\x00\x00\x00"),
            Node::Get(Get {
                index: 0,
                width: Width::Long,
            })
        );
        assert!(decode_error(b"h\x05").contains("memo index 5 is not stored"));
    }

    #[test]
    fn a_persistent_reference_holds_its_persistent_id() {
        let node = state(b"C\x01\x07h\x00\x86Q");
        let Node::Persistent(pid) = node else {
            panic!("{node:?}");
        };
        assert!(matches!(*pid, Node::Tuple(_)));
    }

    #[test]
    fn reduce_of_an_unknown_callable_is_opaque() {
        let body = b"cm\nf\nq\x01K\x01\x85R";
        assert_eq!(state(body), opaque(body, REDUCE));
        // A put after the opaque value stays outside it.
        let node = state(b"cm\nf\nq\x01K\x01\x85Rq\x02");
        assert_eq!(node, put(2, opaque(body, REDUCE)));
    }

    #[test]
    fn reduce_of_set_is_modeled_also_through_a_get() {
        let node = state(b"(cbuiltins\nset\nq\x01)\x85Rh\x01)\x85Rt");
        let Node::Tuple(tuple) = node else {
            panic!("{node:?}");
        };
        assert!(
            tuple
                .items
                .iter()
                .all(|item| matches!(item, Node::Reduce(_)))
        );
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
    }

    #[test]
    fn extension_codes_are_opaque() {
        assert_eq!(state(b"\x82\x05"), opaque(b"\x82\x05", EXT1));
        assert_eq!(state(b"\x83\x05\x01"), opaque(b"\x83\x05\x01", EXT2));
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
    fn a_pickle_without_proto_decodes() {
        let record = b"cm\nC\n.}.";
        let tree = decode_record(record).unwrap();
        assert_eq!(tree.state(), &Node::Dict(vec![]));
        assert_eq!(encode_record(&tree).unwrap(), record);
    }

    #[test]
    fn bad_records_are_errors() {
        for (body, message) in [
            (&b"K"[..], "ends too early"),
            (b"I1\n", "opcode 0x49 is not supported"),
            (b"\x8c\x01a", "opcode 0x8c is not supported"),
            (b"NN", "exactly one value"),
            (b"K\x01t", "no mark"),
            (b"}(K\x01u", "odd number"),
            (b"(", "exactly one value"),
            (b"(\x85", "a mark is in the way"),
            (b"K\x01a", "no list or dict"),
        ] {
            let error = decode_error(body);
            assert!(error.contains(message), "{body:?}: {error}");
        }
        let trailing = [CLASS, b"N.", b"N"].concat();
        let error = decode_record(&trailing).unwrap_err().to_string();
        assert!(error.contains("bytes follow"), "{error}");
        assert!(decode_record(CLASS).is_err());
    }
}
