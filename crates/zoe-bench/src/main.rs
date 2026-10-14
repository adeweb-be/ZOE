//! The `zoe-bench` command.
//!
//! - `zoe-bench conflicts <log path>...` reads Zope logs and prints, as
//!   JSON, the ConflictErrors ranked by class and by oid.
//! - `zoe-bench load` runs clients against a ZEO server and prints, as
//!   JSON, the throughput, the latency of `loadBefore` and the contention.
//! - `zoe-bench saturate` adds clients step by step until the p99 of
//!   `loadBefore` passes a ceiling, and prints the steps as JSON.
//!
//! The exit code is 0 on success, 1 on a failure and 2 on a usage error.

use std::fs::File;
use std::io::{self, BufReader, Write};
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use serde::Serialize;
use zoe_bench::conflicts::Ranking;
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
    #[arg(long, default_value = "0.2", value_name = "SECONDS", value_parser = seconds_or_zero)]
    warmup_seconds: f64,
    /// The time of each step after the warm-up, in seconds.
    #[arg(long, default_value = "1.0", value_name = "SECONDS", value_parser = seconds)]
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

fn seconds_or_zero(text: &str) -> Result<f64, String> {
    match text.parse::<f64>() {
        Ok(x) if x.is_finite() && x >= 0.0 => Ok(x),
        _ => Err("not a number of seconds of 0 or more".into()),
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
    }
}

/// Check the settings that clap cannot check alone, and resolve the
/// address of the server. Return `None` after a usage error.
fn prepare(command: &str, target: &Target, workload: &WorkloadArgs) -> Option<SocketAddr> {
    if workload.txn_size > workload.objects {
        eprintln!("zoe-bench {command}: --txn-size is larger than --objects");
        return None;
    }
    match target.server.to_socket_addrs().map(|mut a| a.next()) {
        Ok(Some(addr)) => Some(addr),
        Ok(None) => {
            eprintln!("zoe-bench {command}: --server gives no address");
            None
        }
        Err(e) => {
            eprintln!("zoe-bench {command}: --server: {e}");
            None
        }
    }
}

/// Run `future` on a tokio runtime, and print its report on stdout.
fn run<R: Serialize>(
    command: &str,
    future: impl Future<Output = Result<R, load::Error>>,
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
}
