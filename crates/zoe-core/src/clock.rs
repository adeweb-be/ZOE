//! The clock that tids come from, and the durations of `lock-warn` and
//! `transaction-warn`.
//!
//! The core reads the time only through `Clock`, so a test or a simulator
//! can give its own time.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::types::Tid;

/// A source of wall-clock time.
pub trait Clock: Send + Sync {
    /// Return the time now.
    fn now(&self) -> SystemTime;
}

/// The clock of the operating system.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// A clock that moves only when a test moves it. Its clones share the
/// time.
#[cfg(test)]
#[derive(Clone, Debug)]
pub(crate) struct ManualClock(std::sync::Arc<std::sync::Mutex<SystemTime>>);

#[cfg(test)]
impl ManualClock {
    pub(crate) fn new() -> Self {
        let start = UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        Self(std::sync::Arc::new(std::sync::Mutex::new(start)))
    }

    pub(crate) fn advance(&self, by: std::time::Duration) {
        *self.0.lock().unwrap() += by;
    }
}

#[cfg(test)]
impl Clock for ManualClock {
    fn now(&self) -> SystemTime {
        *self.0.lock().unwrap()
    }
}

/// Return the ZODB `TimeStamp` of `time`, in UTC.
///
/// The high 32 bits count the minutes since 1900-01-01, with every month 31
/// days long. The low 32 bits are the seconds in the minute, in units of
/// 60 / 2^32 seconds, rounded down. A time before 1970 gives the tid of
/// 1970-01-01.
pub fn timestamp(time: SystemTime) -> Tid {
    let since_epoch = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = since_epoch.as_secs();
    let (year, month, day) = civil_from_days(secs / 86_400);
    let in_day = secs % 86_400;
    let (hour, minute, second) = (in_day / 3_600, in_day % 3_600 / 60, in_day % 60);

    let minutes = ((((year - 1900) * 12 + month - 1) * 31 + day - 1) * 24 + hour) * 60 + minute;
    let nanos = u128::from(second) * 1_000_000_000 + u128::from(since_epoch.subsec_nanos());
    let ticks = nanos * (1u128 << 32) / 60_000_000_000;
    Tid(minutes << 32 | ticks as u64)
}

/// Return the year, month and day of the day `days` after 1970-01-01, in
/// the proleptic Gregorian calendar.
fn civil_from_days(days: u64) -> (u64, u64, u64) {
    // The algorithm of Howard Hinnant, for days on or after 1970-01-01. The
    // era of 400 years starts on March 1.
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z % 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn at(secs: u64, nanos: u32) -> Tid {
        timestamp(UNIX_EPOCH + Duration::new(secs, nanos))
    }

    /// The vectors come from `persistent.timestamp.TimeStamp`.
    #[test]
    fn timestamps_equal_the_ones_of_zodb() {
        assert_eq!(at(0, 0), Tid(0x023c_2b00_0000_0000));
        assert_eq!(at(946_684_800, 0), Tid(0x0331_6200_0000_0000));
        assert_eq!(at(1_709_186_828, 500_000_000), Tid(0x03f6_db4f_2444_4444));
    }

    /// ZODB computes the seconds with floating point, so the last tick can
    /// differ by one.
    #[test]
    fn fractional_seconds_are_within_one_tick() {
        for (secs, nanos, zodb) in [
            (1_791_635_696, 789_000_000, 0x040c_3df2_f24c_bb52u64),
            (1_772_323_199, 999_999_000, 0x0407_32df_ffff_ffb8),
        ] {
            let tid = at(secs, nanos).0;
            assert_eq!(tid >> 32, zodb >> 32);
            assert!(tid.abs_diff(zodb) <= 1, "{tid:#x} {zodb:#x}");
        }
    }

    #[test]
    fn a_time_before_1970_gives_the_tid_of_1970() {
        assert_eq!(timestamp(UNIX_EPOCH - Duration::from_secs(60)), at(0, 0));
    }
}
