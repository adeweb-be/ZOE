//! Acceptance test of the read-only opens of a store with a write-ahead
//! log (ADR 0056, ADR 0028; N7).
//!
//! A clean close folds the log and marks each segment spare, so a strict
//! read-only open reads the store and changes no byte. A writer that a
//! kill stopped leaves commits in the log only: the strict read-only open
//! refuses such a store with `NotClosedCleanly`, and the open of the tools
//! folds the log into `zoe.redb` and writes no other file. A halted writer
//! closes its database cleanly and leaves its log: the strict open refuses
//! that store for the log alone. A refused open of a store of format 2
//! creates nothing.

mod support;

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use support::fresh_dir;
use xxhash_rust::xxh3::xxh3_64;
use zoe_core::testing::{self, commit, new_oid, try_commit};
use zoe_core::vfs::{DirLock, OsFile, OsVfs, Vfs, WriteFile};
use zoe_core::{Options, Store, SystemClock, Tid, WriteLog, ZoeError};

/// The database file of a store.
const DATABASE: &str = "zoe.redb";

/// Settings under which no fold starts by itself.
fn held() -> Options {
    Options {
        write_log: WriteLog {
            fold_bytes: 1 << 40,
            fold_age: std::time::Duration::from_secs(3600),
            backlog_bytes: 1 << 41,
            ..WriteLog::default()
        },
        ..Options::default()
    }
}

/// Return every entry under `dir`, by path: the bytes of a file, `None`
/// for a directory.
fn tree(dir: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    let mut entries = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path.clone());
                entries.insert(path, None);
            } else {
                entries.insert(path.clone(), Some(fs::read(&path).unwrap()));
            }
        }
    }
    entries
}

/// Copy every file and directory under `from` to `to`.
fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let path = entry.unwrap().path();
        let target = to.join(path.file_name().unwrap());
        if path.is_dir() {
            copy_tree(&path, &target);
        } else {
            fs::copy(&path, &target).unwrap();
        }
    }
}

/// Return the files of `wal/` in the store `dir`.
fn segments(dir: &Path) -> Vec<PathBuf> {
    match fs::read_dir(dir.join("wal")) {
        Ok(entries) => entries.map(|entry| entry.unwrap().path()).collect(),
        Err(_) => Vec::new(),
    }
}

/// Return the state of the segment `path`: 1 in use, 2 spare, or `None`
/// for a header that does not pass its checksum.
fn segment_state(path: &Path) -> Option<u32> {
    let bytes = fs::read(path).unwrap();
    let header = bytes.get(..32)?;
    let checksum = u64::from_le_bytes(header[24..32].try_into().unwrap());
    if &header[..8] != b"ZOE-WAL\0" || xxh3_64(&header[..24]) != checksum {
        return None;
    }
    Some(u32::from_le_bytes(header[12..16].try_into().unwrap()))
}

/// Check that only `zoe.redb` differs between `before` and `after`.
fn only_the_database_changed(
    dir: &Path,
    before: &BTreeMap<PathBuf, Option<Vec<u8>>>,
    after: &BTreeMap<PathBuf, Option<Vec<u8>>>,
) {
    assert_eq!(
        before.keys().collect::<Vec<_>>(),
        after.keys().collect::<Vec<_>>(),
        "the open added or removed an entry"
    );
    let database = dir.join(DATABASE);
    for (path, bytes) in before {
        if *path != database {
            assert_eq!(bytes, &after[path], "{} changed", path.display());
        }
    }
}

#[test]
fn a_tool_folds_the_log_of_a_killed_writer_and_a_strict_open_refuses_it() {
    let root = fresh_dir("killed-writer");
    let (live, image) = (root.join("live"), root.join("image"));
    let store = Store::open_with(&live, held()).unwrap();
    let oid = new_oid(&store);
    let first = commit(&store, oid, Tid::ZERO, b"first");
    store.fold().unwrap();
    // The second commit is in the log only. A copy of the directory now is
    // what a kill of the writer leaves.
    let second = commit(&store, oid, first, b"second");
    copy_tree(&live, &image);
    drop(store);
    assert!(!segments(&image).is_empty(), "the image holds no log");

    assert!(matches!(
        Store::open_read_only(&image),
        Err(ZoeError::NotClosedCleanly)
    ));
    let before = tree(&image);
    let store = Store::open_read_only_repairing(&image, Options::default()).unwrap();
    assert!(store.repaired());
    assert_eq!(store.load(oid).unwrap(), (b"second".to_vec(), second));
    assert_eq!(store.load_serial(oid, first).unwrap(), b"first".to_vec());
    drop(store);
    let after = tree(&image);
    only_the_database_changed(&image, &before, &after);

    // The repair folded the log, so a strict open reads the store and
    // changes no byte.
    let store = Store::open_read_only(&image).unwrap();
    assert_eq!(store.load(oid).unwrap(), (b"second".to_vec(), second));
    drop(store);
    assert_eq!(tree(&image), after);
}

#[test]
fn after_a_clean_close_a_strict_read_only_open_reads_every_commit_and_changes_no_byte() {
    let dir = fresh_dir("clean-close");
    let store = Store::open_with(&dir, held()).unwrap();
    let oid = new_oid(&store);
    let first = commit(&store, oid, Tid::ZERO, b"first");
    let second = commit(&store, oid, first, b"second");
    assert!(store.backlog_bytes() > 0);
    drop(store);

    let files = segments(&dir);
    assert!(!files.is_empty(), "the store has no log");
    for file in &files {
        assert_eq!(
            segment_state(file),
            Some(2),
            "{} is not spare",
            file.display()
        );
    }
    let before = tree(&dir);
    let store = Store::open_read_only(&dir).unwrap();
    assert_eq!(store.load(oid).unwrap(), (b"second".to_vec(), second));
    drop(store);
    assert_eq!(tree(&dir), before);
}

/// A directory of the operating system whose files of the log fail each
/// write while `fail` is true.
struct LogFailing {
    vfs: OsVfs,
    fail: Arc<AtomicBool>,
}

/// A file of `LogFailing`.
#[derive(Debug)]
struct LogFailingFile {
    file: OsFile,
    log: bool,
    fail: Arc<AtomicBool>,
}

impl redb::StorageBackend for LogFailingFile {
    fn len(&self) -> io::Result<u64> {
        self.file.len()
    }

    fn read(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
        self.file.read(offset, out)
    }

    fn set_len(&self, len: u64) -> io::Result<()> {
        self.file.set_len(len)
    }

    fn sync_data(&self) -> io::Result<()> {
        self.file.sync_data()
    }

    fn write(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        if self.log && self.fail.load(Ordering::SeqCst) {
            return Err(io::Error::other("an injected write failure"));
        }
        self.file.write(offset, data)
    }

    fn close(&self) -> io::Result<()> {
        self.file.close()
    }
}

impl LogFailing {
    fn wrap(&self, name: &str, file: OsFile) -> LogFailingFile {
        LogFailingFile {
            file,
            log: name.starts_with("wal/"),
            fail: Arc::clone(&self.fail),
        }
    }
}

impl Vfs for LogFailing {
    type File = LogFailingFile;

    fn exists(&self, name: &str) -> io::Result<bool> {
        self.vfs.exists(name)
    }

    fn open(&self, name: &str) -> io::Result<LogFailingFile> {
        Ok(self.wrap(name, self.vfs.open(name)?))
    }

    fn create_new(&self, name: &str) -> io::Result<LogFailingFile> {
        Ok(self.wrap(name, self.vfs.create_new(name)?))
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        self.vfs.rename(from, to)
    }

    fn hard_link(&self, from: &str, to: &str) -> io::Result<()> {
        self.vfs.hard_link(from, to)
    }

    fn remove(&self, name: &str) -> io::Result<()> {
        self.vfs.remove(name)
    }

    fn sync_dir(&self) -> io::Result<()> {
        self.vfs.sync_dir()
    }

    fn sync_dir_at(&self, dir: &str) -> io::Result<()> {
        self.vfs.sync_dir_at(dir)
    }

    fn create_dir(&self, name: &str) -> io::Result<()> {
        self.vfs.create_dir(name)
    }

    fn list_files(&self, dir: &str) -> io::Result<Vec<String>> {
        self.vfs.list_files(dir)
    }

    fn list_entries(&self, dir: &str) -> io::Result<Vec<String>> {
        self.vfs.list_entries(dir)
    }

    fn create_file(&self, name: &str) -> io::Result<Box<dyn WriteFile>> {
        self.vfs.create_file(name)
    }

    fn read_file(&self, name: &str) -> io::Result<Box<dyn io::Read + Send>> {
        self.vfs.read_file(name)
    }

    fn sync_file(&self, name: &str) -> io::Result<()> {
        self.vfs.sync_file(name)
    }

    fn set_read_only(&self, name: &str) -> io::Result<()> {
        self.vfs.set_read_only(name)
    }

    fn lock_dir(&self, name: &str) -> io::Result<DirLock> {
        self.vfs.lock_dir(name)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.vfs.path(name)
    }
}

#[test]
fn a_strict_read_only_open_refuses_the_log_that_a_halted_writer_left() {
    let dir = fresh_dir("halted-writer");
    let fail = Arc::new(AtomicBool::new(false));
    let vfs = LogFailing {
        vfs: OsVfs::new(&dir).unwrap(),
        fail: Arc::clone(&fail),
    };
    let store = Store::open_full(vfs, SystemClock, held()).unwrap();
    let oid = new_oid(&store);
    let first = commit(&store, oid, Tid::ZERO, b"first");
    store.fold().unwrap();
    // In the log only.
    let second = commit(&store, oid, first, b"second");
    // A failed write of the log halts the store. The fold never failed, so
    // the drop closes the database cleanly, and the log stays.
    fail.store(true, Ordering::SeqCst);
    assert!(try_commit(&store, oid, second, b"refused").is_err());
    fail.store(false, Ordering::SeqCst);
    assert!(matches!(
        try_commit(&store, oid, second, b"halted"),
        Err(ZoeError::Halted)
    ));
    drop(store);

    // redb was closed cleanly: the log alone makes the open refuse.
    assert!(matches!(
        Store::open_read_only(&dir),
        Err(ZoeError::NotClosedCleanly)
    ));
    let store = Store::open_read_only_repairing(&dir, Options::default()).unwrap();
    assert!(store.repaired());
    assert_eq!(store.load(oid).unwrap(), (b"second".to_vec(), second));
}

#[test]
fn a_refused_open_of_a_store_of_format_2_creates_nothing() {
    let dir = fresh_dir("format-2");
    let store = Store::open_with(&dir, held()).unwrap();
    let oid = new_oid(&store);
    commit(&store, oid, Tid::ZERO, b"pickle");
    testing::write_meta(&store, "format", b"ZOE format 2").unwrap();
    drop(store);
    fs::remove_dir_all(dir.join("wal")).unwrap();

    let before = tree(&dir);
    match Store::open_with(&dir, held()) {
        Err(ZoeError::Format(text)) => assert!(text.contains("ZOE format 2"), "{text}"),
        Err(other) => panic!("{other}"),
        Ok(_) => panic!("the open passed"),
    }
    // redb can write its own state into `zoe.redb`; nothing else changes.
    let after = tree(&dir);
    assert_eq!(
        before.keys().collect::<Vec<_>>(),
        after.keys().collect::<Vec<_>>(),
        "the refused open created an entry"
    );
    assert!(!dir.join("wal").exists());
}
