//! The bytes and the names of the ZEO 6 msgpack protocol that `zoe serve`
//! and its clients share (ADR 0055).
//!
//! The crate is sans-IO. It reads and writes bytes in memory. It opens no
//! socket, reads no stream and has no async function. The read loops of
//! the frames stay in the server and in the client, because their rules
//! differ.
//!
//! - [`frame`]: the length of a frame, and the encode of a frame.
//! - [`msgpack`]: the heads of a bin and of an array.
//! - [`class`]: the class names of the exceptions that a reply names.
//! - [`extension`]: the names of the ZOE extensions.

pub mod class;
pub mod extension;
pub mod frame;
pub mod msgpack;

use rmpv::Value;

/// The protocol of ZEO 6 with msgpack. Each side sends it first, as the
/// raw bytes of a frame.
pub const PROTOCOL: &[u8] = b"M5";

/// The flags of a reply that carries an error.
pub const ERROR: u8 = 2;

/// The message id of a heartbeat. It gets no reply.
pub const HEARTBEAT: i64 = -1;

/// The method of a reply.
pub const REPLY: &str = ".reply";

/// The key of the W3C traceparent in the trace context of a call.
pub const TRACEPARENT: &str = "traceparent";

/// The tid that a load of the current record gives to `loadBefore`, as
/// `ZEO.ClientStorage` does: `ZODB.utils.maxtid`. It is not `Tid::MAX` of
/// the core.
pub const MAX_TID: u64 = i64::MAX as u64;

/// Return an oid or a tid as the protocol sends it: a bin of 8 big-endian
/// bytes, as `ZODB.utils.p64` gives them.
pub fn p64(n: u64) -> Value {
    Value::Binary(n.to_be_bytes().to_vec())
}

/// Return the number in the 8 big-endian bytes of a bin or a str, as
/// `ZODB.utils.u64` reads them. Return `None` for another value.
pub fn u64_of(value: &Value) -> Option<u64> {
    let bytes: [u8; 8] = value.as_slice()?.try_into().ok()?;
    Some(u64::from_be_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn p64_and_u64_of_give_back_the_number() {
        for n in [0, 1, 0x0102_0304_0506_0708, MAX_TID, u64::MAX] {
            assert_eq!(u64_of(&p64(n)), Some(n));
        }
        assert_eq!(p64(1), Value::Binary(vec![0, 0, 0, 0, 0, 0, 0, 1]));
    }

    #[test]
    fn u64_of_reads_a_bin_or_a_str_of_8_bytes_only() {
        assert_eq!(u64_of(&Value::from("\0\0\0\0\0\0\0\x07")), Some(7));
        for value in [
            Value::Binary(vec![1; 7]),
            Value::Binary(vec![1; 9]),
            Value::from("seven"),
            Value::from(7),
            Value::Nil,
        ] {
            assert_eq!(u64_of(&value), None, "{value:?}");
        }
    }
}
