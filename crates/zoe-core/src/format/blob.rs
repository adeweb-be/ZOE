//! The payload of a blob record: the BLAKE3 hash of the blob file (32
//! bytes), then the size of the file in bytes (8 bytes, little-endian).
//! The blob record is a frame under the 16-byte record key of `(oid,
//! tid)`, with the previous serial 0.

use super::FrameError;
use crate::backend::BlobRecord;

/// The length of the payload of a blob record.
pub const BLOB_RECORD_LEN: usize = 40;

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
