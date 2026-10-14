//! The protocol layer of the client: the frames and the messages of ZEO 6
//! with msgpack.
//!
//! A frame is a 4-byte big-endian length, then the bytes of the frame. The
//! first frame of each side is the protocol, `M5`, as raw bytes. Each later
//! frame is a msgpack array `[id, flags, method, args]`. On a connection
//! that got the extension `trace_context`, a call can have a fifth element:
//! its trace context, a map whose key `traceparent` holds a W3C
//! traceparent.

use std::io;

use rmpv::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use zoe_core::{Oid, Tid};

use crate::Error;

/// The protocol of ZEO 6 with msgpack.
pub(crate) const PROTOCOL: &[u8] = b"M5";

/// The largest buffer that a read reserves before the bytes arrive. A
/// longer frame grows its buffer as its bytes arrive, so a false length
/// does not reserve memory.
const RESERVE: usize = 1 << 20;

/// The tid that `load` gives to `loadBefore`, as `ZEO.ClientStorage` does:
/// `ZODB.utils.maxtid`.
pub(crate) const MAX_TID: Tid = Tid(i64::MAX as u64);

/// The flags of a reply that carries an error.
const ERROR: i64 = 2;

/// The class of the conflict error of ZODB.
const CONFLICT: &str = "ZODB.POSException.ConflictError";

/// The class of the read conflict error of ZODB.
const READ_CONFLICT: &str = "ZODB.POSException.ReadConflictError";

/// Read one frame from `reader`. Return `None` at the end of the stream
/// before a frame starts. An end inside a frame fails with `UnexpectedEof`.
pub(crate) async fn read_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> io::Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    let mut got = 0;
    while got < len.len() {
        let n = reader.read(&mut len[got..]).await?;
        if n == 0 {
            if got == 0 {
                return Ok(None);
            }
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        got += n;
    }
    let len = u32::from_be_bytes(len) as usize;
    let mut payload = Vec::with_capacity(len.min(RESERVE));
    let read = reader.take(len as u64).read_to_end(&mut payload).await?;
    if read < len {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    Ok(Some(payload))
}

/// Write the frame of `payload` to `writer`.
pub(crate) async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    payload: &[u8],
) -> Result<(), Error> {
    writer.write_all(&frame(payload)?).await?;
    Ok(())
}

/// Return the frame of `payload`: its length, then its bytes.
fn frame(payload: &[u8]) -> Result<Vec<u8>, Error> {
    let len = u32::try_from(payload.len())
        .map_err(|_| Error::Invalid(format!("a frame of {} bytes", payload.len())))?;
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&len.to_be_bytes());
    frame.extend_from_slice(payload);
    Ok(frame)
}

/// The key of the W3C traceparent in the trace context of a call.
const TRACEPARENT: &str = "traceparent";

/// Return the frame of the message `[id, flags, method, args]`. With a
/// `traceparent`, the message has a fifth element: the trace context.
fn message(
    id: i64,
    flags: Value,
    method: &str,
    args: Value,
    traceparent: Option<&str>,
) -> Result<Vec<u8>, Error> {
    let mut parts = vec![id.into(), flags, method.into(), args];
    if let Some(traceparent) = traceparent {
        parts.push(Value::Map(vec![(TRACEPARENT.into(), traceparent.into())]));
    }
    let message = Value::Array(parts);
    let mut payload = Vec::new();
    rmpv::encode::write_value(&mut payload, &message)
        .map_err(|e| Error::Invalid(format!("{method}: {e}")))?;
    frame(&payload)
}

/// Return the frame of a call that waits for its reply. A stock client
/// sends `false` as its flags. With a `traceparent`, the call carries it.
pub(crate) fn call(
    id: i64,
    method: &str,
    args: Vec<Value>,
    traceparent: Option<&str>,
) -> Result<Vec<u8>, Error> {
    message(id, false.into(), method, Value::Array(args), traceparent)
}

/// Return the frame of a call without a reply: the id 0 and the flags
/// `true`. With a `traceparent`, the call carries it.
pub(crate) fn notify(
    method: &str,
    args: Vec<Value>,
    traceparent: Option<&str>,
) -> Result<Vec<u8>, Error> {
    message(0, true.into(), method, Value::Array(args), traceparent)
}

/// Return the frame of the heartbeat: `[-1, 0, ".reply", nil]`.
pub(crate) fn heartbeat() -> Vec<u8> {
    message(-1, 0.into(), ".reply", Value::Nil, None).expect("the heartbeat encodes")
}

/// A message from the server.
#[derive(Debug)]
pub(crate) enum Message {
    /// The reply to the call with the id `id`.
    Reply {
        id: i64,
        result: Result<Value, Error>,
    },
    /// A call of the server, as `invalidateTransaction` or `info`.
    Call { method: String, args: Value },
    /// A reply with an id that the client never gives to a call.
    Other,
}

/// Decode the payload of a frame from the server.
pub(crate) fn decode(payload: &[u8]) -> Result<Message, Error> {
    let mut bytes = payload;
    let value = rmpv::decode::read_value(&mut bytes)
        .map_err(|e| Error::Protocol(format!("a frame that is not msgpack: {e}")))?;
    if !bytes.is_empty() {
        return Err(Error::Protocol("bytes after the message".into()));
    }
    let Value::Array(items) = value else {
        return Err(Error::Protocol("a message that is not an array".into()));
    };
    let [id, flags, method, args]: [Value; 4] = items
        .try_into()
        .map_err(|_| Error::Protocol("a message without 4 items".into()))?;
    let Some(method) = method.as_str() else {
        return Err(Error::Protocol("a method that is not a str".into()));
    };
    if method != ".reply" {
        return Ok(Message::Call {
            method: method.to_owned(),
            args,
        });
    }
    let Some(id) = id.as_i64().filter(|id| *id > 0) else {
        return Ok(Message::Other);
    };
    let flags = match flags {
        Value::Boolean(flag) => i64::from(flag),
        other => other.as_i64().unwrap_or(0),
    };
    let result = if flags & ERROR != 0 {
        Err(server_error(args))
    } else {
        Ok(args)
    };
    Ok(Message::Reply { id, result })
}

/// Return the error of an error reply, whose args are `[class, value]`. A
/// `ConflictError` with an oid is `Error::Conflict`, and a
/// `ReadConflictError` with an oid is `Error::ReadConflict`. The message
/// of another error is empty without an argument, the text of its one
/// argument, or else the value.
fn server_error(args: Value) -> Error {
    let (class, value) = match args {
        Value::Array(items) if items.len() == 2 => {
            let mut items = items.into_iter();
            (items.next().unwrap(), items.next().unwrap())
        }
        other => return Error::Protocol(format!("an error reply of {other}")),
    };
    let Some(class) = class.as_str().map(str::to_owned) else {
        return Error::Protocol(format!("an error class of {class}"));
    };
    if class == CONFLICT
        && let Some(conflict) = conflict(&value)
    {
        return conflict;
    }
    if class == READ_CONFLICT
        && let Some(Error::Conflict { oid, serials }) = conflict(&value)
    {
        return Error::ReadConflict { oid, serials };
    }
    let message = match &value {
        Value::Array(items) if items.is_empty() => String::new(),
        Value::Array(items) if items.len() == 1 && items[0].as_str().is_some() => {
            items[0].as_str().unwrap_or_default().to_owned()
        }
        value => value.to_string(),
    };
    Error::Server { class, message }
}

/// Return `Error::Conflict` from a map with the keys `oid` and `serials`,
/// as an error reply and a vote reply give them. `serials` is
/// `[committed, read]`, or nil.
pub(crate) fn conflict(value: &Value) -> Option<Error> {
    let oid = get(value, "oid").and_then(p64).map(Oid)?;
    let serials = match get(value, "serials") {
        None | Some(Value::Nil) => None,
        Some(Value::Array(items)) if items.len() == 2 => {
            Some((Tid(p64(&items[0])?), Tid(p64(&items[1])?)))
        }
        Some(_) => return None,
    };
    Some(Error::Conflict { oid, serials })
}

/// Return the value of the str key `key` in the map `value`.
pub(crate) fn get<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    value
        .as_map()?
        .iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .map(|(_, v)| v)
}

/// Return the 8 big-endian bytes of an oid or a tid.
pub(crate) fn bin8(n: u64) -> Value {
    Value::Binary(n.to_be_bytes().to_vec())
}

/// Read an oid or a tid: a bin of 8 bytes.
pub(crate) fn p64(value: &Value) -> Option<u64> {
    let bytes: [u8; 8] = value.as_slice()?.try_into().ok()?;
    Some(u64::from_be_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(frame: &[u8]) -> &[u8] {
        let len = u32::from_be_bytes(frame[..4].try_into().unwrap()) as usize;
        assert_eq!(frame.len(), 4 + len);
        &frame[4..]
    }

    fn encode(value: Value) -> Vec<u8> {
        let mut out = Vec::new();
        rmpv::encode::write_value(&mut out, &value).unwrap();
        out
    }

    #[test]
    fn a_call_has_an_int_id_and_false_flags() {
        let frame = call(7, "lastTransaction", vec![], None).unwrap();
        let value = rmpv::decode::read_value(&mut payload(&frame)).unwrap();
        assert_eq!(
            value,
            Value::Array(vec![
                7.into(),
                false.into(),
                "lastTransaction".into(),
                Value::Array(vec![])
            ])
        );
    }

    #[test]
    fn a_notification_has_the_id_0_and_true_flags() {
        let frame = notify("storea", vec![bin8(1)], None).unwrap();
        let value = rmpv::decode::read_value(&mut payload(&frame)).unwrap();
        assert_eq!(
            value,
            Value::Array(vec![
                0.into(),
                true.into(),
                "storea".into(),
                Value::Array(vec![Value::Binary(vec![0, 0, 0, 0, 0, 0, 0, 1])])
            ])
        );
    }

    #[test]
    fn a_traceparent_is_a_fifth_element() {
        let traceparent = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        let context = Value::Map(vec![("traceparent".into(), traceparent.into())]);
        let frame = call(7, "loadBefore", vec![bin8(1)], Some(traceparent)).unwrap();
        let value = rmpv::decode::read_value(&mut payload(&frame)).unwrap();
        assert_eq!(
            value,
            Value::Array(vec![
                7.into(),
                false.into(),
                "loadBefore".into(),
                Value::Array(vec![bin8(1)]),
                context.clone(),
            ])
        );
        let frame = notify("storea", vec![], Some(traceparent)).unwrap();
        let value = rmpv::decode::read_value(&mut payload(&frame)).unwrap();
        assert_eq!(value.as_array().unwrap().len(), 5);
        assert_eq!(value.as_array().unwrap()[4], context);
    }

    #[test]
    fn the_heartbeat_is_the_frame_of_a_stock_client() {
        // `[-1, 0, ".reply", nil]`, as in the trace `heartbeat.jsonl`.
        assert_eq!(
            payload(&heartbeat()),
            b"\x94\xff\x00\xa6.reply\xc0".as_slice()
        );
    }

    #[test]
    fn a_reply_goes_to_its_id_with_int_or_bool_flags() {
        for flags in [Value::from(0), Value::from(false)] {
            let bytes = encode(Value::Array(vec![
                3.into(),
                flags,
                ".reply".into(),
                bin8(9),
            ]));
            match decode(&bytes).unwrap() {
                Message::Reply { id: 3, result } => assert_eq!(result.unwrap(), bin8(9)),
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn an_error_reply_gives_its_class() {
        let bytes = encode(Value::Array(vec![
            4.into(),
            2.into(),
            ".reply".into(),
            Value::Array(vec![
                "ZODB.POSException.POSKeyError".into(),
                Value::Array(vec![bin8(1)]),
            ]),
        ]));
        match decode(&bytes).unwrap() {
            Message::Reply {
                id: 4,
                result: Err(Error::Server { class, .. }),
            } => assert_eq!(class, "ZODB.POSException.POSKeyError"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_conflict_error_gives_its_oid_and_serials() {
        let map = |serials: Value| {
            Value::Map(vec![
                ("message".into(), "database conflict error".into()),
                ("oid".into(), bin8(5)),
                ("class_name".into(), Value::Nil),
                ("serials".into(), serials),
            ])
        };
        let reply = |value| {
            encode(Value::Array(vec![
                1.into(),
                2.into(),
                ".reply".into(),
                Value::Array(vec![CONFLICT.into(), value]),
            ]))
        };
        match decode(&reply(map(Value::Array(vec![bin8(3), bin8(2)])))).unwrap() {
            Message::Reply {
                result: Err(Error::Conflict { oid, serials }),
                ..
            } => {
                assert_eq!(oid, Oid(5));
                assert_eq!(serials, Some((Tid(3), Tid(2))));
            }
            other => panic!("{other:?}"),
        }
        // After a lock-wait timeout, the serials are nil.
        match decode(&reply(map(Value::Nil))).unwrap() {
            Message::Reply {
                result: Err(Error::Conflict { serials: None, .. }),
                ..
            } => {}
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_read_conflict_error_gives_its_oid_and_serials() {
        let value = Value::Map(vec![
            ("message".into(), "database read conflict error".into()),
            ("oid".into(), bin8(5)),
            ("class_name".into(), Value::Nil),
            ("serials".into(), Value::Array(vec![bin8(3), bin8(2)])),
        ]);
        let bytes = encode(Value::Array(vec![
            1.into(),
            2.into(),
            ".reply".into(),
            Value::Array(vec![READ_CONFLICT.into(), value]),
        ]));
        match decode(&bytes).unwrap() {
            Message::Reply {
                result: Err(Error::ReadConflict { oid, serials }),
                ..
            } => {
                assert_eq!(oid, Oid(5));
                assert_eq!(serials, Some((Tid(3), Tid(2))));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_message_of_an_error_is_the_text_of_its_arguments() {
        let reply = |args: Value| {
            encode(Value::Array(vec![
                1.into(),
                2.into(),
                ".reply".into(),
                Value::Array(vec!["builtins.TypeError".into(), args]),
            ]))
        };
        match decode(&reply(Value::Array(vec!["bad args".into()]))).unwrap() {
            Message::Reply {
                result: Err(Error::Server { class, message }),
                ..
            } => {
                assert_eq!(class, "builtins.TypeError");
                assert_eq!(message, "bad args");
            }
            other => panic!("{other:?}"),
        }
        match decode(&reply(Value::Array(vec![1.into(), 2.into()]))).unwrap() {
            Message::Reply {
                result: Err(Error::Server { message, .. }),
                ..
            } => assert_eq!(message, "[1, 2]"),
            other => panic!("{other:?}"),
        }
        match decode(&reply(Value::Array(vec![]))).unwrap() {
            Message::Reply {
                result: Err(Error::Server { message, .. }),
                ..
            } => assert_eq!(message, ""),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_call_of_the_server_is_a_call() {
        let bytes = encode(Value::Array(vec![
            0.into(),
            true.into(),
            "invalidateTransaction".into(),
            Value::Array(vec![bin8(2), Value::Array(vec![bin8(1)])]),
        ]));
        match decode(&bytes).unwrap() {
            Message::Call { method, .. } => assert_eq!(method, "invalidateTransaction"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_bad_frame_is_a_protocol_error() {
        for bytes in [
            b"\xc1".to_vec(),
            encode(Value::Array(vec![1.into(), 0.into()])),
            encode(Value::Array(vec![1.into(), 0.into(), 2.into(), Value::Nil])),
            [encode(Value::Nil), vec![0]].concat(),
        ] {
            assert!(matches!(decode(&bytes), Err(Error::Protocol(_))));
        }
    }

    #[test]
    fn a_frame_reads_back() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            let (mut a, mut b) = tokio::io::duplex(64);
            let payload = vec![7u8; 1000];
            let write = async {
                write_frame(&mut a, &payload).await.unwrap();
                drop(a);
            };
            let read = async {
                let first = read_frame(&mut b).await.unwrap();
                let second = read_frame(&mut b).await.unwrap();
                (first, second)
            };
            let ((), (first, second)) = tokio::join!(write, read);
            assert_eq!(first, Some(payload.clone()));
            assert_eq!(second, None);
        });
    }
}
