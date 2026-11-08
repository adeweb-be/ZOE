//! Fuzz the decoders of the plan and of the overlay of a catalog query of
//! `zoe serve` (ADR 0059, section 8).
//!
//! An input is the argument `plan` of a `catalog_query` call, one msgpack
//! value, and then, when bytes remain, the argument `overlay`, a second
//! msgpack value. The decoder reads each within the bounds of a frame, then
//! reads the plan and the kind of each clause, and the records of the
//! overlay as the mirror reads them. A panic fails the run.

#![no_main]

use libfuzzer_sys::fuzz_target;
use zoe_server::fuzz::read_plan;

fuzz_target!(|data: &[u8]| {
    read_plan(data);
});
