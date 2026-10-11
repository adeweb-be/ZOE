//! A fuzz smoke test that runs on stable inside `cargo test`.
//!
//! A mutator with a fixed seed changes the records of the corpus and some
//! hand-written records of protocols 0 to 3. The decoder must not panic on
//! any input. Each input that it accepts must encode to the same bytes,
//! clone into an equal tree, and print, walk and resolve without a panic.

use std::panic;
use std::path::PathBuf;

use zoe_pickle::{Node, decode_record, encode_record};

/// The mutated inputs per seed record.
const ROUNDS: usize = 300;

/// The bytes that the mutator writes as opcodes: each opcode that the
/// decoder reads, and some of protocol 4 that it refuses.
const OPCODES: &[u8] = b"(.012FILNPRSVabcdgilpstGJKMQTUXehj]oqru)}BC\
    \x80\x81\x82\x83\x84\x85\x86\x87\x88\x89\x8a\x8b\x8c\x94\x95";

/// The arguments that the mutator writes after an opcode.
const ARGUMENTS: &[&[u8]] = &[
    b"",
    b"\x00",
    b"\x01",
    b"\x02\x00",
    b"\x01\x00\x00\x00",
    b"\xff\xff\xff\x7f",
    b"0\n",
    b"1\n",
    b"01\n",
    b"-1L\n",
    b"'a'\n",
    b"m\nC\n",
];

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

    /// Returns a number from 0 to `n - 1`. `n` is not 0.
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

/// Returns the records of the corpus and the hand-written records.
fn seeds() -> Vec<Vec<u8>> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus");
    let mut paths: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "record"))
        .collect();
    paths.sort();
    let mut seeds: Vec<Vec<u8>> = paths
        .iter()
        .map(|path| std::fs::read(path).unwrap())
        .collect();
    let class: &[u8] = b"\x80\x03cm\nC\nq\x00.";
    let class0: &[u8] = b"cpersistent.mapping\nPersistentMapping\n.";
    let nested = [&b"\x80\x03)"[..], &[0x85; 50], b"."].concat();
    for record in [
        [class0, b"(dp0\nVa\np1\nI42\nsVb\np2\nL11L\nsVc\np3\nI01\nsVd\np4\n(lp5\nI1\naF1.5\naS'x'\nag5\nas."].concat(),
        [class0, b"}q\x02(U\x01xq\x03(cmod\nOld\nq\x04oq\x05}q\x06bU\x01yq\x07(imod\nOld\nq\x08}q\tbu."].concat(),
        [class, b"\x80\x03(K\x07q\x01K\x09q\x01h\x01]q\x02h\x02\x85q\x03a0h\x03t."].concat(),
        [class, b"\x80\x03(K\x012(K\x021N\x82\x05Pabc\ncbuiltins\nset\n)\x85Rt."].concat(),
        [class, &nested].concat(),
        b"\x80\x03X\x01\x00\x00\x00mq\x00X\x01\x00\x00\x00Cq\x01\x86q\x02)\x86q\x03.\x80\x03}q\x04.".to_vec(),
        b"((U\x01mq\x00U\x01Cq\x01tq\x02Ntq\x03.}q\x04.".to_vec(),
    ] {
        seeds.push(record);
    }
    seeds
}

/// Changes one to three places of an input.
fn mutate(rng: &mut Rng, input: &[u8], seeds: &[Vec<u8>]) -> Vec<u8> {
    let mut out = input.to_vec();
    for _ in 0..1 + rng.below(3) {
        let at = rng.below(out.len() + 1);
        let len = 1 + rng.below(8);
        let end = (at + len).min(out.len());
        match rng.below(8) {
            0 if at < out.len() => out[at] ^= 1 << rng.below(8),
            1 if at < out.len() => out[at] = rng.next() as u8,
            2 if at < out.len() => out[at] = *rng.pick(OPCODES),
            3 => {
                let insert = [&[*rng.pick(OPCODES)][..], rng.pick(ARGUMENTS)].concat();
                out.splice(at..at, insert);
            }
            4 => {
                out.drain(at..end);
            }
            5 => {
                let copy = out[at..end].to_vec();
                out.splice(at..at, copy);
            }
            6 => out.truncate(at),
            _ => {
                let other = rng.pick(seeds);
                let from = rng.below(other.len());
                let to = (from + len).min(other.len());
                out.splice(at..end, other[from..to].iter().copied());
            }
        }
    }
    out
}

/// Runs every check on one input. Returns whether the decoder accepts it.
fn check(input: &[u8]) -> bool {
    let outcome = panic::catch_unwind(|| {
        let Ok(tree) = decode_record(input) else {
            return false;
        };
        let encoded = encode_record(&tree).expect("an accepted tree encodes");
        assert_eq!(encoded, input, "decode then encode changed the bytes");
        let copy = tree.clone();
        assert_eq!(copy, tree);
        assert!(!format!("{copy:?}").is_empty());
        let resolver = tree.resolver();
        for root in [tree.class(), tree.state()] {
            for node in root.walk().filter(|node| matches!(node, Node::Get(_))) {
                let _ = resolver.resolve(node);
            }
        }
        let _ = tree.class_name();
        true
    });
    outcome.unwrap_or_else(|_| panic!("the checks panic on the input {input:02x?}"))
}

#[test]
fn mutated_records_never_panic_and_round_trip_when_accepted() {
    let seeds = seeds();
    let mut rng = Rng(0x5eed);
    let (mut accepted, mut refused) = (0, 0);
    for seed in &seeds {
        assert!(check(seed), "a seed record is refused: {seed:02x?}");
        for _ in 0..ROUNDS {
            let input = mutate(&mut rng, seed, &seeds);
            if check(&input) {
                accepted += 1;
            } else {
                refused += 1;
            }
        }
    }
    eprintln!("fuzz smoke: {accepted} inputs accepted, {refused} refused");
    // A mutator that only breaks records, or never does, tests little.
    assert!(
        accepted * 20 > refused,
        "{accepted} accepted, {refused} refused"
    );
    assert!(
        refused * 20 > accepted,
        "{accepted} accepted, {refused} refused"
    );
}
