//! Acceptance test of `zoe-bench catalog`: against a `zoe serve` on a small
//! store of `zoe_catalog::testing`, with its registration key, its text
//! deltas and the settings of its text indexes in both modes, each scenario
//! runs and gives each of its numbers. A reply `f` counts as a refusal with
//! its reason, and a reply `e` of a text clause as an error with its kind,
//! not as a latency. The scenario `cold` finds the readiness of the mirror
//! and of the text index.
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

/// The entries of the catalog.
const ENTRIES: i32 = 300;
/// The moves of each write, and the writes.
const MOVES: i32 = 10;
const WRITES: i32 = 4;
/// The rebase threshold of the run: two writes stay under it, four pass it.
const REBASE_ROWS: u64 = 50;
/// The minute of the first effective date.
const MINUTE: i64 = 28_000_000;
/// The text indexes.
const TEXT_INDEXES: [&str; 3] = ["SearchableText", "Title", "Description"];

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

fn boolean(index: &str, value: bool) -> Value {
    map(vec![
        ("k", s("b")),
        ("i", s(index)),
        ("q", Value::Boolean(value)),
    ])
}

fn date_range(index: &str, minute: i64) -> Value {
    map(vec![
        ("k", s("d")),
        ("i", s(index)),
        ("t", Value::from(minute)),
    ])
}

fn path(paths: &[&str], depth: i64) -> Value {
    map(vec![
        ("k", s("p")),
        ("i", s("path")),
        (
            "q",
            Value::Array(
                paths
                    .iter()
                    .map(|p| Value::Array(vec![s(p), Value::Nil]))
                    .collect(),
            ),
        ),
        ("lv", Value::from(0)),
        ("and", Value::Boolean(false)),
        ("d", Value::from(depth)),
        ("nt", Value::Boolean(false)),
        ("ns", Value::from(0)),
        ("ip", strs(&["site", "portal_catalog", "path"])),
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

/// A plan with `clauses`, the sort `sort` and its limit, and the columns of
/// the catalog for `rn` rows.
fn plan(clauses: Vec<Value>, sort: Option<(&str, bool)>, limit: Option<i64>) -> Value {
    let sort = sort.map_or(Value::Array(vec![]), |(index, reverse)| {
        Value::Array(vec![Value::Array(vec![s(index), Value::Boolean(reverse)])])
    });
    map(vec![
        ("c", Value::Array(clauses)),
        ("s", sort),
        ("l", limit.map_or(Value::Nil, Value::from)),
        ("bs", Value::from(0)),
        ("bz", Value::Nil),
        ("cols", strs(&["getId", "Title", "modified"])),
        ("rn", Value::from(100)),
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

/// The path of entry `i`, and of its moved twin.
fn entry_path(i: i32) -> String {
    format!("/site/folder-{}/item-{i:03}", i % 5)
}

fn moved_path(i: i32) -> String {
    format!("/site/moved/item-{i:03}")
}

fn values(i: i32) -> Vec<(&'static str, Val)> {
    let kind = if i % 4 == 0 { "Folder" } else { "Document" };
    let state = if i % 3 == 0 { "private" } else { "published" };
    let roles = if i % 3 == 0 { "Manager" } else { "Anonymous" };
    let effective = MINUTE + i64::from(i);
    vec![
        ("portal_type", Val::Str(kind.into())),
        ("review_state", Val::Str(state.into())),
        (
            "allowedRolesAndUsers",
            Val::List(vec![Val::Str(roles.into())].into()),
        ),
        ("effective", Val::Int(effective)),
        ("is_folderish", Val::Bool(i % 4 == 0)),
        (
            "effectiveRange",
            Val::Tuple(vec![Val::Int(effective), Val::None].into()),
        ),
        ("sortable_title", Val::Str(format!("title {i:03}").into())),
        ("getId", Val::Str(format!("item-{i:03}").into())),
        ("Title", Val::Str(format!("Title {i}").into())),
        ("modified", Val::Int(effective)),
    ]
}

fn searchable(i: i32) -> String {
    let rare = if i % 3 == 0 { " mite" } else { "" };
    format!("kalo{rare} item{i}")
}

fn settings(mode: Mode) -> CatalogSettings {
    let mut out = CatalogSettings::default();
    for index in TEXT_INDEXES {
        let mut each = IndexSettings::enhanced(&["en"]);
        each.mode = mode;
        if mode == Mode::Compat {
            each.language.clear();
        }
        out.indexes.insert(index.into(), each);
    }
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
        ("allowedRolesAndUsers", IndexKind::Keyword),
        ("effective", IndexKind::Date),
        ("is_folderish", IndexKind::Boolean),
        (
            "effectiveRange",
            IndexKind::DateRange("effective", "expires"),
        ),
        ("path", IndexKind::Path),
        ("sortable_title", IndexKind::Field),
    ];
    let mut model = CatalogModel::new(10, &indexes, &["getId", "Title", "modified"]);
    for i in 0..ENTRIES {
        model.catalog(1000 + i, &entry_path(i), &values(i));
    }
    testing::commit(&store, &model.records(), &[model.key()], &[]).unwrap();
    let texts: Vec<(i32, String, String)> = (0..ENTRIES)
        .map(|i| (1000 + i, searchable(i), format!("Title {i}")))
        .collect();
    let mut ops = Vec::new();
    for (rid, body, title) in &texts {
        ops.push(TextOp::Set {
            index: "SearchableText",
            rid: *rid,
            language: "en",
            text: body,
        });
        ops.push(TextOp::Set {
            index: "Title",
            rid: *rid,
            language: "en",
            text: title,
        });
        ops.push(TextOp::Set {
            index: "Description",
            rid: *rid,
            language: "en",
            text: "kalo",
        });
    }
    let enhanced = settings(Mode::Enhanced);
    testing::commit_text(&store, model.oid(), &ops, Some(&enhanced), false).unwrap();

    // The writes: moves of entries, which the run commits.
    let mut writes = Vec::new();
    for w in 0..WRITES {
        for m in 0..MOVES {
            let i = w * MOVES + m + 1;
            model.uncatalog(&entry_path(i));
            model.catalog(5000 + i, &moved_path(i), &values(i));
        }
        let records = model
            .records()
            .into_iter()
            .map(|(oid, data)| {
                Value::Array(vec![
                    Value::Binary(oid.to_be_bytes().to_vec()),
                    Value::Binary(data),
                ])
            })
            .collect();
        writes.push(map(vec![
            ("rows", Value::from(2 * MOVES)),
            ("records", Value::Array(records)),
        ]));
    }
    drop(store);

    let now = MINUTE + i64::from(ENTRIES) + 10;
    let listing = || {
        vec![
            field("portal_type", &["Document"]),
            field("review_state", &["published"]),
            field("allowedRolesAndUsers", &["Anonymous"]),
            date_range("effectiveRange", now),
            boolean("is_folderish", false),
        ]
    };
    let mut queries = vec![
        query(
            "listing",
            "intersect",
            plan(listing(), Some(("effective", true)), Some(20)),
            None,
        ),
        query(
            "folders",
            "intersect",
            plan(
                vec![
                    field("portal_type", &["Folder", "Document"]),
                    field("allowedRolesAndUsers", &["Anonymous", "Manager"]),
                    date_range("effectiveRange", now),
                ],
                Some(("sortable_title", false)),
                Some(100),
            ),
            None,
        ),
        query(
            "listing",
            "filter",
            plan(vec![field("portal_type", &["Document"])], None, None),
            None,
        ),
    ];
    for word in ["kalo", "mite"] {
        let variants = [
            ("alone", vec![], None),
            ("listing", vec![field("portal_type", &["Document"])], None),
            (
                "selective",
                vec![field("sortable_title", &["title 003", "title 006"])],
                None,
            ),
            ("sort", vec![], Some(("effective", true))),
            (
                "listing_sort",
                vec![field("portal_type", &["Document"])],
                Some(("effective", true)),
            ),
        ];
        for (variant, mut clauses, sort) in variants {
            clauses.insert(0, text("SearchableText", word));
            let limit = sort.map(|_| 20);
            queries.push(query(
                &format!("text-{word}-{variant}"),
                "text",
                plan(clauses, sort, limit),
                Some(variant),
            ));
        }
    }
    // A text clause on an index without a text index: a reply `e`.
    queries.push(query(
        "text-nowhere-alone",
        "text",
        plan(vec![text("Nowhere", "kalo")], None, None),
        Some("alone"),
    ));
    for (name, depth, paths) in [
        ("folder", -1, vec!["/site/folder-1"]),
        ("children", 1, vec!["/site"]),
        ("moved", -1, vec!["/site/moved"]),
    ] {
        queries.push(query(
            name,
            "path",
            plan(vec![path(&paths, depth)], None, None),
            None,
        ));
    }
    queries.push(query(
        "mix-listing",
        "mix",
        plan(listing(), Some(("effective", true)), Some(10)),
        None,
    ));
    queries.push(query(
        "mix-path",
        "mix",
        plan(vec![path(&["/site/folder-2"], 1)], None, None),
        None,
    ));
    // A clause of a kind that the server does not know: a reply `f`.
    queries.push(query(
        "mix-unknown",
        "mix",
        plan(
            vec![map(vec![("k", s("z")), ("i", s("portal_type"))])],
            None,
            None,
        ),
        None,
    ));
    // A line of the log that the client sends to the Python path.
    queries.push(map(vec![
        ("name", s("mix-start")),
        ("scenario", s("mix")),
        ("plan", Value::Nil),
        ("reason", s("index")),
        ("weight", Value::from(3)),
    ]));

    let sample = texts
        .iter()
        .take(40)
        .map(|(rid, body, _)| {
            Value::Array(vec![
                s("SearchableText"),
                Value::from(*rid),
                s("en"),
                s(body),
            ])
        })
        .collect();
    let file = map(vec![
        ("version", Value::from(1)),
        ("catalog", Value::Binary(model.key().to_vec())),
        (
            "fixture",
            map(vec![
                ("name", s("catalog-test")),
                ("preset", s("custom")),
                ("counts", map(vec![("size", Value::from(ENTRIES))])),
            ]),
        ),
        (
            "modes",
            map(vec![
                ("enhanced", Value::Binary(enhanced.to_json())),
                ("compat", Value::Binary(settings(Mode::Compat).to_json())),
            ]),
        ),
        ("queries", Value::Array(queries)),
        ("writes", Value::Array(writes)),
        ("texts", Value::Array(sample)),
    ]);
    let plans = dir.join("plans.msgpack");
    let mut bytes = Vec::new();
    rmpv::encode::write_value(&mut bytes, &file).unwrap();
    std::fs::write(&plans, bytes).unwrap();
    (store_dir, plans)
}

fn number(value: &Json, what: &str) -> f64 {
    value
        .as_f64()
        .unwrap_or_else(|| panic!("{what} is not a number: {value}"))
}

fn cell<'a>(cells: &'a Json, name: &str, concurrency: u64) -> &'a Json {
    cells
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == name && c["concurrency"] == concurrency)
        .unwrap_or_else(|| panic!("no cell {name} at {concurrency} in {cells}"))
}

/// A cell gives each of its numbers, and answers.
fn answered(cell: &Json) {
    for key in ["p50_seconds", "p99_seconds", "max_seconds", "qps"] {
        number(&cell[key], key);
    }
    assert!(cell["answered"].as_u64().unwrap() > 0, "{cell}");
    assert!(cell["refusals"].is_object() && cell["errors"].is_object());
}

#[test]
fn each_scenario_runs_and_gives_its_numbers() {
    let dir = fresh_dir("catalog");
    let (store, plans) = prepare(&dir);
    let markdown = dir.join("report.md");
    let output = Command::new(BENCH)
        .arg("catalog")
        .arg("--zoe")
        .arg(zoe_binary())
        .arg("--store")
        .arg(&store)
        .arg("--plans")
        .arg(&plans)
        .arg("--markdown")
        .arg(&markdown)
        .args(["--scenario", "text-rebuild,cold,intersect,mix,text"])
        .args(["--seconds", "0.2", "--ready-timeout", "120"])
        .args([
            "--text-commits",
            "3",
            "--text-batch",
            "5",
            "--text-pause",
            "0.1",
        ])
        .args([
            "--rebase-rows",
            &REBASE_ROWS.to_string(),
            "--rebase-seconds",
            "0.5",
        ])
        .env("ZOE_BENCH_DIRTY", "true")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "zoe-bench catalog failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let doc: Json = serde_json::from_slice(&output.stdout).expect("the result is JSON");

    assert_eq!(doc["schema_version"], 1);
    assert_eq!(doc["settings"]["seconds"], 0.2);
    assert_eq!(doc["settings"]["text_commits"], 3);
    assert_eq!(doc["catalog"]["name"], "catalog-test");
    assert_eq!(doc["catalog"]["preset"], "custom");
    assert_eq!(doc["catalog"]["counts"]["size"], ENTRIES);
    assert_eq!(doc["run"]["dirty"], true);
    assert!(doc["run"]["started"].is_string());
    assert!(doc["machine"]["name"].is_string());
    number(&doc["server"]["mirror_bytes"], "the mirror bytes");
    number(
        &doc["server"]["adr_0059_line_bytes"],
        "the line of ADR 0059",
    );
    assert_eq!(doc["server"]["over_adr_line"], false);
    assert!(number(&doc["metrics"]["scrapes"]["count"], "the scrapes") >= 1.0);

    let scenarios = doc["scenarios"].as_array().unwrap();
    let names: Vec<&str> = scenarios
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["cold", "intersect", "mix", "text", "text-rebuild"]);
    for scenario in scenarios {
        number(&scenario["mirror_bytes"], "mirror_bytes");
        number(&scenario["build_seconds"], "build_seconds");
        if cfg!(target_os = "linux") {
            number(&scenario["rss_bytes"], "rss_bytes");
        }
    }
    let [cold, intersect, mix, text, rebuild] = &scenarios[..] else {
        unreachable!()
    };

    // cold: the readiness of the mirror and of the text index.
    number(&cold["listening_seconds"], "listening");
    let ready = number(&cold["mirror_ready_seconds"], "the mirror ready");
    number(&cold["text_ready_seconds"], "the text index ready");
    assert!(number(&cold["first_answer_seconds"], "the first answer") > 0.0);
    assert!(ready > 0.0);
    assert!(number(&cold["first_answers"]["count"], "the first answers") > 0.0);
    number(
        &cold["first_answers"]["p99_seconds"],
        "the first answers p99",
    );
    assert!(cold["refusals"].is_object());
    assert!(number(&cold["hub_during_build"]["commits"], "the probe commits") > 0.0);

    // intersect: G2 at concurrency 1 and 16, and a full page of brains.
    for concurrency in [1, 16] {
        let c = cell(&intersect["cells"], &format!("c{concurrency}"), concurrency);
        answered(c);
        assert_eq!(c["refusals"], serde_json::json!({}));
    }
    let page = &intersect["page"];
    assert!(number(&page["rows"], "the rows of a page") > 0.0);
    // The pickler of the tests writes no DateTime: the store has no date
    // column.
    assert_eq!(page["date_columns"], serde_json::json!([]));
    for name in ["all_columns", "no_date_columns", "no_rows"] {
        answered(cell(&page["cells"], name, 1));
    }
    number(
        &page["client_decode"]["p50_seconds"],
        "the decode of a page",
    );
    number(
        &page["client_encode"]["p50_seconds"],
        "the encode of a page",
    );

    // mix: a reply `f` is a refusal with its reason, not a latency.
    for concurrency in [1, 4, 16] {
        let c = cell(&mix["cells"], &format!("c{concurrency}"), concurrency);
        answered(c);
        let refused = c["refusals"]["plan"].as_u64().unwrap_or(0);
        assert!(refused > 0, "{c}");
        assert_eq!(
            c["answered"].as_u64().unwrap() + refused,
            c["queries"].as_u64().unwrap(),
            "{c}"
        );
    }
    assert_eq!(mix["client_reasons"]["index"], 3);
    assert!(mix["server_reasons"]["plan"].as_u64().unwrap() > 0);
    let share = number(&mix["native_share"], "the native share");
    assert!(share > 0.0 && share < 1.0, "{share}");
    assert_eq!(mix["lines"], 6);
    let overrides = &mix["overrides"];
    assert_eq!(overrides["override_rows"], 2 * 2 * MOVES);
    answered(cell(&overrides["cells"], "path_base", 1));
    answered(cell(&overrides["cells"], "path_overrides", 1));
    assert_eq!(mix["rebase"]["past_threshold"], true);
    assert!(number(&mix["rebase"]["hub"]["commits"], "the probe commits") > 0.0);
    number(
        &mix["rebase"]["hub"]["invalidation_delay"]["p99_seconds"],
        "the invalidation delay",
    );

    // text: both modes, each variant at concurrency 1 and 16.
    let modes = text["modes"].as_array().unwrap();
    let names: Vec<&str> = modes.iter().map(|m| m["mode"].as_str().unwrap()).collect();
    assert_eq!(names, ["enhanced", "compat"]);
    for mode in modes {
        number(&mode["text_ready_seconds"], "the text index ready");
        let cells = &mode["cells"];
        for variant in ["alone", "listing", "selective", "sort", "listing_sort"] {
            for concurrency in [1, 16] {
                answered(cell(cells, variant, concurrency));
            }
            answered(cell(cells, &format!("after_commits/{variant}"), 1));
        }
        // A reply `e` of a text clause is an error with its kind.
        let alone = cell(cells, "alone", 1);
        let failed = alone["errors"]["unavailable"].as_u64().unwrap_or(0);
        assert!(failed > 0, "{alone}");
        assert_eq!(
            alone["answered"].as_u64().unwrap() + failed,
            alone["queries"].as_u64().unwrap(),
            "{alone}"
        );
        let filtered = cell(cells, "text_query", 1);
        answered(filtered);
        assert!(filtered["errors"]["unavailable"].as_u64().unwrap_or(0) > 0);
        answered(cell(cells, "staged", 1));
        answered(cell(cells, "behind/alone", 1));
        answered(cell(cells, "behind/listing", 1));
        assert!(number(&mode["filter_rids"], "the filter") > 0.0);
        assert!(number(&mode["commits"]["deltas"], "the deltas") > 0.0);
    }

    // text-rebuild: the rebuild of the text index from a store without it.
    number(&rebuild["rebuild_seconds"], "the rebuild");
    if cfg!(target_os = "linux") {
        number(
            &rebuild["rss_at_mirror_ready_bytes"],
            "the RSS at the mirror",
        );
        number(&rebuild["peak_rss_after_mirror_ready_bytes"], "the peak");
    }

    let report = std::fs::read_to_string(&markdown).unwrap();
    assert!(report.contains("## intersect") && report.contains("### mode compat"));
}
