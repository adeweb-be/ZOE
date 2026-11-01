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
//! store after the uninstall converts. The guard matches by class, so the
//! record of a deleted principal, which no object references, refuses
//! until `zoe pack` removes it. A store with a moved registry too gets
//! both refusals in one message.
//!
//! The test writes the records directly. It needs no Python and no Plone.

use std::path::Path;
use std::process::Output;

use zoe_core::{Flavour, Oid, Store, Tid};

mod common;

use common::{assert_exit, fresh_dir, s, zoe};

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

/// Run `zoe convert --to-data-fs` of the store `store` into `target`, with
/// the arguments `extra` and the variables `env`. No other `ZOE_*`
/// variable reaches it.
fn convert(store: &Path, target: &Path, extra: &[&str], env: &[(&str, &str)]) -> Output {
    let (data_fs, blobs) = (target.join("Data.fs"), target.join("blobs"));
    let mut args: Vec<String> = [
        "convert",
        "--to-data-fs",
        s(&data_fs),
        "--data-fs-blob-dir",
        s(&blobs),
        "--path",
        s(store),
    ]
    .map(String::from)
    .to_vec();
    args.extend(extra.iter().map(|arg| (*arg).to_owned()));
    zoe(&args, env)
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
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
    let store = common::open_store(dir);
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

/// Return a pickle of a root `PersistentMapping` that refers to `oid`.
fn root_referring_to(oid: Oid) -> Vec<u8> {
    let mut data = b"\x80\x03cpersistent.mapping\nPersistentMapping\nq\x00.".to_vec();
    data.extend_from_slice(b"\x80\x03}X\x04\x00\x00\x00data}X\x01\x00\x00\x00s(C\x08");
    data.extend_from_slice(&oid.0.to_be_bytes());
    data.extend_from_slice(b"cBTrees.OOBTree\nOOBTree\ntQss.");
    data
}

#[test]
fn the_record_of_a_deleted_principal_refuses_until_a_pack() {
    let dir = fresh_dir("pack");
    let store_dir = dir.join("zoe");
    // The tree of the sheet holds dicts after the uninstall. The record of
    // a principal that the site deleted before stays, with no reference.
    let sheet = Oid(0x10);
    {
        let store = common::open_store(&store_dir);
        commit(
            &store,
            &[
                (Oid(0), root_referring_to(sheet)),
                (sheet, pickle("BTrees.OOBTree.OOBTree", b"N")),
                (MEMBER, pickle(PROPERTIES, LIVE)),
            ],
        );
    }
    let target = dir.join("before");
    std::fs::create_dir_all(&target).unwrap();
    let output = convert(&store_dir, &target, &[], &[]);
    assert_exit(&output, 1, "convert a store with an unreachable record");
    let message = stderr(&output);
    assert!(
        message.contains(&format!("{PROPERTIES}: 1 record (0x000000000000002a)"))
            && message.contains("zoe pack"),
        "{message}"
    );

    // A pack as of now, with its GC, removes the record.
    let pack = zoe(
        &["pack", "--path", s(&store_dir), "--days", "0"].map(String::from),
        &[],
    );
    assert_exit(&pack, 0, "zoe pack");
    let target = dir.join("after");
    std::fs::create_dir_all(&target).unwrap();
    let output = convert(&store_dir, &target, &[], &[]);
    assert_exit(&output, 0, "convert after the pack");
    assert!(stderr(&output).is_empty(), "{}", stderr(&output));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_moved_registry_and_records_of_the_classes_refuse_in_one_message() {
    let dir = fresh_dir("both");
    let store_dir = dir.join("zoe");
    {
        let store = store_with_plone_classes(&store_dir);
        let mut marker = vec![1];
        marker.extend_from_slice(&0x2c4u64.to_be_bytes());
        marker.extend_from_slice(&1u64.to_be_bytes());
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.kv_set(
            Flavour::Durable,
            "zoe.plone.registry:0123456789abcdef0123456789abcdef",
            b"m",
            &marker,
            None,
        )
        .unwrap();
        txn.vote().unwrap();
        txn.finish().unwrap();
    }
    let target = dir.join("target");
    std::fs::create_dir_all(&target).unwrap();
    let output = convert(&store_dir, &target, &[], &[]);
    assert_exit(&output, 1, "convert a store with both");
    let message = stderr(&output);
    // The refusal of the registry comes first, then the one of the
    // classes, each with its way out.
    let registry = message.find("move_back").expect("no registry refusal");
    let classes = message.find(PROPERTIES).expect("no class refusal");
    assert!(message.starts_with("zoe convert:"), "{message}");
    assert!(registry < classes, "{message}");
    assert!(message.contains("0x00000000000002c4"), "{message}");
    assert!(message.contains("zoe.plone:uninstall"), "{message}");
    assert!(
        data_fs_files(&target).is_empty(),
        "{:?}",
        data_fs_files(&target)
    );
    let _ = std::fs::remove_dir_all(&dir);
}
