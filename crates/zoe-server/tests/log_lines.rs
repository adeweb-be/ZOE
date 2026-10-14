//! The text logs of `zoe serve`: a text that a client sends cannot forge a
//! log line.
//!
//! A method name and a client label come from the client. The server puts
//! them only into fields, which the text format quotes and escapes, so a
//! line feed in them stays inside its log line.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::process::{Command, Stdio};
use std::time::Duration;

mod wire;

use wire::{Conn, Value, ZOE, fresh_dir, s, spawn_lock};

#[test]
fn a_line_feed_from_the_client_forges_no_text_log_line() {
    let dir = fresh_dir("log-lines");
    let log = dir.join("stderr.log");
    let mut child = {
        let _spawn = spawn_lock();
        Command::new(ZOE)
            .args([
                "serve",
                "--path",
                s(&dir.join("store")),
                "--listen",
                "127.0.0.1:0",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(File::create(&log).unwrap())
            .spawn()
            .unwrap()
    };
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let addr: SocketAddr = loop {
        let mut line = String::new();
        assert!(
            stdout.read_line(&mut line).unwrap() > 0,
            "the server stopped"
        );
        if let Some(addr) = line.trim().strip_prefix("listening ") {
            break addr.parse().unwrap();
        }
    };

    // A call before `register` closes the connection, and the log line
    // names the method.
    let mut first = Conn::open(addr);
    first.send_async("evil\nFORGED before register", vec![]);
    assert!(first.closed_within(Duration::from_secs(10)));
    // An unknown method of a registered client with a label.
    let mut second = Conn::open(addr);
    second.register("1", false).unwrap();
    second
        .call(
            "set_client_label",
            vec![Value::from("worker\nFORGED label")],
        )
        .unwrap();
    second.send_async("evil\nFORGED unknown", vec![]);
    assert!(second.closed_within(Duration::from_secs(10)));
    child.kill().unwrap();
    child.wait().unwrap();

    let text = std::fs::read_to_string(&log).unwrap();
    // The default format is text: each line starts with its time.
    for line in text.lines() {
        assert!(
            line.starts_with(|c: char| c.is_ascii_digit()),
            "a forged log line: {line:?} in:\n{text}"
        );
    }
    assert!(text.contains(r"evil\nFORGED before register"), "{text}");
    assert!(text.contains(r"evil\nFORGED unknown"), "{text}");
    assert!(text.contains("worker FORGED label"), "{text}");
}
