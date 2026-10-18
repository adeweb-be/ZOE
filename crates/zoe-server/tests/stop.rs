//! Tests of the stop of `zoe serve` beside its acceptance test,
//! `tests/shutdown.rs`: SIGINT does what SIGTERM does, and the twin of
//! `--shutdown-timeout` bounds a stop that a client holds.

use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::process::{Child, ChildStdout, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

mod wire;

use wire::{Conn, MAX, Server, Value, begin, commit, fresh_dir, new_oids, p64, record, s};

/// Start `zoe serve` with `args` on a free port of 127.0.0.1. No `ZOE_*`
/// variable reaches it. Return the process, its address, and its stdout,
/// which must stay open while it runs.
fn spawn(args: &[&str]) -> (Child, SocketAddr, BufReader<ChildStdout>) {
    let mut command = Command::new(wire::ZOE);
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("ZOE_") {
            command.env_remove(name);
        }
    }
    command
        .arg("serve")
        .args(args)
        .args(["--listen", "127.0.0.1:0"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    let (child, mut stdout) = wire::spawn_until_output(&mut command);
    loop {
        let mut line = String::new();
        assert_ne!(stdout.read_line(&mut line).unwrap(), 0, "zoe serve stopped");
        if let Some(addr) = line.trim_end().strip_prefix("listening ") {
            return (child, addr.parse().unwrap(), stdout);
        }
    }
}

/// Wait at most `timeout` for the end of `child`. Return its exit status.
fn wait(child: &mut Child, timeout: Duration) -> ExitStatus {
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if started.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            panic!("zoe serve did not stop within {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn sigint_stops_the_server_as_sigterm_does() {
    let dir = fresh_dir("stop-sigint");
    let path = dir.join("store");
    let (mut child, addr, _stdout) = spawn(&["--path", s(&path)]);
    // A transaction that only staged a record.
    let mut staged = Conn::open(addr);
    staged.register("1", false).unwrap();
    let oid = new_oids(&mut staged, 1)[0];
    begin(&mut staged, 1);
    let store = vec![
        p64(oid),
        p64(0),
        Value::Binary(record(b"staged")),
        Value::from(1),
    ];
    staged.send_async("storea", store);
    staged.call("ping", vec![]).unwrap();

    let started = Instant::now();
    let sent = {
        let _spawn = wire::spawn_lock();
        Command::new("kill")
            .args(["-INT", &child.id().to_string()])
            .status()
            .unwrap()
    };
    assert!(sent.success());
    let status = wait(&mut child, Duration::from_secs(20));
    let took = started.elapsed();
    assert_eq!(status.code(), Some(0));
    assert!(took < Duration::from_secs(5), "the stop took {took:?}");
    assert!(staged.closed_within(Duration::from_secs(5)));
    // The store closed cleanly, and the staged record is not in it.
    let _spawn = wire::spawn_lock();
    let store = zoe_core::Store::open_read_only(&path).unwrap();
    assert!(store.load(zoe_core::Oid(oid)).is_err());
}

#[test]
fn the_twin_bounds_a_stop_that_a_client_that_does_not_read_holds() {
    let dir = fresh_dir("stop-not-reading");
    let path = dir.join("store");
    let mut server = Server::start(&["--path", s(&path)], &[("ZOE_SHUTDOWN_TIMEOUT", "2")]);
    let mut writer = server.client("1");
    let mut slow = server.client("1");
    let big = new_oids(&mut writer, 1)[0];
    let large: Vec<u8> = (0..2_000_000u32).map(|n| n as u8).collect();
    commit(&mut writer, 1, &[(big, 0, large)]).unwrap();
    // The slow client asks for 40 MB and reads nothing. So the server
    // cannot send its replies, and its session waits.
    for _ in 0..20 {
        slow.send_call("loadBefore", vec![p64(big), Value::Binary(MAX.to_vec())]);
    }
    std::thread::sleep(Duration::from_millis(300));

    let (status, took) = server.terminate(Duration::from_secs(40));
    assert_eq!(status.code(), Some(0));
    assert!(took < Duration::from_secs(3), "the stop took {took:?}");
    // The close of the slow connection ended its session before the
    // stores closed. So the store closed cleanly.
    let _spawn = wire::spawn_lock();
    zoe_core::Store::open_read_only(&path).unwrap();
}
