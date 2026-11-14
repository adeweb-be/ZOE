//! Acceptance test of the document of `zoe-bench catalog` (ADR 0054, the
//! family `catalog`): the document that a run of the default set writes
//! against a small store of `zoe_catalog::testing` is valid against
//! `bench/schema/catalog.schema.json`. `just bench publish` takes only a
//! document valid against that schema. The key `catalog` names the family,
//! and the document holds none of the keys of the four other families.
//!
//! The test runs the `zoe` binary that `cargo test --workspace` builds next
//! to `zoe-bench`, or `ZOE_BINARY`.

mod support;

use std::path::{Path, PathBuf};
use std::process::Command;

use rmpv::Value;
use serde_json::Value as Json;
use zoe_catalog::testing::{self, CatalogModel, IndexKind, TextOp};
use zoe_catalog::text::{CatalogSettings, IndexSettings, Mode};
use zoe_catalog::value::Val;
use zoe_core::Store;

use support::fresh_dir;

const BENCH: &str = env!("CARGO_BIN_EXE_zoe-bench");

/// The schema of the family `catalog`.
const SCHEMA: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../bench/schema/catalog.schema.json"
);

/// The entries of the catalog.
const ENTRIES: i32 = 120;
/// The minute of the first effective date.
const MINUTE: i64 = 28_000_000;
/// The columns of the catalog.
const COLUMNS: [&str; 3] = ["getId", "Title", "modified"];

fn zoe_binary() -> PathBuf {
    if let Some(binary) = std::env::var_os("ZOE_BINARY") {
        return PathBuf::from(binary);
    }
    let binary = Path::new(BENCH).with_file_name("zoe");
    assert!(
        binary.is_file(),
        "{} is missing: run `cargo build -p zoe-server --bin zoe`, or set ZOE_BINARY",
        binary.display()
    );
    binary
}

fn s(text: &str) -> Value {
    Value::from(text)
}

fn map(items: Vec<(&str, Value)>) -> Value {
    Value::Map(items.into_iter().map(|(k, v)| (s(k), v)).collect())
}

fn strs(words: &[&str]) -> Value {
    Value::Array(words.iter().map(|w| s(w)).collect())
}

fn field(index: &str, values: &[&str]) -> Value {
    map(vec![
        ("k", s("v")),
        ("i", s(index)),
        ("q", strs(values)),
        ("and", Value::Boolean(false)),
        ("r", Value::Nil),
        ("not", Value::Array(vec![])),
    ])
}

fn text(index: &str, word: &str) -> Value {
    map(vec![
        ("k", s("t")),
        ("i", s(index)),
        ("q", map(vec![("n", s("atom")), ("w", strs(&[word]))])),
        ("lg", Value::Nil),
        ("cl", Value::Array(vec![])),
        ("st", Value::Array(vec![])),
        ("set", Value::Nil),
    ])
}

/// A plan with `clauses`, a sort on `effective` with a limit when `sorted`,
/// and the columns of the catalog.
fn plan(clauses: Vec<Value>, sorted: bool) -> Value {
    let (sort, limit) = if sorted {
        (
            Value::Array(vec![Value::Array(vec![
                s("effective"),
                Value::Boolean(true),
            ])]),
            Value::from(20),
        )
    } else {
        (Value::Array(vec![]), Value::Nil)
    };
    map(vec![
        ("c", Value::Array(clauses)),
        ("s", sort),
        ("l", limit),
        ("bs", Value::from(0)),
        ("bz", Value::Nil),
        ("cols", strs(&COLUMNS)),
        ("rn", Value::from(50)),
    ])
}

fn query(name: &str, scenario: &str, plan: Value, variant: Option<&str>) -> Value {
    map(vec![
        ("name", s(name)),
        ("scenario", s(scenario)),
        ("plan", plan),
        ("variant", variant.map_or(Value::Nil, s)),
    ])
}

fn values(i: i32) -> Vec<(&'static str, Val)> {
    let kind = if i % 4 == 0 { "Folder" } else { "Document" };
    let state = if i % 3 == 0 { "private" } else { "published" };
    let effective = MINUTE + i64::from(i);
    vec![
        ("portal_type", Val::Str(kind.into())),
        ("review_state", Val::Str(state.into())),
        ("effective", Val::Int(effective)),
        ("getId", Val::Str(format!("item-{i:03}").into())),
        ("Title", Val::Str(format!("Title {i}").into())),
        ("modified", Val::Int(effective)),
    ]
}

/// The settings of the text index `SearchableText` in the enhanced mode.
fn settings() -> CatalogSettings {
    let mut out = CatalogSettings::default();
    let mut each = IndexSettings::enhanced(&["en"]);
    each.mode = Mode::Enhanced;
    out.indexes.insert("SearchableText".into(), each);
    out
}

/// Build the store and the plans file in `dir`. Return the store path and
/// the plans path.
fn prepare(dir: &Path) -> (PathBuf, PathBuf) {
    let store_dir = dir.join("store");
    let store = Store::open(&store_dir).unwrap();
    let indexes = [
        ("portal_type", IndexKind::Field),
        ("review_state", IndexKind::Field),
        ("effective", IndexKind::Date),
    ];
    let mut model = CatalogModel::new(10, &indexes, &COLUMNS);
    for i in 0..ENTRIES {
        let path = format!("/site/folder-{}/item-{i:03}", i % 5);
        model.catalog(1000 + i, &path, &values(i));
    }
    testing::commit(&store, &model.records(), &[model.key()], &[]).unwrap();
    let texts: Vec<String> = (0..ENTRIES)
        .map(|i| {
            let rare = if i % 3 == 0 { " mite" } else { "" };
            format!("kalo{rare} item{i}")
        })
        .collect();
    let ops: Vec<TextOp> = texts
        .iter()
        .enumerate()
        .map(|(i, body)| TextOp::Set {
            index: "SearchableText",
            rid: 1000 + i32::try_from(i).unwrap(),
            language: "en",
            text: body,
        })
        .collect();
    let enhanced = settings();
    testing::commit_text(&store, model.oid(), &ops, Some(&enhanced), false).unwrap();
    drop(store);

    let listing = || {
        vec![
            field("portal_type", &["Document"]),
            field("review_state", &["published"]),
        ]
    };
    let mut queries = vec![
        query("listing", "intersect", plan(listing(), true), None),
        query(
            "listing",
            "filter",
            plan(vec![field("portal_type", &["Document"])], false),
            None,
        ),
        query("mix-listing", "mix", plan(listing(), true), None),
        // A line of the log that the client sends to the Python path.
        map(vec![
            ("name", s("mix-start")),
            ("scenario", s("mix")),
            ("plan", Value::Nil),
            ("reason", s("index")),
            ("weight", Value::from(2)),
        ]),
    ];
    for word in ["kalo", "mite"] {
        for (variant, mut clauses, sorted) in [
            ("alone", vec![], false),
            ("listing", vec![field("portal_type", &["Document"])], false),
            ("sort", vec![], true),
        ] {
            clauses.insert(0, text("SearchableText", word));
            queries.push(query(
                &format!("text-{word}-{variant}"),
                "text",
                plan(clauses, sorted),
                Some(variant),
            ));
        }
    }
    let counts = map(vec![
        ("size", Value::from(ENTRIES)),
        (
            "indexes",
            map(indexes
                .iter()
                .map(|(name, _)| (*name, Value::from(ENTRIES)))
                .collect()),
        ),
        (
            "columns",
            map(COLUMNS
                .iter()
                .map(|name| (*name, Value::from(ENTRIES)))
                .collect()),
        ),
    ]);
    let file = map(vec![
        ("version", Value::from(1)),
        ("catalog", Value::Binary(model.key().to_vec())),
        (
            "fixture",
            map(vec![
                ("name", s("catalog-document")),
                ("preset", s("custom")),
                ("counts", counts),
            ]),
        ),
        (
            "modes",
            map(vec![("enhanced", Value::Binary(enhanced.to_json()))]),
        ),
        ("queries", Value::Array(queries)),
        ("writes", Value::Nil),
        ("texts", Value::Nil),
    ]);
    let plans = dir.join("plans.msgpack");
    let mut bytes = Vec::new();
    rmpv::encode::write_value(&mut bytes, &file).unwrap();
    std::fs::write(&plans, bytes).unwrap();
    (store_dir, plans)
}

#[test]
fn the_document_of_a_run_is_valid_against_the_schema() {
    let dir = fresh_dir("catalog-document");
    let (store, plans) = prepare(&dir);
    // The default set: cold, intersect, mix and text.
    let output = Command::new(BENCH)
        .arg("catalog")
        .arg("--zoe")
        .arg(zoe_binary())
        .arg("--store")
        .arg(&store)
        .arg("--plans")
        .arg(&plans)
        .args(["--seconds", "0.2", "--ready-timeout", "120"])
        .args(["--text-commits", "0", "--rebase-seconds", "0"])
        .env(
            "ZOE_BENCH_COMMIT",
            "d939b3f580637ba1f457c9041a12b2f26b0a5273",
        )
        .env("ZOE_BENCH_DIRTY", "false")
        .env("ZOE_BENCH_MACHINE", "reference")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "zoe-bench catalog failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let doc: Json = serde_json::from_slice(&output.stdout).expect("the result is JSON");

    let schema: Json = serde_json::from_str(&std::fs::read_to_string(SCHEMA).unwrap()).unwrap();
    let validator = jsonschema::validator_for(&schema).expect("the schema is valid");
    let errors: Vec<String> = validator
        .iter_errors(&doc)
        .map(|e| format!("{}: {e}", e.instance_path()))
        .collect();
    assert!(
        errors.is_empty(),
        "the document is not valid against {SCHEMA}:\n{}",
        errors.join("\n")
    );

    // The key of the family, and none of the keys of the other families.
    assert!(doc["catalog"].is_object());
    for key in ["storage", "cells", "rows", "fixture", "python"] {
        assert!(doc.get(key).is_none(), "the document holds {key}");
    }
    let names: Vec<&str> = doc["scenarios"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["cold", "intersect", "mix", "text"]);
    assert_eq!(doc["run"]["dirty"], false);
    assert_eq!(doc["machine"]["name"], "reference");

    // No path of the store or of the plans, and no address of the server.
    let text = doc.to_string();
    for private in [store.to_str().unwrap(), dir.to_str().unwrap(), "127.0.0.1"] {
        assert!(!text.contains(private), "the document holds {private}");
    }
}
