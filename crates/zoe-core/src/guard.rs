//! The guardrails of a store: the size limits of a store call, and the
//! log lines of slow locks and slow transactions.
//!
//! A size limit refuses a `store` or a `store_blob` before the transaction
//! stages it, so a refused call writes nothing. A lock held longer than
//! `lock-warn` and a transaction open longer than `transaction-warn` give a
//! `tracing` event at WARN level. Each duration comes from the store clock.
//! An event holds only oids, tids, counts and durations, never pickle
//! bytes.

use std::fmt;
use std::time::{Duration, SystemTime};

use crate::error::{Result, ZoeError};
use crate::lock::Held;
use crate::store::Options;
use crate::types::{Oid, Tid};

/// The most per-lock lines that one release logs. A summary line follows
/// them.
const LOCK_LINES: usize = 10;

/// A size limit of a store.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SizeLimit {
    /// The length of one pickle: `max-record-size`.
    Record,
    /// The sum of the pickles of one transaction: `max-transaction-size`.
    Transaction,
    /// The length of one blob: `max-blob-size`.
    Blob,
}

impl SizeLimit {
    /// Return the configuration key of the limit, for example
    /// `max-record-size`.
    pub fn key(self) -> &'static str {
        match self {
            SizeLimit::Record => "max-record-size",
            SizeLimit::Transaction => "max-transaction-size",
            SizeLimit::Blob => "max-blob-size",
        }
    }

    /// Return the name of the thing that the limit measures.
    pub(crate) fn subject(self) -> &'static str {
        match self {
            SizeLimit::Record => "record",
            SizeLimit::Transaction => "transaction",
            SizeLimit::Blob => "blob",
        }
    }
}

/// The text of a limit is its configuration key.
impl fmt::Display for SizeLimit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.key())
    }
}

/// The guardrail options of a store. `None` turns a guardrail off.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Guardrails {
    max_record_size: Option<u64>,
    max_transaction_size: Option<u64>,
    max_blob_size: Option<u64>,
    lock_warn: Option<Duration>,
    transaction_warn: Option<Duration>,
}

impl Guardrails {
    pub(crate) fn new(options: &Options) -> Self {
        Self {
            max_record_size: options.max_record_size,
            max_transaction_size: options.max_transaction_size,
            max_blob_size: options.max_blob_size,
            lock_warn: options.lock_warn,
            transaction_warn: options.transaction_warn,
        }
    }

    /// Fail with `TooLarge` when the store of `oid` makes `size` bytes,
    /// over `limit`. A size equal to the limit passes.
    pub(crate) fn check(&self, limit: SizeLimit, oid: Oid, size: u64) -> Result<()> {
        let max = match limit {
            SizeLimit::Record => self.max_record_size,
            SizeLimit::Transaction => self.max_transaction_size,
            SizeLimit::Blob => self.max_blob_size,
        };
        match max {
            Some(max) if size > max => Err(ZoeError::TooLarge {
                oid,
                limit,
                size,
                max,
            }),
            _ => Ok(()),
        }
    }

    /// Check `size` of a value of a namespace write against `limit`, as
    /// `check` does for a pickle. A refusal is `KvTooLarge`.
    pub(crate) fn check_kv(&self, limit: SizeLimit, size: u64) -> Result<()> {
        match self.check(limit, Oid(0), size) {
            Err(ZoeError::TooLarge {
                limit, size, max, ..
            }) => Err(ZoeError::KvTooLarge { limit, size, max }),
            other => other,
        }
    }

    /// Log the locks of `held` that were held longer than `lock-warn`,
    /// when the locks are released at `now`. Call it after the release, so
    /// that the log does not lengthen the hold.
    ///
    /// The first `LOCK_LINES` such locks, in oid order, get a line each.
    /// Then one summary line gives the number of such locks and the
    /// longest hold.
    pub(crate) fn locks_released(&self, held: &[Held], now: SystemTime) {
        let Some(warn) = self.lock_warn else {
            return;
        };
        let (mut locks, mut longest) = (0_u64, Duration::ZERO);
        for lock in held {
            let duration = elapsed(lock.since, now);
            if duration <= warn {
                continue;
            }
            if locks < LOCK_LINES as u64 {
                tracing::warn!(
                    oid = lock.oid.0,
                    duration_ms = millis(duration),
                    "lock held past lock-warn"
                );
            }
            locks += 1;
            longest = longest.max(duration);
        }
        if locks > 0 {
            tracing::warn!(
                locks,
                max_duration_ms = millis(longest),
                "summary of the locks held past lock-warn"
            );
        }
    }

    /// Log a transaction that began at `began` and ends at `now`, when it
    /// was open longer than `transaction-warn`. `ended` describes the
    /// transaction.
    pub(crate) fn transaction_ended(&self, began: SystemTime, now: SystemTime, ended: Ended) {
        let Some(warn) = self.transaction_warn else {
            return;
        };
        let duration = elapsed(began, now);
        if duration > warn {
            tracing::warn!(
                duration_ms = millis(duration),
                oids = ended.oids,
                oid = ended.oid.map(|oid| oid.0),
                tid = ended.tid.map(|tid| tid.0),
                "transaction open past transaction-warn"
            );
        }
    }
}

/// What the line of a slow transaction tells about it. An absent field is
/// not logged.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Ended {
    /// The number of oids that the transaction writes.
    pub(crate) oids: u64,
    /// The lowest oid that the transaction writes.
    pub(crate) oid: Option<Oid>,
    /// The tid of the commit, when the transaction committed.
    pub(crate) tid: Option<Tid>,
}

/// Return the time from `since` to `now`. A clock that went back gives
/// zero.
fn elapsed(since: SystemTime, now: SystemTime) -> Duration {
    now.duration_since(since).unwrap_or_default()
}

/// Return `duration` in whole milliseconds.
fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::time::UNIX_EPOCH;

    use tracing::Level;

    use super::*;
    use crate::testing::events_of;

    const LIMITS: [SizeLimit; 3] = [SizeLimit::Record, SizeLimit::Transaction, SizeLimit::Blob];

    fn guardrails(options: Options) -> Guardrails {
        Guardrails::new(&options)
    }

    fn at(millis: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_000_000_000) + Duration::from_millis(millis)
    }

    #[test]
    fn a_size_equal_to_a_limit_passes_and_one_byte_more_fails() {
        let limits = guardrails(Options {
            max_record_size: Some(10),
            max_transaction_size: Some(10),
            max_blob_size: Some(10),
            ..Options::default()
        });
        for limit in LIMITS {
            limits.check(limit, Oid(7), 10).unwrap();
            let error = limits.check(limit, Oid(7), 11).unwrap_err();
            assert!(
                matches!(
                    error,
                    ZoeError::TooLarge { oid: Oid(7), limit: refused, size: 11, max: 10 }
                        if refused == limit
                ),
                "{error:?}"
            );
            let message = error.to_string();
            assert!(message.contains(limit.key()), "{message}");
            assert!(message.contains(limit.subject()), "{message}");
        }
    }

    #[test]
    fn no_limit_is_set_by_default() {
        let limits = guardrails(Options::default());
        for limit in LIMITS {
            limits.check(limit, Oid(1), u64::MAX).unwrap();
        }
    }

    #[test]
    fn each_limit_shows_its_configuration_key() {
        let keys: Vec<String> = LIMITS.iter().map(ToString::to_string).collect();
        assert_eq!(
            keys,
            ["max-record-size", "max-transaction-size", "max-blob-size"]
        );
    }

    #[test]
    fn a_lock_is_logged_only_past_lock_warn() {
        let warn = guardrails(Options {
            lock_warn: Some(Duration::from_secs(1)),
            ..Options::default()
        });
        let held = [
            Held {
                oid: Oid(1),
                since: at(0),
            },
            Held {
                oid: Oid(2),
                since: at(500),
            },
            Held {
                oid: Oid(3),
                since: at(499),
            },
        ];
        // At the release, `Oid(2)` was held exactly `lock-warn`.
        let ((), events) = events_of(|| warn.locks_released(&held, at(1_500)));
        let logged: Vec<_> = events
            .iter()
            .map(|e| (e.number("oid"), e.number("duration_ms")))
            .collect();
        assert_eq!(
            logged,
            [(Some(1), Some(1_500)), (Some(3), Some(1_001)), (None, None)],
            "{events:?}"
        );
        let summary = &events[2];
        assert_eq!(summary.number("locks"), Some(2));
        assert_eq!(summary.number("max_duration_ms"), Some(1_500));
        for event in &events {
            assert_eq!(event.level, Level::WARN);
            assert!(event.message().contains("lock-warn"), "{event:?}");
        }
    }

    #[test]
    fn one_release_logs_at_most_ten_lock_lines_and_a_summary() {
        let warn = guardrails(Options {
            lock_warn: Some(Duration::from_secs(1)),
            ..Options::default()
        });
        // The locks of higher oids were taken later.
        let held: Vec<Held> = (0..1_000)
            .map(|n| Held {
                oid: Oid(n),
                since: at(n),
            })
            .collect();
        let ((), events) = events_of(|| warn.locks_released(&held, at(1_995)));
        let oids: Vec<_> = events.iter().filter_map(|e| e.number("oid")).collect();
        assert_eq!(oids, (0..10).collect::<Vec<_>>());
        assert_eq!(events.len(), 11, "{events:?}");
        let summary = &events[10];
        // Oids 0 to 994 were held longer than `lock-warn`.
        assert_eq!(summary.number("locks"), Some(995));
        assert_eq!(summary.number("max_duration_ms"), Some(1_995));
    }

    #[test]
    fn a_lock_is_not_logged_without_lock_warn_or_when_the_clock_goes_back() {
        let held = [Held {
            oid: Oid(1),
            since: at(10_000),
        }];
        let off = guardrails(Options::default());
        let on = guardrails(Options {
            lock_warn: Some(Duration::ZERO),
            ..Options::default()
        });
        let ((), events) = events_of(|| {
            off.locks_released(&held, at(60_000));
            on.locks_released(&held, at(0));
        });
        assert!(events.is_empty(), "{events:?}");
    }

    #[test]
    fn a_transaction_is_logged_only_past_transaction_warn() {
        let warn = guardrails(Options {
            transaction_warn: Some(Duration::from_secs(2)),
            ..Options::default()
        });
        let off = guardrails(Options::default());
        let committed = Ended {
            oids: 4,
            oid: Some(Oid(7)),
            tid: Some(Tid(99)),
        };
        let ((), events) = events_of(|| {
            warn.transaction_ended(at(0), at(2_000), committed);
            warn.transaction_ended(at(0), at(2_001), committed);
            warn.transaction_ended(at(5_000), at(0), committed);
            off.transaction_ended(at(0), at(60_000), committed);
            warn.transaction_ended(at(0), at(3_000), Ended::default());
        });
        assert_eq!(events.len(), 2, "{events:?}");
        let event = &events[0];
        assert_eq!(event.level, Level::WARN);
        assert!(event.message().contains("transaction-warn"), "{event:?}");
        assert_eq!(event.number("duration_ms"), Some(2_001));
        assert_eq!(event.number("oids"), Some(4));
        assert_eq!(event.number("oid"), Some(7));
        assert_eq!(event.number("tid"), Some(99));
        // An empty transaction that did not commit has no oid and no tid.
        let empty = &events[1];
        assert_eq!(empty.number("oids"), Some(0));
        assert!(!empty.fields.contains_key("oid"), "{empty:?}");
        assert!(!empty.fields.contains_key("tid"), "{empty:?}");
    }
}
