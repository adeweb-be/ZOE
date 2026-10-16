//! Spike ZOE-188: probes on a clock that all the processes of a machine
//! share. `ZOE_SPIKE_TRACE` names the prefix of the dump file. Without it,
//! a probe does nothing.
#![allow(unsafe_code)]

use std::io::Write;
use std::sync::{Mutex, OnceLock};

static EVENTS: OnceLock<Option<Mutex<Vec<(u8, u64)>>>> = OnceLock::new();

fn events() -> Option<&'static Mutex<Vec<(u8, u64)>>> {
    EVENTS
        .get_or_init(|| {
            std::env::var_os("ZOE_SPIKE_TRACE").map(|_| Mutex::new(Vec::with_capacity(1 << 22)))
        })
        .as_ref()
}

#[cfg(target_os = "macos")]
const CLOCK: libc::clockid_t = libc::CLOCK_UPTIME_RAW;
#[cfg(not(target_os = "macos"))]
const CLOCK: libc::clockid_t = libc::CLOCK_MONOTONIC;

/// Return the time of the shared clock in nanoseconds.
pub fn now() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid timespec.
    unsafe { libc::clock_gettime(CLOCK, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

/// Record the probe `id` now.
#[inline]
pub fn mark(id: u8) {
    if let Some(events) = events() {
        let t = now();
        events.lock().unwrap().push((id, t));
    }
}

/// Write the probes to `<prefix>.<role>.<pid>` and forget them.
pub fn dump(role: &str) {
    let (Some(events), Some(prefix)) = (events(), std::env::var_os("ZOE_SPIKE_TRACE")) else {
        return;
    };
    let taken = std::mem::take(&mut *events.lock().unwrap());
    if taken.is_empty() {
        return;
    }
    let path = format!(
        "{}.{role}.{}",
        prefix.to_string_lossy(),
        std::process::id()
    );
    let Ok(file) = std::fs::File::create(path) else {
        return;
    };
    let mut out = std::io::BufWriter::new(file);
    for (id, t) in taken {
        let _ = writeln!(out, "{id} {t}");
    }
}
