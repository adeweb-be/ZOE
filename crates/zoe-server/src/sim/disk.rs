//! The clock and the disk of the simulated server: the `Clock` and the
//! `Vfs` that the core reads.
//!
//! The clock gives the simulated time, and its waiters make a wait give
//! the baton back to the driver: the lock wait of the core, the wait of a
//! thread for its channels, and the waits of the fold of the write-ahead
//! log. The fold is a thread of the simulation (`Clock::spawn`). The disk
//! is a `MemVfs`. A sync of the database file or of a file of the log
//! gives the baton back too, so the driver can crash the server in the
//! middle of a commit or of a fold.
//!
//! The writes to the database file and to each file of the log wait in a
//! cache of the file until its next sync, as in the page cache of the
//! system. A read sees them. A sync writes them to the `MemVfs` in order,
//! then syncs the file. At a power loss, the driver lets a seeded subset
//! of them land, some of them torn, in any order. So a crash image keeps
//! what was synced, and of the rest only what a disk can keep.

use std::collections::BTreeMap;
use std::io::{self, Read};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use redb::StorageBackend;
use zoe_core::vfs::{DirLock, MemFile, MemVfs, Vfs, WriteFile};
use zoe_core::{Clock, Waiter};

use super::rng::Rng;
use super::world::{self, Resume, Shared, State, Why};

/// The name of the database file of a store.
pub(crate) const DATABASE: &str = "zoe.redb";

/// The directory of the write-ahead log of a store, with its `/`.
const LOG: &str = "wal/";

/// Tell if the writes to the file `name` wait in a cache until its sync:
/// the database file and each file of the write-ahead log.
fn cached(name: &str) -> bool {
    name == DATABASE || name.starts_with(LOG)
}

/// The wall-clock time when a run starts: 2026-01-01T00:00:00Z.
pub(crate) const START: Duration = Duration::from_secs(1_767_225_600);

/// The simulated clock.
#[derive(Clone)]
pub(crate) struct SimClock(pub(crate) Arc<Shared>);

impl Clock for SimClock {
    fn now(&self) -> SystemTime {
        UNIX_EPOCH + START + self.0.now()
    }

    /// A thread of a dead server sees the end of time, so its lock waits
    /// end at once.
    fn monotonic(&self) -> Duration {
        let state = self.0.lock();
        match world::thread_generation() {
            Some(generation) if generation != state.server.generation || world::killed() => {
                Duration::MAX
            }
            _ => state.now,
        }
    }

    fn waiter(&self) -> Arc<dyn Waiter> {
        let mut state = self.0.lock();
        Arc::new(SimWaiter::new(&mut state, &self.0))
    }

    /// A thread of the store, such as the fold, is a thread of the
    /// simulation in the server generation of the store. It runs under the
    /// baton, and a crash ends it with the other threads of the process.
    fn spawn(&self, name: &str, work: Box<dyn FnOnce() + Send>) -> io::Result<()> {
        // The driver opens the store, so the thread belongs to the server
        // that starts.
        let generation =
            world::thread_generation().unwrap_or_else(|| self.0.lock().server.generation);
        world::spawn_store_thread(&self.0, generation, name, work);
        Ok(())
    }
}

/// The clock of the check after a run: the simulated wall-clock time, and
/// the monotonic time, the waiters and the threads of the system. No
/// thread of the simulation runs during the check, so the fold of the
/// store that the check opens runs on a thread of the system, and the
/// close of that store waits for it there.
pub(crate) struct CheckClock(pub(crate) Arc<Shared>);

impl Clock for CheckClock {
    fn now(&self) -> SystemTime {
        UNIX_EPOCH + START + self.0.now()
    }
}

/// A waiter on simulated time. Its thread gives the baton back while it
/// waits.
pub(crate) struct SimWaiter {
    id: u64,
    shared: Arc<Shared>,
}

impl SimWaiter {
    pub(crate) fn new(state: &mut State, shared: &Arc<Shared>) -> Self {
        Self {
            id: state.new_waiter(),
            shared: Arc::clone(shared),
        }
    }
}

impl Waiter for SimWaiter {
    /// Give the baton back. The driver hands it back after a wake, at the
    /// deadline, or when the server of the thread dies.
    fn wait(&self, timeout: Duration) {
        if self.shared.permits.take(self.id) {
            return;
        }
        let deadline = self.shared.now().saturating_add(timeout);
        assert!(
            world::thread_generation().is_some(),
            "only a thread of the simulation waits"
        );
        world::give_back(Why::Parked {
            waiter: self.id,
            deadline,
        });
        self.shared.permits.take(self.id);
    }

    /// The wake takes only the mutex of the permits, so any thread can
    /// call it at any time.
    fn wake(&self) {
        self.shared.permits.insert(self.id);
    }
}

/// A write to a cached file that waits for the next sync of the file.
#[derive(Clone, Debug)]
enum Cached {
    Write { offset: u64, data: Vec<u8> },
    SetLen(u64),
}

/// The writes to one cached file since its last sync, in order. The
/// handles of the file share it, and the driver reads it at a crash.
#[derive(Debug, Default)]
struct Cache(Mutex<Vec<Cached>>);

impl Cache {
    fn lock(&self) -> MutexGuard<'_, Vec<Cached>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Write the cached writes to `file` in order, then sync `file`.
    fn flush(&self, file: &MemFile) -> io::Result<()> {
        let mut cached = self.lock();
        for op in cached.drain(..) {
            match op {
                Cached::Write { offset, data } => file.write(offset, &data)?,
                Cached::SetLen(len) => file.set_len(len)?,
            }
        }
        file.sync_data()
    }

    /// Apply to the file `name` of `disk` what a crash keeps of the cached
    /// writes, and empty the cache. With `power_loss`, each write lands
    /// whole, does not land, or lands torn: only its first half or only its
    /// second half. `rng` chooses. Without it, every write lands, as the
    /// page cache keeps them after a kill. A write to a file whose creation
    /// is not on disk is lost. Return how many writes landed whole or torn,
    /// and how many there were.
    fn crash(
        &self,
        disk: &MemVfs,
        name: &str,
        power_loss: bool,
        rng: &mut Rng,
    ) -> io::Result<(usize, usize)> {
        let cached = std::mem::take(&mut *self.lock());
        let total = cached.len();
        if total == 0 || !Vfs::exists(disk, name)? {
            return Ok((0, total));
        }
        let file = disk.file(name);
        let mut landed = 0;
        for op in cached {
            let draw = if power_loss { rng.below(8) } else { 0 };
            match (op, draw) {
                (_, 4 | 5) => continue,
                (Cached::SetLen(len), _) => file.set_len(len)?,
                (Cached::Write { offset, data }, 6) if data.len() > 1 => {
                    file.write(offset, &data[..data.len() / 2])?;
                }
                (Cached::Write { offset, data }, 7) if data.len() > 1 => {
                    let half = data.len() / 2;
                    file.write(offset + half as u64, &data[half..])?;
                }
                (Cached::Write { offset, data }, _) => file.write(offset, &data)?,
            }
            landed += 1;
        }
        Ok((landed, total))
    }
}

/// The caches of the cached files of one disk, by the name of the file.
/// The driver reads them at a crash.
#[derive(Debug, Default)]
pub(crate) struct Caches(Mutex<BTreeMap<String, Arc<Cache>>>);

impl Caches {
    fn lock(&self) -> MutexGuard<'_, BTreeMap<String, Arc<Cache>>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Return the cache of the file `name`. The handles of one file share
    /// it.
    fn of(&self, name: &str) -> Arc<Cache> {
        Arc::clone(self.lock().entry(name.to_owned()).or_default())
    }

    /// Give the cache of `from` to `to`: a rename keeps the cached writes
    /// with the file. The cache of a file that `to` replaces goes.
    fn rename(&self, from: &str, to: &str) {
        let mut caches = self.lock();
        let moved = caches.remove(from);
        caches.remove(to);
        if let Some(cache) = moved {
            caches.insert(to.to_owned(), cache);
        }
    }

    /// Forget the cache of the removed file `name`. A crash loses its
    /// cached writes.
    fn remove(&self, name: &str) {
        self.lock().remove(name);
    }

    /// Write the cached writes of each file of `disk` to the file in order,
    /// then sync the file: the late sync of the deliberate durability bug.
    pub(crate) fn flush(&self, disk: &MemVfs) -> io::Result<()> {
        for (name, cache) in self.lock().iter() {
            if Vfs::exists(disk, name)? {
                cache.flush(&disk.file(name))?;
            }
        }
        Ok(())
    }

    /// Apply to `disk` what a crash keeps of the cached writes of each
    /// file, in the order of the names, as `Cache::crash` does, and empty
    /// the caches. Return how many writes landed whole or torn, and how
    /// many there were.
    pub(crate) fn crash(
        &self,
        disk: &MemVfs,
        power_loss: bool,
        rng: &mut Rng,
    ) -> io::Result<(usize, usize)> {
        let mut landed = (0, 0);
        for (name, cache) in self.lock().iter() {
            let (n, total) = cache.crash(disk, name, power_loss, rng)?;
            landed = (landed.0 + n, landed.1 + total);
        }
        Ok(landed)
    }
}

/// The disk of the server: a `MemVfs` whose database file and files of
/// the write-ahead log give the baton back at each sync, and keep their
/// writes in `caches` until then.
pub(crate) struct SimVfs {
    pub(crate) disk: MemVfs,
    pub(crate) caches: Arc<Caches>,
}

/// A file of the disk of the server.
#[derive(Debug)]
pub(crate) struct SimFile {
    inner: MemFile,
    /// The cache of the file, for the database file and the files of the
    /// write-ahead log. Other files have none.
    cache: Option<Arc<Cache>>,
}

/// Return the length of a file of `len` bytes after `cached`.
fn cached_len(mut len: u64, cached: &[Cached]) -> u64 {
    for op in cached {
        len = match op {
            Cached::Write { offset, data } => len.max(offset + data.len() as u64),
            Cached::SetLen(new) => *new,
        };
    }
    len
}

impl StorageBackend for SimFile {
    fn len(&self) -> io::Result<u64> {
        let base = self.inner.len()?;
        match &self.cache {
            Some(cache) => Ok(cached_len(base, &cache.lock())),
            None => Ok(base),
        }
    }

    /// Read the bytes of the file as the cached writes leave them.
    fn read(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
        let Some(cache) = &self.cache else {
            return self.inner.read(offset, out);
        };
        let cached = cache.lock();
        if cached.is_empty() {
            return self.inner.read(offset, out);
        }
        let base = self.inner.len()?;
        let end = offset + out.len() as u64;
        if end > cached_len(base, &cached) {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "read past the end of the file",
            ));
        }
        out.fill(0);
        if offset < base {
            let n = (base.min(end) - offset) as usize;
            self.inner.read(offset, &mut out[..n])?;
        }
        for op in cached.iter() {
            match op {
                // The bytes after a cut read as zeros when the file grows
                // again.
                Cached::SetLen(len) if *len < end => {
                    let from = len.saturating_sub(offset) as usize;
                    out[from..].fill(0);
                }
                Cached::SetLen(_) => {}
                Cached::Write { offset: at, data } => {
                    let start = (*at).max(offset);
                    let stop = (at + data.len() as u64).min(end);
                    if start < stop {
                        let src = &data[(start - at) as usize..(stop - at) as usize];
                        out[(start - offset) as usize..(stop - offset) as usize]
                            .copy_from_slice(src);
                    }
                }
            }
        }
        Ok(())
    }

    fn set_len(&self, len: u64) -> io::Result<()> {
        match &self.cache {
            Some(cache) => {
                cache.lock().push(Cached::SetLen(len));
                Ok(())
            }
            None => self.inner.set_len(len),
        }
    }

    /// A sync of the database file or of a file of the log on a thread of
    /// the server is a point where the driver can crash the server. With
    /// the deliberate bug, the sync does not happen now: the driver syncs
    /// later.
    fn sync_data(&self) -> io::Result<()> {
        let Some(cache) = &self.cache else {
            return self.inner.sync_data();
        };
        if world::thread_generation().is_some() {
            let grows = matches!(cache.lock().last(), Some(Cached::SetLen(_)));
            match world::give_back(Why::Sync { grows }) {
                Resume::Go => {}
                Resume::Defer => return Ok(()),
                Resume::Killed => return Err(io::Error::other("the process was killed")),
            }
        }
        cache.flush(&self.inner)
    }

    fn write(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        match &self.cache {
            Some(cache) => {
                cache.lock().push(Cached::Write {
                    offset,
                    data: data.to_vec(),
                });
                Ok(())
            }
            None => self.inner.write(offset, data),
        }
    }
}

impl SimVfs {
    /// Return a disk on `disk` with empty caches.
    pub(crate) fn new(disk: MemVfs) -> Self {
        Self {
            disk,
            caches: Arc::default(),
        }
    }

    fn file(&self, name: &str, inner: MemFile) -> SimFile {
        SimFile {
            inner,
            cache: cached(name).then(|| self.caches.of(name)),
        }
    }
}

impl Vfs for SimVfs {
    type File = SimFile;

    fn exists(&self, name: &str) -> io::Result<bool> {
        self.disk.exists(name)
    }

    fn open(&self, name: &str) -> io::Result<SimFile> {
        Ok(self.file(name, self.disk.open(name)?))
    }

    fn create_new(&self, name: &str) -> io::Result<SimFile> {
        Ok(self.file(name, self.disk.create_new(name)?))
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        self.disk.rename(from, to)?;
        self.caches.rename(from, to);
        Ok(())
    }

    fn hard_link(&self, from: &str, to: &str) -> io::Result<()> {
        self.disk.hard_link(from, to)
    }

    fn remove(&self, name: &str) -> io::Result<()> {
        self.disk.remove(name)?;
        self.caches.remove(name);
        Ok(())
    }

    fn sync_dir(&self) -> io::Result<()> {
        self.disk.sync_dir()
    }

    fn sync_dir_at(&self, dir: &str) -> io::Result<()> {
        self.disk.sync_dir_at(dir)
    }

    fn create_dir(&self, name: &str) -> io::Result<()> {
        self.disk.create_dir(name)
    }

    fn list_files(&self, dir: &str) -> io::Result<Vec<String>> {
        self.disk.list_files(dir)
    }

    fn list_entries(&self, dir: &str) -> io::Result<Vec<String>> {
        self.disk.list_entries(dir)
    }

    fn create_file(&self, name: &str) -> io::Result<Box<dyn WriteFile>> {
        self.disk.create_file(name)
    }

    fn read_file(&self, name: &str) -> io::Result<Box<dyn Read + Send>> {
        self.disk.read_file(name)
    }

    fn sync_file(&self, name: &str) -> io::Result<()> {
        self.disk.sync_file(name)
    }

    fn set_read_only(&self, name: &str) -> io::Result<()> {
        self.disk.set_read_only(name)
    }

    fn lock_dir(&self, name: &str) -> io::Result<DirLock> {
        self.disk.lock_dir(name)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.disk.path(name)
    }
}
