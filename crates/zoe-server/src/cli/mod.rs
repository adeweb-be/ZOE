//! The `zoe` command line.
//!
//! [`main`] parses the arguments, runs one subcommand and returns the exit
//! code. The binary `zoe` and the console script of the `zoedb` wheel both
//! call it, so both run the same code.
//!
//! Every option has a `ZOE_*` environment twin, and the help of the option
//! names it. An option on the command line wins over its twin.
//!
//! The report of a subcommand goes to stdout. A message goes to stderr.
//! The exit code is [`SUCCESS`], [`FAILURE`] or [`USAGE`]. The command
//! parses all its arguments before it opens a file, so a usage error
//! changes nothing on disk. `zoe serve` also reads its configuration file
//! before it opens a file.

mod backup;
#[cfg(feature = "serve")]
mod config;
mod convert;
mod mcp;
mod pack;
mod restore;
mod serve;
mod verify;

use std::ffi::OsString;
use std::fmt;
use std::io::{self, Write};
use std::path::Path;

use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use zoe_core::{Clock, Options, Store, SystemClock, ZoeError};

/// The exit code of a command that succeeds.
pub const SUCCESS: i32 = 0;

/// The exit code of a command that fails. A conversion that does not
/// verify fails.
pub const FAILURE: i32 = 1;

/// The exit code of a usage error.
pub const USAGE: i32 = 2;

/// The command line of ZOE, the Zope Object Engine.
#[derive(Debug, Parser)]
#[command(
    name = "zoe",
    bin_name = "zoe",
    version,
    subcommand_required = true,
    arg_required_else_help = true,
    after_help = "Exit codes: 0 on success, 1 on a failure, 2 on a usage error."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// The subcommands.
#[derive(Debug, Subcommand)]
enum Command {
    /// Convert a Data.fs into a store, or a store into a Data.fs.
    Convert(convert::ConvertArgs),
    /// Pack a store as of a number of days ago, or report what that pack
    /// would remove.
    Pack(pack::PackArgs),
    /// Serve stores with the ZEO 6 msgpack protocol until SIGTERM, each
    /// under its own storage name.
    Serve(serve::ServeArgs),
    /// Serve the read-only ops tools of a store to an agent, over MCP on
    /// stdin and stdout.
    Mcp(mcp::McpArgs),
    /// Check the checksums, the blob files, the references and the classes
    /// of a store, or compare the dumps of a backup directory with it, and
    /// change nothing. Exit with 1 on a finding.
    Verify(verify::VerifyArgs),
    /// Write a dump of a store into a backup directory: the full dump the
    /// first time, then the transactions after the last dump.
    Backup(backup::BackupArgs),
    /// Rebuild a store from the dumps of a backup directory.
    Restore(restore::RestoreArgs),
}

/// Why a subcommand failed. The command writes it to stderr and exits with
/// [`FAILURE`].
#[derive(Debug)]
enum Failure {
    /// The work failed, or a conversion does not verify.
    Message(String),
    /// The command cannot write its report.
    Report(io::Error),
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Message(message) => f.write_str(message),
            Self::Report(error) => write!(f, "cannot write the report: {error}"),
        }
    }
}

impl From<io::Error> for Failure {
    fn from(error: io::Error) -> Self {
        Self::Report(error)
    }
}

impl From<zoe_convert::Error> for Failure {
    fn from(error: zoe_convert::Error) -> Self {
        Self::Message(error.to_string())
    }
}

/// Return the failure of the store in `path`.
fn store_failure(path: &Path, error: ZoeError) -> Failure {
    Failure::Message(format!("{}: {error}", path.display()))
}

/// Open the store in `path` read-only, with its blob files in `blob_dir`
/// (`blobs` in `path` when `None`), for a command that reads the blob
/// files. The open changes no byte and creates nothing.
///
/// A store with blob records whose blob directory has no layout file
/// fails: that directory is not the one of the store (ADR 0027). The
/// message names the blob directory and `--blob-dir`.
fn open_with_blobs(path: &Path, blob_dir: Option<&Path>) -> Result<Store, Failure> {
    let options = Options {
        blob_dir: blob_dir.map(Path::to_path_buf),
        ..Options::default()
    };
    let store = Store::open_read_only_with(path, options).map_err(|e| store_failure(path, e))?;
    store
        .check_blob_dir()
        .map_err(|e| blob_dir_failure(path, e))?;
    Ok(store)
}

/// Return the failure of the store in `path`, for a command that takes
/// `--blob-dir`. The message of `MissingBlobDir` names the blob directory
/// and `--blob-dir`.
fn blob_dir_failure(path: &Path, error: ZoeError) -> Failure {
    match error {
        ZoeError::MissingBlobDir { path: blobs } => Failure::Message(format!(
            "{}: the store has blobs, but the blob directory {} has no layout file: \
             give the blob directory of the store with --blob-dir (ZOE_BLOB_DIR)",
            path.display(),
            blobs.display()
        )),
        e => store_failure(path, e),
    }
}

/// Tell if `error` comes from a reader that closed its end of the output,
/// as `head` does.
fn closed_by_reader(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::BrokenPipe
}

/// Run the `zoe` command with `args`, the program name first, and return
/// the exit code.
///
/// The binary `zoe` and the console script of the `zoedb` wheel call this
/// function. `zoe pack --days` reads the time of the system clock.
pub fn main(args: Vec<OsString>) -> i32 {
    match parse(args) {
        // Each write takes the lock of stdout. `zoe mcp` writes to stdout
        // from other threads, so the command does not hold the lock.
        Ok(cli) => run(&cli, &SystemClock, &mut io::stdout()),
        Err(error) => usage(&error),
    }
}

/// Parse `args`, the program name first. `zoe serve` also takes options
/// and stores from its configuration file.
fn parse(args: Vec<OsString>) -> Result<Cli, clap::Error> {
    let mut command = Cli::command();
    let matches = command.try_get_matches_from_mut(&args)?;
    let cli = Cli::from_arg_matches(&matches).map_err(|error| error.format(&mut command))?;
    serve::configure(cli, &matches, args)
}

/// Write the help, the version or the usage error of `error`, and return
/// the exit code.
fn usage(error: &clap::Error) -> i32 {
    // clap writes the help and the version to stdout, and a usage error to
    // stderr.
    let code = if error.use_stderr() { USAGE } else { SUCCESS };
    let printed = error.print().and_then(|()| {
        io::stdout().flush()?;
        io::stderr().flush()
    });
    match printed {
        Err(error) if !closed_by_reader(&error) => FAILURE,
        _ => code,
    }
}

/// Run the subcommand of `cli` with the time of `clock`. Write the report
/// to `out` and a failure to stderr. Return the exit code.
fn run(cli: &Cli, clock: &dyn Clock, out: &mut dyn Write) -> i32 {
    let (name, result) = match &cli.command {
        Command::Convert(args) => ("convert", convert::run(args, out)),
        Command::Pack(args) => ("pack", pack::run(args, clock, out)),
        Command::Serve(args) => ("serve", serve::run(args, out)),
        Command::Mcp(args) => ("mcp", mcp::run(args)),
        Command::Verify(args) => ("verify", verify::run(args, out)),
        Command::Backup(args) => ("backup", backup::run(args, out)),
        Command::Restore(args) => ("restore", restore::run(args, out)),
    };
    // The console script runs this code in the Python process. Nothing
    // flushes stdout there when the process ends, so flush it here.
    let flushed = out.flush().map_err(Failure::from);
    match result.and(flushed) {
        Ok(()) => SUCCESS,
        // The work is done when the report goes out. So a reader that
        // stops reading does not fail the command.
        Err(Failure::Report(error)) if closed_by_reader(&error) => SUCCESS,
        Err(failure) => {
            let mut stderr = io::stderr().lock();
            let _ = writeln!(stderr, "zoe {name}: {failure}");
            let _ = stderr.flush();
            FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use clap::CommandFactory;
    use zoe_core::{Oid, Store, Tid, timestamp};

    use super::*;

    /// A clock that always gives the same time.
    struct Fixed(SystemTime);

    impl Clock for Fixed {
        fn now(&self) -> SystemTime {
            self.0
        }
    }

    /// The time `days` days after a fixed start.
    fn day(days: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_700_000_000 + days * 86_400)
    }

    /// A root record with the title `title`.
    fn root(title: u8) -> Vec<u8> {
        let mut p = b"\x80\x03cpersistent.mapping\nPersistentMapping\nq\x00.".to_vec();
        p.extend_from_slice(b"\x80\x03}X\x05\x00\x00\x00titleX\x01\x00\x00\x00");
        p.push(title);
        p.extend_from_slice(b"s.");
        p
    }

    /// A store in a new directory, with a revision of the root at each of
    /// `tids`.
    fn store_with_roots(name: &str, tids: &[Tid]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zoe-cli-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(&dir).unwrap();
        for (title, &tid) in (b'a'..).zip(tids) {
            let mut txn = store.tpc_begin_restore(tid, b"", b"", b"").unwrap();
            txn.restore(Oid(0), Some(&root(title))).unwrap();
            txn.vote().unwrap();
            txn.finish().unwrap();
        }
        dir
    }

    #[test]
    fn the_command_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn a_wrong_blob_directory_names_the_option() {
        let blobs = PathBuf::from("elsewhere");
        let error = ZoeError::MissingBlobDir { path: blobs };
        let Failure::Message(message) = blob_dir_failure(Path::new("store"), error) else {
            panic!("not a message");
        };
        assert!(message.starts_with("store: "), "{message}");
        assert!(message.contains(" elsewhere "), "{message}");
        assert!(message.contains("--blob-dir (ZOE_BLOB_DIR)"), "{message}");

        let other = blob_dir_failure(Path::new("store"), ZoeError::ReadOnly);
        assert!(matches!(other, Failure::Message(m) if !m.contains("--blob-dir")));
    }

    #[test]
    fn the_help_of_the_dump_options_names_their_twins() {
        let expected: [(&str, &[&str]); 3] = [
            ("backup", &["ZOE_PATH", "ZOE_TO"]),
            ("restore", &["ZOE_FROM", "ZOE_PATH"]),
            ("verify", &["ZOE_PATH", "ZOE_BACKUP", "ZOE_SCRATCH"]),
        ];
        let mut command = Cli::command();
        for (name, twins) in expected {
            let subcommand = command.find_subcommand_mut(name).unwrap();
            let help = subcommand.render_long_help().to_string();
            for twin in twins {
                assert!(help.contains(twin), "{name}: no {twin} in:\n{help}");
            }
        }
    }

    #[test]
    fn pack_counts_the_days_back_from_the_clock() {
        let tids = [0, 1, 5].map(|days| timestamp(day(days)));
        let dir = store_with_roots("pack-clock", &tids);
        let clock = Fixed(day(10));
        let pack = |days: &str| {
            let path = dir.to_str().unwrap();
            let args = ["zoe", "pack", "--path", path, "--days", days];
            let cli = Cli::try_parse_from(args).unwrap();
            let mut out = Vec::new();
            let code = run(&cli, &clock, &mut out);
            (code, String::from_utf8(out).unwrap())
        };
        let kept = |tid: Tid| {
            let store = Store::open(&dir).unwrap();
            store.load_serial(Oid(0), tid).is_ok()
        };

        // As of day 0, the first revision is the newest one, so it stays.
        let (code, report) = pack("10");
        assert_eq!(code, SUCCESS, "{report}");
        assert!(tids.into_iter().all(kept), "{report}");
        let at = format!("{:#018x}", tids[0].0);
        assert!(report.contains(&at), "{at} is not in:\n{report}");

        // As of day 2, the revision of day 1 is the newest one.
        let (code, report) = pack("8");
        assert_eq!(code, SUCCESS, "{report}");
        assert!(!kept(tids[0]), "{report}");
        assert!(kept(tids[1]) && kept(tids[2]), "{report}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
