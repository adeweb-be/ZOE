//! The helpers that the measurements of the core share.

use zoe_core::Oid;

/// The record of `oid` in its `version`, of `size` bytes. The first half
/// is text that zstd compresses well, the second half is pseudo-random
/// bytes that it does not, as the records of zodbshootout.
pub fn speedtest_record(oid: Oid, version: u64, size: usize) -> Vec<u8> {
    let mut data = Vec::with_capacity(size);
    data.extend_from_slice(b"\x80\x03czodbshootout.speedtest\nPObject\nq\x00.\x80\x03}q\x01(");
    while data.len() < size / 2 {
        data.extend_from_slice(b"X\x04\x00\x00\x00attrq\x02K\x01X\x04\x00\x00\x00dataq\x03");
    }
    let mut state = oid.0 ^ (version << 32) ^ 0x9e37_79b9_7f4a_7c15;
    while data.len() < size {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        data.extend_from_slice(&state.to_le_bytes());
    }
    data.truncate(size);
    data
}
