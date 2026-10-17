//! The on-disk format: the frame and its checksum, the keys, and the
//! values of the transaction rows, the sequence rows, the rows of the KV
//! namespaces and the `meta` rows. `blob` gives the format of the blob
//! store and of the blob records. This module is a red zone.
//!
//! Every record, every blob record, every transaction row, every sequence
//! row, every row of a KV namespace and every intent file is a frame: a
//! 32-byte header, the typed-field slots, then the payload. The header
//! fields are little-endian:
//!
//! | Offset | Size | Field |
//! | --- | --- | --- |
//! | 0 | 1 | frame version |
//! | 1 | 1 | flags: 0; the encrypted flag is reserved for a later phase; no flag is defined yet |
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
//! bytes in this order: the tag of the key space (1 byte), the key of the
//! row, header bytes 0 to 23, the slots, then the payload. `KeySpace`
//! gives the tags. The key of a record and of a blob record is the 16
//! bytes of `record_key`. The key of a transaction row and of an intent
//! file is the 8 bytes of `txn_key`. The key of a sequence row is the
//! UTF-8 bytes of the name of the sequence. The key of a row of a KV
//! namespace is the bytes of `kv_key`. So a frame that is read under
//! another key, or in another key space, fails the check.
//!
//! The payload holds the data in the form that the codec byte gives. The
//! checksum covers the payload as stored, so a read verifies the stored
//! bytes before it decodes them. `decode_frame` decodes every codec, so
//! each read path gets the data, never the stored form.
//!
//! Keys are big-endian, so that the byte order of the keys is the numeric
//! order of oids and tids. The numbers in the rows of `meta` are 8-byte
//! big-endian values, as ZODB writes oids and tids. The row `object_count`
//! of `meta` holds the number of distinct oids that have at least one
//! record, a deletion included.
//!
//! The transaction index gives the oids of each transaction. It has one
//! key for each record: the 16 bytes of `txn_oid_key`, the tid then the
//! oid. Its value is empty. So a range scan over the keys of one tid gives
//! the oids of that transaction in ascending order. The index has no
//! frame and no checksum of its own: the checksum of a record frame covers
//! the oid and the tid of the record.
//!
//! The KV namespaces have three key spaces (ADR 0043). `kv_ephemeral` and
//! `kv_durable` hold one row for each key of a namespace: its key is
//! `kv_key`, the length of the name in one byte, the name, then the key.
//! The payload of an ephemeral row is the expiry, a little-endian `u64` of
//! milliseconds since the Unix epoch, then the value. The payload of a
//! durable row is the value. The row of a durable namespace with the empty
//! key is its generation row: its payload is the tid of the last commit
//! that wrote the namespace, a little-endian `u64`. `kv_expiry` is the
//! index of the expiries: its key is `kv_expiry_key`, the expiry
//! big-endian, then the key of the ephemeral row. Its value is empty, so it
//! has no frame and no tag, as the transaction index.

pub mod blob;

use std::borrow::Cow;

use xxhash_rust::xxh3::Xxh3;

use crate::backend::{TxnMeta, TxnStatus};
use crate::types::{Oid, Tid};

mod codec;
#[cfg(test)]
mod synthetic;

pub use codec::{Codec, Compression};

#[cfg(test)]
pub(crate) use codec::EMPTY_ZSTD_PAYLOADS;
#[cfg(feature = "testing")]
pub(crate) use codec::compressions;
pub(crate) use codec::{decode as decode_payload, encode as encode_payload};

/// The name and version of the format, in the `format` row of `meta`.
/// A store of this format has the row `object_count` in `meta`. An open
/// refuses a store with a name other than this one and `FORMAT_1_NAME`.
pub const FORMAT_NAME: &[u8] = b"ZOE format 2";

/// The name of the format without the row `object_count`. A writer open
/// counts the objects of such a store once, then writes the count and
/// `FORMAT_NAME`. A read-only open reads such a store and has no count.
pub const FORMAT_1_NAME: &[u8] = b"ZOE format 1";

/// The key space of a frame. Its tag is the first byte of the checksum
/// input, so a frame verifies only in the key space that wrote it.
///
/// A tag never changes, and no key space takes the tag of another, also
/// after one is removed. 0 is not a tag. The tag 6 is reserved for the
/// key space `vectors` that the specification plans. A later frame kind
/// takes the next free tag: 9.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum KeySpace {
    /// The records, in `records`.
    Records = 1,
    /// The blob records, in `blobs`.
    Blobs = 2,
    /// The transaction rows, in `txns`.
    Txns = 3,
    /// The intent files, in `blobs/tmp/`.
    Intents = 4,
    /// The sequence rows, in `sequences`.
    Sequences = 5,
    /// The rows of the ephemeral KV namespaces, in `kv_ephemeral`.
    KvEphemeral = 7,
    /// The rows of the durable KV namespaces, in `kv_durable`.
    KvDurable = 8,
}

impl KeySpace {
    /// Return the tag of the key space.
    pub const fn tag(self) -> u8 {
        self as u8
    }
}

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
    /// not one standard zstd frame, does not state the data length, states
    /// 0 bytes or a length over the bound, or decodes to another length.
    Payload,
}

/// A frame that `decode_frame` read, checked and decoded.
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
    /// The length of the payload as stored, before the codec decodes it.
    pub stored_len: usize,
    /// The data: the payload, decoded with its codec.
    pub data: Cow<'a, [u8]>,
}

/// Encode the frame of the row `key` of `space`, with the data as it is,
/// no flags and no typed-field slots.
pub fn encode_frame(space: KeySpace, key: &[u8], branch: u32, prev: Tid, data: &[u8]) -> Vec<u8> {
    encode_frame_with(space, key, branch, prev, data, Compression::None)
}

/// Encode the frame of the row `key` of `space`, with no flags and no
/// typed-field slots. `compression` selects the codec of the payload.
pub fn encode_frame_with(
    space: KeySpace,
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
    encode_frame_payload(space, key, branch, prev, codec, &payload)
}

/// Encode the frame of the row `key` of `space` around `payload`, a
/// payload that `encode_payload` gave with `codec`. The frame has no flags
/// and no typed-field slots. It is the frame that `encode_frame_with` gives
/// for the same data and compression.
pub(crate) fn encode_frame_payload(
    space: KeySpace,
    key: &[u8],
    branch: u32,
    prev: Tid,
    codec: Codec,
    payload: &[u8],
) -> Vec<u8> {
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
    let checksum = checksum(space, key, &frame[..24], &[], payload);
    frame.extend_from_slice(&checksum.to_le_bytes());
    frame.extend_from_slice(payload);
    frame
}

/// Encode the frame of the row `key` of `space` with the codec byte
/// `codec` and `payload` as the stored payload. Tests use it to write
/// frames that the encoder never writes.
#[cfg(test)]
pub(crate) fn encode_stored_frame(
    space: KeySpace,
    key: &[u8],
    codec: u8,
    payload: &[u8],
) -> Vec<u8> {
    let mut frame = encode_frame(space, key, MAIN_BRANCH, Tid::ZERO, payload);
    frame[2] = codec;
    seal(space, key, &mut frame);
    frame
}

/// Write the checksum of `frame`, the frame of the row `key` of `space`,
/// into its header. Tests use it to change a header byte and keep the
/// checksum valid.
#[cfg(test)]
pub(crate) fn seal(space: KeySpace, key: &[u8], frame: &mut [u8]) {
    let slots_len = usize::from(u16::from_le_bytes(frame[20..22].try_into().unwrap()));
    let (header, body) = frame.split_at(HEADER_LEN);
    let (slots, payload) = body.split_at(slots_len);
    let sum = checksum(space, key, &header[..24], slots, payload);
    frame[24..HEADER_LEN].copy_from_slice(&sum.to_le_bytes());
}

/// Read the frame of the row `key` of `space`, verify its checksum and
/// decode its payload. A frame that another key space wrote fails the
/// checksum.
///
/// A frame with a flag, a codec that this code does not know or a non-zero
/// reserved byte is refused. The typed-field slots are opaque, so slots
/// of any length are accepted.
pub fn decode_frame<'a>(
    space: KeySpace,
    key: &[u8],
    bytes: &'a [u8],
) -> Result<Frame<'a>, FrameError> {
    let (frame, payload) = check_frame(space, key, bytes)?;
    Ok(Frame {
        data: codec::decode(frame.codec, payload)?,
        ..frame
    })
}

/// Verify the frame of the row `key` of `space` as `decode_frame` does,
/// and leave its payload as it is stored. A reader that needs only the
/// key of a row verifies its frame with this call, which decodes
/// nothing.
pub fn verify_frame(space: KeySpace, key: &[u8], bytes: &[u8]) -> Result<(), FrameError> {
    check_frame(space, key, bytes).map(drop)
}

/// Check the header and the checksum of a frame. Return the frame with
/// the stored payload in `data`, and the stored payload.
fn check_frame<'a>(
    space: KeySpace,
    key: &[u8],
    bytes: &'a [u8],
) -> Result<(Frame<'a>, &'a [u8]), FrameError> {
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
    if checksum(space, key, &header[..24], slots, payload) != stored {
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
    let frame = Frame {
        flags: header[1],
        codec,
        branch: u32::from_le_bytes(header[4..8].try_into().unwrap()),
        prev: Tid(u64::from_le_bytes(header[8..16].try_into().unwrap())),
        slots,
        stored_len: payload.len(),
        data: Cow::Borrowed(payload),
    };
    Ok((frame, payload))
}

/// Return the checksum of a frame: the xxh3-64 hash of the tag of
/// `space`, `key`, `header`, `slots` and `payload`, in this order.
fn checksum(space: KeySpace, key: &[u8], header: &[u8], slots: &[u8], payload: &[u8]) -> u64 {
    let mut hasher = Xxh3::new();
    hasher.update(&[space.tag()]);
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

/// Return the key of the transaction index for the record of `oid` at
/// `tid`: the tid, then the oid, both big-endian.
pub fn txn_oid_key(tid: Tid, oid: Oid) -> [u8; 16] {
    let mut key = [0; 16];
    key[..8].copy_from_slice(&tid.0.to_be_bytes());
    key[8..].copy_from_slice(&oid.0.to_be_bytes());
    key
}

/// Split a key of the transaction index into its tid and oid.
pub fn split_txn_oid_key(key: &[u8; 16]) -> (Tid, Oid) {
    let tid = u64::from_be_bytes(key[..8].try_into().unwrap());
    let oid = u64::from_be_bytes(key[8..].try_into().unwrap());
    (Tid(tid), Oid(oid))
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

/// The length of the payload of a sequence row.
pub const SEQUENCE_LEN: usize = 8;

/// Encode the payload of a sequence row: the end of the reserved values,
/// little-endian. The end is the first value that the store has not
/// reserved.
pub fn encode_sequence(end: u64) -> [u8; SEQUENCE_LEN] {
    end.to_le_bytes()
}

/// Decode the payload of a sequence row. Return the end of the reserved
/// values.
pub fn decode_sequence(payload: &[u8]) -> Result<u64, FrameError> {
    let bytes: [u8; SEQUENCE_LEN] = payload.try_into().map_err(|_| FrameError::Truncated)?;
    Ok(u64::from_le_bytes(bytes))
}

/// The most bytes of the name of a KV namespace. The key of a row holds
/// the length of the name in one byte.
pub const KV_MAX_NAME_LEN: usize = 255;

/// The most bytes of a key of a KV namespace.
pub const KV_MAX_KEY_LEN: usize = 1024;

/// The length of the expiry at the start of the payload of an ephemeral
/// row, and of the payload of a generation row.
pub const KV_U64_LEN: usize = 8;

/// Return the key of the row of `key` in the KV namespace `name`: the
/// length of the name in one byte, the name, then the key. The empty key
/// of a durable namespace is the key of its generation row.
///
/// The name is 1 to `KV_MAX_NAME_LEN` bytes. The caller checks it.
pub fn kv_key(name: &[u8], key: &[u8]) -> Vec<u8> {
    let len = u8::try_from(name.len()).expect("a namespace name fits in 255 bytes");
    let mut row = Vec::with_capacity(1 + name.len() + key.len());
    row.push(len);
    row.extend_from_slice(name);
    row.extend_from_slice(key);
    row
}

/// Split the key of a row of a KV namespace into the name and the key.
/// Return `None` when the length of the name is 0 or runs past the end of
/// the row.
pub fn split_kv_key(row: &[u8]) -> Option<(&[u8], &[u8])> {
    let (&len, rest) = row.split_first()?;
    let len = usize::from(len);
    if len == 0 || rest.len() < len {
        return None;
    }
    Some(rest.split_at(len))
}

/// Return the key of `row`, the key of an ephemeral row, in the index of
/// the expiries: `expiry` big-endian, then `row`. So a scan of the index
/// gives the rows in the order of their expiries.
pub fn kv_expiry_key(expiry: u64, row: &[u8]) -> Vec<u8> {
    let mut key = Vec::with_capacity(KV_U64_LEN + row.len());
    key.extend_from_slice(&expiry.to_be_bytes());
    key.extend_from_slice(row);
    key
}

/// Split a key of the index of the expiries into the expiry and the key of
/// the row. Return `None` when the key is shorter than the expiry.
pub fn split_kv_expiry_key(key: &[u8]) -> Option<(u64, &[u8])> {
    let (expiry, row) = key.split_at_checked(KV_U64_LEN)?;
    Some((u64::from_be_bytes(expiry.try_into().unwrap()), row))
}

/// Encode the payload of an ephemeral row: `expiry` in milliseconds since
/// the Unix epoch, little-endian, then `value`.
pub fn encode_kv_ephemeral(expiry: u64, value: &[u8]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(KV_U64_LEN + value.len());
    payload.extend_from_slice(&expiry.to_le_bytes());
    payload.extend_from_slice(value);
    payload
}

/// Decode the payload of an ephemeral row. Return the expiry and the
/// value. A payload shorter than the expiry is `Truncated`.
pub fn decode_kv_ephemeral(payload: &[u8]) -> Result<(u64, &[u8]), FrameError> {
    let (expiry, value) = payload
        .split_at_checked(KV_U64_LEN)
        .ok_or(FrameError::Truncated)?;
    Ok((u64::from_le_bytes(expiry.try_into().unwrap()), value))
}

/// Encode the payload of the generation row of a durable namespace: `tid`,
/// little-endian.
pub fn encode_kv_generation(tid: Tid) -> [u8; KV_U64_LEN] {
    tid.0.to_le_bytes()
}

/// Decode the payload of the generation row of a durable namespace. A
/// payload of another length is `Truncated`.
pub fn decode_kv_generation(payload: &[u8]) -> Result<Tid, FrameError> {
    let bytes: [u8; KV_U64_LEN] = payload.try_into().map_err(|_| FrameError::Truncated)?;
    Ok(Tid(u64::from_le_bytes(bytes)))
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
        let frame = encode_frame(KeySpace::Records, &key(), MAIN_BRANCH, Tid(7), b"pickle");
        assert_eq!(frame.len(), HEADER_LEN + 6);
        let read = decode_frame(KeySpace::Records, &key(), &frame).unwrap();
        assert_eq!(read.prev, Tid(7));
        assert_eq!(read.branch, MAIN_BRANCH);
        assert_eq!(read.codec, Codec::None);
        assert_eq!(read.stored_len, 6);
        assert_eq!(&read.data[..], b"pickle");
        assert!(read.slots.is_empty());
    }

    #[test]
    fn every_flipped_bit_is_detected() {
        let frame = encode_frame(KeySpace::Records, &key(), MAIN_BRANCH, Tid(7), b"pickle");
        for byte in 0..frame.len() {
            for bit in 0..8 {
                let mut bad = frame.clone();
                bad[byte] ^= 1 << bit;
                assert!(
                    decode_frame(KeySpace::Records, &key(), &bad).is_err(),
                    "byte {byte} bit {bit}"
                );
            }
        }
    }

    #[test]
    fn every_flipped_bit_of_the_key_is_detected() {
        let frame = encode_frame(KeySpace::Records, &key(), MAIN_BRANCH, Tid(7), b"pickle");
        for byte in 0..key().len() {
            for bit in 0..8 {
                let mut bad = key();
                bad[byte] ^= 1 << bit;
                assert_eq!(
                    decode_frame(KeySpace::Records, &bad, &frame),
                    Err(FrameError::Checksum),
                    "byte {byte} bit {bit}"
                );
            }
        }
    }

    #[test]
    fn a_frame_under_another_oid_or_tid_is_refused() {
        let frame = encode_frame(KeySpace::Records, &key(), MAIN_BRANCH, Tid(7), b"pickle");
        for (oid, tid) in [(Oid(2), Tid(9)), (Oid(1), Tid(10))] {
            assert_eq!(
                decode_frame(KeySpace::Records, &record_key(oid, tid), &frame),
                Err(FrameError::Checksum),
                "{oid:?} {tid:?}"
            );
        }
    }

    #[test]
    fn a_transaction_row_frame_covers_its_tid() {
        let frame = encode_frame(
            KeySpace::Txns,
            &txn_key(Tid(9)),
            MAIN_BRANCH,
            Tid::ZERO,
            b"row",
        );
        assert!(decode_frame(KeySpace::Txns, &txn_key(Tid(9)), &frame).is_ok());
        assert_eq!(
            decode_frame(KeySpace::Txns, &txn_key(Tid(10)), &frame),
            Err(FrameError::Checksum)
        );
    }

    /// Every key space of this build.
    const SPACES: [KeySpace; 7] = [
        KeySpace::Records,
        KeySpace::Blobs,
        KeySpace::Txns,
        KeySpace::Intents,
        KeySpace::Sequences,
        KeySpace::KvEphemeral,
        KeySpace::KvDurable,
    ];

    #[test]
    fn the_tags_of_the_key_spaces_never_change() {
        let tags = SPACES.map(KeySpace::tag);
        assert_eq!(tags, [1, 2, 3, 4, 5, 7, 8]);
    }

    #[test]
    fn a_frame_verifies_only_in_its_own_key_space() {
        for written in SPACES {
            for compression in [Compression::None, Compression::default()] {
                let frame =
                    encode_frame_with(written, &key(), MAIN_BRANCH, Tid(7), &pickle(), compression);
                for read in SPACES {
                    let decoded = decode_frame(read, &key(), &frame);
                    if read == written {
                        assert_eq!(&decoded.unwrap().data[..], &pickle()[..]);
                    } else {
                        assert_eq!(
                            decoded,
                            Err(FrameError::Checksum),
                            "{written:?} read as {read:?}"
                        );
                    }
                }
            }
        }
    }

    /// Build a frame from the first 24 header bytes, the slots and the
    /// payload, with a valid checksum.
    fn sealed(header: &[u8], slots: &[u8], payload: &[u8]) -> Vec<u8> {
        let mut frame = header[..24].to_vec();
        frame.extend_from_slice(
            &checksum(KeySpace::Records, &key(), &frame, slots, payload).to_le_bytes(),
        );
        frame.extend_from_slice(slots);
        frame.extend_from_slice(payload);
        frame
    }

    #[test]
    fn a_frame_of_a_newer_format_is_refused() {
        let frame = encode_frame(KeySpace::Records, &key(), MAIN_BRANCH, Tid(7), b"pickle");
        for (offset, value) in [(1, 1), (2, 2), (2, 0xff), (3, 1), (22, 1), (23, 1)] {
            let mut header = frame[..24].to_vec();
            header[offset] = value;
            assert_eq!(
                decode_frame(KeySpace::Records, &key(), &sealed(&header, &[], b"pickle")),
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
            KeySpace::Records,
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
        let read = decode_frame(KeySpace::Records, &key(), &frame).unwrap();
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
        assert_eq!(
            checksum(KeySpace::Records, &key(), &frame[..24], &[], stored),
            sum
        );
    }

    #[test]
    fn every_flipped_bit_of_a_compressed_frame_is_detected() {
        let frame = compressed();
        for byte in 0..frame.len() {
            for bit in 0..8 {
                let mut bad = frame.clone();
                bad[byte] ^= 1 << bit;
                assert!(
                    decode_frame(KeySpace::Records, &key(), &bad).is_err(),
                    "byte {byte} bit {bit}"
                );
            }
        }
    }

    #[test]
    fn a_zstd_payload_that_does_not_decode_is_refused() {
        let frame = encode_frame(KeySpace::Records, &key(), MAIN_BRANCH, Tid(7), b"pickle");
        let mut header = frame[..24].to_vec();
        header[2] = CODEC_ZSTD;
        assert_eq!(
            decode_frame(KeySpace::Records, &key(), &sealed(&header, &[], b"pickle")),
            Err(FrameError::Payload)
        );
    }

    #[test]
    fn a_zstd_frame_that_states_no_data_is_refused() {
        for payload in EMPTY_ZSTD_PAYLOADS {
            let frame = encode_stored_frame(KeySpace::Records, &key(), CODEC_ZSTD, payload);
            assert_eq!(
                decode_frame(KeySpace::Records, &key(), &frame),
                Err(FrameError::Payload),
                "{payload:02x?}"
            );
        }
    }

    #[test]
    fn a_transaction_row_is_written_as_it_is() {
        let frame = encode_frame(
            KeySpace::Txns,
            &txn_key(Tid(9)),
            MAIN_BRANCH,
            Tid::ZERO,
            &pickle(),
        );
        assert_eq!(frame[2], CODEC_NONE);
        assert_eq!(frame.len(), HEADER_LEN + pickle().len());
    }

    #[test]
    fn typed_field_slots_of_any_length_are_read() {
        let frame = encode_frame(KeySpace::Records, &key(), MAIN_BRANCH, Tid(7), b"pickle");
        let mut header = frame[..24].to_vec();
        header[20..22].copy_from_slice(&3u16.to_le_bytes());
        let slotted = sealed(&header, b"abc", b"pickle");
        let read = decode_frame(KeySpace::Records, &key(), &slotted).unwrap();
        assert_eq!(read.slots, b"abc");
        assert_eq!(&read.data[..], b"pickle");
    }

    #[test]
    fn a_short_frame_is_truncated() {
        let frame = encode_frame(KeySpace::Records, &key(), MAIN_BRANCH, Tid(7), b"pickle");
        assert_eq!(
            decode_frame(KeySpace::Records, &key(), &frame[..10]),
            Err(FrameError::Truncated)
        );
        assert_eq!(
            decode_frame(KeySpace::Records, &key(), &frame[..frame.len() - 1]),
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
    fn index_keys_sort_by_tid_then_oid() {
        let key = txn_oid_key(Tid(0x0102), Oid(0x0304));
        assert_eq!(
            key,
            [0, 0, 0, 0, 0, 0, 1, 2, 0, 0, 0, 0, 0, 0, 3, 4],
            "the tid, then the oid, big-endian"
        );
        assert_eq!(split_txn_oid_key(&key), (Tid(0x0102), Oid(0x0304)));
        let a = txn_oid_key(Tid(1), Oid(u64::MAX));
        let b = txn_oid_key(Tid(2), Oid(0));
        let c = txn_oid_key(Tid(2), Oid(1));
        assert!(a < b && b < c);
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

    #[test]
    fn a_sequence_row_is_a_frame_of_codec_0_with_the_end_in_8_bytes() {
        let key = "intids".as_bytes();
        let frame = encode_frame(
            KeySpace::Sequences,
            key,
            MAIN_BRANCH,
            Tid::ZERO,
            &encode_sequence(0x0102),
        );
        assert_eq!(frame.len(), HEADER_LEN + SEQUENCE_LEN);
        assert_eq!(frame[2], CODEC_NONE);
        assert_eq!(&frame[16..20], &8u32.to_le_bytes());
        assert_eq!(&frame[HEADER_LEN..], &[0x02, 0x01, 0, 0, 0, 0, 0, 0]);
        let read = decode_frame(KeySpace::Sequences, key, &frame).unwrap();
        assert_eq!(read.prev, Tid::ZERO);
        assert_eq!(decode_sequence(&read.data), Ok(0x0102));
    }

    #[test]
    fn a_sequence_row_under_another_name_is_refused() {
        let frame = encode_frame(
            KeySpace::Sequences,
            b"intids",
            MAIN_BRANCH,
            Tid::ZERO,
            &encode_sequence(101),
        );
        assert_eq!(
            decode_frame(KeySpace::Sequences, b"lexicon", &frame),
            Err(FrameError::Checksum)
        );
    }

    #[test]
    fn a_sequence_payload_of_another_length_is_refused() {
        for payload in [&[][..], &[1; 7], &[1; 9]] {
            assert_eq!(decode_sequence(payload), Err(FrameError::Truncated));
        }
        assert_eq!(decode_sequence(&encode_sequence(u64::MAX)), Ok(u64::MAX));
    }

    #[test]
    fn a_kv_key_holds_the_length_of_the_name_the_name_and_the_key() {
        let row = kv_key(b"sessions", b"id");
        assert_eq!(row, b"\x08sessionsid");
        assert_eq!(split_kv_key(&row), Some((&b"sessions"[..], &b"id"[..])));
        // The generation row of a durable namespace has the empty key.
        assert_eq!(
            split_kv_key(&kv_key(b"registry", b"")),
            Some((&b"registry"[..], &b""[..]))
        );
        let longest = vec![b'n'; KV_MAX_NAME_LEN];
        assert_eq!(kv_key(&longest, b"k")[0], 255);
    }

    #[test]
    fn a_kv_key_whose_name_runs_past_the_row_does_not_split() {
        for row in [&b""[..], b"\x00key", b"\x09sessions", b"\xffshort"] {
            assert_eq!(split_kv_key(row), None, "{row:?}");
        }
    }

    #[test]
    fn the_keys_of_one_namespace_sort_as_their_bytes() {
        let a = kv_key(b"sessions", b"a");
        let b = kv_key(b"sessions", b"ab");
        let c = kv_key(b"sessions", b"b");
        assert!(a < b && b < c);
        // UTF-8 byte order is the code point order of a Python str.
        let e = kv_key(b"sessions", "\u{e9}".as_bytes());
        let z = kv_key(b"sessions", "\u{1f600}".as_bytes());
        assert!(c < e && e < z);
    }

    #[test]
    fn an_expiry_key_sorts_by_the_expiry_then_the_row() {
        let row = kv_key(b"locks", b"doc");
        let key = kv_expiry_key(0x0102, &row);
        assert_eq!(&key[..8], &[0, 0, 0, 0, 0, 0, 1, 2]);
        assert_eq!(split_kv_expiry_key(&key), Some((0x0102, &row[..])));
        assert!(kv_expiry_key(1, &kv_key(b"z", b"z")) < kv_expiry_key(2, &kv_key(b"a", b"a")));
        assert_eq!(split_kv_expiry_key(&[0; 7]), None);
    }

    #[test]
    fn an_ephemeral_row_is_a_frame_with_the_expiry_then_the_value() {
        let row = kv_key(b"sessions", b"id");
        let payload = encode_kv_ephemeral(0x0102, b"value");
        let frame = encode_frame(
            KeySpace::KvEphemeral,
            &row,
            MAIN_BRANCH,
            Tid::ZERO,
            &payload,
        );
        assert_eq!(frame[2], CODEC_NONE);
        assert_eq!(
            &frame[HEADER_LEN..HEADER_LEN + 8],
            &[2, 1, 0, 0, 0, 0, 0, 0]
        );
        let read = decode_frame(KeySpace::KvEphemeral, &row, &frame).unwrap();
        assert_eq!(decode_kv_ephemeral(&read.data), Ok((0x0102, &b"value"[..])));
        assert_eq!(decode_kv_ephemeral(&[0; 7]), Err(FrameError::Truncated));
        assert_eq!(decode_kv_ephemeral(&[0; 8]), Ok((0, &b""[..])));
    }

    #[test]
    fn a_generation_row_holds_a_tid_in_8_bytes() {
        assert_eq!(encode_kv_generation(Tid(0x0102)), [2, 1, 0, 0, 0, 0, 0, 0]);
        assert_eq!(
            decode_kv_generation(&encode_kv_generation(Tid(9))),
            Ok(Tid(9))
        );
        for payload in [&[][..], &[1; 7], &[1; 9]] {
            assert_eq!(decode_kv_generation(payload), Err(FrameError::Truncated));
        }
    }

    #[test]
    fn a_kv_row_verifies_only_under_its_key_and_in_its_flavour() {
        let row = kv_key(b"registry", b"plone.title");
        let frame = encode_frame(KeySpace::KvDurable, &row, MAIN_BRANCH, Tid::ZERO, b"v");
        assert!(decode_frame(KeySpace::KvDurable, &row, &frame).is_ok());
        let other = kv_key(b"registry", b"plone.titlf");
        assert_eq!(
            decode_frame(KeySpace::KvDurable, &other, &frame),
            Err(FrameError::Checksum)
        );
        assert_eq!(
            decode_frame(KeySpace::KvEphemeral, &row, &frame),
            Err(FrameError::Checksum)
        );
    }
}
