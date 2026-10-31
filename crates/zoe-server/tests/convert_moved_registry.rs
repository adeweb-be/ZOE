//! Acceptance test of the guard of `zoe convert --to-data-fs` against a
//! moved registry (F23, ADR 0050).
//!
//! The registry backend of `zoe.plone` keeps the records of a moved
//! registry in the durable namespace `zoe.plone.registry:<id>`, with a
//! marker row `m`: the version of the layout (1 byte, 1), the oid of the
//! registry (8 bytes, big-endian) and the write counter (8 bytes,
//! big-endian). A `Data.fs` holds no namespace, so the registry of a copy
//! raises `Unsupported` on each request.
//!
//! So `zoe convert --to-data-fs` refuses a store that holds such a marker.
//! It exits with 1, writes nothing, and its message names each registry by
//! its oid and tells the way out: `move_back`, a commit, then the
//! conversion. The option `--force` (twin `ZOE_FORCE`) converts anyway and
//! writes a warning that names each registry. A store without a marker of
//! a moved registry converts as before.
//!
//! The test writes the rows directly with `Txn::kv_set`, as the registry
//! backend writes them. It needs no Python and no Plone.
//!
//! The message names an oid as the other messages of the command do,
//! `0x000000000000002a`, or as ZODB's `oid_repr` does, `0x2a`. The test
//! takes both forms.

use std::path::Path;
use std::process::Output;

use zoe_core::{Flavour, Oid, Store, Tid};

mod common;

use common::{assert_exit, fresh_dir, s, zoe};

/// The prefix of the namespace of a moved registry.
const PREFIX: &str = "zoe.plone.registry:";

/// The key of the marker row.
const MARKER: &[u8] = b"m";

/// The oids of the registries of two sites in one store.
const FIRST: Oid = Oid(0x2a);
const SECOND: Oid = Oid(0x2c4);

/// The ids of their namespaces: 32 lowercase hex digits each.
const FIRST_ID: &str = "0123456789abcdef0123456789abcdef";
const SECOND_ID: &str = "fedcba9876543210fedcba9876543210";

/// A durable row to write: the name of the namespace, the key, and the
/// value. `None` deletes the key.
type Row<'a> = (&'a str, &'a [u8], Option<&'a [u8]>);

/// A way to give an option: its name in the messages, the arguments and
/// the variables.
type Case<'a> = (&'a str, &'a [&'a str], &'a [(&'a str, &'a str)]);

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Tell if `text` names `oid` in one of the two forms of the module
/// documentation.
fn names_oid(text: &str, oid: Oid) -> bool {
    text.contains(&format!("{:#018x}", oid.0)) || text.contains(&format!("{:#x}", oid.0))
}

/// Return the marker of the registry `oid` at the write `counter`, as the
/// registry backend encodes it.
fn marker(oid: Oid, counter: u64) -> Vec<u8> {
    let mut data = vec![1];
    data.extend_from_slice(&oid.0.to_be_bytes());
    data.extend_from_slice(&counter.to_be_bytes());
    data
}

/// Return a small pickle of an object of the class `module.name`.
fn pickle(module: &str, name: &str) -> Vec<u8> {
    let mut data = b"\x80\x03c".to_vec();
    data.extend_from_slice(format!("{module}\n{name}\n").as_bytes());
    data.extend_from_slice(b"q\x00.\x80\x03}.");
    data
}

/// Commit one transaction to `store` with the records `records` and the
/// durable rows `rows`.
fn commit(store: &Store, records: &[(Oid, Vec<u8>)], rows: &[Row]) {
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    for (oid, data) in records {
        let serial = store.load(*oid).map_or(Tid::ZERO, |(_, serial)| serial);
        txn.store(*oid, serial, data).unwrap();
    }
    for (name, key, value) in rows {
        match value {
            Some(value) => txn
                .kv_set(Flavour::Durable, name, key, value, None)
                .unwrap(),
            None => txn.kv_delete(Flavour::Durable, name, key).unwrap(),
        }
    }
    txn.vote().unwrap();
    txn.finish().unwrap();
}

/// The records of a root and of a registry at each oid of `registries`.
fn records(registries: &[Oid]) -> Vec<(Oid, Vec<u8>)> {
    let mut records = vec![(Oid(0), pickle("persistent.mapping", "PersistentMapping"))];
    for oid in registries {
        records.push((*oid, pickle("plone.registry.registry", "Registry")));
    }
    records
}

/// Make a store in `dir` with the registries of two sites, both moved: the
/// rows of a field and a value, and the marker, in the namespace of each.
fn store_with_moved_registries(dir: &Path) {
    let store = common::open_store(dir);
    let (first, second) = (
        format!("{PREFIX}{FIRST_ID}"),
        format!("{PREFIX}{SECOND_ID}"),
    );
    let (first_marker, second_marker) = (marker(FIRST, 1), marker(SECOND, 3));
    let field = pickle("plone.registry.field", "TextLine");
    let rows: Vec<Row> = vec![
        (&first, b"fplone.site_title", Some(&field)),
        (
            &first,
            b"vplone.site_title",
            Some(b"\x80\x03X\x04\x00\x00\x00Site."),
        ),
        (&first, MARKER, Some(&first_marker)),
        (&second, b"fplone.site_title", Some(&field)),
        (
            &second,
            b"vplone.site_title",
            Some(b"\x80\x03X\x04\x00\x00\x00Site."),
        ),
        (&second, MARKER, Some(&second_marker)),
    ];
    commit(&store, &records(&[FIRST, SECOND]), &rows);
}

/// The arguments of a conversion of the store `store` into `dir`.
fn to_data_fs<'a>(store: &'a Path, dir: &'a Path, extra: &[&'a str]) -> Vec<String> {
    let mut args: Vec<String> = [
        "convert",
        "--to-data-fs",
        s(&dir.join("Data.fs")),
        "--data-fs-blob-dir",
        s(&dir.join("blobs")),
        "--path",
        s(store),
    ]
    .iter()
    .map(|arg| (*arg).to_owned())
    .collect();
    args.extend(extra.iter().map(|arg| (*arg).to_owned()));
    args
}

fn run(args: &[String], env: &[(&str, &str)]) -> Output {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    zoe(&args, env)
}

/// The names of the entries of `dir` that start with `Data.fs`: the
/// target, its temporary file and its lock file.
fn data_fs_files(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("Data.fs"))
        .collect();
    names.sort();
    names
}

#[test]
fn a_store_with_a_moved_registry_does_not_convert_to_a_data_fs() {
    let dir = fresh_dir("refuses");
    let store = dir.join("zoe");
    store_with_moved_registries(&store);
    let target = dir.join("target");
    std::fs::create_dir_all(&target).unwrap();
    let before = std::fs::read(store.join("zoe.redb")).unwrap();

    let output = run(&to_data_fs(&store, &target, &[]), &[]);

    // A refusal is a failure of the command, not a usage error.
    assert_exit(&output, 1, "convert a store with moved registries");
    let message = stderr(&output);
    assert!(
        message.starts_with("zoe convert:"),
        "the message does not start with the subcommand:\n{message}"
    );
    for oid in [FIRST, SECOND] {
        assert!(
            names_oid(&message, oid),
            "the message does not name the registry {oid:?}:\n{message}"
        );
    }
    assert!(
        message.contains("move_back"),
        "the message does not name move_back:\n{message}"
    );
    assert!(
        message.contains("--force"),
        "the message does not name --force:\n{message}"
    );
    assert!(
        data_fs_files(&target).is_empty(),
        "the refusal wrote {:?}",
        data_fs_files(&target)
    );
    assert_eq!(
        std::fs::read(store.join("zoe.redb")).unwrap(),
        before,
        "the refusal changed the store"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn force_converts_a_store_with_a_moved_registry_and_warns() {
    let dir = fresh_dir("force");
    let store = dir.join("zoe");
    store_with_moved_registries(&store);

    let cases: [Case; 2] = [
        ("--force", &["--force"], &[]),
        ("ZOE_FORCE=true", &[], &[("ZOE_FORCE", "true")]),
    ];
    for (what, extra, env) in cases {
        let target = dir.join(what.replace(['=', '-'], ""));
        std::fs::create_dir_all(&target).unwrap();
        let output = run(&to_data_fs(&store, &target, extra), env);
        assert_exit(&output, 0, &format!("convert with {what}"));
        assert!(
            stdout(&output).starts_with("Converted the store"),
            "{what}: no report:\n{}",
            stdout(&output)
        );
        assert!(target.join("Data.fs").is_file(), "{what}: no Data.fs");
        let warning = stderr(&output);
        assert!(
            warning.contains("warning"),
            "{what}: no warning:\n{warning}"
        );
        for oid in [FIRST, SECOND] {
            assert!(
                names_oid(&warning, oid),
                "{what}: the warning does not name the registry {oid:?}:\n{warning}"
            );
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_help_of_convert_names_force_and_its_twin() {
    let output = zoe(&["convert", "--help"], &[]);
    assert_exit(&output, 0, "zoe convert --help");
    let help = stdout(&output);
    assert!(help.contains("--force"), "no --force in:\n{help}");
    assert!(help.contains("ZOE_FORCE"), "no ZOE_FORCE in:\n{help}");
}

#[test]
fn a_store_without_a_moved_registry_converts_as_before() {
    let dir = fresh_dir("converts");
    let moved_back = format!("{PREFIX}{FIRST_ID}");
    let first_marker = marker(FIRST, 1);
    let field = pickle("plone.registry.field", "TextLine");

    // A store with no namespace.
    let plain = dir.join("plain");
    commit(&common::open_store(&plain), &records(&[FIRST]), &[]);

    // A store whose only durable namespace is not a registry, with a key
    // `m` too.
    let other = dir.join("other");
    commit(
        &common::open_store(&other),
        &records(&[FIRST]),
        &[("app.settings", MARKER, Some(b"1"))],
    );

    // A store whose registry was moved, then moved back: `move_back`
    // deletes each row, the marker too, and leaves the generation row.
    let back = dir.join("back");
    {
        let store = common::open_store(&back);
        commit(
            &store,
            &records(&[FIRST]),
            &[
                (&moved_back, b"fplone.site_title", Some(&field)),
                (&moved_back, MARKER, Some(&first_marker)),
            ],
        );
        commit(
            &store,
            &[(FIRST, pickle("plone.registry.registry", "Registry"))],
            &[
                (&moved_back, b"fplone.site_title", None),
                (&moved_back, MARKER, None),
            ],
        );
    }

    for (name, store, left_out) in [
        ("plain", &plain, 0),
        ("other", &other, 1),
        ("back", &back, 0),
    ] {
        let target = dir.join(format!("{name}-target"));
        std::fs::create_dir_all(&target).unwrap();
        let output = run(&to_data_fs(store, &target, &[]), &[]);
        assert_exit(&output, 0, &format!("convert the store {name}"));
        assert!(target.join("Data.fs").is_file(), "{name}: no Data.fs");
        let report = stdout(&output);
        assert!(
            report.starts_with("Converted the store"),
            "{name}: no report:\n{report}"
        );
        let messages = stderr(&output);
        assert!(
            !messages.contains("registry") && !messages.contains("--force"),
            "{name}: a message about a registry:\n{messages}"
        );
        if left_out > 0 {
            // The warning of the left-out keys stays as it is.
            assert!(
                messages.contains(&format!("leaves out {left_out} keys")),
                "{name}: no warning of the left-out keys:\n{messages}"
            );
        } else {
            assert!(messages.is_empty(), "{name}: a message:\n{messages}");
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}
