//! Spawns of `zoe` and opens of a store that do not race each other.
//!
//! The lock of a store is `flock` on a file descriptor. A spawned child
//! gets a copy of each descriptor of the test process, and keeps it until
//! it runs `exec`. During that time, the lock of a store that a test closed
//! stays held. If another test opens that store again during that time,
//! the open fails with `Locked`.
//!
//! So each spawn and each open of a store in the test process holds the
//! same mutex. A spawn returns after the child runs `exec`, so an open
//! that comes after it finds no copy of an old descriptor.

use std::path::Path;
use std::process::{Command, Output};
use std::sync::{Mutex, MutexGuard, PoisonError};

use zoe_core::Store;

static SPAWN: Mutex<()> = Mutex::new(());

fn spawn_lock() -> MutexGuard<'static, ()> {
    // A test that fails while it holds the mutex does not stop the others.
    SPAWN.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Open the store in `dir`, or make a new one there.
pub fn open(dir: &Path) -> Store {
    let _spawn = spawn_lock();
    Store::open(dir).unwrap()
}

/// Run `command` to its end and return its output. The caller sets the
/// standard streams.
pub fn output(command: &mut Command) -> Output {
    let child = {
        let _spawn = spawn_lock();
        command.spawn().unwrap()
    };
    child.wait_with_output().unwrap()
}
