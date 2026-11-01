//! `zoe restore`: rebuild a store from the dumps of a backup directory.
//!
//! `zoe_convert::restore` does the work. It checks every dump before it
//! writes anything, and never writes into a directory that holds a store.
//! A restore that fails leaves nothing at the target.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use clap::Args;
use zoe_convert::Restored;
use zoe_core::{Tid, civil_from_tid};

use super::{Failure, write_counts};

/// The options of `zoe restore`.
#[derive(Debug, Args)]
pub(super) struct RestoreArgs {
    /// The backup directory.
    #[arg(long, env = "ZOE_FROM", value_name = "DIR")]
    from: PathBuf,

    /// The directory of the new store.
    ///
    /// It must not exist, or be an empty directory. The command refuses a
    /// directory that holds a store.
    #[arg(long, env = "ZOE_PATH", value_name = "STORE")]
    path: PathBuf,
}

/// Run `zoe restore` and write its report to `out`.
pub(super) fn run(args: &RestoreArgs, out: &mut dyn Write) -> Result<(), Failure> {
    let report = zoe_convert::restore(&args.from, &args.path)
        .map_err(|error| Failure::Message(error.to_string()))?;
    write_report(out, &args.from, &args.path, &report)?;
    Ok(())
}

/// Write the report of a restore.
fn write_report(
    out: &mut dyn Write,
    from: &Path,
    path: &Path,
    report: &Restored,
) -> io::Result<()> {
    writeln!(
        out,
        "Restored the store {} from {}",
        path.display(),
        from.display()
    )?;
    let (last, time) = (report.last_tid.hex(), utc(report.last_tid));
    write_counts(
        out,
        14,
        &[
            ("dumps", &report.dumps),
            ("last tid", &format!("{last} ({time})")),
            ("transactions", &report.transactions),
            ("records", &report.records),
            ("blobs", &report.blobs),
            ("sequences", &report.sequences),
            ("durable rows", &report.durable_rows),
        ],
    )
}

/// Return the time of `tid` in UTC, as `YYYY-MM-DD HH:MM:SS.mmm UTC`.
fn utc(tid: Tid) -> String {
    let (year, month, day, hour, minute) = civil_from_tid(tid);
    // Round to the millisecond, and stay below the next minute.
    let millis = ((((tid.0 & 0xffff_ffff) * 60_000) + (1 << 31)) >> 32).min(59_999);
    let (second, milli) = (millis / 1000, millis % 1000);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}.{milli:03} UTC")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_time_of_a_tid_is_its_utc_time() {
        // `persistent.timestamp.TimeStamp` gives this tid for 1709186828,
        // in seconds since 1970.
        assert_eq!(
            utc(Tid(0x03f6_db4f_2222_2222)),
            "2024-02-29 06:07:08.000 UTC"
        );
        assert_eq!(utc(Tid(0)), "1900-01-01 00:00:00.000 UTC");
        assert_eq!(
            utc(Tid(0x03f6_db4f_ffff_ffff)),
            "2024-02-29 06:07:59.999 UTC"
        );
    }

    #[test]
    fn the_report_counts_what_the_restore_wrote() {
        let report = Restored {
            dumps: 2,
            transactions: 8,
            records: 12,
            blobs: 2,
            sequences: 1,
            durable_rows: 0,
            last_tid: Tid(0x10),
        };
        let mut out = Vec::new();
        write_report(&mut out, Path::new("dumps"), Path::new("store"), &report).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.starts_with("Restored the store store from dumps\n"),
            "{text}"
        );
        assert!(
            text.contains("  last tid      0x0000000000000010 (1900-01-01 00:00:00.000 UTC)\n"),
            "{text}"
        );
        assert!(text.contains("  records       12\n"), "{text}");
    }
}
