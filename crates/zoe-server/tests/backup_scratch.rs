//! Acceptance test of the scratch copy of `zoe verify --backup` (N15,
//! dumps). It uses the commands `zoe backup` and `zoe verify --backup` as
//! the build of `zoe backup` and `zoe restore` gives them.
//!
//! `zoe verify --path STORE --backup DIR` restores the dumps of DIR into a
//! scratch copy, compares the copy with the store, and removes the copy.
//!
//! - `--scratch SCRATCH` (`ZOE_SCRATCH`) names the directory of the copy.
//!   The command then puts nothing in the temporary directory (`TMPDIR`).
//!   Without `--scratch`, the copy goes to the temporary directory.
//!   `--scratch` without `--backup` is a usage error.
//! - The command restores the dumps before it opens the store, so the
//!   store stays free while the copy is built. A dump that does not
//!   restore fails the command before the open: the message names the
//!   dump, also when another process holds the store.
//! - The copy is a store that the command removes at the end, also when
//!   the check fails. It is built with the normal durable restore, as
//!   `zoe restore` builds a store.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Mutex, MutexGuard, PoisonError};

use zoe_core::{Store, Tid};

const ZOE: &str = env!("CARGO_BIN_EXE_zoe");

/// A spawned child keeps a copy of each file descriptor of the test
/// process until `exec` closes them in the child, the lock of a store too.
/// So each open of a store holds this mutex, and each spawn holds it until
/// the child ends.
static SPAWN: Mutex<()> = Mutex::new(());

fn spawn_lock() -> MutexGuard<'static, ()> {
    SPAWN.lock().unwrap_or_else(PoisonError::into_inner)
}

fn open_store(dir: &Path) -> Store {
    let _spawn = spawn_lock();
    Store::open(dir).unwrap()
}

fn fresh_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("zoe-scratch-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn s(path: &Path) -> &str {
    path.to_str().unwrap()
}

/// Run `zoe` with the arguments `args` and the variables `env`. No other
/// `ZOE_*` variable reaches it.
fn zoe(args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut command = Command::new(ZOE);
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("ZOE_") {
            command.env_remove(name);
        }
    }
    command
        .args(args)
        .envs(env.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let _spawn = spawn_lock();
    command.spawn().unwrap().wait_with_output().unwrap()
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

/// Commit `n` transactions of one new object each, then one blob.
fn commit(store: &Store, n: u8) {
    for i in 0..n {
        let oid = store.new_oid().unwrap();
        let mut txn = store
            .tpc_begin(b"user", format!("txn {i}").as_bytes(), b"")
            .unwrap();
        txn.store(oid, Tid(0), &record(&[i])).unwrap();
        txn.vote().unwrap();
        txn.finish().unwrap();
    }
    let oid = store.new_oid().unwrap();
    let mut txn = store.tpc_begin(b"", b"blob", b"").unwrap();
    let mut file = store.new_blob_file().unwrap();
    file.write_all(&[7; 1000]).unwrap();
    txn.store_blob(oid, Tid(0), BLOB_RECORD, file).unwrap();
    txn.vote().unwrap();
    txn.finish().unwrap();
}

/// The bytes of every file under `dir`, by path.
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

/// The number of entries in the directory `dir`.
fn entries(dir: &Path) -> usize {
    std::fs::read_dir(dir).unwrap().count()
}

/// A store with 20 transactions and a blob, and its dumps.
struct Backup {
    dir: PathBuf,
    source: PathBuf,
    dumps: PathBuf,
}

fn backup(name: &str) -> Backup {
    let dir = fresh_dir(name);
    let (source, dumps) = (dir.join("source"), dir.join("dumps"));
    {
        let store = open_store(&source);
        commit(&store, 20);
    }
    let output = zoe(&["backup", "--path", s(&source), "--to", s(&dumps)], &[]);
    assert_exit(&output, 0, "backup");
    Backup { dir, source, dumps }
}

#[test]
fn the_copy_goes_into_the_scratch_directory_and_not_into_tmpdir() {
    let backup = backup("copy");
    let scratch = backup.dir.join("scratch");
    std::fs::create_dir_all(&scratch).unwrap();
    // A temporary directory that is a regular file: a copy cannot go there.
    let tmp = backup.dir.join("tmp-is-a-file");
    std::fs::write(&tmp, b"not a directory").unwrap();
    let before = files_of(&backup.source);

    // Without --scratch, the copy goes to TMPDIR, so the check fails.
    let output = zoe(
        &[
            "verify",
            "--path",
            s(&backup.source),
            "--backup",
            s(&backup.dumps),
        ],
        &[("TMPDIR", s(&tmp))],
    );
    assert_exit(&output, 1, "a copy into TMPDIR, which is a file");

    // With --scratch, the copy goes there, and the scratch directory is
    // empty again at the end.
    let output = zoe(
        &[
            "verify",
            "--path",
            s(&backup.source),
            "--backup",
            s(&backup.dumps),
            "--scratch",
            s(&scratch),
        ],
        &[("TMPDIR", s(&tmp))],
    );
    assert_exit(&output, 0, "a copy into the scratch directory");
    assert_eq!(entries(&scratch), 0, "the copy stayed");

    // The twin gives the same scratch directory.
    let output = zoe(
        &[
            "verify",
            "--path",
            s(&backup.source),
            "--backup",
            s(&backup.dumps),
        ],
        &[("TMPDIR", s(&tmp)), ("ZOE_SCRATCH", s(&scratch))],
    );
    assert_exit(&output, 0, "a copy into the scratch directory of the twin");
    assert_eq!(entries(&scratch), 0, "the copy stayed");

    assert_eq!(std::fs::read(&tmp).unwrap(), b"not a directory");
    assert_eq!(files_of(&backup.source), before, "verify changed the store");
}

#[test]
fn scratch_without_backup_is_a_usage_error() {
    let dir = fresh_dir("usage");
    let (source, scratch) = (dir.join("source"), dir.join("scratch"));
    let output = zoe(
        &["verify", "--path", s(&source), "--scratch", s(&scratch)],
        &[],
    );
    assert_exit(&output, 2, "--scratch without --backup");
    // A usage error changes nothing on disk.
    assert!(!source.exists());
    assert!(!scratch.exists());
}

#[test]
fn a_damaged_dump_fails_before_the_open_of_the_store() {
    let backup = backup("order");
    let scratch = backup.dir.join("scratch");
    std::fs::create_dir_all(&scratch).unwrap();
    let (damaged, mut bytes) = files_of(&backup.dumps)
        .into_iter()
        .max_by_key(|(_, bytes)| bytes.len())
        .unwrap();
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0x01;
    std::fs::write(&damaged, bytes).unwrap();

    // This process holds the store. A check that opened the store first
    // would fail on its lock. The check restores the dumps first, so it
    // fails on the damaged dump and names it.
    let held = open_store(&backup.source);
    let output = zoe(
        &[
            "verify",
            "--path",
            s(&backup.source),
            "--backup",
            s(&backup.dumps),
            "--scratch",
            s(&scratch),
        ],
        &[],
    );
    drop(held);
    assert_exit(&output, 1, "a damaged dump");
    let message = String::from_utf8_lossy(&output.stderr);
    assert!(message.contains(s(&damaged)), "{message}");
    assert_eq!(entries(&scratch), 0, "the failed copy stayed");
}
