//! Acceptance test of a backup of a store that `zoe serve` holds open
//! (N15, the CronJob of the Tier 1 manifests).
//!
//! The server holds the lock of its stores, so no other process opens
//! them. `zoe serve --backup-dir DIR` (`ZOE_BACKUP_DIR`) names the
//! directory where the server writes the dumps of each storage, one
//! subdirectory per storage name. `POST /backup?storage=NAME` on the admin
//! port writes one dump of that storage, full or incremental, with the
//! dump writer of `zoe backup`, while the server goes on serving, and
//! answers with the last tid of the dump. `zoe backup --admin HOST:PORT
//! --storage NAME` (`ZOE_ADMIN`, `ZOE_STORAGE`) makes that call and exits
//! with 0 when the dump is written. Without `--backup-dir`, the call fails
//! and writes nothing.
//!
//! `zoe restore` restores such dumps as it restores those of `zoe backup
//! --path`.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use zoe_core::{Store, TxnInfo};

mod wire;

use wire::{Conn, Server, ZOE, commit, fresh_dir, new_oids, record, s, spawn_lock};

fn zoe(args: &[&str]) -> Output {
    let mut command = Command::new(ZOE);
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("ZOE_") {
            command.env_remove(name);
        }
    }
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = {
        let _spawn = spawn_lock();
        command.spawn().unwrap()
    };
    child.wait_with_output().unwrap()
}

fn transactions(path: &Path) -> Vec<TxnInfo> {
    let store = {
        let _spawn = spawn_lock();
        Store::open(path).unwrap()
    };
    store
        .iterator(None, None)
        .unwrap()
        .map(|item| item.unwrap())
        .collect()
}

#[test]
fn a_running_server_writes_a_dump_while_clients_commit() {
    let dir = fresh_dir("online");
    let (store, backups, target): (PathBuf, PathBuf, PathBuf) =
        (dir.join("store"), dir.join("backups"), dir.join("target"));
    let mut server = Server::start(
        &[
            "--path",
            s(&store),
            "--admin-listen",
            "127.0.0.1:0",
            "--backup-dir",
            s(&backups),
        ],
        &[],
    );
    let admin = server.admin.unwrap().to_string();

    // A writer commits all along.
    let stop = Arc::new(AtomicBool::new(false));
    let committed = Arc::new(Mutex::new(0usize));
    let writer = {
        let (stop, committed, addr) = (stop.clone(), committed.clone(), server.addr);
        thread::spawn(move || {
            let mut conn = Conn::open(addr);
            conn.register("1", false).unwrap();
            let mut n = 0;
            while !stop.load(Ordering::Relaxed) {
                n += 1;
                let oid = new_oids(&mut conn, 1)[0];
                commit(&mut conn, n, &[(oid, 0, record(b"w"))]).unwrap();
                *committed.lock().unwrap() += 1;
            }
        })
    };
    while *committed.lock().unwrap() < 10 {
        thread::yield_now();
    }

    let first = zoe(&["backup", "--admin", &admin, "--storage", "1"]);
    assert_eq!(first.status.code(), Some(0), "{first:?}");
    while *committed.lock().unwrap() < 20 {
        thread::yield_now();
    }
    let second = zoe(&["backup", "--admin", &admin, "--storage", "1"]);
    assert_eq!(second.status.code(), Some(0), "{second:?}");
    stop.store(true, Ordering::Relaxed);
    writer.join().unwrap();
    server.kill();

    let dumps = backups.join("1");
    let restored = zoe(&["restore", "--from", s(&dumps), "--path", s(&target)]);
    assert_eq!(restored.status.code(), Some(0), "{restored:?}");

    // The restored store is a prefix of the store: every transaction up to
    // the last tid of the second dump.
    let source = transactions(&store);
    let copy = transactions(&target);
    assert!(copy.len() >= 20);
    assert_eq!(copy[..], source[..copy.len()]);
}

#[test]
fn without_a_backup_directory_the_call_fails() {
    let dir = fresh_dir("no-dir");
    let server = Server::start(
        &[
            "--path",
            s(&dir.join("store")),
            "--admin-listen",
            "127.0.0.1:0",
        ],
        &[],
    );
    let admin = server.admin.unwrap().to_string();
    let output = zoe(&["backup", "--admin", &admin, "--storage", "1"]);
    assert_eq!(output.status.code(), Some(1));
    let output = zoe(&["backup", "--admin", &admin, "--storage", "nothing-here"]);
    assert_eq!(output.status.code(), Some(1));
}
