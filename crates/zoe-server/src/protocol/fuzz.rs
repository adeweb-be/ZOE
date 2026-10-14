//! The entry of the fuzz targets in `fuzz/`: the frame decoder and the
//! message decoder of `zoe serve`, on any bytes.
//!
//! This module compiles with the feature `fuzz`, and in the unit tests of
//! the crate.

use std::pin::pin;
use std::task::{Context, Poll, Waker};

use super::{PROTOCOL, frame, message};

/// Read `data` as the server reads the bytes of a client: the protocol
/// `M5`, then frames of at most `max_frame_size` bytes, each one a message.
/// Stop where the server closes the connection: at the end of `data`, at a
/// frame that is too long or cut, or at a frame that is not a message.
///
/// Return the number of messages after the protocol, heartbeats included.
/// The function never panics.
pub fn read_messages(data: &[u8], max_frame_size: u32) -> usize {
    let mut reader = data;
    match read_now(&mut reader, max_frame_size) {
        Some(protocol) if protocol == PROTOCOL => {}
        _ => return 0,
    }
    let mut messages = 0;
    while let Some(bytes) = read_now(&mut reader, max_frame_size) {
        if message::decode(&bytes).is_err() {
            break;
        }
        messages += 1;
    }
    messages
}

/// Read one frame of at most `max` bytes from `reader`. Return `None` at
/// the end of the bytes, or at a frame that the read refuses. A read from
/// bytes in memory never waits, so one poll ends it.
fn read_now(reader: &mut &[u8], max: u32) -> Option<Vec<u8>> {
    let mut read = pin!(frame::read(reader, max));
    match read.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(Ok(frame)) => frame,
        Poll::Ready(Err(_)) | Poll::Pending => None,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use rmpv::Value;

    use super::*;

    /// A limit that no frame of these tests reaches.
    const MAX: u32 = u32::MAX;

    /// The splitmix64 generator: small, fast and the same on every machine.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        }

        /// Return a number from 0 to `n - 1`. `n` is not 0.
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    /// Return the stream of `payloads`: each one as a frame.
    fn stream(payloads: &[&[u8]]) -> Vec<u8> {
        payloads
            .iter()
            .flat_map(|payload| frame::encode(payload).unwrap())
            .collect()
    }

    /// Return the frame payload of the call `method` with `args`.
    fn call(id: i64, method: &str, args: Vec<Value>) -> Vec<u8> {
        let message = Value::Array(vec![
            Value::from(id),
            Value::Boolean(false),
            Value::from(method),
            Value::Array(args),
        ]);
        let mut bytes = Vec::new();
        rmpv::encode::write_value(&mut bytes, &message).unwrap();
        bytes
    }

    fn register() -> Vec<u8> {
        call(1, "register", vec![Value::from("1"), Value::Boolean(false)])
    }

    /// The heartbeat of ZEO 6.2: `[-1, 0, ".reply", nil]`.
    const HEARTBEAT: [u8; 11] = [
        0x94, 0xff, 0x00, 0xa6, b'.', b'r', b'e', b'p', b'l', b'y', 0xc0,
    ];

    #[test]
    fn the_opening_of_a_stock_client_reads_whole() {
        let get_info = call(2, "get_info", vec![]);
        let data = stream(&[PROTOCOL, &register(), &get_info, &HEARTBEAT]);
        assert_eq!(read_messages(&data, MAX), 3);
    }

    #[test]
    fn another_protocol_or_ruok_reads_no_message() {
        for protocol in [&b"Z5"[..], b"M4", b"M5\0", b"ruok", b""] {
            let data = stream(&[protocol, &register()]);
            assert_eq!(read_messages(&data, MAX), 0, "{protocol:?}");
        }
        assert_eq!(read_messages(b"", MAX), 0);
        assert_eq!(read_messages(b"\0\0", MAX), 0);
    }

    #[test]
    fn a_frame_over_the_limit_stops_the_read() {
        let register = register();
        let limit = u32::try_from(register.len()).unwrap();
        let long = call(2, "set_client_label", vec![Value::from("a label")]);
        let data = stream(&[PROTOCOL, &register, &long, &register]);
        assert_eq!(read_messages(&data, limit), 1);
        // The frame of the protocol has a limit too.
        assert_eq!(read_messages(&data, 1), 0);
    }

    #[test]
    fn a_cut_frame_or_a_frame_that_is_not_a_message_stops_the_read() {
        let register = register();
        let mut data = stream(&[PROTOCOL, &register, &register]);
        data.truncate(data.len() - 1);
        assert_eq!(read_messages(&data, MAX), 1);
        let data = stream(&[PROTOCOL, &register, b"\xc1", &register]);
        assert_eq!(read_messages(&data, MAX), 1);
        let mut deep = vec![0x91; 1_000];
        deep.push(0xc0);
        let data = stream(&[PROTOCOL, &deep, &register]);
        assert_eq!(read_messages(&data, MAX), 0);
    }

    #[test]
    fn a_length_of_nearly_4_gib_reserves_no_memory() {
        let mut data = stream(&[PROTOCOL]);
        data.extend_from_slice(&0xffff_fff0u32.to_be_bytes());
        data.extend_from_slice(&register());
        assert_eq!(read_messages(&data, MAX), 0);
    }

    /// The seeds of the fuzz target `name`, with the name of each file.
    fn corpus(name: &str) -> Vec<(String, Vec<u8>)> {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("fuzz/corpus")
            .join(name);
        let mut seeds: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| {
                let path = entry.unwrap().path();
                let name = path.file_name().unwrap().to_string_lossy().into_owned();
                (name, std::fs::read(&path).unwrap())
            })
            .collect();
        seeds.sort();
        seeds
    }

    /// Return the number of frames in `data`, which holds whole frames.
    fn frames(mut data: &[u8]) -> usize {
        let mut n = 0;
        while let Some((len, rest)) = data.split_first_chunk::<4>() {
            data = &rest[u32::from_be_bytes(*len) as usize..];
            n += 1;
        }
        n
    }

    #[test]
    fn each_seed_of_the_corpus_reads_whole() {
        for target in ["frame", "handshake"] {
            let seeds = corpus(target);
            assert!(seeds.len() >= 5, "{target}: {} seeds", seeds.len());
            for (name, data) in seeds {
                if name.starts_with("ruok") {
                    assert_eq!(read_messages(&data, MAX), 0, "{target}/{name}");
                } else {
                    let messages = frames(&data) - 1;
                    assert!(messages > 0, "{target}/{name}");
                    assert_eq!(read_messages(&data, MAX), messages, "{target}/{name}");
                }
            }
        }
    }

    #[test]
    fn mutated_seeds_never_panic() {
        let seeds: Vec<Vec<u8>> = corpus("frame").into_iter().map(|(_, data)| data).collect();
        let mut rng = Rng(1);
        for _ in 0..20_000 {
            let mut data = seeds[rng.below(seeds.len())].clone();
            for _ in 0..1 + rng.below(8) {
                let at = rng.below(data.len());
                match rng.below(4) {
                    0 => data[at] ^= 1 << rng.below(8),
                    1 => data[at] = [0x00, 0x7f, 0x80, 0xc1, 0xdd, 0xff][rng.below(6)],
                    2 => data.truncate(at.max(1)),
                    _ => data.insert(at, rng.next() as u8),
                }
            }
            let limit = [MAX, 1 << 10, 64][rng.below(3)];
            read_messages(&data, limit);
        }
    }
}
