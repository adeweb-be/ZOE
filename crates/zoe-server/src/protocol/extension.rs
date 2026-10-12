//! The extension of a transaction: a msgpack map on the wire, a pickle in
//! the store.
//!
//! `zoe.Storage` stores `extension_bytes` of ZODB: empty bytes for an
//! empty dict, else the pickle of the dict in protocol 3. The server
//! stores the same, so the store keeps one format in both modes. The
//! values map as ZEO's decoder gives them: a msgpack array is a tuple, a
//! str is a `str` and a bin is `bytes`.
//!
//! The transaction iterator sends the extension back as a msgpack map, as
//! a stock server does: a stock client keeps the value of the wire as the
//! extension, and does not unpickle bytes.

use std::collections::HashMap;

use rmpv::Value;

const PROTO: u8 = 0x80;
const STOP: u8 = b'.';
const MARK: u8 = b'(';
const NONE: u8 = b'N';
const NEWTRUE: u8 = 0x88;
const NEWFALSE: u8 = 0x89;
const INT: u8 = b'I';
const LONG: u8 = b'L';
const BININT1: u8 = b'K';
const BININT2: u8 = b'M';
const BININT: u8 = b'J';
const LONG1: u8 = 0x8a;
const LONG4: u8 = 0x8b;
const BINFLOAT: u8 = b'G';
const SHORT_BINSTRING: u8 = b'U';
const BINSTRING: u8 = b'T';
const BINUNICODE: u8 = b'X';
const SHORT_BINBYTES: u8 = b'C';
const BINBYTES: u8 = b'B';
const EMPTY_TUPLE: u8 = b')';
const TUPLE: u8 = b't';
const TUPLE1: u8 = 0x85;
const TUPLE2: u8 = 0x86;
const TUPLE3: u8 = 0x87;
const EMPTY_LIST: u8 = b']';
const LIST: u8 = b'l';
const APPEND: u8 = b'a';
const APPENDS: u8 = b'e';
const EMPTY_DICT: u8 = b'}';
const DICT: u8 = b'd';
const SETITEM: u8 = b's';
const SETITEMS: u8 = b'u';
const BINPUT: u8 = b'q';
const LONG_BINPUT: u8 = b'r';
const BINGET: u8 = b'h';
const LONG_BINGET: u8 = b'j';

/// The most levels of tuples, lists and dicts in an extension that
/// `unpickle` reads. A frame of the protocol holds about 60 levels, so an
/// extension that a client stored has fewer.
const MAX_DEPTH: usize = 100;

/// Return the extension bytes of the msgpack value `ext`. Nil and an empty
/// map give empty bytes. Another map gives its pickle in protocol 3.
/// Return a text that says why `ext` cannot be stored.
pub(crate) fn pickle(ext: &Value) -> Result<Vec<u8>, String> {
    match ext {
        Value::Nil => Ok(Vec::new()),
        Value::Map(items) if items.is_empty() => Ok(Vec::new()),
        Value::Map(_) => {
            let mut out = vec![PROTO, 3];
            value(&mut out, ext)?;
            out.push(STOP);
            Ok(out)
        }
        _ => Err("the extension is not a map".into()),
    }
}

/// Write the pickle opcodes of `root`. An explicit stack keeps deep values
/// off the call stack.
fn value(out: &mut Vec<u8>, root: &Value) -> Result<(), String> {
    enum Step<'a> {
        Value(&'a Value),
        Byte(u8),
    }
    let mut stack = vec![Step::Value(root)];
    while let Some(step) = stack.pop() {
        let value = match step {
            Step::Byte(byte) => {
                out.push(byte);
                continue;
            }
            Step::Value(value) => value,
        };
        match value {
            Value::Nil => out.push(NONE),
            Value::Boolean(true) => out.push(NEWTRUE),
            Value::Boolean(false) => out.push(NEWFALSE),
            Value::Integer(n) => match (n.as_i64(), n.as_u64()) {
                (Some(n), _) => int(out, i128::from(n)),
                (None, Some(n)) => int(out, i128::from(n)),
                (None, None) => return Err("an integer is out of range".into()),
            },
            Value::F32(f) => float(out, f64::from(*f)),
            Value::F64(f) => float(out, *f),
            Value::String(text) => {
                let bytes = text
                    .as_str()
                    .ok_or("a string of the extension is not UTF-8")?
                    .as_bytes();
                sized(out, BINUNICODE, bytes)?;
            }
            Value::Binary(bytes) => match u8::try_from(bytes.len()) {
                Ok(len) => {
                    out.extend([SHORT_BINBYTES, len]);
                    out.extend_from_slice(bytes);
                }
                Err(_) => sized(out, BINBYTES, bytes)?,
            },
            Value::Array(items) if items.is_empty() => out.push(EMPTY_TUPLE),
            Value::Array(items) => {
                out.push(MARK);
                stack.push(Step::Byte(TUPLE));
                stack.extend(items.iter().rev().map(Step::Value));
            }
            Value::Map(items) => {
                out.push(EMPTY_DICT);
                if !items.is_empty() {
                    out.push(MARK);
                    stack.push(Step::Byte(SETITEMS));
                    for (key, value) in items.iter().rev() {
                        stack.push(Step::Value(value));
                        stack.push(Step::Value(key));
                    }
                }
            }
            Value::Ext(..) => return Err("the extension holds a msgpack ext value".into()),
        }
    }
    Ok(())
}

fn int(out: &mut Vec<u8>, n: i128) {
    if let Ok(n) = u8::try_from(n) {
        out.extend([BININT1, n]);
    } else if let Ok(n) = u16::try_from(n) {
        out.push(BININT2);
        out.extend(n.to_le_bytes());
    } else if let Ok(n) = i32::try_from(n) {
        out.push(BININT);
        out.extend(n.to_le_bytes());
    } else {
        // LONG1: the shortest two's complement, little-endian.
        let bytes = n.to_le_bytes();
        let mut len = bytes.len();
        while len > 1 {
            let last = bytes[len - 1];
            let sign = bytes[len - 2] & 0x80;
            if (last == 0 && sign == 0) || (last == 0xff && sign != 0) {
                len -= 1;
            } else {
                break;
            }
        }
        out.extend([LONG1, len as u8]);
        out.extend_from_slice(&bytes[..len]);
    }
}

fn float(out: &mut Vec<u8>, f: f64) {
    out.push(BINFLOAT);
    out.extend(f.to_be_bytes());
}

fn sized(out: &mut Vec<u8>, opcode: u8, bytes: &[u8]) -> Result<(), String> {
    let len = u32::try_from(bytes.len()).map_err(|_| "a value of the extension is too long")?;
    out.push(opcode);
    out.extend(len.to_le_bytes());
    out.extend_from_slice(bytes);
    Ok(())
}

/// Return the msgpack map of the extension bytes `bytes`. Empty bytes give
/// an empty map. Return a text that says why the bytes do not give a map
/// that a stock client takes.
///
/// The bytes are a pickle of a dict, in protocol 3 or lower, as ZODB's
/// `loads` reads it. The values map as a stock server sends them: `None`,
/// a bool, an int of 64 bits, a float, a `str`, `bytes`, a tuple, a list
/// and a dict. A string of protocol 2 or lower is a `str` when it is
/// ASCII, else `bytes`, as ZODB reads it. A key of a dict is a `str` or
/// `bytes`, because the msgpack decoder of a stock client refuses other
/// keys. Any other value, such as an instance of a class, fails.
///
/// A get from the memo copies its value, so the memo cannot share a list
/// or a dict: a get of one fails. The decoded strings and bytes are at most
/// 64 times the length of the pickle, plus 64 KiB, so a small pickle with
/// many gets cannot grow without a bound.
pub(crate) fn unpickle(bytes: &[u8]) -> Result<Value, String> {
    if bytes.is_empty() {
        return Ok(Value::Map(Vec::new()));
    }
    let budget = bytes.len().saturating_mul(64).saturating_add(1 << 16);
    let mut machine = Machine {
        input: bytes,
        stack: Vec::new(),
        memo: HashMap::new(),
        budget,
    };
    match machine.run()? {
        Slot::Dict(items, _) => Ok(Value::Map(items)),
        _ => Err("the extension is not a dict".into()),
    }
}

/// One entry of the stack of `unpickle`. A container keeps its depth: the
/// levels of containers in it, itself included.
enum Slot {
    Mark,
    /// A value that does not change: a leaf or a tuple.
    Value(Value, usize),
    List(Vec<Value>, usize),
    Dict(Vec<(Value, Value)>, usize),
}

impl Slot {
    /// Return the value and its depth. A mark is not a value.
    fn into_value(self) -> Result<(Value, usize), String> {
        match self {
            Self::Mark => Err("a mark is not a value".into()),
            Self::Value(value, depth) => Ok((value, depth)),
            Self::List(items, depth) => Ok((Value::Array(items), depth)),
            Self::Dict(items, depth) => Ok((Value::Map(items), depth)),
        }
    }
}

/// The state of `unpickle`.
struct Machine<'a> {
    /// The bytes that are not read yet.
    input: &'a [u8],
    stack: Vec<Slot>,
    /// A copy of each value that the memo holds, with its depth. `None`
    /// for a list or a dict.
    memo: HashMap<u32, Option<(Value, usize)>>,
    /// The bytes of strings that a get from the memo can still copy.
    budget: usize,
}

impl<'a> Machine<'a> {
    /// Run the opcodes up to `STOP`. Return the value on top of the stack.
    fn run(&mut self) -> Result<Slot, String> {
        loop {
            let op = self.take(1)?[0];
            match op {
                STOP => break,
                PROTO => {
                    let version = self.take(1)?[0];
                    if version > 3 {
                        return Err(format!("the pickle protocol {version} is not supported"));
                    }
                }
                MARK => self.stack.push(Slot::Mark),
                NONE => self.leaf(Value::Nil),
                NEWTRUE => self.leaf(Value::Boolean(true)),
                NEWFALSE => self.leaf(Value::Boolean(false)),
                INT => {
                    let line = self.line()?;
                    let value = match line {
                        b"00" => Value::Boolean(false),
                        b"01" => Value::Boolean(true),
                        _ => int_value(decimal(line)?)?,
                    };
                    self.leaf(value);
                }
                LONG => {
                    let line = self.line()?;
                    let digits = line.strip_suffix(b"L").unwrap_or(line);
                    let value = int_value(decimal(digits)?)?;
                    self.leaf(value);
                }
                BININT1 => {
                    let n = self.take(1)?[0];
                    self.leaf(Value::from(n));
                }
                BININT2 => {
                    let n = u16::from_le_bytes(self.array()?);
                    self.leaf(Value::from(n));
                }
                BININT => {
                    let n = i32::from_le_bytes(self.array()?);
                    self.leaf(Value::from(n));
                }
                LONG1 => {
                    let len = usize::from(self.take(1)?[0]);
                    let value = int_value(signed(self.take(len)?)?)?;
                    self.leaf(value);
                }
                LONG4 => {
                    let len = self.len4()?;
                    let value = int_value(signed(self.take(len)?)?)?;
                    self.leaf(value);
                }
                BINFLOAT => {
                    let f = f64::from_be_bytes(self.array()?);
                    self.leaf(Value::F64(f));
                }
                SHORT_BINSTRING | BINSTRING => {
                    let len = match op {
                        SHORT_BINSTRING => usize::from(self.take(1)?[0]),
                        _ => self.len4()?,
                    };
                    let bytes = self.take(len)?;
                    let value = match std::str::from_utf8(bytes) {
                        Ok(text) if bytes.is_ascii() => Value::from(text),
                        _ => Value::Binary(bytes.to_vec()),
                    };
                    self.leaf(value);
                }
                BINUNICODE => {
                    let len = self.len4()?;
                    let text = std::str::from_utf8(self.take(len)?)
                        .map_err(|_| "a str of the extension is not UTF-8")?;
                    self.leaf(Value::from(text));
                }
                SHORT_BINBYTES | BINBYTES => {
                    let len = match op {
                        SHORT_BINBYTES => usize::from(self.take(1)?[0]),
                        _ => self.len4()?,
                    };
                    let bytes = self.take(len)?.to_vec();
                    self.leaf(Value::Binary(bytes));
                }
                EMPTY_TUPLE => self.stack.push(Slot::Value(Value::Array(Vec::new()), 1)),
                TUPLE1 | TUPLE2 | TUPLE3 => {
                    let n = usize::from(op - TUPLE1 + 1);
                    let at = self
                        .stack
                        .len()
                        .checked_sub(n)
                        .ok_or("a tuple has too few items")?;
                    let items = self.stack.split_off(at);
                    self.tuple(items)?;
                }
                TUPLE => {
                    let items = self.pop_mark()?;
                    self.tuple(items)?;
                }
                EMPTY_LIST => self.stack.push(Slot::List(Vec::new(), 1)),
                LIST => {
                    let (items, depth) = values(self.pop_mark()?)?;
                    self.stack.push(Slot::List(items, nested(depth)?));
                }
                APPEND => {
                    let item = self.pop()?;
                    self.append(vec![item])?;
                }
                APPENDS => {
                    let items = self.pop_mark()?;
                    self.append(items)?;
                }
                EMPTY_DICT => self.stack.push(Slot::Dict(Vec::new(), 1)),
                DICT => {
                    let items = self.pop_mark()?;
                    self.stack.push(Slot::Dict(Vec::new(), 1));
                    self.set_items(items)?;
                }
                SETITEM => {
                    let value = self.pop()?;
                    let key = self.pop()?;
                    self.set_items(vec![key, value])?;
                }
                SETITEMS => {
                    let items = self.pop_mark()?;
                    self.set_items(items)?;
                }
                BINPUT => {
                    let index = u32::from(self.take(1)?[0]);
                    self.put(index)?;
                }
                LONG_BINPUT => {
                    let index = u32::from_le_bytes(self.array()?);
                    self.put(index)?;
                }
                BINGET => {
                    let index = u32::from(self.take(1)?[0]);
                    self.get(index)?;
                }
                LONG_BINGET => {
                    let index = u32::from_le_bytes(self.array()?);
                    self.get(index)?;
                }
                _ => {
                    return Err(format!(
                        "the extension holds the pickle opcode {op:#04x}, which gives no msgpack value"
                    ));
                }
            }
        }
        let top = self.pop()?;
        if !self.stack.is_empty() {
            return Err("the pickle leaves items on the stack".into());
        }
        Ok(top)
    }

    /// Take the next `len` bytes.
    fn take(&mut self, len: usize) -> Result<&'a [u8], String> {
        let input: &'a [u8] = self.input;
        if input.len() < len {
            return Err("the pickle ends early".into());
        }
        let (head, rest) = input.split_at(len);
        self.input = rest;
        Ok(head)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], String> {
        Ok(self.take(N)?.try_into().expect("N bytes"))
    }

    /// Take a 4-byte little-endian length.
    fn len4(&mut self) -> Result<usize, String> {
        let len = u32::from_le_bytes(self.array()?);
        usize::try_from(len).map_err(|_| "a length of the pickle is too large".into())
    }

    /// Take the bytes up to the next newline. Return them without the
    /// newline.
    fn line(&mut self) -> Result<&'a [u8], String> {
        let end = self
            .input
            .iter()
            .position(|&byte| byte == b'\n')
            .ok_or("the pickle ends early")?;
        Ok(&self.take(end + 1)?[..end])
    }

    fn leaf(&mut self, value: Value) {
        self.stack.push(Slot::Value(value, 0));
    }

    fn pop(&mut self) -> Result<Slot, String> {
        self.stack.pop().ok_or_else(|| "the stack is empty".into())
    }

    /// Pop the items above the top mark, and the mark.
    fn pop_mark(&mut self) -> Result<Vec<Slot>, String> {
        let at = self
            .stack
            .iter()
            .rposition(|slot| matches!(slot, Slot::Mark))
            .ok_or("no mark on the stack")?;
        let items = self.stack.split_off(at + 1);
        self.stack.pop();
        Ok(items)
    }

    fn tuple(&mut self, items: Vec<Slot>) -> Result<(), String> {
        let (items, depth) = values(items)?;
        self.stack
            .push(Slot::Value(Value::Array(items), nested(depth)?));
        Ok(())
    }

    /// Append `items` to the list on top of the stack.
    fn append(&mut self, items: Vec<Slot>) -> Result<(), String> {
        let (items, depth) = values(items)?;
        let Some(Slot::List(list, own)) = self.stack.last_mut() else {
            return Err("an append does not follow a list".into());
        };
        *own = (*own).max(nested(depth)?);
        list.extend(items);
        Ok(())
    }

    /// Set the keys and values of `items`, in pairs, in the dict on top of
    /// the stack.
    fn set_items(&mut self, items: Vec<Slot>) -> Result<(), String> {
        if !items.len().is_multiple_of(2) {
            return Err("a key of a dict has no value".into());
        }
        let (items, depth) = values(items)?;
        let Some(Slot::Dict(dict, own)) = self.stack.last_mut() else {
            return Err("a set item does not follow a dict".into());
        };
        *own = (*own).max(nested(depth)?);
        let mut items = items.into_iter();
        while let (Some(key), Some(value)) = (items.next(), items.next()) {
            if !matches!(key, Value::String(_) | Value::Binary(_)) {
                return Err("a key of the extension is not a str or bytes".into());
            }
            dict.push((key, value));
        }
        Ok(())
    }

    /// Keep a copy of the top of the stack in the memo.
    fn put(&mut self, index: u32) -> Result<(), String> {
        let entry = match self.stack.last() {
            Some(Slot::Value(value, depth)) => Some((value.clone(), *depth)),
            Some(Slot::List(..) | Slot::Dict(..)) => None,
            Some(Slot::Mark) | None => return Err("a put has no value".into()),
        };
        self.memo.insert(index, entry);
        Ok(())
    }

    /// Push a copy of the value of the memo at `index`.
    fn get(&mut self, index: u32) -> Result<(), String> {
        let Some(entry) = self.memo.get(&index) else {
            return Err("a get names no value of the memo".into());
        };
        let Some((value, depth)) = entry else {
            return Err("the extension shares a list or a dict".into());
        };
        let size = weight(value);
        self.budget = self
            .budget
            .checked_sub(size)
            .ok_or("the extension grows too much")?;
        self.stack.push(Slot::Value(value.clone(), *depth));
        Ok(())
    }
}

/// Return the values of `slots` and the depth of the deepest one.
fn values(slots: Vec<Slot>) -> Result<(Vec<Value>, usize), String> {
    let mut depth = 0;
    let mut items = Vec::with_capacity(slots.len());
    for slot in slots {
        let (value, levels) = slot.into_value()?;
        depth = depth.max(levels);
        items.push(value);
    }
    Ok((items, depth))
}

/// Return the depth of a container whose deepest item has the depth
/// `depth`.
fn nested(depth: usize) -> Result<usize, String> {
    let depth = depth + 1;
    if depth > MAX_DEPTH {
        return Err(format!("the extension nests more than {MAX_DEPTH} levels"));
    }
    Ok(depth)
}

/// The size of `value` for the budget: the length of each string and each
/// bytes, and 1 for each other value.
fn weight(value: &Value) -> usize {
    match value {
        Value::String(text) => text.as_bytes().len().max(1),
        Value::Binary(bytes) => bytes.len().max(1),
        Value::Array(items) => items.iter().map(weight).sum::<usize>() + 1,
        Value::Map(items) => {
            items
                .iter()
                .map(|(key, value)| weight(key) + weight(value))
                .sum::<usize>()
                + 1
        }
        _ => 1,
    }
}

/// The int of the decimal text `digits`, with an optional sign.
fn decimal(digits: &[u8]) -> Result<i128, String> {
    std::str::from_utf8(digits)
        .ok()
        .and_then(|text| text.trim().parse::<i128>().ok())
        .ok_or_else(|| "an int of the pickle is not a decimal of 128 bits".into())
}

/// The int of the little-endian two's complement `bytes`.
fn signed(bytes: &[u8]) -> Result<i128, String> {
    if bytes.is_empty() {
        return Ok(0);
    }
    if bytes.len() > 16 {
        return Err("an int of the extension does not fit in 64 bits".into());
    }
    let fill = if bytes[bytes.len() - 1] & 0x80 != 0 {
        0xff
    } else {
        0
    };
    let mut full = [fill; 16];
    full[..bytes.len()].copy_from_slice(bytes);
    Ok(i128::from_le_bytes(full))
}

/// The msgpack int of `n`. msgpack has ints of 64 bits, signed or not.
fn int_value(n: i128) -> Result<Value, String> {
    if let Ok(n) = i64::try_from(n) {
        Ok(Value::from(n))
    } else if let Ok(n) = u64::try_from(n) {
        Ok(Value::from(n))
    } else {
        Err("an int of the extension does not fit in 64 bits".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(items: Vec<(Value, Value)>) -> Value {
        Value::Map(items)
    }

    #[test]
    fn an_empty_extension_is_empty_bytes() {
        assert_eq!(pickle(&Value::Nil).unwrap(), b"");
        assert_eq!(pickle(&map(vec![])).unwrap(), b"");
        assert!(pickle(&Value::from(1)).is_err());
    }

    #[test]
    fn a_map_is_the_pickle_of_a_dict_in_protocol_3() {
        // pickle.loads gives {'user_id': 'x', 'n': 1}.
        let ext = map(vec![
            (Value::from("user_id"), Value::from("x")),
            (Value::from("n"), Value::from(1)),
        ]);
        assert_eq!(
            pickle(&ext).unwrap(),
            b"\x80\x03}(X\x07\x00\x00\x00user_idX\x01\x00\x00\x00xX\x01\x00\x00\x00nK\x01u."
        );
    }

    #[test]
    fn values_map_as_zeo_decodes_them() {
        let ext = map(vec![(
            Value::from("v"),
            Value::Array(vec![
                Value::Nil,
                Value::Boolean(true),
                Value::from(300),
                Value::from(-1),
                Value::from(1u64 << 40),
                Value::Binary(b"ab".to_vec()),
                Value::F64(0.5),
                Value::Array(vec![]),
            ]),
        )]);
        let mut expected = b"\x80\x03}(X\x01\x00\x00\x00v(N\x88M\x2c\x01J\xff\xff\xff\xff".to_vec();
        expected.extend(b"\x8a\x06\x00\x00\x00\x00\x00\x01");
        expected.extend(b"C\x02abG\x3f\xe0\x00\x00\x00\x00\x00\x00)tu.");
        assert_eq!(pickle(&ext).unwrap(), expected);
    }

    #[test]
    fn long_ints_take_the_shortest_form() {
        let mut out = Vec::new();
        int(&mut out, i128::from(u64::MAX));
        assert_eq!(out, b"\x8a\x09\xff\xff\xff\xff\xff\xff\xff\xff\x00");
        out.clear();
        int(&mut out, -(1i128 << 40));
        assert_eq!(out, b"\x8a\x06\x00\x00\x00\x00\x00\xff");
        out.clear();
        int(&mut out, i128::from(i64::MIN));
        assert_eq!(out, b"\x8a\x08\x00\x00\x00\x00\x00\x00\x00\x80");
    }

    #[test]
    fn an_ext_value_is_refused() {
        let ext = map(vec![(Value::from("v"), Value::Ext(1, vec![0]))]);
        assert!(pickle(&ext).is_err());
    }

    fn text(s: &str) -> Value {
        Value::from(s)
    }

    #[test]
    fn unpickle_reads_back_what_pickle_writes() {
        assert_eq!(unpickle(b"").unwrap(), map(vec![]));
        let ext = map(vec![
            (text("user_id"), text("x")),
            (
                text("v"),
                Value::Array(vec![
                    Value::Nil,
                    Value::Boolean(false),
                    Value::from(300),
                    Value::from(-1),
                    Value::from(i64::MIN),
                    Value::from(u64::MAX),
                    Value::Binary(vec![7; 300]),
                    Value::F64(0.5),
                    Value::Array(vec![]),
                    map(vec![(Value::Binary(b"k".to_vec()), text("\u{e9}"))]),
                ]),
            ),
        ]);
        assert_eq!(unpickle(&pickle(&ext).unwrap()).unwrap(), ext);
    }

    #[test]
    fn unpickle_reads_the_extension_bytes_of_zodb() {
        // `TransactionMetaData(extension=...).extension_bytes` of ZODB 6.
        // The pickle shares the str "a" and the key "user_id" by a get.
        let bytes = b"\x80\x03}q\x00(X\x07\x00\x00\x00user_idq\x01X\x05\x00\x00\x00adminq\x02\
            X\x01\x00\x00\x00nq\x03M,\x01X\x03\x00\x00\x00negq\x04J\xfe\xff\xff\xff\
            X\x03\x00\x00\x00bigq\x05\x8a\x06\x00\x00\x00\x00\x00\x01\
            X\x01\x00\x00\x00fq\x06G?\xe0\x00\x00\x00\x00\x00\x00X\x04\x00\x00\x00noneq\x07N\
            X\x03\x00\x00\x00yesq\x08\x88X\x02\x00\x00\x00noq\t\x89\
            X\x03\x00\x00\x00rawq\nC\x02\x00\xffq\x0bX\x01\x00\x00\x00tq\x0c\
            X\x01\x00\x00\x00aq\rh\r\x86q\x0eX\x01\x00\x00\x00lq\x0f]q\x10(K\x01\
            X\x01\x00\x00\x00xq\x11eX\x01\x00\x00\x00dq\x12}q\x13h\x01\
            X\x01\x00\x00\x00uq\x14su.";
        let expected = map(vec![
            (text("user_id"), text("admin")),
            (text("n"), Value::from(300)),
            (text("neg"), Value::from(-2)),
            (text("big"), Value::from(1u64 << 40)),
            (text("f"), Value::F64(0.5)),
            (text("none"), Value::Nil),
            (text("yes"), Value::Boolean(true)),
            (text("no"), Value::Boolean(false)),
            (text("raw"), Value::Binary(vec![0, 0xff])),
            (text("t"), Value::Array(vec![text("a"), text("a")])),
            (text("l"), Value::Array(vec![Value::from(1), text("x")])),
            (text("d"), map(vec![(text("user_id"), text("u"))])),
        ]);
        assert_eq!(unpickle(bytes).unwrap(), expected);
    }

    #[test]
    fn unpickle_reads_the_old_protocols_as_zodb_does() {
        // Protocol 1 of Python: a bool and a long as text lines.
        let bytes = b"}q\x00(X\x04\x00\x00\x00flagq\x01I01\n\
            X\x04\x00\x00\x00longq\x02L1099511627776L\nX\x01\x00\x00\x00kq\x03\
            X\x01\x00\x00\x00vq\x04u.";
        let expected = map(vec![
            (text("flag"), Value::Boolean(true)),
            (text("long"), Value::from(1u64 << 40)),
            (text("k"), text("v")),
        ]);
        assert_eq!(unpickle(bytes).unwrap(), expected);
        // A string of Python 2 is a str when it is ASCII, else bytes, as
        // `loads` of ZODB gives it.
        let bytes = b"}q\x01(U\x04userq\x02U\x05adminq\x03U\x03b\xc3\xa9q\x04\
            T\x01\x00\x00\x00xq\x05u.";
        let expected = map(vec![
            (text("user"), text("admin")),
            (Value::Binary(b"b\xc3\xa9".to_vec()), text("x")),
        ]);
        assert_eq!(unpickle(bytes).unwrap(), expected);
        // A list and a dict of the items after a mark.
        let bytes = b"(X\x01\x00\x00\x00k(K\x01K\x02ld.";
        let expected = map(vec![(
            text("k"),
            Value::Array(vec![Value::from(1), Value::from(2)]),
        )]);
        assert_eq!(unpickle(bytes).unwrap(), expected);
    }

    #[test]
    fn unpickle_refuses_what_a_stock_client_cannot_take() {
        for (bytes, why) in [
            (&b"\x80\x03N."[..], "not a dict"),
            (b"\x80\x04}.", "protocol 4"),
            (b"\x80\x03}K\x01K\x02s.", "an int key"),
            (
                b"\x80\x03}X\x01\x00\x00\x00k\x8a\x09\x00\x00\x00\x00\x00\x00\x00\x00\x01s.",
                "an int of 65 bits",
            ),
            (
                b"\x80\x03}X\x01\x00\x00\x00kcdatetime\ndate\ns.",
                "a global",
            ),
            // A list that the memo shares: `l = [1]; {"a": l, "b": l}`.
            (
                b"\x80\x03}q\x00(X\x01\x00\x00\x00aq\x01]q\x02K\x01aX\x01\x00\x00\x00bq\x03h\x02u.",
                "a shared list",
            ),
            (
                b"\x80\x03}X\x01\x00\x00\x00kX\x05\x00\x00\x00",
                "an end in a str",
            ),
            (
                b"\x80\x03}X\x02\x00\x00\x00\xff\xfeK\x01s.",
                "a str not in UTF-8",
            ),
            (b"(X\x01\x00\x00\x00k(K\x01K\x02l(K\x03td.", "an odd dict"),
            (b"\x80\x03}h\x05.", "a get of nothing"),
            (b"\x80\x03}}.", "two values left"),
        ] {
            assert!(unpickle(bytes).is_err(), "{why}");
        }
    }

    #[test]
    fn unpickle_bounds_the_depth_and_the_growth() {
        // A key, then a tuple of one item in a tuple of one item, and so
        // on. The dict is a level too.
        let nest = |levels: usize| {
            let mut bytes = b"\x80\x03}X\x01\x00\x00\x00k)".to_vec();
            bytes.extend(std::iter::repeat_n(0x85, levels - 1));
            bytes.extend(b"s.");
            bytes
        };
        assert!(unpickle(&nest(MAX_DEPTH - 1)).is_ok());
        assert!(unpickle(&nest(MAX_DEPTH)).is_err());
        assert!(unpickle(&nest(1_000_000)).is_err());
        // A str of 64 KiB, then many gets of it in a list.
        let mut bytes = b"\x80\x03}X\x01\x00\x00\x00k](X\x00\x00\x01\x00".to_vec();
        bytes.extend(vec![b'x'; 1 << 16]);
        bytes.extend(b"q\x00");
        let gets = |n: usize| std::iter::repeat_n(*b"h\x00", n).flatten();
        let mut few = bytes.clone();
        few.extend(gets(63));
        few.extend(b"es.");
        let Value::Map(items) = unpickle(&few).unwrap() else {
            panic!("not a map");
        };
        assert_eq!(items[0].1.as_array().unwrap().len(), 64);
        bytes.extend(gets(1_000));
        bytes.extend(b"es.");
        assert!(unpickle(&bytes).is_err());
    }
}
