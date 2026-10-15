//! The payload of a pickle, encoded before the finish.
//!
//! A skin can encode a staged pickle on another thread with
//! `Encoded::encode`, and give the result to its transaction with
//! `Txn::set_encoded`. The finish then writes that payload and does not
//! encode the pickle again. Only the core makes an `Encoded`, with the
//! codec of the store, so the frame is the frame of an inline encode.

use std::fmt;

use xxhash_rust::xxh3::xxh3_64;

use crate::format::{self, Codec, Compression};

/// A pickle encoded for the payload of its frame, with the length and the
/// xxh3-64 hash of the pickle. `Txn::set_encoded` attaches it only to a
/// staged pickle with the same length and hash.
pub struct Encoded {
    /// The compression that made the payload.
    pub(crate) compression: Compression,
    /// The length of the pickle.
    pub(crate) len: usize,
    /// The xxh3-64 hash of the pickle.
    pub(crate) hash: u64,
    /// The zstd payload, or `None` when the frame holds the pickle as it
    /// is.
    pub(crate) packed: Option<Vec<u8>>,
}

impl Encoded {
    /// Encode `data` with `compression`, as the finish of a store with that
    /// compression encodes it. This is the work that the finish does not do
    /// for an attached payload. It needs no store and no transaction, so
    /// any thread can run it.
    pub fn encode(data: &[u8], compression: Compression) -> Self {
        let (codec, payload) = format::encode_payload(data, compression);
        let packed = match codec {
            Codec::Zstd => Some(payload.into_owned()),
            Codec::None => None,
        };
        Self {
            compression,
            len: data.len(),
            hash: xxh3_64(data),
            packed,
        }
    }

    /// Return the length of the pickle.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Tell if the pickle is empty: a deletion.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Return the length of the payload in its frame.
    pub fn stored_len(&self) -> usize {
        self.packed.as_ref().map_or(self.len, Vec::len)
    }

    /// Return the codec of the payload.
    pub fn codec(&self) -> Codec {
        if self.packed.is_some() {
            Codec::Zstd
        } else {
            Codec::None
        }
    }
}

impl fmt::Debug for Encoded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Encoded")
            .field("compression", &self.compression)
            .field("len", &self.len)
            .field("codec", &self.codec())
            .field("stored_len", &self.stored_len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_payload_is_the_payload_of_an_inline_encode() {
        let compressible = b"persistent mapping ".repeat(500);
        let noise: Vec<u8> = (0..5_000u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect();
        for data in [&compressible[..], &noise[..], b"x", b""] {
            for compression in [Compression::None, Compression::default()] {
                let encoded = Encoded::encode(data, compression);
                let (codec, payload) = format::encode_payload(data, compression);
                assert_eq!(encoded.codec(), codec);
                assert_eq!(encoded.stored_len(), payload.len());
                if let Some(packed) = &encoded.packed {
                    assert_eq!(&packed[..], &payload[..]);
                }
                assert_eq!((encoded.len(), encoded.hash), (data.len(), xxh3_64(data)));
            }
        }
    }
}
