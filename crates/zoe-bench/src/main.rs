//! The `zoe-bench` command.
//!
//! - `zoe-bench conflicts <log path>...` reads Zope logs and prints, as
//!   JSON, the ConflictErrors ranked by class and by oid.
//! - `zoe-bench load` runs clients against a ZEO server and prints, as
//!   JSON, the throughput, the latency of `loadBefore` and the contention.
//! - `zoe-bench saturate` adds clients step by step until the p99 of
//!   `loadBefore` passes a ceiling, and prints the steps as JSON.
//! - `zoe-bench catalog` starts `zoe serve` on a catalog fixture, runs the
//!   plans of its plans file through `catalog_query` and `text_query`, and
//!   prints the result as JSON.
//!
//! The exit code is 0 on success, 1 on a failure and 2 on a usage error.

use std::fs::File;
use std::io::{self, BufReader, Write};
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use serde::Serialize;
use zoe_bench::catalog::plans::Plans;
use zoe_bench::catalog::server::Launch;
use zoe_bench::catalog::{self, Scenario};
use zoe_bench::conflicts::Ranking;
use zoe_bench::hot::{self, HotSettings, Object};
use zoe_bench::load::{self, LoadSettings, SaturateSettings, Workload};

/// A load generator that speaks the ZEO protocol.
#[derive(Debug, Parser)]
#[command(name = "zoe-bench", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Rank the ConflictErrors of Zope logs by class and by oid.
    Conflicts {
        /// The Zope logs to read.
        #[arg(required = true, value_name = "LOG")]
        paths: Vec<PathBuf>,
    },
    /// Run clients against a ZEO server, and report the throughput, the
    /// latency of loadBefore and the contention.
    Load(LoadArgs),
    /// Add clients step by step (1, 2, 4 and on) until the p99 of
    /// loadBefore passes a ceiling, and report the loads per second of the
    /// last step under it.
    Saturate(SaturateArgs),
    /// Run writers of one hot object, with vote-time locks or with early
    /// write-intent locks, and report the conflicts and the commits of a
    /// sync.
    Hot(HotArgs),
    /// Start zoe serve on a catalog fixture and run the plans of its plans
    /// file: the five-index intersections, the cold start, the text
    /// queries and the query mix.
    Catalog(CatalogArgs),
}

#[derive(Debug, Args)]
struct CatalogArgs {
    /// The `zoe` binary.
    #[arg(long, env = "ZOE_BINARY", value_name = "FILE")]
    zoe: PathBuf,
    /// The ZOE store of the fixture: a copy, which the run changes.
    #[arg(long, value_name = "DIR")]
    store: PathBuf,
    /// The plans file of the fixture.
    #[arg(long, value_name = "FILE")]
    plans: PathBuf,
    /// The scenarios, separated by commas. They run in the order cold,
    /// intersect, mix, text, text-rebuild.
    #[arg(
        long,
        value_enum,
        value_delimiter = ',',
        default_value = "cold,intersect,mix,text"
    )]
    scenario: Vec<Scenario>,
    /// The bound of the mirrors of zoe serve, such as 8GB. Without it, the
    /// default of the server.
    #[arg(long, value_name = "SIZE")]
    catalog_memory: Option<String>,
    /// The rebase threshold of the mirror: `--catalog-rebase-rows` of zoe
    /// serve, whose default it is.
    #[arg(long, default_value = "20000", value_name = "ROWS")]
    rebase_rows: u64,
    /// The time of each cell, in seconds.
    #[arg(long, default_value = "20", value_name = "SECONDS", value_parser = seconds)]
    seconds: f64,
    /// The least rounds of the calls of each connection in a cell.
    #[arg(long, default_value = "1", value_name = "N")]
    rounds: usize,
    /// The longest wait for the mirror and for the text index, in seconds.
    #[arg(long, default_value = "900", value_name = "SECONDS", value_parser = seconds)]
    ready_timeout: f64,
    /// The commits of text deltas of the scenario text.
    #[arg(long, default_value = "20", value_name = "N")]
    text_commits: usize,
    /// The text deltas of each of these commits.
    #[arg(long, default_value = "500", value_name = "N", value_parser = positive)]
    text_batch: usize,
    /// The time between two of these commits, in seconds.
    #[arg(long, default_value = "0.6", value_name = "SECONDS", value_parser = seconds_or_zero)]
    text_pause: f64,
    /// The time that the probe of the invalidations runs after the last
    /// write of the mix, for the rebase, in seconds.
    #[arg(long, default_value = "30", value_name = "SECONDS", value_parser = seconds_or_zero)]
    rebase_seconds: f64,
    /// Whether the tree of the run differs from its commit.
    #[arg(long, env = "ZOE_BENCH_DIRTY", value_name = "BOOL")]
    dirty: Option<bool>,
    /// Also write the Markdown report to this file.
    #[arg(long, value_name = "FILE")]
    markdown: Option<PathBuf>,
    /// Send each plan that has the answer of the Python path once, compare
    /// the rids and the counts, and exit with 1 on a difference.
    #[arg(long)]
    check: bool,
}

#[derive(Debug, Args)]
struct HotArgs {
    #[command(flatten)]
    target: Target,
    /// The admin address of the server, for its /metrics: the syncs, the
    /// lock hand-offs and the intent locks of the run.
    #[arg(long, value_name = "HOST:PORT")]
    admin: Option<String>,
    /// The number of writers. Each writer has its own connection.
    #[arg(long, default_value = "64", value_name = "N", value_parser = positive)]
    writers: usize,
    /// The increments of each writer.
    #[arg(long, default_value = "20", value_name = "T", value_parser = positive)]
    transactions: usize,
    /// The object: length, which tier 1 merges, or mapping, which no tier
    /// merges.
    #[arg(long, default_value = "length", value_enum)]
    object: Object,
    /// Take the early write-intent lock of the object before each read.
    #[arg(long)]
    early_locks: bool,
    /// The most attempts of one transaction. A transaction that meets a
    /// conflict at each of them gives up, as a request of Zope does after
    /// its retries.
    #[arg(long, default_value = "4", value_name = "A", value_parser = positive)]
    attempts: usize,
}

#[derive(Debug, Args)]
struct LoadArgs {
    #[command(flatten)]
    target: Target,
    /// The number of clients. Each client has its own connection.
    #[arg(long, default_value = "8", value_name = "N", value_parser = positive)]
    clients: usize,
    /// The number of transactions of each client.
    #[arg(long, default_value = "100", value_name = "T", value_parser = positive)]
    transactions: usize,
    /// The probability that a transaction writes back the objects that it
    /// reads, from 0 to 1.
    #[arg(long, default_value = "0.1", value_name = "W", value_parser = ratio)]
    write_ratio: f64,
    #[command(flatten)]
    workload: WorkloadArgs,
}

#[derive(Debug, Args)]
struct SaturateArgs {
    #[command(flatten)]
    target: Target,
    /// The most clients of a step.
    #[arg(long, default_value = "64", value_name = "N", value_parser = positive)]
    max_clients: usize,
    /// The ceiling of the p99 of loadBefore, in seconds. The default is
    /// the number of N3. A run on the small default store is not an N3
    /// verdict: N3 is a cache miss on a 10 GB store.
    #[arg(long, default_value = "0.0003", value_name = "SECONDS", value_parser = seconds)]
    p99_ceiling: f64,
    /// The time at the start of each step that does not count, in seconds.
    /// At most 3600.
    #[arg(long, default_value = "0.2", value_name = "SECONDS", value_parser = seconds_or_zero)]
    warmup_seconds: f64,
    /// The time of each step after the warm-up, in seconds. At most 3600.
    #[arg(long, default_value = "1.0", value_name = "SECONDS", value_parser = step_seconds)]
    step_seconds: f64,
    /// The most transactions of each client in a step after the warm-up.
    /// A step ends at the end of its time or at this limit, the first of
    /// the two. Without it, only the time ends a step.
    #[arg(long, value_name = "T", value_parser = positive)]
    transactions: Option<usize>,
    /// The probability that a transaction writes back the objects that it
    /// reads, from 0 to 1. The default 0 measures only the read path.
    #[arg(long, default_value = "0", value_name = "W", value_parser = ratio)]
    write_ratio: f64,
    #[command(flatten)]
    workload: WorkloadArgs,
}

#[derive(Debug, Args)]
struct Target {
    /// The address of the ZEO server.
    #[arg(long, value_name = "HOST:PORT")]
    server: String,
    /// The storage name that each client gives to register.
    #[arg(long, default_value = "1", value_name = "NAME")]
    storage: String,
}

#[derive(Debug, Args)]
struct WorkloadArgs {
    /// The most loadBefore calls in flight for one client.
    #[arg(long, default_value = "1", value_name = "D", value_parser = positive)]
    pipeline: usize,
    /// The number of objects that the run creates first.
    #[arg(long, default_value = "10000", value_name = "K", value_parser = positive)]
    objects: usize,
    /// The number of different objects that each transaction reads. At
    /// most --objects.
    #[arg(long, default_value = "10", value_name = "S", value_parser = positive)]
    txn_size: usize,
    /// The exponent of the Zipf law that picks the objects. 0 is uniform.
    /// A larger exponent makes a smaller hot set. 1.0 is the hot-spot
    /// case: the hottest object is in almost every transaction.
    #[arg(long, default_value = "0.8", value_name = "Z", value_parser = exponent)]
    zipf: f64,
    /// The seed of the schedules of the clients. The same seed gives the
    /// same schedules.
    #[arg(long, default_value = "0")]
    seed: u64,
}

impl WorkloadArgs {
    fn workload(self, storage: String, write_ratio: f64) -> Workload {
        Workload {
            storage,
            pipeline: self.pipeline,
            objects: self.objects,
            txn_size: self.txn_size,
            write_ratio,
            zipf: self.zipf,
            seed: self.seed,
            object_size: Workload::record_size(),
        }
    }
}

fn positive(text: &str) -> Result<usize, String> {
    match text.parse::<usize>() {
        Ok(n) if n >= 1 => Ok(n),
        _ => Err("not an integer of 1 or more".into()),
    }
}

fn ratio(text: &str) -> Result<f64, String> {
    match text.parse::<f64>() {
        Ok(x) if (0.0..=1.0).contains(&x) => Ok(x),
        _ => Err("not a number from 0 to 1".into()),
    }
}

fn exponent(text: &str) -> Result<f64, String> {
    match text.parse::<f64>() {
        Ok(x) if x.is_finite() && x >= 0.0 => Ok(x),
        _ => Err("not a number of 0 or more".into()),
    }
}

/// The longest warm-up or step of `saturate`, in seconds.
const MAX_STEP_SECONDS: f64 = 3600.0;

fn seconds_or_zero(text: &str) -> Result<f64, String> {
    match text.parse::<f64>() {
        Ok(x) if (0.0..=MAX_STEP_SECONDS).contains(&x) => Ok(x),
        _ => Err(format!(
            "not a number of seconds from 0 to {MAX_STEP_SECONDS}"
        )),
    }
}

fn step_seconds(text: &str) -> Result<f64, String> {
    match text.parse::<f64>() {
        Ok(x) if x > 0.0 && x <= MAX_STEP_SECONDS => Ok(x),
        _ => Err(format!(
            "not a number of seconds over 0 and at most {MAX_STEP_SECONDS}"
        )),
    }
}

fn seconds(text: &str) -> Result<f64, String> {
    match text.parse::<f64>() {
        Ok(x) if x.is_finite() && x > 0.0 => Ok(x),
        _ => Err("not a number of seconds over 0".into()),
    }
}

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            let _ = e.print();
            return ExitCode::from(if e.use_stderr() { 2 } else { 0 });
        }
    };
    match cli.command {
        Command::Conflicts { paths } => conflicts(&paths),
        Command::Load(args) => {
            let Some(addr) = prepare("load", &args.target, &args.workload) else {
                return ExitCode::from(2);
            };
            let settings = LoadSettings {
                clients: args.clients,
                transactions: args.transactions,
                workload: args
                    .workload
                    .workload(args.target.storage, args.write_ratio),
            };
            run("load", load::load(addr, settings))
        }
        Command::Hot(args) => {
            let Some(addr) = address("hot", "--server", &args.target.server) else {
                return ExitCode::from(2);
            };
            let admin = match &args.admin {
                Some(admin) => match address("hot", "--admin", admin) {
                    Some(admin) => Some(admin),
                    None => return ExitCode::from(2),
                },
                None => None,
            };
            let settings = HotSettings {
                storage: args.target.storage,
                writers: args.writers,
                transactions: args.transactions,
                object: args.object,
                early_locks: args.early_locks,
                attempts: args.attempts,
            };
            run("hot", hot::hot(addr, admin, settings))
        }
        Command::Saturate(args) => {
            let Some(addr) = prepare("saturate", &args.target, &args.workload) else {
                return ExitCode::from(2);
            };
            let settings = SaturateSettings {
                max_clients: args.max_clients,
                p99_ceiling_seconds: args.p99_ceiling,
                warmup_seconds: args.warmup_seconds,
                step_seconds: args.step_seconds,
                transactions: args.transactions,
                workload: args
                    .workload
                    .workload(args.target.storage, args.write_ratio),
            };
            run("saturate", load::saturate(addr, settings))
        }
        Command::Catalog(args) => catalog_command(args),
    }
}

/// Run `zoe-bench catalog`.
fn catalog_command(args: CatalogArgs) -> ExitCode {
    let plans = match Plans::read(&args.plans) {
        Ok(plans) => plans,
        Err(e) => {
            eprintln!("zoe-bench catalog: {e}");
            return ExitCode::from(2);
        }
    };
    let launch = Launch {
        binary: args.zoe,
        store: args.store,
        catalog_memory: args.catalog_memory,
        rebase_rows: Some(args.rebase_rows),
    };
    let timeout = Duration::from_secs_f64(args.ready_timeout);
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("zoe-bench catalog: {e}");
            return ExitCode::FAILURE;
        }
    };
    if args.check {
        return match runtime.block_on(catalog::check(&launch, &plans, timeout)) {
            Ok(report) => {
                let code = print("catalog", &report);
                let clean = report["checked"].as_u64().unwrap_or(0) > 0
                    && report["mismatches"].as_array().is_some_and(Vec::is_empty);
                if clean { code } else { ExitCode::FAILURE }
            }
            Err(e) => {
                eprintln!("zoe-bench catalog: {e}");
                ExitCode::FAILURE
            }
        };
    }
    let settings = catalog::Settings {
        launch,
        scenarios: args.scenario,
        seconds: args.seconds,
        rounds: args.rounds,
        ready_timeout: timeout,
        text_commits: args.text_commits,
        text_batch: args.text_batch,
        text_pause: Duration::from_secs_f64(args.text_pause),
        rebase_rows: args.rebase_rows,
        rebase_seconds: args.rebase_seconds,
        dirty: args.dirty,
    };
    match runtime.block_on(catalog::run(settings, plans)) {
        Ok(doc) => {
            if let Some(path) = &args.markdown
                && let Err(e) = std::fs::write(path, catalog::report::markdown(&doc))
            {
                eprintln!("zoe-bench catalog: {}: {e}", path.display());
                return ExitCode::FAILURE;
            }
            print("catalog", &doc)
        }
        Err(e) => {
            eprintln!("zoe-bench catalog: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Check the settings that clap cannot check alone, and resolve the
/// address of the server. Return `None` after a usage error.
fn prepare(command: &str, target: &Target, workload: &WorkloadArgs) -> Option<SocketAddr> {
    if workload.txn_size > workload.objects {
        eprintln!("zoe-bench {command}: --txn-size is larger than --objects");
        return None;
    }
    address(command, "--server", &target.server)
}

/// Resolve the address `text` of the option `option`. Return `None` after
/// a usage error.
fn address(command: &str, option: &str, text: &str) -> Option<SocketAddr> {
    match text.to_socket_addrs().map(|mut a| a.next()) {
        Ok(Some(addr)) => Some(addr),
        Ok(None) => {
            eprintln!("zoe-bench {command}: {option} gives no address");
            None
        }
        Err(e) => {
            eprintln!("zoe-bench {command}: {option}: {e}");
            None
        }
    }
}

/// Run `future` on a tokio runtime, and print its report on stdout.
fn run<R: Serialize>(
    command: &str,
    future: impl Future<Output = Result<R, zoe_bench::Error>>,
) -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("zoe-bench {command}: {e}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(future) {
        Ok(report) => print(command, &report),
        Err(e) => {
            eprintln!("zoe-bench {command}: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Rank the conflicts of every log in `paths` together and print the
/// report on stdout.
fn conflicts(paths: &[PathBuf]) -> ExitCode {
    let mut ranking = Ranking::default();
    for path in paths {
        let read = File::open(path).and_then(|file| ranking.read(BufReader::new(file)));
        if let Err(e) = read {
            eprintln!("zoe-bench: {}: {e}", path.display());
            return ExitCode::FAILURE;
        }
    }
    print("conflicts", &ranking.report())
}

/// Print `report` on stdout as JSON.
fn print(command: &str, report: &impl Serialize) -> ExitCode {
    let json = serde_json::to_string_pretty(report).expect("the report is valid JSON");
    match writeln!(io::stdout().lock(), "{json}") {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("zoe-bench {command}: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Command {
        Cli::try_parse_from(args).unwrap().command
    }

    #[test]
    fn the_defaults_of_load_and_saturate() {
        let Command::Load(load) = parse(&["zoe-bench", "load", "--server", "h:1"]) else {
            panic!("not load");
        };
        assert_eq!(load.write_ratio, 0.1);
        assert_eq!(load.transactions, 100);
        assert_eq!(load.workload.zipf, 0.8);
        let Command::Saturate(saturate) = parse(&["zoe-bench", "saturate", "--server", "h:1"])
        else {
            panic!("not saturate");
        };
        assert_eq!(saturate.write_ratio, 0.0);
        assert_eq!((saturate.warmup_seconds, saturate.step_seconds), (0.2, 1.0));
        assert_eq!(saturate.transactions, None);
        assert_eq!(saturate.workload.zipf, 0.8);
        let Command::Saturate(saturate) = parse(&[
            "zoe-bench",
            "saturate",
            "--server",
            "h:1",
            "--write-ratio",
            "0.5",
        ]) else {
            panic!("not saturate");
        };
        assert_eq!(saturate.write_ratio, 0.5);
    }

    #[test]
    fn a_huge_or_bad_step_time_is_a_usage_error() {
        for (option, value) in [
            ("--warmup-seconds", "3601"),
            ("--warmup-seconds", "1e300"),
            ("--warmup-seconds", "-1"),
            ("--warmup-seconds", "inf"),
            ("--step-seconds", "3601"),
            ("--step-seconds", "1e300"),
            ("--step-seconds", "0"),
            ("--step-seconds", "NaN"),
        ] {
            let args = ["zoe-bench", "saturate", "--server", "h:1", option, value];
            let error = Cli::try_parse_from(args).unwrap_err();
            assert!(error.use_stderr(), "{option} {value}");
        }
        let args = [
            "zoe-bench",
            "saturate",
            "--server",
            "h:1",
            "--warmup-seconds",
            "3600",
            "--step-seconds",
            "3600",
        ];
        assert!(Cli::try_parse_from(args).is_ok());
    }
}
