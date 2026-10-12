//! The clock and the disk of the simulated server: the `Clock` and the
//! `Vfs` that the core reads.
//!
//! The clock gives the simulated time, and its waiters make a lock wait
//! give the baton back to the driver. The disk is a `MemVfs`. A sync of
//! the database file gives the baton back too, so the driver can crash
//! the server in the middle of a commit.

use std::io::{self, Read};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use redb::StorageBackend;
use zoe_core::vfs::{DirLock, MemFile, MemVfs, Vfs, WriteFile};
use zoe_core::{Clock, Waiter};

use super::world::{self, Resume, Shared, Why};

/// The name of the database file of a store.
const DATABASE: &str = "zoe.redb";

/// The wall-clock time when a run starts: 2026-01-01T00:00:00Z.
const START: Duration = Duration::from_secs(1_767_225_600);

/// The simulated clock.
#[derive(Clone)]
pub(crate) struct SimClock(pub(crate) Arc<Shared>);

impl Clock for SimClock {
    fn now(&self) -> SystemTime {
        UNIX_EPOCH + START + self.0.now()
    }

    /// A session thread of a dead server sees the end of time, so its
    /// lock waits end at once.
    fn monotonic(&self) -> Duration {
        let state = self.0.lock();
        match world::session_generation() {
            Some(generation) if generation != state.server.generation => Duration::MAX,
            _ => state.now,
        }
    }

    fn waiter(&self) -> Arc<dyn Waiter> {
        let id = self.0.lock().new_waiter();
        Arc::new(SimWaiter {
            id,
            shared: Arc::clone(&self.0),
        })
    }
}

/// The waiter of a lock request on simulated time.
struct SimWaiter {
    id: u64,
    shared: Arc<Shared>,
}

impl Waiter for SimWaiter {
    fn wait(&self, timeout: Duration) {
        let deadline = {
            let mut state = self.shared.lock();
            if state.permits.remove(&self.id) {
                return;
            }
            state.now.saturating_add(timeout)
        };
        assert!(
            world::session_generation().is_some(),
            "only a session thread waits for a lock"
        );
        world::give_back(Why::Parked {
            waiter: self.id,
            deadline,
        });
        self.shared.lock().permits.remove(&self.id);
    }

    fn wake(&self) {
        self.shared.lock().permits.insert(self.id);
    }
}

/// The disk of the server: a `MemVfs` whose database file gives the
/// baton back at each sync.
pub(crate) struct SimVfs(pub(crate) MemVfs);

/// A file of the disk of the server.
#[derive(Debug)]
pub(crate) struct SimFile {
    inner: MemFile,
    database: bool,
}

impl StorageBackend for SimFile {
    fn len(&self) -> io::Result<u64> {
        self.inner.len()
    }

    fn read(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
        self.inner.read(offset, out)
    }

    fn set_len(&self, len: u64) -> io::Result<()> {
        self.inner.set_len(len)
    }

    /// A sync of the database file on a session thread is a point where
    /// the driver can crash the server. With the deliberate bug, the sync
    /// does not happen now: the driver syncs later.
    fn sync_data(&self) -> io::Result<()> {
        if self.database && world::session_generation().is_some() {
            match world::give_back(Why::Sync) {
                Resume::Go => {}
                Resume::Defer => return Ok(()),
                Resume::Killed => return Err(io::Error::other("the process was killed")),
            }
        }
        self.inner.sync_data()
    }

    fn write(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        self.inner.write(offset, data)
    }
}

impl SimVfs {
    fn file(&self, name: &str, inner: MemFile) -> SimFile {
        SimFile {
            inner,
            database: name == DATABASE,
        }
    }
}

impl Vfs for SimVfs {
    type File = SimFile;

    fn exists(&self, name: &str) -> io::Result<bool> {
        self.0.exists(name)
    }

    fn open(&self, name: &str) -> io::Result<SimFile> {
        Ok(self.file(name, self.0.open(name)?))
    }

    fn create_new(&self, name: &str) -> io::Result<SimFile> {
        Ok(self.file(name, self.0.create_new(name)?))
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        self.0.rename(from, to)
    }

    fn remove(&self, name: &str) -> io::Result<()> {
        self.0.remove(name)
    }

    fn sync_dir(&self) -> io::Result<()> {
        self.0.sync_dir()
    }

    fn sync_dir_at(&self, dir: &str) -> io::Result<()> {
        self.0.sync_dir_at(dir)
    }

    fn create_dir(&self, name: &str) -> io::Result<()> {
        self.0.create_dir(name)
    }

    fn list_files(&self, dir: &str) -> io::Result<Vec<String>> {
        self.0.list_files(dir)
    }

    fn list_entries(&self, dir: &str) -> io::Result<Vec<String>> {
        self.0.list_entries(dir)
    }

    fn create_file(&self, name: &str) -> io::Result<Box<dyn WriteFile>> {
        self.0.create_file(name)
    }

    fn read_file(&self, name: &str) -> io::Result<Box<dyn Read + Send>> {
        self.0.read_file(name)
    }

    fn sync_file(&self, name: &str) -> io::Result<()> {
        self.0.sync_file(name)
    }

    fn set_read_only(&self, name: &str) -> io::Result<()> {
        self.0.set_read_only(name)
    }

    fn lock_dir(&self, name: &str) -> io::Result<DirLock> {
        self.0.lock_dir(name)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.path(name)
    }
}

/// Sync the database file of `vfs` now: the late sync of the deliberate
/// bug.
pub(crate) fn sync_database(vfs: &MemVfs) {
    let _ = vfs.file(DATABASE).sync_data();
}
