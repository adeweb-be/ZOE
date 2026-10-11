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

fn encode(value: Value) -> Vec<u8> {
    let mut bytes = Vec::new();
    // A write to a vector never fails.
    rmpv::encode::write_value(&mut bytes, &value).expect("write to a vector");
    bytes
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
