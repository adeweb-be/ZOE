//! The encoder: a tree back to pickles.
//!
//! Each pickle starts with the PROTO opcode that the tree keeps, if any.
//! Each node writes the opcode form that it keeps, in the order of
//! `order.rs`. A value that its opcode cannot hold is an error: the encoder
//! never picks another opcode. The encoder also refuses a value that the
//! decoder refuses, so its output always decodes.

pub(crate) mod order;

use crate::error::Error;
use crate::opcode::*;
use crate::text;
use crate::tree::{Bool, ByteString, Float, Global, Int, MemoForm, Node, Tree, Unicode, Width};
use order::{Op, Order};

/// Encodes a tree: the class pickle, then the state pickle.
pub fn encode_record(tree: &Tree) -> Result<Vec<u8>, Error> {
    let mut out = Vec::new();
    for pickle in [&tree.class, &tree.state] {
        if let Some(protocol) = pickle.protocol {
            if protocol > HIGHEST_PROTOCOL {
                return Err(Error::Encode(format!(
                    "protocol {protocol} is not supported"
                )));
            }
            out.extend([PROTO, protocol]);
        }
        for op in Order::new(&pickle.root) {
            match op? {
                Op::Byte(opcode) => out.push(opcode),
                Op::Put(put, _) => memo(&mut out, [BINPUT, LONG_BINPUT, PUT], put.index, put.form)?,
                Op::Leaf(node) => leaf(&mut out, node)?,
            }
        }
        out.push(STOP);
    }
    Ok(out)
}

/// Writes a node that has no children.
fn leaf(out: &mut Vec<u8>, node: &Node) -> Result<(), Error> {
    match node {
        Node::None => out.push(NONE),
        Node::Bool(Bool::New(value)) => out.push(if *value { NEWTRUE } else { NEWFALSE }),
        Node::Bool(Bool::Text(value)) => {
            let valid = text::int_bool(value.as_bytes()).is_some();
            line(out, INT, value.as_bytes(), valid)?;
        }
        Node::Int(value) => int(out, value)?,
        Node::Float(Float::Binary(value)) => {
            out.push(BINFLOAT);
            out.extend(value.to_be_bytes());
        }
        Node::Float(Float::Text(value)) => {
            line(
                out,
                FLOAT,
                value.as_bytes(),
                text::is_float(value.as_bytes()),
            )?;
        }
        Node::Bytes(bytes) => sized(
            out,
            [SHORT_BINBYTES, BINBYTES],
            &bytes.data,
            bytes.width,
            u32::MAX as usize,
        )?,
        Node::ByteString(ByteString::Binary(bytes)) => sized(
            out,
            [SHORT_BINSTRING, BINSTRING],
            &bytes.data,
            bytes.width,
            i32::MAX as usize,
        )?,
        Node::ByteString(ByteString::Text(raw)) => line(out, STRING, raw, text::is_quoted(raw))?,
        Node::Unicode(Unicode::Binary(value)) => {
            sized(
                out,
                [0, BINUNICODE],
                value.as_bytes(),
                Width::Long,
                u32::MAX as usize,
            )?;
        }
        Node::Unicode(Unicode::Text(raw)) => line(out, UNICODE, raw, text::is_raw_unicode(raw))?,
        Node::Global(value) => global(out, value)?,
        Node::Get(get) => memo(out, [BINGET, LONG_BINGET, GET], get.index, get.form)?,
        Node::Opaque(opaque) => out.extend(&opaque.bytes),
        Node::Tuple(_)
        | Node::List(_)
        | Node::Dict(_)
        | Node::Persistent(_)
        | Node::Reduce(_)
        | Node::Put(_) => return Err(Error::Encode("a container is not a leaf".to_owned())),
    }
    Ok(())
}

fn int(out: &mut Vec<u8>, value: &Int) -> Result<(), Error> {
    match value {
        Int::U8(value) => out.extend([BININT1, *value]),
        Int::U16(value) => {
            out.push(BININT2);
            out.extend(value.to_le_bytes());
        }
        Int::I32(value) => {
            out.push(BININT);
            out.extend(value.to_le_bytes());
        }
        Int::Long { bytes, width } => {
            sized(out, [LONG1, LONG4], bytes, *width, i32::MAX as usize)?;
        }
        Int::Text(value) => {
            // The C unpickler of Python reads some texts as a bool.
            let bytes = value.as_bytes();
            let valid = text::is_int(bytes, false) && text::int_bool(bytes).is_none();
            line(out, INT, bytes, valid)?;
        }
        Int::LongText(value) => {
            line(
                out,
                LONG,
                value.as_bytes(),
                text::is_int(value.as_bytes(), true),
            )?;
        }
    }
    Ok(())
}

/// Writes an opcode, a length and the bytes. The short opcode has a length
/// of one byte. The long opcode has a length of four bytes, at most `max`.
/// A zero short opcode means that the opcode has no short form.
fn sized(
    out: &mut Vec<u8>,
    [short, long]: [u8; 2],
    data: &[u8],
    width: Width,
    max: usize,
) -> Result<(), Error> {
    let len = data.len();
    match width {
        Width::Short if short != 0 => {
            let len = u8::try_from(len)
                .map_err(|_| Error::Encode(format!("length {len} does not fit one byte")))?;
            out.extend([short, len]);
        }
        Width::Short => return Err(Error::Encode("the opcode has no short form".to_owned())),
        Width::Long => {
            let len = u32::try_from(len)
                .ok()
                .filter(|len| *len as usize <= max)
                .ok_or_else(|| Error::Encode(format!("length {len} is more than {max}")))?;
            out.push(long);
            out.extend(len.to_le_bytes());
        }
    }
    out.extend(data);
    Ok(())
}

/// Writes a put or a get: `[short, long, text]` are the opcodes of the
/// three forms.
fn memo(out: &mut Vec<u8>, opcodes: [u8; 3], index: u32, form: MemoForm) -> Result<(), Error> {
    let [short, long, text] = opcodes;
    match form {
        MemoForm::Short => {
            let index = u8::try_from(index)
                .map_err(|_| Error::Encode(format!("memo index {index} does not fit one byte")))?;
            out.extend([short, index]);
        }
        MemoForm::Long => {
            out.push(long);
            out.extend(index.to_le_bytes());
        }
        MemoForm::Text => {
            out.push(text);
            out.extend(index.to_string().as_bytes());
            out.push(b'\n');
        }
    }
    Ok(())
}

/// Writes an opcode, a text argument and a newline. `valid` tells if the
/// decoder accepts the text.
fn line(out: &mut Vec<u8>, opcode: u8, text: &[u8], valid: bool) -> Result<(), Error> {
    if !valid || text.contains(&b'\n') {
        return Err(Error::Encode(format!(
            "the text {:?} is not valid for opcode 0x{opcode:02x}",
            String::from_utf8_lossy(text)
        )));
    }
    out.push(opcode);
    out.extend(text);
    out.push(b'\n');
    Ok(())
}

fn global(out: &mut Vec<u8>, global: &Global) -> Result<(), Error> {
    for part in [&global.module, &global.name] {
        if part.is_empty() || part.contains('\n') {
            return Err(Error::Encode(format!(
                "the GLOBAL name {part:?} is empty or holds a newline"
            )));
        }
    }
    out.push(GLOBAL);
    for part in [&global.module, &global.name] {
        out.extend(part.as_bytes());
        out.push(b'\n');
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{Batch, Bytes, Dict, Get, List, Opaque, Pickle, Put, Reduce, Tuple};

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

    fn put(index: u32, form: MemoForm, value: Node) -> Node {
        Node::Put(Put {
            index,
            form,
            value: Box::new(value),
        })
    }

    fn get(index: u32) -> Node {
        Node::Get(Get {
            index,
            form: MemoForm::Short,
        })
    }

    fn list(initial: Option<Vec<Node>>, batches: Vec<Batch<Node>>) -> Node {
        Node::List(List { initial, batches })
    }

    fn one(item: Node) -> Batch<Node> {
        Batch {
            items: vec![item],
            marked: false,
        }
    }

    fn encode_state(state: Node) -> Result<Vec<u8>, Error> {
        let bytes = encode_record(&record(Node::None, state))?;
        Ok(bytes[b"\x80\x03N.\x80\x03".len()..].to_vec())
    }

    #[test]
    fn writes_each_node_with_its_opcode() {
        let class = put(0, MemoForm::Short, global("m", "C"));
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
            MemoForm::Long,
            Node::Dict(Dict {
                initial: None,
                batches: vec![
                    Batch {
                        items: vec![
                            (
                                Node::Unicode(Unicode::Binary("a".to_owned())),
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
                ],
            }),
        );
        let expected: &[u8] = b"\x80\x03cm\nC\nq\x00.\
            \x80\x03}r\x01\x00\x00\x00(X\x01\x00\x00\x00aC\x01\x07h\x00\x86Q\
            J\xfe\xff\xff\xffcbuiltins\nset\n)Ru\
            N\x82\x01s.";
        assert_eq!(encode_record(&record(class, state)).unwrap(), expected);
    }

    #[test]
    fn writes_the_puts_of_a_list_before_its_items() {
        let node = put(
            3,
            MemoForm::Short,
            put(
                2,
                MemoForm::Short,
                list(None, vec![one(Node::Bool(Bool::New(true)))]),
            ),
        );
        assert_eq!(encode_state(node).unwrap(), b"]q\x02q\x03\x88a.");
    }

    #[test]
    fn writes_the_puts_of_other_values_after_them() {
        let tuple = Node::Tuple(Tuple {
            items: vec![get(0)],
            marked: true,
        });
        let node = put(2, MemoForm::Long, put(1, MemoForm::Short, tuple));
        assert_eq!(
            encode_state(node).unwrap(),
            b"(h\x00tq\x01r\x02\x00\x00\x00."
        );
    }

    #[test]
    fn writes_the_protocol_0_forms() {
        let dict = Node::Dict(Dict {
            initial: Some(vec![]),
            batches: vec![Batch {
                items: vec![(
                    put(
                        1,
                        MemoForm::Text,
                        Node::Unicode(Unicode::Text(b"a".to_vec())),
                    ),
                    put(
                        2,
                        MemoForm::Text,
                        list(
                            Some(vec![Node::Int(Int::Text("-5".to_owned()))]),
                            vec![one(Node::Float(Float::Text("1.5".to_owned())))],
                        ),
                    ),
                )],
                marked: false,
            }],
        });
        let node = put(
            0,
            MemoForm::Text,
            Node::Tuple(Tuple {
                items: vec![
                    dict,
                    Node::Bool(Bool::Text("01".to_owned())),
                    Node::Int(Int::LongText("7L".to_owned())),
                    Node::ByteString(ByteString::Text(b"'x'".to_vec())),
                    Node::Get(Get {
                        index: 1,
                        form: MemoForm::Text,
                    }),
                ],
                marked: true,
            }),
        );
        assert_eq!(
            encode_state(node).unwrap(),
            b"((dVa\np1\n(I-5\nlp2\nF1.5\nasI01\nL7L\nS'x'\ng1\ntp0\n."
        );
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
            put(256, MemoForm::Short, Node::None),
            Node::Get(Get {
                index: 300,
                form: MemoForm::Short,
            }),
            Node::Bytes(Bytes {
                data: vec![0; 256],
                width: Width::Short,
            }),
            Node::Int(Int::Long {
                bytes: vec![0; 256],
                width: Width::Short,
            }),
            list(
                None,
                vec![Batch {
                    items: vec![int.clone(), int],
                    marked: false,
                }],
            ),
            global("m", "a\nb"),
            global("", "b"),
            Node::Int(Int::Text("01".to_owned())),
            Node::Int(Int::Text("+1".to_owned())),
            Node::Bool(Bool::Text("1".to_owned())),
            Node::Bool(Bool::Text("10".to_owned())),
            Node::Int(Int::Text("1x".to_owned())),
            Node::Int(Int::LongText("1LL".to_owned())),
            Node::Float(Float::Text("one".to_owned())),
            Node::ByteString(ByteString::Text(b"x".to_vec())),
            Node::Unicode(Unicode::Text(b"a\nb".to_vec())),
        ];
        for node in cases {
            assert!(
                matches!(encode_state(node.clone()), Err(Error::Encode(_))),
                "{node:?}"
            );
        }
    }

    #[test]
    fn refuses_an_unknown_protocol() {
        let mut tree = record(Node::None, Node::None);
        tree.state.protocol = Some(6);
        assert!(matches!(encode_record(&tree), Err(Error::Encode(_))));
    }
}
