//! Fuzz the decoder of the plan of a catalog query of `zoe serve` (ADR
//! 0059, section 8).
//!
//! An input is the argument `plan` of a `catalog_query` call: one msgpack
//! value. The decoder reads it within the bounds of a frame, then reads
//! the plan and the kind of each clause. A panic fails the run.

#![no_main]

use libfuzzer_sys::fuzz_target;
use zoe_server::fuzz::read_plan;

fuzz_target!(|data: &[u8]| {
    read_plan(data);
});
