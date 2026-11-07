//! The counts of the mirrors of a store (ADR 0059, section 11), as the
//! store gives them: the queries by path, their latency, the fallbacks by
//! reason, the drops, and the state of each mirror.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::reason::Reason;

/// The upper bounds of the buckets of `zoe_catalog_query_latency_seconds`,
/// in seconds.
pub const LATENCY_BUCKETS: [f64; 12] = [
    0.0001, 0.00025, 0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 1.0,
];

/// The counters of a store. They only grow.
#[derive(Debug, Default)]
pub struct Counters {
    native: AtomicU64,
    python: AtomicU64,
    fallbacks: [AtomicU64; Reason::ALL.len()],
    latency: [AtomicU64; LATENCY_BUCKETS.len() + 1],
    latency_micros: AtomicU64,
    drops: AtomicU64,
}

impl Counters {
    /// Counts a query that the mirror answered, and its latency.
    pub fn native(&self, took: Duration) {
        self.native.fetch_add(1, Ordering::Relaxed);
        self.latency(took);
    }

    /// Counts a query that the mirror refused, with its reason.
    pub fn python(&self, reason: Reason, took: Duration) {
        self.python.fetch_add(1, Ordering::Relaxed);
        if let Some(i) = Reason::ALL.iter().position(|r| *r == reason) {
            self.fallbacks[i].fetch_add(1, Ordering::Relaxed);
        }
        self.latency(took);
    }

    fn latency(&self, took: Duration) {
        let seconds = took.as_secs_f64();
        let bucket = LATENCY_BUCKETS
            .iter()
            .position(|b| seconds <= *b)
            .unwrap_or(LATENCY_BUCKETS.len());
        self.latency[bucket].fetch_add(1, Ordering::Relaxed);
        self.latency_micros
            .fetch_add(took.as_micros() as u64, Ordering::Relaxed);
    }

    /// Counts the drop of a mirror by the memory bound.
    pub fn drop_mirror(&self) {
        self.drops.fetch_add(1, Ordering::Relaxed);
    }

    /// The counts now.
    pub fn snapshot(&self) -> Counts {
        let load = |a: &AtomicU64| a.load(Ordering::Relaxed);
        Counts {
            native: load(&self.native),
            python: load(&self.python),
            fallbacks: Reason::ALL
                .iter()
                .zip(&self.fallbacks)
                .map(|(r, n)| (*r, load(n)))
                .filter(|(_, n)| *n > 0)
                .collect(),
            latency: self.latency.iter().map(load).collect(),
            latency_seconds: load(&self.latency_micros) as f64 / 1e6,
            drops: load(&self.drops),
            mirrors: Vec::new(),
        }
    }
}

/// The counts of a store at one time.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Counts {
    /// `zoe_catalog_queries_total{path="native"}`.
    pub native: u64,
    /// `zoe_catalog_queries_total{path="python"}`, the queries that the
    /// store refused.
    pub python: u64,
    /// `zoe_catalog_fallbacks_total{side="server"}`, by reason. A reason
    /// that never came is left out.
    pub fallbacks: Vec<(Reason, u64)>,
    /// The queries of each bucket of `LATENCY_BUCKETS`, and of no bucket.
    pub latency: Vec<u64>,
    /// The sum of the latencies, in seconds.
    pub latency_seconds: f64,
    /// `zoe_catalog_mirror_drops_total`.
    pub drops: u64,
    /// The state of each mirror.
    pub mirrors: Vec<MirrorCounts>,
}

/// The counts of one mirror.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MirrorCounts {
    /// The oid of the catalog.
    pub catalog: u64,
    /// `zoe_catalog_mirror_ready`.
    pub ready: bool,
    /// `zoe_catalog_mirror_build_seconds`: the last build.
    pub build_seconds: f64,
    /// `zoe_catalog_mirror_lag_tids`: the commits that the mirror did not
    /// apply yet.
    pub lag_tids: u64,
    /// `zoe_catalog_mirror_bytes`.
    pub bytes: u64,
}
