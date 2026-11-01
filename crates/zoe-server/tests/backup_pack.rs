//! Acceptance test of `zoe backup` and `zoe verify --backup` after a pack
//! of the store (N15).
//!
//! A pack at the last tid of the dumps removes only history that the dumps
//! hold, and it does not change the transactions after that tid. So the
//! chain stays complete: `zoe backup` continues it, and
//! `zoe verify --backup` finds no difference.
//!
//! A pack after the last tid of the dumps changes history that the next
//! dump must hold. Then `zoe backup` refuses, writes nothing and exits
//! with 1, and `zoe verify --backup` reports a difference.
//!
//! `zoe pack --days 0` packs at the last tid of the store, because a pack
//! never goes past the last tid. With no commit after the backup, that is
//! the last tid of the dumps.

use std::path::Path;
use std::process::Output;

use serde_json::Value;
use zoe_core::{Oid, Store, Tid};

mod common;

use common::{assert_exit, fresh_dir, record, s};

/// The root object. A pack keeps what the root reaches.
const ROOT: Oid = Oid(0);

/// Run `zoe` with `args`, without the `ZOE_*` variables of the test
/// process. The temporary directory of the child is `scratch`.
fn zoe(args: &[&str], scratch: &Path) -> Output {
    let mut command = common::zoe_command(args, &[]);
    command.env("TMPDIR", scratch);
    common::output(&mut command)
}

/// Commit one transaction that stores each record of `writes` over its
/// serial. Return the tid of the transaction.
fn commit(store: &Store, writes: &[(Oid, Tid, Vec<u8>)]) -> Tid {
    let mut txn = store.tpc_begin(b"user", b"", b"").unwrap();
    for (oid, serial, data) in writes {
        txn.store(*oid, *serial, data).unwrap();
    }
    txn.vote().unwrap();
    txn.finish().unwrap()
}

/// The names of the entries of the backup directory, in order.
fn entries(dumps: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dumps)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// The pack watermark and the last tid of the store in `path`.
fn watermark_and_last(path: &Path) -> (Tid, Tid) {
    let store = common::open_store(path);
    (store.pack_watermark().unwrap(), store.last_transaction())
}

/// The JSON report of `zoe verify --backup`.
fn report(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "{error}\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

#[test]
fn a_pack_at_the_last_dumped_tid_lets_the_chain_continue() {
    let dir = fresh_dir("chain");
    let (source, dumps, scratch) = (dir.join("source"), dir.join("dumps"), dir.join("scratch"));
    std::fs::create_dir_all(&scratch).unwrap();
    let backup = || {
        zoe(
            &["backup", "--path", s(&source), "--to", s(&dumps)],
            &scratch,
        )
    };
    let verify = || {
        zoe(
            &[
                "verify",
                "--path",
                s(&source),
                "--backup",
                s(&dumps),
                "--format",
                "json",
            ],
            &scratch,
        )
    };
    let pack = || zoe(&["pack", "--path", s(&source), "--days", "0"], &scratch);

    // The root changes twice, so a pack removes its first record. The
    // root does not reach the other object, so a pack removes that object.
    let (first, dumped) = {
        let store = common::open_store(&source);
        let first = commit(&store, &[(ROOT, Tid(0), record(b"1"))]);
        let other = store.new_oid().unwrap();
        commit(&store, &[(other, Tid(0), record(b"other"))]);
        let dumped = commit(&store, &[(ROOT, first, record(b"2"))]);
        (first, dumped)
    };
    assert_exit(&backup(), 0, "full dump");
    assert_eq!(entries(&dumps).len(), 1, "{:?}", entries(&dumps));

    // A pack with no commit after the backup.
    assert_exit(&pack(), 0, "pack at the last dumped tid");
    assert_eq!(
        watermark_and_last(&source),
        (dumped, dumped),
        "the pack is at the last tid of the dumps"
    );
    {
        let store = common::open_store(&source);
        assert!(
            store.load_serial(ROOT, first).is_err(),
            "the pack removed the first record of the root"
        );
    }

    // The dumps end at the last tid of the store: nothing new.
    let output = backup();
    assert_exit(&output, 0, "backup with nothing new after the pack");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.starts_with("Nothing new"), "{stdout}");
    assert_eq!(entries(&dumps).len(), 1, "{:?}", entries(&dumps));

    // The pack removed only history that the dumps hold.
    let output = verify();
    assert_exit(&output, 0, "verify after a pack at the last dumped tid");
    let check = report(&output);
    assert_eq!(check["differences"], Value::Array(Vec::new()), "{check}");
    assert_eq!(check["last_tid"].as_u64(), Some(dumped.0), "{check}");

    // A commit after the pack: the next dump continues the chain.
    let next = {
        let store = common::open_store(&source);
        commit(&store, &[(ROOT, dumped, record(b"3"))])
    };
    let output = backup();
    assert_exit(
        &output,
        0,
        "incremental dump after a pack at the last dumped tid",
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("(incremental)"), "{stdout}");
    assert_eq!(entries(&dumps).len(), 2, "{:?}", entries(&dumps));

    let output = verify();
    assert_exit(&output, 0, "verify after the incremental dump");
    let check = report(&output);
    assert_eq!(check["differences"], Value::Array(Vec::new()), "{check}");
    assert_eq!(check["last_tid"].as_u64(), Some(next.0), "{check}");
    assert_eq!(check["dumps"].as_u64(), Some(2), "{check}");

    // A commit, then a pack after the last tid of the dumps: the backup
    // refuses and writes nothing, and the check reports a difference.
    let later = {
        let store = common::open_store(&source);
        let other = store.new_oid().unwrap();
        commit(&store, &[(other, Tid(0), record(b"4"))])
    };
    assert_exit(&pack(), 0, "pack after the last dumped tid");
    assert_eq!(watermark_and_last(&source), (later, later));
    let before = entries(&dumps);

    let output = backup();
    assert_exit(&output, 1, "backup after a pack after the last dumped tid");
    let stderr = String::from_utf8_lossy(&output.stderr);
    let packed = format!("the store was packed at tid {:#018x}", later.0);
    assert!(stderr.contains(&packed), "{stderr}");
    assert!(
        stderr.contains("write a new full dump into a new directory"),
        "{stderr}"
    );
    assert_eq!(entries(&dumps), before);

    let output = verify();
    assert_exit(&output, 1, "verify after a pack after the last dumped tid");
    let check = report(&output);
    let differences = check["differences"].as_array().unwrap();
    assert!(
        differences
            .iter()
            .any(|line| line.as_str().is_some_and(|line| line.starts_with(&packed))),
        "{check}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
