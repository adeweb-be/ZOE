//! Acceptance test of the ThreadSanitizer job.
//!
//! `just tsan` runs every `zoe-core` test under ThreadSanitizer and must be
//! green. Then it runs the ignored test below, which holds a deliberate
//! data race, and passes only when ThreadSanitizer reports that race. So a
//! green `just tsan` proves that the sanitizer is really on.
#![allow(unsafe_code)]

use std::thread;

static mut COUNTER: u64 = 0;

#[test]
#[ignore = "run by just tsan, which expects ThreadSanitizer to report this race"]
fn a_deliberate_data_race_is_reported() {
    let writers: Vec<_> = (0..4)
        .map(|_| {
            thread::spawn(|| {
                for _ in 0..10_000 {
                    // SAFETY: none. This is the race that ThreadSanitizer must find.
                    unsafe {
                        let value = std::ptr::addr_of_mut!(COUNTER).read_volatile();
                        std::ptr::addr_of_mut!(COUNTER).write_volatile(value + 1);
                    }
                }
            })
        })
        .collect();
    for writer in writers {
        writer.join().unwrap();
    }
}

#[test]
fn the_store_is_shared_between_threads() {
    // A guard that the core stays Send + Sync, which the job relies on.
    fn shared<T: Send + Sync>() {}
    shared::<zoe_core::Store>();
}
