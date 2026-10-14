//! The times of the protocol: the `time` of a history entry, the `start`
//! of a status and the time of a pack.

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

/// Return the tid of the time `time`, in seconds since 1970-01-01, as
/// FileStorage's `pack` computes it:
/// `TimeStamp(*time.gmtime(t)[:5] + (t % 60,))`, to the last bit. Return
/// `None` for a time that has no tid: a time that is not finite, before
/// 1900, or too far in the future for the minutes of a tid.
pub(crate) fn pack_tid(time: f64) -> Option<Tid> {
    // `gmtime` takes the floor of the time.
    let floor = time.floor();
    if !floor.is_finite() || floor.abs() > 1e15 {
        return None;
    }
    let secs = floor as i64;
    let (year, month, day) = civil_from_days(secs.div_euclid(86_400));
    let in_day = secs.rem_euclid(86_400) as u64;
    let year = u64::try_from(year).ok()?.checked_sub(1900)?;
    let minutes =
        (((year * 12 + month - 1) * 31 + day - 1) * 24 + in_day / 3_600) * 60 + in_day % 3_600 / 60;
    let minutes = u32::try_from(minutes).ok()?;
    // `t % 60` of Python: the remainder has the sign of 60.
    let mut seconds = time % 60.0;
    if seconds < 0.0 {
        seconds += 60.0;
    }
    // `TimeStamp` truncates the seconds to ticks of 60 / 2**32 seconds.
    let ticks = (seconds / (60.0 / 65_536.0 / 65_536.0)) as u32;
    Some(Tid(u64::from(minutes) << 32 | u64::from(ticks)))
}

/// Return `time` in UTC, in the format of Python's `time.ctime()`: for
/// example `Mon Oct  5 09:05:03 2026`.
pub(crate) fn ctime(time: SystemTime) -> String {
    let secs = time
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = secs / 86_400;
    let (year, month, day) = civil_from_days(days as i64);
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

/// Return the year, month and day of the day `days` after 1970-01-01. A
/// negative `days` is a day before.
fn civil_from_days(days: i64) -> (i64, u64, u64) {
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
    fn pack_tid_equals_the_pack_tid_of_filestorage() {
        // `TimeStamp(*time.gmtime(t)[:5] + (t % 60,)).raw()` of persistent
        // for each time. The first is the pack time of
        // docs/spikes/zeo-wire/plain.jsonl.
        for (time, tid) in [
            (1_791_752_629.333_730_2, 0x040c_458f_d27d_96cc),
            (0.0, 0x023c_2b00_0000_0000),
            (59.999_999_999, 0x023c_2b00_ffff_ffff),
            (951_825_599.5, 0x0332_b0af_fddd_dddd),
            (1_709_186_828.123_456, 0x03f6_db4f_22a8_faee),
            (4_102_444_799.999_999, 0x0662_c3ff_ffff_ffbb),
            (1e9, 0x033f_2eea_aaaa_aaaa),
            (86_399.75, 0x023c_309f_feee_eeee),
            (1_791_752_628.0, 0x040c_458f_cccc_cccc),
            // 23:59:59.5 on 1969-12-31, and the start of 1900.
            (-0.5, 0x023c_2aff_fddd_dddd),
            (-2_208_988_800.0, 0),
        ] {
            assert_eq!(pack_tid(time), Some(Tid(tid)), "{time}");
        }
        for time in [f64::NAN, f64::INFINITY, -3e9, 1e15] {
            assert_eq!(pack_tid(time), None, "{time}");
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
