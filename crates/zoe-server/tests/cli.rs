//! Acceptance test of the `zoe` command and its subcommands `convert` and
//! `pack`. `zoe convert` turns a `Data.fs` into a store and a store back
//! into a `Data.fs`. `zoe pack` packs a store as of a number of days ago, or
//! reports what that pack would remove. Every option has a `ZOE_*`
//! environment twin, and an option on the command line wins over its
//! twin. The command exits with 0 on success, with 1 when it fails, and
//! with 2 on a usage error.
//!
//! The binary `zoe` and the library entry point `zoe_server::cli::main`
//! run the same code. The console script of the `zoedb` wheel calls that
//! entry point: `python/zoedb/tests/acceptance/test_console_script.py`
//! tests it.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::{Duration, SystemTime};

use zoe_convert::from_datafs;
use zoe_core::{Oid, Store, Tid, ZoeError, timestamp};

mod common;

use common::{assert_exit, fresh_dir, s, zoe};

/// The subcommands that this test covers, as the naming table of the
/// specification names them.
const SUBCOMMANDS: [&str; 2] = ["convert", "pack"];

/// The `Data.fs` fixture of the zoe-convert tests.
fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../zoe-convert/tests/data-fs")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// One record of a store: the oid, the pickle and the blob.
type Record = (Oid, Option<Vec<u8>>, Option<Vec<u8>>);

/// One transaction of a store: the tid, the user, the description, the
/// extension and the records by oid.
type Txn = (Tid, Vec<u8>, Vec<u8>, Vec<u8>, Vec<Record>);

/// Every transaction of the store in `dir`, with its records and blobs.
fn contents(dir: &Path) -> Vec<Txn> {
    let store = common::open_store(dir);
    store
        .iterator(None, None)
        .unwrap()
        .map(|txn| {
            let txn = txn.unwrap();
            let mut records: Vec<Record> = txn
                .records
                .iter()
                .map(|record| {
                    let blob = store.open_blob(record.oid, txn.tid).ok().map(|mut reader| {
                        let mut bytes = Vec::new();
                        std::io::Read::read_to_end(&mut reader, &mut bytes).unwrap();
                        bytes
                    });
                    (record.oid, record.data.clone(), blob)
                })
                .collect();
            records.sort();
            (
                txn.tid,
                txn.user.clone(),
                txn.description.clone(),
                txn.extension.clone(),
                records,
            )
        })
        .collect()
}

/// What the library converter writes from the fixture.
fn converted_fixture(dir: &Path) -> Vec<Txn> {
    {
        let store = common::open_store(dir);
        let report = from_datafs(
            &fixture().join("Data.fs"),
            Some(&fixture().join("blobs-bushy")),
            &store,
        )
        .unwrap();
        assert!(report.problems.is_empty(), "{report:?}");
    }
    contents(dir)
}

#[test]
fn help_lists_the_subcommands() {
    let output = zoe(&["--help"], &[]);
    assert_exit(&output, 0, "zoe --help");
    let help = stdout(&output);
    for subcommand in SUBCOMMANDS {
        assert!(help.contains(subcommand), "{subcommand} is not in:\n{help}");
        assert_exit(&zoe(&[subcommand, "--help"], &[]), 0, subcommand);
    }
}

#[test]
fn version_is_the_version_of_the_crates() {
    let output = zoe(&["--version"], &[]);
    assert_exit(&output, 0, "zoe --version");
    assert!(stdout(&output).contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn every_option_names_its_environment_twin_in_the_help() {
    let expected: [(&str, &[&str]); 2] = [
        (
            "convert",
            &[
                "--path",
                "--from-data-fs",
                "--to-data-fs",
                "--data-fs-blob-dir",
            ],
        ),
        ("pack", &["--path", "--days", "--dry-run"]),
    ];
    for (subcommand, options) in expected {
        let output = zoe(&[subcommand, "--help"], &[]);
        assert_exit(&output, 0, subcommand);
        let help = stdout(&output);
        let found: BTreeSet<&str> = help
            .split(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'))
            .filter(|word| word.len() > 2 && word.starts_with("--"))
            .collect();
        for option in options {
            assert!(
                found.contains(option),
                "{subcommand}: no {option} in:\n{help}"
            );
        }
        for option in found {
            if option == "--help" || option == "--version" {
                continue;
            }
            let twin = format!("ZOE_{}", option[2..].to_uppercase().replace('-', "_"));
            assert!(
                help.contains(&twin),
                "{subcommand}: the help of {option} does not name {twin}:\n{help}"
            );
        }
    }
}

#[test]
fn a_usage_error_exits_with_2() {
    let dir = fresh_dir("usage");
    let store = dir.join("zoe");
    let data_fs = dir.join("Data.fs");
    let cases: [&[&str]; 6] = [
        &[],
        &["no-such-command"],
        &["pack", "--path", s(&store), "--no-such-option"],
        &["pack", "--path", s(&store), "--days", "many"],
        // A conversion needs a direction.
        &["convert", "--path", s(&store)],
        &[
            "convert",
            "--path",
            s(&store),
            "--from-data-fs",
            s(&data_fs),
            "--to-data-fs",
            s(&data_fs),
        ],
    ];
    for args in cases {
        assert_exit(&zoe(args, &[]), 2, &format!("zoe {}", args.join(" ")));
    }
    assert!(!store.exists(), "a usage error created the store");
}

#[test]
fn convert_reads_a_data_fs_into_a_store_and_writes_it_back() {
    let dir = fresh_dir("convert");
    let expected = converted_fixture(&dir.join("library"));
    let (source, source_blobs) = (fixture().join("Data.fs"), fixture().join("blobs-bushy"));
    let store = dir.join("zoe");
    let from = [
        "convert",
        "--from-data-fs",
        s(&source),
        "--data-fs-blob-dir",
        s(&source_blobs),
        "--path",
        s(&store),
    ];
    assert_exit(&zoe(&from, &[]), 0, "convert from a Data.fs");
    assert_eq!(contents(&store), expected);

    let out = dir.join("out");
    std::fs::create_dir_all(&out).unwrap();
    let (data_fs, blobs) = (out.join("Data.fs"), out.join("blobs"));
    let to = [
        "convert",
        "--to-data-fs",
        s(&data_fs),
        "--data-fs-blob-dir",
        s(&blobs),
        "--path",
        s(&store),
    ];
    assert_exit(&zoe(&to, &[]), 0, "convert to a Data.fs");
    // The written Data.fs reads back into the same transactions.
    let again = dir.join("again");
    {
        let store = common::open_store(&again);
        let report = from_datafs(&data_fs, Some(&blobs), &store).unwrap();
        assert!(report.problems.is_empty(), "{report:?}");
    }
    assert_eq!(contents(&again), expected);

    // A conversion resumes after the last tid, so a second run of each
    // direction copies nothing.
    let written = std::fs::read(&data_fs).unwrap();
    assert_exit(&zoe(&from, &[]), 0, "convert from a Data.fs again");
    assert_exit(&zoe(&to, &[]), 0, "convert to a Data.fs again");
    assert_eq!(contents(&store), expected);
    assert!(
        std::fs::read(&data_fs).unwrap() == written,
        "the second run changed the Data.fs"
    );
}

#[test]
fn convert_takes_each_option_from_its_environment_twin() {
    let dir = fresh_dir("convert-env");
    let expected = converted_fixture(&dir.join("library"));
    let data_fs = fixture().join("Data.fs");
    let blobs = fixture().join("blobs-bushy");

    // Each option comes from its twin.
    let store = dir.join("zoe");
    let env = [
        ("ZOE_FROM_DATA_FS", s(&data_fs)),
        ("ZOE_DATA_FS_BLOB_DIR", s(&blobs)),
        ("ZOE_PATH", s(&store)),
    ];
    assert_exit(&zoe(&["convert"], &env), 0, "convert from the environment");
    assert_eq!(contents(&store), expected);

    let (flags, twins) = (dir.join("flags"), dir.join("twins"));
    for out in [&flags, &twins] {
        std::fs::create_dir_all(out).unwrap();
    }
    let (flags_fs, flags_blobs) = (flags.join("Data.fs"), flags.join("blobs"));
    let (twins_fs, twins_blobs) = (twins.join("Data.fs"), twins.join("blobs"));
    let to = [
        "convert",
        "--to-data-fs",
        s(&flags_fs),
        "--data-fs-blob-dir",
        s(&flags_blobs),
        "--path",
        s(&store),
    ];
    assert_exit(&zoe(&to, &[]), 0, "convert to a Data.fs");
    let env = [
        ("ZOE_TO_DATA_FS", s(&twins_fs)),
        ("ZOE_DATA_FS_BLOB_DIR", s(&twins_blobs)),
        ("ZOE_PATH", s(&store)),
    ];
    assert_exit(
        &zoe(&["convert"], &env),
        0,
        "convert to a Data.fs from the environment",
    );
    assert!(
        std::fs::read(&flags_fs).unwrap() == std::fs::read(&twins_fs).unwrap(),
        "the twins wrote another Data.fs than the options"
    );

    // An option on the command line wins over its twin.
    let elsewhere = dir.join("elsewhere");
    let chosen = dir.join("chosen");
    let args = [
        "convert",
        "--from-data-fs",
        s(&data_fs),
        "--data-fs-blob-dir",
        s(&blobs),
        "--path",
        s(&chosen),
    ];
    assert_exit(
        &zoe(&args, &[("ZOE_PATH", s(&elsewhere))]),
        0,
        "convert with --path and ZOE_PATH",
    );
    assert_eq!(contents(&chosen), expected);
    assert!(!elsewhere.exists(), "the twin won over the option");
}

#[test]
fn a_conversion_that_fails_or_does_not_verify_exits_with_1() {
    let dir = fresh_dir("convert-fails");

    let not_data_fs = dir.join("not-a-data-fs");
    std::fs::write(&not_data_fs, b"this is not a FileStorage file").unwrap();
    let store = dir.join("zoe");
    let output = zoe(
        &[
            "convert",
            "--from-data-fs",
            s(&not_data_fs),
            "--path",
            s(&store),
        ],
        &[],
    );
    assert_exit(&output, 1, "convert a file that is not a Data.fs");
    assert!(!output.stderr.is_empty(), "the failure has no message");

    // A missing blob makes the verification of the next run fail.
    let (source, source_blobs) = (fixture().join("Data.fs"), fixture().join("blobs-bushy"));
    let verified = dir.join("verified");
    let args = [
        "convert",
        "--from-data-fs",
        s(&source),
        "--data-fs-blob-dir",
        s(&source_blobs),
        "--path",
        s(&verified),
    ];
    assert_exit(&zoe(&args, &[]), 0, "convert the fixture");
    let expected = std::fs::read_to_string(fixture().join("expected.txt")).unwrap();
    let blob = expected
        .lines()
        .find_map(|line| line.strip_prefix("blob "))
        .unwrap();
    let fields: Vec<&str> = blob.split(' ').collect();
    let oid = Oid(u64::from_str_radix(fields[0], 16).unwrap());
    let tid = Tid(u64::from_str_radix(fields[1], 16).unwrap());
    let path = common::open_store(&verified).blob_path(oid, tid).unwrap();
    std::fs::remove_file(path).unwrap();
    assert_exit(&zoe(&args, &[]), 1, "convert with a missing blob");

    // A store that does not exist does not convert, and stays missing.
    let missing = dir.join("missing");
    let (target, target_blobs) = (dir.join("Data.fs"), dir.join("blobs"));
    let output = zoe(
        &[
            "convert",
            "--to-data-fs",
            s(&target),
            "--data-fs-blob-dir",
            s(&target_blobs),
            "--path",
            s(&missing),
        ],
        &[],
    );
    assert_exit(&output, 1, "convert a store that does not exist");
    assert!(!missing.exists(), "the conversion created a store");
}

const ROOT: Oid = Oid(0);
const A: Oid = Oid(1);
const B: Oid = Oid(2);
const DAY: u64 = 86_400;

/// The tid of the time `days` days ago.
fn days_ago(days: u64) -> Tid {
    timestamp(SystemTime::now() - Duration::from_secs(days * DAY))
}

fn text(p: &mut Vec<u8>, s: &str) {
    p.push(b'X');
    p.extend_from_slice(&(s.len() as u32).to_le_bytes());
    p.extend_from_slice(s.as_bytes());
}

/// A protocol-3 record of `module.name` with a dict state: a title, and a
/// `(oid, class)` reference for each of `refs`.
fn record(module: &str, name: &str, title: &str, refs: &[(&str, Oid)]) -> Vec<u8> {
    let mut p = b"\x80\x03c".to_vec();
    p.extend_from_slice(format!("{module}\n{name}\n").as_bytes());
    p.extend_from_slice(b"q\x00.\x80\x03}(");
    text(&mut p, "title");
    text(&mut p, title);
    for &(key, oid) in refs {
        text(&mut p, key);
        p.extend_from_slice(b"C\x08");
        p.extend_from_slice(&oid.0.to_be_bytes());
        p.extend_from_slice(b"capp.content\nDocument\n\x86Q");
    }
    p.extend_from_slice(b"u.");
    p
}

fn mapping(refs: &[(&str, Oid)]) -> Vec<u8> {
    record("persistent.mapping", "PersistentMapping", "root", refs)
}

fn document(title: &str) -> Vec<u8> {
    record("app.content", "Document", title, &[])
}

fn file(title: &str) -> Vec<u8> {
    record("app.content", "File", title, &[])
}

/// Copy one transaction with the tid `tid` into the store.
fn restore(store: &Store, tid: Tid, writes: &[(Oid, Vec<u8>)]) {
    let mut txn = store
        .tpc_begin_restore(tid, b"someone", b"aged", b"")
        .unwrap();
    for (oid, data) in writes {
        txn.restore(*oid, Some(data.as_slice())).unwrap();
    }
    txn.vote().unwrap();
    assert_eq!(txn.finish().unwrap(), tid);
}

/// Write a store in `dir` whose transactions are 10, 9 and 5 days old,
/// and return their tids:
///
/// 1. the root holds the document A and the file B;
/// 2. the root drops B, and A changes;
/// 3. A changes again.
///
/// A pack as of 7 days ago removes the first revisions of the root and of
/// A, and B. A pack as of 3 days ago also removes the second revision of
/// A.
fn aged_store(dir: &Path) -> [Tid; 3] {
    let tids = [days_ago(10), days_ago(9), days_ago(5)];
    let store = common::open_store(dir);
    restore(
        &store,
        tids[0],
        &[
            (ROOT, mapping(&[("a", A), ("b", B)])),
            (A, document("a1")),
            (B, file("b1")),
        ],
    );
    restore(
        &store,
        tids[1],
        &[(ROOT, mapping(&[("a", A)])), (A, document("a2"))],
    );
    restore(&store, tids[2], &[(A, document("a3"))]);
    tids
}

/// The records of the aged store that are gone.
fn removed(dir: &Path, tids: [Tid; 3]) -> BTreeSet<(Oid, Tid)> {
    let store = common::open_store(dir);
    [(ROOT, 0), (ROOT, 1), (A, 0), (A, 1), (A, 2), (B, 0)]
        .into_iter()
        .map(|(oid, i)| (oid, tids[i]))
        .filter(|&(oid, tid)| match store.load_serial(oid, tid) {
            Ok(_) => false,
            Err(ZoeError::NotFound { .. }) => true,
            Err(e) => panic!("{oid:?} at {tid:?}: {e}"),
        })
        .collect()
}

/// What a pack as of 7 days ago removes from the aged store.
fn removed_at_7_days(tids: [Tid; 3]) -> BTreeSet<(Oid, Tid)> {
    BTreeSet::from([(ROOT, tids[0]), (A, tids[0]), (B, tids[0])])
}

/// The classes of the records that a pack as of 7 days ago removes.
const CLASSES_AT_7_DAYS: [&str; 3] = [
    "persistent.mapping.PersistentMapping",
    "app.content.Document",
    "app.content.File",
];

#[test]
fn pack_packs_as_of_the_given_number_of_days_ago() {
    let dir = fresh_dir("pack");
    let store = dir.join("zoe");
    let tids = aged_store(&store);

    let pack = |days: &str| zoe(&["pack", "--path", s(&store), "--days", days], &[]);
    assert_exit(&pack("30"), 0, "pack as of 30 days ago");
    assert_eq!(removed(&store, tids), BTreeSet::new());

    assert_exit(&pack("7"), 0, "pack as of 7 days ago");
    assert_eq!(removed(&store, tids), removed_at_7_days(tids));

    assert_exit(&pack("3"), 0, "pack as of 3 days ago");
    let mut expected = removed_at_7_days(tids);
    expected.insert((A, tids[1]));
    assert_eq!(removed(&store, tids), expected);
    let store = common::open_store(&store);
    assert_eq!(store.load(A).unwrap(), (document("a3"), tids[2]));
}

#[test]
fn a_dry_run_reports_by_class_and_removes_nothing() {
    let dir = fresh_dir("dry-run");
    let store = dir.join("zoe");
    let tids = aged_store(&store);

    let output = zoe(
        &["pack", "--path", s(&store), "--days", "7", "--dry-run"],
        &[],
    );
    assert_exit(&output, 0, "a dry run");
    let report = stdout(&output);
    for class in CLASSES_AT_7_DAYS {
        assert!(report.contains(class), "{class} is not in:\n{report}");
    }
    assert_eq!(removed(&store, tids), BTreeSet::new());
    assert_eq!(common::open_store(&store).last_transaction(), tids[2]);

    // The pack removes what the dry run reported.
    assert_exit(
        &zoe(&["pack", "--path", s(&store), "--days", "7"], &[]),
        0,
        "the pack",
    );
    assert_eq!(removed(&store, tids), removed_at_7_days(tids));
}

#[test]
fn pack_takes_each_option_from_its_environment_twin() {
    let dir = fresh_dir("pack-env");
    let store = dir.join("zoe");
    let tids = aged_store(&store);

    let env = [
        ("ZOE_PATH", s(&store)),
        ("ZOE_DAYS", "7"),
        ("ZOE_DRY_RUN", "true"),
    ];
    let output = zoe(&["pack"], &env);
    assert_exit(&output, 0, "a dry run from the environment");
    for class in CLASSES_AT_7_DAYS {
        assert!(
            stdout(&output).contains(class),
            "{class} is not in the report"
        );
    }
    assert_eq!(removed(&store, tids), BTreeSet::new());

    let env = [
        ("ZOE_PATH", s(&store)),
        ("ZOE_DAYS", "7"),
        ("ZOE_DRY_RUN", "false"),
    ];
    assert_exit(&zoe(&["pack"], &env), 0, "a pack from the environment");
    assert_eq!(removed(&store, tids), removed_at_7_days(tids));

    // An option on the command line wins over its twin.
    let elsewhere = dir.join("elsewhere");
    let env = [("ZOE_PATH", s(&elsewhere)), ("ZOE_DAYS", "3")];
    assert_exit(
        &zoe(&["pack", "--path", s(&store), "--days", "7"], &env),
        0,
        "a pack with options and twins",
    );
    assert_eq!(removed(&store, tids), removed_at_7_days(tids));
    assert!(!elsewhere.exists(), "the twin won over the option");
}

#[test]
fn pack_fails_with_1_on_a_missing_or_locked_store_and_changes_nothing() {
    let dir = fresh_dir("pack-fails");

    let missing = dir.join("missing");
    let output = zoe(&["pack", "--path", s(&missing), "--days", "7"], &[]);
    assert_exit(&output, 1, "pack a store that does not exist");
    assert!(!output.stderr.is_empty(), "the failure has no message");
    assert!(!missing.exists(), "the pack created a store");

    let store = dir.join("zoe");
    let tids = aged_store(&store);
    let running = common::open_store(&store);
    let output = zoe(&["pack", "--path", s(&store), "--days", "7"], &[]);
    assert_exit(&output, 1, "pack a store that another process holds");
    drop(running);
    assert_eq!(removed(&store, tids), BTreeSet::new());
}

fn args(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

#[test]
fn the_library_entry_point_returns_the_exit_code() {
    let dir = fresh_dir("entry-point");
    let missing = dir.join("missing");
    let code: i32 = zoe_server::cli::main(args(&["zoe", "no-such-command"]));
    assert_eq!(code, 2);
    let code: i32 =
        zoe_server::cli::main(args(&["zoe", "pack", "--path", s(&missing), "--days", "7"]));
    assert_eq!(code, 1);
    assert!(!missing.exists());
}
