//! The frames and keys of the store, as the core writes them: a 32-byte
//! header, the xxh3-64 checksum over the key-space tag, the key, the
//! header and the payload, then the payload, zstd at level 3 when it is
//! smaller.
use std::borrow::Cow;
use std::cell::RefCell;

use xxhash_rust::xxh3::Xxh3;
use zstd::bulk::{Compressor, Decompressor};

pub const HEADER_LEN: usize = 32;
pub const TAG_RECORDS: u8 = 1;
pub const TAG_TXNS: u8 = 3;

thread_local! {
    static COMPRESSOR: RefCell<Option<Compressor<'static>>> = const { RefCell::new(None) };
    static DECOMPRESSOR: RefCell<Option<Decompressor<'static>>> = const { RefCell::new(None) };
}

fn zstd_smaller(data: &[u8]) -> Option<Vec<u8>> {
    let mut packed = Vec::with_capacity(data.len().checked_sub(1)?);
    COMPRESSOR.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            let mut c = Compressor::new(3).ok()?;
            c.include_contentsize(true).ok()?;
            *slot = Some(c);
        }
        slot.as_mut()?.compress_to_buffer(data, &mut packed).ok()
    })?;
    (packed.len() < data.len()).then_some(packed)
}

/// Encode the payload of a record: zstd when it is smaller, else the
/// data as it is. Return the codec and the payload.
pub fn encode_payload(data: &[u8], compress: bool) -> (u8, Cow<'_, [u8]>) {
    match compress.then(|| zstd_smaller(data)).flatten() {
        Some(packed) => (1, Cow::Owned(packed)),
        None => (0, Cow::Borrowed(data)),
    }
}

pub fn encode_frame(tag: u8, key: &[u8], prev: u64, data: &[u8], compress: bool) -> Vec<u8> {
    let (codec, payload) = encode_payload(data, compress);
    frame_of_payload(tag, key, prev, codec, &payload)
}

/// Frame an encoded payload: the header, the checksum, the payload.
pub fn frame_of_payload(tag: u8, key: &[u8], prev: u64, codec: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(HEADER_LEN + payload.len());
    frame.extend_from_slice(&[1, 0, codec, 0]);
    frame.extend_from_slice(&0u32.to_le_bytes());
    frame.extend_from_slice(&prev.to_le_bytes());
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(&0u16.to_le_bytes());
    frame.extend_from_slice(&0u16.to_le_bytes());
    let mut h = Xxh3::new();
    h.update(&[tag]);
    h.update(key);
    h.update(&frame[..24]);
    h.update(payload);
    frame.extend_from_slice(&h.digest().to_le_bytes());
    frame.extend_from_slice(payload);
    frame
}

/// Verify a frame and decode its payload.
pub fn decode_frame(tag: u8, key: &[u8], bytes: &[u8]) -> Vec<u8> {
    let (header, payload) = bytes.split_at(HEADER_LEN);
    let mut h = Xxh3::new();
    h.update(&[tag]);
    h.update(key);
    h.update(&header[..24]);
    h.update(payload);
    let stored = u64::from_le_bytes(header[24..32].try_into().unwrap());
    assert_eq!(h.digest(), stored, "a frame fails its checksum");
    if header[2] == 1 {
        let len = zstd::zstd_safe::get_frame_content_size(payload)
            .ok()
            .flatten()
            .expect("a zstd frame states its length") as usize;
        let mut out = Vec::with_capacity(len);
        DECOMPRESSOR.with(|slot| {
            let mut slot = slot.borrow_mut();
            if slot.is_none() {
                *slot = Some(Decompressor::new().unwrap());
            }
            slot.as_mut().unwrap().decompress_to_buffer(payload, &mut out).unwrap();
        });
        out
    } else {
        payload.to_vec()
    }
}

pub fn record_key(oid: u64, tid: u64) -> [u8; 16] {
    let mut key = [0; 16];
    key[..8].copy_from_slice(&oid.to_be_bytes());
    key[8..].copy_from_slice(&tid.to_be_bytes());
    key
}

pub fn txn_oid_key(tid: u64, oid: u64) -> [u8; 16] {
    let mut key = [0; 16];
    key[..8].copy_from_slice(&tid.to_be_bytes());
    key[8..].copy_from_slice(&oid.to_be_bytes());
    key
}

/// The payload of a transaction row: the status, the oid count, then the
/// user, the description and the extension with their lengths.
pub fn txn_payload(oid_count: u64, user: &[u8], description: &[u8]) -> Vec<u8> {
    let mut payload = vec![1u8];
    payload.extend_from_slice(&oid_count.to_le_bytes());
    for field in [user, description, &[][..]] {
        payload.extend_from_slice(&(field.len() as u32).to_le_bytes());
        payload.extend_from_slice(field);
    }
    payload
}

/// The record of `oid` in its `version`, of `size` bytes: half text that
/// zstd shrinks, half pseudo-random bytes, as a zodbshootout record.
pub fn record(oid: u64, version: u64, size: usize) -> Vec<u8> {
    let mut data = Vec::with_capacity(size);
    data.extend_from_slice(b"\x80\x03czodbshootout.speedtest\nPObject\nq\x00.\x80\x03}q\x01(");
    while data.len() < size / 2 {
        data.extend_from_slice(b"X\x04\x00\x00\x00attrq\x02K\x01X\x04\x00\x00\x00dataq\x03");
    }
    let mut state = oid ^ (version << 32) ^ 0x9e37_79b9_7f4a_7c15;
    while data.len() < size {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        data.extend_from_slice(&state.to_le_bytes());
    }
    data.truncate(size);
    data
}
