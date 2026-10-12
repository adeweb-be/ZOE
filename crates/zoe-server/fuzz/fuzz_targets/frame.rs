//! Fuzz the frame decoder and the message decoder of `zoe serve`.
//!
//! An input is the bytes that a client sends: its protocol, then its
//! frames. The decoders read it with the default limit of a frame. A panic
//! fails the run.

#![no_main]

use libfuzzer_sys::fuzz_target;
use zoe_server::fuzz::read_messages;
use zoe_server::serve::DEFAULT_MAX_FRAME_SIZE;

fuzz_target!(|data: &[u8]| {
    read_messages(data, DEFAULT_MAX_FRAME_SIZE);
});
