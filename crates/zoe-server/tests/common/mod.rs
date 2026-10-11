//! Spawns of `zoe` and opens of a store that do not race each other.
//!
//! The lock of a store is `flock` on a file descriptor. A spawned child
//! gets a copy of each descriptor of the test process. The lock of a store
//! that a test closed stays held while the child keeps its copy. If a test
//! opens that store again during that time, the open fails with `Locked`.
//!
//! The child keeps its copies until `exec` returns in the child.
//! `Command::spawn` can return before that: it uses `vfork`, and the kernel
//! wakes the parent before it releases these copies.
//!
//! So each open of a store holds a mutex, and each child holds the same
//! mutex from its spawn to its end. When `wait` returns, the child holds no
//! descriptor.

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
    let _spawn = spawn_lock();
    command.spawn().unwrap().wait_with_output().unwrap()
}
