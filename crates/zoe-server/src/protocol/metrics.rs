//! The metrics of `zoe serve` in the Prometheus text format, for
//! `GET /metrics` on the admin port.
//!
//! Each metric has the label `storage`, the storage name. A duration is a
//! histogram in seconds, and a counter ends in `_total`. The counters and
//! the histograms of a storage are atomics that all its connections
//! share. The core reports the size of each sync batch, each lock wait and
//! the lock hand-offs through its `Observer` trait. The gauges are read at
//! each scrape.
//!
//! - `zoe_loads_total`, `zoe_load_latency_seconds`: the `loadBefore` calls.
//! - `zoe_commits_total`: the commits that `tpc_finish` made.
//! - `zoe_vote_latency_seconds`, `zoe_finish_latency_seconds`: the `vote`
//!   calls, and the `tpc_finish` calls that ended a transaction.
//! - `zoe_fsync_batch_size`: the commits that one sync made durable.
//! - `zoe_lock_wait_seconds`: the time of each vote to take its locks.
//! - `zoe_lock_handoffs_total`: the locks that a finish handed to the vote
//!   at the head of their queues (ADR 0044).
//! - `zoe_intent_locks_total`, `zoe_intent_lock_wait_seconds`: the early
//!   write-intent locks that the sessions took, and the time of each
//!   `lock_intent` to take them (ADR 0045).
//! - `zoe_conflicts_resolved_total{path}`: the oids in conflict that tier 1
//!   or tier 2 merged in a vote that passed (`native`), and the oids that a
//!   second vote passed after the client resolved them (`python`).
//! - `zoe_structural_merges_total{tier, mode, outcome}`: the results of
//!   tier 1 and of tier 2 in the votes, merged, refused or undecided, in
//!   mode `on` or `shadow`.
//! - `zoe_shadow_verifications_total{tier, same}`: the merges of shadow
//!   mode that the second vote compared with the record of the client,
//!   by tier and by whether the two records are the same.
//! - `zoe_conflicts_raised_total`: the transactions that raised a
//!   conflict: a vote that failed with a conflict, or a transaction that
//!   ended between the two votes, so that the client raised
//!   `ConflictError`.
//! - `zoe_writes_on_get_total{class}`: the records that the transactions
//!   of GET and HEAD requests committed, by class (F32). `zoe.plone`
//!   marks such a transaction with the key `zoe.request_method` of its
//!   extension.
//! - `zoe_clients_connected`: the connections that registered.
//! - `zoe_invalidation_queue_depth`: the messages that wait in the deepest
//!   hub queue among the connections of the storage.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use super::invalidation::Invalidations;
use super::status::Stats;

/// The content type of the Prometheus text format.
pub(crate) const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// The upper bounds of the buckets of a duration, in seconds.
const SECONDS: &[f64] = &[
    0.0001, 0.00025, 0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5,
    5.0, 10.0,
];

/// The upper bounds of the buckets of a sync batch, in commits.
const COMMITS: &[f64] = &[1.0, 2.0, 4.0, 8.0, 16.0, 32.0, 64.0, 128.0];

/// A histogram with fixed buckets. Each observation adds 1 to one bucket
/// and its value to the sum.
pub(crate) struct Histogram {
    /// The upper bound of each bucket but the last, whose bound is
    /// infinite.
    bounds: &'static [f64],
    /// The observations of each bucket, not cumulative. One more than
    /// `bounds`.
    counts: Box<[AtomicU64]>,
    /// The bits of the `f64` sum of the observations.
    sum: AtomicU64,
}

impl Histogram {
    fn new(bounds: &'static [f64]) -> Self {
        Self {
            bounds,
            counts: (0..=bounds.len()).map(|_| AtomicU64::new(0)).collect(),
            sum: AtomicU64::new(0),
        }
    }

    /// Add one observation of `value`.
    pub(crate) fn observe(&self, value: f64) {
        let bucket = self
            .bounds
            .iter()
            .position(|&bound| value <= bound)
            .unwrap_or(self.bounds.len());
        self.counts[bucket].fetch_add(1, Ordering::Relaxed);
        let _ = self
            .sum
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |bits| {
                Some((f64::from_bits(bits) + value).to_bits())
            });
    }

    /// Return the number of observations and their sum.
    #[cfg(any(test, feature = "sim"))]
    pub(crate) fn totals(&self) -> (u64, f64) {
        let count = self
            .counts
            .iter()
            .map(|bucket| bucket.load(Ordering::Relaxed))
            .sum();
        (count, f64::from_bits(self.sum.load(Ordering::Relaxed)))
    }

    /// Add one observation of `duration`, in seconds.
    pub(crate) fn observe_duration(&self, duration: Duration) {
        self.observe(duration.as_secs_f64());
    }

    /// Write the samples of the histogram `name` for `labels`: one
    /// cumulative bucket for each bound and for `+Inf`, the sum and the
    /// count.
    fn write(&self, out: &mut String, name: &str, labels: &str) {
        let mut count = 0;
        for (n, bucket) in self.counts.iter().enumerate() {
            count += bucket.load(Ordering::Relaxed);
            let le = self
                .bounds
                .get(n)
                .map_or_else(|| "+Inf".to_owned(), f64::to_string);
            let _ = writeln!(out, "{name}_bucket{{{labels},le=\"{le}\"}} {count}");
        }
        let sum = f64::from_bits(self.sum.load(Ordering::Relaxed));
        let _ = writeln!(out, "{name}_sum{{{labels}}} {sum}");
        let _ = writeln!(out, "{name}_count{{{labels}}} {count}");
    }
}

/// The metrics of one storage that the status of ZEO does not keep. All
/// the connections of the storage share them, and its store reports to
/// them.
pub(crate) struct Metrics {
    pub(crate) load_latency: Histogram,
    pub(crate) vote_latency: Histogram,
    pub(crate) finish_latency: Histogram,
    pub(crate) fsync_batch_size: Histogram,
    pub(crate) lock_wait: Histogram,
    /// The commits that `tpc_finish` made.
    pub(crate) commits: AtomicU64,
    /// The locks that a finish handed off.
    pub(crate) lock_handoffs: AtomicU64,
    /// The early write-intent locks that the sessions took.
    pub(crate) intent_locks: AtomicU64,
    /// The time of each `lock_intent` to take its locks.
    pub(crate) intent_lock_wait: Histogram,
    /// The conflicts that reached the client as `ConflictError`.
    pub(crate) conflicts_raised: AtomicU64,
    /// The records that the transactions of GET and HEAD requests
    /// committed, by class.
    writes_on_get: Mutex<BTreeMap<String, u64>>,
    /// The oids in conflict that tier 1 or tier 2 merged in a vote that
    /// passed.
    pub(crate) conflicts_native: AtomicU64,
    /// The results of tiers 1 and 2, by tier, mode (`on`, `shadow`) and
    /// outcome (`merged`, `refused`, `undecided`).
    pub(crate) structural_merges: [[[AtomicU64; 3]; 2]; 2],
    /// The checks of the merges of shadow mode, by tier (1, 2) and by
    /// whether the records are the same (`true`, `false`).
    pub(crate) shadow_verifications: [[AtomicU64; 2]; 2],
}

impl Metrics {
    /// Add `classes`, the records of a commit of a GET or a HEAD request by
    /// class, to the counts of the report.
    pub(crate) fn add_writes_on_get(&self, classes: BTreeMap<String, u64>) {
        let mut counts = self
            .writes_on_get
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        for (class, n) in classes {
            *counts.entry(class).or_default() += n;
        }
    }

    /// Return the counts of the report.
    fn writes_on_get(&self) -> BTreeMap<String, u64> {
        self.writes_on_get
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Count the results of tiers 1 and 2 of one vote.
    pub(crate) fn count_resolutions(&self, resolutions: &[zoe_core::Resolution]) {
        use zoe_core::{Mode, Resolved};
        for resolution in resolutions {
            let tier = usize::from(resolution.tier == 2);
            let mode = match resolution.mode {
                Mode::On => 0,
                Mode::Shadow => 1,
                Mode::Off => continue,
            };
            let outcome = match resolution.outcome {
                Resolved::Merged => 0,
                Resolved::Refused(_) => 1,
                Resolved::Undecided(_) => 2,
            };
            self.structural_merges[tier][mode][outcome].fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Count the checks of the merges of shadow mode of one vote.
    pub(crate) fn count_verifications(&self, verifications: &[zoe_core::merge::Verification]) {
        for verification in verifications {
            let tier = usize::from(verification.tier == 2);
            let same = usize::from(!verification.same);
            self.shadow_verifications[tier][same].fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            load_latency: Histogram::new(SECONDS),
            vote_latency: Histogram::new(SECONDS),
            finish_latency: Histogram::new(SECONDS),
            fsync_batch_size: Histogram::new(COMMITS),
            lock_wait: Histogram::new(SECONDS),
            commits: AtomicU64::new(0),
            lock_handoffs: AtomicU64::new(0),
            intent_locks: AtomicU64::new(0),
            intent_lock_wait: Histogram::new(SECONDS),
            conflicts_raised: AtomicU64::new(0),
            writes_on_get: Mutex::new(BTreeMap::new()),
            conflicts_native: AtomicU64::new(0),
            structural_merges: Default::default(),
            shadow_verifications: Default::default(),
        }
    }
}

impl zoe_core::Observer for Metrics {
    fn batch_synced(&self, commits: usize) {
        self.fsync_batch_size.observe(commits as f64);
    }

    fn locks_acquired(&self, wait: Duration) {
        self.lock_wait.observe_duration(wait);
    }

    fn locks_handed_off(&self, locks: usize) {
        self.lock_handoffs
            .fetch_add(locks as u64, Ordering::Relaxed);
        #[cfg(feature = "sim")]
        crate::sim::handed_off(locks);
    }

    fn intent_locks_acquired(&self, locks: usize, wait: Duration) {
        self.intent_locks.fetch_add(locks as u64, Ordering::Relaxed);
        self.intent_lock_wait.observe_duration(wait);
    }
}

/// What the admin port reads for the metrics of one storage. It holds no
/// store, so it never keeps a store open.
#[derive(Clone)]
pub(crate) struct Exported {
    pub(crate) name: String,
    pub(crate) stats: Arc<Stats>,
    pub(crate) metrics: Arc<Metrics>,
    pub(crate) hub: Arc<Invalidations>,
}

/// Write the help line and the type line of the metric `name`.
fn head(out: &mut String, name: &str, kind: &str, help: &str) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} {kind}");
}

/// Return the value of `counter`.
fn get(counter: &AtomicU64) -> u64 {
    counter.load(Ordering::Relaxed)
}

/// Return the metrics of `storages` in the Prometheus text format.
pub(crate) fn render(storages: &[Exported]) -> String {
    let mut out = String::new();
    let storages: Vec<(&Exported, String)> = storages
        .iter()
        .map(|storage| (storage, format!("storage=\"{}\"", escape(&storage.name))))
        .collect();
    let scalar = |out: &mut String, name, kind, help, value: &dyn Fn(&Exported) -> u64| {
        head(out, name, kind, help);
        for (storage, labels) in &storages {
            let _ = writeln!(out, "{name}{{{labels}}} {}", value(storage));
        }
    };
    let histogram = |out: &mut String, name, help, of: &dyn Fn(&Metrics) -> &Histogram| {
        head(out, name, "histogram", help);
        for (storage, labels) in &storages {
            of(&storage.metrics).write(out, name, labels);
        }
    };
    scalar(
        &mut out,
        "zoe_loads_total",
        "counter",
        "The number of loadBefore calls.",
        &|s| get(&s.stats.loads),
    );
    histogram(
        &mut out,
        "zoe_load_latency_seconds",
        "The time of each loadBefore call, in seconds.",
        &|m| &m.load_latency,
    );
    scalar(
        &mut out,
        "zoe_commits_total",
        "counter",
        "The number of commits that tpc_finish made.",
        &|s| get(&s.metrics.commits),
    );
    histogram(
        &mut out,
        "zoe_vote_latency_seconds",
        "The time of each vote call, its lock wait included, in seconds.",
        &|m| &m.vote_latency,
    );
    histogram(
        &mut out,
        "zoe_finish_latency_seconds",
        "The time of each tpc_finish call that ended a transaction, until the commit is durable and published, in seconds.",
        &|m| &m.finish_latency,
    );
    histogram(
        &mut out,
        "zoe_fsync_batch_size",
        "The number of commits that one sync made durable, in commits.",
        &|m| &m.fsync_batch_size,
    );
    histogram(
        &mut out,
        "zoe_lock_wait_seconds",
        "The time of each vote to take its locks, a wait past the lock timeout included, in seconds.",
        &|m| &m.lock_wait,
    );
    scalar(
        &mut out,
        "zoe_lock_handoffs_total",
        "counter",
        "The number of locks that a finish handed to the vote at the head of their queues.",
        &|s| get(&s.metrics.lock_handoffs),
    );
    scalar(
        &mut out,
        "zoe_intent_locks_total",
        "counter",
        "The number of early write-intent locks that the sessions took.",
        &|s| get(&s.metrics.intent_locks),
    );
    histogram(
        &mut out,
        "zoe_intent_lock_wait_seconds",
        "The time of each lock_intent call to take its locks, a wait past the lock timeout included, in seconds.",
        &|m| &m.intent_lock_wait,
    );
    let name = "zoe_conflicts_resolved_total";
    head(
        &mut out,
        name,
        "counter",
        "The number of conflicting oids that a second vote passed after their resolution: native on the server, python in the client.",
    );
    for (storage, labels) in &storages {
        let python = get(&storage.stats.conflicts_resolved);
        let native = get(&storage.metrics.conflicts_native);
        let _ = writeln!(out, "{name}{{{labels},path=\"native\"}} {native}");
        let _ = writeln!(out, "{name}{{{labels},path=\"python\"}} {python}");
    }
    let name = "zoe_structural_merges_total";
    head(
        &mut out,
        name,
        "counter",
        "The number of results of the merges of tier 1 and tier 2 in the votes, by tier, mode and outcome.",
    );
    for (storage, labels) in &storages {
        for (t, tier) in ["1", "2"].into_iter().enumerate() {
            for (m, mode) in ["on", "shadow"].into_iter().enumerate() {
                for (o, outcome) in ["merged", "refused", "undecided"].into_iter().enumerate() {
                    let count = get(&storage.metrics.structural_merges[t][m][o]);
                    let _ = writeln!(
                        out,
                        "{name}{{{labels},tier=\"{tier}\",mode=\"{mode}\",outcome=\"{outcome}\"}} {count}"
                    );
                }
            }
        }
    }
    let name = "zoe_shadow_verifications_total";
    head(
        &mut out,
        name,
        "counter",
        "The number of merges of shadow mode that a second vote compared with the record of the client, by tier and by whether the records are the same.",
    );
    for (storage, labels) in &storages {
        for (t, tier) in ["1", "2"].into_iter().enumerate() {
            for (s, same) in ["true", "false"].into_iter().enumerate() {
                let count = get(&storage.metrics.shadow_verifications[t][s]);
                let _ = writeln!(
                    out,
                    "{name}{{{labels},tier=\"{tier}\",same=\"{same}\"}} {count}"
                );
            }
        }
    }
    scalar(
        &mut out,
        "zoe_conflicts_raised_total",
        "counter",
        "The number of transactions that raised a conflict: a vote that failed with a conflict, or an end between the two votes.",
        &|s| get(&s.metrics.conflicts_raised),
    );
    let name = "zoe_writes_on_get_total";
    head(
        &mut out,
        name,
        "counter",
        "The number of records that the transactions of GET and HEAD requests committed, by class.",
    );
    for (storage, labels) in &storages {
        for (class, n) in storage.metrics.writes_on_get() {
            let class = escape(&class);
            let _ = writeln!(out, "{name}{{{labels},class=\"{class}\"}} {n}");
        }
    }
    scalar(
        &mut out,
        "zoe_clients_connected",
        "gauge",
        "The number of connections that registered.",
        &|s| get(&s.stats.connections),
    );
    scalar(
        &mut out,
        "zoe_invalidation_queue_depth",
        "gauge",
        "The messages that wait in the deepest hub queue among the connections of the storage.",
        &|s| s.hub.deepest_queue() as u64,
    );
    out
}

/// Escape `value` for a label value: a backslash, a double quote and a
/// line feed.
fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use zoe_core::{Observer, Tid};

    use super::*;

    fn exported(name: &str) -> Exported {
        Exported {
            name: name.to_owned(),
            stats: Arc::new(Stats::new(SystemTime::UNIX_EPOCH)),
            metrics: Arc::new(Metrics::default()),
            hub: Arc::new(Invalidations::new(Tid::ZERO)),
        }
    }

    #[test]
    fn a_histogram_gives_cumulative_buckets_a_sum_and_a_count() {
        let histogram = Histogram::new(&[1.0, 2.0]);
        for value in [0.5, 1.0, 1.5, 3.0] {
            histogram.observe(value);
        }
        let mut out = String::new();
        histogram.write(&mut out, "h", "storage=\"1\"");
        assert_eq!(
            out,
            "h_bucket{storage=\"1\",le=\"1\"} 2\n\
             h_bucket{storage=\"1\",le=\"2\"} 3\n\
             h_bucket{storage=\"1\",le=\"+Inf\"} 4\n\
             h_sum{storage=\"1\"} 6\n\
             h_count{storage=\"1\"} 4\n"
        );
    }

    #[test]
    fn each_family_has_one_help_one_type_and_a_sample_for_each_storage() {
        let one = exported("1");
        Stats::add(&one.stats.loads);
        one.metrics.batch_synced(3);
        one.metrics.locks_acquired(Duration::from_millis(2));
        let classes = |items: &[(&str, u64)]| {
            items
                .iter()
                .map(|&(class, n)| (class.to_owned(), n))
                .collect::<BTreeMap<_, _>>()
        };
        one.metrics
            .add_writes_on_get(classes(&[("persistent.mapping.PersistentMapping", 2)]));
        one.metrics.add_writes_on_get(classes(&[
            ("persistent.mapping.PersistentMapping", 1),
            ("BTrees.Length.Length", 1),
        ]));
        one.metrics.locks_handed_off(2);
        let text = render(&[one, exported("a\"b")]);
        assert_eq!(text.matches("# TYPE ").count(), 17);
        assert!(
            text.contains(
                "zoe_writes_on_get_total{storage=\"1\",class=\"persistent.mapping.PersistentMapping\"} 3\n"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "zoe_writes_on_get_total{storage=\"1\",class=\"BTrees.Length.Length\"} 1\n"
            ),
            "{text}"
        );
        assert!(
            text.contains("zoe_lock_handoffs_total{storage=\"1\"} 2\n"),
            "{text}"
        );
        assert!(
            text.contains("zoe_loads_total{storage=\"1\"} 1\n"),
            "{text}"
        );
        assert!(
            text.contains("zoe_loads_total{storage=\"a\\\"b\"} 0\n"),
            "{text}"
        );
        assert!(
            text.contains("zoe_fsync_batch_size_bucket{storage=\"1\",le=\"4\"} 1\n"),
            "{text}"
        );
        assert!(
            text.contains("zoe_lock_wait_seconds_count{storage=\"1\"} 1\n"),
            "{text}"
        );
        assert!(
            text.contains("zoe_conflicts_resolved_total{storage=\"1\",path=\"native\"} 0\n"),
            "{text}"
        );
        for tier in ["1", "2"] {
            for mode in ["on", "shadow"] {
                for outcome in ["merged", "refused", "undecided"] {
                    let line = format!(
                        "zoe_structural_merges_total{{storage=\"1\",tier=\"{tier}\",mode=\"{mode}\",outcome=\"{outcome}\"}} 0\n"
                    );
                    assert!(text.contains(&line), "{text}");
                }
            }
        }
        for tier in ["1", "2"] {
            for same in ["true", "false"] {
                let line = format!(
                    "zoe_shadow_verifications_total{{storage=\"1\",tier=\"{tier}\",same=\"{same}\"}} 0\n"
                );
                assert!(text.contains(&line), "{text}");
            }
        }
        assert!(
            text.contains("zoe_invalidation_queue_depth{storage=\"1\"} 0\n"),
            "{text}"
        );
        // Each help text gives the unit of its metric.
        let help = |name: &str| {
            let head = format!("# HELP {name} ");
            text.lines()
                .find_map(|line| line.strip_prefix(head.as_str()))
                .unwrap_or_else(|| panic!("no help for {name}"))
                .to_owned()
        };
        for name in [
            "zoe_load_latency_seconds",
            "zoe_vote_latency_seconds",
            "zoe_finish_latency_seconds",
            "zoe_lock_wait_seconds",
        ] {
            assert!(help(name).ends_with("in seconds."), "{name}");
        }
        assert!(help("zoe_fsync_batch_size").contains("in commits"));
        assert!(help("zoe_conflicts_resolved_total").contains("oids"));
        assert!(help("zoe_conflicts_raised_total").contains("transactions"));
        assert!(help("zoe_writes_on_get_total").contains("by class"));
        assert!(help("zoe_lock_handoffs_total").contains("locks"));
        assert!(help("zoe_intent_locks_total").contains("intent locks"));
        assert!(help("zoe_intent_lock_wait_seconds").ends_with("in seconds."));
        assert!(help("zoe_structural_merges_total").contains("tier, mode and outcome"));
        assert!(
            help("zoe_invalidation_queue_depth")
                .contains("the deepest hub queue among the connections of the storage")
        );
        // Each sample line is a name, its labels and a number.
        for line in text.lines().filter(|line| !line.starts_with('#')) {
            let (_, value) = line.rsplit_once(' ').unwrap();
            value.parse::<f64>().unwrap();
        }
    }
}
