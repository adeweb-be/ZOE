//! The `zoe-bench` command.
//!
//! `zoe-bench conflicts <log path>...` reads Zope logs and prints, as JSON,
//! the ConflictErrors ranked by class and by oid.
//!
//! `zoe-bench record-store commits` and `zoe-bench record-store crash`
//! measure the record-store backends and print Markdown tables. With
//! `--out <path>`, they also write the raw numbers as JSON.

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Duration;

use serde::Serialize;
use zoe_bench::conflicts::Ranking;
use zoe_bench::record_store::crash::{self, OpenTimes};
use zoe_bench::record_store::summary::{self, Machine};
use zoe_bench::record_store::{BackendKind, Cell, Plan, Row, measure_batched, measure_cell};

const USAGE: &str = "usage: zoe-bench conflicts <log path>...
       zoe-bench record-store commits [--repeat N] [--fsync-ms MS] [--quick] [--kinds K,...] [--out PATH]
       zoe-bench record-store crash --dir DIR [--records N,...] [--repeat N] [--kinds K,...] [--out PATH]";

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    match args.split_first() {
        Some((command, paths)) if command == "conflicts" && !paths.is_empty() => conflicts(paths),
        Some((command, rest)) if command == "record-store" => {
            let rest: Vec<String> = rest
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            match record_store(&rest) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("zoe-bench: {e}");
                    ExitCode::FAILURE
                }
            }
        }
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

type Error = Box<dyn std::error::Error>;

/// The options of a `record-store` command.
struct Options {
    repeat: usize,
    fsync_ms: f64,
    quick: bool,
    kinds: Vec<BackendKind>,
    out: Option<PathBuf>,
    dir: Option<PathBuf>,
    records: Vec<u64>,
}

impl Options {
    fn parse(args: &[String], repeat: usize) -> Result<Self, Error> {
        let mut options = Options {
            repeat,
            fsync_ms: 10.0,
            quick: false,
            kinds: BackendKind::ALL.to_vec(),
            out: None,
            dir: None,
            records: vec![10_000, 100_000],
        };
        let mut args = args.iter();
        while let Some(flag) = args.next() {
            if flag == "--quick" {
                options.quick = true;
                continue;
            }
            let value = args.next().ok_or(format!("{flag} needs a value"))?;
            match flag.as_str() {
                "--repeat" => options.repeat = value.parse()?,
                "--fsync-ms" => options.fsync_ms = value.parse()?,
                "--out" => options.out = Some(value.into()),
                "--dir" => options.dir = Some(value.into()),
                "--records" => {
                    options.records = value.split(',').map(str::parse).collect::<Result<_, _>>()?
                }
                "--kinds" => {
                    options.kinds = value
                        .split(',')
                        .map(|name| BackendKind::from_name(name).ok_or(name))
                        .collect::<Result<_, _>>()
                        .map_err(|name| format!("unknown backend kind {name}"))?
                }
                _ => return Err(format!("unknown option {flag}\n{USAGE}").into()),
            }
        }
        Ok(options)
    }
}

/// Run a `record-store` command.
fn record_store(args: &[String]) -> Result<(), Error> {
    match args.split_first() {
        Some((command, rest)) if command == "commits" => commits(&Options::parse(rest, 3)?),
        Some((command, rest)) if command == "crash" => crash_opens(&Options::parse(rest, 5)?),
        Some((command, rest)) if command == "writer" => writer(rest),
        _ => Err(USAGE.into()),
    }
}

/// The cells of the card: 1 KB and 100 KB objects, 1, 10 and 100 objects
/// per transaction, 1, 16 and 64 writers.
fn cells() -> Vec<Cell> {
    let mut cells = Vec::new();
    for object_size in [1024, 100 * 1024] {
        for objects_per_txn in [1, 10, 100] {
            for writers in [1, 16, 64] {
                cells.push(Cell {
                    object_size,
                    objects_per_txn,
                    writers,
                });
            }
        }
    }
    cells
}

/// The JSON of a `commits` run.
#[derive(Serialize)]
struct CommitRun {
    machine: Machine,
    store: &'static str,
    plan: Plan,
    repeats: usize,
    rows: Vec<Row>,
}

/// Measure every cell for every kind, `repeat` times, on the commit path
/// of the store and, with more than one writer, as a batched estimate.
/// The kinds take turns inside each repeat, so a noisy moment hits all of
/// them.
fn commits(options: &Options) -> Result<(), Error> {
    let fsync_seconds = options.fsync_ms / 1e3;
    let plan = if options.quick {
        Plan::quick(fsync_seconds)
    } else {
        Plan::full(fsync_seconds)
    };
    let mut machine = Machine::describe(None);
    let cells = cells();
    let mut rows = Vec::new();
    for repeat in 0..options.repeat {
        for cell in &cells {
            for &kind in &options.kinds {
                let mut measured = vec![measure_cell(kind, cell, &plan)?];
                if cell.writers > 1 {
                    measured.push(measure_batched(kind, cell, &plan)?);
                }
                for row in measured {
                    eprintln!(
                        "repeat {repeat} {} batch {} {cell:?}: {:.2} WA, {:.1} commits/s",
                        kind.name(),
                        row.batch,
                        row.write_amplification,
                        row.commits_per_second
                    );
                    rows.push(row);
                }
            }
        }
    }
    machine.finish();
    println!("Commit path of the store:\n");
    println!(
        "{}",
        summary::commit_table(&rows, &cells, &options.kinds, false)
    );
    println!("Batched estimate, one batch per backend commit:\n");
    println!(
        "{}",
        summary::commit_table(&rows, &cells, &options.kinds, true)
    );
    println!("Machine: {machine:?}");
    if let Some(out) = &options.out {
        let run = CommitRun {
            machine,
            store: "in memory; each sync sleeps for the fsync time",
            plan,
            repeats: options.repeat,
            rows,
        };
        serde_json::to_writer_pretty(File::create(out)?, &run)?;
    }
    Ok(())
}

/// The JSON of a `crash` run.
#[derive(Serialize)]
struct CrashRun {
    machine: Machine,
    object_size: usize,
    objects_per_txn: usize,
    repeats: usize,
    opens: Vec<OpenTimes>,
}

/// The objects of the crash images: 1 KB pickles, 100 per transaction.
const CRASH_OBJECT_SIZE: usize = 1024;
const CRASH_OBJECTS_PER_TXN: usize = 100;

/// Build a crash image for every kind and store size, and time its opens.
fn crash_opens(options: &Options) -> Result<(), Error> {
    let base = options.dir.as_ref().ok_or("crash needs --dir")?;
    std::fs::create_dir_all(base)?;
    let mut machine = Machine::describe(Some(base));
    let mut opens = Vec::new();
    for &records in &options.records {
        for &kind in &options.kinds {
            let image = base.join(format!("{}-{records}", kind.name()));
            if image.exists() {
                std::fs::remove_dir_all(&image)?;
            }
            let mut writer = Command::new(std::env::current_exe()?);
            writer.args(["record-store", "writer", kind.name()]);
            writer.arg(&image);
            writer.args([
                records.to_string(),
                CRASH_OBJECT_SIZE.to_string(),
                CRASH_OBJECTS_PER_TXN.to_string(),
            ]);
            // The kill lands a few milliseconds after the store is ready,
            // in a commit that is not acknowledged yet.
            let delay = Duration::from_millis(3 + opens.len() as u64 % 5);
            let (tid, written) = crash::kill_when_ready(writer, delay)?;
            let work = base.join("work");
            let times = crash::time_opens(kind, &image, &work, tid, written, options.repeat)?;
            eprintln!(
                "{} {written} records: crash open {:?} ms warm, {:?} ms cold",
                kind.name(),
                times.crash_warm_ms,
                times.crash_cold_ms
            );
            std::fs::remove_dir_all(&image)?;
            opens.push(times);
        }
    }
    machine.finish();
    println!("{}", summary::open_table(&opens));
    println!("Machine: {machine:?}");
    if let Some(out) = &options.out {
        let run = CrashRun {
            machine,
            object_size: CRASH_OBJECT_SIZE,
            objects_per_txn: CRASH_OBJECTS_PER_TXN,
            repeats: options.repeat,
            opens,
        };
        serde_json::to_writer_pretty(File::create(out)?, &run)?;
    }
    Ok(())
}

/// The writer process of a crash image: `writer <kind> <dir> <records>
/// <object size> <objects per txn>`. It commits until it is killed.
fn writer(args: &[String]) -> Result<(), Error> {
    let [kind, dir, records, object_size, objects_per_txn] = args else {
        return Err(USAGE.into());
    };
    let kind = BackendKind::from_name(kind).ok_or("unknown backend kind")?;
    crash::write_until_killed(
        kind,
        Path::new(dir),
        records.parse()?,
        object_size.parse()?,
        objects_per_txn.parse()?,
    )?;
    Ok(())
}
