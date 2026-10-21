//! `zoe backup`: write a dump of a store into a backup directory.
//!
//! `zoe_convert::backup` does the work. The command opens the store
//! read-only, so it changes no byte of a store that was closed cleanly.
//! It repairs a store that a writer left open, and says so. The first
//! dump of a directory is the full dump. A later dump holds the
//! transactions after the last dump. With nothing new, the command writes
//! nothing.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use clap::Args;
use zoe_convert::Backup;
use zoe_core::Options;

use super::{Failure, open_read_only};

/// The options of `zoe backup`.
#[derive(Debug, Args)]
pub(super) struct BackupArgs {
    /// The directory of the store.
    #[arg(long, env = "ZOE_PATH", value_name = "STORE")]
    path: PathBuf,

    /// The backup directory.
    ///
    /// The command creates it when it is missing, writes the full dump
    /// there the first time, then a dump of the transactions after the
    /// last dump. It never changes a dump.
    #[arg(long, env = "ZOE_TO", value_name = "DIR")]
    to: PathBuf,
}

/// Run `zoe backup` and write its report to `out`.
pub(super) fn run(args: &BackupArgs, out: &mut dyn Write) -> Result<(), Failure> {
    let path = &args.path;
    // A read-only open changes no byte of a clean store, and fails on a
    // directory without a store.
    let store = open_read_only(path, Options::default())?;
    let report = zoe_convert::backup(&store, &args.to)
        .map_err(|error| Failure::Message(error.to_string()))?;
    drop(store);
    write_report(out, path, &args.to, &report)?;
    Ok(())
}

/// Write the report of a backup.
fn write_report(out: &mut dyn Write, path: &Path, to: &Path, report: &Backup) -> io::Result<()> {
    let Some(dump) = &report.dump else {
        return writeln!(
            out,
            "Nothing new: the dumps in {} end at tid {}, the last tid of the store {}",
            to.display(),
            report.last.hex(),
            path.display()
        );
    };
    let kind = if report.after.0 == 0 {
        "full"
    } else {
        "incremental"
    };
    writeln!(
        out,
        "Backed up the store {} into {}",
        path.display(),
        to.display()
    )?;
    writeln!(out, "  dump          {} ({kind})", dump.display())?;
    writeln!(out, "  after tid     {}", report.after.hex())?;
    writeln!(out, "  last tid      {}", report.last.hex())?;
    writeln!(out, "  transactions  {}", report.transactions)?;
    writeln!(out, "  records       {}", report.records)?;
    let (blobs, bytes) = (report.blobs, report.blob_bytes);
    writeln!(out, "  blobs         {blobs} ({bytes} bytes)")?;
    writeln!(out, "  sequences     {}", report.sequences)?;
    writeln!(out, "  durable rows  {}", report.durable_rows)
}

#[cfg(test)]
mod tests {
    use zoe_core::Tid;

    use super::*;

    fn report(dump: Option<&str>, after: u64) -> Backup {
        Backup {
            dump: dump.map(PathBuf::from),
            after: Tid(after),
            last: Tid(7),
            transactions: 2,
            records: 3,
            blobs: 1,
            blob_bytes: 10,
            sequences: 1,
            durable_rows: 0,
        }
    }

    fn text(report: &Backup) -> String {
        let mut out = Vec::new();
        write_report(&mut out, Path::new("store"), Path::new("dumps"), report).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn the_report_names_the_dump_and_its_kind() {
        let full = text(&report(Some("dumps/a.zoedump"), 0));
        assert!(full.contains("dumps/a.zoedump (full)\n"), "{full}");
        assert!(full.contains("  blobs         1 (10 bytes)\n"), "{full}");
        let incremental = text(&report(Some("dumps/b.zoedump"), 5));
        assert!(incremental.contains("(incremental)\n"), "{incremental}");
    }

    #[test]
    fn the_report_says_when_nothing_is_new() {
        let nothing = text(&report(None, 7));
        assert!(nothing.starts_with("Nothing new: "), "{nothing}");
        assert!(nothing.contains("0x0000000000000007"), "{nothing}");
    }
}
