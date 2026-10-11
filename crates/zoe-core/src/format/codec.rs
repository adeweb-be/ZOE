//! The codecs of the frame payload.
//!
//! The codec byte of a frame says how the payload holds the data:
//!
//! | Byte | Codec | Payload |
//! | --- | --- | --- |
//! | 0 | none | the data as it is |
//! | 1 | zstd | one zstd frame that states the length of the data |
//!
//! A reader decodes every codec in this table, whatever the compression
//! setting of the store. It refuses every other byte as a frame of a newer
//! format.
//!
//! The reader checks the length that a zstd frame states before it
//! allocates. The length must be at least 1, not more than `MAX_DATA_LEN`,
//! and not more than `ZSTD_MAX_EXPANSION` times the stored length. The reader then
//! decodes into a buffer of that length. A payload that decodes to another
//! length is an error.

use std::borrow::Cow;

use zstd::bulk::{Compressor, Decompressor};
use zstd::zstd_safe;

use super::{CODEC_NONE, CODEC_ZSTD, FrameError};

/// The zstd level of the default compression.
const DEFAULT_ZSTD_LEVEL: i32 = 3;

/// The longest data that a frame holds. It is the limit of the 4-byte
/// payload length field, for every codec.
const MAX_DATA_LEN: u64 = u32::MAX as u64;

/// The most data bytes that one byte of a zstd frame can give. The
/// smallest zstd block is 4 bytes, and one block gives at most 128 KiB.
const ZSTD_MAX_EXPANSION: u64 = 32 * 1024;

/// How the payload of a frame holds the data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    /// The payload is the data as it is. The codec byte is 0.
    None,
    /// The payload is one zstd frame. The codec byte is 1.
    Zstd,
}

impl Codec {
    /// Return the codec byte.
    pub fn byte(self) -> u8 {
        match self {
            Self::None => CODEC_NONE,
            Self::Zstd => CODEC_ZSTD,
        }
    }

    /// Return the codec of `byte`, or `None` for a byte that this build
    /// cannot read.
    pub fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            CODEC_NONE => Some(Self::None),
            CODEC_ZSTD => Some(Self::Zstd),
            _ => None,
        }
    }
}

/// How a store writes the data of new records.
///
/// A store reads the records of every codec, whatever this setting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compression {
    /// Write the data as it is, with the codec `Codec::None`.
    None,
    /// Compress the data with zstd, with the codec `Codec::Zstd`. Data that
    /// zstd does not make smaller is written as it is, with `Codec::None`.
    Zstd {
        /// The zstd level. zstd clamps a level that is out of its range.
        level: i32,
    },
}

impl Default for Compression {
    /// zstd at level 3.
    fn default() -> Self {
        Self::Zstd {
            level: DEFAULT_ZSTD_LEVEL,
        }
    }
}

/// Encode `data` for the payload of a frame. Return the codec and the
/// payload.
pub(crate) fn encode(data: &[u8], compression: Compression) -> (Codec, Cow<'_, [u8]>) {
    if let Compression::Zstd { level } = compression
        && let Some(packed) = zstd_smaller(data, level)
    {
        return (Codec::Zstd, Cow::Owned(packed));
    }
    (Codec::None, Cow::Borrowed(data))
}

/// Compress `data` with zstd at `level`. Return `None` when the result is
/// not smaller than `data`.
///
/// A zstd error also gives `None`. The caller then writes the data as it
/// is, and that is always correct.
fn zstd_smaller(data: &[u8], level: i32) -> Option<Vec<u8>> {
    // The buffer is shorter than the data, so zstd fails when its result
    // is not smaller.
    let mut packed = Vec::with_capacity(data.len().checked_sub(1)?);
    let mut compressor = Compressor::new(level).ok()?;
    // The reader needs the data length in the zstd frame header.
    compressor.include_contentsize(true).ok()?;
    compressor.compress_to_buffer(data, &mut packed).ok()?;
    (packed.len() < data.len()).then_some(packed)
}

/// Decode the payload `stored` of the codec `codec`. Return the data.
pub(crate) fn decode(codec: Codec, stored: &[u8]) -> Result<Cow<'_, [u8]>, FrameError> {
    match codec {
        Codec::None => Ok(Cow::Borrowed(stored)),
        Codec::Zstd => unzstd(stored).map(Cow::Owned),
    }
}

/// Decompress the zstd payload `stored`.
fn unzstd(stored: &[u8]) -> Result<Vec<u8>, FrameError> {
    let len = zstd_data_len(stored)?;
    // The capacity of the buffer limits the output of zstd.
    let mut data = Vec::with_capacity(len);
    let written = Decompressor::new()
        .and_then(|mut decompressor| decompressor.decompress_to_buffer(stored, &mut data))
        .map_err(|_| FrameError::Payload)?;
    if written != len {
        return Err(FrameError::Payload);
    }
    Ok(data)
}

/// Return the data length that the zstd payload `stored` states, after the
/// checks.
fn zstd_data_len(stored: &[u8]) -> Result<usize, FrameError> {
    // The payload is exactly one zstd frame, with no bytes after it.
    if zstd_safe::find_frame_compressed_size(stored) != Ok(stored.len()) {
        return Err(FrameError::Payload);
    }
    let Ok(Some(len)) = zstd_safe::get_frame_content_size(stored) else {
        return Err(FrameError::Payload);
    };
    // Empty data marks a deletion. The writer never compresses empty data,
    // so a zstd payload that states 0 bytes is not valid.
    if len == 0 {
        return Err(FrameError::Payload);
    }
    let bound = (stored.len() as u64)
        .saturating_mul(ZSTD_MAX_EXPANSION)
        .min(MAX_DATA_LEN);
    if len > bound {
        return Err(FrameError::Payload);
    }
    usize::try_from(len).map_err(|_| FrameError::Payload)
}

/// zstd payloads that state no data. Empty data marks a deletion, so a
/// reader refuses each of them.
#[cfg(test)]
pub(crate) const EMPTY_ZSTD_PAYLOADS: [&[u8]; 4] = [
    // A skippable frame with 4 bytes of content.
    &[0x50, 0x2a, 0x4d, 0x18, 4, 0, 0, 0, 1, 2, 3, 4],
    // A skippable frame with no content.
    &[0x50, 0x2a, 0x4d, 0x18, 0, 0, 0, 0],
    // A frame that states 0 bytes, with one empty raw block.
    &[0x28, 0xb5, 0x2f, 0xfd, 0x20, 0, 0x01, 0, 0],
    // A frame that states 0 bytes, with two empty raw blocks.
    &[0x28, 0xb5, 0x2f, 0xfd, 0x20, 0, 0x00, 0, 0, 0x01, 0, 0],
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Bytes with the repetition of a pickle: the same keys many times.
    fn repetitive(len: usize) -> Vec<u8> {
        b"X\x05\x00\x00\x00titleq\x02X\x0b\x00\x00\x00descriptionq\x03"
            .iter()
            .copied()
            .cycle()
            .take(len)
            .collect()
    }

    /// Bytes that zstd cannot make smaller.
    fn noise(len: usize) -> Vec<u8> {
        let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect()
    }

    /// A zstd frame header with a stated content size of `len` bytes: the
    /// magic number, a descriptor for a single segment with an 8-byte
    /// size, then the size.
    fn zstd_header(len: u64) -> Vec<u8> {
        let mut frame = vec![0x28, 0xb5, 0x2f, 0xfd, 0xe0];
        frame.extend_from_slice(&len.to_le_bytes());
        frame
    }

    /// Add to `frame` a last zstd block that repeats `byte` `count` times.
    fn rle_block(mut frame: Vec<u8>, byte: u8, count: u32) -> Vec<u8> {
        let header = (count << 3) | (1 << 1) | 1;
        frame.extend_from_slice(&header.to_le_bytes()[..3]);
        frame.push(byte);
        frame
    }

    #[test]
    fn codec_bytes_are_fixed() {
        assert_eq!(CODEC_NONE, 0);
        assert_eq!(CODEC_ZSTD, 1);
        for codec in [Codec::None, Codec::Zstd] {
            assert_eq!(Codec::from_byte(codec.byte()), Some(codec));
        }
        for byte in 2..=u8::MAX {
            assert_eq!(Codec::from_byte(byte), None, "{byte}");
        }
    }

    #[test]
    fn the_default_is_zstd_at_level_3() {
        assert_eq!(Compression::default(), Compression::Zstd { level: 3 });
    }

    #[test]
    fn data_round_trips_at_every_level() {
        let inputs = [
            Vec::new(),
            b"a".to_vec(),
            repetitive(20),
            repetitive(5_000),
            repetitive(300_000),
            noise(5_000),
        ];
        let settings = [-1_000_000, -5, 0, 1, 3, 19, 22, 1_000]
            .map(|level| Compression::Zstd { level })
            .into_iter()
            .chain([Compression::None]);
        for compression in settings {
            for data in &inputs {
                let (codec, payload) = encode(data, compression);
                let back = decode(codec, &payload).unwrap();
                assert_eq!(&back[..], &data[..], "{compression:?} {}", data.len());
            }
        }
    }

    #[test]
    fn zstd_is_kept_only_when_it_is_smaller() {
        let compression = Compression::default();
        let data = repetitive(5_000);
        let (codec, payload) = encode(&data, compression);
        assert_eq!(codec, Codec::Zstd);
        assert!(payload.len() < data.len() / 4, "{}", payload.len());

        for data in [Vec::new(), b"a".to_vec(), noise(5_000)] {
            let (codec, payload) = encode(&data, compression);
            assert_eq!(codec, Codec::None);
            assert_eq!(&payload[..], &data[..]);
        }
    }

    #[test]
    fn no_compression_stores_the_data_as_it_is() {
        let data = repetitive(5_000);
        let (codec, payload) = encode(&data, Compression::None);
        assert_eq!(codec, Codec::None);
        assert!(matches!(payload, Cow::Borrowed(_)));
    }

    #[test]
    fn the_zstd_payload_states_the_data_length() {
        let data = repetitive(5_000);
        let (_, payload) = encode(&data, Compression::default());
        assert_eq!(zstd_data_len(&payload), Ok(data.len()));
    }

    #[test]
    fn a_zstd_frame_never_expands_past_the_bound() {
        // Zeros give the largest expansion that zstd makes.
        let data = vec![0; 8 << 20];
        for level in [-5, 1, 3, 19] {
            let (codec, payload) = encode(&data, Compression::Zstd { level });
            assert_eq!(codec, Codec::Zstd);
            let expansion = data.len() as u64 / payload.len() as u64;
            assert!(
                expansion <= ZSTD_MAX_EXPANSION,
                "level {level}: {expansion}"
            );
            assert_eq!(decode(codec, &payload).unwrap().len(), data.len());
        }
    }

    #[test]
    fn a_frame_that_states_a_huge_length_is_refused() {
        // 4 GiB and 1 TiB are over `MAX_DATA_LEN`. 1 GiB is in it, but a
        // 17-byte frame cannot give it.
        for len in [1 << 30, 1 << 32, 1 << 40, u64::MAX - 2] {
            let frame = rle_block(zstd_header(len), 0, 1_000);
            assert_eq!(
                decode(Codec::Zstd, &frame),
                Err(FrameError::Payload),
                "{len}"
            );
        }
    }

    #[test]
    fn a_frame_that_decodes_to_another_length_is_refused() {
        for (stated, real) in [(1_000, 999), (1_000, 1_001), (0, 1)] {
            let frame = rle_block(zstd_header(stated), 7, real);
            assert_eq!(
                decode(Codec::Zstd, &frame),
                Err(FrameError::Payload),
                "{stated} {real}"
            );
        }
        let exact = rle_block(zstd_header(1_000), 7, 1_000);
        assert_eq!(decode(Codec::Zstd, &exact).unwrap(), vec![7; 1_000]);
    }

    #[test]
    fn a_payload_that_states_no_data_is_refused() {
        for payload in EMPTY_ZSTD_PAYLOADS {
            assert_eq!(
                decode(Codec::Zstd, payload),
                Err(FrameError::Payload),
                "{payload:02x?}"
            );
        }
    }

    #[test]
    fn a_frame_without_a_stated_length_is_refused() {
        let data = repetitive(5_000);
        let mut compressor = Compressor::new(3).unwrap();
        compressor.include_contentsize(false).unwrap();
        let frame = compressor.compress(&data).unwrap();
        assert_eq!(decode(Codec::Zstd, &frame), Err(FrameError::Payload));
    }

    #[test]
    fn a_payload_that_is_not_one_zstd_frame_is_refused() {
        let data = repetitive(5_000);
        let (_, frame) = encode(&data, Compression::default());
        let two = [&frame[..], &frame[..]].concat();
        let trailing = [&frame[..], b"x"].concat();
        let short = &frame[..frame.len() - 1];
        for bad in [&two[..], &trailing[..], short, b"pickle", b""] {
            assert_eq!(decode(Codec::Zstd, bad), Err(FrameError::Payload));
        }
    }
}
