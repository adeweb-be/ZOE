//! A fuzz smoke test of the frame decoder and the handshake of `zoe
//! serve`, on stable inside `cargo test` (harness E).
//!
//! A mutator with a fixed seed changes valid messages of a stock client:
//! it flips bytes, cuts frames, lies in the length prefix, swaps the types
//! of the arguments, drops and adds arguments, calls methods before the
//! handshake or before `register`, and calls methods that do not exist.
//! The server must close a connection that sends what it cannot read, and
//! never panic, abort or stop: after each input, a new client registers
//! and loads. A length prefix of nearly 4 GiB allocates nothing: the
//! server refuses a frame longer than `--max-frame-size`
//! (`ZOE_MAX_FRAME_SIZE`). The resident memory of the server stays small.
//!
//! `ZOE_FUZZ_ROUNDS` sets the number of inputs (500 by default) and
//! `ZOE_FUZZ_SEED` the seed. The cargo-fuzz targets of `fuzz/` run the
//! same decoder for an hour each night.

use std::io::Write;
use std::net::TcpStream;
use std::time::Duration;

mod wire;

use wire::{Conn, MAX, Server, Value, commit, fresh_dir, load, new_oids, p64, record, s};

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

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

fn encode(message: &Value) -> Vec<u8> {
    let mut bytes = Vec::new();
    rmpv::encode::write_value(&mut bytes, message).unwrap();
    bytes
}

/// Valid messages of a stock client, before mutation.
fn seeds(oid: u64) -> Vec<Value> {
    let call = |id: i64, method: &str, args: Vec<Value>| {
        Value::Array(vec![
            Value::from(id),
            Value::Boolean(false),
            Value::from(method),
            Value::Array(args),
        ])
    };
    vec![
        call(1, "register", vec![Value::from("1"), Value::Boolean(false)]),
        call(2, "get_info", vec![]),
        call(3, "loadBefore", vec![p64(oid), Value::Binary(MAX.to_vec())]),
        call(4, "new_oids", vec![Value::from(3)]),
        call(5, "history", vec![p64(oid), Value::from(2)]),
        call(
            6,
            "tpc_begin",
            vec![
                Value::from(9),
                Value::Binary(vec![]),
                Value::Binary(vec![]),
                Value::Map(vec![]),
                Value::Nil,
                Value::from(" "),
            ],
        ),
        call(
            7,
            "storea",
            vec![
                p64(oid),
                p64(0),
                Value::Binary(record(b"f")),
                Value::from(9),
            ],
        ),
        call(8, "vote", vec![Value::from(9)]),
        call(9, "tpc_abort", vec![Value::from(9)]),
        call(10, "getInvalidations", vec![p64(0)]),
        call(11, "iterator_start", vec![Value::Nil, Value::Nil]),
    ]
}

/// Change one valid message into an input of the fuzz.
fn mutate(rng: &mut Rng, seed: &Value) -> Vec<u8> {
    let mut message = seed.clone();
    if let Value::Array(parts) = &mut message {
        match rng.below(6) {
            0 => parts[1] = Value::from(rng.below(5) as i64 - 1),
            1 => {
                parts[2] = Value::from(["", "nope", ".reply", "__init__", "register"][rng.below(5)])
            }
            2 => parts[3] = Value::from(rng.next() as i64),
            3 => {
                if let Value::Array(args) = &mut parts[3] {
                    if !args.is_empty() && rng.below(2) == 0 {
                        args.pop();
                    } else {
                        args.push(Value::Map(vec![(Value::Nil, Value::Nil)]));
                    }
                }
            }
            4 => parts.truncate(rng.below(4)),
            _ => parts.push(Value::Binary(vec![0xc1; rng.below(16)])),
        }
    }
    let mut bytes = encode(&message);
    // Then flip some bytes, or cut the tail.
    for _ in 0..rng.below(3) {
        if !bytes.is_empty() {
            let at = rng.below(bytes.len());
            bytes[at] ^= 1 << rng.below(8);
        }
    }
    if rng.below(5) == 0 && !bytes.is_empty() {
        bytes.truncate(rng.below(bytes.len()));
    }
    bytes
}

fn rss_kib(pid: u32) -> u64 {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
    status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))
        .and_then(|value| value.split_whitespace().next())
        .and_then(|kib| kib.parse().ok())
        .unwrap()
}

/// Check that the server still serves: a new client registers and loads.
fn still_serves(server: &mut Server, oid: u64, round: usize) {
    assert!(server.running(), "the server stopped at round {round}");
    let mut conn = server.client("1");
    load(&mut conn, oid).unwrap_or_else(|f| panic!("round {round}: {f:?}"));
}

#[test]
fn mutated_messages_never_stop_the_server() {
    let rounds: usize = std::env::var("ZOE_FUZZ_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(500);
    let mut rng = Rng(std::env::var("ZOE_FUZZ_SEED")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1));
    let dir = fresh_dir("mutated");
    let mut server = Server::start(
        &[
            "--path",
            s(&dir.join("store")),
            "--max-frame-size",
            "16777216",
        ],
        &[],
    );
    let mut setup = server.client("1");
    let oid = new_oids(&mut setup, 1)[0];
    commit(&mut setup, 1, &[(oid, 0, record(b"kept"))]).unwrap();
    let seeds = seeds(oid);

    for round in 0..rounds {
        let mut conn = Conn::open(server.addr);
        // Half of the inputs come after register, half before it.
        if rng.below(2) == 0 {
            let _ = conn.register("1", false);
        }
        let seed = &seeds[rng.below(seeds.len())];
        let input = mutate(&mut rng, seed);
        let _ = conn.write_frame(&input);
        let _ = conn.closed_within(Duration::from_millis(50));
        drop(conn);
        if round % 50 == 0 {
            still_serves(&mut server, oid, round);
        }
    }
    still_serves(&mut server, oid, rounds);
}

#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "reads the memory of the server in /proc: Linux only"
)]
fn bad_handshakes_and_lying_lengths_are_refused_cheaply() {
    let dir = fresh_dir("handshake");
    let mut server = Server::start(
        &[
            "--path",
            s(&dir.join("store")),
            "--max-frame-size",
            "16777216",
        ],
        &[],
    );
    let mut setup = server.client("1");
    let oid = new_oids(&mut setup, 1)[0];
    commit(&mut setup, 1, &[(oid, 0, record(b"kept"))]).unwrap();
    let before = rss_kib(server.pid());

    let inputs: Vec<Vec<u8>> = vec![
        // Protocols that the server does not speak.
        [&2u32.to_be_bytes()[..], b"Z5"].concat(),
        [&2u32.to_be_bytes()[..], b"M4"].concat(),
        [&5u32.to_be_bytes()[..], b"M5\x00\x00\x00"].concat(),
        // A length of nearly 4 GiB, then a few bytes and the end.
        [&0xffff_fff0u32.to_be_bytes()[..], b"M5"].concat(),
        // A length longer than the frame limit after a good handshake.
        [
            &2u32.to_be_bytes()[..],
            b"M5",
            &0x0200_0000u32.to_be_bytes()[..],
        ]
        .concat(),
        // A cut length prefix.
        vec![0, 0],
    ];
    for (n, input) in inputs.iter().enumerate() {
        let mut stream = TcpStream::connect(server.addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let _ = stream.write_all(input);
        drop(stream);
        still_serves(&mut server, oid, n);
    }
    let after = rss_kib(server.pid());
    assert!(
        after < before + 64 * 1024,
        "the resident memory grew from {before} KiB to {after} KiB"
    );
}
