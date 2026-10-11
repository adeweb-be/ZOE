//! `zoe pack`: pack a store as of a number of days ago, or report what
//! that pack would remove.
//!
//! `--days N` gives the pack tid: the ZODB `TimeStamp` of the time of the
//! clock minus N days of 86,400 seconds, as ZODB's `DB.pack(days=N)`
//! computes it. The command reads the time through the `Clock` trait, so a
//! test can give its own time.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::Args;
use clap::builder::BoolishValueParser;
use zoe_core::{Clock, PackReport, Store, Tid, timestamp};

use super::{Failure, store_failure};

/// The seconds in a day.
const DAY: u64 = 86_400;

/// The database file in the directory of a store.
const DATABASE: &str = "zoe.redb";

/// The options of `zoe pack`.
#[derive(Debug, Args)]
pub(super) struct PackArgs {
    /// The directory of the store.
    #[arg(long, env = "ZOE_PATH", value_name = "STORE")]
    path: PathBuf,

    /// Pack as of N days ago.
    ///
    /// The pack keeps every revision after that time. Of the older
    /// revisions, it keeps the newest one of each object that a reader at
    /// that time can reach, and removes the others.
    #[arg(long, env = "ZOE_DAYS", value_name = "N")]
    days: u64,

    /// Report what the pack would remove, by class, and change nothing.
    #[arg(long, env = "ZOE_DRY_RUN", value_parser = BoolishValueParser::new())]
    dry_run: bool,
}

/// Run `zoe pack` and write its report to `out`.
pub(super) fn run(args: &PackArgs, clock: &dyn Clock, out: &mut dyn Write) -> Result<(), Failure> {
    let path = &args.path;
    let at = days_ago(clock.now(), args.days);
    if args.dry_run {
        return Err(Failure(format!(
            "{}: the dry run of a pack is not built yet",
            path.display()
        )));
    }
    let store = open_existing(path)?;
    let report = store.pack(at).map_err(|e| store_failure(path, e))?;
    let title = format!(
        "Packed the store {} as of tid {:#018x}. It removed:",
        path.display(),
        at.0
    );
    write_report(out, &title, &report)?;
    Ok(())
}

/// Return the tid of the time `days` days before `now`, by ZODB's
/// `TimeStamp` rule. A time before the Unix epoch gives the tid of the
/// epoch.
fn days_ago(now: SystemTime, days: u64) -> Tid {
    let back = Duration::from_secs(days.saturating_mul(DAY));
    timestamp(now.checked_sub(back).unwrap_or(UNIX_EPOCH))
}

/// Open the existing store in `path` for writes. A directory without a
/// store fails, and the call creates nothing.
fn open_existing(path: &Path) -> Result<Store, Failure> {
    if !path.join(DATABASE).is_file() {
        return Err(Failure(format!("no store in {}", path.display())));
    }
    Store::open(path).map_err(|e| store_failure(path, e))
}

/// Write the report of a pack: the totals, then a line for each class.
fn write_report(out: &mut dyn Write, title: &str, report: &PackReport) -> io::Result<()> {
    writeln!(out, "{title}")?;
    let (records, bytes) = (report.records, report.bytes);
    writeln!(out, "  records  {records} ({bytes} bytes)")?;
    writeln!(out, "  objects  {}", report.objects)?;
    let (blobs, blob_bytes) = (report.blobs, report.blob_bytes);
    writeln!(out, "  blobs    {blobs} ({blob_bytes} bytes)")?;
    if report.by_class.is_empty() {
        return Ok(());
    }
    // Each number column is as wide as its header or its widest number.
    let digits = |n: u64| n.to_string().len();
    let (mut records, mut bytes) = ("records".len(), "bytes".len());
    for totals in report.by_class.values() {
        records = records.max(digits(totals.records));
        bytes = bytes.max(digits(totals.bytes));
    }
    writeln!(out, "  by class:")?;
    writeln!(
        out,
        "    {:>records$}  {:>bytes$}  class",
        "records", "bytes"
    )?;
    for (class, totals) in &report.by_class {
        let class = class.as_deref().unwrap_or("<unknown>");
        let (n, size) = (totals.records, totals.bytes);
        writeln!(out, "    {n:>records$}  {size:>bytes$}  {class}")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The time `secs` seconds after the Unix epoch.
    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn days_ago_is_the_timestamp_of_now_minus_whole_days() {
        let now = at(1_709_186_828);
        assert_eq!(days_ago(now, 0), timestamp(now));
        assert_eq!(days_ago(now, 7), timestamp(at(1_709_186_828 - 7 * DAY)));
        // `persistent.timestamp.TimeStamp` gives this tid for this time.
        assert_eq!(days_ago(now, 0), Tid(0x03f6_db4f_2222_2222));
    }

    #[test]
    fn days_before_the_epoch_give_the_tid_of_the_epoch() {
        let now = at(1_709_186_828);
        assert_eq!(days_ago(now, 1_000_000), timestamp(UNIX_EPOCH));
        assert_eq!(days_ago(now, u64::MAX), timestamp(UNIX_EPOCH));
    }

    #[test]
    fn a_report_has_a_line_for_each_class() {
        let mut report = PackReport {
            records: 3,
            bytes: 1_500,
            objects: 1,
            ..PackReport::default()
        };
        for (class, records, bytes) in [(Some("app.content.Document"), 2, 1_400), (None, 1, 100)] {
            let totals = report.by_class.entry(class.map(str::to_owned)).or_default();
            totals.records = records;
            totals.bytes = bytes;
        }
        let mut out = Vec::new();
        write_report(&mut out, "Title", &report).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("records  3 (1500 bytes)"), "{text}");
        assert!(text.contains("\n  by class:\n"), "{text}");
        let document = "\n          2   1400  app.content.Document\n";
        assert!(text.contains(document), "{text}");
        assert!(text.contains("\n          1    100  <unknown>\n"), "{text}");
    }
}
