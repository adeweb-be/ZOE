//! Acceptance test of `zoe verify` (F25).
//!
//! `zoe verify --path STORE` opens the store read-only and checks:
//!
//! - checksums: every frame of every record, transaction row, blob record
//!   and sequence row reads and passes its checksum;
//! - blobs: every blob file has the size and the BLAKE3 hash of its blob
//!   record;
//! - references: every persistent reference of a current record names an
//!   oid that has a current record;
//! - classes: the class of every current record, with its count. With
//!   `--classes FILE` (`ZOE_CLASSES`), a file of `module.name` lines, the
//!   classes that the file does not name are findings.
//!
//! `--format json` (`ZOE_FORMAT`) prints one JSON document:
//! `{"checksums": [{"oid", "tid", "error"}], "blobs": [{"oid", "tid",
//! "error"}], "references": [{"oid", "tid", "missing"}], "classes":
//! {"module.name": count}, "unknown_classes": ["module.name"]}`, with oids
//! and tids as integers. The text format says the same for a person. The
//! command exits with 0 when it finds nothing, with 1 when it finds
//! something, and with 2 on a usage error. It changes no byte of the store.
//! References come from zoe-pickle, never from an unpickler.

use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::{Mutex, MutexGuard, PoisonError};

use zoe_core::{Compression, Oid, Options, Store, Tid};

/// A spawned child keeps a copy of each file descriptor of the test
/// process until `exec` closes them in the child, the lock of a store too.
/// `Command::spawn` can return before that: the kernel wakes the parent of
/// a `vfork` before the child closes these copies. So each open of a store
/// holds this mutex, and each spawn holds it until the child ends.
static SPAWN: Mutex<()> = Mutex::new(());

fn spawn_lock() -> MutexGuard<'static, ()> {
    SPAWN.lock().unwrap_or_else(PoisonError::into_inner)
}

fn open_store(dir: &Path) -> Store {
    let _spawn = spawn_lock();
    Store::open(dir).unwrap()
}

const ZOE: &str = env!("CARGO_BIN_EXE_zoe");

fn fresh_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("zoe-verify-{name}-{}", std::process::id()));
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
    let _spawn = spawn_lock();
    command.spawn().unwrap().wait_with_output().unwrap()
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

/// A record of a class that no module provides, holding `payload`.
fn thing(payload: &[u8]) -> Vec<u8> {
    let mut pickle = b"\x80\x03cno.such.module\nThing\nq\x00.".to_vec();
    pickle.extend_from_slice(b"\x80\x03}X\x01\x00\x00\x00pB");
    pickle.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    pickle.extend_from_slice(payload);
    pickle.extend_from_slice(b"s.");
    pickle
}

/// Bytes that zstd does not shrink, so the store keeps them as they are.
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

struct Corpus {
    dir: std::path::PathBuf,
    marker: Vec<u8>,
    marked: Oid,
    blob: (Oid, Tid),
}

/// A store with a dangling reference, a class that no module provides, a
/// blob, and a record whose bytes hold a marker.
fn corpus(name: &str) -> Corpus {
    let dir = fresh_dir(name);
    // Without compression, the bytes of the marker are in the file.
    let store = {
        let _spawn = spawn_lock();
        let options = Options {
            compression: Compression::None,
            ..Options::default()
        };
        Store::open_with(&dir, options).unwrap()
    };
    let marker = noise(7, 256);
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    // oid 0 refers to 1, which exists, and to 99, which does not.
    txn.store(Oid(0), Tid(0), &mapping(&[1, 99])).unwrap();
    txn.store(Oid(1), Tid(0), &mapping(&[2, 3])).unwrap();
    txn.store(Oid(2), Tid(0), &thing(&marker)).unwrap();
    txn.vote().unwrap();
    txn.finish().unwrap();

    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    let mut file = store.new_blob_file().unwrap();
    std::io::Write::write_all(&mut file, b"blob bytes").unwrap();
    txn.store_blob(
        Oid(3),
        Tid(0),
        b"\x80\x03cZODB.blob\nBlob\nq\x00.\x80\x03N.",
        file,
    )
    .unwrap();
    txn.vote().unwrap();
    let tid = txn.finish().unwrap();
    let blob = (Oid(3), tid);
    drop(store);
    Corpus {
        dir,
        marker,
        marked: Oid(2),
        blob,
    }
}

fn report(dir: &Path, extra: &[&str]) -> (i32, serde_json::Value) {
    let mut args = vec!["verify", "--path", s(dir), "--format", "json"];
    args.extend_from_slice(extra);
    let output = zoe(&args);
    let json = serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        panic!(
            "no JSON report\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    });
    (output.status.code().unwrap(), json)
}

fn digest(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut files = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(path) = stack.pop() {
        for entry in std::fs::read_dir(&path).unwrap() {
            let entry = entry.unwrap().path();
            if entry.is_dir() {
                stack.push(entry);
            } else {
                files.push((entry.display().to_string(), std::fs::read(&entry).unwrap()));
            }
        }
    }
    files.sort();
    files
}

#[test]
fn a_dangling_reference_and_the_classes_are_reported() {
    let corpus = corpus("references");
    let before = digest(&corpus.dir);
    let (code, json) = report(&corpus.dir, &[]);
    assert_eq!(code, 1, "{json}");
    assert_eq!(digest(&corpus.dir), before, "verify changed the store");

    let references = json["references"].as_array().unwrap();
    assert_eq!(references.len(), 1, "{json}");
    assert_eq!(references[0]["oid"], 0);
    assert_eq!(references[0]["missing"], 99);

    assert_eq!(json["classes"]["persistent.mapping.PersistentMapping"], 2);
    assert_eq!(json["classes"]["no.such.module.Thing"], 1);
    assert_eq!(json["classes"]["ZODB.blob.Blob"], 1);
    assert!(json["checksums"].as_array().unwrap().is_empty());
    assert!(json["blobs"].as_array().unwrap().is_empty());
}

#[test]
fn classes_outside_the_list_are_findings() {
    let corpus = corpus("classes");
    let list = corpus.dir.join("classes.txt");
    std::fs::write(
        &list,
        "persistent.mapping.PersistentMapping\nZODB.blob.Blob\n",
    )
    .unwrap();
    let (code, json) = report(&corpus.dir, &["--classes", s(&list)]);
    assert_eq!(code, 1);
    assert_eq!(
        json["unknown_classes"],
        serde_json::json!(["no.such.module.Thing"])
    );
}

#[test]
fn a_damaged_blob_file_is_reported() {
    let corpus = corpus("blob");
    let path = {
        let store = open_store(&corpus.dir);
        store.blob_path(corpus.blob.0, corpus.blob.1).unwrap()
    };
    let mut permissions = std::fs::metadata(&path).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    permissions.set_readonly(false);
    std::fs::set_permissions(&path, permissions).unwrap();
    std::fs::write(&path, b"blob bytez").unwrap();

    let (code, json) = report(&corpus.dir, &[]);
    assert_eq!(code, 1);
    let blobs = json["blobs"].as_array().unwrap();
    assert_eq!(blobs.len(), 1, "{json}");
    assert_eq!(blobs[0]["oid"], corpus.blob.0.0);
    assert_eq!(blobs[0]["tid"], corpus.blob.1.0);
}

#[test]
fn a_flipped_bit_in_a_record_is_reported() {
    let corpus = corpus("checksum");
    let db = corpus.dir.join("zoe.redb");
    let mut bytes = std::fs::read(&db).unwrap();
    let at = bytes
        .windows(corpus.marker.len())
        .position(|w| w == corpus.marker.as_slice())
        .expect("the marker is not in the database file");
    bytes[at + 100] ^= 0x10;
    std::fs::write(&db, bytes).unwrap();

    let (code, json) = report(&corpus.dir, &[]);
    assert_eq!(code, 1);
    let checksums = json["checksums"].as_array().unwrap();
    assert!(
        checksums.iter().any(|c| c["oid"] == corpus.marked.0),
        "{json}"
    );
}

#[test]
fn a_sound_store_passes_and_a_missing_store_is_an_error() {
    let dir = fresh_dir("sound");
    {
        let store = open_store(&dir);
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.store(Oid(0), Tid(0), &mapping(&[])).unwrap();
        txn.vote().unwrap();
        txn.finish().unwrap();
    }
    let (code, json) = report(&dir, &[]);
    assert_eq!(code, 0, "{json}");

    let output = zoe(&["verify", "--path", s(&dir.join("nothing-here"))]);
    assert_eq!(output.status.code(), Some(1));
    assert!(!dir.join("nothing-here").exists());

    let output = zoe(&["verify"]);
    assert_eq!(output.status.code(), Some(2));
}
