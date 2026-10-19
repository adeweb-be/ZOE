//! Test of the guard of `zoe convert --to-data-fs` against the records of
//! the classes of `zoe.plone` (ADR 0046, section 9).
//!
//! The profile `zoe.plone:default` keeps the properties of each member and
//! group of a site in a record of `zoe.plone.properties.PrincipalProperties`,
//! and the behaviour `zoe.plone.history` keeps a workflow history in a
//! record of `zoe.plone.history.WorkflowHistory`. A client without
//! `zoedb` loads these records as broken objects. So `zoe convert
//! --to-data-fs` refuses a store with a current record of one of these
//! classes: it exits with 1, writes nothing, and its message names each
//! class, its first oids and the way back, the profile
//! `zoe.plone:uninstall`. `--force` (twin `ZOE_FORCE`) converts anyway and
//! warns. The uninstall gives the stock values back and retires each
//! record of the classes: it writes the state `{0: None}` or `{0: ()}`. A
//! store after the uninstall converts.
//!
//! The test writes the records directly. It needs no Python and no Plone.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use zoe_core::{Oid, Store, Tid};

mod common;

/// The binary under test.
const ZOE: &str = env!("CARGO_BIN_EXE_zoe");

/// A way to give `--force`: a name, the arguments and the variables.
type Case<'a> = (&'a str, &'a [&'a str], &'a [(&'a str, &'a str)]);

/// The class of the properties of a member or a group.
const PROPERTIES: &str = "zoe.plone.properties.PrincipalProperties";

/// The class of a workflow history.
const HISTORY: &str = "zoe.plone.history.WorkflowHistory";

/// The oids of two principals and of one history.
const MEMBER: Oid = Oid(0x2a);
const GROUP: Oid = Oid(0x2b);
const WORKFLOW: Oid = Oid(0x40);

fn fresh_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "zoe-convert-plone-classes-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn s(path: &Path) -> &str {
    path.to_str().unwrap()
}

/// Run `zoe convert --to-data-fs` of the store `store` into `target`, with
/// the arguments `extra` and the variables `env`. No other `ZOE_*`
/// variable reaches it.
fn convert(store: &Path, target: &Path, extra: &[&str], env: &[(&str, &str)]) -> Output {
    let mut command = Command::new(ZOE);
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("ZOE_") {
            command.env_remove(name);
        }
    }
    command
        .args([
            "convert",
            "--to-data-fs",
            s(&target.join("Data.fs")),
            "--data-fs-blob-dir",
            s(&target.join("blobs")),
            "--path",
            s(store),
        ])
        .args(extra)
        .envs(env.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    common::output(&mut command)
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn assert_exit(output: &Output, code: i32, what: &str) {
    assert_eq!(
        output.status.code(),
        Some(code),
        "{what}\nstdout:\n{}\nstderr:\n{}",
        stdout(output),
        stderr(output)
    );
}

/// Return a pickle of an object of the class `class`, whose state is the
/// pickle `state` of a dict.
fn pickle(class: &str, state: &[u8]) -> Vec<u8> {
    let (module, name) = class.rsplit_once('.').unwrap();
    let mut data = b"\x80\x03c".to_vec();
    data.extend_from_slice(format!("{module}\n{name}\n").as_bytes());
    data.extend_from_slice(b"q\x00.\x80\x03");
    data.extend_from_slice(state);
    data.push(b'.');
    data
}

/// The state `{"fullname": "x"}`, with a memo put, as ZODB writes it.
const LIVE: &[u8] = b"}q\x01X\x08\x00\x00\x00fullnameq\x02X\x01\x00\x00\x00xq\x03s";
/// The state of a retired record of `PrincipalProperties`: `{0: None}`.
const RETIRED_PROPERTIES: &[u8] = b"}q\x01K\x00Ns";
/// The state of a retired `WorkflowHistory`: `{0: ()}`.
const RETIRED_HISTORY: &[u8] = b"}q\x01K\x00)s";

/// Commit one transaction to `store` with `records`.
fn commit(store: &Store, records: &[(Oid, Vec<u8>)]) {
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    for (oid, data) in records {
        let serial = store.load(*oid).map_or(Tid::ZERO, |(_, serial)| serial);
        txn.store(*oid, serial, data).unwrap();
    }
    txn.vote().unwrap();
    txn.finish().unwrap();
}

/// Make a store in `dir` with a root, two principals and one history,
/// each in a record of its class of `zoe.plone`.
fn store_with_plone_classes(dir: &Path) -> Store {
    let store = common::open(dir);
    commit(
        &store,
        &[
            (Oid(0), pickle("persistent.mapping.PersistentMapping", b"}")),
            (MEMBER, pickle(PROPERTIES, LIVE)),
            (GROUP, pickle(PROPERTIES, LIVE)),
            (WORKFLOW, pickle(HISTORY, b"}")),
        ],
    );
    store
}

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
fn a_store_with_records_of_the_classes_of_zoe_plone_does_not_convert() {
    let dir = fresh_dir("refuses");
    let store = dir.join("zoe");
    drop(store_with_plone_classes(&store));
    let target = dir.join("target");
    std::fs::create_dir_all(&target).unwrap();
    let before = std::fs::read(store.join("zoe.redb")).unwrap();

    let output = convert(&store, &target, &[], &[]);

    assert_exit(&output, 1, "convert a store with records of zoe.plone");
    let message = stderr(&output);
    assert!(message.starts_with("zoe convert:"), "{message}");
    for text in [
        "holds 3 records",
        &format!("{PROPERTIES}: 2 records (0x000000000000002a, 0x000000000000002b)"),
        &format!("{HISTORY}: 1 record (0x0000000000000040)"),
        "zoe.plone:uninstall",
        "--force",
    ] {
        assert!(message.contains(text), "no {text:?} in:\n{message}");
    }
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
fn force_converts_a_store_with_records_of_zoe_plone_and_warns() {
    let dir = fresh_dir("force");
    let store = dir.join("zoe");
    drop(store_with_plone_classes(&store));
    let cases: [Case; 2] = [
        ("flag", &["--force"], &[]),
        ("twin", &[], &[("ZOE_FORCE", "true")]),
    ];
    for (what, extra, env) in cases {
        let target = dir.join(what);
        std::fs::create_dir_all(&target).unwrap();
        let output = convert(&store, &target, extra, env);
        assert_exit(&output, 0, &format!("convert with --force ({what})"));
        assert!(target.join("Data.fs").is_file(), "{what}: no Data.fs");
        let report = stdout(&output);
        assert!(
            report.contains("zoe.plone     3 records of its classes, converted with --force"),
            "{what}: no line in the report:\n{report}"
        );
        let warning = stderr(&output);
        assert!(
            warning.contains("warning: --force:")
                && warning.contains(&format!("{PROPERTIES} (2 records)"))
                && warning.contains(&format!("{HISTORY} (1 record)")),
            "{what}: no warning:\n{warning}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_store_after_the_uninstall_converts() {
    let dir = fresh_dir("uninstalled");
    let store_dir = dir.join("zoe");
    {
        let store = store_with_plone_classes(&store_dir);
        // The uninstall: the stock values in other records, and each
        // record of the classes retired.
        commit(
            &store,
            &[
                (MEMBER, pickle(PROPERTIES, RETIRED_PROPERTIES)),
                (GROUP, pickle(PROPERTIES, RETIRED_PROPERTIES)),
                (WORKFLOW, pickle(HISTORY, RETIRED_HISTORY)),
                (
                    Oid(0x41),
                    pickle("Persistence.mapping.PersistentMapping", LIVE),
                ),
            ],
        );
    }
    let target = dir.join("target");
    std::fs::create_dir_all(&target).unwrap();
    let output = convert(&store_dir, &target, &[], &[]);
    assert_exit(&output, 0, "convert a store after the uninstall");
    assert!(target.join("Data.fs").is_file(), "no Data.fs");
    let messages = stderr(&output);
    assert!(messages.is_empty(), "a message:\n{messages}");
    assert!(
        !stdout(&output).contains("zoe.plone"),
        "{}",
        stdout(&output)
    );
    let _ = std::fs::remove_dir_all(&dir);
}
