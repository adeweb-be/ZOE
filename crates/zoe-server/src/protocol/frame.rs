//! The read and the write of the frames of the ZEO protocol on a stream.
//! `zoe_wire::frame` holds the bytes of a frame: a 4-byte big-endian
//! length, then the payload. The read of a frame has a limit, the option
//! `--max-frame-size` of `zoe serve`.

use std::io::{self, IoSlice};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use zoe_wire::frame::{self, RESERVE};

/// The frame of a payload, for the tests of the crate.
#[cfg(test)]
pub(crate) use zoe_wire::frame::encode;

/// Read one frame of at most `max` bytes from `reader`. Return `None` at
/// the end of the stream before a frame starts. An end inside a frame
/// fails with `UnexpectedEof`. A length over `max` fails with `InvalidData`
/// before the read reserves memory for the frame.
pub(crate) async fn read<R: AsyncRead + Unpin>(
    reader: &mut R,
    max: u32,
) -> io::Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    let mut got = 0;
    while got < len.len() {
        let n = reader.read(&mut len[got..]).await?;
        if n == 0 {
            if got == 0 {
                return Ok(None);
            }
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        got += n;
    }
    let len = frame::length(len);
    if len > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("a frame of {len} bytes is longer than --max-frame-size, {max} bytes"),
        ));
    }
    let len = len as usize;
    let mut payload = Vec::with_capacity(len.min(RESERVE));
    let read = reader.take(len as u64).read_to_end(&mut payload).await?;
    if read < len {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    Ok(Some(payload))
}

/// Write the frame of `payload` to `writer`.
pub(crate) async fn write<W: AsyncWrite + Unpin>(writer: &mut W, payload: &[u8]) -> io::Result<()> {
    write_parts(writer, &[payload]).await
}

/// Write the frame of the payload `parts` to `writer`: its length, then
/// each part in its order. The bytes of the parts go to the writer as they
/// are, with no copy, in one vectored write when the writer takes them all.
/// A payload longer than `u32::MAX` bytes fails with `InvalidInput`.
pub(crate) async fn write_parts<W: AsyncWrite + Unpin>(
    writer: &mut W,
    parts: &[&[u8]],
) -> io::Result<()> {
    let len: usize = parts.iter().map(|part| part.len()).sum();
    let len = frame::header(len)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "frame too long"))?;
    let mut slices: Vec<IoSlice<'_>> = std::iter::once(&len[..])
        .chain(parts.iter().copied())
        .filter(|part| !part.is_empty())
        .map(IoSlice::new)
        .collect();
    let mut slices = &mut slices[..];
    while !slices.is_empty() {
        let written = writer.write_vectored(slices).await?;
        if written == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        IoSlice::advance_slices(&mut slices, written);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A limit that no frame of these tests reaches.
    const MAX: u32 = u32::MAX;

    fn block_on<F: Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(future)
    }

    #[test]
    fn a_frame_is_the_length_then_the_bytes() {
        assert_eq!(encode(b"M5").unwrap(), b"\0\0\0\x02M5");
        assert_eq!(encode(b"").unwrap(), b"\0\0\0\0");
    }

    #[test]
    fn frames_read_back_in_order() {
        let mut stream = Vec::new();
        for payload in [&b"M5"[..], b"", &[7u8; 300]] {
            stream.extend(encode(payload).unwrap());
        }
        let mut reader = stream.as_slice();
        block_on(async {
            assert_eq!(read(&mut reader, MAX).await.unwrap().unwrap(), b"M5");
            assert_eq!(read(&mut reader, MAX).await.unwrap().unwrap(), b"");
            assert_eq!(
                read(&mut reader, MAX).await.unwrap().unwrap(),
                vec![7u8; 300]
            );
            assert_eq!(read(&mut reader, MAX).await.unwrap(), None);
        });
    }

    /// A writer that takes at most 3 bytes for each call, from the first
    /// buffer that is not empty.
    struct Slow(Vec<u8>);

    impl AsyncWrite for Slow {
        fn poll_write(
            mut self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            bytes: &[u8],
        ) -> std::task::Poll<io::Result<usize>> {
            let n = bytes.len().min(3);
            self.0.extend_from_slice(&bytes[..n]);
            std::task::Poll::Ready(Ok(n))
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[test]
    fn the_parts_of_a_frame_go_out_whole_and_in_order() {
        let parts: [&[u8]; 4] = [b"head", b"", b"the data", b"t"];
        let mut slow = Slow(Vec::new());
        block_on(write_parts(&mut slow, &parts)).unwrap();
        assert_eq!(slow.0, encode(b"headthe datat").unwrap());
        // A writer that takes all the parts at once gets the same bytes.
        let mut whole = Vec::new();
        block_on(write_parts(&mut whole, &parts)).unwrap();
        assert_eq!(whole, slow.0);
    }

    #[test]
    fn a_frame_that_arrives_in_pieces_reads_whole() {
        let frame = encode(b"one frame").unwrap();
        block_on(async {
            let (mut client, mut server) = tokio::io::duplex(4);
            let writer = async {
                for byte in &frame {
                    client.write_all(&[*byte]).await.unwrap();
                }
                drop(client);
            };
            let reader = async {
                let first = read(&mut server, MAX).await.unwrap();
                let end = read(&mut server, MAX).await.unwrap();
                (first, end)
            };
            let ((), (first, end)) = tokio::join!(writer, reader);
            assert_eq!(first.unwrap(), b"one frame");
            assert_eq!(end, None);
        });
    }

    #[test]
    fn an_end_inside_a_frame_is_an_error() {
        for cut in [1, 3, 4, 6] {
            let frame = encode(b"abcdef").unwrap();
            let mut reader = &frame[..cut];
            let error = block_on(read(&mut reader, MAX)).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof, "cut at {cut}");
        }
    }

    #[test]
    fn a_false_length_reserves_no_memory() {
        let mut reader = &b"\xff\xff\xff\xffab"[..];
        let error = block_on(read(&mut reader, MAX)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn a_frame_longer_than_the_limit_is_refused_before_its_bytes() {
        // The length says 32 MiB, and no byte of the frame follows.
        let mut reader = &b"\x02\x00\x00\x00"[..];
        let error = block_on(read(&mut reader, 16 << 20)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("33554432"), "{error}");
        // A frame of the limit reads whole.
        let frame = encode(b"four").unwrap();
        let mut reader = frame.as_slice();
        assert_eq!(block_on(read(&mut reader, 4)).unwrap().unwrap(), b"four");
        let mut reader = frame.as_slice();
        let error = block_on(read(&mut reader, 3)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn write_sends_one_frame() {
        let mut out = Vec::new();
        block_on(write(&mut out, b"M5")).unwrap();
        assert_eq!(out, b"\0\0\0\x02M5");
    }
}
