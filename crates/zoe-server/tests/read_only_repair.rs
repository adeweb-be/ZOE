//! Acceptance test of the repair on open in each command that reads a
//! store (N1, F15, F20, F25, F26, F27, N15).
//!
//! A `zoe serve` that SIGKILL ends leaves its store open: the database
//! file keeps the recovery flag of redb. Each command that opens a store
//! read-only then repairs the store under its lock, closes the database
//! cleanly and opens the store read-only: `zoe verify`, `zoe verify
//! --backup`, `zoe convert --to-data-fs`, `zoe pack --dry-run`, `zoe
//! backup`, `zoe mcp` and `zoe serve --read-only`. After the command, the
//! strict read-only open of the core accepts the store, and the last
//! acknowledged commit is there. On a store that was closed cleanly, the
//! command changes no byte of the store.
//!
//! A command that cannot write the database file, as on a read-only file
//! system, fails with 1 and changes no byte. Its message says that the
//! store was not closed cleanly and gives the error of the system. The
//! test takes the write permission away from the files of the store: a
//! read-only file system fails the open for writes in the same place.

mod wire;

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use wire::{Server, ZOE, commit, fresh_dir, load, record, s};
use zoe_core::{Oid, Store, ZoeError};

/// The longest run of one command.
const TIMEOUT: Duration = Duration::from_secs(60);

/// The capabilities that let root ignore the mode of a file.
const MODE_CAPS: &str = "-dac_override,-dac_read_search";

/// The end of a run of the `zoe` binary.
struct Run {
    status: ExitStatus,
    stderr: String,
}

/// Make a store in `root/killed` that a writer left open: `zoe serve`
/// commits twice, then SIGKILL ends it. Return the store directory and the
/// tid of the last commit.
fn killed_store(root: &Path) -> (PathBuf, u64) {
    let path = root.join("killed");
    let mut server = Server::path(&path);
    let mut conn = server.client("1");
    let first = commit(&mut conn, 1, &[(0, 0, record(b"first"))]).unwrap();
    let last = commit(&mut conn, 2, &[(0, first, record(b"second"))]).unwrap();
    drop(conn);
    server.kill();
    let _spawn = wire::spawn_lock();
    let strict = Store::open_read_only(&path);
    assert!(
        matches!(strict, Err(ZoeError::NotClosedCleanly)),
        "the killed server left a clean store: {:?}",
        strict.err()
    );
    (path, last)
}

/// Copy the store directory `from` to `to`, with every file under it.
fn copy_store(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_store(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).unwrap();
        }
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
                let bytes = fs::read(&path).unwrap();
                entries.insert(path, Some(bytes));
            }
        }
    }
    entries
}

/// Give each file under `dir` the mode `file`, and each directory, `dir`
/// included, the mode `directory`.
fn set_modes(dir: &Path, file: u32, directory: u32) {
    for entry in fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            set_modes(&entry.path(), file, directory);
        } else {
            fs::set_permissions(entry.path(), fs::Permissions::from_mode(file)).unwrap();
        }
    }
    fs::set_permissions(dir, fs::Permissions::from_mode(directory)).unwrap();
}

/// Tell if this process obeys the mode of a file. Root ignores the mode
/// while it holds the capabilities that bypass it.
fn modes_apply() -> bool {
    static APPLY: OnceLock<bool> = OnceLock::new();
    *APPLY.get_or_init(|| {
        let probe = fresh_dir("repair-modes").join("probe");
        fs::write(&probe, b"").unwrap();
        fs::set_permissions(&probe, fs::Permissions::from_mode(0o444)).unwrap();
        let refused = fs::OpenOptions::new().write(true).open(&probe).is_err();
        fs::set_permissions(&probe, fs::Permissions::from_mode(0o644)).unwrap();
        refused
    })
}

/// Run the `zoe` binary with `args`, an empty stdin and no `ZOE_*`
/// variable, and wait at most `TIMEOUT` for its end. Its output goes to
/// files in `root`.
///
/// With `obey_modes`, the binary obeys the mode of each file, as a user
/// that is not root does: when this process ignores the modes, the binary
/// runs through `setpriv`, without the capabilities that bypass them.
fn zoe(root: &Path, args: &[String], obey_modes: bool) -> Run {
    static RUNS: AtomicUsize = AtomicUsize::new(0);
    let n = RUNS.fetch_add(1, Ordering::Relaxed);
    let stdout = root.join(format!("run-{n}.out"));
    let stderr = root.join(format!("run-{n}.err"));
    let mut command = if obey_modes && !modes_apply() {
        let mut command = Command::new("setpriv");
        command
            .arg(format!("--inh-caps={MODE_CAPS}"))
            .arg(format!("--bounding-set={MODE_CAPS}"))
            .arg(ZOE);
        command
    } else {
        Command::new(ZOE)
    };
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("ZOE_") {
            command.env_remove(name);
        }
    }
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(fs::File::create(&stdout).unwrap())
        .stderr(fs::File::create(&stderr).unwrap());
    let mut child = {
        let _spawn = wire::spawn_lock();
        command
            .spawn()
            .unwrap_or_else(|e| panic!("zoe {args:?} does not start: {e}"))
    };
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if started.elapsed() > TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "zoe {args:?} did not end within {TIMEOUT:?}: {}",
                fs::read_to_string(&stderr).unwrap_or_default()
            );
        }
        thread::sleep(Duration::from_millis(20));
    };
    Run {
        status,
        stderr: fs::read_to_string(&stderr).unwrap(),
    }
}

/// Return `words` as the arguments of a command.
fn args(words: &[&str]) -> Vec<String> {
    words.iter().map(|word| word.to_string()).collect()
}

/// Write the dumps of the store in `killed` into `root/dumps`, from a copy
/// that a writer open repaired. Return the backup directory.
fn dumps_of(root: &Path, killed: &Path) -> PathBuf {
    let source = root.join("source");
    copy_store(killed, &source);
    drop(wire::open_store(&source));
    let dumps = root.join("dumps");
    let run = zoe(
        root,
        &args(&["backup", "--path", s(&source), "--to", s(&dumps)]),
        false,
    );
    assert!(run.status.success(), "zoe backup: {}", run.stderr);
    dumps
}

/// The commands that open a store read-only and end by themselves.
const COMMANDS: [&str; 6] = [
    "verify",
    "verify --backup",
    "convert --to-data-fs",
    "pack --dry-run",
    "backup",
    "mcp",
];

/// Return the arguments of the command `name` for the store in `path`.
/// What the command writes outside the store goes into `out`. `dumps`
/// holds the dumps of the store.
fn command(name: &str, path: &Path, out: &Path, dumps: &Path) -> Vec<String> {
    let scratch = out.join("scratch");
    fs::create_dir_all(&scratch).unwrap();
    let path = s(path);
    let data_fs = out.join("Data.fs");
    let data_fs_blobs = out.join("blobs");
    let backup = out.join("backup");
    match name {
        "verify" => args(&["verify", "--path", path]),
        "verify --backup" => args(&[
            "verify",
            "--path",
            path,
            "--backup",
            s(dumps),
            "--scratch",
            s(&scratch),
        ]),
        "convert --to-data-fs" => args(&[
            "convert",
            "--to-data-fs",
            s(&data_fs),
            "--data-fs-blob-dir",
            s(&data_fs_blobs),
            "--path",
            path,
        ]),
        "pack --dry-run" => args(&["pack", "--path", path, "--days", "0", "--dry-run"]),
        "backup" => args(&["backup", "--path", path, "--to", s(&backup)]),
        "mcp" => args(&["mcp", "--path", path]),
        "serve --read-only" => args(&[
            "serve",
            "--read-only",
            "--path",
            path,
            "--listen",
            "127.0.0.1:0",
        ]),
        _ => unreachable!("no command {name}"),
    }
}

/// Check that the store in `path` was closed cleanly, and that its last
/// commit is the one of `killed_store`.
fn assert_clean(path: &Path, last: u64, after: &str) {
    let _spawn = wire::spawn_lock();
    let store = Store::open_read_only(path)
        .unwrap_or_else(|e| panic!("after zoe {after}, the strict read-only open fails: {e}"));
    let (data, tid) = store.load(Oid(0)).unwrap();
    assert_eq!(
        (data, tid.0),
        (record(b"second"), last),
        "after zoe {after}"
    );
}

#[test]
fn each_read_only_command_repairs_a_store_that_a_writer_left_open() {
    let root = fresh_dir("repair-commands");
    let (killed, last) = killed_store(&root);
    let dumps = dumps_of(&root, &killed);

    for (index, name) in COMMANDS.into_iter().enumerate() {
        let image = root.join(format!("image-{index}"));
        copy_store(&killed, &image);
        let arguments = command(name, &image, &root.join(format!("out-{index}")), &dumps);

        let run = zoe(&root, &arguments, false);
        assert!(run.status.success(), "zoe {name}: {}", run.stderr);
        assert_clean(&image, last, name);

        // The store is clean now: the command runs again and changes no
        // byte of it.
        let before = tree(&image);
        let run = zoe(&root, &arguments, false);
        assert!(run.status.success(), "zoe {name} again: {}", run.stderr);
        assert!(tree(&image) == before, "zoe {name} changed a clean store");
    }
}

#[test]
fn a_read_only_server_repairs_a_store_that_a_writer_left_open() {
    let root = fresh_dir("repair-serve");
    let (killed, last) = killed_store(&root);
    let image = root.join("image");
    copy_store(&killed, &image);

    let mut server = Server::start(&["--path", s(&image), "--read-only"], &[]);
    let mut conn = server.connect();
    conn.register("1", true).unwrap();
    assert_eq!(load(&mut conn, 0).unwrap(), (record(b"second"), last));
    drop(conn);
    let (status, _) = server.terminate(TIMEOUT);
    assert_eq!(status.code(), Some(0));
    assert_clean(&image, last, "serve --read-only");

    // The store is clean now: a read-only server changes no byte of it.
    let before = tree(&image);
    let mut server = Server::start(&["--path", s(&image), "--read-only"], &[]);
    let mut conn = server.connect();
    conn.register("1", true).unwrap();
    assert_eq!(load(&mut conn, 0).unwrap(), (record(b"second"), last));
    drop(conn);
    let (status, _) = server.terminate(TIMEOUT);
    assert_eq!(status.code(), Some(0));
    assert!(
        tree(&image) == before,
        "a read-only server changed a clean store"
    );
}

#[test]
fn a_command_that_cannot_write_the_store_fails_with_the_cause_and_changes_nothing() {
    let root = fresh_dir("repair-denied");
    let (killed, _) = killed_store(&root);
    let dumps = dumps_of(&root, &killed);

    let names = COMMANDS.into_iter().chain(["serve --read-only"]);
    for (index, name) in names.enumerate() {
        let image = root.join(format!("image-{index}"));
        copy_store(&killed, &image);
        let arguments = command(name, &image, &root.join(format!("out-{index}")), &dumps);
        set_modes(&image, 0o444, 0o555);
        let before = tree(&image);

        let run = zoe(&root, &arguments, true);

        let after = tree(&image);
        set_modes(&image, 0o644, 0o755);
        assert_eq!(run.status.code(), Some(1), "zoe {name}: {}", run.stderr);
        assert!(
            run.stderr.contains("not closed cleanly"),
            "zoe {name} does not say that the store was not closed cleanly: {}",
            run.stderr
        );
        assert!(
            run.stderr.contains("Permission denied"),
            "zoe {name} does not give the error of the system: {}",
            run.stderr
        );
        assert!(after == before, "zoe {name} changed the store");
    }
}
