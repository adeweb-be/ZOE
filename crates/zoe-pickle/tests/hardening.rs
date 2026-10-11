//! Acceptance test of the build of zoe-pickle: what the spike lacked
//! (see docs/spikes/pickle-round-trip.md). Deep nesting never aborts the
//! process, every ZODB class format names its class, the protocol-0 and
//! protocol-1 opcodes of Python 2 stores round-trip, a memo get resolves
//! to the latest put before it, and input that Python refuses is refused.

use std::thread;

use zoe_pickle::{Int, Node, decode_record, encode_record};

/// A protocol-3 class pickle: GLOBAL, BINPUT 0, STOP.
const CLASS: &[u8] = b"\x80\x03cm\nC\nq\x00.";

/// The stack of a thread that Rust spawns, and of a `cargo test` thread.
const SMALL_STACK: usize = 2 << 20;

fn record(class: &[u8], state: &[u8]) -> Vec<u8> {
    [class, state].concat()
}

/// A state pickle: an empty tuple wrapped in `depth` one-item tuples.
fn nested_tuples(depth: usize) -> Vec<u8> {
    let mut state = b"\x80\x03)".to_vec();
    state.extend(std::iter::repeat_n(b'\x85', depth));
    state.push(b'.');
    state
}

/// Decode, encode, clone, compare and drop a record on a small stack.
/// Returns whether the record round-trips; an abort fails the whole test.
fn round_trip_on_a_small_stack(bytes: Vec<u8>) -> Result<bool, String> {
    thread::Builder::new()
        .stack_size(SMALL_STACK)
        .spawn(move || match decode_record(&bytes) {
            Ok(tree) => {
                let copy = tree.clone();
                assert!(copy == tree);
                let encoded = encode_record(&tree).map_err(|e| e.to_string())?;
                drop(copy);
                drop(tree);
                Ok(encoded == bytes)
            }
            Err(e) => Err(e.to_string()),
        })
        .unwrap()
        .join()
        .unwrap()
}

#[test]
fn nesting_that_zodb_writes_round_trips_on_a_small_stack() {
    // CPython's pickler writes tuples 5,000 levels deep.
    let bytes = record(CLASS, &nested_tuples(5_000));
    assert_eq!(round_trip_on_a_small_stack(bytes), Ok(true));
}

#[test]
fn extreme_nesting_is_refused_or_round_trips_but_never_aborts() {
    let bytes = record(CLASS, &nested_tuples(200_000));
    match round_trip_on_a_small_stack(bytes) {
        Ok(same) => assert!(same, "decode then encode changed the bytes"),
        Err(message) => assert!(!message.is_empty()),
    }
}

#[test]
fn every_zodb_class_format_names_its_class() {
    let state = b"\x80\x03}q\x04.";
    let formats: [(&str, &[u8]); 4] = [
        ("1: the class", b"\x80\x03cm\nC\nq\x00."),
        ("2: (class, newargs)", b"\x80\x03cm\nC\nq\x00)\x86q\x01."),
        (
            "3: ((module, name), None)",
            b"\x80\x03X\x01\x00\x00\x00mq\x00X\x01\x00\x00\x00Cq\x01\x86q\x02N\x86q\x03.",
        ),
        (
            "7: ((module, name), newargs)",
            b"\x80\x03X\x01\x00\x00\x00mq\x00X\x01\x00\x00\x00Cq\x01\x86q\x02)\x86q\x03.",
        ),
    ];
    for (format, class) in formats {
        let bytes = record(class, state);
        let tree = decode_record(&bytes).unwrap_or_else(|e| panic!("format {format}: {e}"));
        assert_eq!(tree.class_name(), Some(("m", "C")), "format {format}");
        assert_eq!(encode_record(&tree).unwrap(), bytes, "format {format}");
    }
}

#[test]
fn python_2_opcodes_round_trip() {
    let class0: &[u8] = b"cpersistent.mapping\nPersistentMapping\n.";
    let class1: &[u8] = b"cpersistent.mapping\nPersistentMapping\nq\x01.";
    let records: [(&str, Vec<u8>); 4] = [
        (
            "protocol 0: INT, LONG, MARK, DICT, LIST, PUT",
            record(
                class0,
                b"(dp0\nVa\np1\nI42\nsVb\np2\nL1180591620717411303424L\nsVc\np3\nI01\nsVd\np4\n(lp5\nI1\naI-5\nas.",
            ),
        ),
        (
            "protocol 1: LONG",
            record(
                class1,
                b"}q\x00(X\x03\x00\x00\x00bigq\x01L-1208925819614629174706176L\nX\x05\x00\x00\x00smallq\x02K\x07u.",
            ),
        ),
        (
            "protocol 1: OBJ and BUILD",
            record(class1, b"}q\x02(U\x01xq\x03(cmod\nOld\nq\x04oq\x05}q\x06bu."),
        ),
        (
            "protocol 1: INST and BUILD",
            record(class1, b"}q\x02(U\x01yq\x03(imod\nOld\nq\x04}q\x05bu."),
        ),
    ];
    for (name, bytes) in records {
        let tree = decode_record(&bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(encode_record(&tree).unwrap(), bytes, "{name}");
        assert_eq!(
            tree.class_name(),
            Some(("persistent.mapping", "PersistentMapping")),
            "{name}"
        );
    }
}

#[test]
fn a_get_resolves_to_the_latest_put_before_it() {
    // MARK, 7, BINPUT 1, 9, BINPUT 1, BINGET 1, TUPLE: Python loads 9.
    let bytes = record(CLASS, b"\x80\x03(K\x07q\x01K\x09q\x01h\x01t.");
    let tree = decode_record(&bytes).unwrap();
    let get = tree
        .state()
        .walk()
        .find(|node| matches!(node, Node::Get(_)))
        .expect("a get node");
    assert_eq!(tree.resolve(get), Some(&Node::Int(Int::U8(9))));
    assert_eq!(encode_record(&tree).unwrap(), bytes);
}

#[test]
fn input_that_python_refuses_is_refused() {
    let refused: [(&str, Vec<u8>); 3] = [
        ("an unknown protocol", record(CLASS, b"\x80\xffN.")),
        (
            "a BINSTRING length of 2**31",
            record(CLASS, b"\x80\x03T\x00\x00\x00\x80abc."),
        ),
        (
            "a LONG4 length of 2**31",
            record(CLASS, b"\x80\x03\x8b\x00\x00\x00\x80\x01."),
        ),
    ];
    for (name, bytes) in refused {
        assert!(decode_record(&bytes).is_err(), "{name} was accepted");
    }
}
