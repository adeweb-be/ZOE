//! `zoe verify`: check the checksums, the blob files, the references and
//! the classes of a store, or compare the dumps of a backup directory with
//! the store.
//!
//! `zoe_convert::verify` and `zoe_convert::verify_backup` do the work. The
//! command opens the store read-only, so it changes no byte. With
//! `--backup`, it opens the store only after it built its copy of the
//! dumps. It fails with a report that lists a finding.

use std::collections::BTreeSet;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use clap::{Args, ValueEnum};
use serde_json::{Value, json};
use zoe_convert::{BackupCheck, Verification};

use super::{Failure, closed_by_reader, open_with_blobs};

/// The format of the report.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(super) enum Format {
    /// Lines for a person.
    Text,
    /// One JSON document for a program. Oids and tids are integers.
    Json,
}

/// The options of `zoe verify`.
#[derive(Debug, Args)]
pub(super) struct VerifyArgs {
    /// The directory of the store.
    #[arg(long, env = "ZOE_PATH", value_name = "STORE")]
    path: PathBuf,

    /// The blob directory of the store, the key blob-dir of the <zoe>
    /// section. The default is blobs in the store directory.
    ///
    /// The store does not record its blob directory. A store with blobs
    /// whose blob directory has no layout file fails the command before
    /// the check.
    #[arg(long, env = "ZOE_BLOB_DIR", value_name = "DIR")]
    blob_dir: Option<PathBuf>,

    /// The format of the report.
    #[arg(
        long,
        env = "ZOE_FORMAT",
        value_name = "FORMAT",
        value_enum,
        default_value_t = Format::Text
    )]
    format: Format,

    /// A file of the classes that the application provides, one
    /// module.name per line.
    ///
    /// A class of the store that the file does not name is a finding.
    #[arg(long, env = "ZOE_CLASSES", value_name = "FILE")]
    classes: Option<PathBuf>,

    /// Compare the dumps of this backup directory with the store, in place
    /// of the check of the store.
    ///
    /// The command restores the dumps into a copy in the scratch directory
    /// before it opens the store. Then it opens the store read-only,
    /// compares the copy with the store up to the last tid of the dumps,
    /// and removes the copy. A difference is a finding. A dump that does
    /// not restore fails the command before the open.
    #[arg(
        long,
        env = "ZOE_BACKUP",
        value_name = "DIR",
        conflicts_with = "classes"
    )]
    backup: Option<PathBuf>,

    /// The directory of the copy of --backup, on a filesystem with room for
    /// the whole store. The default is the temporary directory (TMPDIR).
    ///
    /// The command writes the copy only there, and removes it at the end,
    /// also when the check fails. It removes a copy that a killed check
    /// left there.
    #[arg(long, env = "ZOE_SCRATCH", value_name = "SCRATCH", requires = "backup")]
    scratch: Option<PathBuf>,
}

/// Run `zoe verify` and write its report to `out`.
pub(super) fn run(args: &VerifyArgs, out: &mut dyn Write) -> Result<(), Failure> {
    if let Some(dumps) = &args.backup {
        return run_backup(args, dumps, out);
    }
    let known = args.classes.as_deref().map(read_classes).transpose()?;
    let path = &args.path;
    // A read-only open changes no byte of the store, and fails on a
    // directory without a store. A wrong blob directory fails here, so the
    // report does not list each blob.
    let store = open_with_blobs(path, args.blob_dir.as_deref())?;
    let report = zoe_convert::verify(&store, known.as_ref());
    drop(store);
    let written = match args.format {
        Format::Text => write_text(out, path, &report),
        Format::Json => write_json(out, &report),
    }
    .and_then(|()| out.flush());
    found(path, report.problems(), written)
}

/// Compare the dumps in `dumps` with the store of `args`, and write the
/// report to `out`.
fn run_backup(args: &VerifyArgs, dumps: &Path, out: &mut dyn Write) -> Result<(), Failure> {
    let path = &args.path;
    let scratch = args.scratch.clone().unwrap_or_else(std::env::temp_dir);
    // The check builds its copy before it opens the store, and drops the
    // store before it returns.
    let check = zoe_convert::verify_backup(dumps, &scratch, || {
        open_with_blobs(path, args.blob_dir.as_deref())
    })?;
    let written = match args.format {
        Format::Text => write_backup_text(out, path, dumps, &check),
        Format::Json => write_backup_json(out, &check),
    }
    .and_then(|()| out.flush());
    found(path, check.differences.len(), written)
}

/// Return the comparison of the dumps as one JSON value.
fn backup_to_json(check: &BackupCheck) -> Value {
    let restored = &check.restored;
    json!({
        "dumps": restored.dumps,
        "last_tid": restored.last_tid.0,
        "transactions": restored.transactions,
        "records": restored.records,
        "blobs": restored.blobs,
        "sequences": restored.sequences,
        "differences": check.differences,
    })
}

/// Write the comparison of the dumps as one JSON document.
fn write_backup_json(out: &mut dyn Write, check: &BackupCheck) -> io::Result<()> {
    serde_json::to_writer_pretty(&mut *out, &backup_to_json(check)).map_err(io::Error::from)?;
    writeln!(out)
}

/// Write the comparison of the dumps for a person.
fn write_backup_text(
    out: &mut dyn Write,
    path: &Path,
    dumps: &Path,
    check: &BackupCheck,
) -> io::Result<()> {
    let restored = &check.restored;
    writeln!(
        out,
        "Compared the dumps in {} with the store {}",
        dumps.display(),
        path.display()
    )?;
    writeln!(out, "  dumps            {}", restored.dumps)?;
    writeln!(out, "  last tid         {}", hex(restored.last_tid.0))?;
    writeln!(out, "  transactions     {}", restored.transactions)?;
    writeln!(out, "  records          {}", restored.records)?;
    writeln!(out, "  blobs            {}", restored.blobs)?;
    writeln!(out, "  sequences        {}", restored.sequences)?;
    writeln!(out, "Differences: {}", check.differences.len())?;
    for difference in &check.differences {
        writeln!(out, "  {difference}")?;
    }
    Ok(())
}

/// Return the result of a verification that found `n` findings in the
/// store `path` and wrote its report with the result `written`.
///
/// A store with a finding fails, also when the report does not go out. A
/// reader that closes stdout, as `head` does, got the start of the report.
fn found(path: &Path, n: usize, written: io::Result<()>) -> Result<(), Failure> {
    let (findings, them) = match n {
        0 => return Ok(written?),
        1 => ("1 finding".to_owned(), "it"),
        n => (format!("{n} findings"), "them"),
    };
    let path = path.display();
    Err(Failure::Message(match written {
        Err(error) if !closed_by_reader(&error) => {
            format!("{path}: {findings}; cannot write the report: {error}")
        }
        _ => format!("{path}: {findings}: the report lists {them}"),
    }))
}

/// Read the classes of the file `path`: one `module.name` per line. Blank
/// lines do not count.
fn read_classes(path: &Path) -> Result<BTreeSet<String>, Failure> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| Failure::Message(format!("{}: {e}", path.display())))?;
    Ok(text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect())
}

/// Return the report as one JSON value.
fn to_json(report: &Verification) -> Value {
    let checksums: Vec<Value> = report
        .checksums
        .iter()
        .map(|p| json!({"oid": p.oid.map(|o| o.0), "tid": p.tid.map(|t| t.0), "error": p.error}))
        .collect();
    let blobs: Vec<Value> = report
        .blob_files
        .iter()
        .map(|p| json!({"oid": p.oid.0, "tid": p.tid.0, "error": p.error}))
        .collect();
    let references: Vec<Value> = report
        .references
        .iter()
        .map(|d| json!({"oid": d.oid.0, "tid": d.tid.0, "missing": d.missing.0}))
        .collect();
    let pickles: Vec<Value> = report
        .pickles
        .iter()
        .map(|p| json!({"oid": p.oid.0, "tid": p.tid.0, "error": p.error}))
        .collect();
    json!({
        "checksums": checksums,
        "blobs": blobs,
        "references": references,
        "pickles": pickles,
        "classes": report.classes,
        "unknown_classes": report.unknown_classes,
    })
}

/// Write the report as one JSON document.
fn write_json(out: &mut dyn Write, report: &Verification) -> io::Result<()> {
    serde_json::to_writer_pretty(&mut *out, &to_json(report)).map_err(io::Error::from)?;
    writeln!(out)
}

/// Return an oid or a tid as ZODB prints it.
fn hex(n: u64) -> String {
    format!("{n:#018x}")
}

/// Write the report for a person.
fn write_text(out: &mut dyn Write, path: &Path, report: &Verification) -> io::Result<()> {
    writeln!(out, "Verified the store {}", path.display())?;
    writeln!(out, "  transactions     {}", report.transactions)?;
    writeln!(out, "  sequences        {}", report.sequences)?;
    writeln!(out, "  records          {}", report.records)?;
    writeln!(out, "  objects          {}", report.objects)?;
    writeln!(out, "  blobs            {}", report.blobs)?;
    writeln!(out, "Findings: {}", report.problems())?;
    writeln!(out, "  checksums        {}", report.checksums.len())?;
    for p in &report.checksums {
        let oid = p.oid.map(|o| format!("oid {}, ", hex(o.0)));
        let tid = p.tid.map(|t| format!("tid {}: ", hex(t.0)));
        let (oid, tid) = (oid.unwrap_or_default(), tid.unwrap_or_default());
        writeln!(out, "    {oid}{tid}{}", p.error)?;
    }
    writeln!(out, "  blob files       {}", report.blob_files.len())?;
    for p in &report.blob_files {
        let (oid, tid) = (hex(p.oid.0), hex(p.tid.0));
        writeln!(out, "    oid {oid}, tid {tid}: {}", p.error)?;
    }
    writeln!(out, "  references       {}", report.references.len())?;
    for d in &report.references {
        let (oid, tid, missing) = (hex(d.oid.0), hex(d.tid.0), hex(d.missing.0));
        writeln!(
            out,
            "    oid {oid}, tid {tid}: oid {missing} has no current record"
        )?;
    }
    writeln!(out, "  pickles          {}", report.pickles.len())?;
    for p in &report.pickles {
        let (oid, tid) = (hex(p.oid.0), hex(p.tid.0));
        writeln!(out, "    oid {oid}, tid {tid}: {}", p.error)?;
    }
    writeln!(out, "  unknown classes  {}", report.unknown_classes.len())?;
    for class in &report.unknown_classes {
        writeln!(out, "    {class}")?;
    }
    writeln!(out, "Classes: {}", report.classes.len())?;
    let width = report
        .classes
        .values()
        .map(|n| n.to_string().len())
        .max()
        .unwrap_or(1);
    for (class, n) in &report.classes {
        writeln!(out, "  {n:>width$}  {class}")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use zoe_convert::{Dangling, FrameProblem};
    use zoe_core::{Oid, Tid};

    use super::*;

    fn report() -> Verification {
        Verification {
            checksums: vec![FrameProblem {
                oid: None,
                tid: Some(Tid(5)),
                error: "corrupt transaction row".to_owned(),
            }],
            references: vec![Dangling {
                oid: Oid(0),
                tid: Tid(3),
                missing: Oid(99),
            }],
            classes: [("app.Thing".to_owned(), 2)].into(),
            ..Verification::default()
        }
    }

    #[test]
    fn the_json_has_integers_and_null_for_no_oid() {
        let json = to_json(&report());
        assert_eq!(json["checksums"][0]["oid"], Value::Null);
        assert_eq!(json["checksums"][0]["tid"], 5);
        assert_eq!(json["references"][0]["missing"], 99);
        assert_eq!(json["classes"]["app.Thing"], 2);
        assert_eq!(json["unknown_classes"], json!([]));
    }

    #[test]
    fn the_text_names_each_finding() {
        let mut out = Vec::new();
        write_text(&mut out, Path::new("store"), &report()).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("Findings: 2\n"), "{text}");
        assert!(text.contains("\n  sequences        0\n"), "{text}");
        assert!(
            text.contains("    tid 0x0000000000000005: corrupt transaction row\n"),
            "{text}"
        );
        let dangling = "oid 0x0000000000000063 has no current record";
        assert!(text.contains(dangling), "{text}");
        assert!(text.contains("\n  2  app.Thing\n"), "{text}");
    }

    #[test]
    fn the_message_counts_the_findings_and_names_a_write_error() {
        let path = Path::new("store");
        let message = |n, written| match found(path, n, written) {
            Err(Failure::Message(message)) => message,
            other => panic!("{other:?}"),
        };
        assert_eq!(message(1, Ok(())), "store: 1 finding: the report lists it");
        let lists = "store: 2 findings: the report lists them";
        assert_eq!(message(2, Ok(())), lists);
        // A reader that closes stdout got the start of the report.
        let closed = io::Error::from(io::ErrorKind::BrokenPipe);
        assert_eq!(message(2, Err(closed)), lists);
        let full = io::Error::from(io::ErrorKind::StorageFull);
        let text = message(1, Err(full));
        assert!(
            text.starts_with("store: 1 finding; cannot write the report: "),
            "{text}"
        );

        assert!(found(path, 0, Ok(())).is_ok());
        let full = io::Error::from(io::ErrorKind::StorageFull);
        assert!(matches!(found(path, 0, Err(full)), Err(Failure::Report(_))));
    }

    fn backup_check() -> BackupCheck {
        BackupCheck {
            restored: zoe_convert::Restored {
                dumps: 2,
                transactions: 8,
                records: 12,
                blobs: 2,
                sequences: 1,
                last_tid: Tid(0x10),
            },
            differences: vec!["the store lacks transaction 0x0000000000000010".to_owned()],
        }
    }

    #[test]
    fn the_comparison_of_the_dumps_lists_each_difference() {
        let mut out = Vec::new();
        write_backup_text(
            &mut out,
            Path::new("store"),
            Path::new("dumps"),
            &backup_check(),
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.starts_with("Compared the dumps in dumps with the store store\n"),
            "{text}"
        );
        assert!(
            text.contains("  last tid         0x0000000000000010\n"),
            "{text}"
        );
        assert!(
            text.contains("Differences: 1\n  the store lacks transaction"),
            "{text}"
        );

        let json = backup_to_json(&backup_check());
        assert_eq!(json["last_tid"], 16);
        assert_eq!(json["dumps"], 2);
        assert_eq!(json["differences"].as_array().map(Vec::len), Some(1));
    }

    #[test]
    fn a_class_file_gives_one_class_per_line() {
        let dir = std::env::temp_dir().join(format!("zoe-verify-list-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("classes.txt");
        std::fs::write(&file, "a.B\n\n  c.D  \n").unwrap();
        let classes = read_classes(&file).unwrap();
        assert_eq!(
            classes,
            BTreeSet::from(["a.B".to_owned(), "c.D".to_owned()])
        );
        assert!(read_classes(&dir.join("absent")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
