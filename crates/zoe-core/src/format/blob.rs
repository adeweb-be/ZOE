//! The on-disk format of the blob store: the names of the blob files, the
//! payload of a blob record and the intent file of a finish.
//!
//! The blob files follow the BushyLayout of ZODB, under `blobs/` in the
//! store directory:
//!
//! - `blobs/.layout` holds `bushy`.
//! - The blob of `(oid, tid)` is `blobs/0xAA/0xBB/.../0xHH/<tid>.blob`: one
//!   directory for each of the 8 big-endian bytes of the oid, then the tid
//!   as ZODB's `tid_repr` writes it.
//! - `blobs/tmp/` holds the files of uncommitted blobs and the intent
//!   files of finishes.
//!
//! The payload of a blob record is 40 bytes: the BLAKE3 hash of the blob
//! file (32 bytes), then the size of the file in bytes (8 bytes,
//! little-endian). The blob record is a frame of the key space
//! `KeySpace::Blobs` under the 16-byte record key of `(oid, tid)`, with the
//! previous serial 0 and the codec `CODEC_NONE`: the compression of the
//! store never applies to it.
//!
//! An intent file is `blobs/tmp/0x<16 hex digits of the tid>.intent`. It
//! holds a frame of the key space `KeySpace::Intents` with the codec
//! `CODEC_NONE` under the 8-byte transaction key of the tid. Its payload
//! is the oids of the blobs of the finish, 8 big-endian bytes each.

use super::{FrameError, KeySpace, MAIN_BRANCH, decode_frame, encode_frame, txn_key};
use crate::backend::BlobRecord;
use crate::types::{Oid, Tid};

/// The directory of the blob store.
pub const BLOB_DIR: &str = "blobs";

/// The directory of the uncommitted blob files and the intent files.
pub const TMP_DIR: &str = "blobs/tmp";

/// The file that names the layout of the blob store.
pub const LAYOUT_FILE: &str = "blobs/.layout";

/// The name under which the layout file is written before it is renamed.
pub const NEW_LAYOUT_FILE: &str = "blobs/tmp/layout.tmp";

/// The content of the layout file.
pub const LAYOUT: &str = "bushy";

/// The length of the payload of a blob record.
pub const BLOB_RECORD_LEN: usize = 40;

/// The suffix of a blob file.
const BLOB_SUFFIX: &str = ".blob";

/// The suffix of an intent file.
const INTENT_SUFFIX: &str = ".intent";

/// Return the tid as ZODB's `tid_repr` writes it: `0x`, then the hex
/// digits without leading zeros, padded to an even count. A tid whose
/// high byte is not 0 has 16 digits. Every tid that `timestamp` gives has
/// such a high byte.
pub fn tid_repr(tid: Tid) -> String {
    let digits = format!("{:x}", tid.0);
    if digits.len() % 2 == 1 {
        format!("0x0{digits}")
    } else {
        format!("0x{digits}")
    }
}

/// Return the directory of the blob files of `oid`.
pub fn oid_dir(oid: Oid) -> String {
    let mut dir = String::from(BLOB_DIR);
    for byte in oid.0.to_be_bytes() {
        dir.push_str(&format!("/0x{byte:02x}"));
    }
    dir
}

/// Return the name of the blob file of `oid` at `tid`.
pub fn blob_name(oid: Oid, tid: Tid) -> String {
    format!("{}/{}{BLOB_SUFFIX}", oid_dir(oid), tid_repr(tid))
}

/// Encode the payload of a blob record.
pub fn encode_blob(blob: &BlobRecord) -> [u8; BLOB_RECORD_LEN] {
    let mut payload = [0; BLOB_RECORD_LEN];
    payload[..32].copy_from_slice(&blob.hash);
    payload[32..].copy_from_slice(&blob.size.to_le_bytes());
    payload
}

/// Decode the payload of a blob record.
pub fn decode_blob(payload: &[u8]) -> Result<BlobRecord, FrameError> {
    let payload: &[u8; BLOB_RECORD_LEN] = payload.try_into().map_err(|_| FrameError::Truncated)?;
    Ok(BlobRecord {
        hash: payload[..32].try_into().unwrap(),
        size: u64::from_le_bytes(payload[32..].try_into().unwrap()),
    })
}

/// Return the name of the intent file of the finish `tid`.
pub fn intent_name(tid: Tid) -> String {
    format!("{TMP_DIR}/0x{:016x}{INTENT_SUFFIX}", tid.0)
}

/// Return the tid of the intent file `file`, a name in `TMP_DIR` without
/// the directory part. Return `None` when `file` is not an intent file.
pub fn intent_tid(file: &str) -> Option<Tid> {
    let digits = file.strip_prefix("0x")?.strip_suffix(INTENT_SUFFIX)?;
    if digits.len() != 16 || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u64::from_str_radix(digits, 16).ok().map(Tid)
}

/// Encode the intent file of the finish `tid`, which places the blobs of
/// `oids`.
pub fn encode_intent(tid: Tid, oids: &[Oid]) -> Vec<u8> {
    let payload: Vec<u8> = oids.iter().flat_map(|oid| oid.0.to_be_bytes()).collect();
    encode_frame(
        KeySpace::Intents,
        &txn_key(tid),
        MAIN_BRANCH,
        Tid::ZERO,
        &payload,
    )
}

/// Decode the intent file of the finish `tid` and return its oids.
pub fn decode_intent(tid: Tid, bytes: &[u8]) -> Result<Vec<Oid>, FrameError> {
    let frame = decode_frame(KeySpace::Intents, &txn_key(tid), bytes)?;
    let (oids, rest) = frame.data.as_chunks::<8>();
    if !rest.is_empty() {
        return Err(FrameError::Truncated);
    }
    Ok(oids
        .iter()
        .map(|oid| Oid(u64::from_be_bytes(*oid)))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tids_are_written_as_zodb_writes_them() {
        // The expected values come from `ZODB.utils.tid_repr`.
        for (tid, repr) in [
            (0, "0x00"),
            (0x123, "0x0123"),
            (0x1234, "0x1234"),
            (0x023c_2b00_0000_0000, "0x023c2b0000000000"),
            (0x03f6_db4f_2444_4444, "0x03f6db4f24444444"),
            (u64::MAX, "0xffffffffffffffff"),
        ] {
            assert_eq!(tid_repr(Tid(tid)), repr);
        }
    }

    #[test]
    fn blob_files_follow_the_bushy_layout() {
        let oid = Oid(0x0102_0304_0506_0708);
        let tid = Tid(0x03f6_db4f_2444_4444);
        assert_eq!(
            blob_name(oid, tid),
            "blobs/0x01/0x02/0x03/0x04/0x05/0x06/0x07/0x08/0x03f6db4f24444444.blob"
        );
        assert_eq!(
            oid_dir(Oid(1)),
            "blobs/0x00/0x00/0x00/0x00/0x00/0x00/0x00/0x01"
        );
    }

    #[test]
    fn a_blob_record_round_trips() {
        let blob = BlobRecord {
            hash: *blake3::hash(b"blob").as_bytes(),
            size: 4,
        };
        let payload = encode_blob(&blob);
        assert_eq!(payload[32..], 4u64.to_le_bytes());
        assert_eq!(decode_blob(&payload), Ok(blob));
        assert_eq!(decode_blob(&payload[..39]), Err(FrameError::Truncated));
    }

    #[test]
    fn an_intent_round_trips() {
        let tid = Tid(0x03f6_db4f_2444_4444);
        let name = intent_name(tid);
        assert_eq!(name, "blobs/tmp/0x03f6db4f24444444.intent");
        let file = name.rsplit('/').next().unwrap();
        assert_eq!(intent_tid(file), Some(tid));

        let oids = [Oid(1), Oid(u64::MAX)];
        let bytes = encode_intent(tid, &oids);
        assert_eq!(bytes[2], crate::format::CODEC_NONE);
        assert_eq!(decode_intent(tid, &bytes), Ok(oids.to_vec()));
    }

    #[test]
    fn other_files_are_not_intents() {
        for file in [
            "1.tmp",
            "layout.tmp",
            "0x03f6db4f24444444.blob",
            "0x03f6db4f2444444.intent",
            "0x03f6db4f2444444g.intent",
        ] {
            assert_eq!(intent_tid(file), None, "{file}");
        }
    }

    #[test]
    fn a_torn_intent_is_refused() {
        let tid = Tid(9);
        let bytes = encode_intent(tid, &[Oid(1), Oid(2)]);
        for len in 0..bytes.len() {
            assert!(decode_intent(tid, &bytes[..len]).is_err(), "{len}");
        }
        assert!(decode_intent(Tid(10), &bytes).is_err());
    }

    /// An intent file and a transaction row have the same key. Only the
    /// key space tells them apart.
    #[test]
    fn an_intent_and_a_transaction_row_do_not_verify_as_each_other() {
        let tid = Tid(9);
        let key = txn_key(tid);
        let intent = encode_intent(tid, &[Oid(1), Oid(2)]);
        assert_eq!(
            decode_frame(KeySpace::Txns, &key, &intent),
            Err(FrameError::Checksum)
        );
        let payload = [1, 2].map(|oid: u64| oid.to_be_bytes()).concat();
        let row = encode_frame(KeySpace::Txns, &key, MAIN_BRANCH, Tid::ZERO, &payload);
        assert_eq!(decode_intent(tid, &row), Err(FrameError::Checksum));
    }
}
