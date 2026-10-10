//! The machine description and the Markdown tables of a run.

use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::process::Command;

use serde::Serialize;

use super::crash::OpenTimes;
use super::{BackendKind, Cell, Row};

/// The machine that a run measured.
#[derive(Clone, Debug, Serialize)]
pub struct Machine {
    /// The CPU model.
    pub cpu: String,
    /// The logical CPUs.
    pub logical_cpus: usize,
    /// The total memory in bytes.
    pub memory_bytes: u64,
    /// The kernel release.
    pub kernel: String,
    /// The load averages over 1, 5 and 15 minutes when the run started.
    pub load_at_start: String,
    /// The load averages when the run ended.
    pub load_at_end: String,
    /// The device and filesystem of the directory of the crash images.
    pub disk: Option<String>,
}

impl Machine {
    /// Describe this machine. `dir` is the directory on the disk that the
    /// run uses, if any.
    pub fn describe(dir: Option<&Path>) -> Self {
        let cpuinfo = fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
        let cpu = cpuinfo
            .lines()
            .find_map(|line| line.strip_prefix("model name"))
            .and_then(|rest| rest.split_once(':'))
            .map(|(_, name)| name.trim().to_owned())
            .unwrap_or_default();
        let meminfo = fs::read_to_string("/proc/meminfo").unwrap_or_default();
        let memory_kib: u64 = meminfo
            .lines()
            .find_map(|line| line.strip_prefix("MemTotal:"))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|n| n.parse().ok())
            .unwrap_or(0);
        let disk = dir.and_then(|dir| {
            let output = Command::new("findmnt")
                .args(["--noheadings", "--output", "SOURCE,FSTYPE", "--target"])
                .arg(dir)
                .output()
                .ok()?;
            Some(String::from_utf8_lossy(&output.stdout).trim().to_owned())
        });
        Self {
            cpu,
            logical_cpus: std::thread::available_parallelism().map_or(0, |n| n.get()),
            memory_bytes: memory_kib * 1024,
            kernel: read_trimmed("/proc/sys/kernel/osrelease"),
            load_at_start: load(),
            load_at_end: String::new(),
            disk,
        }
    }

    /// Record the load at the end of the run.
    pub fn finish(&mut self) {
        self.load_at_end = load();
    }
}

fn read_trimmed(path: &str) -> String {
    fs::read_to_string(path)
        .map(|s| s.trim().to_owned())
        .unwrap_or_default()
}

fn load() -> String {
    read_trimmed("/proc/loadavg")
        .split_whitespace()
        .take(3)
        .collect::<Vec<_>>()
        .join(" ")
}

/// The median, the minimum and the maximum of some values.
#[derive(Clone, Copy, Debug)]
pub struct Spread {
    /// The median.
    pub median: f64,
    /// The minimum.
    pub min: f64,
    /// The maximum.
    pub max: f64,
}

impl Spread {
    /// Return the spread of `values`, which must not be empty.
    pub fn of(values: impl IntoIterator<Item = f64>) -> Self {
        let mut values: Vec<f64> = values.into_iter().collect();
        values.sort_by(f64::total_cmp);
        let n = values.len();
        let median = if n % 2 == 1 {
            values[n / 2]
        } else {
            (values[n / 2 - 1] + values[n / 2]) / 2.0
        };
        Self {
            median,
            min: values[0],
            max: values[n - 1],
        }
    }

    /// Format the median, then the range when it is wider than the
    /// precision.
    fn show(&self, decimals: usize) -> String {
        let median = format!("{:.*}", decimals, self.median);
        let (min, max) = (
            format!("{:.*}", decimals, self.min),
            format!("{:.*}", decimals, self.max),
        );
        if min == max {
            median
        } else {
            format!("{median} ({min}–{max})")
        }
    }
}

/// Format a size in bytes as KB or MB with powers of 1,024.
fn size(bytes: usize) -> String {
    if bytes >= 1 << 20 {
        format!("{} MB", bytes >> 20)
    } else {
        format!("{} KB", bytes >> 10)
    }
}

/// Return the Markdown table of the commit measurements with `batch`
/// transactions per backend commit: 1 for the commit path of the store,
/// more for a batched estimate. Each line is a cell and a kind; the
/// values are the median and the range over the repeats.
pub fn commit_table(rows: &[Row], cells: &[Cell], kinds: &[BackendKind], batched: bool) -> String {
    let mut out = String::new();
    out.push_str(
        "| Object | Objects per txn | Writers | Backend | Commits | Write amplification | KB written per commit | fsyncs per commit | Commits/s | p50 commit ms | p99 commit ms |\n",
    );
    out.push_str("| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |\n");
    for cell in cells {
        for &kind in kinds {
            let group: Vec<&Row> = rows
                .iter()
                .filter(|r| r.kind == kind && r.cell == *cell && (r.batch > 1) == batched)
                .collect();
            if group.is_empty() {
                continue;
            }
            let spread = |f: fn(&Row) -> f64| Spread::of(group.iter().map(|r| f(r)));
            let _ = writeln!(
                out,
                "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
                size(cell.object_size),
                cell.objects_per_txn,
                cell.writers,
                kind.name(),
                group[0].commits,
                spread(|r| r.write_amplification).show(2),
                spread(|r| r.bytes_written_per_commit / 1024.0).show(1),
                spread(|r| r.fsyncs_per_commit).show(2),
                spread(|r| r.commits_per_second).show(1),
                spread(|r| r.p50_commit_ms).show(1),
                spread(|r| r.p99_commit_ms).show(1),
            );
        }
    }
    out
}

/// Return the Markdown table of the open times.
pub fn open_table(times: &[OpenTimes]) -> String {
    let mut out = String::new();
    out.push_str(
        "| Backend | Records | Store MB | Crash open, warm ms | Crash open, cold ms | Clean open, warm ms | Clean open, cold ms |\n",
    );
    out.push_str("| --- | --- | --- | --- | --- | --- | --- |\n");
    for t in times {
        let show = |values: &[f64]| Spread::of(values.iter().copied()).show(1);
        let _ = writeln!(
            out,
            "| {} | {} | {:.1} | {} | {} | {} | {} |",
            t.kind.name(),
            t.records,
            t.file_bytes as f64 / f64::from(1 << 20),
            show(&t.crash_warm_ms),
            show(&t.crash_cold_ms),
            show(&t.clean_warm_ms),
            show(&t.clean_cold_ms),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_spread_shows_the_range_only_when_it_is_wide() {
        let even = Spread::of([3.0, 1.0, 2.0, 4.0]);
        assert_eq!(even.median, 2.5);
        assert_eq!(even.show(1), "2.5 (1.0–4.0)");
        assert_eq!(Spread::of([2.001, 2.002]).show(2), "2.00");
    }
}
