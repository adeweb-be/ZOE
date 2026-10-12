//! The messages of ZEO 6 with msgpack: `[message_id, flags, method, args]`.
//!
//! A reply has the method `.reply`. Its flags are 0, or 2 when it carries
//! an error. The args of an error are `[class, args]`, as ZEO's
//! `reduce_exception` gives them.

use rmpv::Value;

/// The message id of a heartbeat. It gets no reply.
pub(crate) const HEARTBEAT: i64 = -1;

/// The flags of an error reply.
const ERROR: u8 = 2;

/// The decode budget for one frame. rmpv spends about two steps on each
/// level of arrays or maps, so this allows about 60 levels. A deeper frame
/// is not a call. The bound keeps the decode far from the end of the stack
/// of a runtime thread.
const MAX_DEPTH: usize = 128;

/// The most msgpack values in one frame, the message itself included. rmpv
/// makes a `Value` of 40 bytes for each value, and an array or a map takes
/// more. So without this bound, a frame of 1-byte values costs about 40
/// times its size in memory. With it, the values of one frame take at most
/// about 16.2 MiB. The worst case is chains of arrays of one element: rmpv
/// gives each array room for 4 values, so a value takes about 176 bytes. A
/// message of a stock client has fewer than 20 values: a record is one
/// `bin` value, whatever its size.
const MAX_VALUES: usize = 100_000;

/// A call from the client.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Call {
    /// The message id. The reply carries it back, whatever its type.
    pub(crate) id: Value,
    /// True for a call that gets no reply, as `storea` and `tpc_begin`.
    pub(crate) oneway: bool,
    /// The name of the method.
    pub(crate) method: String,
    /// The arguments.
    pub(crate) args: Vec<Value>,
}

impl Call {
    /// Tell if the call is a heartbeat.
    pub(crate) fn is_heartbeat(&self) -> bool {
        self.id.as_i64() == Some(HEARTBEAT)
    }
}

/// Decode the frame of a call. Return a text that says why the frame is
/// not a call.
pub(crate) fn decode(frame: &[u8]) -> Result<Call, String> {
    check_values(frame, MAX_VALUES)?;
    let mut bytes = frame;
    let value = rmpv::decode::read_value_with_max_depth(&mut bytes, MAX_DEPTH)
        .map_err(|e| e.to_string())?;
    if !bytes.is_empty() {
        return Err("bytes after the message".into());
    }
    let Value::Array(parts) = value else {
        return Err("a message is not an array".into());
    };
    let Ok([id, flags, method, args]) = <[Value; 4]>::try_from(parts) else {
        return Err("a message does not have four parts".into());
    };
    // The stock client sends its heartbeat with nil args. A heartbeat gets
    // no reply, so its other parts do not matter.
    if id.as_i64() == Some(HEARTBEAT) {
        return Ok(Call {
            id,
            oneway: true,
            method: String::new(),
            args: Vec::new(),
        });
    }
    let oneway = match flags {
        Value::Boolean(flag) => flag,
        Value::Integer(n) => n.as_u64() != Some(0),
        Value::Nil => false,
        _ => return Err("the flags are not a bool or an int".into()),
    };
    let method = match method {
        Value::String(text) => text.into_str().ok_or("the method is not UTF-8")?,
        _ => return Err("the method is not a string".into()),
    };
    let args = match args {
        Value::Array(args) => args,
        _ => return Err("the args are not an array".into()),
    };
    Ok(Call {
        id,
        oneway,
        method,
        args,
    })
}

/// Encode the reply to the call `id` with `result`.
pub(crate) fn reply(id: Value, result: Value) -> Vec<u8> {
    encode(Value::Array(vec![
        id,
        Value::from(0),
        Value::from(".reply"),
        result,
    ]))
}

/// Encode the error reply to the call `id`: the exception `class` with
/// `args`.
pub(crate) fn error(id: Value, class: &str, args: Value) -> Vec<u8> {
    encode(Value::Array(vec![
        id,
        Value::from(ERROR),
        Value::from(".reply"),
        Value::Array(vec![Value::from(class), args]),
    ]))
}

/// Encode a call of the server to the client: `method` with `args`. It
/// has the id 0 and the flags true, and it gets no reply.
pub(crate) fn notify(method: &str, args: Vec<Value>) -> Vec<u8> {
    encode(Value::Array(vec![
        Value::from(0),
        Value::Boolean(true),
        Value::from(method),
        Value::Array(args),
    ]))
}

fn encode(value: Value) -> Vec<u8> {
    let mut bytes = Vec::new();
    // A write to a vector never fails.
    rmpv::encode::write_value(&mut bytes, &value).expect("write to a vector");
    bytes
}

/// Check that the first msgpack value of `frame` has at most `max` values,
/// itself included, before rmpv builds them. The scan reads only the
/// markers and the lengths, and it allocates nothing. It counts each value
/// that an array or a map announces, so a false count fails at once. A
/// frame that ends too early passes, and rmpv refuses it.
fn check_values(frame: &[u8], max: usize) -> Result<(), String> {
    let too_many = || format!("a message has more than {max} values");
    let max = max as u64;
    // The values that the scan read, and the values that it must still read.
    let (mut read, mut pending) = (0u64, 1u64);
    let mut at = 0usize;
    while pending > 0 {
        let Some(&marker) = frame.get(at) else {
            return Ok(());
        };
        at += 1;
        pending -= 1;
        read += 1;
        // The bytes after the marker, and the values that it announces.
        let (skip, values) = match marker {
            0x80..=0x8f => (0, 2 * u64::from(marker & 0x0f)),
            0x90..=0x9f => (0, u64::from(marker & 0x0f)),
            0xa0..=0xbf => (u64::from(marker & 0x1f), 0),
            0xc4 | 0xd9 => (1 + length(frame, at, 1), 0),
            0xc5 | 0xda => (2 + length(frame, at, 2), 0),
            0xc6 | 0xdb => (4 + length(frame, at, 4), 0),
            0xc7 => (2 + length(frame, at, 1), 0),
            0xc8 => (3 + length(frame, at, 2), 0),
            0xc9 => (5 + length(frame, at, 4), 0),
            0xcc | 0xd0 => (1, 0),
            0xcd | 0xd1 | 0xd4 => (2, 0),
            0xd5 => (3, 0),
            0xca | 0xce | 0xd2 => (4, 0),
            0xd6 => (5, 0),
            0xcb | 0xcf | 0xd3 => (8, 0),
            0xd7 => (9, 0),
            0xd8 => (17, 0),
            0xdc => (2, length(frame, at, 2)),
            0xdd => (4, length(frame, at, 4)),
            0xde => (2, 2 * length(frame, at, 2)),
            0xdf => (4, 2 * length(frame, at, 4)),
            // A fixint, nil, a bool, or the reserved marker, which rmpv
            // reads as nil.
            _ => (0, 0),
        };
        pending += values;
        if read + pending > max {
            return Err(too_many());
        }
        at = at.saturating_add(usize::try_from(skip).unwrap_or(usize::MAX));
    }
    Ok(())
}

/// Return the big-endian number of `size` bytes at `at` in `frame`, or 0
/// when the frame ends before it.
fn length(frame: &[u8], at: usize, size: usize) -> u64 {
    frame.get(at..at.saturating_add(size)).map_or(0, |bytes| {
        bytes.iter().fold(0, |n, &b| n << 8 | u64::from(b))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(value: Value) -> Vec<u8> {
        encode(value)
    }

    #[test]
    fn a_call_decodes_with_any_message_id() {
        let id = Value::Array(vec![Value::Binary(vec![0; 8]), Value::Binary(vec![1; 8])]);
        let bytes = frame(Value::Array(vec![
            id.clone(),
            Value::Boolean(false),
            Value::from("loadBefore"),
            Value::Array(vec![Value::Nil]),
        ]));
        let call = decode(&bytes).unwrap();
        assert_eq!(call.id, id);
        assert!(!call.oneway);
        assert_eq!(call.method, "loadBefore");
        assert_eq!(call.args, vec![Value::Nil]);
    }

    #[test]
    fn a_heartbeat_has_the_id_minus_one() {
        let bytes = frame(Value::Array(vec![
            Value::from(-1),
            Value::from(0),
            Value::from(".reply"),
            Value::Array(vec![]),
        ]));
        assert!(decode(&bytes).unwrap().is_heartbeat());
    }

    #[test]
    fn the_heartbeat_of_a_stock_client_with_nil_args_is_a_heartbeat() {
        // [-1, 0, ".reply", nil], as ZEO 6.2 sends it.
        let bytes = [
            0x94, 0xff, 0x00, 0xa6, b'.', b'r', b'e', b'p', b'l', b'y', 0xc0,
        ];
        assert!(decode(&bytes).unwrap().is_heartbeat());
    }

    #[test]
    fn a_frame_nested_too_deep_is_refused() {
        let mut deep = vec![0x91; 100_000];
        deep.push(0xc0);
        assert!(decode(&deep).is_err());
        // A call with 20 levels of arrays in its args decodes.
        let mut args = Value::Nil;
        for _ in 0..20 {
            args = Value::Array(vec![args]);
        }
        let bytes = frame(Value::Array(vec![
            Value::from(1),
            Value::Boolean(false),
            Value::from("ping"),
            Value::Array(vec![args.clone()]),
        ]));
        assert_eq!(decode(&bytes).unwrap().args, vec![args]);
    }

    /// Return the number of values in `value`, itself included.
    fn values(value: &Value) -> usize {
        1 + match value {
            Value::Array(items) => items.iter().map(values).sum(),
            Value::Map(pairs) => pairs.iter().map(|(k, v)| values(k) + values(v)).sum(),
            _ => 0,
        }
    }

    /// A value with each marker of msgpack: each width of int, of float, of
    /// str, of bin, of ext, of array and of map. With `long`, it has the
    /// markers of 32 bits too.
    fn each_marker(long: bool) -> Value {
        let mut items = vec![
            Value::Nil,
            Value::Boolean(true),
            Value::from(7),
            Value::from(-7),
            Value::from(200u8),
            Value::from(60_000u16),
            Value::from(70_000u32),
            Value::from(u64::MAX),
            Value::from(-100i8),
            Value::from(-1_000i16),
            Value::from(-100_000i32),
            Value::from(-10_000_000_000i64),
            Value::F32(1.5),
            Value::F64(2.5),
            Value::from("x"),
            Value::from("y".repeat(40)),
            Value::from("z".repeat(300)),
            Value::Binary(vec![1; 3]),
            Value::Binary(vec![2; 300]),
            Value::Array((0..20).map(Value::from).collect()),
            Value::Map((0..3).map(|n| (Value::from(n), Value::Nil)).collect()),
            Value::Map((0..20).map(|n| (Value::from(n), Value::Nil)).collect()),
        ];
        for len in [1, 2, 4, 8, 16, 3, 300] {
            items.push(Value::Ext(5, vec![0; len]));
        }
        if long {
            items.extend([
                Value::from("s".repeat(70_000)),
                Value::Binary(vec![3; 70_000]),
                Value::Ext(5, vec![0; 70_000]),
                Value::Array(vec![Value::Nil; 70_000]),
                Value::Map(vec![(Value::Nil, Value::Nil); 70_000]),
            ]);
        }
        Value::Array(items)
    }

    #[test]
    fn the_scan_counts_the_values_as_rmpv_reads_them() {
        for long in [false, true] {
            let value = each_marker(long);
            let bytes = frame(value.clone());
            let n = values(&value);
            assert_eq!(check_values(&bytes, n), Ok(()), "long: {long}");
            assert!(check_values(&bytes, n - 1).is_err(), "long: {long}");
        }
        // The scan passes each cut frame, and rmpv refuses it.
        let bytes = frame(each_marker(false));
        for cut in 0..bytes.len() {
            assert_eq!(
                check_values(&bytes[..cut], MAX_VALUES),
                Ok(()),
                "cut at {cut}"
            );
            assert!(decode(&bytes[..cut]).is_err(), "cut at {cut}");
        }
    }

    #[test]
    fn a_frame_with_too_many_values_is_refused_before_the_decode() {
        let call = |args: Value| {
            frame(Value::Array(vec![
                Value::from(1),
                Value::Boolean(false),
                Value::from("ping"),
                args,
            ]))
        };
        // The bound counts the message, its 3 first parts, the args and
        // the values in the args.
        let nils = |n: usize| Value::Array(vec![Value::Nil; n]);
        assert!(decode(&call(nils(MAX_VALUES - 5))).is_ok());
        let error = decode(&call(nils(MAX_VALUES - 4))).unwrap_err();
        assert!(error.contains("100000 values"), "{error}");
        // An array that announces 4 Gi values and holds none: the scan
        // refuses it at its marker.
        let mut announced = vec![0x94, 0x01, 0xc2, 0xa4, b'p', b'i', b'n', b'g', 0xdd];
        announced.extend_from_slice(&u32::MAX.to_be_bytes());
        assert!(decode(&announced).unwrap_err().contains("values"));
        // Small maps in one array: each announces few values, and the scan
        // counts them all.
        let maps = Value::Array(vec![Value::Map(vec![(Value::Nil, Value::Nil)]); 40_000]);
        assert!(decode(&call(maps)).unwrap_err().contains("values"));
    }

    #[test]
    fn a_frame_that_is_not_a_call_is_refused() {
        for value in [
            Value::from(1),
            Value::Array(vec![Value::from(1), Value::Boolean(false)]),
            Value::Array(vec![
                Value::from(1),
                Value::Boolean(false),
                Value::from(3),
                Value::Array(vec![]),
            ]),
        ] {
            assert!(decode(&frame(value)).is_err());
        }
        assert!(decode(b"").is_err());
    }

    /// Return the bytes of the hex text `hex`.
    fn unhex(hex: &str) -> Vec<u8> {
        (0..hex.len())
            .step_by(2)
            .map(|at| u8::from_str_radix(&hex[at..at + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn the_calls_of_the_server_have_the_bytes_of_a_stock_server() {
        // `info` and `invalidateTransaction` as `plain.jsonl` of the wire
        // spike recorded them.
        let info = notify(
            "info",
            vec![Value::Map(vec![
                (Value::from("length"), Value::from(1)),
                (Value::from("size"), Value::from(225)),
            ])],
        );
        assert_eq!(
            info,
            unhex("9400c3a4696e666f9182a66c656e67746801a473697a65cce1")
        );
        let tid = Value::Binary(unhex("040c458fcdb65344"));
        let oid = Value::Binary(unhex("0000000000000001"));
        let invalidation = notify("invalidateTransaction", vec![tid, Value::Array(vec![oid])]);
        assert_eq!(
            invalidation,
            unhex(
                "9400c3b5696e76616c69646174655472616e73616374696f6e92\
                 c408040c458fcdb6534491c4080000000000000001"
            )
        );
    }

    #[test]
    fn receive_blob_stop_has_the_bytes_of_a_stock_server() {
        // `receiveBlobStop [oid, serial]` as `plain.jsonl` of the wire spike
        // recorded it.
        let oid = Value::Binary(unhex("0000000000000004"));
        let serial = Value::Binary(unhex("040c458fd0d4a466"));
        assert_eq!(
            notify("receiveBlobStop", vec![oid, serial]),
            unhex(
                "9400c3af72656365697665426c6f6253746f7092\
                 c4080000000000000004c408040c458fd0d4a466"
            )
        );
    }

    #[test]
    fn a_reply_carries_the_id_and_an_error_the_flags_2() {
        let id = Value::from(7);
        let ok = rmpv::decode::read_value(&mut reply(id.clone(), Value::Nil).as_slice()).unwrap();
        assert_eq!(
            ok,
            Value::Array(vec![
                id.clone(),
                Value::from(0),
                Value::from(".reply"),
                Value::Nil
            ])
        );
        let bytes = error(id.clone(), "builtins.ValueError", Value::Array(vec![]));
        let failed = rmpv::decode::read_value(&mut bytes.as_slice()).unwrap();
        assert_eq!(
            failed,
            Value::Array(vec![
                id,
                Value::from(2),
                Value::from(".reply"),
                Value::Array(vec![
                    Value::from("builtins.ValueError"),
                    Value::Array(vec![])
                ]),
            ])
        );
    }
}
