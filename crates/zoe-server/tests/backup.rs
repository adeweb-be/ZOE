//! Acceptance test of `zoe backup` and `zoe restore`, repozo-style dumps
//! (N15).
//!
//! `zoe backup --path STORE --to DIR` (`ZOE_PATH`, `ZOE_TO`) opens the store
//! read-only and writes a dump into DIR: a full dump the first time, then
//! an incremental dump of the transactions after the last tid that DIR
//! holds. It reads the transactions through the iterator of the store. A
//! dump holds each transaction with its tid, user, description, extension
//! and records, deletions included, the blob file of each blob record, and
//! the end of each sequence. A backup never changes a dump that DIR holds.
//! With nothing new, it writes nothing and exits with 0.
//!
//! `zoe restore --from DIR --path NEW` (`ZOE_FROM`) rebuilds the store from
//! the dumps, in order, under the same tids: the same transactions, the
//! same records, the same blob files. Each sequence restarts at or above
//! its end in the dump, so no value comes twice. A restore refuses a target
//! that holds a store. A damaged or missing dump fails the restore with 1
//! and leaves no store at the target.
//!
//! `zoe verify --path STORE --backup DIR` restores the dumps into a scratch
//! directory and compares them with the store up to the last tid of the
//! dumps. It exits with 1 when they differ or when a dump is damaged.
//!
//! The layout of a dump is ADR 0030, written by the card that builds it.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Mutex, MutexGuard, PoisonError};

use zoe_core::{Oid, Store, Tid, TxnInfo};

const ZOE: &str = env!("CARGO_BIN_EXE_zoe");

static SPAWN: Mutex<()> = Mutex::new(());

fn spawn_lock() -> MutexGuard<'static, ()> {
    SPAWN.lock().unwrap_or_else(PoisonError::into_inner)
}

fn open_store(dir: &Path) -> Store {
    let _spawn = spawn_lock();
    Store::open(dir).unwrap()
}

fn fresh_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("zoe-backup-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn s(path: &Path) -> &str {
    path.to_str().unwrap()
}

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

fn assert_exit(output: &Output, code: i32, what: &str) {
    assert_eq!(
        output.status.code(),
        Some(code),
        "{what}\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A record that holds `value`.
fn record(value: &[u8]) -> Vec<u8> {
    let mut pickle = b"\x80\x03cpersistent.mapping\nPersistentMapping\nq\x00.".to_vec();
    pickle.extend_from_slice(b"\x80\x03}q\x00X\x01\x00\x00\x00vq\x01C");
    pickle.push(value.len() as u8);
    pickle.extend_from_slice(value);
    pickle.extend_from_slice(b"q\x02s.");
    pickle
}

const BLOB_RECORD: &[u8] = b"\x80\x03cZODB.blob\nBlob\nq\x00.\x80\x03N.";

/// Commit one round of changes: new objects, a change, a deletion and a
/// blob. Return the oids that it wrote.
fn round(store: &Store, n: u8) -> Vec<Oid> {
    let a = store.new_oid().unwrap();
    let b = store.new_oid().unwrap();
    let mut txn = store
        .tpc_begin(b"user", format!("round {n}").as_bytes(), b"")
        .unwrap();
    txn.store(a, Tid(0), &record(&[n])).unwrap();
    txn.store(b, Tid(0), &record(&[n, n])).unwrap();
    txn.vote().unwrap();
    let tid = txn.finish().unwrap();

    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(a, tid, &record(&[n, 1])).unwrap();
    txn.vote().unwrap();
    let changed = txn.finish().unwrap();

    // Only a restore transaction writes a deletion.
    let mut txn = store
        .tpc_begin_restore(Tid(changed.0 + 1), b"", b"delete", b"")
        .unwrap();
    txn.restore(b, None).unwrap();
    txn.vote().unwrap();
    txn.finish().unwrap();

    let c = store.new_oid().unwrap();
    let mut txn = store.tpc_begin(b"", b"blob", b"").unwrap();
    let mut file = store.new_blob_file().unwrap();
    file.write_all(&vec![n; 100_000]).unwrap();
    txn.store_blob(c, Tid(0), BLOB_RECORD, file).unwrap();
    txn.vote().unwrap();
    txn.finish().unwrap();
    vec![a, b, c]
}

/// Every transaction of a store, with its records.
fn transactions(store: &Store) -> Vec<TxnInfo> {
    store
        .iterator(None, None)
        .unwrap()
        .map(|item| item.unwrap())
        .collect()
}

fn blob_bytes(store: &Store, oid: Oid, tid: Tid) -> Vec<u8> {
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut store.open_blob(oid, tid).unwrap(), &mut bytes).unwrap();
    bytes
}

fn same_content(left: &Store, right: &Store, blobs: &[(Oid, Tid)]) {
    assert_eq!(transactions(left), transactions(right));
    assert_eq!(left.last_transaction(), right.last_transaction());
    for (oid, tid) in blobs {
        assert_eq!(blob_bytes(left, *oid, *tid), blob_bytes(right, *oid, *tid));
    }
}

/// The blobs of a store: each blob record, as `(oid, tid)`.
fn blobs_of(store: &Store, oids: &[Oid]) -> Vec<(Oid, Tid)> {
    oids.iter()
        .flat_map(|oid| {
            store
                .history(*oid, 100)
                .unwrap_or_default()
                .into_iter()
                .filter(|entry| store.open_blob(*oid, entry.tid).is_ok())
                .map(move |entry| (*oid, entry.tid))
        })
        .collect()
}

fn files_of(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut files = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(path) = stack.pop() {
        for entry in std::fs::read_dir(&path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                let bytes = std::fs::read(&path).unwrap();
                files.push((path, bytes));
            }
        }
    }
    files.sort();
    files
}

#[test]
fn a_full_then_an_incremental_dump_restore_the_same_store() {
    let dir = fresh_dir("restore");
    let (source, dumps, target) = (dir.join("source"), dir.join("dumps"), dir.join("target"));
    let mut oids = Vec::new();
    let last_value;
    {
        let store = open_store(&source);
        oids.extend(round(&store, 1));
        let sequence = store.sequence("intids");
        for _ in 0..5 {
            sequence.next().unwrap();
        }
    }
    assert_exit(
        &zoe(&["backup", "--path", s(&source), "--to", s(&dumps)]),
        0,
        "full",
    );
    let full = files_of(&dumps);
    assert!(!full.is_empty());

    {
        let store = open_store(&source);
        oids.extend(round(&store, 2));
        last_value = store.sequence("intids").next().unwrap();
    }
    assert_exit(
        &zoe(&["backup", "--path", s(&source), "--to", s(&dumps)]),
        0,
        "incremental",
    );
    let both = files_of(&dumps);
    // The incremental dump adds files and changes none of the full dump.
    for file in &full {
        assert!(both.contains(file), "{} changed", file.0.display());
    }
    assert!(both.len() > full.len());

    // Nothing new: nothing written.
    assert_exit(
        &zoe(&["backup", "--path", s(&source), "--to", s(&dumps)]),
        0,
        "nothing new",
    );
    assert_eq!(files_of(&dumps), both);

    assert_exit(
        &zoe(&["restore", "--from", s(&dumps), "--path", s(&target)]),
        0,
        "restore",
    );
    let source = open_store(&source);
    let target = open_store(&target);
    let blobs = blobs_of(&source, &oids);
    assert!(!blobs.is_empty());
    same_content(&source, &target, &blobs);
    // No sequence value comes twice, and new oids come after every oid.
    assert!(target.sequence("intids").next().unwrap() > last_value);
    let new = target.new_oid().unwrap();
    assert!(oids.iter().all(|oid| new > *oid));
}

#[test]
fn verify_compares_the_dumps_with_the_store() {
    let dir = fresh_dir("verify");
    let (source, dumps) = (dir.join("source"), dir.join("dumps"));
    {
        let store = open_store(&source);
        round(&store, 1);
    }
    assert_exit(
        &zoe(&["backup", "--path", s(&source), "--to", s(&dumps)]),
        0,
        "backup",
    );
    let check = || zoe(&["verify", "--path", s(&source), "--backup", s(&dumps)]);
    assert_exit(&check(), 0, "fresh dumps");

    // A commit after the last dump is not a difference.
    {
        let store = open_store(&source);
        round(&store, 2);
    }
    assert_exit(&check(), 0, "a commit after the dumps");

    // A damaged dump is.
    let (largest, mut bytes) = files_of(&dumps)
        .into_iter()
        .max_by_key(|(_, bytes)| bytes.len())
        .unwrap();
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0x01;
    std::fs::write(&largest, bytes).unwrap();
    assert_exit(&check(), 1, "a damaged dump");
}

#[test]
fn a_bad_restore_leaves_no_store_and_never_overwrites_one() {
    let dir = fresh_dir("refusals");
    let (source, dumps) = (dir.join("source"), dir.join("dumps"));
    {
        let store = open_store(&source);
        round(&store, 1);
    }
    assert_exit(
        &zoe(&["backup", "--path", s(&source), "--to", s(&dumps)]),
        0,
        "backup",
    );

    // A target that holds a store is refused, and stays as it is.
    let before = files_of(&source);
    assert_exit(
        &zoe(&["restore", "--from", s(&dumps), "--path", s(&source)]),
        1,
        "overwrite",
    );
    assert_eq!(files_of(&source), before);

    // A damaged dump fails the restore and leaves no store.
    let (largest, mut bytes) = files_of(&dumps)
        .into_iter()
        .max_by_key(|(_, bytes)| bytes.len())
        .unwrap();
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0x01;
    std::fs::write(&largest, bytes).unwrap();
    let target = dir.join("target");
    assert_exit(
        &zoe(&["restore", "--from", s(&dumps), "--path", s(&target)]),
        1,
        "damaged",
    );
    assert!(!target.join("zoe.redb").exists());

    // An empty directory is no backup.
    let empty = fresh_dir("refusals-empty");
    assert_exit(
        &zoe(&[
            "restore",
            "--from",
            s(&empty),
            "--path",
            s(&dir.join("other")),
        ]),
        1,
        "no dump",
    );
}
