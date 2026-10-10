//! The on-disk format: record framing, checksums and the store header.
//! This module is a red zone.
//!
//! Every record and every transaction row is a frame: a 32-byte header,
//! the typed-field slots, then the payload. The header fields are
//! little-endian:
//!
//! | Offset | Size | Field |
//! | --- | --- | --- |
//! | 0 | 1 | frame version |
//! | 1 | 1 | flags: compressed, encrypted |
//! | 2 | 1 | codec: 0 none, 1 zstd, 2 zstd with the store dictionary |
//! | 3 | 1 | reserved, 0 |
//! | 4 | 4 | branch id |
//! | 8 | 8 | previous serial |
//! | 16 | 4 | payload length |
//! | 20 | 2 | length of the typed-field slots |
//! | 22 | 2 | reserved, 0 |
//! | 24 | 8 | xxh3-64 of bytes 0 to 23, the slots and the payload |
//!
//! Keys are big-endian, so that the byte order of the keys is the numeric
//! order of oids and tids. The numbers in the rows of `meta` are 8-byte
//! big-endian values, as ZODB writes oids and tids.

use xxhash_rust::xxh3::Xxh3;

use crate::backend::{TxnMeta, TxnStatus};
use crate::types::{Oid, Tid};

/// The name and version of the format, in the `format` row of `meta`.
pub const FORMAT_NAME: &[u8] = b"ZOE format 1";

/// The version of the frame header that this code writes and reads.
pub const FRAME_VERSION: u8 = 1;

/// The length of the frame header.
pub const HEADER_LEN: usize = 32;

/// The branch id of the main store. Forks use other ids.
pub const MAIN_BRANCH: u32 = 0;

/// The codec that stores the payload as it is.
pub const CODEC_NONE: u8 = 0;

/// Why a frame cannot be read.
#[derive(Debug, PartialEq, Eq)]
pub enum FrameError {
    /// The bytes are shorter than the header or than the lengths it gives.
    Truncated,
    /// The frame version is not one that this code reads.
    Version(u8),
    /// The checksum does not match the bytes.
    Checksum,
}

/// A frame that was read and checked.
#[derive(Debug, PartialEq, Eq)]
pub struct Frame<'a> {
    /// The flags byte.
    pub flags: u8,
    /// The codec byte.
    pub codec: u8,
    /// The branch id.
    pub branch: u32,
    /// The previous serial.
    pub prev: Tid,
    /// The typed-field slots.
    pub slots: &'a [u8],
    /// The payload.
    pub payload: &'a [u8],
}

/// Encode a frame with no flags, no codec and no typed-field slots.
pub fn encode_frame(branch: u32, prev: Tid, payload: &[u8]) -> Vec<u8> {
    let payload_len = u32::try_from(payload.len()).expect("a payload fits in 4 GiB");
    let mut frame = Vec::with_capacity(HEADER_LEN + payload.len());
    frame.push(FRAME_VERSION);
    frame.push(0);
    frame.push(CODEC_NONE);
    frame.push(0);
    frame.extend_from_slice(&branch.to_le_bytes());
    frame.extend_from_slice(&prev.0.to_le_bytes());
    frame.extend_from_slice(&payload_len.to_le_bytes());
    frame.extend_from_slice(&0u16.to_le_bytes());
    frame.extend_from_slice(&0u16.to_le_bytes());
    let checksum = checksum(&frame[..24], &[], payload);
    frame.extend_from_slice(&checksum.to_le_bytes());
    frame.extend_from_slice(payload);
    frame
}

/// Read a frame and verify its checksum.
pub fn decode_frame(bytes: &[u8]) -> Result<Frame<'_>, FrameError> {
    let header = bytes.get(..HEADER_LEN).ok_or(FrameError::Truncated)?;
    if header[0] != FRAME_VERSION {
        return Err(FrameError::Version(header[0]));
    }
    let payload_len = u32::from_le_bytes(header[16..20].try_into().unwrap()) as usize;
    let slots_len = usize::from(u16::from_le_bytes(header[20..22].try_into().unwrap()));
    let body = &bytes[HEADER_LEN..];
    if body.len() != slots_len + payload_len {
        return Err(FrameError::Truncated);
    }
    let (slots, payload) = body.split_at(slots_len);
    let stored = u64::from_le_bytes(header[24..32].try_into().unwrap());
    if checksum(&header[..24], slots, payload) != stored {
        return Err(FrameError::Checksum);
    }
    Ok(Frame {
        flags: header[1],
        codec: header[2],
        branch: u32::from_le_bytes(header[4..8].try_into().unwrap()),
        prev: Tid(u64::from_le_bytes(header[8..16].try_into().unwrap())),
        slots,
        payload,
    })
}

fn checksum(header: &[u8], slots: &[u8], payload: &[u8]) -> u64 {
    let mut hasher = Xxh3::new();
    hasher.update(header);
    hasher.update(slots);
    hasher.update(payload);
    hasher.digest()
}

/// Return the key of a record: the oid, then the tid, both big-endian.
pub fn record_key(oid: Oid, tid: Tid) -> [u8; 16] {
    let mut key = [0; 16];
    key[..8].copy_from_slice(&oid.0.to_be_bytes());
    key[8..].copy_from_slice(&tid.0.to_be_bytes());
    key
}

/// Split a record key into its oid and tid.
pub fn split_record_key(key: &[u8; 16]) -> (Oid, Tid) {
    let oid = u64::from_be_bytes(key[..8].try_into().unwrap());
    let tid = u64::from_be_bytes(key[8..].try_into().unwrap());
    (Oid(oid), Tid(tid))
}

/// Return the key of a transaction row: the tid, big-endian.
pub fn txn_key(tid: Tid) -> [u8; 8] {
    tid.0.to_be_bytes()
}

/// Encode a number for a row of `meta`.
pub fn encode_u64(n: u64) -> [u8; 8] {
    n.to_be_bytes()
}

/// Decode a number of a row of `meta`.
pub fn decode_u64(bytes: &[u8]) -> Option<u64> {
    Some(u64::from_be_bytes(bytes.try_into().ok()?))
}

/// Encode the payload of a transaction row: the status byte, the oid
/// count, then the user, the description and the extension, each with a
/// 4-byte length. Numbers are little-endian.
pub fn encode_txn(meta: &TxnMeta) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.push(match meta.status {
        TxnStatus::Committing => 0,
        TxnStatus::Committed => 1,
    });
    payload.extend_from_slice(&meta.oid_count.to_le_bytes());
    for field in [&meta.user, &meta.description, &meta.extension] {
        let len = u32::try_from(field.len()).expect("a field fits in 4 GiB");
        payload.extend_from_slice(&len.to_le_bytes());
        payload.extend_from_slice(field);
    }
    payload
}

/// Decode the payload of the transaction row of `tid`.
pub fn decode_txn(tid: Tid, payload: &[u8]) -> Result<TxnMeta, FrameError> {
    let status = match payload.first() {
        Some(0) => TxnStatus::Committing,
        Some(1) => TxnStatus::Committed,
        _ => return Err(FrameError::Truncated),
    };
    let mut rest = &payload[1..];
    let oid_count = u64::from_le_bytes(take(&mut rest, 8)?.try_into().unwrap());
    let mut fields = [Vec::new(), Vec::new(), Vec::new()];
    for field in &mut fields {
        let len = u32::from_le_bytes(take(&mut rest, 4)?.try_into().unwrap()) as usize;
        *field = take(&mut rest, len)?.to_vec();
    }
    if !rest.is_empty() {
        return Err(FrameError::Truncated);
    }
    let [user, description, extension] = fields;
    Ok(TxnMeta {
        tid,
        user,
        description,
        extension,
        oid_count,
        status,
    })
}

fn take<'a>(rest: &mut &'a [u8], n: usize) -> Result<&'a [u8], FrameError> {
    if rest.len() < n {
        return Err(FrameError::Truncated);
    }
    let (head, tail) = rest.split_at(n);
    *rest = tail;
    Ok(head)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_round_trips() {
        let frame = encode_frame(MAIN_BRANCH, Tid(7), b"pickle");
        assert_eq!(frame.len(), HEADER_LEN + 6);
        let read = decode_frame(&frame).unwrap();
        assert_eq!(read.prev, Tid(7));
        assert_eq!(read.branch, MAIN_BRANCH);
        assert_eq!(read.codec, CODEC_NONE);
        assert_eq!(read.payload, b"pickle");
        assert!(read.slots.is_empty());
    }

    #[test]
    fn every_flipped_bit_is_detected() {
        let frame = encode_frame(MAIN_BRANCH, Tid(7), b"pickle");
        for byte in 0..frame.len() {
            for bit in 0..8 {
                let mut bad = frame.clone();
                bad[byte] ^= 1 << bit;
                assert!(decode_frame(&bad).is_err(), "byte {byte} bit {bit}");
            }
        }
    }

    #[test]
    fn a_short_frame_is_truncated() {
        let frame = encode_frame(MAIN_BRANCH, Tid(7), b"pickle");
        assert_eq!(decode_frame(&frame[..10]), Err(FrameError::Truncated));
        assert_eq!(
            decode_frame(&frame[..frame.len() - 1]),
            Err(FrameError::Truncated)
        );
    }

    #[test]
    fn keys_sort_by_oid_then_tid() {
        let a = record_key(Oid(1), Tid(u64::MAX));
        let b = record_key(Oid(2), Tid(0));
        assert!(a < b);
        assert_eq!(split_record_key(&b), (Oid(2), Tid(0)));
    }

    #[test]
    fn a_transaction_row_round_trips() {
        let meta = TxnMeta {
            tid: Tid(9),
            user: b"user".to_vec(),
            description: b"description".to_vec(),
            extension: Vec::new(),
            oid_count: 3,
            status: TxnStatus::Committed,
        };
        assert_eq!(decode_txn(Tid(9), &encode_txn(&meta)), Ok(meta));
    }
}
