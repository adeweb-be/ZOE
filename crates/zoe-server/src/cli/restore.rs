//! `zoe restore`: rebuild a store from the dumps of a backup directory.
//!
//! `zoe_convert::restore` does the work. It checks every dump before it
//! writes anything, and never writes into a directory that holds a store.
//! A restore that fails leaves nothing at the target.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use clap::Args;
use zoe_convert::Restored;

use super::Failure;

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
    writeln!(out, "  dumps         {}", report.dumps)?;
    writeln!(out, "  last tid      {:#018x}", report.last_tid.0)?;
    writeln!(out, "  transactions  {}", report.transactions)?;
    writeln!(out, "  records       {}", report.records)?;
    writeln!(out, "  blobs         {}", report.blobs)?;
    writeln!(out, "  sequences     {}", report.sequences)
}

#[cfg(test)]
mod tests {
    use zoe_core::Tid;

    use super::*;

    #[test]
    fn the_report_counts_what_the_restore_wrote() {
        let report = Restored {
            dumps: 2,
            transactions: 8,
            records: 12,
            blobs: 2,
            sequences: 1,
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
            text.contains("  last tid      0x0000000000000010\n"),
            "{text}"
        );
        assert!(text.contains("  records       12\n"), "{text}");
    }
}
