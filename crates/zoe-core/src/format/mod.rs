//! The on-disk format: the frame and its checksum, the keys, and the
//! values of the transaction rows and the `meta` rows. This module is a
//! red zone.
//!
//! Every record and every transaction row is a frame: a 32-byte header,
//! the typed-field slots, then the payload. The header fields are
//! little-endian:
//!
//! | Offset | Size | Field |
//! | --- | --- | --- |
//! | 0 | 1 | frame version |
//! | 1 | 1 | flags, 0: no flag is defined |
//! | 2 | 1 | codec: 0 none, 1 zstd (see `codec`) |
//! | 3 | 1 | reserved, 0 |
//! | 4 | 4 | branch id |
//! | 8 | 8 | previous serial |
//! | 16 | 4 | payload length, as stored |
//! | 20 | 2 | length of the typed-field slots |
//! | 22 | 2 | reserved, 0 |
//! | 24 | 8 | checksum |
//!
//! The checksum is the xxh3-64 hash, with the default seed 0, of these
//! bytes in this order: the key of the row, header bytes 0 to 23, the
//! slots, then the payload. The key of a record is the 16 bytes of
//! `record_key`. The key of a transaction row is the 8 bytes of
//! `txn_key`. So a frame that is read under another key fails the check.
//!
//! The payload holds the data in the form that the codec byte gives. The
//! checksum covers the payload as stored, so a read verifies the stored
//! bytes before it decodes them. `decode_frame` decodes every codec, so
//! each read path gets the data, never the stored form.
//!
//! Keys are big-endian, so that the byte order of the keys is the numeric
//! order of oids and tids. The numbers in the rows of `meta` are 8-byte
//! big-endian values, as ZODB writes oids and tids.

use std::borrow::Cow;

use xxhash_rust::xxh3::Xxh3;

use crate::backend::{TxnMeta, TxnStatus};
use crate::types::{Oid, Tid};

mod codec;
#[cfg(test)]
mod synthetic;

pub use codec::{Codec, Compression};

/// The name and version of the format, in the `format` row of `meta`.
pub const FORMAT_NAME: &[u8] = b"ZOE format 1";

/// The version of the frame header that this code writes and reads.
pub const FRAME_VERSION: u8 = 1;

/// The length of the frame header.
pub const HEADER_LEN: usize = 32;

/// The branch id of the main store. Forks use other ids.
pub const MAIN_BRANCH: u32 = 0;

/// The codec byte of a payload that holds the data as it is.
pub const CODEC_NONE: u8 = 0;

/// The codec byte of a payload that is one zstd frame.
pub const CODEC_ZSTD: u8 = 1;

/// Why a frame cannot be read.
#[derive(Debug, PartialEq, Eq)]
pub enum FrameError {
    /// The bytes are shorter than the header or than the lengths it gives.
    Truncated,
    /// The frame version is not one that this code reads.
    Version(u8),
    /// The checksum does not match the key and the bytes.
    Checksum,
    /// The header byte at `offset` is a flag, a codec or a reserved byte
    /// with a value that this code cannot read. A newer format wrote the
    /// frame.
    Unsupported {
        /// The offset of the byte in the header.
        offset: usize,
        /// The value of the byte.
        value: u8,
    },
    /// The payload does not decode with its codec. For zstd, the payload is
    /// not one zstd frame, does not state the data length, states a length
    /// over the bound, or decodes to another length.
    Payload,
}

/// A frame that was read, checked and decoded.
#[derive(Debug, PartialEq, Eq)]
pub struct Frame<'a> {
    /// The flags byte.
    pub flags: u8,
    /// The codec of the payload.
    pub codec: Codec,
    /// The branch id.
    pub branch: u32,
    /// The previous serial.
    pub prev: Tid,
    /// The typed-field slots.
    pub slots: &'a [u8],
    /// The length of the payload as stored, before it is decoded.
    pub stored_len: usize,
    /// The data: the payload, decoded with its codec.
    pub data: Cow<'a, [u8]>,
}

/// Encode the frame of the row `key`, with the data as it is, no flags and
/// no typed-field slots.
pub fn encode_frame(key: &[u8], branch: u32, prev: Tid, data: &[u8]) -> Vec<u8> {
    encode_frame_with(key, branch, prev, data, Compression::None)
}

/// Encode the frame of the row `key`, with no flags and no typed-field
/// slots. `compression` selects the codec of the payload.
pub fn encode_frame_with(
    key: &[u8],
    branch: u32,
    prev: Tid,
    data: &[u8],
    compression: Compression,
) -> Vec<u8> {
    // The reader refuses a longer data length, for every codec.
    assert!(
        u32::try_from(data.len()).is_ok(),
        "the data of a frame fits in 4 GiB"
    );
    let (codec, payload) = codec::encode(data, compression);
    let payload_len = u32::try_from(payload.len()).expect("a payload fits in 4 GiB");
    let mut frame = Vec::with_capacity(HEADER_LEN + payload.len());
    frame.push(FRAME_VERSION);
    frame.push(0);
    frame.push(codec.byte());
    frame.push(0);
    frame.extend_from_slice(&branch.to_le_bytes());
    frame.extend_from_slice(&prev.0.to_le_bytes());
    frame.extend_from_slice(&payload_len.to_le_bytes());
    frame.extend_from_slice(&0u16.to_le_bytes());
    frame.extend_from_slice(&0u16.to_le_bytes());
    let checksum = checksum(key, &frame[..24], &[], &payload);
    frame.extend_from_slice(&checksum.to_le_bytes());
    frame.extend_from_slice(&payload);
    frame
}

/// Read the frame of the row `key`, verify its checksum and decode its
/// payload.
///
/// A frame with a flag, a codec that this code does not know or a non-zero
/// reserved byte is refused. The typed-field slots are opaque, so slots
/// of any length are accepted.
pub fn decode_frame<'a>(key: &[u8], bytes: &'a [u8]) -> Result<Frame<'a>, FrameError> {
    let header = bytes.get(..HEADER_LEN).ok_or(FrameError::Truncated)?;
    // This check comes before the checksum. A newer version and a damaged
    // version byte look the same here, so both are a corrupt frame.
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
    if checksum(key, &header[..24], slots, payload) != stored {
        return Err(FrameError::Checksum);
    }
    for offset in [1, 3, 22, 23] {
        if header[offset] != 0 {
            return Err(FrameError::Unsupported {
                offset,
                value: header[offset],
            });
        }
    }
    let codec = Codec::from_byte(header[2]).ok_or(FrameError::Unsupported {
        offset: 2,
        value: header[2],
    })?;
    Ok(Frame {
        flags: header[1],
        codec,
        branch: u32::from_le_bytes(header[4..8].try_into().unwrap()),
        prev: Tid(u64::from_le_bytes(header[8..16].try_into().unwrap())),
        slots,
        stored_len: payload.len(),
        data: codec::decode(codec, payload)?,
    })
}

fn checksum(key: &[u8], header: &[u8], slots: &[u8], payload: &[u8]) -> u64 {
    let mut hasher = Xxh3::new();
    hasher.update(key);
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

    /// The key of the record of oid 1 at tid 9.
    fn key() -> [u8; 16] {
        record_key(Oid(1), Tid(9))
    }

    #[test]
    fn a_frame_round_trips() {
        let frame = encode_frame(&key(), MAIN_BRANCH, Tid(7), b"pickle");
        assert_eq!(frame.len(), HEADER_LEN + 6);
        let read = decode_frame(&key(), &frame).unwrap();
        assert_eq!(read.prev, Tid(7));
        assert_eq!(read.branch, MAIN_BRANCH);
        assert_eq!(read.codec, Codec::None);
        assert_eq!(read.stored_len, 6);
        assert_eq!(&read.data[..], b"pickle");
        assert!(read.slots.is_empty());
    }

    #[test]
    fn every_flipped_bit_is_detected() {
        let frame = encode_frame(&key(), MAIN_BRANCH, Tid(7), b"pickle");
        for byte in 0..frame.len() {
            for bit in 0..8 {
                let mut bad = frame.clone();
                bad[byte] ^= 1 << bit;
                assert!(decode_frame(&key(), &bad).is_err(), "byte {byte} bit {bit}");
            }
        }
    }

    #[test]
    fn every_flipped_bit_of_the_key_is_detected() {
        let frame = encode_frame(&key(), MAIN_BRANCH, Tid(7), b"pickle");
        for byte in 0..key().len() {
            for bit in 0..8 {
                let mut bad = key();
                bad[byte] ^= 1 << bit;
                assert_eq!(
                    decode_frame(&bad, &frame),
                    Err(FrameError::Checksum),
                    "byte {byte} bit {bit}"
                );
            }
        }
    }

    #[test]
    fn a_frame_under_another_oid_or_tid_is_refused() {
        let frame = encode_frame(&key(), MAIN_BRANCH, Tid(7), b"pickle");
        for (oid, tid) in [(Oid(2), Tid(9)), (Oid(1), Tid(10))] {
            assert_eq!(
                decode_frame(&record_key(oid, tid), &frame),
                Err(FrameError::Checksum),
                "{oid:?} {tid:?}"
            );
        }
    }

    #[test]
    fn a_transaction_row_frame_covers_its_tid() {
        let frame = encode_frame(&txn_key(Tid(9)), MAIN_BRANCH, Tid::ZERO, b"row");
        assert!(decode_frame(&txn_key(Tid(9)), &frame).is_ok());
        assert_eq!(
            decode_frame(&txn_key(Tid(10)), &frame),
            Err(FrameError::Checksum)
        );
    }

    /// Build a frame from the first 24 header bytes, the slots and the
    /// payload, with a valid checksum.
    fn seal(header: &[u8], slots: &[u8], payload: &[u8]) -> Vec<u8> {
        let mut frame = header[..24].to_vec();
        frame.extend_from_slice(&checksum(&key(), &frame, slots, payload).to_le_bytes());
        frame.extend_from_slice(slots);
        frame.extend_from_slice(payload);
        frame
    }

    #[test]
    fn a_frame_of_a_newer_format_is_refused() {
        let frame = encode_frame(&key(), MAIN_BRANCH, Tid(7), b"pickle");
        for (offset, value) in [(1, 1), (2, 2), (2, 0xff), (3, 1), (22, 1), (23, 1)] {
            let mut header = frame[..24].to_vec();
            header[offset] = value;
            assert_eq!(
                decode_frame(&key(), &seal(&header, &[], b"pickle")),
                Err(FrameError::Unsupported { offset, value }),
                "offset {offset}"
            );
        }
    }

    /// A pickle with the repetition of a content object: the same keys
    /// many times.
    fn pickle() -> Vec<u8> {
        b"X\x05\x00\x00\x00titleq\x02X\x0b\x00\x00\x00descriptionq\x03".repeat(100)
    }

    /// The frame of `pickle()` under `key()`, compressed with zstd.
    fn compressed() -> Vec<u8> {
        let frame = encode_frame_with(
            &key(),
            MAIN_BRANCH,
            Tid(7),
            &pickle(),
            Compression::default(),
        );
        assert_eq!(frame[2], CODEC_ZSTD);
        frame
    }

    #[test]
    fn a_compressed_frame_round_trips() {
        let frame = compressed();
        let read = decode_frame(&key(), &frame).unwrap();
        assert_eq!(read.codec, Codec::Zstd);
        assert_eq!(read.prev, Tid(7));
        assert_eq!(read.stored_len, frame.len() - HEADER_LEN);
        assert!(read.stored_len * 10 < pickle().len(), "{}", read.stored_len);
        assert_eq!(&read.data[..], &pickle()[..]);
    }

    #[test]
    fn the_header_states_the_stored_length_and_the_checksum_covers_the_stored_bytes() {
        let frame = compressed();
        let stored = &frame[HEADER_LEN..];
        let payload_len = u32::from_le_bytes(frame[16..20].try_into().unwrap());
        assert_eq!(payload_len as usize, stored.len());
        let sum = u64::from_le_bytes(frame[24..32].try_into().unwrap());
        assert_eq!(checksum(&key(), &frame[..24], &[], stored), sum);
    }

    #[test]
    fn every_flipped_bit_of_a_compressed_frame_is_detected() {
        let frame = compressed();
        for byte in 0..frame.len() {
            for bit in 0..8 {
                let mut bad = frame.clone();
                bad[byte] ^= 1 << bit;
                assert!(decode_frame(&key(), &bad).is_err(), "byte {byte} bit {bit}");
            }
        }
    }

    #[test]
    fn a_zstd_payload_that_does_not_decode_is_refused() {
        let frame = encode_frame(&key(), MAIN_BRANCH, Tid(7), b"pickle");
        let mut header = frame[..24].to_vec();
        header[2] = CODEC_ZSTD;
        assert_eq!(
            decode_frame(&key(), &seal(&header, &[], b"pickle")),
            Err(FrameError::Payload)
        );
    }

    #[test]
    fn a_transaction_row_is_written_as_it_is() {
        let frame = encode_frame(&txn_key(Tid(9)), MAIN_BRANCH, Tid::ZERO, &pickle());
        assert_eq!(frame[2], CODEC_NONE);
        assert_eq!(frame.len(), HEADER_LEN + pickle().len());
    }

    #[test]
    fn typed_field_slots_of_any_length_are_read() {
        let frame = encode_frame(&key(), MAIN_BRANCH, Tid(7), b"pickle");
        let mut header = frame[..24].to_vec();
        header[20..22].copy_from_slice(&3u16.to_le_bytes());
        let slotted = seal(&header, b"abc", b"pickle");
        let read = decode_frame(&key(), &slotted).unwrap();
        assert_eq!(read.slots, b"abc");
        assert_eq!(&read.data[..], b"pickle");
    }

    #[test]
    fn a_short_frame_is_truncated() {
        let frame = encode_frame(&key(), MAIN_BRANCH, Tid(7), b"pickle");
        assert_eq!(
            decode_frame(&key(), &frame[..10]),
            Err(FrameError::Truncated)
        );
        assert_eq!(
            decode_frame(&key(), &frame[..frame.len() - 1]),
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
