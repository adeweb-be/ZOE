//! `zoe verify`: check the checksums, the blob files, the references and
//! the classes of a store.
//!
//! `zoe_convert::verify` does the work. The command opens the store
//! read-only, so it changes no byte. It fails with a report that lists a
//! finding.

use std::collections::BTreeSet;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use clap::{Args, ValueEnum};
use serde_json::{Value, json};
use zoe_convert::Verification;
use zoe_core::Store;

use super::{Failure, store_failure};

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
}

/// Run `zoe verify` and write its report to `out`.
pub(super) fn run(args: &VerifyArgs, out: &mut dyn Write) -> Result<(), Failure> {
    let known = args.classes.as_deref().map(read_classes).transpose()?;
    let path = &args.path;
    // A read-only open changes no byte of the store, and fails on a
    // directory without a store.
    let store = Store::open_read_only(path).map_err(|e| store_failure(path, e))?;
    let report = zoe_convert::verify(&store, known.as_ref());
    drop(store);
    let written = match args.format {
        Format::Text => write_text(out, path, &report),
        Format::Json => write_json(out, &report),
    };
    // A store with a finding fails, also when the report does not go out.
    match report.problems() {
        0 => Ok(written?),
        n => Err(Failure::Message(format!(
            "{}: {n} findings: the report lists them",
            path.display()
        ))),
    }
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
        assert!(
            text.contains("    tid 0x0000000000000005: corrupt transaction row\n"),
            "{text}"
        );
        let dangling = "oid 0x0000000000000063 has no current record";
        assert!(text.contains(dangling), "{text}");
        assert!(text.contains("\n  2  app.Thing\n"), "{text}");
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
