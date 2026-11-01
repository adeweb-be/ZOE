//! Acceptance test of the startup time and the memory bound of `zoe
//! serve` on a store larger than its memory (N6, N7).
//!
//! N7: the server is ready, `listening` on stdout and `/readyz` at 200,
//! within 2 s of its start, whatever the size of the store: an open
//! rebuilds no index.
//!
//! N6: the resident memory of the server stays under a bound that its
//! configuration sets, whatever the size of the store. `--cache-size`
//! (`ZOE_CACHE_SIZE`) bounds the page cache of the store. With it, the
//! blocking threads, the queues of the clients and the staging memory of
//! the transactions, the documentation of `zoe serve` gives the bound. The
//! test reads objects at random over the whole store from many clients
//! and checks the peak resident memory of the server.
//!
//! The defaults are small, so that `just test` runs it: a store of 128 MB,
//! a cache of 16 MB and a bound of 96 MB. The reference machine runs it with
//! `ZOE_LARGE_STORE_MB=102400`, `ZOE_CACHE_SIZE_MB=1024` and
//! `ZOE_RSS_LIMIT_MB=2048`, the numbers of N6 and N7.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use zoe_core::{Compression, Oid, Options, Store, Tid};

mod common;

use common::{Server, ZOE, fresh_dir, load, s, spawn_lock};

fn env_mb(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Bytes that do not compress.
fn noise(seed: u64, len: usize) -> Vec<u8> {
    let mut x = seed | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

/// A record of about `size` bytes.
fn record(seed: u64, size: usize) -> Vec<u8> {
    let payload = noise(seed, size);
    let mut pickle = b"\x80\x03cpersistent.mapping\nPersistentMapping\nq\x00.".to_vec();
    pickle.extend_from_slice(b"\x80\x03}X\x01\x00\x00\x00vB");
    pickle.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    pickle.extend_from_slice(&payload);
    pickle.extend_from_slice(b"s.");
    pickle
}

const RECORD: usize = 16 << 10;

/// Write a store of about `mb` megabytes. Return its number of objects.
fn generate(dir: &std::path::Path, mb: u64) -> u64 {
    let objects = mb * (1 << 20) / RECORD as u64;
    let store = {
        let _spawn = spawn_lock();
        let options = Options {
            compression: Compression::None,
            ..Options::default()
        };
        Store::open_with(dir, options).unwrap()
    };
    let mut oid = 0;
    while oid < objects {
        let mut txn = store.tpc_begin(b"", b"generate", b"").unwrap();
        for _ in 0..256.min(objects - oid) {
            txn.store(Oid(oid), Tid(0), &record(oid, RECORD)).unwrap();
            oid += 1;
        }
        txn.vote().unwrap();
        txn.finish().unwrap();
    }
    objects
}

fn status_kib(pid: u32, key: &str) -> u64 {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
    status
        .lines()
        .find_map(|line| line.strip_prefix(key))
        .and_then(|value| value.split_whitespace().next())
        .and_then(|kib| kib.parse().ok())
        .unwrap()
}

fn ready(admin: std::net::SocketAddr) -> bool {
    let Ok(mut stream) = TcpStream::connect(admin) else {
        return false;
    };
    let _ = write!(
        stream,
        "GET /readyz HTTP/1.1\r\nHost: zoe\r\nConnection: close\r\n\r\n"
    );
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    response.starts_with("HTTP/1.1 200")
}

#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "reads the memory of the server in /proc: Linux only"
)]
fn a_large_store_starts_fast_and_serves_in_bounded_memory() {
    let store_mb = env_mb("ZOE_LARGE_STORE_MB", 128);
    let cache_mb = env_mb("ZOE_CACHE_SIZE_MB", 16);
    let limit_mb = env_mb("ZOE_RSS_LIMIT_MB", 96);
    assert!(
        store_mb > limit_mb,
        "the store must be larger than the bound"
    );
    let dir = fresh_dir("large");
    let path = dir.join("store");
    let objects = generate(&path, store_mb);

    // N7: ready within 2 s.
    let started = Instant::now();
    let server = Server::start(
        &[
            "--path",
            s(&path),
            "--admin-listen",
            "127.0.0.1:0",
            "--cache-size",
            &format!("{cache_mb}MB"),
        ],
        &[],
    );
    let admin = server.admin.unwrap();
    while !ready(admin) {
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "not ready within 2 s"
        );
        thread::sleep(Duration::from_millis(5));
    }
    assert!(started.elapsed() < Duration::from_secs(2));

    // N6: many clients read the whole store at random.
    let clients: Vec<_> = (0..16u64)
        .map(|c| {
            let addr = server.addr;
            thread::spawn(move || {
                let mut conn = common::Conn::open(addr);
                conn.register("1", false).unwrap();
                let mut x = c * 7919 + 1;
                for _ in 0..(objects / 4).max(500) {
                    x = x
                        .wrapping_mul(6364136223846793005)
                        .wrapping_add(1442695040888963407);
                    load(&mut conn, (x >> 11) % objects).unwrap();
                }
            })
        })
        .collect();
    for client in clients {
        client.join().unwrap();
    }
    let peak_mb = status_kib(server.pid(), "VmHWM:") / 1024;
    assert!(
        peak_mb <= limit_mb,
        "the server peaked at {peak_mb} MB on a store of {store_mb} MB with a bound of {limit_mb} MB"
    );

    // The help names the option and its twin.
    let help = {
        let _spawn = spawn_lock();
        Command::new(ZOE)
            .args(["serve", "--help"])
            .stdout(Stdio::piped())
            .output()
            .unwrap()
    };
    let help = String::from_utf8_lossy(&help.stdout);
    assert!(help.contains("--cache-size") && help.contains("ZOE_CACHE_SIZE"));
}
