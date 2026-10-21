//! Acceptance test of the recovery of the blob temporary directory (F2).
//!
//! A writer open removes only the files of `blobs/tmp/` whose names the
//! server owns: `<digits>.tmp`, the intent files, `layout.tmp` and
//! `upload-<digits>`. Every other file stays. With a shared blob
//! directory, `blobs/tmp/` is the `temporaryDirectory()` of each ZEO
//! client, so a restart of the server keeps the files of the clients.

mod support;

use std::fs;
use std::io::{Read, Write};
use std::path::Path;

use support::fresh_dir;
use zoe_core::vfs::{MemVfs, Vfs};
use zoe_core::{Options, Store};

/// The names that the server owns. A writer open removes them.
const OWNED: &[&str] = &[
    "7.tmp",
    "1234567890.tmp",
    "layout.tmp",
    "0x03f0000000000010.intent",
    "0x00000000000000FF.intent",
    "upload-3",
    "upload-12",
];

/// Names that the server does not own. A writer open keeps them.
const FOREIGN: &[&str] = &[
    "tmpk3x9q2b_",
    "tmp7.tmp",
    "savepoint-a1b2c3",
    "client.tmp-",
    "7.tmp-",
    "12a.tmp",
    ".tmp",
    "7.tmp.part",
    "0x10.intent",
    "0x03f0000000000010.intent.old",
    "layout.tmp.1",
    "upload-",
    "upload-3x",
    "upload--3",
    "Upload-3",
    "notes.txt",
];

/// The bytes of the file `name`.
fn bytes_of(name: &str) -> Vec<u8> {
    format!("the file {name}").into_bytes()
}

/// Write each file of `names` in the blob temporary directory of the
/// store in `vfs`, and make the files durable.
fn write_files(vfs: &MemVfs, names: &[&str]) {
    for name in names {
        let mut file = Vfs::create_file(vfs, &format!("blobs/tmp/{name}")).unwrap();
        file.write_all(&bytes_of(name)).unwrap();
        file.sync().unwrap();
    }
    Vfs::sync_dir_at(vfs, "blobs/tmp").unwrap();
}

/// Return the names of the files in the blob temporary directory of the
/// store in `vfs`, sorted.
fn tmp_files(vfs: &MemVfs) -> Vec<String> {
    let mut names = Vfs::list_files(vfs, "blobs/tmp").unwrap();
    names.sort();
    names
}

/// Return `names` sorted, as strings.
fn sorted(names: &[&str]) -> Vec<String> {
    let mut names: Vec<String> = names.iter().map(|name| name.to_string()).collect();
    names.sort();
    names
}

/// Read the file `name` of `vfs`.
fn read(vfs: &MemVfs, name: &str) -> Vec<u8> {
    let mut bytes = Vec::new();
    Vfs::read_file(vfs, name)
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    bytes
}

#[test]
fn a_writer_open_removes_only_the_names_that_the_server_owns() {
    let vfs = MemVfs::new();
    let store = Store::open_vfs(vfs.clone()).unwrap();
    // The blob directory gets its layout file and `tmp/`.
    store.blob_temp_dir().unwrap();
    drop(store);
    write_files(&vfs, OWNED);
    write_files(&vfs, FOREIGN);

    drop(Store::open_vfs(vfs.clone()).unwrap());
    assert_eq!(tmp_files(&vfs), sorted(FOREIGN));
    for name in FOREIGN {
        assert_eq!(read(&vfs, &format!("blobs/tmp/{name}")), bytes_of(name));
    }

    // The next open keeps them too.
    drop(Store::open_vfs(vfs.clone()).unwrap());
    assert_eq!(tmp_files(&vfs), sorted(FOREIGN));
}

#[test]
fn a_crash_keeps_the_files_of_the_clients() {
    let vfs = MemVfs::new();
    let store = Store::open_vfs(vfs.clone()).unwrap();
    store.blob_temp_dir().unwrap();
    write_files(&vfs, OWNED);
    write_files(&vfs, FOREIGN);

    // A power loss while the store is open.
    let image = vfs.crash_now(|_| 0);
    drop(store);
    drop(Store::open_vfs(image.clone()).unwrap());
    assert_eq!(tmp_files(&image), sorted(FOREIGN));
}

/// Open the store in `dir`/store with its blob directory `dir`/shared, as
/// `zoe serve --blob-dir` does.
fn open_shared(dir: &Path) -> Store {
    let options = Options {
        blob_dir: Some(dir.join("shared")),
        ..Options::default()
    };
    Store::open_with(dir.join("store"), options).unwrap()
}

/// Return the names of the files in `dir`, sorted.
fn files_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap())
        .filter(|entry| entry.file_type().unwrap().is_file())
        .map(|entry| entry.file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "needs a file system that tells case apart: Linux only"
)]
fn a_restart_keeps_the_temporary_files_of_the_clients_of_a_shared_blob_directory() {
    let dir = fresh_dir("shared");
    let store = open_shared(&dir);
    let tmp = store.blob_temp_dir().unwrap();
    // The clients write their uncommitted blobs and their savepoints in
    // `temporaryDirectory()`, which is this directory.
    for name in OWNED.iter().chain(FOREIGN) {
        fs::write(tmp.join(name), bytes_of(name)).unwrap();
    }
    drop(store);

    // The server restarts. The clients stay up.
    let store = open_shared(&dir);
    assert_eq!(files_in(&tmp), sorted(FOREIGN));
    for name in FOREIGN {
        assert_eq!(fs::read(tmp.join(name)).unwrap(), bytes_of(name));
    }
    drop(store);
    fs::remove_dir_all(&dir).unwrap();
}
