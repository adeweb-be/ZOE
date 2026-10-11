//! The extension of a transaction: a msgpack map on the wire, a pickle in
//! the store.
//!
//! `zoe.Storage` stores `extension_bytes` of ZODB: empty bytes for an
//! empty dict, else the pickle of the dict in protocol 3. The server
//! stores the same, so the store keeps one format in both modes. The
//! values map as ZEO's decoder gives them: a msgpack array is a tuple, a
//! str is a `str` and a bin is `bytes`.

use rmpv::Value;

const PROTO: u8 = 0x80;
const STOP: u8 = b'.';
const MARK: u8 = b'(';
const NONE: u8 = b'N';
const NEWTRUE: u8 = 0x88;
const NEWFALSE: u8 = 0x89;
const BININT1: u8 = b'K';
const BININT2: u8 = b'M';
const BININT: u8 = b'J';
const LONG1: u8 = 0x8a;
const BINFLOAT: u8 = b'G';
const BINUNICODE: u8 = b'X';
const SHORT_BINBYTES: u8 = b'C';
const BINBYTES: u8 = b'B';
const EMPTY_TUPLE: u8 = b')';
const TUPLE: u8 = b't';
const EMPTY_DICT: u8 = b'}';
const SETITEMS: u8 = b'u';

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
}
