//! Tests of the `zoe` binary beside its acceptance test.

use std::path::{Path, PathBuf};
use std::process::Output;

mod common;

use common::{assert_exit, fresh_dir, zoe, zoe_command};

/// Make an empty store in `dir`.
fn empty_store(dir: &Path) {
    drop(common::open_store(dir));
}

#[test]
fn dry_run_on_the_command_line_wins_over_its_twin() {
    let dir = fresh_dir("dry-run");
    let store = dir.join("zoe");
    empty_store(&store);
    let path = store.to_str().unwrap();
    let pack = |flag: &str, twin: &str| {
        let output = zoe(
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

/// A record of class `m.C` whose state is a dict that holds a reference
/// to each oid of `refs`.
fn record(refs: &[zoe_core::Oid]) -> Vec<u8> {
    let mut p = b"\x80\x03cm\nC\nq\x00.\x80\x03}(".to_vec();
    for (i, oid) in refs.iter().enumerate() {
        p.push(b'K');
        p.push(u8::try_from(i).unwrap());
        p.extend_from_slice(b"C\x08");
        p.extend_from_slice(&oid.0.to_be_bytes());
        p.extend_from_slice(b"h\x00\x86Q");
    }
    p.extend_from_slice(b"u.");
    p
}

/// Make a store in `dir` whose root holds an object, then drops it. Return
/// the oid of the object.
fn unlinked_store(dir: &Path) -> zoe_core::Oid {
    use zoe_core::{Oid, Tid};

    let (root, object) = (Oid(0), Oid(1));
    let store = common::open_store(dir);
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(root, Tid::ZERO, &record(&[object])).unwrap();
    txn.store(object, Tid::ZERO, &record(&[])).unwrap();
    txn.vote().unwrap();
    let first = txn.finish().unwrap();
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(root, first, &record(&[])).unwrap();
    txn.vote().unwrap();
    txn.finish().unwrap();
    object
}

#[test]
fn pack_gc_on_the_command_line_wins_over_its_twin() {
    let dir = fresh_dir("pack-gc");
    let store = dir.join("zoe");
    let object = unlinked_store(&store);
    let path = store.to_str().unwrap();
    let pack = |flags: &[&str], twin: &str| {
        let mut args = vec!["pack", "--path", path, "--days", "0"];
        args.extend_from_slice(flags);
        let output = zoe(&args, &[("ZOE_PACK_GC", twin)]);
        assert_exit(&output, 0, &format!("{flags:?} with ZOE_PACK_GC={twin}"));
        String::from_utf8(output.stdout).unwrap()
    };
    let kept = || common::open_store(&store).load(object).is_ok();

    // A dry run reports the pack of its option, and removes nothing.
    let report = pack(&["--dry-run"], "false");
    assert!(
        report.contains("records  1 ") && report.contains("objects  0"),
        "{report}"
    );
    let report = pack(&["--dry-run", "--pack-gc"], "false");
    assert!(
        report.contains("records  2 ") && report.contains("objects  1"),
        "{report}"
    );
    assert!(kept());

    // Without garbage collection, the pack keeps the object.
    pack(&[], "false");
    assert!(kept());
    pack(&["--pack-gc=false"], "true");
    assert!(kept());
    // With it, the pack removes the object.
    pack(&["--pack-gc=true"], "false");
    assert!(!kept());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_source_that_is_not_a_data_fs_creates_no_store() {
    let dir = fresh_dir("not-data-fs");
    let not_data_fs = dir.join("not-a-data-fs");
    std::fs::write(&not_data_fs, b"this is not a FileStorage file").unwrap();
    let short = dir.join("short");
    std::fs::write(&short, b"FS").unwrap();
    for source in [
        &dir.to_path_buf(),
        &not_data_fs,
        &short,
        &dir.join("missing"),
    ] {
        let store = dir.join("zoe");
        let args = [
            "convert",
            "--from-data-fs",
            source.to_str().unwrap(),
            "--path",
            store.to_str().unwrap(),
        ];
        let output = zoe(&args, &[]);
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
    common::output(zoe_command(args, &[]).stdout(writer))
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
    let last = common::open_store(&store).last_transaction();
    assert_ne!(last, zoe_core::Tid::ZERO, "the conversion copied nothing");

    let output = run_with_closed_stdout(&["pack", "--path", path, "--days", "7"]);
    assert_exit(&output, 0, "pack with a closed stdout");
    let output = run_with_closed_stdout(&["--help"]);
    assert_exit(&output, 0, "help with a closed stdout");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_frame_limit_out_of_range_is_a_usage_error() {
    let dir = fresh_dir("frame-limit");
    let store = dir.join("zoe");
    let path = store.to_str().unwrap();
    for (option, twin) in [
        (Some("0"), None),
        (Some("4294967296"), None),
        (Some("16MiB"), None),
        (None, Some("0")),
    ] {
        let mut args = vec!["serve", "--path", path, "--listen", "127.0.0.1:0"];
        args.extend(option.iter().flat_map(|value| ["--max-frame-size", value]));
        let env: Vec<_> = twin
            .iter()
            .map(|value| ("ZOE_MAX_FRAME_SIZE", *value))
            .collect();
        let output = zoe(&args, &env);
        assert_exit(&output, 2, &format!("{option:?} {twin:?}"));
        assert!(!store.exists(), "a usage error created the store");
    }
    let help = zoe(&["serve", "--help"], &[]);
    assert_exit(&help, 0, "serve --help");
    let help = String::from_utf8(help.stdout).unwrap();
    assert!(help.contains("--max-frame-size"), "{help}");
    assert!(help.contains("ZOE_MAX_FRAME_SIZE"), "{help}");
    let _ = std::fs::remove_dir_all(&dir);
}
