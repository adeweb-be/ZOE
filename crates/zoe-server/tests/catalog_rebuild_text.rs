//! Acceptance test of `zoe catalog rebuild-text` (ADR 0060, section 15):
//! the text index of each registered catalog of a store folder, rebuilt
//! from the live texts of the deltas, with no Plone and no server.
//!
//! A store holds two registered catalogs with the settings of their text
//! indexes and their deltas. The mirrors of the store maintain the index
//! of each catalog in `text/<oid>/` while the deltas commit. Then the
//! command rebuilds the indexes, and a new open of the store uses them.
//!
//! The test of the kills runs the rebuild in a child process, the test
//! binary itself, which kills itself at one step of the write or of the
//! renames.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use zoe_catalog::testing::{self, TextOp};
use zoe_catalog::text::rebuild::{self, Step};
use zoe_catalog::text::{CatalogSettings, IndexSettings, TextConfig, engine};
use zoe_catalog::{Config, Hooks, Mirrors, Opener, Reads, Wire};
use zoe_core::{Options, Store};

mod common;

use common::{assert_exit, fresh_dir, open_store, s, zoe};

/// The longest wait of a test for an index.
const WAIT: Duration = Duration::from_secs(30);

/// The first catalog: the indexes `SearchableText` and `Title`.
const FIRST: u64 = 0x10;

/// The second catalog: the index `Title`, in two languages.
const SECOND: u64 = 0x20;

/// The words of the texts. No report may hold one.
const WORDS: [&str; 6] = [
    "zelkova", "quillon", "marram", "tussock", "ferrule", "gambrel",
];

/// The variables of the child process of the kills.
const CHILD_STORE: &str = "REBUILD_TEXT_CHILD_STORE";
const CHILD_STEP: &str = "REBUILD_TEXT_CHILD_STEP";

fn w(v: &str) -> Wire {
    Wire::Str(v.into())
}

fn map(items: Vec<(&str, Wire)>) -> Wire {
    Wire::Map(items.into_iter().map(|(k, v)| (w(k), v)).collect())
}

fn strs(words: &[&str]) -> Wire {
    Wire::Array(words.iter().map(|x| w(x)).collect())
}

/// The text clause of `text_query` on `index` with the node `q`.
fn clause(index: &str, q: Wire) -> Wire {
    map(vec![
        ("i", w(index)),
        ("q", q),
        ("lg", Wire::Nil),
        ("cl", strs(&[])),
        ("st", Wire::Array(vec![])),
        ("set", Wire::Nil),
    ])
}

/// The queries of the test: each word alone, and two phrases, on each
/// text index of each catalog.
fn queries() -> Vec<(u64, &'static str, Wire)> {
    let mut out = Vec::new();
    for (catalog, index) in [
        (FIRST, "SearchableText"),
        (FIRST, "Title"),
        (SECOND, "Title"),
    ] {
        for word in WORDS {
            let q = map(vec![("n", w("atom")), ("w", strs(&[word]))]);
            out.push((catalog, index, clause(index, q)));
        }
        for pair in [["zelkova", "quillon"], ["marram", "tussock"]] {
            let q = map(vec![("n", w("phrase")), ("w", strs(&pair))]);
            out.push((catalog, index, clause(index, q)));
        }
    }
    out
}

fn first_settings() -> CatalogSettings {
    let mut out = CatalogSettings::default();
    out.indexes
        .insert("SearchableText".into(), IndexSettings::enhanced(&["en"]));
    out.indexes
        .insert("Title".into(), IndexSettings::enhanced(&["en"]));
    out
}

fn second_settings() -> CatalogSettings {
    let mut out = CatalogSettings::default();
    out.indexes
        .insert("Title".into(), IndexSettings::enhanced(&["en", "de"]));
    out
}

fn set<'a>(index: &'a str, rid: i32, language: &'a str, text: &'a str) -> TextOp<'a> {
    TextOp::Set {
        index,
        rid,
        language,
        text,
    }
}

/// The text of `rid` in the corpus: two or three words of `WORDS`.
fn text_of(rid: i32) -> String {
    let n = WORDS.len();
    let at = |k: i32| WORDS[(rid + k).unsigned_abs() as usize % n];
    if rid % 3 == 0 {
        format!("{} {} {}", at(0), at(1), at(3))
    } else {
        format!("{} {}", at(0), at(2))
    }
}

/// Start the mirrors of `store`, with the text indexes in `dir`, and the
/// hook `hooks`.
fn start(store: &Arc<Store>, dir: &Path, hooks: Hooks) -> Mirrors {
    let weak = Arc::downgrade(store);
    let open: Opener = Arc::new(move || weak.upgrade().map(|s| s as Arc<dyn Reads>));
    let config = Config {
        wait: Duration::from_secs(5),
        hooks,
        text: TextConfig {
            dir: Some(dir.to_owned()),
            read_only: false,
        },
        ..Config::default()
    };
    Mirrors::start(open, store.last_transaction().0, config)
}

/// Hooks that count the rebuilds of the text indexes.
fn counting() -> (Arc<AtomicUsize>, Hooks) {
    let builds = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&builds);
    let hooks = Hooks {
        before_text_build: Some(Arc::new(move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
        })),
        ..Hooks::default()
    };
    (builds, hooks)
}

/// Wait until the index of `catalog` holds the deltas up to `tid`.
fn ready_at(mirrors: &Mirrors, catalog: u64, tid: u64) {
    let texts = mirrors.texts().expect("the text indexes");
    assert!(
        texts.wait_for(catalog, WAIT, |c| c
            .is_some_and(|c| c.ready && c.tid >= tid)),
        "the index of {catalog:#x} reaches {tid:#x}: {:?}",
        texts.counts()
    );
}

/// The hits of each query at the last tid, by its place in `queries`.
fn hits(mirrors: &Mirrors, store: &Store) -> Vec<Vec<i32>> {
    let at = store.last_transaction().0 + 1;
    queries()
        .iter()
        .map(|(catalog, _, q)| {
            engine::text_query(mirrors.texts(), store, *catalog, at, q, &Wire::Nil)
                .expect("a clause of the form")
                .expect("hits")
                .rids
        })
        .collect()
}

/// The documents of the index of `catalog`, as its counts give them.
fn docs(mirrors: &Mirrors, catalog: u64) -> u64 {
    let texts = mirrors.texts().expect("the text indexes");
    texts
        .counts()
        .iter()
        .find(|c| c.catalog == catalog)
        .map(|c| c.docs)
        .expect("the counts of the catalog")
}

/// What the test knows of a prepared store.
struct Prepared {
    /// The hits of each query on the indexes that the store maintained.
    hits: Vec<Vec<i32>>,
    /// The live documents of each catalog.
    docs: BTreeMap<u64, u64>,
}

/// Write the store of the test in `dir`: two registered catalogs, their
/// settings, and deltas that the mirrors of the store follow, a clear, an
/// unindex and a change of text among them. Return what the maintained
/// indexes give, after the close of the store.
fn prepare(dir: &Path) -> Prepared {
    let store = Arc::new(open_store(dir));
    testing::commit_text(&store, FIRST, &[], Some(&first_settings()), true).unwrap();
    testing::commit_text(&store, SECOND, &[], Some(&second_settings()), true).unwrap();
    let mirrors = start(&store, dir, Hooks::default());
    let texts: Vec<(i32, String)> = (1..=40).map(|rid| (rid, text_of(rid))).collect();
    let commit = |catalog: u64, ops: &[TextOp<'_>]| {
        let tid = testing::commit_text(&store, catalog, ops, None, false).unwrap();
        mirrors.committed(tid);
        tid
    };
    let ops: Vec<TextOp<'_>> = texts
        .iter()
        .flat_map(|(rid, text)| {
            let mut ops = vec![set("SearchableText", *rid, "en", text)];
            if rid % 2 == 0 {
                ops.push(set("Title", *rid, "en", text));
            }
            ops
        })
        .collect();
    commit(FIRST, &ops);
    // A clear empties `SearchableText`; the first rids come back.
    let mut ops = vec![TextOp::Clear {
        index: "SearchableText",
    }];
    ops.extend(
        texts[..12]
            .iter()
            .map(|(rid, text)| set("SearchableText", *rid, "en", text)),
    );
    commit(FIRST, &ops);
    commit(FIRST, &[set("SearchableText", 7, "en", "gambrel ferrule")]);
    commit(
        FIRST,
        &[
            TextOp::Unindex {
                index: "Title",
                rid: 2,
            },
            TextOp::Unindex {
                index: "SearchableText",
                rid: 3,
            },
        ],
    );
    let ops: Vec<TextOp<'_>> = texts[..20]
        .iter()
        .map(|(rid, text)| {
            let language = if rid % 4 == 0 { "de" } else { "en" };
            set("Title", 100 + rid, language, text)
        })
        .collect();
    commit(SECOND, &ops);
    let last = commit(
        SECOND,
        &[TextOp::Unindex {
            index: "Title",
            rid: 105,
        }],
    );
    ready_at(&mirrors, FIRST, last);
    ready_at(&mirrors, SECOND, last);
    let prepared = Prepared {
        hits: hits(&mirrors, &store),
        docs: [FIRST, SECOND]
            .into_iter()
            .map(|c| (c, docs(&mirrors, c)))
            .collect(),
    };
    // The rids of the first catalog with a live text: 1 to 12 but 3 in
    // `SearchableText`, and the even rids but 2 in `Title`.
    assert_eq!(prepared.docs[&FIRST], 25);
    assert_eq!(prepared.docs[&SECOND], 19);
    // Most queries find something, so the comparison of the hits counts.
    let found = prepared.hits.iter().filter(|h| !h.is_empty()).count();
    assert!(found * 2 > prepared.hits.len(), "{:?}", prepared.hits);
    mirrors.stop();
    drop(mirrors);
    drop(store);
    prepared
}

/// The directory of the index of `catalog` in the store `dir`.
fn index_dir(dir: &Path, catalog: u64) -> PathBuf {
    dir.join("text").join(format!("{catalog:016x}"))
}

/// Each file below `dir` with its bytes, by its path from `dir`, outside
/// the directories `skip`.
fn files(dir: &Path, skip: &[&str]) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut todo = vec![dir.to_owned()];
    while let Some(at) = todo.pop() {
        let Ok(entries) = std::fs::read_dir(&at) else {
            continue;
        };
        for entry in entries {
            let path = entry.unwrap().path();
            let rel = path.strip_prefix(dir).unwrap().to_owned();
            if skip.iter().any(|s| rel == Path::new(s)) {
                continue;
            }
            if path.is_dir() {
                todo.push(path);
            } else {
                out.insert(rel, std::fs::read(&path).unwrap());
            }
        }
    }
    out
}

/// The tables of the store `dir`: each file outside `text/`.
fn tables(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    files(dir, &["text"])
}

/// The ids of the segments of the index in `dir`: the stems of its files
/// of 32 hexadecimal digits.
fn segments(dir: &Path) -> BTreeSet<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| {
            let name = e.unwrap().file_name().into_string().unwrap();
            let stem = name.split('.').next().unwrap_or("").to_owned();
            (stem.len() == 32 && stem.bytes().all(|b| b.is_ascii_hexdigit())).then_some(stem)
        })
        .collect()
}

/// The entries of `text/` in the store `dir`.
fn text_entries(dir: &Path) -> BTreeSet<String> {
    std::fs::read_dir(dir.join("text"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect()
}

/// Assert that `text/` holds the directory of each catalog of `catalogs`
/// and nothing else: no `.new` and no `.old` directory.
fn assert_clean(dir: &Path, catalogs: &[u64]) {
    let expected: BTreeSet<String> = catalogs.iter().map(|c| format!("{c:016x}")).collect();
    assert_eq!(text_entries(dir), expected);
}

/// Run `zoe catalog rebuild-text` with `args` on the store `dir`.
fn rebuild_text(dir: &Path, args: &[&str]) -> Output {
    let mut all = vec!["catalog", "rebuild-text", s(dir)];
    all.extend_from_slice(args);
    zoe(&all, &[])
}

/// Assert that `output` holds no word of a text.
fn assert_no_text(output: &Output) {
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    for word in WORDS {
        assert!(!all.contains(word), "a word of a text in:\n{all}");
    }
}

/// Open the store `dir` with its mirrors, wait until each index of
/// `catalogs` is ready at the last tid, and return the hits of the queries
/// and the number of rebuilds.
fn reopen(dir: &Path, catalogs: &[u64]) -> (Vec<Vec<i32>>, usize, BTreeMap<u64, u64>) {
    let store = Arc::new(open_store(dir));
    let (builds, hooks) = counting();
    let mirrors = start(&store, dir, hooks);
    let last = store.last_transaction().0;
    for catalog in catalogs {
        ready_at(&mirrors, *catalog, last);
    }
    let found = if catalogs.contains(&FIRST) && catalogs.contains(&SECOND) {
        hits(&mirrors, &store)
    } else {
        Vec::new()
    };
    let counts = catalogs.iter().map(|c| (*c, docs(&mirrors, *c))).collect();
    mirrors.stop();
    let builds = builds.load(Ordering::SeqCst);
    drop(mirrors);
    drop(store);
    (found, builds, counts)
}

#[test]
fn the_rebuilt_indexes_give_the_hits_of_the_maintained_ones_and_the_tables_stay() {
    let dir = fresh_dir("rebuild-text");
    let prepared = prepare(&dir);
    let before = tables(&dir);
    let old_first = segments(&index_dir(&dir, FIRST));
    let old_second = segments(&index_dir(&dir, SECOND));

    let output = rebuild_text(&dir, &[]);
    assert_exit(&output, 0, "rebuild-text");
    assert_no_text(&output);
    let report = String::from_utf8(output.stdout.clone()).unwrap();
    for catalog in [FIRST, SECOND] {
        let line = report
            .lines()
            .find(|l| l.contains(&format!("{catalog:#018x}")))
            .unwrap_or_else(|| panic!("no line of {catalog:#x} in:\n{report}"));
        let count = prepared.docs[&catalog].to_string();
        assert!(line.contains(&count), "{count} is not in: {line}");
        assert!(line.contains(" s"), "no time in: {line}");
    }

    // The command writes nothing in the tables.
    assert_eq!(tables(&dir), before, "the tables changed");
    // Each index is new, and nothing else stays in `text/`.
    assert_clean(&dir, &[FIRST, SECOND]);
    assert!(old_first.is_disjoint(&segments(&index_dir(&dir, FIRST))));
    assert!(old_second.is_disjoint(&segments(&index_dir(&dir, SECOND))));

    // The store opens with the new indexes and does not rebuild them.
    let (found, builds, counts) = reopen(&dir, &[FIRST, SECOND]);
    assert_eq!(builds, 0, "the open rebuilt an index");
    assert_eq!(found, prepared.hits);
    assert_eq!(counts, prepared.docs);
}

#[test]
fn the_option_catalog_rebuilds_one_catalog_only() {
    let dir = fresh_dir("rebuild-text-one");
    let prepared = prepare(&dir);
    let second = files(&index_dir(&dir, SECOND), &[]);
    let old_first = segments(&index_dir(&dir, FIRST));

    let oid = format!("{FIRST:#x}");
    let output = rebuild_text(&dir, &["--catalog", &oid]);
    assert_exit(&output, 0, "rebuild-text --catalog");
    assert_no_text(&output);
    let report = String::from_utf8(output.stdout.clone()).unwrap();
    assert!(!report.contains(&format!("{SECOND:#018x}")), "{report}");

    assert_eq!(files(&index_dir(&dir, SECOND), &[]), second);
    assert!(old_first.is_disjoint(&segments(&index_dir(&dir, FIRST))));
    assert_clean(&dir, &[FIRST, SECOND]);
    let (found, builds, _) = reopen(&dir, &[FIRST, SECOND]);
    assert_eq!(builds, 0);
    assert_eq!(found, prepared.hits);

    // A catalog with no text index fails with 1, and nothing changes.
    let before = files(&dir, &[]);
    let output = rebuild_text(&dir, &["--catalog", "0x30"]);
    assert_exit(&output, 1, "rebuild-text of a catalog with no text index");
    assert_eq!(files(&dir, &[]), before);
    // An oid that does not read is a usage error.
    let output = rebuild_text(&dir, &["--catalog", "30"]);
    assert_exit(&output, 2, "rebuild-text with a bad oid");
    assert_eq!(files(&dir, &[]), before);
}

#[test]
fn a_store_that_another_process_holds_is_refused_and_nothing_changes() {
    let dir = fresh_dir("rebuild-text-held");
    prepare(&dir);
    let store = open_store(&dir);
    let before = files(&dir, &[]);
    let output = rebuild_text(&dir, &[]);
    assert_exit(&output, 1, "rebuild-text of a held store");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("locked"), "{stderr}");
    assert_eq!(files(&dir, &[]), before);
    drop(store);
}

#[test]
fn a_registered_catalog_with_no_delta_gets_an_empty_index() {
    let dir = fresh_dir("rebuild-text-empty");
    {
        let store = open_store(&dir);
        testing::commit_text(&store, FIRST, &[], Some(&first_settings()), true).unwrap();
    }
    let before = tables(&dir);
    let output = rebuild_text(&dir, &[]);
    assert_exit(&output, 0, "rebuild-text with no delta");
    assert_eq!(tables(&dir), before);
    assert_clean(&dir, &[FIRST]);
    let (_, builds, counts) = reopen(&dir, &[FIRST]);
    assert_eq!(builds, 0);
    assert_eq!(counts[&FIRST], 0);
}

/// Copy the directory `from` into the new directory `to`.
fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// Run the rebuild of each catalog in a child process that kills itself
/// with SIGKILL at `step` of the first catalog. Return its output.
fn killed_at(dir: &Path, step: Step) -> Output {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "child_rebuilds_until_its_kill", "--nocapture"])
        .env(CHILD_STORE, dir)
        .env(CHILD_STEP, step.name())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    common::output(&mut command)
}

/// The child process of the kills: it rebuilds the text index of each
/// catalog as `zoe catalog rebuild-text` does, and kills itself at the
/// step that its variables name. Outside a child, it does nothing.
#[test]
fn child_rebuilds_until_its_kill() {
    let (Some(dir), Some(step)) = (
        std::env::var_os(CHILD_STORE),
        std::env::var(CHILD_STEP).ok(),
    ) else {
        return;
    };
    let dir = PathBuf::from(dir);
    let store = Store::open_read_only_repairing(&dir, Options::default()).unwrap();
    let catalogs = rebuild::catalogs(&store, None).unwrap();
    for (catalog, settings) in &catalogs {
        let kill = |at: Step| {
            if at.name() == step {
                let pid = std::process::id().to_string();
                let status = Command::new("kill")
                    .args(["-s", "KILL", &pid])
                    .status()
                    .expect("kill runs");
                assert!(status.success(), "kill fails: {status}");
                // The signal comes at once: a child that still runs fails.
                std::thread::sleep(Duration::from_secs(10));
                panic!("the child lives after its kill at {step}");
            }
        };
        rebuild::rebuild(&store, &dir, *catalog, settings, &kill).unwrap();
    }
    panic!("the child reached no step {step}");
}

#[test]
fn a_kill_at_each_step_leaves_the_old_index_or_the_new_one() {
    use std::os::unix::process::ExitStatusExt;

    let base = fresh_dir("rebuild-text-kills");
    let source = base.join("source");
    std::fs::create_dir_all(&source).unwrap();
    let prepared = prepare(&source);
    let old_first = segments(&index_dir(&source, FIRST));
    let old_second = segments(&index_dir(&source, SECOND));
    let before = tables(&source);
    for step in Step::ALL {
        let dir = base.join(step.name());
        copy_dir(&source, &dir);
        let output = killed_at(&dir, step);
        assert_eq!(
            output.status.signal(),
            Some(9),
            "the child dies at {}\nstdout:\n{}\nstderr:\n{}",
            step.name(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            tables(&dir),
            before,
            "the tables changed at {}",
            step.name()
        );
        // What the kill leaves in `text/` for the first catalog, and what
        // the open then serves.
        let name = format!("{FIRST:016x}");
        let (left, expect_old): (&[&str], bool) = match step {
            Step::Created | Step::Added | Step::Committed => (&["", ".new"], true),
            Step::Retired => (&[".new", ".old"], false),
            Step::Placed | Step::Synced => (&["", ".old"], false),
            Step::Removed => (&[""], false),
        };
        let mut expected: BTreeSet<String> = left.iter().map(|s| format!("{name}{s}")).collect();
        expected.insert(format!("{SECOND:016x}"));
        assert_eq!(
            text_entries(&dir),
            expected,
            "text/ after the kill at {}",
            step.name()
        );
        // Until its removal, `.old` holds the whole old index.
        let retired = dir.join("text").join(format!("{name}.old"));
        if retired.is_dir() {
            assert_eq!(
                segments(&retired),
                old_first,
                "the old index at {}",
                step.name()
            );
        }
        let first_left = index_dir(&dir, FIRST).is_dir();

        let (found, builds, counts) = reopen(&dir, &[FIRST, SECOND]);
        // The open removes what the kill left in `text/`.
        assert_clean(&dir, &[FIRST, SECOND]);
        let first = segments(&index_dir(&dir, FIRST));
        if expect_old {
            assert_eq!(first, old_first, "the old index at {}", step.name());
        } else {
            assert!(
                first.is_disjoint(&old_first),
                "a new index at {}",
                step.name()
            );
        }
        // Only a kill between the two renames leaves no index of the first
        // catalog: then the open rebuilds it.
        assert_eq!(
            builds,
            usize::from(!first_left),
            "rebuilds at {}",
            step.name()
        );
        // The second catalog keeps its old index.
        assert_eq!(segments(&index_dir(&dir, SECOND)), old_second);
        assert_eq!(found, prepared.hits, "hits at {}", step.name());
        assert_eq!(counts, prepared.docs, "documents at {}", step.name());
    }
}
