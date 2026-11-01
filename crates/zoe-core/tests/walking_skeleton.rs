//! Acceptance test of the walking skeleton: open a store, store one object,
//! finish, reload, kill, reopen. The crash simulation is attached.

mod support;

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use support::fresh_dir;
use zoe_core::vfs::MemVfs;
use zoe_core::{Oid, Store, Tid, ZoeError};

/// A protocol-3 ZODB record: the class pickle, then the state pickle.
const PICKLE: &[u8] =
    b"\x80\x03cpersistent.mapping\nPersistentMapping\nq\x00.\x80\x03}q\x01X\x04\x00\x00\x00datag\x01s.";

/// The variable that makes this test binary act as the writer process.
const WRITER: &str = "ZOE_WALKING_SKELETON_WRITER";

/// Commit one new object and return its oid and the tid of the commit.
fn commit_one(store: &Store) -> (Oid, Tid) {
    let oid = store.new_oid().unwrap();
    let mut txn = store.tpc_begin(b"user", b"walking skeleton", b"").unwrap();
    txn.store(oid, Tid::ZERO, PICKLE).unwrap();
    txn.vote().unwrap();
    let tid = txn.finish().unwrap();
    (oid, tid)
}

#[test]
fn a_finished_object_loads_after_reopen() {
    let dir = fresh_dir("reopen");
    let store = Store::open(&dir).unwrap();
    let (oid, tid) = commit_one(&store);
    assert_eq!(store.load(oid).unwrap(), (PICKLE.to_vec(), tid));
    assert_eq!(store.last_transaction(), tid);
    drop(store);

    let store = Store::open(&dir).unwrap();
    assert_eq!(store.load(oid).unwrap(), (PICKLE.to_vec(), tid));
    assert_eq!(store.last_transaction(), tid);
}

#[test]
fn tids_increase() {
    let dir = fresh_dir("tids");
    let store = Store::open(&dir).unwrap();
    let (_, first) = commit_one(&store);
    let (_, second) = commit_one(&store);
    assert!(second > first);
}

#[test]
fn an_acknowledged_commit_survives_every_crash_image() {
    let vfs = MemVfs::new();
    let (oid, tid) = {
        let store = Store::open_vfs(vfs.clone()).unwrap();
        let committed = commit_one(&store);
        vfs.mark("acknowledged");
        committed
    };

    // Each image is a prefix of the recorded writes, with unsynced writes
    // reordered or dropped and the tail truncated.
    let images = vfs.crash_images();
    assert!(images.len() > 1, "the simulation produced crash images");
    for image in &images {
        let store = Store::open_vfs(image.vfs())
            .unwrap_or_else(|e| panic!("{image:?} does not open without repair: {e}"));
        match store.load(oid) {
            Ok(found) => assert_eq!(found, (PICKLE.to_vec(), tid), "{image:?}"),
            Err(ZoeError::NotFound { .. }) => {
                assert!(
                    !image.is_after("acknowledged"),
                    "{image:?} lost an acknowledged commit"
                )
            }
            Err(e) => panic!("{image:?}: {e}"),
        }
    }
}

#[test]
fn an_acknowledged_commit_survives_sigkill() {
    // The child process is this test binary, with the writer variable set.
    if let Ok(dir) = std::env::var(WRITER) {
        let store = Store::open(PathBuf::from(dir).as_path()).unwrap();
        let (oid, tid) = commit_one(&store);
        println!("acknowledged {} {}", oid.0, tid.0);
        loop {
            std::thread::park();
        }
    }

    let dir = fresh_dir("sigkill");
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "an_acknowledged_commit_survives_sigkill",
            "--exact",
            "--nocapture",
        ])
        .env(WRITER, dir.path())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();

    let stdout = BufReader::new(child.stdout.take().unwrap());
    let line = stdout
        .lines()
        .map(Result::unwrap)
        .find(|line| line.starts_with("acknowledged "))
        .expect("the writer acknowledged a commit");
    child.kill().unwrap();
    child.wait().unwrap();

    let mut parts = line.split_whitespace().skip(1);
    let oid = Oid(parts.next().unwrap().parse().unwrap());
    let tid = Tid(parts.next().unwrap().parse().unwrap());

    let store = Store::open(&dir).unwrap();
    assert_eq!(store.load(oid).unwrap(), (PICKLE.to_vec(), tid));
}
