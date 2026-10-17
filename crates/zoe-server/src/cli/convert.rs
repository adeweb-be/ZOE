//! `zoe convert`: a `Data.fs` into a store, or a store into a `Data.fs`.
//!
//! The converters of `zoe-convert` do the work. Each conversion resumes
//! after the last transaction of its target, verifies the target and
//! writes a report. A report that lists a problem fails the command.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use clap::{ArgGroup, Args};
use zoe_convert::Report;
use zoe_core::{Options, Store};

use super::{Failure, blob_dir_failure, open_with_blobs, store_failure};

/// The first bytes of a FileStorage file.
const MAGIC: &[u8] = b"FS30";

/// The options of `zoe convert`. The command takes one direction:
/// `--from-data-fs` or `--to-data-fs`.
#[derive(Debug, Args)]
#[command(group(
    ArgGroup::new("direction")
        .required(true)
        .args(["from_data_fs", "to_data_fs"])
))]
pub(super) struct ConvertArgs {
    /// The directory of the store.
    #[arg(long, env = "ZOE_PATH", value_name = "STORE")]
    path: PathBuf,

    /// The blob directory of the store, the key blob-dir of the <zoe>
    /// section. The default is blobs in the store directory.
    ///
    /// The store does not record its blob directory. With --from-data-fs,
    /// the blobs of the store go there. With --to-data-fs, a store with
    /// blobs whose blob directory has no layout file fails the command
    /// before it writes anything.
    #[arg(long, env = "ZOE_BLOB_DIR", value_name = "DIR")]
    blob_dir: Option<PathBuf>,

    /// Copy the transactions of this Data.fs into the store.
    ///
    /// The command creates the store if it does not exist, and resumes
    /// after the last transaction of the store.
    #[arg(long, env = "ZOE_FROM_DATA_FS", value_name = "FILE")]
    from_data_fs: Option<PathBuf>,

    /// Copy the transactions of the store into this Data.fs.
    ///
    /// The store must exist. The command resumes after the last
    /// transaction of the Data.fs, and needs the blob directory.
    #[arg(
        long,
        env = "ZOE_TO_DATA_FS",
        value_name = "FILE",
        requires = "data_fs_blob_dir"
    )]
    to_data_fs: Option<PathBuf>,

    /// The blob directory of the Data.fs.
    ///
    /// A conversion from a Data.fs without it copies no blob, and its
    /// report counts the blob records as a problem.
    #[arg(long, env = "ZOE_DATA_FS_BLOB_DIR", value_name = "DIR")]
    data_fs_blob_dir: Option<PathBuf>,
}

/// Run `zoe convert` and write its report to `out`.
pub(super) fn run(args: &ConvertArgs, out: &mut dyn Write) -> Result<(), Failure> {
    let (path, blob_dir) = (&args.path, args.blob_dir.as_deref());
    let data_fs_blobs = args.data_fs_blob_dir.as_deref();
    match (&args.from_data_fs, &args.to_data_fs, data_fs_blobs) {
        (Some(data_fs), None, _) => from_data_fs(data_fs, data_fs_blobs, path, blob_dir, out),
        (None, Some(data_fs), Some(data_fs_blobs)) => {
            to_data_fs(path, blob_dir, data_fs, data_fs_blobs, out)
        }
        // The parser refuses these cases. This arm keeps the match whole.
        _ => Err(Failure::Message(
            "give --from-data-fs, or --to-data-fs with --data-fs-blob-dir".to_owned(),
        )),
    }
}

/// Convert the `Data.fs` `data_fs`, with its blob files in
/// `data_fs_blobs`, into the store in `path`, with its blob files in
/// `blob_dir` (`blobs` in `path` when `None`).
fn from_data_fs(
    data_fs: &Path,
    data_fs_blobs: Option<&Path>,
    path: &Path,
    blob_dir: Option<&Path>,
    out: &mut dyn Write,
) -> Result<(), Failure> {
    // The open of the store creates it. Check the file first, so that a
    // wrong file creates no store.
    check_data_fs(data_fs)?;
    let options = Options {
        blob_dir: blob_dir.map(Path::to_path_buf),
        ..Options::default()
    };
    let store = Store::open_with(path, options).map_err(|e| blob_dir_failure(path, e))?;
    let report = zoe_convert::from_datafs(data_fs, data_fs_blobs, &store)
        .map_err(|e| convert_failure(path, e))?;
    let title = format!("{} into the store {}", data_fs.display(), path.display());
    // A conversion that does not verify fails, also when the report does
    // not go out.
    let written = write_report(out, &title, &report);
    verified(&report)?;
    Ok(written?)
}

/// Check that `data_fs` is a regular file that starts with the magic of a
/// FileStorage file. The reader of `zoe-convert` checks this too, but
/// only after the open of the store.
fn check_data_fs(data_fs: &Path) -> Result<(), Failure> {
    let failure =
        |what: &dyn std::fmt::Display| Failure::Message(format!("{}: {what}", data_fs.display()));
    // A FIFO is not a regular file. Its open would wait for a writer.
    let metadata = std::fs::metadata(data_fs).map_err(|e| failure(&e))?;
    if !metadata.is_file() {
        return Err(failure(&"not a regular file"));
    }
    let mut magic = Vec::with_capacity(MAGIC.len());
    File::open(data_fs)
        .and_then(|file| file.take(MAGIC.len() as u64).read_to_end(&mut magic))
        .map_err(|e| failure(&e))?;
    if magic != MAGIC {
        return Err(failure(
            &"not a FileStorage file: the magic is not \"FS30\"",
        ));
    }
    Ok(())
}

/// Convert the store in `path`, with its blob files in `blob_dir`, into
/// the `Data.fs` `data_fs` and the blob directory `data_fs_blobs`.
fn to_data_fs(
    path: &Path,
    blob_dir: Option<&Path>,
    data_fs: &Path,
    data_fs_blobs: &Path,
    out: &mut dyn Write,
) -> Result<(), Failure> {
    // The conversion only reads the store. A read-only open changes no
    // byte of a clean store. It writes `zoe.redb` only when it repairs a
    // store that a writer left open. It fails on a directory without a
    // store. A wrong blob directory fails here, before the conversion
    // writes anything.
    let store = open_with_blobs(path, blob_dir)?;
    let report = zoe_convert::to_datafs(&store, data_fs, data_fs_blobs)
        .map_err(|e| convert_failure(path, e))?;
    let title = format!("the store {} into {}", path.display(), data_fs.display());
    if report.durable_keys > 0 {
        eprintln!("{}", left_out_warning(report.durable_keys));
    }
    // A conversion that does not verify fails, also when the report does
    // not go out.
    let written = write_report(out, &title, &report);
    verified(&report)?;
    Ok(written?)
}

/// Return the warning of a conversion to a `Data.fs` that leaves out
/// `keys` keys of the durable namespaces.
fn left_out_warning(keys: u64) -> String {
    format!(
        "warning: the Data.fs leaves out {keys} keys of the durable KV namespaces: \
         a Data.fs holds no namespace. An application that keeps data there reads \
         none of it from this copy."
    )
}

/// Return the failure of a conversion with the store in `path`. A
/// mismatch lists all its differences, one per line.
fn convert_failure(path: &Path, error: zoe_convert::Error) -> Failure {
    match error {
        zoe_convert::Error::Store(e) => store_failure(path, e),
        zoe_convert::Error::Mismatch { ref problems, .. } => {
            // The message names the first problem. Add the others.
            let mut message = error.to_string();
            for problem in problems.iter().skip(1) {
                message.push_str("\n  ");
                message.push_str(problem);
            }
            Failure::Message(message)
        }
        e => Failure::Message(e.to_string()),
    }
}

/// Write the report of a conversion.
///
/// The line `problems` gives the number of problems. One line for each
/// kind with a problem follows, with its count and no cap. Then come the
/// lines of the capped list.
fn write_report(out: &mut dyn Write, title: &str, report: &Report) -> std::io::Result<()> {
    writeln!(out, "Converted {title}")?;
    writeln!(out, "  transactions  {}", report.transactions)?;
    writeln!(out, "  copied        {}", report.copied)?;
    writeln!(out, "  records       {}", report.records)?;
    writeln!(out, "  blobs         {}", report.blobs)?;
    writeln!(out, "  last tid      {:#018x}", report.last_tid.0)?;
    if report.durable_keys > 0 {
        writeln!(
            out,
            "  left out      {} keys of the durable namespaces: a Data.fs holds no namespace",
            report.durable_keys
        )?;
        writeln!(out, "  {}", left_out_warning(report.durable_keys))?;
    }
    writeln!(out, "  problems      {}", report.problem_count())?;
    let width = report
        .counts()
        .map(|(kind, _)| kind.label().len())
        .max()
        .unwrap_or(0);
    for (kind, count) in report.counts() {
        writeln!(out, "    {:<width$}  {count}", kind.label())?;
    }
    for problem in &report.problems {
        writeln!(out, "    {problem}")?;
    }
    Ok(())
}

/// Fail when the report lists a problem: then the conversion does not
/// verify.
fn verified(report: &Report) -> Result<(), Failure> {
    if report.problems.is_empty() {
        Ok(())
    } else {
        Err(Failure::Message(
            "the conversion does not verify: the report lists the problems".to_owned(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::left_out_warning;

    #[test]
    fn the_warning_of_left_out_keys_counts_them() {
        let warning = left_out_warning(3);
        assert!(warning.starts_with("warning: "), "{warning}");
        assert!(warning.contains("leaves out 3 keys"), "{warning}");
    }
}
