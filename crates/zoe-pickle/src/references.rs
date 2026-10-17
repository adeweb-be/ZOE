//! The persistent ids of a record, for pack and the other tools that
//! follow references.
//!
//! The tree keeps the persistent ids of the parts that it models, but an
//! opaque node keeps only bytes. A REDUCE of an unknown callable, NEWOBJ
//! and BUILD hold persistent ids in those bytes. So [`persistent_ids`]
//! does not read the tree. After the decoder accepts the record, a small
//! stack machine runs over all the opcodes of both pickles, as the
//! `noload` of ZODB's `referencesf` does. It builds only the values that a
//! persistent id can hold: strings, tuples and lists. Every other value is
//! a placeholder. Each BINPERSID and PERSID gives one persistent id.
//!
//! The machine keeps tuples and lists in two arenas and refers to them by
//! index. So a value never owns another value, and a deep or a recursive
//! value cannot overflow the call stack when it drops.

use std::collections::HashMap;

use crate::decode::{Limits, decode_record_with};
use crate::error::Error;
use crate::opcode::*;

/// One persistent id of a record: the value that one BINPERSID or PERSID
/// takes. ZODB writes the forms below.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PersistentId {
    /// A reference to an object of the same database: `(oid, class)`, or
    /// the oid alone for a class with `__getnewargs__`. The value is the
    /// oid as the record stores it, 8 bytes for every oid that ZODB
    /// writes. These are the oids that ZODB's `referencesf` gives.
    Oid(Vec<u8>),
    /// A weak reference: `['w', (oid,)]`, or `['w', (oid, database)]`
    /// for an object of another database. It keeps nothing alive.
    Weak(Vec<u8>),
    /// A reference into another database: `['m', (database, oid, class)]`
    /// or `['n', (database, oid)]`. It keeps nothing alive in this
    /// database.
    Foreign {
        /// The name of the other database.
        database: Vec<u8>,
        /// The oid in the other database.
        oid: Vec<u8>,
    },
    /// A list of another form. ZODB's `referencesf` skips every list.
    List,
}

/// Returns the persistent ids of a record, in pickle order, with each
/// repeat. The class pickle and the state pickle both count, and so do the
/// parts that the tree keeps as opaque bytes.
///
/// The decoder reads the record first, with a node budget equal to the
/// length of the record, so that no budget refuses it. Its error is the
/// error of this function: a record that does not decode has no known
/// references, and a caller must not read it as a record without any.
///
/// A persistent id that is not a string, a tuple that starts with a
/// string, or a list is an error, as it is for `referencesf`. So is a text
/// oid that is not ASCII, and a protocol-0 text oid with escapes.
pub fn persistent_ids(record: &[u8]) -> Result<Vec<PersistentId>, Error> {
    let limits = Limits {
        node_budget: record.len(),
    };
    decode_record_with(record, limits)?;
    let mut scanner = Scanner {
        bytes: record,
        pos: 0,
        memo: HashMap::new(),
        tuples: Vec::new(),
        lists: Vec::new(),
        ids: Vec::new(),
    };
    scanner.pickle()?;
    scanner.pickle()?;
    Ok(scanner.ids)
}

/// How the bytes of a string are written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Form {
    /// SHORT_BINBYTES, BINBYTES, SHORT_BINSTRING or BINSTRING: the bytes.
    Binary,
    /// BINUNICODE: UTF-8.
    Utf8,
    /// STRING: the quoted text with escapes.
    Quoted,
    /// UNICODE: text in raw-unicode-escape.
    Raw,
}

/// A value of the machine.
#[derive(Clone, Copy, Debug)]
enum Value {
    /// A string: the start and the end of its bytes in the record, and
    /// their form.
    Text(usize, usize, Form),
    /// A tuple: an index in the tuple arena.
    Tuple(usize),
    /// A list: an index in the list arena.
    List(usize),
    /// Any other value.
    Other,
}

/// One entry of the stack.
#[derive(Clone, Copy, Debug)]
enum Entry {
    Mark,
    Value(Value),
}

struct Scanner<'a> {
    bytes: &'a [u8],
    pos: usize,
    /// The memo, shared by both pickles.
    memo: HashMap<u32, Value>,
    /// The items of each tuple.
    tuples: Vec<Vec<Value>>,
    /// The items of each list. APPEND and APPENDS add to them.
    lists: Vec<Vec<Value>>,
    ids: Vec<PersistentId>,
}

impl Scanner<'_> {
    /// Runs one pickle, up to and with its STOP.
    fn pickle(&mut self) -> Result<(), Error> {
        let mut stack = Vec::new();
        if self.bytes.get(self.pos) == Some(&PROTO) {
            self.pos += 2;
        }
        loop {
            let at = self.pos;
            let opcode = *self.bytes.get(at).ok_or_else(|| error(at, "no STOP"))?;
            if opcode == STOP {
                self.pos = at + 1;
                return Ok(());
            }
            let rest = &self.bytes[at + 1..];
            let len = argument_len(opcode, rest).ok_or_else(|| error(at, "unknown opcode"))?;
            let arg = at + 1..at + 1 + len;
            self.pos = arg.end;
            let value = match opcode {
                MARK => {
                    stack.push(Entry::Mark);
                    continue;
                }
                POP => {
                    stack.pop();
                    continue;
                }
                POP_MARK => {
                    pop_mark(&mut stack, at)?;
                    continue;
                }
                DUP => top(&stack, at)?,
                SHORT_BINBYTES | SHORT_BINSTRING => {
                    Value::Text(arg.start + 1, arg.end, Form::Binary)
                }
                BINBYTES | BINSTRING => Value::Text(arg.start + 4, arg.end, Form::Binary),
                BINUNICODE => Value::Text(arg.start + 4, arg.end, Form::Utf8),
                STRING => Value::Text(arg.start, arg.end - 1, Form::Quoted),
                UNICODE => Value::Text(arg.start, arg.end - 1, Form::Raw),
                EMPTY_TUPLE => self.tuple(Vec::new()),
                TUPLE1 | TUPLE2 | TUPLE3 => {
                    let n = usize::from(opcode - TUPLE1) + 1;
                    let items = pop_values(&mut stack, n, at)?;
                    self.tuple(items)
                }
                TUPLE => {
                    let items = pop_mark(&mut stack, at)?;
                    self.tuple(items)
                }
                EMPTY_LIST => self.list(Vec::new()),
                LIST => {
                    let items = pop_mark(&mut stack, at)?;
                    self.list(items)
                }
                APPEND => {
                    let items = pop_values(&mut stack, 1, at)?;
                    self.append(&stack, items, at)?;
                    continue;
                }
                APPENDS => {
                    let items = pop_mark(&mut stack, at)?;
                    self.append(&stack, items, at)?;
                    continue;
                }
                SETITEM => {
                    pop_values(&mut stack, 2, at)?;
                    continue;
                }
                SETITEMS => {
                    pop_mark(&mut stack, at)?;
                    continue;
                }
                BUILD => {
                    pop_values(&mut stack, 1, at)?;
                    continue;
                }
                DICT | INST | OBJ => {
                    pop_mark(&mut stack, at)?;
                    Value::Other
                }
                REDUCE | NEWOBJ => {
                    pop_values(&mut stack, 2, at)?;
                    Value::Other
                }
                PERSID => {
                    // The decoder checks that the line is ASCII. Python
                    // reads it as it is, with no escapes.
                    let line = Value::Text(arg.start, arg.end - 1, Form::Binary);
                    let oid = self.oid(&line, at)?;
                    self.ids.push(PersistentId::Oid(oid));
                    Value::Other
                }
                BINPERSID => {
                    let pid = pop_values(&mut stack, 1, at)?[0];
                    let id = self.persistent_id(&pid, at)?;
                    self.ids.push(id);
                    Value::Other
                }
                BINPUT | LONG_BINPUT | PUT => {
                    let index = memo_index(opcode, &self.bytes[arg], at)?;
                    self.memo.insert(index, top(&stack, at)?);
                    continue;
                }
                BINGET | LONG_BINGET | GET => {
                    let index = memo_index(opcode, &self.bytes[arg], at)?;
                    self.memo
                        .get(&index)
                        .copied()
                        .ok_or_else(|| error(at, "a get of a memo index that is not stored"))?
                }
                _ => Value::Other,
            };
            stack.push(Entry::Value(value));
        }
    }

    fn tuple(&mut self, items: Vec<Value>) -> Value {
        self.tuples.push(items);
        Value::Tuple(self.tuples.len() - 1)
    }

    fn list(&mut self, items: Vec<Value>) -> Value {
        self.lists.push(items);
        Value::List(self.lists.len() - 1)
    }

    /// Adds `items` to the list on top of the stack. Another value on top
    /// takes nothing, as `noload` does.
    fn append(&mut self, stack: &[Entry], items: Vec<Value>, at: usize) -> Result<(), Error> {
        if let Value::List(list) = top(stack, at)? {
            self.lists[list].extend(items);
        }
        Ok(())
    }

    /// Reads the form of the persistent id `pid`, which BINPERSID at `at`
    /// takes.
    fn persistent_id(&self, pid: &Value, at: usize) -> Result<PersistentId, Error> {
        match pid {
            Value::Text(..) => Ok(PersistentId::Oid(self.oid(pid, at)?)),
            Value::Tuple(tuple) => match self.tuples[*tuple].first() {
                Some(oid @ Value::Text(..)) => Ok(PersistentId::Oid(self.oid(oid, at)?)),
                _ => Err(error(at, "a persistent id is a tuple without an oid first")),
            },
            Value::List(list) => Ok(self.list_id(*list)),
            Value::Other => Err(error(
                at,
                "a persistent id is not a string, a tuple or a list",
            )),
        }
    }

    /// Reads a list persistent id: a weak reference, a reference into
    /// another database, or another list.
    fn list_id(&self, list: usize) -> PersistentId {
        let [marker, Value::Tuple(args)] = &self.lists[list][..] else {
            return PersistentId::List;
        };
        let text = |value: &Value| self.text(value);
        let args: Vec<Option<Vec<u8>>> = self.tuples[*args].iter().map(text).collect();
        match (text(marker).as_deref(), &args[..]) {
            (Some(b"w"), [Some(oid)] | [Some(oid), _]) => PersistentId::Weak(oid.clone()),
            (Some(b"m"), [Some(database), Some(oid), _])
            | (Some(b"n"), [Some(database), Some(oid)]) => PersistentId::Foreign {
                database: database.clone(),
                oid: oid.clone(),
            },
            _ => PersistentId::List,
        }
    }

    /// Returns the bytes of an oid. A text oid must be ASCII, as
    /// `referencesf` encodes it to ASCII.
    fn oid(&self, value: &Value, at: usize) -> Result<Vec<u8>, Error> {
        let bytes = self
            .text(value)
            .ok_or_else(|| error(at, "a protocol-0 text oid has escapes"))?;
        let ascii = matches!(value, Value::Text(_, _, Form::Binary)) || bytes.is_ascii();
        if !ascii {
            return Err(error(at, "a text oid is not ASCII"));
        }
        Ok(bytes)
    }

    /// Returns the bytes of a string. Returns `None` for another value,
    /// and for a protocol-0 text with escapes: ZODB writes no oid and no
    /// marker in that form.
    fn text(&self, value: &Value) -> Option<Vec<u8>> {
        let Value::Text(start, end, form) = *value else {
            return None;
        };
        let bytes = &self.bytes[start..end];
        let inner = match form {
            Form::Binary | Form::Utf8 => bytes,
            Form::Quoted => bytes.get(1..bytes.len().checked_sub(1)?)?,
            Form::Raw => bytes,
        };
        let escaped = matches!(form, Form::Quoted | Form::Raw) && inner.contains(&b'\\');
        (!escaped).then(|| inner.to_vec())
    }
}

/// Returns the value on top of the stack.
fn top(stack: &[Entry], at: usize) -> Result<Value, Error> {
    match stack.last() {
        Some(Entry::Value(value)) => Ok(*value),
        _ => Err(error(at, "no value on the stack")),
    }
}

/// Pops `n` values, and returns them in stack order.
fn pop_values(stack: &mut Vec<Entry>, n: usize, at: usize) -> Result<Vec<Value>, Error> {
    let start = stack
        .len()
        .checked_sub(n)
        .ok_or_else(|| error(at, "the stack is too short"))?;
    stack
        .split_off(start)
        .into_iter()
        .map(|entry| match entry {
            Entry::Value(value) => Ok(value),
            Entry::Mark => Err(error(at, "a mark is in the way")),
        })
        .collect()
}

/// Pops the values down to the last mark, then the mark. Returns the
/// values in stack order.
fn pop_mark(stack: &mut Vec<Entry>, at: usize) -> Result<Vec<Value>, Error> {
    let mark = stack
        .iter()
        .rposition(|entry| matches!(entry, Entry::Mark))
        .ok_or_else(|| error(at, "no mark on the stack"))?;
    let values = pop_values(stack, stack.len() - mark - 1, at)?;
    stack.pop();
    Ok(values)
}

/// Reads the memo index of a put or a get from its argument.
fn memo_index(opcode: u8, arg: &[u8], at: usize) -> Result<u32, Error> {
    let index = match opcode {
        BINPUT | BINGET => arg.first().copied().map(u32::from),
        LONG_BINPUT | LONG_BINGET => arg.try_into().ok().map(u32::from_le_bytes),
        _ => arg
            .split_last()
            .and_then(|(_, digits)| std::str::from_utf8(digits).ok())
            .and_then(|digits| digits.parse().ok()),
    };
    index.ok_or_else(|| error(at, "a memo index does not read"))
}

fn error(offset: usize, message: &str) -> Error {
    Error::Decode {
        offset,
        message: message.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    /// The oids of the strong persistent ids, as integers.
    fn oids(record: &[u8]) -> Vec<u64> {
        persistent_ids(record)
            .unwrap()
            .into_iter()
            .filter_map(|id| match id {
                PersistentId::Oid(oid) => Some(u64::from_be_bytes(oid.try_into().unwrap())),
                _ => None,
            })
            .collect()
    }

    /// The records of the corpus that hold references, with the oids that
    /// `ZODB.serialize.referencesf` gives for them (ZODB 6.3, zodbpickle
    /// 4.5). It gives no oid for the other records.
    const CORPUS: &[(&str, &[u64])] = &[
        ("postings-3", &[15]),
        ("iiset-2", &[67]),
        ("iiset-3", &[66]),
        ("iiset-4", &[65]),
        ("iiset-5", &[64]),
        ("iiset-6", &[63]),
        ("iiset-7", &[62]),
        ("iiset-8", &[61]),
        ("iiset-9", &[60]),
        ("iiset-10", &[59]),
        ("iiset-11", &[58]),
        ("iiset-12", &[57]),
        ("iiset-13", &[56]),
        ("iiset-14", &[55]),
        ("iiset-15", &[54]),
        ("iiset-16", &[53]),
        ("iiset-17", &[52]),
        ("iiset-18", &[51]),
        (
            "iitreeset-2",
            &[
                50, 51, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 63, 64, 65, 66, 67, 50,
            ],
        ),
        ("oobtree-2", &[68, 69, 70, 71, 72, 73, 68]),
        ("oobucket-3", &[73]),
        ("oobucket-4", &[72]),
        ("oobucket-5", &[71]),
        ("oobucket-6", &[70]),
        ("oobucket-7", &[69]),
        (
            "persistent-mapping-5",
            &[
                1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23,
                24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44,
                45, 46, 47, 48, 49,
            ],
        ),
        ("persistent-reference-1", &[74]),
        ("persistent-reference-2", &[74, 74, 76, 77, 78]),
        ("persistent-reference-3", &[75]),
    ];

    #[test]
    fn the_corpus_gives_the_oids_of_referencesf() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus");
        let mut checked = 0;
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_none_or(|ext| ext != "record") {
                continue;
            }
            let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
            let expected = CORPUS
                .iter()
                .find(|(name, _)| *name == stem)
                .map_or(&[][..], |(_, oids)| oids);
            assert_eq!(oids(&std::fs::read(&path).unwrap()), expected, "{stem}");
            checked += 1;
        }
        assert_eq!(checked, 95);
    }

    #[test]
    fn a_weak_reference_in_the_corpus_is_weak() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/corpus/persistent-reference-4.record");
        let ids = persistent_ids(&std::fs::read(path).unwrap()).unwrap();
        let [PersistentId::Weak(oid)] = &ids[..] else {
            panic!("{ids:?}");
        };
        assert_eq!(oid, &74_u64.to_be_bytes());
    }

    /// Hand-built records with references in the places that the tree
    /// keeps opaque, and the oids that `referencesf` gives for them.
    const HAND_BUILT: &[(&str, &[u8], &[u64])] = &[
        (
            "the arguments of a REDUCE",
            b"\x80\x03cm\nC\nq\x00.\x80\x03}(X\x01\x00\x00\x00acmod\nfactory\n(C\x08\x00\x00\x00\x00\x00\x00\x00\x01h\x00\x86QtRu.",
            &[1],
        ),
        (
            "the state of an object that NEWOBJ and BUILD make",
            b"\x80\x03cm\nC\nq\x00.\x80\x03}(X\x01\x00\x00\x00bcmod\nInline\n)\x81}(X\x06\x00\x00\x00targetC\x08\x00\x00\x00\x00\x00\x00\x00\x02Qubu.",
            &[2],
        ),
        (
            "an oid that a put outside the opaque bytes stores",
            b"\x80\x03cm\nC\nq\x00.\x80\x03}(X\x01\x00\x00\x00cC\x08\x00\x00\x00\x00\x00\x00\x00\x03q\x05h\x00\x86QX\x01\x00\x00\x00dcmod\nf\n(h\x05h\x00\x86QtRu.",
            &[3, 3],
        ),
        (
            "an oid that a put inside the opaque bytes stores",
            b"\x80\x03cm\nC\nq\x00.\x80\x03}(X\x01\x00\x00\x00ecmod\nf\n(C\x08\x00\x00\x00\x00\x00\x00\x00\x04q\x06h\x00\x86QtRX\x01\x00\x00\x00fh\x06Qu.",
            &[4, 4],
        ),
        (
            "weak references, foreign references and another list",
            b"\x80\x03cm\nC\nq\x00.\x80\x03}(X\x01\x00\x00\x00w](X\x01\x00\x00\x00wC\x08\x00\x00\x00\x00\x00\x00\x00\x05\x85eQX\x01\x00\x00\x00v](X\x01\x00\x00\x00wC\x08\x00\x00\x00\x00\x00\x00\x00\x06X\x02\x00\x00\x00db\x86eQX\x01\x00\x00\x00m](X\x01\x00\x00\x00m(X\x02\x00\x00\x00dbC\x08\x00\x00\x00\x00\x00\x00\x00\x07cmod\nC\nteQX\x01\x00\x00\x00n](X\x01\x00\x00\x00nX\x02\x00\x00\x00dbC\x08\x00\x00\x00\x00\x00\x00\x00\x08\x86eQX\x01\x00\x00\x00o](X\x01\x00\x00\x00xeQu.",
            &[],
        ),
        (
            "a Python 2 string oid",
            b"cm\nC\nq\x01.}q\x02(U\x01gU\x08\x00\x00\x00\x00\x00\x00\x00\th\x01\x86Qu.",
            &[9],
        ),
        (
            "a persistent class in the class pickle",
            b"\x80\x03C\x08\x00\x00\x00\x00\x00\x00\x00\nQ)\x86q\x00.\x80\x03}(X\x01\x00\x00\x00hC\x08\x00\x00\x00\x00\x00\x00\x00\x0bQu.",
            &[10, 11],
        ),
        (
            "the state that BUILD gives to an object that REDUCE makes",
            b"\x80\x03cm\nC\nq\x00.\x80\x03}(X\x01\x00\x00\x00icmod\nf\n)R}(X\x01\x00\x00\x00tC\x08\x00\x00\x00\x00\x00\x00\x00\x0ch\x00\x86Qubu.",
            &[12],
        ),
        (
            "the arguments of OBJ",
            b"cm\nC\nq\x01.}(U\x01j(cmod\nOld\nU\x08\x00\x00\x00\x00\x00\x00\x00\rh\x01\x86Qou.",
            &[13],
        ),
        (
            "a list in the arguments of a REDUCE",
            b"\x80\x03cm\nC\nq\x00.\x80\x03}(X\x01\x00\x00\x00kcmod\nf\n](C\x08\x00\x00\x00\x00\x00\x00\x00\x0eh\x00\x86QC\x08\x00\x00\x00\x00\x00\x00\x00\x0fQe\x85Ru.",
            &[14, 15],
        ),
    ];

    #[test]
    fn hand_built_records_give_the_oids_of_referencesf() {
        for (name, record, expected) in HAND_BUILT {
            assert_eq!(oids(record), *expected, "{name}");
        }
    }

    #[test]
    fn the_opaque_references_are_not_in_the_tree() {
        // The tree of each hand-built record with an opaque reference has
        // fewer persistent nodes than the record has persistent ids.
        for (name, record, expected) in &HAND_BUILT[..4] {
            let tree = crate::decode_record(record).unwrap();
            let modeled = tree
                .class()
                .walk()
                .chain(tree.state().walk())
                .filter(|node| matches!(node, crate::Node::Persistent(_)))
                .count();
            assert!(modeled < expected.len(), "{name}");
        }
    }

    #[test]
    fn weak_and_foreign_references_keep_their_form() {
        let ids = persistent_ids(HAND_BUILT[4].1).unwrap();
        let oid = |n: u64| n.to_be_bytes().to_vec();
        assert_eq!(
            ids,
            [
                PersistentId::Weak(oid(5)),
                PersistentId::Weak(oid(6)),
                PersistentId::Foreign {
                    database: b"db".to_vec(),
                    oid: oid(7)
                },
                PersistentId::Foreign {
                    database: b"db".to_vec(),
                    oid: oid(8)
                },
                PersistentId::List,
            ]
        );
    }

    /// Returns a record whose state is a dict with one key, `x`, and the
    /// value `value`.
    fn holding(value: &[u8]) -> Vec<u8> {
        [
            &b"\x80\x03cm\nC\nq\x00.\x80\x03}(X\x01\x00\x00\x00x"[..],
            value,
            b"u.",
        ]
        .concat()
    }

    #[test]
    fn a_persistent_id_of_another_form_is_an_error() {
        // An int, an empty tuple, a tuple of an int and a text oid that is
        // not ASCII. `referencesf` fails on each of these too. The fifth is
        // an oid in protocol-0 text with an escape, which ZODB never
        // writes.
        for pid in [
            &b"K\x05Q"[..],
            b")Q",
            b"K\x05\x85Q",
            b"X\x02\x00\x00\x00\xc3\xa9Q",
            b"S'\\x01'\nQ",
        ] {
            let record = holding(pid);
            assert!(crate::decode_record(&record).is_ok(), "{pid:?}");
            let error = persistent_ids(&record);
            assert!(
                matches!(error, Err(Error::Decode { .. })),
                "{pid:?}: {error:?}"
            );
        }
    }

    #[test]
    fn a_text_oid_in_ascii_is_read_as_bytes() {
        // `referencesf` gives `b"abcdefgh"` for this record.
        let record = holding(b"X\x08\x00\x00\x00abcdefghQ");
        assert_eq!(
            persistent_ids(&record).unwrap(),
            [PersistentId::Oid(b"abcdefgh".to_vec())]
        );
    }

    #[test]
    fn a_record_that_does_not_decode_is_an_error() {
        // A reference, then an opcode that Python refuses.
        let mut record = holding(b"C\x08\x00\x00\x00\x00\x00\x00\x00\x01Q");
        record.insert(record.len() - 2, 0xff);
        assert!(matches!(persistent_ids(&record), Err(Error::Decode { .. })));
    }

    #[test]
    fn the_budget_of_a_large_record_refuses_nothing() {
        // A list of more nodes than the default budget, with one reference
        // at its end.
        let mut list = b"](".to_vec();
        list.extend(std::iter::repeat_n(b'N', crate::NODE_BUDGET + 1));
        list.extend_from_slice(b"C\x08\x00\x00\x00\x00\x00\x00\x00\x07Qe");
        let record = holding(&list);
        assert!(matches!(
            crate::decode_record(&record),
            Err(Error::Budget { .. })
        ));
        assert_eq!(oids(&record), [7]);
    }

    #[test]
    fn a_changed_record_never_panics() {
        // Each small record of the corpus and each hand-built record, with
        // one byte changed to an opcode that builds or takes a value, and
        // with BINPERSID put before each byte. The decoder refuses most of
        // these. The scan of the others must not panic.
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus");
        let mut records: Vec<Vec<u8>> = HAND_BUILT.iter().map(|(_, r, _)| r.to_vec()).collect();
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|ext| ext == "record") {
                let bytes = std::fs::read(&path).unwrap();
                if bytes.len() < 600 {
                    records.push(bytes);
                }
            }
        }
        let mut accepted = 0;
        for record in &records {
            for at in 0..record.len() {
                for opcode in *b"Q(htq]e)R\x81b" {
                    let mut changed = record.clone();
                    changed[at] = opcode;
                    accepted += usize::from(persistent_ids(&changed).is_ok());
                }
                let mut inserted = record.clone();
                inserted.insert(at, b'Q');
                accepted += usize::from(persistent_ids(&inserted).is_ok());
            }
        }
        assert!(accepted > 0);
    }

    #[test]
    fn a_deep_value_does_not_overflow_the_stack() {
        // A reference at the bottom of nested tuples inside the arguments
        // of a REDUCE, so the tree keeps it opaque.
        const DEPTH: usize = 200_000;
        let mut value = b"cmod\nf\n(".to_vec();
        value.extend(std::iter::repeat_n(b'(', DEPTH));
        value.extend_from_slice(b"C\x08\x00\x00\x00\x00\x00\x00\x00\x08Q");
        value.extend(std::iter::repeat_n(b't', DEPTH));
        value.extend_from_slice(b"tR");
        assert_eq!(oids(&holding(&value)), [8]);
    }
}
