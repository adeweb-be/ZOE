//! Tests of the `zoe` binary beside its acceptance test.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

mod common;

/// The binary under test.
const ZOE: &str = env!("CARGO_BIN_EXE_zoe");

fn fresh_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("zoe-cli-options-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Return a command that runs `zoe` with `args` and the variables `env`.
/// No other `ZOE_*` variable reaches it. Its stdout and stderr are pipes.
fn zoe(args: &[&str], env: &[(&str, &str)]) -> Command {
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
    command
}

fn run(args: &[&str], env: &[(&str, &str)]) -> Output {
    common::output(&mut zoe(args, env))
}

/// Check the exit code, and show the output if it is not the one expected.
fn assert_exit(output: &Output, code: i32, what: &str) {
    assert_eq!(
        output.status.code(),
        Some(code),
        "{what}\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Make an empty store in `dir`.
fn empty_store(dir: &Path) {
    drop(common::open(dir));
}

#[test]
fn dry_run_on_the_command_line_wins_over_its_twin() {
    let dir = fresh_dir("dry-run");
    let store = dir.join("zoe");
    empty_store(&store);
    let path = store.to_str().unwrap();
    let pack = |flag: &str, twin: &str| {
        let output = run(
            &["pack", "--path", path, "--days", "7", flag],
            &[("ZOE_DRY_RUN", twin)],
        );
        assert_exit(&output, 0, &format!("{flag} with ZOE_DRY_RUN={twin}"));
        String::from_utf8(output.stdout).unwrap()
    };
    assert!(pack("--dry-run=false", "true").starts_with("Packed"));
    assert!(pack("--dry-run", "false").starts_with("A pack"));
    assert!(pack("--dry-run=true", "false").starts_with("A pack"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_source_that_is_not_a_data_fs_creates_no_store() {
    let dir = fresh_dir("not-data-fs");
    let not_data_fs = dir.join("not-a-data-fs");
    std::fs::write(&not_data_fs, b"this is not a FileStorage file").unwrap();
    let short = dir.join("short");
    std::fs::write(&short, b"FS").unwrap();
    for source in [&dir, &not_data_fs, &short, &dir.join("missing")] {
        let store = dir.join("zoe");
        let args = [
            "convert",
            "--from-data-fs",
            source.to_str().unwrap(),
            "--path",
            store.to_str().unwrap(),
        ];
        let output = run(&args, &[]);
        assert_exit(&output, 1, &format!("convert {}", source.display()));
        assert!(!output.stderr.is_empty(), "the failure has no message");
        assert!(!store.exists(), "{} created a store", source.display());
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Run `zoe` with `args` and a stdout whose reader is gone.
fn run_with_closed_stdout(args: &[&str]) -> Output {
    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);
    common::output(zoe(args, &[]).stdout(writer))
}

#[test]
fn a_reader_that_closes_stdout_does_not_fail_the_command() {
    let dir = fresh_dir("closed-stdout");
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../zoe-convert/tests/data-fs");
    let (data_fs, blobs) = (fixture.join("Data.fs"), fixture.join("blobs-bushy"));
    let store = dir.join("zoe");
    let path = store.to_str().unwrap();
    let convert = [
        "convert",
        "--from-data-fs",
        data_fs.to_str().unwrap(),
        "--data-fs-blob-dir",
        blobs.to_str().unwrap(),
        "--path",
        path,
    ];
    let output = run_with_closed_stdout(&convert);
    assert_exit(&output, 0, "convert with a closed stdout");
    let last = common::open(&store).last_transaction();
    assert_ne!(last, zoe_core::Tid::ZERO, "the conversion copied nothing");

    let output = run_with_closed_stdout(&["pack", "--path", path, "--days", "7"]);
    assert_exit(&output, 0, "pack with a closed stdout");
    let output = run_with_closed_stdout(&["--help"]);
    assert_exit(&output, 0, "help with a closed stdout");
    let _ = std::fs::remove_dir_all(&dir);
}
