//! The heads of a msgpack bin and of a msgpack array, read and written
//! without a decoder. The server writes the head of the bin of a record,
//! so the bytes of the record go to the socket without a copy. The client
//! reads the head of a long reply, so the bytes of its bin go from the
//! socket into a buffer of their own.

/// Write the head of a bin of `len` bytes to `out`: bin 8, bin 16 or bin
/// 32, the shortest that holds `len`, as rmpv writes it.
pub fn write_bin_head(out: &mut Vec<u8>, len: u32) {
    if let Ok(len) = u8::try_from(len) {
        out.extend_from_slice(&[0xc4, len]);
    } else if let Ok(len) = u16::try_from(len) {
        out.push(0xc5);
        out.extend_from_slice(&len.to_be_bytes());
    } else {
        out.push(0xc6);
        out.extend_from_slice(&len.to_be_bytes());
    }
}

/// Read the head of a msgpack array at the start of `bytes`, and move
/// `bytes` past it. Return its number of items. Return `None` for another
/// value, and for a head that `bytes` cuts.
pub fn array_len(bytes: &mut &[u8]) -> Option<usize> {
    let (&marker, rest) = bytes.split_first()?;
    let (len, size) = match marker {
        0x90..=0x9f => (usize::from(marker & 0x0f), 0),
        0xdc => (big_endian(rest, 2)?, 2),
        0xdd => (big_endian(rest, 4)?, 4),
        _ => return None,
    };
    *bytes = &rest[size..];
    Some(len)
}

/// Read the head of a msgpack bin at the start of `bytes`, and move
/// `bytes` past it. Return its number of bytes. Return `None` for another
/// value, and for a head that `bytes` cuts.
pub fn bin_len(bytes: &mut &[u8]) -> Option<usize> {
    let (&marker, rest) = bytes.split_first()?;
    let size = match marker {
        0xc4 => 1,
        0xc5 => 2,
        0xc6 => 4,
        _ => return None,
    };
    let len = big_endian(rest, size)?;
    *bytes = &rest[size..];
    Some(len)
}

/// Return the big-endian number in the first `size` bytes of `bytes`, or
/// `None` when `bytes` is shorter. `size` is at most 8.
pub fn big_endian(bytes: &[u8], size: usize) -> Option<usize> {
    let bytes = bytes.get(..size)?;
    Some(bytes.iter().fold(0, |n, &byte| n << 8 | usize::from(byte)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bin_head_takes_the_shortest_form() {
        for (len, head) in [
            (0, vec![0xc4, 0]),
            (255, vec![0xc4, 255]),
            (256, vec![0xc5, 1, 0]),
            (65_535, vec![0xc5, 255, 255]),
            (65_536, vec![0xc6, 0, 1, 0, 0]),
        ] {
            let mut out = Vec::new();
            write_bin_head(&mut out, len);
            assert_eq!(out, head, "{len}");
            let mut bytes = out.as_slice();
            assert_eq!(bin_len(&mut bytes), Some(len as usize));
            assert!(bytes.is_empty());
        }
    }

    #[test]
    fn the_heads_read_and_move_past_their_bytes() {
        let mut bytes: &[u8] = &[0x93, 0xdc, 0, 20, 0xdd, 0, 1, 0, 0, 0xc0];
        assert_eq!(array_len(&mut bytes), Some(3));
        assert_eq!(array_len(&mut bytes), Some(20));
        assert_eq!(array_len(&mut bytes), Some(65_536));
        assert_eq!(array_len(&mut bytes), None);
        assert_eq!(bytes, [0xc0]);
        // A cut head reads nothing and moves nothing.
        let mut cut: &[u8] = &[0xc5, 1];
        assert_eq!(bin_len(&mut cut), None);
        assert_eq!(cut, [0xc5, 1]);
        assert_eq!(big_endian(&[1, 2, 3], 4), None);
        assert_eq!(big_endian(&[1, 2, 3], 2), Some(0x0102));
    }
}
