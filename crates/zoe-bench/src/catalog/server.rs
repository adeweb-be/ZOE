//! The `zoe serve` of a run of `zoe-bench catalog`: its start and its stop,
//! its memory, its `/metrics`, the drop of the pages of a store from the
//! page cache, and the mode of the text indexes of the store.

use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use zoe_core::{Flavour, Oid, Store};

/// The durable namespace of the settings of the text indexes.
pub const TEXT_SETTINGS: &str = "zoe.text";

/// The longest wait for `zoe serve` to listen, and to stop.
const START_TIMEOUT: Duration = Duration::from_secs(300);
const STOP_TIMEOUT: Duration = Duration::from_secs(120);

/// A `zoe serve` that the run started.
pub struct Server {
    child: Child,
    /// The ZEO address.
    pub addr: SocketAddr,
    /// The admin address, for `/metrics`.
    pub admin: SocketAddr,
    /// The time of the spawn: the open of the store starts then.
    pub started: Instant,
    /// The time from the spawn to the line `listening`.
    pub listening: Duration,
}

/// The settings of a start of `zoe serve`.
#[derive(Clone, Debug)]
pub struct Launch {
    /// The `zoe` binary.
    pub binary: PathBuf,
    /// The store directory.
    pub store: PathBuf,
    /// `--catalog-memory`, or the default of the server.
    pub catalog_memory: Option<String>,
    /// `--catalog-rebase-rows`, or the default of the server.
    pub rebase_rows: Option<u64>,
}

impl Server {
    /// Start `zoe serve` on the store of `launch`, on free ports of
    /// 127.0.0.1. No `ZOE_*` variable of this process reaches it. Its
    /// stderr goes to the file `<store>.serve.log`.
    pub fn start(launch: &Launch) -> Result<Server, String> {
        let log = log_path(&launch.store);
        let stderr = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)
            .map_err(|e| format!("{}: {e}", log.display()))?;
        let mut command = Command::new(&launch.binary);
        command.arg("serve").arg("--path").arg(&launch.store).args([
            "--listen",
            "127.0.0.1:0",
            "--admin-listen",
            "127.0.0.1:0",
        ]);
        if let Some(memory) = &launch.catalog_memory {
            command.args(["--catalog-memory", memory]);
        }
        if let Some(rows) = launch.rebase_rows {
            command.arg("--catalog-rebase-rows").arg(rows.to_string());
        }
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("ZOE_") {
                command.env_remove(key);
            }
        }
        let started = Instant::now();
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(stderr)
            .spawn()
            .map_err(|e| format!("{}: {e}", launch.binary.display()))?;
        let stdout = child.stdout.take().expect("a piped stdout");
        let (lines, received) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if lines.send(line).is_err() {
                    break;
                }
            }
        });
        let mut admin = None;
        loop {
            let wait = START_TIMEOUT.saturating_sub(started.elapsed());
            match received.recv_timeout(wait) {
                Ok(line) => {
                    if let Some(rest) = line.strip_prefix("admin ") {
                        admin = rest.trim().parse().ok();
                    } else if let Some(rest) = line.strip_prefix("listening ") {
                        let addr = rest.trim().parse().map_err(|e| format!("{rest}: {e}"))?;
                        let Some(admin) = admin else {
                            let _ = child.kill();
                            return Err("zoe serve gave no admin address".into());
                        };
                        return Ok(Server {
                            child,
                            addr,
                            admin,
                            started,
                            listening: started.elapsed(),
                        });
                    }
                }
                Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let tail = std::fs::read_to_string(&log).unwrap_or_default();
                    let tail: Vec<&str> = tail.lines().rev().take(20).collect();
                    return Err(format!(
                        "zoe serve did not listen:\n{}",
                        tail.into_iter().rev().collect::<Vec<_>>().join("\n")
                    ));
                }
            }
        }
    }

    /// The memory of the server process now.
    pub fn memory(&self) -> Memory {
        Memory::of(self.child.id())
    }

    /// Stop the server: SIGTERM, then SIGKILL after a wait.
    pub fn stop(mut self) {
        let term = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status();
        if term.is_ok_and(|s| s.success()) {
            let deadline = Instant::now() + STOP_TIMEOUT;
            while Instant::now() < deadline {
                if let Ok(Some(_)) = self.child.try_wait() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The log file of the servers of a store.
pub fn log_path(store: &Path) -> PathBuf {
    let mut name = store.file_name().unwrap_or_default().to_os_string();
    name.push(".serve.log");
    store.with_file_name(name)
}

/// The resident memory of a process, from `/proc` on Linux.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct Memory {
    /// The resident set now, in bytes.
    pub rss_bytes: Option<u64>,
    /// The peak of the resident set since the start, in bytes.
    pub peak_rss_bytes: Option<u64>,
}

impl Memory {
    /// The memory of the process `pid`. Each number is `None` where `/proc`
    /// does not give it.
    pub fn of(pid: u32) -> Memory {
        let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) else {
            return Memory::default();
        };
        let field = |name: &str| {
            status.lines().find_map(|line| {
                let rest = line.strip_prefix(name)?;
                let kib: u64 = rest.trim().strip_suffix("kB")?.trim().parse().ok()?;
                Some(kib * 1024)
            })
        };
        Memory {
            rss_bytes: field("VmRSS:"),
            peak_rss_bytes: field("VmHWM:"),
        }
    }
}

/// The text of `GET /metrics` of `admin`.
pub async fn scrape(admin: SocketAddr) -> std::io::Result<String> {
    let mut stream = tokio::net::TcpStream::connect(admin).await?;
    let request = format!("GET /metrics HTTP/1.0\r\nHost: {admin}\r\n\r\n");
    stream.write_all(request.as_bytes()).await?;
    let mut text = String::new();
    stream.read_to_string(&mut text).await?;
    Ok(text)
}

/// The value of the sample `name` of `text` whose labels hold each of
/// `labels`, as `key="value"`.
pub fn sample(text: &str, name: &str, labels: &[&str]) -> Option<f64> {
    text.lines().find_map(|line| {
        let rest = line.strip_prefix(name)?;
        let (head, value) = rest.rsplit_once(' ')?;
        if !(head.is_empty() || head.starts_with('{')) {
            return None;
        }
        if !labels.iter().all(|label| head.contains(label)) {
            return None;
        }
        value.trim().parse().ok()
    })
}

/// The label of a catalog in the gauges of the mirror.
pub fn catalog_label(catalog: Oid) -> String {
    format!("catalog=\"{:016x}\"", catalog.0)
}

/// Drop the pages of every file under `dir` from the page cache, with
/// `posix_fadvise(POSIX_FADV_DONTNEED)`. Return the bytes of the files, or
/// `None` where the system has no such call.
pub fn drop_page_cache(dir: &Path) -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let mut bytes = 0;
        let mut stack = vec![dir.to_path_buf()];
        while let Some(path) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&path) else {
                continue;
            };
            for entry in entries.flatten() {
                let Ok(kind) = entry.file_type() else {
                    continue;
                };
                if kind.is_dir() {
                    stack.push(entry.path());
                } else if kind.is_file()
                    && let Ok(file) = std::fs::File::open(entry.path())
                {
                    bytes += file.metadata().map_or(0, |m| m.len());
                    dont_need(&file);
                }
            }
        }
        Some(bytes)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = dir;
        None
    }
}

/// Drop the clean pages of `file` from the page cache.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn dont_need(file: &std::fs::File) {
    use std::os::fd::AsRawFd;
    // SAFETY: posix_fadvise(2) reads only its arguments; the descriptor is
    // open for the life of `file`.
    unsafe {
        libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED);
    }
}

/// The directory of the text index of `catalog` in `store`.
pub fn text_dir(store: &Path, catalog: Oid) -> PathBuf {
    store.join("text").join(format!("{:016x}", catalog.0))
}

/// The directory that keeps the text index of `catalog` in `mode` while
/// the store runs in the other mode: beside the store, not in it.
fn kept_dir(store: &Path, catalog: Oid, mode: &str) -> PathBuf {
    let mut name = store.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".text-{mode}"));
    store
        .with_file_name(name)
        .join(format!("{:016x}", catalog.0))
}

/// The mode of the settings `value` of `zoe.text`: the mode of its first
/// index.
pub fn mode_of(value: &[u8]) -> Option<String> {
    let json: serde_json::Value = serde_json::from_slice(value).ok()?;
    let indexes = json.get("indexes")?.as_object()?;
    let first = indexes.values().next()?;
    first.get("mode")?.as_str().map(str::to_owned)
}

/// Give the text indexes of `catalog` in the store `store` the settings
/// `value` of the mode `mode`, with no server on the store. The text index
/// of the mode before moves beside the store, and a kept index of `mode`
/// moves into `text/`, so the open finds the index of its settings and
/// does not rebuild it. Return true when the settings changed.
pub fn set_mode(store: &Path, catalog: Oid, mode: &str, value: &[u8]) -> Result<bool, String> {
    let fail = |e: &dyn std::fmt::Display| format!("{}: {e}", store.display());
    let opened = Store::open(store).map_err(|e| fail(&e))?;
    let key = catalog.0.to_be_bytes();
    let current = opened
        .kv_get(Flavour::Durable, TEXT_SETTINGS, &key)
        .map_err(|e| fail(&e))?;
    if current.as_deref() == Some(value) {
        return Ok(false);
    }
    let dir = text_dir(store, catalog);
    if let Some(before) = current.as_deref().and_then(mode_of)
        && dir.is_dir()
    {
        let keep = kept_dir(store, catalog, &before);
        let _ = std::fs::remove_dir_all(&keep);
        std::fs::create_dir_all(keep.parent().expect("a parent")).map_err(|e| fail(&e))?;
        std::fs::rename(&dir, &keep).map_err(|e| fail(&e))?;
    }
    let kept = kept_dir(store, catalog, mode);
    if kept.is_dir() {
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.parent().expect("a parent")).map_err(|e| fail(&e))?;
        std::fs::rename(&kept, &dir).map_err(|e| fail(&e))?;
    }
    let mut txn = opened
        .tpc_begin(b"zoe-bench", b"text mode", b"")
        .map_err(|e| fail(&e))?;
    txn.kv_set(Flavour::Durable, TEXT_SETTINGS, &key, value, None)
        .map_err(|e| fail(&e))?;
    txn.vote().map_err(|e| fail(&e))?;
    txn.finish().map_err(|e| fail(&e))?;
    Ok(true)
}

/// The mode of the text indexes of `catalog` in the store, with no server
/// on the store.
pub fn current_mode(store: &Path, catalog: Oid) -> Result<Option<String>, String> {
    let opened = Store::open(store).map_err(|e| format!("{}: {e}", store.display()))?;
    let value = opened
        .kv_get(Flavour::Durable, TEXT_SETTINGS, &catalog.0.to_be_bytes())
        .map_err(|e| format!("{}: {e}", store.display()))?;
    Ok(value.as_deref().and_then(mode_of))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sample_matches_its_name_and_labels() {
        let text = "# HELP x\n\
            zoe_catalog_mirror_ready{storage=\"1\",catalog=\"0000000000000007\"} 1\n\
            zoe_catalog_mirror_ready_other{storage=\"1\"} 5\n\
            zoe_catalog_mirror_bytes{storage=\"1\",catalog=\"0000000000000007\"} 1234\n\
            plain 3\n";
        let label = catalog_label(Oid(7));
        assert_eq!(
            sample(text, "zoe_catalog_mirror_ready", &[&label]),
            Some(1.0)
        );
        assert_eq!(
            sample(text, "zoe_catalog_mirror_bytes", &[&label]),
            Some(1234.0)
        );
        assert_eq!(sample(text, "zoe_catalog_mirror_ready", &["x=\"y\""]), None);
        assert_eq!(sample(text, "plain", &[]), Some(3.0));
    }

    #[test]
    fn the_mode_is_the_mode_of_the_first_index() {
        let value = br#"{"indexes": {"SearchableText": {"mode": "compat"}}}"#;
        assert_eq!(mode_of(value).as_deref(), Some("compat"));
        assert_eq!(mode_of(b"{}"), None);
    }
}
