//! The helpers that the tests of zoe-bench share. Each test binary uses
//! some of them.

#![allow(dead_code)]

use zoe_core::Oid;
use zoe_core::testing::TempDir;

/// Return a new, empty directory for the test `name` of this test binary.
/// The drop of the directory removes it, except when the test panics.
pub fn fresh_dir(name: &str) -> TempDir {
    zoe_core::testing::fresh_dir(&format!("bench-{name}"))
}

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
