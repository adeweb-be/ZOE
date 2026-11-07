//! The cells of the metadata table: a value of a record of `data` as
//! msgpack (ADR 0059, section 8).
//!
//! A cell is nil, a bool, an int of 64 bits, a float, a str, a bin, an array
//! for a tuple, a map for a dict, or one of three ext types:
//!
//! - ext 1, `Missing.Value`: no data;
//! - ext 2, a `DateTime`: `_micros` as 8 bytes of a big-endian int64, one
//!   byte for `_timezone_naive` (0 or 1), then the UTF-8 of the name of
//!   `_tz`;
//! - ext 3, a `Message`: the msgpack array of the arguments of its
//!   `__reduce__`.
//!
//! A value of another type (a list, a set, an instance of another class, a
//! persistent reference, a class, an int beyond 64 bits) has no cell.

use zoe_pickle::python::{Heap, Made, Object, Value};

use crate::plan::Wire;

/// The ext type of `Missing.Value`.
pub const EXT_MISSING: i8 = 1;
/// The ext type of a `DateTime`.
pub const EXT_DATE_TIME: i8 = 2;
/// The ext type of a `Message`.
pub const EXT_MESSAGE: i8 = 3;

/// The deepest value that a cell holds.
const DEPTH: usize = 32;

/// The cell of `value` of `heap`, or `None` for a value of another type.
pub fn encode(heap: &Heap, value: Value) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    write(heap, value, 0, &mut out).then_some(out)
}

fn write(heap: &Heap, value: Value, depth: usize, out: &mut Vec<u8>) -> bool {
    if depth > DEPTH {
        return false;
    }
    match value {
        Value::None => out.push(0xc0),
        Value::Bool(b) => out.push(if b { 0xc3 } else { 0xc2 }),
        Value::Int(i) => match i64::try_from(i) {
            Ok(i) => int(i, out),
            Err(_) => return false,
        },
        Value::Float(f) => {
            out.push(0xcb);
            out.extend(f.to_be_bytes());
        }
        Value::Object(id) => match heap.get(id) {
            Object::Str(s) => text(s, out),
            Object::Bytes(b) => bin(b, out),
            Object::Tuple(items) => {
                array_head(items.len(), out);
                for item in items {
                    if !write(heap, *item, depth + 1, out) {
                        return false;
                    }
                }
            }
            Object::Dict(items) => {
                map_head(items.len(), out);
                for (k, v) in items {
                    if !write(heap, *k, depth + 1, out) || !write(heap, *v, depth + 1, out) {
                        return false;
                    }
                }
            }
            Object::Class { module, name } if module == "Missing" && name == "V" => {
                ext(EXT_MISSING, &[], out);
            }
            Object::Instance(instance) => match (heap.class(instance.class), instance.made) {
                (Some(("DateTime.DateTime", "DateTime")), Made::New) => {
                    if heap.tuple(instance.args).is_none_or(|a| !a.is_empty()) {
                        return false;
                    }
                    let Some(state) = instance.state.and_then(|s| heap.tuple(s)) else {
                        return false;
                    };
                    let [Value::Int(micros), Value::Bool(naive), tz] = *state else {
                        return false;
                    };
                    let (Ok(micros), Some(tz)) = (i64::try_from(micros), heap.str(tz)) else {
                        return false;
                    };
                    let mut data = Vec::with_capacity(9 + tz.len());
                    data.extend(micros.to_be_bytes());
                    data.push(u8::from(naive));
                    data.extend(tz.as_bytes());
                    ext(EXT_DATE_TIME, &data, out);
                }
                (Some(("zope.i18nmessageid.message", "Message")), Made::Reduce) => {
                    if instance.state.is_some() {
                        return false;
                    }
                    let Some(args) = heap.tuple(instance.args) else {
                        return false;
                    };
                    if !matches!(args.first().map(|a| heap.str(*a)), Some(Some(_))) {
                        return false;
                    }
                    let mut data = Vec::new();
                    array_head(args.len(), &mut data);
                    for arg in args {
                        if !write(heap, *arg, depth + 1, &mut data) {
                            return false;
                        }
                    }
                    ext(EXT_MESSAGE, &data, out);
                }
                _ => return false,
            },
            Object::List(_) | Object::Class { .. } | Object::Reference(_) | Object::Set { .. } => {
                return false;
            }
        },
    }
    true
}

fn int(i: i64, out: &mut Vec<u8>) {
    if (0..128).contains(&i) {
        out.push(i as u8);
    } else if (-32..0).contains(&i) {
        out.push(i as i8 as u8);
    } else if let Ok(v) = i8::try_from(i) {
        out.extend([0xd0, v as u8]);
    } else if let Ok(v) = i16::try_from(i) {
        out.push(0xd1);
        out.extend(v.to_be_bytes());
    } else if let Ok(v) = i32::try_from(i) {
        out.push(0xd2);
        out.extend(v.to_be_bytes());
    } else {
        out.push(0xd3);
        out.extend(i.to_be_bytes());
    }
}

fn text(s: &str, out: &mut Vec<u8>) {
    let n = s.len();
    if n < 32 {
        out.push(0xa0 | n as u8);
    } else if n < 256 {
        out.extend([0xd9, n as u8]);
    } else if n < 65536 {
        out.push(0xda);
        out.extend((n as u16).to_be_bytes());
    } else {
        out.push(0xdb);
        out.extend((n as u32).to_be_bytes());
    }
    out.extend(s.as_bytes());
}

fn bin(b: &[u8], out: &mut Vec<u8>) {
    let n = b.len();
    if n < 256 {
        out.extend([0xc4, n as u8]);
    } else if n < 65536 {
        out.push(0xc5);
        out.extend((n as u16).to_be_bytes());
    } else {
        out.push(0xc6);
        out.extend((n as u32).to_be_bytes());
    }
    out.extend(b);
}

/// Writes the head of an array of `n` items.
pub fn array_head(n: usize, out: &mut Vec<u8>) {
    if n < 16 {
        out.push(0x90 | n as u8);
    } else if n < 65536 {
        out.push(0xdc);
        out.extend((n as u16).to_be_bytes());
    } else {
        out.push(0xdd);
        out.extend((n as u32).to_be_bytes());
    }
}

fn map_head(n: usize, out: &mut Vec<u8>) {
    if n < 16 {
        out.push(0x80 | n as u8);
    } else if n < 65536 {
        out.push(0xde);
        out.extend((n as u16).to_be_bytes());
    } else {
        out.push(0xdf);
        out.extend((n as u32).to_be_bytes());
    }
}

fn ext(kind: i8, data: &[u8], out: &mut Vec<u8>) {
    let n = data.len();
    if n < 256 {
        out.extend([0xc7, n as u8]);
    } else if n < 65536 {
        out.push(0xc8);
        out.extend((n as u16).to_be_bytes());
    } else {
        out.push(0xc9);
        out.extend((n as u32).to_be_bytes());
    }
    out.push(kind as u8);
    out.extend(data);
}

/// The value of msgpack `bytes`, one value that fills them. `None` for
/// bytes of another form.
pub fn decode(bytes: &[u8]) -> Option<Wire> {
    let mut at = 0;
    let value = read(bytes, &mut at, 0)?;
    (at == bytes.len()).then_some(value)
}

fn take<'a>(bytes: &'a [u8], at: &mut usize, n: usize) -> Option<&'a [u8]> {
    let end = at.checked_add(n)?;
    let out = bytes.get(*at..end)?;
    *at = end;
    Some(out)
}

fn be(bytes: &[u8], at: &mut usize, n: usize) -> Option<u64> {
    Some(
        take(bytes, at, n)?
            .iter()
            .fold(0u64, |acc, b| (acc << 8) | u64::from(*b)),
    )
}

fn read(bytes: &[u8], at: &mut usize, depth: usize) -> Option<Wire> {
    if depth > DEPTH + 2 {
        return None;
    }
    let head = *take(bytes, at, 1)?.first()?;
    let items = |n: usize, at: &mut usize| -> Option<Vec<Wire>> {
        (0..n).map(|_| read(bytes, at, depth + 1)).collect()
    };
    let pairs = |n: usize, at: &mut usize| -> Option<Vec<(Wire, Wire)>> {
        (0..n)
            .map(|_| Some((read(bytes, at, depth + 1)?, read(bytes, at, depth + 1)?)))
            .collect()
    };
    let string = |n: usize, at: &mut usize| -> Option<Wire> {
        let raw = take(bytes, at, n)?;
        Some(Wire::Str(std::str::from_utf8(raw).ok()?.to_owned()))
    };
    let ext_of = |n: usize, at: &mut usize| -> Option<Wire> {
        let kind = *take(bytes, at, 1)?.first()? as i8;
        Some(Wire::Ext(kind, take(bytes, at, n)?.to_vec()))
    };
    Some(match head {
        0x00..=0x7f => Wire::Int(i64::from(head)),
        0x80..=0x8f => Wire::Map(pairs(usize::from(head & 0x0f), at)?),
        0x90..=0x9f => Wire::Array(items(usize::from(head & 0x0f), at)?),
        0xa0..=0xbf => string(usize::from(head & 0x1f), at)?,
        0xc0 => Wire::Nil,
        0xc2 => Wire::Bool(false),
        0xc3 => Wire::Bool(true),
        0xc4..=0xc6 => {
            let n = be(bytes, at, 1 << (head - 0xc4))? as usize;
            Wire::Bin(take(bytes, at, n)?.to_vec())
        }
        0xc7..=0xc9 => {
            let n = be(bytes, at, 1 << (head - 0xc7))? as usize;
            ext_of(n, at)?
        }
        0xca => Wire::Float(f64::from(f32::from_bits(be(bytes, at, 4)? as u32))),
        0xcb => Wire::Float(f64::from_bits(be(bytes, at, 8)?)),
        0xcc..=0xcf => Wire::Int(i64::try_from(be(bytes, at, 1 << (head - 0xcc))?).ok()?),
        0xd0 => Wire::Int(i64::from(be(bytes, at, 1)? as u8 as i8)),
        0xd1 => Wire::Int(i64::from(be(bytes, at, 2)? as u16 as i16)),
        0xd2 => Wire::Int(i64::from(be(bytes, at, 4)? as u32 as i32)),
        0xd3 => Wire::Int(be(bytes, at, 8)? as i64),
        0xd4..=0xd8 => ext_of(1 << (head - 0xd4), at)?,
        0xd9..=0xdb => {
            let n = be(bytes, at, 1 << (head - 0xd9))? as usize;
            string(n, at)?
        }
        0xdc | 0xdd => {
            let n = be(bytes, at, if head == 0xdc { 2 } else { 4 })? as usize;
            Wire::Array(items(n, at)?)
        }
        0xde | 0xdf => {
            let n = be(bytes, at, if head == 0xde { 2 } else { 4 })? as usize;
            Wire::Map(pairs(n, at)?)
        }
        0xe0..=0xff => Wire::Int(i64::from(head as i8)),
        0xc1 => return None,
    })
}

/// The fields of the data of an ext 2: `_micros`, `_timezone_naive` and
/// the name of `_tz`.
pub fn date_time(data: &[u8]) -> Option<(i64, bool, &str)> {
    let micros = i64::from_be_bytes(data.get(..8)?.try_into().ok()?);
    let naive = match data.get(8)? {
        0 => false,
        1 => true,
        _ => return None,
    };
    Some((micros, naive, std::str::from_utf8(&data[9..]).ok()?))
}

/// The arguments of the data of an ext 3.
pub fn message_args(data: &[u8]) -> Option<Vec<Wire>> {
    match decode(data)? {
        Wire::Array(args) if !args.is_empty() => Some(args),
        _ => None,
    }
}
