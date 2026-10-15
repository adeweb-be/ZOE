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
use std::sync::Mutex;

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

/// The shortest payload that `read_payload` reads in two parts: first its
/// start, then the rest. A shorter payload is one read.
const SPLIT: usize = 16 * 1024;

/// The bytes at the start of a long payload that `read_payload` reads
/// first. They hold the head of a reply up to the first byte of its bin.
const PEEK: usize = 64;

/// Read one frame from `reader`. Return `None` at the end of the stream
/// before a frame starts. An end inside a frame fails with `UnexpectedEof`.
pub(crate) async fn read_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> io::Result<Option<Vec<u8>>> {
    let Some(len) = read_len(reader).await? else {
        return Ok(None);
    };
    let mut payload = Vec::new();
    read_more(reader, &mut payload, len).await?;
    Ok(Some(payload))
}

/// Read the length of a frame. Return `None` at the end of the stream
/// before the length starts.
async fn read_len<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<Option<usize>> {
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
    Ok(Some(u32::from_be_bytes(len) as usize))
}

/// Read `len` more bytes from `reader` at the end of `buffer`.
async fn read_more<R: AsyncRead + Unpin>(
    reader: &mut R,
    buffer: &mut Vec<u8>,
    len: usize,
) -> io::Result<()> {
    buffer.reserve(len.min(RESERVE));
    let read = reader.take(len as u64).read_to_end(buffer).await?;
    if read < len {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    Ok(())
}

/// The most buffers that `recycle` keeps.
const SPARES: usize = 16;

/// The largest buffer that `recycle` keeps.
const SPARE_BYTES: usize = 256 * 1024;

/// The buffers that `recycle` keeps for the next bins.
static SPARE: Spares = Spares(Mutex::new(Vec::new()));

/// Give the buffer of a bin back after its use, for example the data of a
/// `Record`. The next bin of a long payload then reuses it: the process
/// does not free and allocate a large buffer for each load, and the
/// allocator does not give the pages back to the kernel and fault them in
/// again. Keep at most `SPARES` buffers, each of `SPLIT` to `SPARE_BYTES`
/// bytes. Drop each other buffer.
pub fn recycle(buffer: Vec<u8>) {
    SPARE.give(buffer);
}

/// Buffers for bins. The code never waits for the lock: while another
/// thread holds it, a buffer drops or a new buffer comes. So a caller that
/// holds the GIL does not wait, and a child of `fork` whose parent held the
/// lock at the fork does not wait for ever.
struct Spares(Mutex<Vec<Vec<u8>>>);

impl Spares {
    /// Keep `buffer` for a later bin, with the rules of `recycle`.
    fn give(&self, mut buffer: Vec<u8>) {
        if !(SPLIT..=SPARE_BYTES).contains(&buffer.capacity()) {
            return;
        }
        if let Ok(mut spare) = self.0.try_lock()
            && spare.len() < SPARES
        {
            buffer.clear();
            spare.push(buffer);
        }
    }

    /// Return an empty buffer for `len` bytes: a kept buffer that holds
    /// them, else a new buffer of at most `RESERVE` bytes.
    fn take(&self, len: usize) -> Vec<u8> {
        let kept = self.0.try_lock().ok().and_then(|mut spare| {
            let at = spare.iter().position(|buffer| buffer.capacity() >= len)?;
            Some(spare.swap_remove(at))
        });
        kept.unwrap_or_else(|| Vec::with_capacity(len.min(RESERVE)))
    }
}

/// The payload of a frame from the server, as `read_payload` reads it.
#[derive(Clone, Debug)]
pub(crate) enum Payload {
    /// The bytes of the payload in one buffer.
    Whole(Vec<u8>),
    /// A reply whose args are a bin, or an array whose first item is a bin.
    /// The bin has its own buffer, so its bytes go from the stream to that
    /// buffer and are not copied again.
    Bin {
        /// The id of the reply.
        id: i64,
        /// The items of the args, or `None` when the args are the bin.
        items: Option<usize>,
        /// The bytes of the bin.
        bin: Vec<u8>,
        /// The bytes of the payload after the bin.
        rest: Vec<u8>,
    },
}

/// Read one frame from `reader`, as `read_frame` does. A payload of at
/// least `SPLIT` bytes that starts as a reply with a bin (`lead`) gives
/// `Payload::Bin`. Each other payload gives `Payload::Whole`.
pub(crate) async fn read_payload<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> io::Result<Option<Payload>> {
    let Some(len) = read_len(reader).await? else {
        return Ok(None);
    };
    let mut head = Vec::new();
    if len < SPLIT {
        read_more(reader, &mut head, len).await?;
        return Ok(Some(Payload::Whole(head)));
    }
    read_more(reader, &mut head, PEEK).await?;
    let Some(lead) = lead(&head).filter(|lead| lead.len <= len - lead.at) else {
        read_more(reader, &mut head, len - PEEK).await?;
        return Ok(Some(Payload::Whole(head)));
    };
    // A short rest comes in the same read as the bin, then moves to its
    // own buffer. So the frame of a load needs no read for its rest.
    let after = len - lead.at - lead.len;
    let with_bin = if after <= PEEK {
        lead.len + after
    } else {
        lead.len
    };
    let mut bin = SPARE.take(with_bin);
    let end = PEEK.min(lead.at + with_bin);
    bin.extend_from_slice(&head[lead.at..end]);
    let more = with_bin - bin.len();
    read_more(reader, &mut bin, more).await?;
    let mut rest = if with_bin > lead.len {
        bin.split_off(lead.len)
    } else {
        head.split_off(end)
    };
    let more = after - rest.len();
    read_more(reader, &mut rest, more).await?;
    Ok(Some(Payload::Bin {
        id: lead.id,
        items: lead.items,
        bin,
        rest,
    }))
}

/// The start of a reply whose args are a bin, or an array whose first item
/// is a bin.
#[derive(Debug, PartialEq)]
struct Lead {
    id: i64,
    /// The items of the args, or `None` when the args are the bin.
    items: Option<usize>,
    /// The offset of the first byte of the bin in the payload.
    at: usize,
    /// The bytes of the bin.
    len: usize,
}

/// Read the start of a payload. Return its `Lead` when it is a reply that
/// `decode` gives as `Message::Reply` without an error, and its args are a
/// bin or an array whose first item is a bin. Return `None` for each other
/// payload, and when `head` ends before the first byte of the bin.
fn lead(head: &[u8]) -> Option<Lead> {
    let mut bytes = head;
    if array_len(&mut bytes)? != 4 {
        return None;
    }
    let mut item = || read_value(&mut bytes).ok();
    let id = item()?.as_i64().filter(|id| *id > 0)?;
    let flags = match item()? {
        Value::Boolean(flag) => i64::from(flag),
        other => other.as_i64()?,
    };
    if flags & ERROR != 0 || item()?.as_str() != Some(".reply") {
        return None;
    }
    let items = match bytes.first()? {
        0xc4..=0xc6 => None,
        _ => Some(array_len(&mut bytes).filter(|items| *items > 0)?),
    };
    let len = bin_len(&mut bytes)?;
    Some(Lead {
        id,
        items,
        at: head.len() - bytes.len(),
        len,
    })
}

/// Read the head of a msgpack array, and return its number of items.
fn array_len(bytes: &mut &[u8]) -> Option<usize> {
    let (&marker, rest) = bytes.split_first()?;
    let (len, size) = match marker {
        0x90..=0x9f => (usize::from(marker & 0x0f), 0),
        0xdc => (be(rest, 2)?, 2),
        0xdd => (be(rest, 4)?, 4),
        _ => return None,
    };
    *bytes = &rest[size..];
    Some(len)
}

/// Read the head of a msgpack bin, and return its number of bytes.
fn bin_len(bytes: &mut &[u8]) -> Option<usize> {
    let (&marker, rest) = bytes.split_first()?;
    let size = match marker {
        0xc4 => 1,
        0xc5 => 2,
        0xc6 => 4,
        _ => return None,
    };
    let len = be(rest, size)?;
    *bytes = &rest[size..];
    Some(len)
}

/// Read the big-endian number in the first `size` bytes of `bytes`.
fn be(bytes: &[u8], size: usize) -> Option<usize> {
    let bytes = bytes.get(..size)?;
    Some(bytes.iter().fold(0, |n, &byte| n << 8 | usize::from(byte)))
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

/// Decode a payload that `read_payload` read. A `Payload::Bin` keeps the
/// buffer of its bin as the bytes of `Value::Binary`.
pub(crate) fn decode_payload(payload: Payload) -> Result<Message, Error> {
    let (id, items, bin, rest) = match payload {
        Payload::Whole(bytes) => return decode(&bytes),
        Payload::Bin {
            id,
            items,
            bin,
            rest,
        } => (id, items, bin, rest),
    };
    let mut bytes = rest.as_slice();
    let args = match items {
        None => Value::Binary(bin),
        Some(items) => {
            // Each item takes at least one byte: a false number of items
            // does not reserve memory.
            let mut values = Vec::with_capacity(items.min(bytes.len() + 1));
            values.push(Value::Binary(bin));
            for _ in 1..items {
                values.push(read_value(&mut bytes)?);
            }
            Value::Array(values)
        }
    };
    if !bytes.is_empty() {
        return Err(Error::Protocol("bytes after the message".into()));
    }
    Ok(Message::Reply {
        id,
        result: Ok(args),
    })
}

/// Read one msgpack value from `bytes`.
fn read_value(bytes: &mut &[u8]) -> Result<Value, Error> {
    rmpv::decode::read_value(bytes)
        .map_err(|e| Error::Protocol(format!("a frame that is not msgpack: {e}")))
}

/// Decode the payload of a frame from the server.
pub(crate) fn decode(payload: &[u8]) -> Result<Message, Error> {
    let mut bytes = payload;
    let value = read_value(&mut bytes)?;
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
        && let Some(Error::Conflict { oid, serials, .. }) = conflict(&value)
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
    Error::Server {
        class,
        message,
        value,
    }
}

/// Return the `POSKeyError` of `oid`, as the server gives it for an oid
/// without a record: one bytes argument, the oid.
pub(crate) fn pos_key_error(oid: Oid) -> Error {
    Error::Server {
        class: "ZODB.POSException.POSKeyError".into(),
        message: format!("{oid:?}"),
        value: Value::Array(vec![bin8(oid.0)]),
    }
}

/// Return `Error::Conflict` from a map with the keys `oid` and `serials`,
/// as an error reply and a vote reply give them. `serials` is
/// `[committed, read]`, or nil. A str `class_name` gives the class of the
/// object.
pub(crate) fn conflict(value: &Value) -> Option<Error> {
    let oid = get(value, "oid").and_then(p64).map(Oid)?;
    let serials = match get(value, "serials") {
        None | Some(Value::Nil) => None,
        Some(Value::Array(items)) if items.len() == 2 => {
            Some((Tid(p64(&items[0])?), Tid(p64(&items[1])?)))
        }
        Some(_) => return None,
    };
    let class_name = get(value, "class_name")
        .and_then(Value::as_str)
        .map(str::to_owned);
    Some(Error::Conflict {
        oid,
        serials,
        class_name,
    })
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
                result: Err(Error::Server { class, value, .. }),
            } => {
                assert_eq!(class, "ZODB.POSException.POSKeyError");
                // The oid stays bytes in the value.
                assert_eq!(value, Value::Array(vec![bin8(1)]));
            }
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
                result:
                    Err(Error::Conflict {
                        oid,
                        serials,
                        class_name,
                    }),
                ..
            } => {
                assert_eq!(oid, Oid(5));
                assert_eq!(serials, Some((Tid(3), Tid(2))));
                assert_eq!(class_name, None);
            }
            other => panic!("{other:?}"),
        }
        // A stock server names the class of the object.
        let named = Value::Map(vec![
            ("message".into(), "database conflict error".into()),
            ("oid".into(), bin8(5)),
            ("class_name".into(), "persistent.Counter".into()),
            ("serials".into(), Value::Array(vec![bin8(3), bin8(2)])),
        ]);
        match decode(&reply(named)).unwrap() {
            Message::Reply {
                result: Err(Error::Conflict { class_name, .. }),
                ..
            } => assert_eq!(class_name.as_deref(), Some("persistent.Counter")),
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
                result: Err(Error::Server { class, message, .. }),
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

    /// Write the frame of `payload` through a pipe of 64 bytes, then read
    /// it with `read_payload`.
    fn read_through_pipe(payload: &[u8]) -> io::Result<Option<Payload>> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            let (mut a, mut b) = tokio::io::duplex(64);
            let write = async {
                write_frame(&mut a, payload).await.unwrap();
                drop(a);
            };
            let read = async {
                let first = read_payload(&mut b).await;
                if let Ok(Some(_)) = &first {
                    assert!(read_payload(&mut b).await.unwrap().is_none());
                }
                first
            };
            tokio::join!(write, read).1
        })
    }

    /// Check that `read_payload` and `decode_payload` give the message that
    /// `decode` gives for `payload`.
    fn decodes_as_whole(payload: &[u8]) -> Payload {
        let read = read_through_pipe(payload).unwrap().unwrap();
        let split = read.clone();
        assert_eq!(
            format!("{:?}", decode_payload(split)),
            format!("{:?}", decode(payload))
        );
        read
    }

    fn large(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 31 + 7) as u8).collect()
    }

    #[test]
    fn the_bin_of_a_long_reply_has_its_own_buffer() {
        let data = large(100 * 1024 + 3);
        for (id, end) in [(3, Value::Nil), (1 << 40, bin8(9))] {
            let payload = encode(Value::Array(vec![
                Value::from(id),
                false.into(),
                ".reply".into(),
                Value::Array(vec![Value::Binary(data.clone()), bin8(8), end.clone()]),
            ]));
            match decodes_as_whole(&payload) {
                Payload::Bin {
                    id: read,
                    items,
                    bin,
                    rest,
                } => {
                    assert_eq!((read, items), (id, Some(3)));
                    assert_eq!(bin, data);
                    assert_eq!(
                        rest,
                        encode(bin8(8))
                            .into_iter()
                            .chain(encode(end))
                            .collect::<Vec<_>>()
                    );
                }
                other => panic!("{other:?}"),
            }
        }
        // The reply of `loadSerial`: the args are the bin.
        let payload = encode(Value::Array(vec![
            5.into(),
            0.into(),
            ".reply".into(),
            Value::Binary(data.clone()),
        ]));
        match decodes_as_whole(&payload) {
            Payload::Bin {
                items, bin, rest, ..
            } => {
                assert_eq!(items, None);
                assert_eq!(bin, data);
                assert!(rest.is_empty());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_bin_reuses_a_buffer_that_came_back() {
        let spares = Spares(Mutex::new(Vec::new()));
        let mut given = Vec::with_capacity(100 * 1024);
        given.extend_from_slice(b"old");
        let at = given.as_ptr();
        spares.give(given);
        // A bin that the buffer holds takes it, empty.
        let taken = spares.take(100 * 1024);
        assert_eq!((taken.as_ptr(), taken.len()), (at, 0));
        // A longer bin gets a new buffer.
        spares.give(taken);
        let longer = spares.take(100 * 1024 + 1);
        assert_ne!(longer.as_ptr(), at);
        assert_eq!(longer.capacity(), 100 * 1024 + 1);
        // A buffer shorter than `SPLIT` or longer than `SPARE_BYTES` drops.
        spares.give(Vec::with_capacity(SPLIT - 1));
        spares.give(Vec::with_capacity(SPARE_BYTES + 1));
        assert_eq!(spares.0.lock().unwrap().len(), 1);
        // At most `SPARES` buffers stay.
        for _ in 0..2 * SPARES {
            spares.give(Vec::with_capacity(SPLIT));
        }
        assert_eq!(spares.0.lock().unwrap().len(), SPARES);
        // While another thread holds the lock, a buffer drops and a new
        // one comes, without a wait.
        let held = spares.0.lock().unwrap();
        let new = std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    spares.give(Vec::with_capacity(SPLIT));
                    spares.take(SPLIT)
                })
                .join()
                .unwrap()
        });
        assert_eq!((held.len(), new.capacity()), (SPARES, SPLIT));
        drop(held);
        // A bin longer than `RESERVE` gets a buffer of `RESERVE` bytes.
        assert_eq!(spares.take(RESERVE + 1).capacity(), RESERVE);
    }

    #[test]
    fn the_decode_of_a_bin_keeps_its_buffer() {
        let bin = vec![3u8; 100];
        let at = bin.as_ptr();
        let payload = Payload::Bin {
            id: 2,
            items: Some(2),
            bin,
            rest: encode(Value::Nil),
        };
        match decode_payload(payload).unwrap() {
            Message::Reply {
                id: 2,
                result: Ok(Value::Array(items)),
            } => {
                assert!(matches!(&items[0], Value::Binary(data) if data.as_ptr() == at));
                assert_eq!(items[1], Value::Nil);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_short_bin_in_a_long_reply_ends_inside_the_start() {
        let payload = encode(Value::Array(vec![
            5.into(),
            false.into(),
            ".reply".into(),
            Value::Array(vec![
                Value::Binary(b"abc".to_vec()),
                Value::Binary(large(SPLIT)),
            ]),
        ]));
        match decodes_as_whole(&payload) {
            Payload::Bin { items, bin, .. } => {
                assert_eq!(items, Some(2));
                assert_eq!(bin, b"abc");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn other_long_payloads_are_whole() {
        let data = Value::Binary(large(SPLIT));
        for message in [
            // A call of the server.
            vec![
                0.into(),
                true.into(),
                "receiveBlobChunk".into(),
                Value::Array(vec![data.clone()]),
            ],
            // An error reply.
            vec![
                1.into(),
                2.into(),
                ".reply".into(),
                Value::Array(vec!["E".into(), data.clone()]),
            ],
            // An id that the client never gives.
            vec![
                0.into(),
                false.into(),
                ".reply".into(),
                Value::Array(vec![data.clone()]),
            ],
            // Args whose first item is not a bin.
            vec![
                1.into(),
                false.into(),
                ".reply".into(),
                Value::Array(vec![Value::Nil, data.clone()]),
            ],
            // An empty array in the args.
            vec![
                1.into(),
                false.into(),
                ".reply".into(),
                Value::Array(vec![Value::Array(vec![]), data.clone()]),
            ],
            // Flags that are not a bool or an int.
            vec![
                1.into(),
                Value::Nil,
                ".reply".into(),
                Value::Array(vec![data.clone()]),
            ],
        ] {
            let payload = encode(Value::Array(message));
            assert!(matches!(decodes_as_whole(&payload), Payload::Whole(_)));
        }
        // A short reply is one read.
        let payload = encode(Value::Array(vec![
            1.into(),
            false.into(),
            ".reply".into(),
            Value::Array(vec![Value::Binary(vec![1; 100])]),
        ]));
        assert!(matches!(decodes_as_whole(&payload), Payload::Whole(_)));
    }

    #[test]
    fn a_bad_long_reply_is_a_protocol_error() {
        let reply = |args: Value| {
            encode(Value::Array(vec![
                1.into(),
                false.into(),
                ".reply".into(),
                args,
            ]))
        };
        let data = large(SPLIT);
        // An array with more items than the payload holds.
        let mut short = reply(Value::Array(vec![Value::Binary(data.clone()), Value::Nil]));
        short.pop();
        let bad = [
            // Bytes after the message.
            [reply(Value::Binary(data.clone())), vec![0]].concat(),
            [
                reply(Value::Array(vec![Value::Binary(data.clone())])),
                vec![0],
            ]
            .concat(),
            short,
        ];
        for payload in bad {
            let read = read_through_pipe(&payload).unwrap().unwrap();
            assert!(matches!(decode_payload(read), Err(Error::Protocol(_))));
        }
        // A bin longer than the payload is a whole payload, and not msgpack.
        let mut payload = reply(Value::Binary(data));
        payload.truncate(payload.len() - 1);
        let read = read_through_pipe(&payload).unwrap().unwrap();
        assert!(matches!(read, Payload::Whole(_)));
        assert!(matches!(decode_payload(read), Err(Error::Protocol(_))));
    }

    #[test]
    fn an_end_inside_a_long_frame_fails() {
        let payload = encode(Value::Array(vec![
            1.into(),
            false.into(),
            ".reply".into(),
            Value::Array(vec![Value::Binary(large(SPLIT)), bin8(1), Value::Nil]),
        ]));
        let frame = frame(&payload).unwrap();
        // Cut inside the start, inside the bin, and inside the rest.
        for cut in [4 + PEEK / 2, 4 + PEEK + 10, frame.len() - 1] {
            let rt = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap();
            let error = rt.block_on(read_payload(&mut &frame[..cut])).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
        }
    }

    #[test]
    fn the_start_of_a_reply_gives_its_bin() {
        let head = |args: &[u8]| {
            let mut bytes = encode(Value::Array(vec![
                7.into(),
                false.into(),
                ".reply".into(),
                Value::Nil,
            ]));
            bytes.pop();
            bytes.extend_from_slice(args);
            bytes
        };
        // An array of 16 bits, then a bin of 32 bits.
        let bytes = head(b"\xdc\x00\x03\xc6\x00\x01\x00\x00");
        assert_eq!(
            lead(&bytes),
            Some(Lead {
                id: 7,
                items: Some(3),
                at: bytes.len(),
                len: 1 << 16,
            })
        );
        // A bin of 8 bits as the args.
        let bytes = head(b"\xc4\x05");
        assert_eq!(
            lead(&bytes),
            Some(Lead {
                id: 7,
                items: None,
                at: bytes.len(),
                len: 5,
            })
        );
        // A start that ends before the length of the bin.
        assert_eq!(lead(&head(b"\x93\xc5\x01")), None);
        assert_eq!(lead(&head(b"\x93")), None);
        assert_eq!(lead(b"\x94\x07"), None);
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
