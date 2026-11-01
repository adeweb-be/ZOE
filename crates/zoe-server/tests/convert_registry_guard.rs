//! Tests of the guard of `zoe convert --to-data-fs` against a moved
//! registry (F23, ADR 0050), beyond its acceptance test
//! `convert_moved_registry.rs`: a marker of another layout, `--force=false`
//! against its twin, and `--force` with `--from-data-fs`.

use std::path::{Path, PathBuf};
use std::process::Output;

use zoe_core::{Flavour, Oid};

mod common;

use common::{assert_exit, fresh_dir, s, zoe};

/// The namespace of a moved registry.
const NAMESPACE: &str = "zoe.plone.registry:0123456789abcdef0123456789abcdef";

/// The `Data.fs` fixture of the zoe-convert tests.
fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../zoe-convert/tests/data-fs")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Make a store in `dir` with a root and the marker `marker` in the
/// namespace of a registry.
fn store_with_marker(dir: &Path, marker: &[u8]) {
    let store = common::open_store(dir);
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    let root = b"\x80\x03cpersistent.mapping\nPersistentMapping\nq\x00.\x80\x03}.";
    txn.store(Oid(0), zoe_core::Tid::ZERO, root).unwrap();
    txn.kv_set(Flavour::Durable, NAMESPACE, b"m", marker, None)
        .unwrap();
    txn.vote().unwrap();
    txn.finish().unwrap();
}

/// The arguments of a conversion of the store `store` into `dir`.
fn to_data_fs(store: &Path, dir: &Path, extra: &[&str]) -> Vec<String> {
    let data_fs = dir.join("Data.fs");
    let blobs = dir.join("blobs");
    let mut args = vec![
        "convert",
        "--to-data-fs",
        s(&data_fs),
        "--data-fs-blob-dir",
        s(&blobs),
        "--path",
        s(store),
    ];
    args.extend_from_slice(extra);
    args.iter().map(|arg| (*arg).to_owned()).collect()
}

fn run(args: &[String], env: &[(&str, &str)]) -> Output {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    zoe(&args, env)
}

#[test]
fn a_marker_of_another_layout_refuses_and_names_its_namespace() {
    let dir = fresh_dir("layout");
    let store = dir.join("zoe");
    // A layout that this version does not decode.
    store_with_marker(&store, &[2, 0, 0, 0, 0, 0, 0, 0, 0x2a]);
    let target = dir.join("target");
    std::fs::create_dir_all(&target).unwrap();

    let output = run(&to_data_fs(&store, &target, &[]), &[]);
    assert_exit(
        &output,
        1,
        "convert a store with a marker of another layout",
    );
    let message = stderr(&output);
    assert!(message.starts_with("zoe convert:"), "{message}");
    assert!(message.contains(NAMESPACE), "{message}");
    assert!(message.contains("another layout"), "{message}");
    assert!(
        !target.join("Data.fs").exists(),
        "the refusal wrote a Data.fs"
    );

    let output = run(&to_data_fs(&store, &target, &["--force"]), &[]);
    assert_exit(&output, 0, "force the conversion");
    assert!(stderr(&output).contains(NAMESPACE), "{}", stderr(&output));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn force_false_on_the_command_line_wins_over_its_twin() {
    let dir = fresh_dir("twin");
    let store = dir.join("zoe");
    let mut marker = vec![1];
    marker.extend_from_slice(&0x2au64.to_be_bytes());
    marker.extend_from_slice(&1u64.to_be_bytes());
    store_with_marker(&store, &marker);
    let target = dir.join("target");
    std::fs::create_dir_all(&target).unwrap();

    let args = to_data_fs(&store, &target, &["--force=false"]);
    let output = run(&args, &[("ZOE_FORCE", "true")]);
    assert_exit(&output, 1, "--force=false with ZOE_FORCE=true");
    assert!(stderr(&output).contains("0x000000000000002a"));
    assert!(
        !target.join("Data.fs").exists(),
        "the refusal wrote a Data.fs"
    );

    // The forced report names the registries too.
    let output = run(&to_data_fs(&store, &target, &["--force=true"]), &[]);
    assert_exit(&output, 0, "--force=true");
    let report = stdout(&output);
    assert!(report.contains("registries    1 moved"), "{report}");
    assert!(report.contains("0x000000000000002a"), "{report}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn force_has_no_effect_with_from_data_fs() {
    let dir = fresh_dir("from");
    let store = dir.join("zoe");
    let (source, blobs) = (fixture().join("Data.fs"), fixture().join("blobs-bushy"));
    let args = [
        "convert",
        "--from-data-fs",
        s(&source),
        "--data-fs-blob-dir",
        s(&blobs),
        "--path",
        s(&store),
        "--force",
    ];
    let output = zoe(&args, &[]);
    assert_exit(&output, 0, "convert from a Data.fs with --force");
    assert!(stderr(&output).is_empty(), "{}", stderr(&output));
    let _ = std::fs::remove_dir_all(&dir);
}
