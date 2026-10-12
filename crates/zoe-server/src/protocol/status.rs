//! The status of a named storage: the map of `server_status` and the
//! JSON reply of `ruok`, with the keys of ZEO 6's server.
//!
//! The store has no storage-wide commit lock. So `lock_time` is always
//! nil, `waiting` is always 0, and `timeout-thread-is-alive` is `"stub"`,
//! the value of a ZEO server without a transaction timeout.
//! `conflicts_resolved` is always 0, as in ZEO 6's server.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use rmpv::Value;
use zoe_core::Tid;

use super::clock;

/// The counters of a named storage, as ZEO's `StorageStats` keeps them.
/// All the connections of the storage share them.
#[derive(Debug)]
pub(crate) struct Stats {
    /// The time when the server started, as `time.ctime()` gives it, in
    /// UTC.
    start: String,
    /// The connections that registered and did not close.
    pub(crate) connections: AtomicU64,
    /// The calls of `loadBefore`.
    pub(crate) loads: AtomicU64,
    /// The stores that a transaction received.
    pub(crate) stores: AtomicU64,
    /// The calls of `tpc_finish` of a current transaction.
    pub(crate) commits: AtomicU64,
    /// The transactions that a client aborted or left open at its close.
    pub(crate) aborts: AtomicU64,
    /// The transactions that began and did not end.
    pub(crate) active_txns: AtomicU64,
    /// The votes that failed with a conflict.
    pub(crate) conflicts: AtomicU64,
}

impl Stats {
    /// Return counters at 0 for a server that started at `start`.
    pub(crate) fn new(start: SystemTime) -> Self {
        Self {
            start: clock::ctime(start),
            connections: AtomicU64::new(0),
            loads: AtomicU64::new(0),
            stores: AtomicU64::new(0),
            commits: AtomicU64::new(0),
            aborts: AtomicU64::new(0),
            active_txns: AtomicU64::new(0),
            conflicts: AtomicU64::new(0),
        }
    }

    /// Add 1 to `counter`.
    pub(crate) fn add(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// Subtract 1 from `counter`. A counter at 0 stays at 0.
    pub(crate) fn sub(counter: &AtomicU64) {
        let _ = counter.try_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
            Some(n.saturating_sub(1))
        });
    }

    /// Return the status now. `last` is the last tid of the store.
    pub(crate) fn status(&self, last: Tid) -> Status {
        let get = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        Status {
            connections: get(&self.connections),
            loads: get(&self.loads),
            stores: get(&self.stores),
            commits: get(&self.commits),
            aborts: get(&self.aborts),
            active_txns: get(&self.active_txns),
            conflicts: get(&self.conflicts),
            start: self.start.clone(),
            last_transaction: last,
        }
    }
}

/// The status of a named storage at one time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Status {
    connections: u64,
    loads: u64,
    stores: u64,
    commits: u64,
    aborts: u64,
    active_txns: u64,
    conflicts: u64,
    start: String,
    last_transaction: Tid,
}

/// One value of a status.
enum Item {
    Int(u64),
    Nil,
    Text(String),
}

impl Status {
    /// Return the items of the status, in the order of ZEO 6's server.
    fn items(&self) -> [(&'static str, Item); 13] {
        [
            ("connections", Item::Int(self.connections)),
            ("loads", Item::Int(self.loads)),
            ("stores", Item::Int(self.stores)),
            ("commits", Item::Int(self.commits)),
            ("aborts", Item::Int(self.aborts)),
            ("active_txns", Item::Int(self.active_txns)),
            ("lock_time", Item::Nil),
            ("conflicts", Item::Int(self.conflicts)),
            ("conflicts_resolved", Item::Int(0)),
            ("start", Item::Text(self.start.clone())),
            ("waiting", Item::Int(0)),
            ("timeout-thread-is-alive", Item::Text("stub".into())),
            (
                "last-transaction",
                Item::Text(format!("{:016x}", self.last_transaction.0)),
            ),
        ]
    }

    /// Return the status as the map of `server_status`.
    pub(crate) fn to_value(&self) -> Value {
        Value::Map(
            self.items()
                .into_iter()
                .map(|(key, item)| {
                    let value = match item {
                        Item::Int(n) => Value::from(n),
                        Item::Nil => Value::Nil,
                        Item::Text(text) => Value::from(text),
                    };
                    (Value::from(key), value)
                })
                .collect(),
        )
    }

    /// Write the status as a JSON object to `out`, as Python's
    /// `json.dumps` writes it.
    fn write_json(&self, out: &mut String) {
        out.push('{');
        for (n, (key, item)) in self.items().into_iter().enumerate() {
            if n > 0 {
                out.push_str(", ");
            }
            write_json_text(out, key);
            out.push_str(": ");
            match item {
                Item::Int(n) => {
                    let _ = write!(out, "{n}");
                }
                Item::Nil => out.push_str("null"),
                Item::Text(text) => write_json_text(out, &text),
            }
        }
        out.push('}');
    }
}

/// Return the reply to `ruok`: a JSON object that maps the name of each
/// storage to its status, as ZEO 6's server writes it.
pub(crate) fn ruok(statuses: &[(&str, Status)]) -> String {
    let mut out = String::from("{");
    for (n, (name, status)) in statuses.iter().enumerate() {
        if n > 0 {
            out.push_str(", ");
        }
        write_json_text(&mut out, name);
        out.push_str(": ");
        status.write_json(&mut out);
    }
    out.push('}');
    out
}

/// Write `text` as a JSON string in ASCII, as Python's `json.dumps` does.
fn write_json_text(out: &mut String, text: &str) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            ' '..='~' => out.push(c),
            _ => {
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    let _ = write!(out, "\\u{unit:04x}");
                }
            }
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The status of the storage that the traces recorded. The traces
    /// replace the start time with a placeholder of the same length.
    fn recorded(connections: u64) -> Status {
        Status {
            connections,
            loads: 0,
            stores: 0,
            commits: 0,
            aborts: 0,
            active_txns: 0,
            conflicts: 0,
            start: "<start time placeholder>".into(),
            last_transaction: Tid::ZERO,
        }
    }

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn server_status_has_the_bytes_of_zeo() {
        // The reply to `server_status` in docs/spikes/zeo-wire/plain.jsonl,
        // line 18.
        let raw = hex(concat!(
            "940400a62e7265706c798dab636f6e6e656374696f6e7302a56c6f61647300",
            "a673746f72657300a7636f6d6d69747300a661626f72747300ab6163746976",
            "655f74786e7300a96c6f636b5f74696d65c0a9636f6e666c6963747300b263",
            "6f6e666c696374735f7265736f6c76656400a57374617274b83c7374617274",
            "2074696d6520706c616365686f6c6465723ea777616974696e6700b774696d",
            "656f75742d7468726561642d69732d616c697665a473747562b06c6173742d",
            "7472616e73616374696f6eb030303030303030303030303030303030",
        ));
        let reply = super::super::message::reply(Value::from(4), recorded(2).to_value());
        assert_eq!(reply, raw);
    }

    #[test]
    fn ruok_has_the_bytes_of_zeo() {
        // The JSON reply to `ruok` in
        // docs/spikes/zeo-wire/unregistered.jsonl.
        let raw = concat!(
            r#"{"1": {"connections": 1, "loads": 0, "stores": 0, "commits": 0, "#,
            r#""aborts": 0, "active_txns": 0, "lock_time": null, "conflicts": 0, "#,
            r#""conflicts_resolved": 0, "start": "<start time placeholder>", "#,
            r#""waiting": 0, "timeout-thread-is-alive": "stub", "#,
            r#""last-transaction": "0000000000000000"}}"#,
        );
        assert_eq!(ruok(&[("1", recorded(1))]), raw);
    }

    #[test]
    fn a_json_text_is_ascii_as_python_writes_it() {
        let mut out = String::new();
        write_json_text(&mut out, "a\"b\\c\nd\u{1}é😀");
        assert_eq!(out, r#""a\"b\\c\nd\u0001\u00e9\ud83d\ude00""#);
    }

    #[test]
    fn counters_count_and_never_go_below_0() {
        let stats = Stats::new(SystemTime::UNIX_EPOCH);
        Stats::add(&stats.commits);
        Stats::add(&stats.connections);
        Stats::sub(&stats.connections);
        Stats::sub(&stats.connections);
        let status = stats.status(Tid(0x0123_4567_89ab_cdef));
        assert_eq!(status.commits, 1);
        assert_eq!(status.connections, 0);
        assert_eq!(status.start, "Thu Jan  1 00:00:00 1970");
        assert_eq!(status.last_transaction, Tid(0x0123_4567_89ab_cdef));
        let Value::Map(items) = status.to_value() else {
            panic!("a status is a map");
        };
        assert_eq!(
            items.last().unwrap(),
            &(
                Value::from("last-transaction"),
                Value::from("0123456789abcdef")
            )
        );
    }
}
