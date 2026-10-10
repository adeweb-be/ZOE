//! The time to open a store after a crash.
//!
//! A writer process commits to a store on a real directory, with real
//! fsyncs, until the store holds the wanted number of records. It then
//! goes on committing, and the parent kills it with SIGKILL, most often
//! in the middle of a commit. The directory that the killed process
//! leaves is the crash image.
//!
//! Each measurement opens a fresh, synced copy of the image. A cold open
//! first drops the files from the page cache. A warm open finds them in
//! the page cache. After the crash open, the store closes cleanly,
//! and two more opens give the time of a normal restart.

use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::Serialize;
use zoe_core::vfs::OsVfs;
use zoe_core::{Oid, Result, Tid, ZoeError};

use super::{BackendKind, commit, payload};

/// The line that the writer prints when the store holds enough records.
const READY: &str = "ready";

/// The open times of one store.
#[derive(Clone, Debug, Serialize)]
pub struct OpenTimes {
    /// The backend kind.
    pub kind: BackendKind,
    /// The records that the store held when the writer said it was ready.
    pub records: u64,
    /// The size of the store files in the crash image, in bytes.
    pub file_bytes: u64,
    /// Opens of the crash image with the files in the page cache, in
    /// milliseconds.
    pub crash_warm_ms: Vec<f64>,
    /// Opens of the crash image with the files out of the page cache.
    pub crash_cold_ms: Vec<f64>,
    /// Opens after a clean close, with the files in the page cache.
    pub clean_warm_ms: Vec<f64>,
    /// Opens after a clean close, with the files out of the page cache.
    pub clean_cold_ms: Vec<f64>,
    /// False when the page cache kept a part of a file that a cold open
    /// needed out of it.
    pub evicted: bool,
}

/// Commit transactions of `objects_per_txn` new objects to the store of
/// `kind` in `dir`, forever. Print `ready <tid>` when the store holds
/// `records` records. The parent kills this process.
pub fn write_until_killed(
    kind: BackendKind,
    dir: &Path,
    records: u64,
    object_size: usize,
    objects_per_txn: usize,
) -> Result<()> {
    let store = kind.open(OsVfs::new(dir)?)?;
    let payload = payload(object_size);
    let mut written = 0;
    let mut announced = false;
    loop {
        let set = (0..objects_per_txn)
            .map(|_| store.new_oid().map(|oid| (oid, Tid::ZERO)))
            .collect::<Result<Vec<(Oid, Tid)>>>()?;
        let tid = commit(&store, &set, &payload)?;
        written += objects_per_txn as u64;
        if written >= records && !announced {
            let mut out = io::stdout().lock();
            writeln!(out, "{READY} {} {written}", tid.0)?;
            out.flush()?;
            announced = true;
        }
    }
}

/// Run `writer`, wait until it is ready, and kill it a few milliseconds
/// later. Return the last tid and the record count that it announced.
pub fn kill_when_ready(mut writer: Command, delay: Duration) -> io::Result<(Tid, u64)> {
    let mut child = writer.stdout(Stdio::piped()).spawn()?;
    let stdout = BufReader::new(child.stdout.take().expect("stdout is piped"));
    let mut announced = None;
    for line in stdout.lines() {
        let line = line?;
        let mut words = line.split_whitespace();
        if words.next() == Some(READY) {
            let tid = words.next().and_then(|w| w.parse().ok());
            let records = words.next().and_then(|w| w.parse().ok());
            announced = tid.zip(records);
            break;
        }
    }
    std::thread::sleep(delay);
    child.kill()?;
    child.wait()?;
    let (tid, records) =
        announced.ok_or_else(|| io::Error::other("the writer stopped before it was ready"))?;
    Ok((Tid(tid), records))
}

/// Time the opens of the crash image in `image`, `repeats` times each.
/// `work` is a scratch directory. `acknowledged` is a tid that the
/// writer announced, so every open must find it.
pub fn time_opens(
    kind: BackendKind,
    image: &Path,
    work: &Path,
    acknowledged: Tid,
    records: u64,
    repeats: usize,
) -> Result<OpenTimes> {
    let mut times = OpenTimes {
        kind,
        records,
        file_bytes: store_files(image)?.iter().map(|(_, len)| len).sum(),
        crash_warm_ms: Vec::new(),
        crash_cold_ms: Vec::new(),
        clean_warm_ms: Vec::new(),
        clean_cold_ms: Vec::new(),
        evicted: true,
    };
    for _ in 0..repeats {
        copy_image(image, work)?;
        times.evicted &= evict(work)?;
        times
            .crash_cold_ms
            .push(timed_open(kind, work, acknowledged)?);
        times
            .clean_warm_ms
            .push(timed_open(kind, work, acknowledged)?);
        times.evicted &= evict(work)?;
        times
            .clean_cold_ms
            .push(timed_open(kind, work, acknowledged)?);

        copy_image(image, work)?;
        times
            .crash_warm_ms
            .push(timed_open(kind, work, acknowledged)?);
    }
    fs::remove_dir_all(work)?;
    Ok(times)
}

/// Open the store in `dir`, check that it holds `acknowledged`, close it,
/// and return the time of the open in milliseconds.
fn timed_open(kind: BackendKind, dir: &Path, acknowledged: Tid) -> Result<f64> {
    let begun = Instant::now();
    let store = kind.open(OsVfs::new(dir)?)?;
    let elapsed = begun.elapsed();
    if store.last_transaction() < acknowledged {
        return Err(ZoeError::Backend(
            format!("{} lost the acknowledged tid {acknowledged:?}", kind.name()).into(),
        ));
    }
    drop(store);
    Ok(elapsed.as_secs_f64() * 1e3)
}

/// Return the name and the length of each store file in `dir`: every
/// file but the lock.
fn store_files(dir: &Path) -> io::Result<Vec<(String, u64)>> {
    let mut files = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name != "lock" && entry.file_type()?.is_file() {
            files.push((name, entry.metadata()?.len()));
        }
    }
    Ok(files)
}

/// Replace `work` with a copy of the store files of `image`, and sync the
/// copy. So an fsync in the open does not write the copy back.
fn copy_image(image: &Path, work: &Path) -> io::Result<()> {
    if work.exists() {
        fs::remove_dir_all(work)?;
    }
    fs::create_dir_all(work)?;
    for (name, _) in store_files(image)? {
        fs::copy(image.join(&name), work.join(&name))?;
        fs::File::open(work.join(&name))?.sync_all()?;
    }
    fs::File::open(work)?.sync_all()
}

/// Sync the store files of `dir` and drop them from the page cache. Return
/// false when `fincore` shows that a part of a file stays in the cache.
fn evict(dir: &Path) -> io::Result<bool> {
    let mut evicted = true;
    for (name, _) in store_files(dir)? {
        let path = dir.join(&name);
        fs::File::open(&path)?.sync_all()?;
        // `dd` with `iflag=nocache` and no block asks the kernel to drop
        // the whole file from the page cache.
        let status = Command::new("dd")
            .arg(format!("if={}", path.display()))
            .args(["iflag=nocache", "count=0", "status=none"])
            .status()?;
        if !status.success() {
            return Err(io::Error::other("dd could not drop the page cache"));
        }
        let resident = Command::new("fincore")
            .args(["--bytes", "--noheadings", "--output", "RES"])
            .arg(&path)
            .output()?;
        let bytes: u64 = String::from_utf8_lossy(&resident.stdout)
            .trim()
            .parse()
            .unwrap_or(u64::MAX);
        evicted &= bytes == 0;
    }
    Ok(evicted)
}
