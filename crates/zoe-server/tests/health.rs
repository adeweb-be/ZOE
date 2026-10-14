//! Acceptance test of the admin port: health and readiness (N9).
//!
//! `--admin-listen HOST:PORT` (`ZOE_ADMIN_LISTEN`) opens an HTTP port
//! beside the ZEO port. The server writes `admin <address>` to stdout
//! before `listening <address>`. `GET /healthz` answers 200 while every
//! store is open. `GET /readyz` answers 200 when every store has loaded its
//! last tid and its lock manager holds no stale entry, and 503 before. Any
//! other path is 404. Without the option, the server opens no admin port.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

mod wire;

use wire::{Server, commit, fresh_dir, new_oids, record, s};

/// Send `GET path` to the admin port and return the status and the body.
fn get(addr: SocketAddr, path: &str) -> (u16, String) {
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(10)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: zoe\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let status = response
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("no status line in {response:?}"));
    let body = response
        .split_once("\r\n\r\n")
        .map(|(_, body)| body.to_string())
        .unwrap_or_default();
    (status, body)
}

#[test]
fn the_help_names_the_admin_option_and_its_twin() {
    let output = std::process::Command::new(wire::ZOE)
        .args(["serve", "--help"])
        .output()
        .unwrap();
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(help.contains("--admin-listen"));
    assert!(help.contains("ZOE_ADMIN_LISTEN"));
}

#[test]
fn a_running_server_is_healthy_and_ready() {
    let dir = fresh_dir("ready");
    let server = Server::start(
        &[
            "--path",
            s(&dir.join("store")),
            "--admin-listen",
            "127.0.0.1:0",
        ],
        &[],
    );
    let admin = server
        .admin
        .expect("no admin line before the listening line");
    assert_ne!(admin, server.addr);
    assert_eq!(get(admin, "/healthz").0, 200);
    assert_eq!(get(admin, "/readyz").0, 200);
    assert_eq!(get(admin, "/no-such-page").0, 404);

    // Commits do not change it.
    let mut conn = server.client("1");
    let oid = new_oids(&mut conn, 1)[0];
    commit(&mut conn, 1, &[(oid, 0, record(b"x"))]).unwrap();
    assert_eq!(get(admin, "/readyz").0, 200);
}

#[test]
fn every_named_storage_counts() {
    let dir = fresh_dir("named");
    let server = Server::start(
        &[
            "--storage",
            &format!("a={}", s(&dir.join("a"))),
            "--storage",
            &format!("b={}", s(&dir.join("b"))),
            "--admin-listen",
            "127.0.0.1:0",
        ],
        &[],
    );
    let admin = server.admin.unwrap();
    let (status, body) = get(admin, "/readyz");
    assert_eq!(status, 200);
    // The body names each storage.
    assert!(body.contains('a') && body.contains('b'), "{body}");
}

#[test]
fn the_twin_gives_the_admin_address() {
    let dir = fresh_dir("twin");
    let server = Server::start(
        &["--path", s(&dir.join("store"))],
        &[("ZOE_ADMIN_LISTEN", "127.0.0.1:0")],
    );
    let admin = server.admin.expect("the twin opened no admin port");
    assert_eq!(get(admin, "/healthz").0, 200);
}

#[test]
fn without_the_option_there_is_no_admin_port() {
    let dir = fresh_dir("none");
    let server = Server::path(&dir.join("store"));
    assert!(server.admin.is_none());
}
