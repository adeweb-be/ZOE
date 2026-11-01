//! The helpers that the integration tests of `zoe-server` share.
//!
//! - This module: the `zoe` binary and its commands, the mutex of the
//!   spawns, a new directory that the test removes at its end, and a
//!   record.
//! - `wire`: the `zoe serve` process and a raw client of the ZEO 6 msgpack
//!   protocol. This module exports its items.
//! - `mcp`: a client of `zoe mcp` over its stdio.
//!
//! # The mutex of the spawns
//!
//! The lock of a store is `flock` on a file descriptor. A spawned child
//! gets a copy of each descriptor of the test process. The lock of a store
//! that a test closed stays held while the child keeps its copy. If a test
//! opens that store again during that time, the open fails with `Locked`.
//!
//! The child keeps its copies until `exec` closes them in the child.
//! `Command::spawn` can return before that: it uses `vfork`, and the kernel
//! wakes the parent before the child releases these copies.
//!
//! So each open of a store holds a mutex. Each spawn holds the same mutex
//! until the child is past `exec`: until the child writes to its stdout
//! (`spawn_until_output`), answers a request, or ends (`output`).

// Each test binary uses a part of the helpers.
#![allow(dead_code)]

pub mod mcp;
mod wire;

// Some test binaries use no item of `wire`.
#[allow(unused_imports)]
pub use wire::*;

use std::ffi::OsStr;
use std::io::{BufRead, BufReader};
use std::mem::ManuallyDrop;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Output, Stdio};
use std::sync::{Mutex, MutexGuard, PoisonError};

use zoe_core::Store;

/// The binary under test.
pub const ZOE: &str = env!("CARGO_BIN_EXE_zoe");

static SPAWN: Mutex<()> = Mutex::new(());

/// Take the mutex of the spawns.
pub fn spawn_lock() -> MutexGuard<'static, ()> {
    // A test that fails while it holds the mutex does not stop the others.
    SPAWN.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Open the store in `dir`, or make a new one there.
pub fn open_store(dir: &Path) -> Store {
    let _spawn = spawn_lock();
    Store::open(dir).unwrap()
}

/// Run `command` to its end and return its output. The caller sets the
/// standard streams. The mutex stays held until the child ends, so the
/// child must not wait for another test.
pub fn output(command: &mut Command) -> Output {
    let _spawn = spawn_lock();
    command.spawn().unwrap().wait_with_output().unwrap()
}

/// Spawn `command`, whose stdout is a pipe, and wait for its first output
/// or its end, with the mutex of the spawns held. A child that writes has
/// finished `exec`, so it holds no descriptor of the test process. Return
/// the child and its stdout, with nothing read from it.
pub fn spawn_until_output(command: &mut Command) -> (Child, BufReader<ChildStdout>) {
    let _spawn = spawn_lock();
    let mut child = command.spawn().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    stdout.fill_buf().unwrap();
    (child, stdout)
}

/// Remove each `ZOE_*` variable of the test process from `command`.
pub fn clear_zoe_env(command: &mut Command) -> &mut Command {
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("ZOE_") {
            command.env_remove(name);
        }
    }
    command
}

/// Return a command that runs `zoe` with `args` and the variables `env`.
/// No other `ZOE_*` variable reaches it. Its stdin is empty. Its stdout
/// and stderr are pipes.
pub fn zoe_command<S: AsRef<OsStr>>(args: &[S], env: &[(&str, &str)]) -> Command {
    let mut command = Command::new(ZOE);
    clear_zoe_env(&mut command)
        .args(args)
        .envs(env.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

/// Run `zoe` with `args` and the variables `env` to its end, with the
/// streams of `zoe_command`. Return its output.
pub fn zoe<S: AsRef<OsStr>>(args: &[S], env: &[(&str, &str)]) -> Output {
    output(&mut zoe_command(args, env))
}

/// Check the exit code, and show the output if it is not the one expected.
pub fn assert_exit(output: &Output, code: i32, what: &str) {
    assert_eq!(
        output.status.code(),
        Some(code),
        "{what}\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A new empty directory of a test, in the temporary directory. The drop
/// removes it. After a failure of the test, the directory stays, so that
/// its files can be examined.
pub struct Fresh(PathBuf);

impl Fresh {
    /// Keep the directory after the drop. Return its path.
    pub fn keep(self) -> PathBuf {
        let mut fresh = ManuallyDrop::new(self);
        std::mem::take(&mut fresh.0)
    }
}

impl Deref for Fresh {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for Fresh {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl Drop for Fresh {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

/// Return a new empty directory for the test `name` of this test binary.
pub fn fresh_dir(name: &str) -> Fresh {
    let binary = env!("CARGO_CRATE_NAME");
    let pid = std::process::id();
    let dir = std::env::temp_dir().join(format!("zoe-server-{binary}-{name}-{pid}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    Fresh(dir)
}

/// Return the text of a path.
pub fn s(path: &Path) -> &str {
    path.to_str().unwrap()
}

/// A ZODB record of a `PersistentMapping` whose state holds `value` under
/// the key `v`.
pub fn record(value: &[u8]) -> Vec<u8> {
    assert!(value.len() < 256);
    let mut pickle = b"\x80\x03cpersistent.mapping\nPersistentMapping\nq\x00.".to_vec();
    pickle.extend_from_slice(b"\x80\x03}q\x00X\x01\x00\x00\x00vq\x01C");
    pickle.push(value.len() as u8);
    pickle.extend_from_slice(value);
    pickle.extend_from_slice(b"q\x02s.");
    pickle
}
