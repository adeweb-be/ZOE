//! The clock that tids come from, and the durations of `lock-warn` and
//! `transaction-warn`.
//!
//! The core reads the time only through `Clock`, so a test or a simulator
//! can give its own time.

use std::sync::{Arc, Condvar, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::types::Tid;

/// A source of wall-clock time, of monotonic time, and of the waits of the
/// lock requests.
///
/// `monotonic` and `waiter` have defaults that use the operating system. A
/// simulator gives its own, so a lock wait runs on simulated time.
pub trait Clock: Send + Sync {
    /// Return the time now.
    fn now(&self) -> SystemTime;

    /// Return the time since a fixed origin. It never goes back. The lock
    /// timeout is measured on it.
    fn monotonic(&self) -> Duration {
        static ORIGIN: OnceLock<Instant> = OnceLock::new();
        ORIGIN.get_or_init(Instant::now).elapsed()
    }

    /// Return a new waiter for one lock request.
    fn waiter(&self) -> Arc<dyn Waiter> {
        Arc::new(ThreadWaiter::default())
    }
}

/// The wait of one thread. The thread waits on it, and another thread
/// wakes it.
///
/// A wake has permit semantics: a `wake` before the `wait` makes that
/// `wait` return at once, so no wake is lost.
pub trait Waiter: Send + Sync {
    /// Block the calling thread until a call of `wake`, or until `timeout`
    /// passes. A `wake` before this call makes it return at once. The call
    /// can also return early, so the caller checks its state again.
    fn wait(&self, timeout: Duration);

    /// Wake the thread that waits, or make its next wait return at once.
    ///
    /// It must not panic: the lock manager also calls it while the drop of
    /// a transaction unwinds a panic, and a second panic aborts the
    /// process. A waiter that misses its wake sees its lock at the end of
    /// its wait.
    fn wake(&self);
}

/// A waiter on the threads of the operating system: a flag, a mutex and a
/// condition variable.
#[derive(Debug, Default)]
struct ThreadWaiter {
    woken: Mutex<bool>,
    changed: Condvar,
}

impl Waiter for ThreadWaiter {
    fn wait(&self, timeout: Duration) {
        let woken = self.woken.lock().unwrap_or_else(PoisonError::into_inner);
        // A timeout past the range of the clock waits without a limit.
        let mut woken = self
            .changed
            .wait_timeout_while(woken, timeout, |woken| !*woken)
            .unwrap_or_else(PoisonError::into_inner)
            .0;
        *woken = false;
    }

    fn wake(&self) {
        *self.woken.lock().unwrap_or_else(PoisonError::into_inner) = true;
        self.changed.notify_one();
    }
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
    // The days fit an i64, and a day after 1970 has a year after 1970.
    let (year, month, day) = civil_from_days((secs / 86_400) as i64);
    let year = year as u64;
    let in_day = secs % 86_400;
    let (hour, minute, second) = (in_day / 3_600, in_day % 3_600 / 60, in_day % 60);

    let minutes = ((((year - 1900) * 12 + month - 1) * 31 + day - 1) * 24 + hour) * 60 + minute;
    let nanos = u128::from(second) * 1_000_000_000 + u128::from(since_epoch.subsec_nanos());
    let ticks = nanos * (1u128 << 32) / 60_000_000_000;
    Tid(minutes << 32 | ticks as u64)
}

/// Return the year, month and day of the day `days` after 1970-01-01, in
/// the proleptic Gregorian calendar. A negative `days` is a day before.
pub fn civil_from_days(days: i64) -> (i64, u64, u64) {
    // The algorithm of Howard Hinnant. The era of 400 years starts on
    // March 1.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe as i64 + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// Return the year, the month, the day, the hour and the minute of `tid`,
/// in UTC, as ZODB's `TimeStamp` holds them. The month and the day start
/// at 1.
///
/// The high 32 bits of a tid count the minutes since 1900, in a mixed
/// radix: the year since 1900, then 12 months, 31 days, 24 hours and 60
/// minutes. The low 32 bits hold the second in units of 60 / 2^32
/// seconds.
pub fn civil_from_tid(tid: Tid) -> (u64, u64, u64, u64, u64) {
    let minutes = tid.0 >> 32;
    let (minute, hours) = (minutes % 60, minutes / 60);
    let (hour, days) = (hours % 24, hours / 24);
    let (day, months) = (days % 31 + 1, days / 31);
    let (month, year) = (months % 12 + 1, months / 12 + 1900);
    (year, month, day, hour, minute)
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
    fn a_day_and_a_tid_give_their_civil_date() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
        assert_eq!(civil_from_tid(Tid(0)), (1900, 1, 1, 0, 0));
        // The tid of 1709186828 seconds since 1970.
        assert_eq!(civil_from_tid(at(1_709_186_828, 0)), (2024, 2, 29, 6, 7));
    }

    #[test]
    fn a_time_before_1970_gives_the_tid_of_1970() {
        assert_eq!(timestamp(UNIX_EPOCH - Duration::from_secs(60)), at(0, 0));
    }

    #[test]
    fn a_wake_before_the_wait_is_not_lost() {
        let waiter = SystemClock.waiter();
        waiter.wake();
        let start = Instant::now();
        waiter.wait(Duration::from_secs(60));
        assert!(start.elapsed() < Duration::from_secs(30));
        // The wake was used: the next wait lasts its timeout.
        let start = Instant::now();
        waiter.wait(Duration::from_millis(20));
        assert!(start.elapsed() >= Duration::from_millis(20));
    }

    #[test]
    fn a_wake_from_another_thread_ends_a_wait_without_a_limit() {
        let waiter = SystemClock.waiter();
        let waking = {
            let waiter = Arc::clone(&waiter);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(20));
                waiter.wake();
            })
        };
        waiter.wait(Duration::MAX);
        waking.join().unwrap();
    }

    #[test]
    fn the_monotonic_time_never_goes_back() {
        let first = SystemClock.monotonic();
        std::thread::sleep(Duration::from_millis(2));
        assert!(SystemClock.monotonic() > first);
    }
}
