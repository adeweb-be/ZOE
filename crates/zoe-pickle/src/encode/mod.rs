//! The encoder: a tree back to protocol-3 pickles. This module is a red
//! zone.
//!
//! Each pickle starts with the PROTO opcode that the tree keeps, if any.
//! Each node writes the opcode that it keeps. A value that its opcode
//! cannot hold is an error: the encoder never picks another opcode.

use crate::error::Error;
use crate::opcode::*;
use crate::tree::{Batch, Bytes, Global, Int, Node, Pickle, Tree, Tuple, Width};

/// Encodes a tree: the class pickle, then the state pickle.
pub fn encode_record(tree: &Tree) -> Result<Vec<u8>, Error> {
    let mut encoder = Encoder { out: Vec::new() };
    encoder.pickle(&tree.class)?;
    encoder.pickle(&tree.state)?;
    Ok(encoder.out)
}

struct Encoder {
    out: Vec<u8>,
}

impl Encoder {
    fn pickle(&mut self, pickle: &Pickle) -> Result<(), Error> {
        if let Some(protocol) = pickle.protocol {
            self.out.extend([PROTO, protocol]);
        }
        self.node(&pickle.root)?;
        self.out.push(STOP);
        Ok(())
    }

    fn node(&mut self, node: &Node) -> Result<(), Error> {
        self.create(node)?;
        self.fill(node)
    }

    /// Writes the opcodes that push the value. For a put, this includes the
    /// put. A list or a dict is still empty after this step.
    fn create(&mut self, node: &Node) -> Result<(), Error> {
        match node {
            Node::None => self.out.push(NONE),
            Node::Bool(true) => self.out.push(NEWTRUE),
            Node::Bool(false) => self.out.push(NEWFALSE),
            Node::Int(int) => self.int(int)?,
            Node::Float(value) => {
                self.out.push(BINFLOAT);
                self.out.extend(value.to_be_bytes());
            }
            Node::Bytes(bytes) => self.sized(SHORT_BINBYTES, BINBYTES, bytes)?,
            Node::ByteString(bytes) => self.sized(SHORT_BINSTRING, BINSTRING, bytes)?,
            Node::Unicode(text) => {
                self.out.push(BINUNICODE);
                self.out.extend(len_u32(text.len())?.to_le_bytes());
                self.out.extend(text.as_bytes());
            }
            Node::Tuple(tuple) => self.tuple(tuple)?,
            Node::List(_) => self.out.push(EMPTY_LIST),
            Node::Dict(_) => self.out.push(EMPTY_DICT),
            Node::Global(global) => self.global(global)?,
            Node::Persistent(pid) => {
                self.node(pid)?;
                self.out.push(BINPERSID);
            }
            Node::Reduce(reduce) => {
                self.node(&reduce.callable)?;
                self.node(&reduce.args)?;
                self.out.push(REDUCE);
            }
            Node::Put(put) => {
                self.create(&put.value)?;
                self.index(BINPUT, LONG_BINPUT, put.index, put.width)?;
            }
            Node::Get(get) => self.index(BINGET, LONG_BINGET, get.index, get.width)?,
            Node::Opaque(opaque) => self.out.extend(&opaque.bytes),
        }
        Ok(())
    }

    /// Writes the batches of a list or a dict, after its put.
    fn fill(&mut self, node: &Node) -> Result<(), Error> {
        match node {
            Node::List(batches) => {
                for batch in batches {
                    self.batch(batch, APPEND, APPENDS, |encoder, item| encoder.node(item))?;
                }
            }
            Node::Dict(batches) => {
                for batch in batches {
                    self.batch(batch, SETITEM, SETITEMS, |encoder, (key, value)| {
                        encoder.node(key)?;
                        encoder.node(value)
                    })?;
                }
            }
            Node::Put(put) => self.fill(&put.value)?,
            _ => {}
        }
        Ok(())
    }

    fn batch<T>(
        &mut self,
        batch: &Batch<T>,
        one: u8,
        many: u8,
        item: impl Fn(&mut Self, &T) -> Result<(), Error>,
    ) -> Result<(), Error> {
        if batch.marked {
            self.out.push(MARK);
        } else if batch.items.len() != 1 {
            return Err(Error::Encode(format!(
                "a batch without a mark has {} items, not 1",
                batch.items.len()
            )));
        }
        for value in &batch.items {
            item(self, value)?;
        }
        self.out.push(if batch.marked { many } else { one });
        Ok(())
    }

    fn int(&mut self, int: &Int) -> Result<(), Error> {
        match int {
            Int::U8(value) => self.out.extend([BININT1, *value]),
            Int::U16(value) => {
                self.out.push(BININT2);
                self.out.extend(value.to_le_bytes());
            }
            Int::I32(value) => {
                self.out.push(BININT);
                self.out.extend(value.to_le_bytes());
            }
            Int::Long { bytes, width } => {
                self.length(LONG1, LONG4, bytes.len(), *width)?;
                self.out.extend(bytes);
            }
        }
        Ok(())
    }

    fn sized(&mut self, short: u8, long: u8, bytes: &Bytes) -> Result<(), Error> {
        self.length(short, long, bytes.data.len(), bytes.width)?;
        self.out.extend(&bytes.data);
        Ok(())
    }

    /// Writes an opcode and a length: one byte for the short opcode, four
    /// bytes for the long one.
    fn length(&mut self, short: u8, long: u8, len: usize, width: Width) -> Result<(), Error> {
        match width {
            Width::Short => {
                let len = u8::try_from(len)
                    .map_err(|_| Error::Encode(format!("length {len} does not fit one byte")))?;
                self.out.extend([short, len]);
            }
            Width::Long => {
                self.out.push(long);
                self.out.extend(len_u32(len)?.to_le_bytes());
            }
        }
        Ok(())
    }

    fn index(&mut self, short: u8, long: u8, index: u32, width: Width) -> Result<(), Error> {
        match width {
            Width::Short => {
                let index = u8::try_from(index).map_err(|_| {
                    Error::Encode(format!("memo index {index} does not fit one byte"))
                })?;
                self.out.extend([short, index]);
            }
            Width::Long => {
                self.out.push(long);
                self.out.extend(index.to_le_bytes());
            }
        }
        Ok(())
    }

    fn tuple(&mut self, tuple: &Tuple) -> Result<(), Error> {
        if tuple.marked {
            self.out.push(MARK);
        }
        for item in &tuple.items {
            self.node(item)?;
        }
        let opcode = match (tuple.marked, tuple.items.len()) {
            (true, _) => TUPLE,
            (false, 0) => EMPTY_TUPLE,
            (false, 1) => TUPLE1,
            (false, 2) => TUPLE2,
            (false, 3) => TUPLE3,
            (false, len) => {
                return Err(Error::Encode(format!(
                    "a tuple without a mark has {len} items"
                )));
            }
        };
        self.out.push(opcode);
        Ok(())
    }

    fn global(&mut self, global: &Global) -> Result<(), Error> {
        if global.module.contains('\n') || global.name.contains('\n') {
            return Err(Error::Encode("a GLOBAL name contains a newline".to_owned()));
        }
        self.out.push(GLOBAL);
        for part in [&global.module, &global.name] {
            self.out.extend(part.as_bytes());
            self.out.push(b'\n');
        }
        Ok(())
    }
}

fn len_u32(len: usize) -> Result<u32, Error> {
    u32::try_from(len).map_err(|_| Error::Encode(format!("length {len} does not fit four bytes")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{Get, Opaque, Put, Reduce};

    fn record(class: Node, state: Node) -> Tree {
        let pickle = |root| Pickle {
            protocol: Some(3),
            root,
        };
        Tree {
            class: pickle(class),
            state: pickle(state),
        }
    }

    fn global(module: &str, name: &str) -> Node {
        Node::Global(Global {
            module: module.to_owned(),
            name: name.to_owned(),
        })
    }

    fn put(index: u32, width: Width, value: Node) -> Node {
        Node::Put(Put {
            index,
            width,
            value: Box::new(value),
        })
    }

    fn get(index: u32) -> Node {
        Node::Get(Get {
            index,
            width: Width::Short,
        })
    }

    fn encode_state(state: Node) -> Result<Vec<u8>, Error> {
        let bytes = encode_record(&record(Node::None, state))?;
        Ok(bytes[b"\x80\x03N.\x80\x03".len()..].to_vec())
    }

    #[test]
    fn writes_each_node_with_its_opcode() {
        let class = put(0, Width::Short, global("m", "C"));
        let pid = Node::Tuple(Tuple {
            items: vec![
                Node::Bytes(Bytes {
                    data: vec![7],
                    width: Width::Short,
                }),
                get(0),
            ],
            marked: false,
        });
        let set = Node::Reduce(Reduce {
            callable: Box::new(global("builtins", "set")),
            args: Box::new(Node::Tuple(Tuple {
                items: vec![],
                marked: false,
            })),
        });
        let state = put(
            1,
            Width::Long,
            Node::Dict(vec![
                Batch {
                    items: vec![
                        (
                            Node::Unicode("a".to_owned()),
                            Node::Persistent(Box::new(pid)),
                        ),
                        (Node::Int(Int::I32(-2)), set),
                    ],
                    marked: true,
                },
                Batch {
                    items: vec![(
                        Node::None,
                        Node::Opaque(Opaque {
                            bytes: b"\x82\x01".to_vec(),
                            opcode: EXT1,
                        }),
                    )],
                    marked: false,
                },
            ]),
        );
        let expected: &[u8] = b"\x80\x03cm\nC\nq\x00.\
            \x80\x03}r\x01\x00\x00\x00(X\x01\x00\x00\x00aC\x01\x07h\x00\x86Q\
            J\xfe\xff\xff\xffcbuiltins\nset\n)Ru\
            N\x82\x01s.";
        assert_eq!(encode_record(&record(class, state)).unwrap(), expected);
    }

    #[test]
    fn writes_the_put_of_a_list_before_its_items() {
        let list = Node::List(vec![Batch {
            items: vec![Node::Bool(true)],
            marked: false,
        }]);
        let node = put(2, Width::Short, list);
        assert_eq!(encode_state(node).unwrap(), b"]q\x02\x88a.");
    }

    #[test]
    fn writes_no_proto_for_a_pickle_without_one() {
        let mut tree = record(global("m", "C"), Node::None);
        tree.class.protocol = None;
        tree.state.protocol = None;
        assert_eq!(encode_record(&tree).unwrap(), b"cm\nC\n.N.");
    }

    #[test]
    fn refuses_a_value_that_its_opcode_cannot_hold() {
        let int = Node::Int(Int::U8(1));
        let cases = [
            Node::Tuple(Tuple {
                items: vec![int.clone(); 4],
                marked: false,
            }),
            put(256, Width::Short, Node::None),
            Node::Get(Get {
                index: 300,
                width: Width::Short,
            }),
            Node::Bytes(Bytes {
                data: vec![0; 256],
                width: Width::Short,
            }),
            Node::Int(Int::Long {
                bytes: vec![0; 256],
                width: Width::Short,
            }),
            Node::List(vec![Batch {
                items: vec![int.clone(), int],
                marked: false,
            }]),
            global("m", "a\nb"),
        ];
        for node in cases {
            assert!(
                matches!(encode_state(node.clone()), Err(Error::Encode(_))),
                "{node:?}"
            );
        }
    }
}
