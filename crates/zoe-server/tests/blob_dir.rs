//! Acceptance test of the blob directory of a store in `zoe verify` and
//! `zoe convert` (F25, F14, ADR 0027).
//!
//! A store can keep its blob files outside the store directory
//! (`Options::blob_dir`, the key `blob-dir` of the `<zoe>` section). The
//! store does not record that directory, so each open must give it.
//! `--blob-dir DIR` (`ZOE_BLOB_DIR`) gives it to `zoe verify` and to
//! `zoe convert`, in both directions. `--data-fs-blob-dir` stays the blob
//! directory of the `Data.fs`.
//!
//! - With the correct blob directory, a sound store passes, and the
//!   command changes no byte in the store directory or in the blob
//!   directory.
//! - A store with blob records whose blob directory has no layout file is
//!   not the store of that directory: the option is missing or wrong, or
//!   the directory is not mounted (ADR 0027). `zoe verify` and
//!   `zoe convert --to-data-fs` then fail with 1 before they walk the
//!   store. They write nothing and create nothing, and the message names
//!   the blob directory and `--blob-dir`. They do not report each blob.
//! - `zoe verify` tells a missing blob file from a damaged one. The text
//!   of a missing file says that it is missing. The text of a damaged file
//!   does not.
//! - `zoe convert --from-data-fs` with `--blob-dir` puts the blobs of the
//!   new store into that directory, and nothing into `<path>/blobs`.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Mutex, MutexGuard, PoisonError};

use zoe_core::{Oid, Options, Store, Tid};

const ZOE: &str = env!("CARGO_BIN_EXE_zoe");

/// A spawned child keeps a copy of each file descriptor of the test
/// process until its `exec` returns, the lock of a store too. So each open
/// of a store holds this mutex, and each spawn holds it until the child
/// ends.
static SPAWN: Mutex<()> = Mutex::new(());

fn spawn_lock() -> MutexGuard<'static, ()> {
    SPAWN.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Open the store in `dir` with its blob files in `blobs`.
fn open(dir: &Path, blobs: &Path) -> Store {
    let _spawn = spawn_lock();
    let options = Options {
        blob_dir: Some(blobs.to_path_buf()),
        ..Options::default()
    };
    Store::open_with(dir, options).unwrap()
}

fn fresh_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("zoe-blob-dir-{name}-{}", std::process::id()));
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

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Check the exit code, and show the output if it is not the one expected.
fn assert_exit(output: &Output, code: i32, what: &str) {
    assert_eq!(
        output.status.code(),
        Some(code),
        "{what}\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        stderr(output)
    );
}

/// Run `zoe verify --format json` on the store `dir` with the extra
/// arguments `extra` and the variables `env`. Return the exit code and
/// the report.
fn report(dir: &Path, extra: &[&str], env: &[(&str, &str)]) -> (i32, serde_json::Value) {
    let mut args = vec!["verify", "--path", s(dir), "--format", "json"];
    args.extend_from_slice(extra);
    let output = zoe(&args, env);
    let json = serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        panic!(
            "no JSON report\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            stderr(&output)
        )
    });
    (output.status.code().unwrap(), json)
}

/// The bytes of every file under `dir`, by path.
fn files(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
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

/// A `PersistentMapping` record whose state holds references to `refs`.
fn mapping(refs: &[u64]) -> Vec<u8> {
    let mut pickle = b"\x80\x03cpersistent.mapping\nPersistentMapping\nq\x00.".to_vec();
    pickle.extend_from_slice(b"\x80\x03}X\x04\x00\x00\x00refs]");
    for oid in refs {
        pickle.extend_from_slice(b"(C\x08");
        pickle.extend_from_slice(&oid.to_be_bytes());
        pickle.extend_from_slice(b"cpersistent.mapping\nPersistentMapping\ntQa");
    }
    pickle.extend_from_slice(b"s.");
    pickle
}

/// The record of a `ZODB.blob.Blob`.
const BLOB_RECORD: &[u8] = b"\x80\x03cZODB.blob\nBlob\nq\x00.\x80\x03N.";

/// The bytes of the two blobs of the corpus. Each has 11 bytes.
const FIRST: &[u8] = b"first blob.";
const SECOND: &[u8] = b"second blob";

struct Corpus {
    /// The store directory.
    dir: PathBuf,
    /// The blob directory, beside the store directory.
    blobs: PathBuf,
    /// The blob records of `FIRST` and `SECOND`, as `(oid, tid)`.
    first: (Oid, Tid),
    second: (Oid, Tid),
}

/// A store whose blob files live in a directory beside the store
/// directory: a root that refers to two blobs.
fn corpus(name: &str) -> Corpus {
    let root = fresh_dir(name);
    let (dir, blobs) = (root.join("store"), root.join("blobs-elsewhere"));
    let store = open(&dir, &blobs);
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(Oid(0), Tid(0), &mapping(&[1, 2])).unwrap();
    for (oid, bytes) in [(Oid(1), FIRST), (Oid(2), SECOND)] {
        let mut file = store.new_blob_file().unwrap();
        file.write_all(bytes).unwrap();
        txn.store_blob(oid, Tid(0), BLOB_RECORD, file).unwrap();
    }
    txn.vote().unwrap();
    let tid = txn.finish().unwrap();
    drop(store);
    Corpus {
        dir,
        blobs,
        first: (Oid(1), tid),
        second: (Oid(2), tid),
    }
}

#[test]
fn the_help_names_the_option_and_its_twin() {
    for subcommand in ["verify", "convert"] {
        let output = zoe(&[subcommand, "--help"], &[]);
        assert_exit(&output, 0, subcommand);
        let help = String::from_utf8_lossy(&output.stdout);
        assert!(help.contains("--blob-dir"), "{subcommand}:\n{help}");
        assert!(help.contains("ZOE_BLOB_DIR"), "{subcommand}:\n{help}");
    }
}

#[test]
fn verify_reads_the_blobs_from_the_blob_directory_that_it_gets() {
    let corpus = corpus("verify");
    let before = (files(&corpus.dir), files(&corpus.blobs));

    let (code, json) = report(&corpus.dir, &["--blob-dir", s(&corpus.blobs)], &[]);
    assert_eq!(code, 0, "{json}");
    assert!(json["blobs"].as_array().unwrap().is_empty(), "{json}");
    assert_eq!(json["classes"]["ZODB.blob.Blob"], 2, "{json}");

    // The twin gives the same blob directory.
    let (code, json) = report(&corpus.dir, &[], &[("ZOE_BLOB_DIR", s(&corpus.blobs))]);
    assert_eq!(code, 0, "{json}");

    // The read-only open changes no byte in either directory.
    assert_eq!((files(&corpus.dir), files(&corpus.blobs)), before);
}

#[test]
fn a_missing_blob_directory_fails_with_a_message_that_names_it() {
    let corpus = corpus("no-blob-dir");
    let before = files(&corpus.dir);

    // Without --blob-dir, the store looks for `blobs` in the store
    // directory, where it has no blob.
    let output = zoe(
        &["verify", "--path", s(&corpus.dir), "--format", "json"],
        &[],
    );
    assert_exit(&output, 1, "verify without the blob directory");
    let message = stderr(&output);
    assert!(message.contains("--blob-dir"), "{message}");
    assert!(message.contains(s(&corpus.dir.join("blobs"))), "{message}");
    // The command reports no blob: it stops before the walk.
    assert!(
        output.stdout.is_empty(),
        "a report came out:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(!corpus.dir.join("blobs").exists());

    // A wrong blob directory: an empty one.
    let empty = fresh_dir("no-blob-dir-empty");
    let output = zoe(
        &["verify", "--path", s(&corpus.dir), "--blob-dir", s(&empty)],
        &[],
    );
    assert_exit(&output, 1, "verify with an empty blob directory");
    let message = stderr(&output);
    assert!(message.contains(s(&empty)), "{message}");
    assert_eq!(std::fs::read_dir(&empty).unwrap().count(), 0);

    // A blob directory whose parent does not exist: the command creates
    // nothing.
    let nowhere = empty.join("not-here").join("blobs");
    let output = zoe(
        &[
            "verify",
            "--path",
            s(&corpus.dir),
            "--blob-dir",
            s(&nowhere),
        ],
        &[],
    );
    assert_exit(
        &output,
        1,
        "verify with a blob directory that has no parent",
    );
    assert!(!empty.join("not-here").exists());

    assert_eq!(files(&corpus.dir), before, "verify changed the store");
}

#[test]
fn verify_tells_a_missing_blob_file_from_a_damaged_one() {
    let corpus = corpus("missing-or-damaged");
    let (missing, damaged) = {
        let store = open(&corpus.dir, &corpus.blobs);
        let (first, second) = (corpus.first, corpus.second);
        (
            store.blob_path(first.0, first.1).unwrap(),
            store.blob_path(second.0, second.1).unwrap(),
        )
    };
    std::fs::remove_file(&missing).unwrap();
    // A committed blob file is read-only. The damage keeps its size.
    let mut permissions = std::fs::metadata(&damaged).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    permissions.set_readonly(false);
    std::fs::set_permissions(&damaged, permissions).unwrap();
    std::fs::write(&damaged, b"second blub").unwrap();

    let (code, json) = report(&corpus.dir, &["--blob-dir", s(&corpus.blobs)], &[]);
    assert_eq!(code, 1, "{json}");
    let blobs = json["blobs"].as_array().unwrap();
    assert_eq!(blobs.len(), 2, "{json}");
    let error_of = |oid: Oid| -> String {
        let finding = blobs
            .iter()
            .find(|finding| finding["oid"] == oid.0)
            .unwrap_or_else(|| panic!("no finding for oid {}: {json}", oid.0));
        finding["error"].as_str().unwrap().to_owned()
    };
    let missing_text = error_of(corpus.first.0);
    let damaged_text = error_of(corpus.second.0);
    assert!(
        missing_text.to_lowercase().contains("missing"),
        "{missing_text}"
    );
    assert!(
        !damaged_text.to_lowercase().contains("missing"),
        "{damaged_text}"
    );
    assert_ne!(missing_text, damaged_text);

    // The text report gives the same two texts.
    let output = zoe(
        &[
            "verify",
            "--path",
            s(&corpus.dir),
            "--blob-dir",
            s(&corpus.blobs),
        ],
        &[],
    );
    assert_exit(&output, 1, "the text report");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains(&missing_text), "{text}");
    assert!(text.contains(&damaged_text), "{text}");
}

#[test]
fn convert_to_a_data_fs_reads_the_blobs_from_the_blob_directory_that_it_gets() {
    let corpus = corpus("to-data-fs");
    let out = fresh_dir("to-data-fs-out");
    let (data_fs, data_fs_blobs) = (out.join("Data.fs"), out.join("blobs"));
    let to = [
        "convert",
        "--to-data-fs",
        s(&data_fs),
        "--data-fs-blob-dir",
        s(&data_fs_blobs),
        "--path",
        s(&corpus.dir),
    ];

    // Without the blob directory of the store: refused, and nothing
    // written.
    let output = zoe(&to, &[]);
    assert_exit(
        &output,
        1,
        "convert without the blob directory of the store",
    );
    assert!(
        stderr(&output).contains("--blob-dir"),
        "{}",
        stderr(&output)
    );
    assert!(!data_fs.exists());
    assert!(!data_fs_blobs.exists());

    // With it: the blob directory of the Data.fs holds both blobs.
    let mut with = to.to_vec();
    with.extend_from_slice(&["--blob-dir", s(&corpus.blobs)]);
    assert_exit(&zoe(&with, &[]), 0, "convert with --blob-dir");
    assert!(data_fs.is_file());
    let mut written: Vec<Vec<u8>> = files(&data_fs_blobs)
        .into_iter()
        .filter(|(path, _)| path.extension().is_some_and(|ext| ext == "blob"))
        .map(|(_, bytes)| bytes)
        .collect();
    written.sort();
    assert_eq!(written, vec![FIRST.to_vec(), SECOND.to_vec()]);

    // The twin gives the same blob directory. The store has nothing new,
    // so the conversion only verifies the file.
    let output = zoe(&to, &[("ZOE_BLOB_DIR", s(&corpus.blobs))]);
    assert_exit(&output, 0, "convert with ZOE_BLOB_DIR");
}

#[test]
fn convert_from_a_data_fs_writes_the_blobs_into_the_blob_directory_that_it_gets() {
    let corpus = corpus("from-data-fs");
    let out = fresh_dir("from-data-fs-out");
    let (data_fs, data_fs_blobs) = (out.join("Data.fs"), out.join("blobs"));
    let output = zoe(
        &[
            "convert",
            "--to-data-fs",
            s(&data_fs),
            "--data-fs-blob-dir",
            s(&data_fs_blobs),
            "--path",
            s(&corpus.dir),
            "--blob-dir",
            s(&corpus.blobs),
        ],
        &[],
    );
    assert_exit(&output, 0, "convert to a Data.fs");

    let (target, target_blobs) = (out.join("target"), out.join("target-blobs"));
    let output = zoe(
        &[
            "convert",
            "--from-data-fs",
            s(&data_fs),
            "--data-fs-blob-dir",
            s(&data_fs_blobs),
            "--path",
            s(&target),
            "--blob-dir",
            s(&target_blobs),
        ],
        &[],
    );
    assert_exit(&output, 0, "convert from a Data.fs");
    assert!(
        !target.join("blobs").exists(),
        "the blobs went into the store directory"
    );

    let store = open(&target, &target_blobs);
    for ((oid, tid), expected) in [(corpus.first, FIRST), (corpus.second, SECOND)] {
        let mut found = Vec::new();
        std::io::Read::read_to_end(&mut store.open_blob(oid, tid).unwrap(), &mut found).unwrap();
        assert_eq!(found, expected, "oid {}", oid.0);
    }
}
