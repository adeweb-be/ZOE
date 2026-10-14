//! Test of the helpers in `tests/common/mod.rs`: a store that one thread
//! closes opens again at once, while another thread runs `zoe`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::sync_channel;
use std::time::{Duration, Instant};

mod common;

/// The binary under test.
const ZOE: &str = env!("CARGO_BIN_EXE_zoe");

/// The number of rounds. In each round, one thread closes the store and
/// opens it again while the other thread runs `zoe --version`.
const ROUNDS: u32 = 200;

/// A directory in memory when the system has one. On a disk, `Store::open`
/// syncs the store directory before it takes the lock, and the sync takes
/// longer than the time in which the race can occur.
fn memory_dir(name: &str) -> PathBuf {
    let shm = Path::new("/dev/shm");
    let base = if shm.is_dir() {
        shm.to_owned()
    } else {
        std::env::temp_dir()
    };
    let dir = base.join(format!("zoe-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[test]
fn a_store_opens_again_while_another_thread_runs_zoe() {
    let dir = memory_dir("cli-helpers");
    drop(common::open(&dir));
    // A channel without a buffer: each send waits for the receive. It
    // carries the time that the last close of the store took.
    let (round_start, round_started) = sync_channel::<Duration>(0);
    std::thread::scope(|scope| {
        scope.spawn(move || {
            for (round, close) in (0..).zip(round_started) {
                // Start the child at a different time in each round, so
                // that the fork falls at each point of the close in the
                // other thread.
                let delay = close * (round % 20) / 20;
                let start = Instant::now();
                while start.elapsed() < delay {
                    std::hint::spin_loop();
                }
                let mut command = Command::new(ZOE);
                command
                    .arg("--version")
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
                assert!(common::output(&mut command).status.success());
            }
        });
        // The sender drops when this thread fails, so the other one stops.
        let round_start = round_start;
        let mut close = Duration::ZERO;
        for _ in 0..ROUNDS {
            let store = common::open(&dir);
            round_start.send(close).unwrap();
            let start = Instant::now();
            drop(store);
            close = start.elapsed();
            drop(common::open(&dir));
        }
    });
    std::fs::remove_dir_all(&dir).unwrap();
}
