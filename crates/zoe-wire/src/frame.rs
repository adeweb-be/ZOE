//! The frames of the ZEO protocol: a 4-byte big-endian length, then the
//! payload.
//!
//! The read loops stay with the callers. The server refuses a frame longer
//! than its limit, and the client reads a long reply in two parts. Both
//! read the length with [`length`] and reserve at most [`RESERVE`] bytes
//! before the bytes of a payload arrive.

use std::fmt;

/// The largest buffer that a read reserves before the bytes arrive. A
/// longer frame grows its buffer as its bytes arrive, so a false length
/// does not reserve memory.
pub const RESERVE: usize = 1 << 20;

/// A payload of more than `u32::MAX` bytes. No frame holds it. It keeps
/// the length of the payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TooLong(pub usize);

impl fmt::Display for TooLong {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "a payload of {} bytes is too long for a frame", self.0)
    }
}

impl std::error::Error for TooLong {}

/// Return the 4 bytes of the length of a frame whose payload has `len`
/// bytes.
pub fn header(len: usize) -> Result<[u8; 4], TooLong> {
    u32::try_from(len)
        .map(u32::to_be_bytes)
        .map_err(|_| TooLong(len))
}

/// Return the frame of `payload`: its length, then its bytes.
pub fn encode(payload: &[u8]) -> Result<Vec<u8>, TooLong> {
    let header = header(payload.len())?;
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&header);
    frame.extend_from_slice(payload);
    Ok(frame)
}

/// Return the length of the payload that the 4 bytes `header` give.
pub fn length(header: [u8; 4]) -> u32 {
    u32::from_be_bytes(header)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_is_the_length_then_the_payload() {
        assert_eq!(encode(b"M5").unwrap(), b"\0\0\0\x02M5");
        assert_eq!(encode(b"").unwrap(), b"\0\0\0\0");
        assert_eq!(length(*b"\x01\x02\x03\x04"), 0x0102_0304);
        assert_eq!(header(300).unwrap(), [0, 0, 1, 44]);
    }

    #[test]
    fn a_payload_longer_than_u32_max_has_no_header() {
        let len = u32::MAX as usize + 1;
        assert_eq!(header(len), Err(TooLong(len)));
        assert_eq!(header(u32::MAX as usize).unwrap(), [0xff; 4]);
    }
}
