//! The description of the machine that runs `zoe-bench`, and the run.
//!
//! The description holds no host name, user name or path. It describes the
//! machine of the clients. The server can run on another machine.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

/// The machine that runs the clients, as the zodbshootout results describe
/// it, without the disk: the disk of the store is on the server side.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Machine {
    /// A stable name: `ZOE_BENCH_MACHINE`, or else a slug of the CPU model.
    pub name: String,
    /// The CPU model.
    pub cpu: String,
    /// The number of CPUs that the process can use.
    pub logical_cpus: usize,
    /// The physical memory, in bytes, or 0 when Linux does not show it.
    pub memory_bytes: u64,
    /// The operating system, for example `Linux`.
    pub os: String,
    /// The kernel release.
    pub kernel: String,
    /// The architecture, for example `x86_64`.
    pub arch: String,
}

impl Machine {
    /// Describe this machine.
    pub fn describe() -> Machine {
        let cpu = read("/proc/cpuinfo")
            .as_deref()
            .and_then(cpu_model)
            .unwrap_or_else(|| std::env::consts::ARCH.to_owned());
        let name = std::env::var("ZOE_BENCH_MACHINE")
            .ok()
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| slug(&cpu));
        Machine {
            name,
            logical_cpus: std::thread::available_parallelism().map_or(1, |n| n.get()),
            memory_bytes: read("/proc/meminfo")
                .as_deref()
                .and_then(memory_bytes)
                .unwrap_or(0),
            os: read("/proc/sys/kernel/ostype").unwrap_or_else(|| std::env::consts::OS.to_owned()),
            kernel: read("/proc/sys/kernel/osrelease").unwrap_or_else(|| "unknown".to_owned()),
            arch: std::env::consts::ARCH.to_owned(),
            cpu,
        }
    }
}

/// The run: when it started and the commit of ZOE.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Run {
    /// The start of the run, in UTC, as `YYYY-MM-DDTHH:MM:SSZ`.
    pub started: String,
    /// `ZOE_BENCH_COMMIT` when it is a commit id, else `None`.
    pub commit: Option<String>,
}

impl Run {
    /// Describe a run that starts now.
    pub fn now() -> Run {
        let commit = std::env::var("ZOE_BENCH_COMMIT").ok().filter(|commit| {
            (7..=40).contains(&commit.len())
                && commit
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        });
        Run {
            started: utc(SystemTime::now()),
            commit,
        }
    }
}

/// Read a small text file, trimmed, or `None`.
fn read(path: &str) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

/// Return the CPU model of the text of `/proc/cpuinfo`.
fn cpu_model(cpuinfo: &str) -> Option<String> {
    cpuinfo.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        (key.trim() == "model name").then(|| value.trim().to_owned())
    })
}

/// Return the physical memory of the text of `/proc/meminfo`, in bytes.
fn memory_bytes(meminfo: &str) -> Option<u64> {
    meminfo.lines().find_map(|line| {
        let rest = line.strip_prefix("MemTotal:")?;
        let kib: u64 = rest.trim().strip_suffix("kB")?.trim().parse().ok()?;
        Some(kib * 1024)
    })
}

/// Return a name of lower-case letters, digits and hyphens, as the
/// zodbshootout harness makes it from the CPU model.
pub fn slug(text: &str) -> String {
    let text = text.to_lowercase().replace("(r)", "").replace("(tm)", "");
    let words: Vec<&str> = text
        .split(|c: char| !c.is_ascii_lowercase() && !c.is_ascii_digit())
        .filter(|word| !word.is_empty())
        .collect();
    if words.is_empty() {
        "unknown".to_owned()
    } else {
        words.join("-")
    }
}

/// Format `time` in UTC, as `YYYY-MM-DDTHH:MM:SSZ`.
pub fn utc(time: SystemTime) -> String {
    let seconds = time.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let (days, rest) = (seconds / 86_400, seconds % 86_400);
    let (year, month, day) = civil(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

/// Return the date of a number of days since 1970-01-01, in the proleptic
/// Gregorian calendar (the algorithm of Howard Hinnant).
fn civil(days: u64) -> (u64, u64, u64) {
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn the_cpu_model_comes_from_cpuinfo() {
        let cpuinfo = "processor\t: 0\nvendor_id\t: GenuineIntel\nmodel name\t: Intel(R) Core(TM) i7 CPU @ 2.00GHz\n";
        assert_eq!(
            cpu_model(cpuinfo).as_deref(),
            Some("Intel(R) Core(TM) i7 CPU @ 2.00GHz")
        );
        assert_eq!(cpu_model("processor\t: 0\n"), None);
    }

    #[test]
    fn the_memory_comes_from_meminfo() {
        let meminfo = "MemTotal:       16303996 kB\nMemFree:         1234 kB\n";
        assert_eq!(memory_bytes(meminfo), Some(16_303_996 * 1024));
        assert_eq!(memory_bytes("MemFree: 1 kB\n"), None);
    }

    #[test]
    fn a_slug_has_only_letters_digits_and_hyphens() {
        assert_eq!(
            slug("Intel(R) Core(TM) Ultra 7 155H"),
            "intel-core-ultra-7-155h"
        );
        assert_eq!(slug("  "), "unknown");
    }

    #[test]
    fn a_time_is_formatted_in_utc() {
        assert_eq!(utc(UNIX_EPOCH), "1970-01-01T00:00:00Z");
        let leap = UNIX_EPOCH + Duration::from_secs(951_782_400 + 3_723);
        assert_eq!(utc(leap), "2000-02-29T01:02:03Z");
        let end = UNIX_EPOCH + Duration::from_secs(4_102_444_799);
        assert_eq!(utc(end), "2099-12-31T23:59:59Z");
    }

    #[test]
    fn the_description_names_no_host_user_or_path() {
        let machine = serde_json::to_string(&Machine::describe()).unwrap();
        for name in ["HOSTNAME", "USER", "HOME", "PWD"] {
            if let Ok(value) = std::env::var(name)
                && value.len() > 2
            {
                assert!(!machine.contains(&value), "the description names {name}");
            }
        }
    }
}
