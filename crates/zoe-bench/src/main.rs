//! The `zoe-bench` command.
//!
//! `zoe-bench conflicts <log path>...` reads Zope logs and prints, as JSON,
//! the ConflictErrors ranked by class and by oid.

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, BufReader, Write};
use std::path::Path;
use std::process::ExitCode;

use zoe_bench::conflicts::Ranking;

const USAGE: &str = "usage: zoe-bench conflicts <log path>...";

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    match args.split_first() {
        Some((command, paths)) if command == "conflicts" && !paths.is_empty() => conflicts(paths),
        _ => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

/// Rank the conflicts of every log in `paths` together and print the
/// report on stdout.
fn conflicts(paths: &[OsString]) -> ExitCode {
    let mut ranking = Ranking::default();
    for path in paths {
        let path = Path::new(path);
        let read = File::open(path).and_then(|file| ranking.read(BufReader::new(file)));
        if let Err(e) = read {
            eprintln!("zoe-bench: {}: {e}", path.display());
            return ExitCode::FAILURE;
        }
    }

    let json = serde_json::to_string_pretty(&ranking.report()).expect("the report is valid JSON");
    match writeln!(io::stdout().lock(), "{json}") {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("zoe-bench: {e}");
            ExitCode::FAILURE
        }
    }
}
