//! `zoe convert`: a `Data.fs` into a store, or a store into a `Data.fs`.
//!
//! The converters of `zoe-convert` do the work. Each conversion resumes
//! after the last transaction of its target, verifies the target and
//! writes a report. A report that lists a problem fails the command.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use clap::builder::BoolishValueParser;
use clap::{ArgAction, ArgGroup, Args};
use zoe_convert::Report;
use zoe_core::{DurableRow, Options, Store};

use super::{Failure, blob_dir_failure, open_with_blobs, store_failure};

/// The first bytes of a FileStorage file.
const MAGIC: &[u8] = b"FS30";

/// The start of the name of the durable namespace of a moved registry of
/// `zoe.plone` (ADR 0050).
const REGISTRY_PREFIX: &str = "zoe.plone.registry:";

/// The key of the marker row of a moved registry.
const MARKER: &[u8] = b"m";

/// The version of the layout of a marker that the command decodes. A
/// marker of this layout holds 17 bytes: this byte, the oid of the
/// registry (8 bytes, big-endian) and the write counter (8 bytes).
const MARKER_LAYOUT: u8 = 1;

/// The number of durable rows that one read of the walk gives.
const ROW_CHUNK: usize = 1024;

/// A moved registry that a marker row shows.
#[derive(Debug, PartialEq, Eq)]
struct MovedRegistry {
    /// The name of its namespace.
    namespace: String,
    /// Its oid, or `None` for a marker of another layout.
    oid: Option<u64>,
}

impl MovedRegistry {
    /// Return the moved registry that `row` shows, or `None` when `row`
    /// is not the marker of a registry namespace.
    fn of(row: &DurableRow) -> Option<Self> {
        if row.key != MARKER || !row.name.starts_with(REGISTRY_PREFIX) {
            return None;
        }
        let oid = match row.payload.as_slice() {
            [MARKER_LAYOUT, rest @ ..] if rest.len() == 16 => {
                rest[..8].try_into().ok().map(u64::from_be_bytes)
            }
            _ => None,
        };
        Some(Self {
            namespace: row.name.clone(),
            oid,
        })
    }
}

impl std::fmt::Display for MovedRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.oid {
            Some(oid) => write!(f, "registry {oid:#018x} (namespace {})", self.namespace),
            None => write!(
                f,
                "registry of the namespace {}: a marker of another layout",
                self.namespace
            ),
        }
    }
}

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

    /// Convert a store that holds a moved registry of zoe.plone into a
    /// Data.fs anyway.
    ///
    /// Without it, --to-data-fs refuses such a store and writes nothing: a
    /// Data.fs holds no namespace, so the registry of the copy raises
    /// Unsupported on each request. Move each registry back first. With
    /// --force, the command converts and writes a warning that names each
    /// registry. With --force=false the command refuses, also when the twin
    /// forces. With --from-data-fs it has no effect.
    #[arg(
        long,
        env = "ZOE_FORCE",
        value_name = "BOOL",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        default_value = "false",
        value_parser = BoolishValueParser::new(),
        action = ArgAction::Set
    )]
    force: bool,
}

/// Run `zoe convert` and write its report to `out`.
pub(super) fn run(args: &ConvertArgs, out: &mut dyn Write) -> Result<(), Failure> {
    let (path, blob_dir) = (&args.path, args.blob_dir.as_deref());
    let data_fs_blobs = args.data_fs_blob_dir.as_deref();
    match (&args.from_data_fs, &args.to_data_fs, data_fs_blobs) {
        (Some(data_fs), None, _) => from_data_fs(data_fs, data_fs_blobs, path, blob_dir, out),
        (None, Some(data_fs), Some(data_fs_blobs)) => {
            to_data_fs(path, blob_dir, data_fs, data_fs_blobs, args.force, out)
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
    let written = write_report(out, &title, &report, &[]);
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
///
/// A store that holds a moved registry fails before the conversion
/// writes anything, unless `force` is true.
fn to_data_fs(
    path: &Path,
    blob_dir: Option<&Path>,
    data_fs: &Path,
    data_fs_blobs: &Path,
    force: bool,
    out: &mut dyn Write,
) -> Result<(), Failure> {
    // The conversion only reads the store. A read-only open changes no
    // byte of a clean store. It writes `zoe.redb` only when it repairs a
    // store that a writer left open. It fails on a directory without a
    // store. A wrong blob directory fails here, before the conversion
    // writes anything.
    let store = open_with_blobs(path, blob_dir)?;
    // The guard runs before the conversion, so a refusal writes nothing.
    let moved = moved_registries(&store).map_err(|e| store_failure(path, e))?;
    if !moved.is_empty() && !force {
        return Err(Failure::Message(refusal(path, &moved)));
    }
    let report = zoe_convert::to_datafs(&store, data_fs, data_fs_blobs)
        .map_err(|e| convert_failure(path, e))?;
    let title = format!("the store {} into {}", path.display(), data_fs.display());
    if report.durable_keys > 0 {
        eprintln!("{}", left_out_warning(report.durable_keys));
    }
    if !moved.is_empty() {
        eprintln!("{}", forced_warning(&moved));
    }
    // A conversion that does not verify fails, also when the report does
    // not go out.
    let written = write_report(out, &title, &report, &moved);
    verified(&report)?;
    Ok(written?)
}

/// Return each moved registry of `store`, in the order of the names of
/// their namespaces.
///
/// The walk reads the durable rows in chunks of `ROW_CHUNK` rows, and
/// keeps only the marker rows of the registry namespaces.
fn moved_registries(store: &Store) -> zoe_core::Result<Vec<MovedRegistry>> {
    let mut moved = Vec::new();
    let mut after: Option<Vec<u8>> = None;
    loop {
        let (rows, next) = store.kv_durable_rows(after.as_deref(), ROW_CHUNK)?;
        moved.extend(rows.iter().filter_map(MovedRegistry::of));
        if rows.len() < ROW_CHUNK {
            moved.sort_by(|a, b| a.namespace.cmp(&b.namespace));
            return Ok(moved);
        }
        after = next;
    }
}

/// Return the message of the refusal of the store in `path`, which holds
/// the moved registries `moved`.
fn refusal(path: &Path, moved: &[MovedRegistry]) -> String {
    let mut message = format!(
        "{}: the store holds {}. A Data.fs holds no namespace, so the registry of \
         the copy raises Unsupported on each request:",
        path.display(),
        count(moved)
    );
    for registry in moved {
        message.push_str("\n  ");
        message.push_str(&registry.to_string());
    }
    message.push_str(
        "\nMove each registry back first: run move_back(site.portal_registry) and \
         commit, then convert. Or give --force (ZOE_FORCE) to convert anyway.",
    );
    message
}

/// Return the warning of a forced conversion of a store that holds the
/// moved registries `moved`.
fn forced_warning(moved: &[MovedRegistry]) -> String {
    let registries: Vec<String> = moved.iter().map(ToString::to_string).collect();
    format!(
        "warning: --force: the Data.fs leaves out the rows of {}: {}. The registry \
         of the copy raises Unsupported on each request. Move each registry back in \
         the store, then convert again.",
        count(moved),
        registries.join(", ")
    )
}

/// Return the count of the moved registries `moved`, as text.
fn count(moved: &[MovedRegistry]) -> String {
    match moved.len() {
        1 => "1 moved registry of zoe.plone".to_owned(),
        n => format!("{n} moved registries of zoe.plone"),
    }
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
/// lines of the capped list. A forced conversion of a store with the moved
/// registries `moved` gives them before the line `problems`.
fn write_report(
    out: &mut dyn Write,
    title: &str,
    report: &Report,
    moved: &[MovedRegistry],
) -> std::io::Result<()> {
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
    if !moved.is_empty() {
        writeln!(
            out,
            "  registries    {}, converted with --force",
            count(moved)
        )?;
        writeln!(out, "  {}", forced_warning(moved))?;
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
    use std::path::Path;

    use zoe_core::DurableRow;

    use super::{MovedRegistry, forced_warning, left_out_warning, refusal};

    /// The name of the namespace of a registry.
    const NAMESPACE: &str = "zoe.plone.registry:0123456789abcdef0123456789abcdef";

    fn row(name: &str, key: &[u8], payload: &[u8]) -> DurableRow {
        DurableRow {
            name: name.to_owned(),
            key: key.to_vec(),
            payload: payload.to_vec(),
        }
    }

    /// Return a marker of the layout 1 for the registry `oid`.
    fn marker(oid: u64) -> Vec<u8> {
        let mut data = vec![1];
        data.extend_from_slice(&oid.to_be_bytes());
        data.extend_from_slice(&7u64.to_be_bytes());
        data
    }

    #[test]
    fn the_warning_of_left_out_keys_counts_them() {
        let warning = left_out_warning(3);
        assert!(warning.starts_with("warning: "), "{warning}");
        assert!(warning.contains("leaves out 3 keys"), "{warning}");
    }

    #[test]
    fn only_the_marker_of_a_registry_namespace_shows_a_moved_registry() {
        let moved = MovedRegistry::of(&row(NAMESPACE, b"m", &marker(0x2a)));
        assert_eq!(
            moved,
            Some(MovedRegistry {
                namespace: NAMESPACE.to_owned(),
                oid: Some(0x2a),
            })
        );
        // The other rows of the namespace, its generation row included.
        for key in [&b""[..], b"fplone.site_title", b"vplone.site_title", b"mm"] {
            assert_eq!(MovedRegistry::of(&row(NAMESPACE, key, &marker(1))), None);
        }
        // A key `m` of another namespace.
        for name in ["app.settings", "zoe.plone.registry", "zoe.plone.other:1"] {
            assert_eq!(MovedRegistry::of(&row(name, b"m", &marker(1))), None);
        }
    }

    #[test]
    fn a_marker_of_another_layout_shows_a_registry_without_an_oid() {
        let mut other = marker(0x2a);
        other[0] = 2;
        let mut longer = marker(0x2a);
        longer.push(0);
        for payload in [other, longer, vec![1], Vec::new()] {
            let moved = MovedRegistry::of(&row(NAMESPACE, b"m", &payload));
            assert_eq!(
                moved,
                Some(MovedRegistry {
                    namespace: NAMESPACE.to_owned(),
                    oid: None,
                }),
                "{payload:?}"
            );
        }
    }

    #[test]
    fn the_refusal_and_the_warning_name_each_registry() {
        let moved = [
            MovedRegistry {
                namespace: NAMESPACE.to_owned(),
                oid: Some(0x2a),
            },
            MovedRegistry {
                namespace: "zoe.plone.registry:other".to_owned(),
                oid: None,
            },
        ];
        let message = refusal(Path::new("store"), &moved);
        assert!(message.starts_with("store: "), "{message}");
        let warning = forced_warning(&moved);
        assert!(warning.starts_with("warning: "), "{warning}");
        for text in [&message, &warning] {
            assert!(text.contains("0x000000000000002a"), "{text}");
            assert!(text.contains(NAMESPACE), "{text}");
            assert!(text.contains("zoe.plone.registry:other"), "{text}");
            assert!(text.contains("another layout"), "{text}");
        }
        assert!(message.contains("move_back"), "{message}");
        assert!(message.contains("--force (ZOE_FORCE)"), "{message}");
    }
}
