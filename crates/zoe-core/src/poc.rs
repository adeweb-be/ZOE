//! Spike instrumentation and toggles. Not for merge.
#![allow(missing_docs)]

use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Instant;

use crate::format::Compression;

pub struct Flags {
    pub profile: bool,
    pub tid_only: bool,
    pub zstd_tls: bool,
    pub precompress: bool,
    pub nosync: bool,
    pub store_compress: bool,
    pub par_compress: usize,
    pub compression: Option<Compression>,
}

fn on(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| v == "1")
}

pub static FLAGS: LazyLock<Flags> = LazyLock::new(|| Flags {
    profile: on("ZOE_POC_PROFILE"),
    tid_only: on("ZOE_POC_TIDONLY"),
    zstd_tls: on("ZOE_POC_ZSTD_TLS"),
    precompress: on("ZOE_POC_PRECOMPRESS"),
    nosync: on("ZOE_POC_NOSYNC"),
    store_compress: on("ZOE_POC_STORECOMPRESS"),
    par_compress: std::env::var("ZOE_POC_PARCOMPRESS")
        .ok()
        .map_or(0, |v| v.parse().unwrap()),
    compression: std::env::var("ZOE_POC_COMPRESSION").ok().map(|v| {
        if v == "none" {
            Compression::None
        } else {
            let level = v.strip_prefix("zstd:").unwrap_or("3").parse().unwrap();
            Compression::Zstd { level }
        }
    }),
});

#[derive(Clone, Copy)]
pub enum K {
    VoteCheck,
    Precompress,
    SeqHeld,
    LeaderWrite,
    Encode,
    Insert,
    Commit,
    Load,
    Batches,
    Members,
    Records,
    Loads,
    Votes,
}

const NAMES: [&str; 13] = [
    "vote_check_ns",
    "precompress_ns",
    "seq_held_ns",
    "leader_write_ns",
    "encode_ns",
    "insert_ns",
    "commit_ns",
    "load_ns",
    "batches",
    "members",
    "records",
    "loads",
    "votes",
];

static C: [AtomicU64; 13] = [const { AtomicU64::new(0) }; 13];

pub fn add(k: K, n: u64) {
    if FLAGS.profile {
        C[k as usize].fetch_add(n, Relaxed);
    }
}

pub fn start() -> Option<Instant> {
    FLAGS.profile.then(Instant::now)
}

pub fn stop(k: K, t: Option<Instant>) {
    if let Some(t) = t {
        C[k as usize].fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
    }
}

pub fn dump() -> String {
    let parts: Vec<String> = NAMES
        .iter()
        .zip(C.iter())
        .map(|(n, c)| format!("\"{n}\":{}", c.load(Relaxed)))
        .collect();
    format!("POC {{{}}}", parts.join(","))
}
