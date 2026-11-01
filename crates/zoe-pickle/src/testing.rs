//! Builders of records for the tests of the crates that read records.
//!
//! The feature `testing` builds this module. Only dev-dependencies turn
//! the feature on, so only test builds have it.

/// Returns a record of the class `module.name` with the state pickle
/// `state`, which has no STOP: PROTO 3, GLOBAL and STOP, then PROTO 3,
/// `state` and STOP.
pub fn record(module: &str, name: &str, state: &[u8]) -> Vec<u8> {
    let class = format!("c{module}\n{name}\n.");
    [b"\x80\x03", class.as_bytes(), b"\x80\x03", state, b"."].concat()
}

/// Returns a record as [`record`] does, with BINPUT 0 after the GLOBAL.
/// So a BINGET 0 in `state` gives the class, as in [`reference_to_put`].
pub fn record_with_put(module: &str, name: &str, state: &[u8]) -> Vec<u8> {
    let class = format!("c{module}\n{name}\nq\x00.");
    [b"\x80\x03", class.as_bytes(), b"\x80\x03", state, b"."].concat()
}

/// Returns the pickle of the str `value`: BINUNICODE.
pub fn text(value: &str) -> Vec<u8> {
    let length = u32::try_from(value.len()).unwrap().to_le_bytes();
    [&b"X"[..], &length, value.as_bytes()].concat()
}

/// Returns the pickle of a persistent reference to `oid` with the class
/// `module.name`: the 8 bytes of the oid, the GLOBAL, TUPLE2, BINPERSID.
pub fn reference(oid: u64, module: &str, name: &str) -> Vec<u8> {
    let class = format!("c{module}\n{name}\n");
    [
        &b"C\x08"[..],
        &oid.to_be_bytes(),
        class.as_bytes(),
        b"\x86Q",
    ]
    .concat()
}

/// Returns the pickle of a persistent reference to `oid` whose class is
/// BINGET 0: the class of a record of [`record_with_put`].
pub fn reference_to_put(oid: u64) -> Vec<u8> {
    [&b"C\x08"[..], &oid.to_be_bytes(), b"h\x00\x86Q"].concat()
}
