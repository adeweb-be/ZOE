//! The frames of the ZEO protocol: a 4-byte big-endian length, then the
//! bytes of the frame.

use std::io;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// The largest buffer that a read reserves before the bytes arrive. A
/// longer frame grows its buffer as its bytes arrive, so a false length
/// does not reserve memory.
const RESERVE: usize = 1 << 20;

/// Return the frame of `payload`: its length, then its bytes. A payload
/// longer than `u32::MAX` bytes fails with `InvalidInput`.
pub(crate) fn encode(payload: &[u8]) -> io::Result<Vec<u8>> {
    let len = u32::try_from(payload.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "frame too long"))?;
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&len.to_be_bytes());
    frame.extend_from_slice(payload);
    Ok(frame)
}

/// Read one frame from `reader`. Return `None` at the end of the stream
/// before a frame starts. An end inside a frame fails with
/// `UnexpectedEof`.
pub(crate) async fn read<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<Option<Vec<u8>>> {
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
    let len = u32::from_be_bytes(len) as usize;
    let mut payload = Vec::with_capacity(len.min(RESERVE));
    let read = reader.take(len as u64).read_to_end(&mut payload).await?;
    if read < len {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    Ok(Some(payload))
}

/// Write the frame of `payload` to `writer`.
pub(crate) async fn write<W: AsyncWrite + Unpin>(writer: &mut W, payload: &[u8]) -> io::Result<()> {
    writer.write_all(&encode(payload)?).await
}

#[cfg(test)]
mod tests {
    use super::*;

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
            assert_eq!(read(&mut reader).await.unwrap().unwrap(), b"M5");
            assert_eq!(read(&mut reader).await.unwrap().unwrap(), b"");
            assert_eq!(read(&mut reader).await.unwrap().unwrap(), vec![7u8; 300]);
            assert_eq!(read(&mut reader).await.unwrap(), None);
        });
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
                let first = read(&mut server).await.unwrap();
                let end = read(&mut server).await.unwrap();
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
            let error = block_on(read(&mut reader)).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof, "cut at {cut}");
        }
    }

    #[test]
    fn a_false_length_reserves_no_memory() {
        let mut reader = &b"\xff\xff\xff\xffab"[..];
        let error = block_on(read(&mut reader)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn write_sends_one_frame() {
        let mut out = Vec::new();
        block_on(write(&mut out, b"M5")).unwrap();
        assert_eq!(out, b"\0\0\0\x02M5");
    }
}
