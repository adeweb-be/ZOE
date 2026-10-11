//! Acceptance test of the blob store: a blob is a file beside the store,
//! immutable per (oid, tid), laid out as ZODB's BushyLayout, with its
//! BLAKE3 hash in the blob record. The blob file and its record become
//! durable together.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use zoe_core::vfs::MemVfs;
use zoe_core::{Oid, Store, Tid, ZoeError};

const PICKLE: &[u8] = b"\x80\x03cZODB.blob\nBlob\nq\x00.\x80\x03N.";

fn fresh_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("zoe-blobs-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn content(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 7 + 3) as u8).collect()
}

/// Commit one blob for `oid` and return the tid.
fn commit_blob(store: &Store, oid: Oid, serial: Tid, bytes: &[u8]) -> Tid {
    let mut file = store.new_blob_file().unwrap();
    file.write_all(bytes).unwrap();
    let mut txn = store.tpc_begin(b"", b"blob", b"").unwrap();
    txn.store_blob(oid, serial, PICKLE, file).unwrap();
    txn.vote().unwrap();
    txn.finish().unwrap()
}

fn read_blob(store: &Store, oid: Oid, tid: Tid) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    store.open_blob(oid, tid).unwrap().read_to_end(&mut bytes)?;
    Ok(bytes)
}

#[test]
fn a_committed_blob_reads_back_after_reopen() {
    let dir = fresh_dir("reopen");
    let bytes = content(300_000);
    let (oid, tid) = {
        let store = Store::open(&dir).unwrap();
        let oid = store.new_oid().unwrap();
        (oid, commit_blob(&store, oid, Tid::ZERO, &bytes))
    };
    let store = Store::open(&dir).unwrap();
    assert_eq!(store.load(oid).unwrap(), (PICKLE.to_vec(), tid));
    assert_eq!(read_blob(&store, oid, tid).unwrap(), bytes);
    store.verify_blob(oid, tid).unwrap();
}

#[test]
fn each_revision_keeps_its_own_file() {
    let store = Store::open(fresh_dir("revisions")).unwrap();
    let oid = store.new_oid().unwrap();
    let t1 = commit_blob(&store, oid, Tid::ZERO, b"first");
    let t2 = commit_blob(&store, oid, t1, b"second");
    assert_eq!(read_blob(&store, oid, t1).unwrap(), b"first");
    assert_eq!(read_blob(&store, oid, t2).unwrap(), b"second");
    assert_ne!(
        store.blob_path(oid, t1).unwrap(),
        store.blob_path(oid, t2).unwrap()
    );
}

#[test]
fn the_files_follow_zodb_bushy_layout() {
    let dir = fresh_dir("layout");
    let store = Store::open(&dir).unwrap();
    let oid = store.new_oid().unwrap();
    let tid = commit_blob(&store, oid, Tid::ZERO, b"bushy");

    let blobs = dir.join("blobs");
    assert_eq!(
        std::fs::read_to_string(blobs.join(".layout"))
            .unwrap()
            .trim(),
        "bushy"
    );
    let mut expected = blobs.clone();
    for byte in oid.0.to_be_bytes() {
        expected.push(format!("0x{byte:02x}"));
    }
    expected.push(format!("0x{:016x}.blob", tid.0));
    assert_eq!(store.blob_path(oid, tid).unwrap(), expected);
    assert_eq!(std::fs::read(expected).unwrap(), b"bushy");
}

#[test]
fn a_changed_blob_file_is_detected() {
    let dir = fresh_dir("corrupt");
    let store = Store::open(&dir).unwrap();
    let oid = store.new_oid().unwrap();
    let tid = commit_blob(&store, oid, Tid::ZERO, &content(10_000));

    flip_byte(&store.blob_path(oid, tid).unwrap(), 5_000);

    let verified = store.verify_blob(oid, tid);
    assert!(
        matches!(verified, Err(ZoeError::CorruptBlob { oid: o, tid: t }) if o == oid && t == tid),
        "{verified:?}"
    );
    let read = read_blob(&store, oid, tid);
    assert_eq!(
        read.unwrap_err().kind(),
        std::io::ErrorKind::InvalidData,
        "reading a changed blob to its end fails"
    );
}

#[test]
fn an_aborted_blob_leaves_no_file() {
    let dir = fresh_dir("abort");
    let store = Store::open(&dir).unwrap();
    let oid = store.new_oid().unwrap();
    let mut file = store.new_blob_file().unwrap();
    file.write_all(b"never committed").unwrap();
    let mut txn = store.tpc_begin(b"", b"aborted", b"").unwrap();
    txn.store_blob(oid, Tid::ZERO, PICKLE, file).unwrap();
    txn.abort();
    assert_eq!(blob_files(&dir.join("blobs")), 0);
}

#[test]
fn a_crash_leaves_no_blob_without_record_and_no_record_without_blob() {
    let vfs = MemVfs::new();
    let bytes = content(50_000);
    let (oid, tid) = {
        let store = Store::open_vfs(vfs.clone()).unwrap();
        let oid = store.new_oid().unwrap();
        let tid = commit_blob(&store, oid, Tid::ZERO, &bytes);
        vfs.mark("acknowledged");
        (oid, tid)
    };

    let images = vfs.crash_images();
    assert!(images.len() > 1);
    for image in &images {
        let image_vfs = image.vfs();
        let store = Store::open_vfs(image_vfs.clone())
            .unwrap_or_else(|e| panic!("{image:?} does not open: {e}"));
        match store.load(oid) {
            Ok(found) => {
                assert_eq!(found, (PICKLE.to_vec(), tid), "{image:?}");
                assert_eq!(read_blob(&store, oid, tid).unwrap(), bytes, "{image:?}");
            }
            Err(ZoeError::NotFound { .. }) => {
                assert!(
                    !image.is_after("acknowledged"),
                    "{image:?} lost a blob commit"
                );
                // Opening removed any blob file whose record is missing.
                assert!(
                    image_vfs
                        .names()
                        .iter()
                        .all(|name| !name.ends_with(".blob")),
                    "{image:?} kept a blob without its record"
                );
            }
            Err(e) => panic!("{image:?}: {e}"),
        }
    }
}

fn flip_byte(path: &Path, offset: usize) {
    use std::os::unix::fs::PermissionsExt;
    // A committed blob file can be read-only, as in ZODB.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let mut bytes = std::fs::read(path).unwrap();
    bytes[offset] ^= 0x01;
    std::fs::write(path, bytes).unwrap();
}

fn blob_files(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .map(|entry| entry.unwrap().path())
        .map(|path| {
            if path.is_dir() {
                blob_files(&path)
            } else {
                usize::from(path.extension().is_some_and(|ext| ext == "blob"))
            }
        })
        .sum()
}
