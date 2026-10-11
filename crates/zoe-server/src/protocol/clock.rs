//! The times that the protocol sends: the `time` of a history entry and
//! the `start` of a status.

use std::time::{SystemTime, UNIX_EPOCH};

use zoe_core::Tid;

/// The days before each month, in a common year and in a leap year.
const MONTH_DAYS: [[u64; 12]; 2] = [
    [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334],
    [0, 31, 60, 91, 121, 152, 182, 213, 244, 274, 305, 335],
];

/// The seconds from the base of `TimeStamp` to 1970-01-01.
const EPOCH: f64 = 25_202.0 * 86_400.0;

const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Return the time of `tid` in seconds since 1970-01-01, as
/// `TimeStamp(tid).timeTime()` of `persistent` gives it, to the last bit.
pub(crate) fn tid_time(tid: Tid) -> f64 {
    let minutes = tid.0 >> 32;
    let year = minutes / 535_680 + 1900;
    let month = (minutes % 535_680 / 44_640) as usize;
    let day = minutes % 44_640 / 1_440;
    let in_day = minutes % 1_440;
    let days = year_days(year) + MONTH_DAYS[usize::from(leap(year))][month] as i64 + day as i64;
    let whole = days as f64 * 86_400.0 + (in_day * 60) as f64;
    let seconds = (tid.0 & 0xffff_ffff) as f64 * (60.0 / 4_294_967_296.0);
    // The C code of `persistent` adds and subtracts in this order. Another
    // order can change the last bit.
    whole + seconds - EPOCH
}

/// Return `time` in UTC, in the format of Python's `time.ctime()`: for
/// example `Mon Oct  5 09:05:03 2026`.
pub(crate) fn ctime(time: SystemTime) -> String {
    let secs = time
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = secs / 86_400;
    let (year, month, day) = civil_from_days(days);
    let in_day = secs % 86_400;
    format!(
        "{} {} {:>2} {:02}:{:02}:{:02} {}",
        WEEKDAYS[((days + 4) % 7) as usize],
        MONTHS[(month - 1) as usize],
        day,
        in_day / 3_600,
        in_day % 3_600 / 60,
        in_day % 60,
        year
    )
}

/// Return the days from the base of `TimeStamp` to the start of `year`, as
/// the C code of `persistent` counts them.
fn year_days(year: u64) -> i64 {
    let mut y = year as i64 - 1900;
    let days = (y - 1) * 365;
    let sign = if y > 0 {
        y -= 1;
        1
    } else {
        y = -y;
        -1
    };
    days + sign * (y / 4 - y / 100 + (y + 300) / 400)
}

fn leap(year: u64) -> bool {
    year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400))
}

/// Return the year, month and day of the day `days` after 1970-01-01.
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

    #[test]
    fn tid_time_equals_the_time_of_zodb() {
        // The tids and times of the history reply in
        // docs/spikes/zeo-wire/plain.jsonl.
        assert_eq!(
            tid_time(Tid(0x040c_458f_ce39_5288)),
            1_791_752_628.333_730_2
        );
        assert_eq!(
            tid_time(Tid(0x040c_458f_cdb6_5344)),
            1_791_752_628.213_798_5
        );
        // The tid of 1970-01-01 and the tid of 2000-01-01.
        assert_eq!(tid_time(Tid(0x023c_2b00_0000_0000)), 0.0);
        assert_eq!(tid_time(Tid(0x0331_6200_0000_0000)), 946_684_800.0);
        // The tid 0 is in 1900, before the epoch.
        assert_eq!(tid_time(Tid::ZERO), -2_208_988_800.0);
    }

    #[test]
    fn tid_time_follows_the_core_timestamp() {
        for secs in [86_399, 951_782_400, 1_709_186_828, 4_102_444_799] {
            let time = UNIX_EPOCH + Duration::from_secs(secs);
            assert_eq!(tid_time(zoe_core::timestamp(time)), secs as f64);
        }
    }

    #[test]
    fn ctime_has_the_format_of_python() {
        let at = |secs| ctime(UNIX_EPOCH + Duration::from_secs(secs));
        assert_eq!(at(0), "Thu Jan  1 00:00:00 1970");
        assert_eq!(at(951_825_599), "Tue Feb 29 11:59:59 2000");
        assert_eq!(at(1_791_752_628), "Sun Oct 11 21:03:48 2026");
        assert_eq!(at(1_791_000_000), "Sat Oct  3 04:00:00 2026");
    }
}
