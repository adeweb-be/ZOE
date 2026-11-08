//! The catalog mirrors of a storage (ADR 0059): the mirrors of
//! `zoe-catalog`, hosted by the server for every client of the store, the
//! call `catalog_query` of the extension `catalog_query`, and the metrics
//! of the catalog.
//!
//! The mirrors of a store start at the open of a store whose durable
//! namespace `zoe.catalog` holds a registration key, or at the first
//! `kv_set` of that namespace, never before. Their thread follows the
//! invalidation hub of the store: the hub hands it each tid after the
//! publication, and never waits for it (`Invalidations::follow`).
//!
//! `catalog_query(catalog, tid, plan, overlay)` runs on the read pool. A
//! query whose snapshot holds a commit that the mirror did not apply waits
//! for it, at most `catalog-wait`, on its thread of the read pool. The
//! server reads no transaction overlay: an overlay that is not nil gives
//! the reason `plan`.
//!
//! The mirrors also host the text index of each registered catalog with
//! settings in `zoe.text` (ADR 0060), in `text/<oid>/` of the store
//! directory. A plan can hold a text clause; `text_query(catalog, tid,
//! clause, filter)` and `text_info(catalog, index, tid, rid)` of the
//! extension `text` read the text index without the mirror. A store whose
//! mirrors did not start has no text index: its text queries answer from
//! the staged texts of an index in `cl`, or give `unavailable`.

use std::fmt::{self, Write as _};
use std::sync::{Arc, Mutex, PoisonError, Weak};

use rmpv::Value;
use zoe_catalog::counts::LATENCY_BUCKETS;
use zoe_catalog::text::{CallError, clause, engine};
use zoe_catalog::{
    Config, Fallback, Mirrors, NAMESPACE, Opener, PlanError, Reads, Reason, Wire, decode_typed,
    encode_reply, split_text,
};
use zoe_core::{Flavour, Store};

use super::invalidation::Invalidations;
use super::metrics::Exported;

/// The settings of the mirrors of each store: the five keys of `zoe serve`
/// and the hooks of the tests.
#[derive(Clone, Default)]
pub(crate) struct CatalogConfig(pub(crate) Config);

impl fmt::Debug for CatalogConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let config = &self.0;
        f.debug_struct("CatalogConfig")
            .field("memory", &config.memory)
            .field("window_tids", &config.window_tids)
            .field("window", &config.window)
            .field("rebase_rows", &config.rebase_rows)
            .field("wait", &config.wait)
            .finish_non_exhaustive()
    }
}

/// The host of the mirrors of one store. It holds no share of the store.
pub(crate) struct Catalog {
    config: Config,
    store: Weak<Store>,
    /// Held while the mirrors start, so they start once.
    starting: Mutex<()>,
}

impl Catalog {
    /// The host of the mirrors of `store`, with `config`. It starts no
    /// mirror.
    pub(crate) fn new(store: &Arc<Store>, config: Config) -> Self {
        Self {
            config,
            store: Arc::downgrade(store),
            starting: Mutex::new(()),
        }
    }

    /// Start the mirrors of the store when its namespace `zoe.catalog`
    /// holds a registration key.
    pub(crate) fn start_if_registered(&self, hub: &Invalidations) {
        let Some(store) = self.store.upgrade() else {
            return;
        };
        let registered = store
            .kv_scan(Flavour::Durable, NAMESPACE, None, None, 1)
            .is_ok_and(|keys| !keys.is_empty());
        drop(store);
        if registered {
            self.start(hub);
        }
    }

    /// Start the mirrors of the store, once, and make their thread follow
    /// `hub`.
    pub(crate) fn start(&self, hub: &Invalidations) {
        let _starting = self.starting.lock().unwrap_or_else(PoisonError::into_inner);
        if hub.follower().is_some() {
            return;
        }
        let weak = self.store.clone();
        let open: Opener = Arc::new(move || weak.upgrade().map(|s| s as Arc<dyn Reads>));
        let mirrors = Arc::new(Mirrors::start(open, hub.published().0, self.config.clone()));
        hub.follow(Arc::clone(&mirrors));
        // A commit that the hub published between the start and the follow
        // reaches the mirrors here.
        mirrors.committed(hub.published().0);
    }

    /// Tell the thread of the mirrors and their builds to stop. Do not
    /// wait for them.
    pub(crate) fn signal_stop(hub: &Invalidations) {
        if let Some(mirrors) = hub.follower() {
            mirrors.signal_stop();
        }
    }

    /// Stop the thread of the mirrors and their builds, and wait for them.
    pub(crate) fn stop(hub: &Invalidations) {
        if let Some(mirrors) = hub.follower() {
            mirrors.stop();
        }
    }
}

/// Tell if a `kv_set` or a `kv_delete` in the namespace `name` of
/// `flavour` writes the registration keys.
pub(crate) fn registers(flavour: Flavour, name: &str) -> bool {
    flavour == Flavour::Durable && name == NAMESPACE
}

/// Answer `catalog_query` on the catalog `catalog`, at the exclusive tid
/// `tid`, with the plan `plan` and the overlay `overlay`. A plan that does
/// not have the form of section 8 is an error: its text.
pub(crate) fn query(
    hub: &Invalidations,
    catalog: u64,
    tid: u64,
    plan: &Value,
    overlay: &Value,
) -> Result<Value, String> {
    let wire = wire_of(plan)?;
    let mirrors = hub.follower();
    // The text clause comes apart from the plan; the mirrors take the
    // whole plan.
    let (rest, _) = split_text(&wire).map_err(|e| match e {
        PlanError::Malformed(why) | PlanError::Unknown(why) => why,
    })?;
    let answer = match decode_typed(&rest) {
        Err(PlanError::Malformed(why)) => return Err(why),
        Err(PlanError::Unknown(why)) => Err(Fallback::new(Reason::Plan, why)),
        Ok(_) if !overlay.is_nil() => Err(Fallback::new(
            Reason::Plan,
            "the server reads no transaction overlay",
        )),
        Ok(_) => match mirrors {
            None => Err(Fallback::new(
                Reason::NotRegistered,
                "the store has no registration key",
            )),
            Some(mirrors) => {
                let answer = mirrors.query_wire(catalog, tid, &wire)?;
                log(mirrors);
                return Ok(value_of(encode_reply(tid, &answer)));
            }
        },
    };
    if let (Some(mirrors), Err(fallback)) = (mirrors, &answer) {
        mirrors.count_fallback(fallback.reason);
    }
    Ok(value_of(encode_reply(tid, &answer)))
}

/// Answer `text_query(catalog, tid, clause, filter)` (ADR 0060, section
/// 12) with the text index of the store.
pub(crate) fn text_query(
    hub: &Invalidations,
    store: &Store,
    catalog: u64,
    tid: u64,
    clause: &Value,
    filter: &Value,
) -> Result<Value, CallError> {
    let clause = wire_of(clause).map_err(CallError::Malformed)?;
    let filter = wire_of(filter).map_err(CallError::Malformed)?;
    let texts = hub.follower().and_then(|m| m.texts());
    let answer = engine::text_query(texts, store, catalog, tid, &clause, &filter)?;
    Ok(value_of(clause::encode_hits(tid, &answer)))
}

/// Answer `text_info(catalog, index, tid, rid)` with the text index of the
/// store.
pub(crate) fn text_info(
    hub: &Invalidations,
    store: &Store,
    catalog: u64,
    index: &str,
    tid: u64,
    rid: Option<i64>,
) -> Result<Value, CallError> {
    let texts = hub.follower().and_then(|m| m.texts());
    let answer = engine::text_info(texts, store, catalog, index, tid, rid)?;
    Ok(value_of(clause::encode_info(tid, &answer)))
}

/// Log the events of `mirrors`: a refused catalog, a drop, a stale mirror.
pub(crate) fn log(mirrors: &Mirrors) {
    for event in mirrors.take_events() {
        tracing::warn!("{event}");
    }
}

/// The plan `value` as a tree of msgpack values. The frame decoder bounds
/// its depth and its number of values. An int past 64 signed bits, and a
/// str that is not UTF-8, are errors.
pub(crate) fn wire_of(value: &Value) -> Result<Wire, String> {
    Ok(match value {
        Value::Nil => Wire::Nil,
        Value::Boolean(b) => Wire::Bool(*b),
        Value::Integer(n) => Wire::Int(
            n.as_i64()
                .ok_or_else(|| "an int of the plan beyond 64 bits".to_owned())?,
        ),
        Value::F32(f) => Wire::Float(f64::from(*f)),
        Value::F64(f) => Wire::Float(*f),
        Value::String(s) => Wire::Str(
            s.as_str()
                .ok_or_else(|| "a str of the plan that is not UTF-8".to_owned())?
                .to_owned(),
        ),
        Value::Binary(b) => Wire::Bin(b.clone()),
        Value::Array(items) => Wire::Array(items.iter().map(wire_of).collect::<Result<_, _>>()?),
        Value::Map(items) => Wire::Map(
            items
                .iter()
                .map(|(k, v)| Ok((wire_of(k)?, wire_of(v)?)))
                .collect::<Result<_, String>>()?,
        ),
        Value::Ext(kind, data) => Wire::Ext(*kind, data.clone()),
    })
}

/// The msgpack value of `wire`.
pub(crate) fn value_of(wire: Wire) -> Value {
    match wire {
        Wire::Nil => Value::Nil,
        Wire::Bool(b) => Value::Boolean(b),
        Wire::Int(i) => Value::from(i),
        Wire::Float(f) => Value::F64(f),
        Wire::Str(s) => Value::from(s),
        Wire::Bin(b) => Value::Binary(b),
        Wire::Array(items) => Value::Array(items.into_iter().map(value_of).collect()),
        Wire::Map(items) => Value::Map(
            items
                .into_iter()
                .map(|(k, v)| (value_of(k), value_of(v)))
                .collect(),
        ),
        Wire::Ext(kind, data) => Value::Ext(kind, data),
    }
}

/// Write the metrics of the catalog of `storages` in the Prometheus text
/// format (ADR 0059, section 11). Each sample has the label `storage`; the
/// gauges of a mirror also have the label `catalog`, the oid of its
/// `Catalog` object in 16 hex digits. A storage whose mirrors did not
/// start gives 0 for each counter, and no gauge of a mirror.
pub(crate) fn render(storages: &[Exported]) -> String {
    let mut out = String::new();
    let counts: Vec<(String, zoe_catalog::counts::Counts)> = storages
        .iter()
        .map(|s| {
            let labels = format!("storage=\"{}\"", super::metrics::escape(&s.name));
            let counts = s.hub.follower().map(|m| m.counts()).unwrap_or_default();
            (labels, counts)
        })
        .collect();
    let head = |out: &mut String, name: &str, kind: &str, help: &str| {
        let _ = writeln!(out, "# HELP {name} {help}");
        let _ = writeln!(out, "# TYPE {name} {kind}");
    };
    let name = "zoe_catalog_queries_total";
    head(
        &mut out,
        name,
        "counter",
        "The number of catalog queries that the mirrors answered (native) or refused (python).",
    );
    for (labels, c) in &counts {
        let _ = writeln!(out, "{name}{{{labels},path=\"native\"}} {}", c.native);
        let _ = writeln!(out, "{name}{{{labels},path=\"python\"}} {}", c.python);
    }
    let name = "zoe_catalog_query_latency_seconds";
    head(
        &mut out,
        name,
        "histogram",
        "The time of each catalog query in the server, its wait for the mirror included, in seconds.",
    );
    for (labels, c) in &counts {
        let mut cumulative = 0;
        for (n, bound) in LATENCY_BUCKETS.iter().enumerate() {
            cumulative += c.latency.get(n).copied().unwrap_or(0);
            let _ = writeln!(out, "{name}_bucket{{{labels},le=\"{bound}\"}} {cumulative}");
        }
        let count: u64 = c.latency.iter().sum();
        let _ = writeln!(out, "{name}_bucket{{{labels},le=\"+Inf\"}} {count}");
        let _ = writeln!(out, "{name}_sum{{{labels}}} {}", c.latency_seconds);
        let _ = writeln!(out, "{name}_count{{{labels}}} {count}");
    }
    let name = "zoe_catalog_fallbacks_total";
    head(
        &mut out,
        name,
        "counter",
        "The number of catalog queries that took the Python path, by the reason that the server decided.",
    );
    for (labels, c) in &counts {
        for (reason, n) in &c.fallbacks {
            let reason = reason.name();
            let _ = writeln!(
                out,
                "{name}{{{labels},reason=\"{reason}\",side=\"server\"}} {n}"
            );
        }
    }
    let name = "zoe_catalog_mirror_drops_total";
    head(
        &mut out,
        name,
        "counter",
        "The number of catalog mirrors that the memory bound catalog-memory dropped.",
    );
    for (labels, c) in &counts {
        let _ = writeln!(out, "{name}{{{labels}}} {}", c.drops);
    }
    type Gauge = fn(&zoe_catalog::counts::MirrorCounts) -> String;
    let gauges: [(&str, &str, Gauge); 4] = [
        (
            "zoe_catalog_mirror_ready",
            "1 when the catalog mirror is ready, else 0.",
            |m| u8::from(m.ready).to_string(),
        ),
        (
            "zoe_catalog_mirror_build_seconds",
            "The time of the last build of the catalog mirror, in seconds.",
            |m| m.build_seconds.to_string(),
        ),
        (
            "zoe_catalog_mirror_lag_tids",
            "The published commits that the catalog mirror did not apply yet.",
            |m| m.lag_tids.to_string(),
        ),
        (
            "zoe_catalog_mirror_bytes",
            "The memory of the catalog mirror, in bytes.",
            |m| m.bytes.to_string(),
        ),
    ];
    for (name, help, value) in gauges {
        head(&mut out, name, "gauge", help);
        for (labels, c) in &counts {
            for m in &c.mirrors {
                let _ = writeln!(
                    out,
                    "{name}{{{labels},catalog=\"{:016x}\"}} {}",
                    m.catalog,
                    value(m)
                );
            }
        }
    }
    let name = "zoe_text_commit_lag_seconds";
    head(
        &mut out,
        name,
        "gauge",
        "The age of the oldest committed text delta that the text index of the catalog does not hold yet, in seconds; 0 when it holds all.",
    );
    for (labels, c) in &counts {
        for t in &c.texts {
            let _ = writeln!(
                out,
                "{name}{{{labels},catalog=\"{:016x}\"}} {}",
                t.catalog, t.lag_seconds
            );
        }
    }
    out
}
