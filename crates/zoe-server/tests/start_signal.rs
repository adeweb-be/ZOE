//! Acceptance test of a SIGTERM during the start of `zoe serve` (N10).
//!
//! A SIGTERM can come at any time of the start: before the store exists,
//! while the start creates it, and before the server listens. A process
//! that the signal kills before it can handle the signal has created
//! nothing. A process that handles it stops with 0. In both cases, the
//! store directory is absent, or holds a store that opens, closed
//! cleanly. A SIGTERM never leaves a directory without a store.
//!
//! The test starts `zoe serve --path NEW` again and again, and sends
//! SIGTERM after a delay that grows from 0 ms, so that the signal falls
//! in each step of the start.

use std::os::unix::process::ExitStatusExt;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

mod wire;

use wire::{ZOE, fresh_dir, s, spawn_lock};

/// The number of the signal SIGTERM on Linux.
const SIGTERM: i32 = 15;

/// Wait at most `timeout` for the end of `child`. Return its status.
fn wait(child: &mut std::process::Child, timeout: Duration) -> ExitStatus {
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if started.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            panic!("zoe serve did not stop within {timeout:?} after SIGTERM");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn a_sigterm_during_the_start_leaves_a_store_or_nothing() {
    let dir = fresh_dir("start-signal");
    for step in 0..60u64 {
        let path = dir.join(format!("store-{step}"));
        let mut command = Command::new(ZOE);
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("ZOE_") {
                command.env_remove(name);
            }
        }
        command
            .args(["serve", "--path", s(&path), "--listen", "127.0.0.1:0"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = {
            let _spawn = spawn_lock();
            command.spawn().unwrap()
        };
        // stdout stays open until the end of the process.
        let stdout = child.stdout.take().unwrap();
        std::thread::sleep(Duration::from_millis(step));
        let sent = Command::new("kill")
            .args(["-TERM", &child.id().to_string()])
            .status()
            .unwrap();
        assert!(sent.success());
        let status = wait(&mut child, Duration::from_secs(30));
        drop(stdout);

        if status.signal() == Some(SIGTERM) {
            // The signal killed the process before it could handle it.
            // So the process must not have created anything yet.
            assert!(
                !path.exists(),
                "step {step}: the process died of SIGTERM and left {}",
                path.display()
            );
            continue;
        }
        assert_eq!(status.code(), Some(0), "step {step}: {status}");
        if path.exists() {
            let _spawn = spawn_lock();
            let opened = zoe_core::Store::open_read_only(&path);
            assert!(
                opened.is_ok(),
                "step {step}: {} holds no store that opens: {:?}",
                path.display(),
                opened.err()
            );
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}
