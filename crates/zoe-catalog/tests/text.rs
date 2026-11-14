//! The acceptance tests of the native text index (ADR 0060, sections 10 to
//! 13).
//!
//! A store of `zoe_catalog::testing` holds a registered catalog, its
//! settings in `zoe.text` and the deltas of its text indexes. The mirrors of
//! the store start the text index of the catalog in `text/<oid>/`. The tests
//! hand each commit to the mirrors, as a host does after the publication;
//! a commit that the test does not hand over leaves the index behind.
//!
//! `tests/data/text.json` keeps the hits and the integer scores of the
//! queries of `corpus_queries` on the corpus of `corpus`. The ignored test
//! `write_the_corpus_data` writes it again.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value as Json, json};
use zoe_catalog::testing::{self, CatalogModel, IndexKind, TextOp};
use zoe_catalog::text::clause::{self, TextHits};
use zoe_catalog::text::{
    CallError, CatalogSettings, IndexSettings, Query, Strategy, TextConfig, TextError,
    TextErrorKind, Want, engine,
};
use zoe_catalog::value::Val;
use zoe_catalog::{Config, Hooks, Mirrors, Opener, Reads, Wire};
use zoe_core::testing::{TempDir, fresh_dir};
use zoe_core::{Oid, Store, Tid};

const WAIT: Duration = Duration::from_secs(30);

/// The oid of the catalog object of the tests without a mirror.
const CATALOG: u64 = 0x100;

fn s(v: &str) -> Wire {
    Wire::Str(v.into())
}

fn map(items: Vec<(&str, Wire)>) -> Wire {
    Wire::Map(items.into_iter().map(|(k, v)| (s(k), v)).collect())
}

fn strs(words: &[&str]) -> Wire {
    Wire::Array(words.iter().map(|w| s(w)).collect())
}

fn atom(words: &[&str]) -> Wire {
    map(vec![("n", s("atom")), ("w", strs(words))])
}

fn phrase(words: &[&str]) -> Wire {
    map(vec![("n", s("phrase")), ("w", strs(words))])
}

fn glob(pattern: &str) -> Wire {
    map(vec![("n", s("glob")), ("p", s(pattern))])
}

fn and(items: Vec<Wire>, nots: Vec<Wire>) -> Wire {
    map(vec![
        ("n", s("and")),
        ("items", Wire::Array(items)),
        ("nots", Wire::Array(nots)),
    ])
}

fn or(items: Vec<Wire>) -> Wire {
    map(vec![("n", s("or")), ("items", Wire::Array(items))])
}

/// A staged text for `st`.
fn staged(index: &str, rid: i32, text: Option<&str>) -> Wire {
    Wire::Array(vec![
        s(index),
        Wire::Int(i64::from(rid)),
        s("en"),
        text.map_or(Wire::Nil, s),
    ])
}

/// The text clause of `text_query` on `index` with `q`.
fn clause_of(index: &str, q: Wire) -> Wire {
    clause_with(index, q, vec![], vec![], Wire::Nil)
}

fn clause_with(index: &str, q: Wire, cl: Vec<&str>, st: Vec<Wire>, set: Wire) -> Wire {
    map(vec![
        ("i", s(index)),
        ("q", q),
        ("lg", Wire::Nil),
        ("cl", strs(&cl)),
        ("st", Wire::Array(st)),
        ("set", set),
    ])
}

/// The clause of a plan of `catalog_query`: the clause with `"k": "t"`.
fn plan_clause(index: &str, q: Wire) -> Wire {
    let Wire::Map(mut items) = clause_of(index, q) else {
        unreachable!()
    };
    items.insert(0, (s("k"), s("t")));
    Wire::Map(items)
}

/// The settings of the stock indexes of Plone: `SearchableText` with the
/// boosts `Title^3` and `Description^1.5`, `Title`, `Description`, and
/// `Body` with fuzzy matching at the distance 1.
fn settings(languages: &[&str]) -> CatalogSettings {
    let mut out = CatalogSettings::default();
    let mut searchable = IndexSettings::enhanced(languages);
    searchable.boosts = vec![
        ("Title".into(), 3.0),
        ("Description".into(), 1.5),
        ("SearchableText".into(), 1.0),
    ];
    out.indexes.insert("SearchableText".into(), searchable);
    out.indexes
        .insert("Title".into(), IndexSettings::enhanced(languages));
    out.indexes
        .insert("Description".into(), IndexSettings::enhanced(languages));
    let mut body = IndexSettings::enhanced(languages);
    body.fuzzy = 1;
    out.indexes.insert("Body".into(), body);
    out
}

/// A store in a directory of its own, with the text index of its catalog.
/// The fields drop in their order: the mirrors, the store, the directory.
struct Site {
    mirrors: Mirrors,
    store: Arc<Store>,
    catalog: u64,
    dir: TempDir,
}

impl Drop for Site {
    fn drop(&mut self) {
        self.mirrors.stop();
    }
}

impl Site {
    /// A new store with the catalog `CATALOG`, registered, and its
    /// settings, with no mirror of its trees.
    fn new(name: &str, settings: &CatalogSettings) -> Site {
        Site::open(name, settings, zoe_core::Options::default())
    }

    /// A new store as `new` gives it, whose pack keeps the deltas of a
    /// catalog that no record reaches: the catalog of these tests has no
    /// record.
    fn without_gc(name: &str, settings: &CatalogSettings) -> Site {
        let options = zoe_core::Options {
            pack_gc: false,
            ..zoe_core::Options::default()
        };
        Site::open(name, settings, options)
    }

    fn open(name: &str, settings: &CatalogSettings, options: zoe_core::Options) -> Site {
        let dir = fresh_dir(name);
        let store = Arc::new(Store::open_with(dir.path(), options).unwrap());
        testing::commit_text(&store, CATALOG, &[], Some(settings), true).unwrap();
        let mirrors = start(
            &store,
            dir.path(),
            false,
            Duration::from_secs(1),
            Hooks::default(),
        );
        Site {
            dir,
            store,
            mirrors,
            catalog: CATALOG,
        }
    }

    /// Commit `ops` and hand the commit to the mirrors.
    fn commit(&self, ops: &[TextOp<'_>]) -> u64 {
        let tid = self.commit_quietly(ops);
        self.mirrors.committed(tid);
        tid
    }

    /// Commit `ops` and do not hand the commit to the mirrors: the index
    /// stays behind.
    fn commit_quietly(&self, ops: &[TextOp<'_>]) -> u64 {
        testing::commit_text(&self.store, self.catalog, ops, None, false).unwrap()
    }

    /// Wait until the index holds the deltas up to `tid`.
    fn ready_at(&self, tid: u64) {
        let texts = self.mirrors.texts().expect("the text indexes");
        assert!(
            texts.wait_for(self.catalog, WAIT, |c| c
                .is_some_and(|c| c.ready && c.tid >= tid)),
            "the index reaches {tid:#x}: {:?}",
            texts.counts()
        );
    }

    /// `text_query` at the exclusive tid `tid`.
    fn query(
        &self,
        tid: u64,
        clause: &Wire,
        filter: Option<&[i32]>,
    ) -> Result<TextHits, TextError> {
        let filter = filter.map_or(Wire::Nil, |f| clause::rids_bin(f.iter().copied()));
        engine::text_query(
            self.mirrors.texts(),
            &self.store,
            self.catalog,
            tid,
            clause,
            &filter,
        )
        .expect("a clause of the form")
    }

    /// The rids of `text_query` at `tid`.
    fn rids(&self, tid: u64, clause: &Wire) -> BTreeSet<i32> {
        self.query(tid, clause, None)
            .expect("hits")
            .rids
            .into_iter()
            .collect()
    }

    /// The next exclusive tid.
    fn now(&self) -> u64 {
        self.store.last_transaction().0 + 1
    }

    /// Stop the mirrors, then start them again on the same store.
    fn restart(&mut self, hooks: Hooks) {
        self.mirrors.stop();
        self.mirrors = start(
            &self.store,
            self.dir.path(),
            false,
            Duration::from_secs(1),
            hooks,
        );
    }

    /// Stop the mirrors, then start them again with the hand-offs up to
    /// `published` only: the index follows no later commit until the test
    /// hands it one.
    fn restart_at(&mut self, hooks: Hooks, published: u64) {
        self.mirrors.stop();
        let weak = Arc::downgrade(&self.store);
        let open: Opener = Arc::new(move || weak.upgrade().map(|s| s as Arc<dyn Reads>));
        let config = Config {
            wait: Duration::from_secs(1),
            hooks,
            text: TextConfig {
                dir: Some(self.dir.path().to_owned()),
                read_only: false,
            },
            ..Config::default()
        };
        self.mirrors = Mirrors::start(open, published, config);
    }

    fn text_dir(&self) -> PathBuf {
        self.dir
            .path()
            .join("text")
            .join(format!("{:016x}", self.catalog))
    }
}

/// Start the mirrors of `store`, with its text indexes in `dir`.
fn start(store: &Arc<Store>, dir: &Path, read_only: bool, wait: Duration, hooks: Hooks) -> Mirrors {
    let weak = Arc::downgrade(store);
    let open: Opener = Arc::new(move || weak.upgrade().map(|s| s as Arc<dyn Reads>));
    let config = Config {
        wait,
        hooks,
        text: TextConfig {
            dir: Some(dir.to_owned()),
            read_only,
        },
        ..Config::default()
    };
    Mirrors::start(open, store.last_transaction().0, config)
}

fn set<'a>(index: &'a str, rid: i32, text: &'a str) -> TextOp<'a> {
    TextOp::Set {
        index,
        rid,
        language: "en",
        text,
    }
}

fn unindex(index: &str, rid: i32) -> TextOp<'_> {
    TextOp::Unindex { index, rid }
}

#[test]
fn committed_deltas_come_into_the_hits_within_the_lag() {
    let site = Site::new("text-lag", &settings(&["en"]));
    let tid = site.commit(&[set("Title", 7, "kalo mite"), set("Title", 9, "ruso")]);
    site.ready_at(tid);
    let texts = site.mirrors.texts().unwrap();
    assert!(texts.wait_for(site.catalog, WAIT, |c| {
        c.is_some_and(|c| c.lag_seconds == 0.0)
    }));
    assert_eq!(
        site.rids(site.now(), &clause_of("Title", atom(&["kalo"]))),
        BTreeSet::from([7])
    );
    let counts = site.mirrors.counts();
    let text = counts
        .texts
        .iter()
        .find(|c| c.catalog == site.catalog)
        .unwrap();
    assert_eq!(text.lag_seconds, 0.0);
    assert_eq!(text.docs, 2);
}

/// The live texts of `Title` at each tid.
#[derive(Clone, Default)]
struct Model {
    texts: BTreeMap<i32, String>,
}

impl Model {
    fn apply(&mut self, ops: &[TextOp<'_>]) {
        for op in ops {
            match op {
                TextOp::Set { rid, text, .. } => {
                    self.texts.insert(*rid, (*text).to_owned());
                }
                TextOp::Unindex { rid, .. } => {
                    self.texts.remove(rid);
                }
                TextOp::Clear { .. } => self.texts.clear(),
            }
        }
    }

    fn with(&self, word: &str) -> BTreeSet<i32> {
        self.texts
            .iter()
            .filter(|(_, t)| t.split(' ').any(|w| w == word))
            .map(|(r, _)| *r)
            .collect()
    }
}

#[test]
fn the_matched_set_is_the_set_of_the_live_texts_at_each_snapshot() {
    let site = Site::new("text-snapshots", &settings(&["en"]));
    let steps: Vec<Vec<TextOp<'_>>> = vec![
        vec![
            set("Title", 1, "kalo mite"),
            set("Title", 2, "kalo"),
            set("Title", 3, "mite ruso"),
        ],
        vec![set("Title", 1, "ruso"), unindex("Title", 2)],
        vec![set("Title", 4, "kalo ruso"), set("Title", -5, "mite")],
        vec![unindex("Title", 3), set("Title", 2, "mite kalo")],
        vec![unindex("Title", 4)],
    ];
    let words = ["kalo", "mite", "ruso"];
    let mut model = Model::default();
    let mut snapshots = Vec::new();
    // The first commits follow the index; the last two leave it behind.
    for (n, ops) in steps.iter().enumerate() {
        let tid = if n < 3 {
            site.commit(ops)
        } else {
            site.commit_quietly(ops)
        };
        if n == 2 {
            site.ready_at(tid);
        }
        model.apply(ops);
        snapshots.push((tid + 1, model.clone()));
    }
    // The index holds the third commit: the snapshots before it read the
    // history, the snapshots after it the commits that it lacks.
    for (tid, model) in &snapshots {
        for word in words {
            let found = site.rids(*tid, &clause_of("Title", atom(&[word])));
            assert_eq!(found, model.with(word), "{word} at {tid:#x}");
        }
    }
    // The index follows the last commits: the same sets, the document
    // deleted after a snapshot included.
    let last = site.store.last_transaction().0;
    site.mirrors.committed(last);
    site.ready_at(last);
    for (tid, model) in &snapshots {
        for word in words {
            let found = site.rids(*tid, &clause_of("Title", atom(&[word])));
            assert_eq!(found, model.with(word), "{word} at {tid:#x}");
        }
    }
}

#[test]
fn staged_texts_replace_their_documents_for_their_query_only() {
    let site = Site::new("text-staged", &settings(&["en"]));
    let tid = site.commit(&[set("Title", 1, "kalo"), set("Title", 2, "kalo mite")]);
    site.ready_at(tid);
    let now = site.now();
    let staging = clause_with(
        "Title",
        atom(&["kalo"]),
        vec![],
        vec![
            staged("Title", 1, Some("ruso")),
            staged("Title", 3, Some("kalo")),
        ],
        Wire::Nil,
    );
    let found: BTreeSet<i32> = site
        .query(now, &staging, None)
        .unwrap()
        .rids
        .into_iter()
        .collect();
    assert_eq!(found, BTreeSet::from([2, 3]));
    // An unindex staged.
    let unindexing = clause_with(
        "Title",
        atom(&["kalo"]),
        vec![],
        vec![staged("Title", 2, None)],
        Wire::Nil,
    );
    let found: BTreeSet<i32> = site
        .query(now, &unindexing, None)
        .unwrap()
        .rids
        .into_iter()
        .collect();
    assert_eq!(found, BTreeSet::from([1]));
    // Another query sees none of them.
    assert_eq!(
        site.rids(now, &clause_of("Title", atom(&["kalo"]))),
        BTreeSet::from([1, 2])
    );
    // An index in `cl` answers from the staged texts alone, with the
    // settings of `set`.
    let set = map(vec![
        ("mode", s("enhanced")),
        ("language", strs(&["en"])),
        ("boosts", Wire::Map(vec![])),
    ]);
    let cleared = clause_with(
        "Title",
        atom(&["kalo"]),
        vec!["Title"],
        vec![
            staged("Title", 8, Some("kalo kalo")),
            staged("Title", 9, Some("mite")),
        ],
        set,
    );
    let hits = site.query(now, &cleared, None).unwrap();
    assert_eq!(hits.rids, vec![8]);
    assert!(hits.scores[0] > 0);
}

#[test]
fn an_index_in_cl_needs_no_index_of_the_catalog() {
    let dir = fresh_dir("text-alone");
    let store = Store::open(dir.path()).unwrap();
    testing::commit_text(&store, CATALOG, &[], None, false).unwrap();
    let set = map(vec![
        ("mode", s("enhanced")),
        ("language", strs(&["en"])),
        ("boosts", Wire::Map(vec![])),
    ]);
    let cleared = clause_with(
        "SearchableText",
        or(vec![atom(&["kalo"]), atom(&["ruso"])]),
        vec!["SearchableText"],
        vec![
            staged("SearchableText", 1, Some("kalo mite")),
            staged("SearchableText", 2, Some("ruso ruso")),
            staged("SearchableText", 3, Some("mite")),
        ],
        set,
    );
    let tid = store.last_transaction().0 + 1;
    let hits = engine::text_query(None, &store, CATALOG, tid, &cleared, &Wire::Nil)
        .unwrap()
        .unwrap();
    assert_eq!(hits.rids, vec![1, 2]);
    // Without `cl`, a catalog with no index is unavailable at once.
    let plain = clause_of("SearchableText", atom(&["kalo"]));
    let failed = engine::text_query(None, &store, CATALOG, tid, &plain, &Wire::Nil).unwrap();
    assert_eq!(failed.unwrap_err().kind, TextErrorKind::Unavailable);
}

#[test]
fn a_packed_or_a_cleared_snapshot_is_refused() {
    let site = Site::new("text-refusals", &settings(&["en"]));
    let t1 = site.commit(&[set("Title", 1, "kalo")]);
    let t2 = site.commit(&[set("Title", 2, "kalo")]);
    site.ready_at(t2);
    site.store.pack(Tid(t1)).unwrap();
    let watermark = site.store.pack_watermark().unwrap().0;
    assert!(watermark >= t1);
    let q = clause_of("Title", atom(&["kalo"]));
    assert_eq!(
        site.query(watermark, &q, None).unwrap_err().kind,
        TextErrorKind::Packed
    );
    assert_eq!(site.rids(watermark + 1, &q).len(), 1);
    // A clear delta: the index rebuilds, and a snapshot before the clear
    // is refused.
    let t3 = site.commit(&[TextOp::Clear { index: "Title" }, set("Title", 3, "kalo")]);
    site.ready_at(t3);
    assert_eq!(
        site.query(t3, &q, None).unwrap_err().kind,
        TextErrorKind::Cleared
    );
    assert_eq!(site.rids(t3 + 1, &q), BTreeSet::from([3]));
}

/// The rids and the scores of a set of queries on SearchableText.
fn answers(site: &Site, tid: u64) -> Vec<TextHits> {
    [
        atom(&["kalo"]),
        atom(&["mite"]),
        phrase(&["kalo", "mite"]),
        glob("ru*"),
    ]
    .into_iter()
    .map(|q| {
        site.query(tid, &clause_of("SearchableText", q), None)
            .unwrap()
    })
    .collect()
}

#[test]
fn a_rebuilt_index_gives_the_hits_of_the_index_that_followed_the_commits() {
    let mut site = Site::new("text-rebuilds", &settings(&["en"]));
    let builds = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&builds);
    let hooks = || Hooks {
        before_text_build: Some(Arc::new({
            let c = Arc::clone(&counter);
            move |_| {
                c.fetch_add(1, Ordering::SeqCst);
            }
        })),
        ..Hooks::default()
    };
    site.restart(hooks());
    let mut last = 0;
    for i in 0..40 {
        let text = format!(
            "kalo mite {} ruso{}",
            if i % 3 == 0 { "kalo" } else { "vite" },
            i % 5
        );
        last = site.commit(&[
            set("SearchableText", i, &text),
            set("Title", i, if i % 4 == 0 { "kalo" } else { "mite" }),
        ]);
    }
    last = last.max(site.commit(&[unindex("SearchableText", 5)]));
    site.ready_at(last);
    // The rebuilt index holds no deleted document, so its statistics, and
    // its scores, can differ: the hits are the same.
    let sets =
        |hits: Vec<TextHits>| -> Vec<Vec<i32>> { hits.into_iter().map(|h| h.rids).collect() };
    let followed = sets(answers(&site, site.now()));
    let built = builds.load(Ordering::SeqCst);
    // A missing directory.
    site.mirrors.stop();
    std::fs::remove_dir_all(site.text_dir()).unwrap();
    site.restart(hooks());
    site.ready_at(last);
    assert_eq!(sets(answers(&site, site.now())), followed);
    assert_eq!(builds.load(Ordering::SeqCst), built + 1);
    // A damaged file.
    site.mirrors.stop();
    let file = std::fs::read_dir(site.text_dir())
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|e| e == "idx"))
        .expect("a file of postings");
    let mut bytes = std::fs::read(&file).unwrap();
    bytes[0] ^= 0xff;
    std::fs::write(&file, bytes).unwrap();
    site.restart(hooks());
    site.ready_at(last);
    assert_eq!(sets(answers(&site, site.now())), followed);
    assert_eq!(builds.load(Ordering::SeqCst), built + 2);
    // A clean open does not rebuild.
    site.restart(hooks());
    site.ready_at(last);
    assert_eq!(builds.load(Ordering::SeqCst), built + 2);
    // A change of language rebuilds; the words of the test have one form.
    let t = testing::commit_text(
        &site.store,
        site.catalog,
        &[],
        Some(&settings(&["de"])),
        false,
    )
    .unwrap();
    site.mirrors.committed(t);
    assert!(
        site.mirrors
            .texts()
            .unwrap()
            .wait_for(site.catalog, WAIT, |_| builds.load(Ordering::SeqCst)
                == built + 3)
    );
    site.ready_at(t);
    assert_eq!(sets(answers(&site, site.now())), followed);
    // A clear delta rebuilds from the live texts.
    let t = site.commit(&[
        TextOp::Clear {
            index: "SearchableText",
        },
        set("SearchableText", 100, "kalo"),
    ]);
    site.ready_at(t);
    assert_eq!(builds.load(Ordering::SeqCst), built + 4);
    let q = clause_of("SearchableText", atom(&["mite"]));
    let found: BTreeSet<i32> = site
        .query(site.now(), &q, None)
        .unwrap()
        .rids
        .into_iter()
        .collect();
    // Only the boosts of Title still hold the word.
    let titles: BTreeSet<i32> = (0..40).filter(|i| i % 4 != 0).collect();
    assert_eq!(found, titles);
}

/// A hook that holds the thread until the test releases it.
#[derive(Clone, Default)]
struct Gate {
    open: Arc<(Mutex<bool>, std::sync::Condvar)>,
}

impl Gate {
    fn hook(&self, reached: mpsc::Sender<()>) -> zoe_catalog::mirrors::Hook {
        let gate = self.clone();
        let reached = Mutex::new(reached);
        Arc::new(move |_| {
            let _ = reached.lock().unwrap().send(());
            let (lock, cv) = &*gate.open;
            let mut open = lock.lock().unwrap();
            while !*open {
                open = cv.wait(open).unwrap();
            }
        })
    }

    fn release(&self) {
        let (lock, cv) = &*self.open;
        *lock.lock().unwrap() = true;
        cv.notify_all();
    }
}

#[test]
fn a_query_waits_for_a_rebuild_then_gets_unavailable() {
    let dir = fresh_dir("text-wait");
    let store = Arc::new(Store::open(dir.path()).unwrap());
    testing::commit_text(
        &store,
        CATALOG,
        &[set("Title", 1, "kalo")],
        Some(&settings(&["en"])),
        true,
    )
    .unwrap();
    let gate = Gate::default();
    let (tx, reached) = mpsc::channel();
    let hooks = Hooks {
        before_text_build: Some(gate.hook(tx)),
        ..Hooks::default()
    };
    let wait = Duration::from_millis(300);
    let mirrors = start(&store, dir.path(), false, wait, hooks);
    reached.recv_timeout(WAIT).expect("the rebuild starts");
    // The lag grows while the rebuild runs.
    std::thread::sleep(Duration::from_millis(50));
    let lag = mirrors.counts().texts[0].lag_seconds;
    assert!(lag > 0.0, "{lag}");
    let tid = store.last_transaction().0 + 1;
    let q = clause_of("Title", atom(&["kalo"]));
    let started = Instant::now();
    let failed = engine::text_query(mirrors.texts(), &store, CATALOG, tid, &q, &Wire::Nil).unwrap();
    assert_eq!(failed.unwrap_err().kind, TextErrorKind::Unavailable);
    assert!(started.elapsed() >= wait);
    // A catalog with no index is unavailable at once.
    let started = Instant::now();
    let other =
        engine::text_query(mirrors.texts(), &store, CATALOG + 1, tid, &q, &Wire::Nil).unwrap();
    assert_eq!(other.unwrap_err().kind, TextErrorKind::Unavailable);
    assert!(started.elapsed() < wait);
    gate.release();
    let hits = loop {
        let found =
            engine::text_query(mirrors.texts(), &store, CATALOG, tid, &q, &Wire::Nil).unwrap();
        if let Ok(hits) = found {
            break hits;
        }
        assert!(started.elapsed() < WAIT);
    };
    assert_eq!(hits.rids, vec![1]);
    mirrors.stop();
}

#[test]
fn a_damaged_delta_makes_each_text_query_corrupt() {
    let mut site = Site::new("text-corrupt", &settings(&["en"]));
    let tid = site.commit(&[set("Title", 1, "kalo"), set("Description", 2, "mite")]);
    site.ready_at(tid);
    site.mirrors.stop();
    let key = zoe_core::testing::text_delta_key(Oid(site.catalog), "Title", Some(1), Tid(tid));
    let mut frame = zoe_core::testing::read_text_frame(&site.store, &key).unwrap();
    let last = frame.len() - 1;
    frame[last] ^= 0x55;
    zoe_core::testing::write_text_frame(&site.store, &key, Some(&frame)).unwrap();
    std::fs::remove_dir_all(site.text_dir()).unwrap();
    site.restart(Hooks::default());
    let now = site.now();
    for q in [
        clause_of("Title", atom(&["kalo"])),
        clause_of("Description", atom(&["mite"])),
    ] {
        let failed = loop {
            match site.query(now, &q, None) {
                Err(e) if e.kind == TextErrorKind::Corrupt => break e,
                Err(e) if e.kind == TextErrorKind::Unavailable => {}
                other => panic!("{other:?}"),
            }
        };
        assert!(failed.why.contains("Title"), "{}", failed.why);
    }
}

/// The names, the sizes and the times of the files of `dir`.
fn listing(dir: &Path) -> BTreeMap<String, (u64, std::time::SystemTime)> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            let m = e.metadata().unwrap();
            (
                e.file_name().to_string_lossy().into_owned(),
                (m.len(), m.modified().unwrap()),
            )
        })
        .collect()
}

#[test]
fn a_read_only_open_writes_nothing_in_text() {
    let dir = fresh_dir("text-read-only");
    {
        let store = Arc::new(Store::open(dir.path()).unwrap());
        let tid = testing::commit_text(
            &store,
            CATALOG,
            &[set("Title", 1, "kalo"), set("Title", 2, "mite")],
            Some(&settings(&["en"])),
            true,
        )
        .unwrap();
        let mirrors = start(
            &store,
            dir.path(),
            false,
            Duration::from_secs(1),
            Hooks::default(),
        );
        let texts = mirrors.texts().unwrap();
        assert!(texts.wait_for(CATALOG, WAIT, |c| {
            c.is_some_and(|c| c.ready && c.tid >= tid)
        }));
        mirrors.stop();
    }
    let text = dir.path().join("text");
    let index_dir = text.join(format!("{CATALOG:016x}"));
    let before = (listing(&text), listing(&index_dir));
    std::thread::sleep(Duration::from_millis(20));
    let store = Arc::new(Store::open_read_only(dir.path()).unwrap());
    let mirrors = start(
        &store,
        dir.path(),
        true,
        Duration::from_secs(5),
        Hooks::default(),
    );
    let tid = store.last_transaction().0 + 1;
    let q = clause_of("Title", atom(&["kalo"]));
    let hits = engine::text_query(mirrors.texts(), &store, CATALOG, tid, &q, &Wire::Nil)
        .unwrap()
        .unwrap();
    assert_eq!(hits.rids, vec![1]);
    mirrors.stop();
    drop(mirrors);
    drop(store);
    assert_eq!((listing(&text), listing(&index_dir)), before);
}

/// The words of the corpus: two syllables of a fixed list, in the order of
/// a generator with a fixed seed.
fn corpus() -> Vec<(i32, String, String, String)> {
    let syllables = ["ka", "lo", "mi", "te", "ru", "so", "va", "ne"];
    let mut seed: u64 = 0x2545_f491_4f6c_dd1d;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let mut word = || {
        let a = syllables[(next() % 8) as usize];
        let b = syllables[(next() % 8) as usize];
        format!("{a}{b}")
    };
    let mut docs = Vec::new();
    for rid in 0..60 {
        let n = 4 + (rid as usize % 7);
        let body: Vec<String> = (0..n).map(|_| word()).collect();
        let title = word();
        let description = word();
        docs.push((rid - 20, title, description, body.join(" ")));
    }
    // The words of the features: stems, accents, stop words, phrases.
    docs.push((
        90,
        "running".into(),
        "the café".into(),
        "runs and ran kalo mite".into(),
    ));
    docs.push((
        91,
        "runner".into(),
        "cafes".into(),
        "kalo of the mite ruso".into(),
    ));
    docs.push((92, "Café".into(), "run".into(), "mite kalo ruso".into()));
    docs
}

/// The queries of the corpus: a name, the index and the parse tree.
fn corpus_queries() -> Vec<(&'static str, &'static str, Wire)> {
    vec![
        ("stemming", "SearchableText", atom(&["running"])),
        ("folding", "Description", atom(&["cafe"])),
        ("stop_word", "SearchableText", atom(&["the"])),
        ("phrase_two", "Body", phrase(&["kalo", "mite"])),
        (
            "phrase_three",
            "Body",
            phrase(&["kalo", "the", "mite", "ruso"]),
        ),
        ("prefix", "Body", glob("kal*")),
        ("fuzzy", "Body", atom(&["kalp"])),
        ("boosts", "SearchableText", atom(&["kalo"])),
        (
            "and_not",
            "Body",
            and(vec![atom(&["mite"])], vec![atom(&["ruso"])]),
        ),
        ("or", "Body", or(vec![atom(&["kasa"]), atom(&["lomi"])])),
    ]
}

/// The store of the corpus, ready.
fn corpus_site(name: &str) -> Site {
    let site = Site::new(name, &settings(&["en"]));
    let docs = corpus();
    let mut ops = Vec::new();
    for (rid, title, description, body) in &docs {
        let searchable = format!("{title} {description} {body}");
        ops.push(TextOp::Set {
            index: "SearchableText",
            rid: *rid,
            language: "en",
            text: Box::leak(searchable.into_boxed_str()),
        });
        ops.push(set("Title", *rid, title));
        ops.push(set("Description", *rid, description));
        ops.push(set("Body", *rid, body));
    }
    let tid = site.commit(&ops);
    site.ready_at(tid);
    site
}

fn hits_json(hits: &TextHits) -> Json {
    let mut by_score: Vec<(i32, i32)> = hits
        .rids
        .iter()
        .copied()
        .zip(hits.scores.iter().copied())
        .collect();
    by_score.sort_by_key(|a| std::cmp::Reverse((a.1, a.0)));
    Json::Array(by_score.into_iter().map(|(r, s)| json!([r, s])).collect())
}

fn data_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/text.json")
}

#[test]
#[ignore = "writes tests/data/text.json"]
fn write_the_corpus_data() {
    let site = corpus_site("text-corpus-write");
    let mut out = serde_json::Map::new();
    for (name, index, q) in corpus_queries() {
        let hits = site.query(site.now(), &clause_of(index, q), None).unwrap();
        out.insert(name.into(), hits_json(&hits));
    }
    // One query on each line.
    let lines: Vec<String> = out
        .iter()
        .map(|(name, hits)| format!("  {}: {hits}", Json::String(name.clone())))
        .collect();
    std::fs::write(data_path(), format!("{{\n{}\n}}\n", lines.join(",\n"))).unwrap();
}

#[test]
fn the_corpus_gives_the_hits_and_the_scores_of_the_test_data() {
    let site = corpus_site("text-corpus");
    let data: Json = serde_json::from_str(&std::fs::read_to_string(data_path()).unwrap()).unwrap();
    for (name, index, q) in corpus_queries() {
        let hits = site.query(site.now(), &clause_of(index, q), None).unwrap();
        assert_eq!(hits_json(&hits), data[name], "{name}");
    }
    let stem = &data["stemming"];
    let rids: BTreeSet<i64> = stem
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h[0].as_i64().unwrap())
        .collect();
    // `runner` has a stem of its own.
    assert_eq!(rids, BTreeSet::from([90, 92]));
    assert_eq!(data["stop_word"], json!([]));
    let folded: BTreeSet<i64> = data["folding"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h[0].as_i64().unwrap())
        .collect();
    assert!(folded.is_superset(&BTreeSet::from([90, 91])));
    let three: Vec<i64> = data["phrase_three"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h[0].as_i64().unwrap())
        .collect();
    assert_eq!(three, vec![91]);
}

/// A catalog of `n` entries with a FieldIndex `kind`, a FieldIndex `num`
/// with a distinct int for each entry, and the text of each entry in
/// `Title`, which has no fuzzy matching: `kalo` in each entry, `mite` in one entry in 10, and `ruso` in
/// one entry in 50.
struct Mirrored {
    site: Site,
    model: CatalogModel,
    kinds: BTreeMap<i32, &'static str>,
}

fn mirrored(name: &str, n: i32) -> Mirrored {
    let dir = fresh_dir(name);
    let store = Arc::new(Store::open(dir.path()).unwrap());
    let mut model = CatalogModel::new(
        0x1000,
        &[("kind", IndexKind::Field), ("num", IndexKind::Field)],
        &["getId"],
    );
    let mut kinds = BTreeMap::new();
    let mut ops_texts = Vec::new();
    for i in 0..n {
        let rid = 1000 + 3 * i;
        let kind = if i % 2 == 0 { "even" } else { "odd" };
        let kind = if i < n / 40 { "rare" } else { kind };
        kinds.insert(rid, kind);
        model.catalog(
            rid,
            &format!("/site/item-{i}"),
            &[
                ("kind", Val::Str(kind.into())),
                ("num", Val::Int(i64::from(n - i))),
                ("getId", Val::Str(format!("item-{i}").into())),
            ],
        );
        let mut text = String::from("kalo");
        for _ in 0..(i % 4) {
            text.push_str(" kalo");
        }
        if i % 10 == 0 {
            text.push_str(" mite");
        }
        if i % 50 == 0 {
            text.push_str(" ruso");
        }
        text.push_str(&format!(" filler{}", i % 7));
        ops_texts.push((rid, text));
    }
    let catalog = model.oid();
    testing::commit(&store, &model.records(), &[model.key()], &[]).unwrap();
    let ops: Vec<TextOp<'_>> = ops_texts
        .iter()
        .map(|(rid, text)| set("Title", *rid, text))
        .collect();
    let tid = testing::commit_text(&store, catalog, &ops, Some(&settings(&["en"])), false).unwrap();
    let mirrors = start(
        &store,
        dir.path(),
        false,
        Duration::from_secs(5),
        Hooks::default(),
    );
    assert!(mirrors.wait_for(catalog, WAIT, |s| {
        s.is_some_and(|s| s.state == "ready" && s.applied.is_some_and(|a| a >= tid))
    }));
    let site = Site {
        dir,
        store,
        mirrors,
        catalog,
    };
    site.ready_at(tid);
    Mirrored { site, model, kinds }
}

/// A plan of `catalog_query` with the clause `kind == kind`, the text
/// clause `text`, a sort on `num` when `sort`, and `b_start` and `b_size`.
fn text_plan(
    kind: Option<&str>,
    text: Wire,
    sort: bool,
    b_start: i64,
    b_size: Option<i64>,
) -> Wire {
    let mut clauses = Vec::new();
    if let Some(kind) = kind {
        clauses.push(map(vec![
            ("k", s("v")),
            ("i", s("kind")),
            ("q", Wire::Array(vec![s(kind)])),
            ("and", Wire::Bool(false)),
            ("r", Wire::Nil),
            ("not", Wire::Array(vec![])),
        ]));
    }
    clauses.push(text);
    let sorts = if sort {
        Wire::Array(vec![Wire::Array(vec![s("num"), Wire::Bool(false)])])
    } else {
        Wire::Array(vec![])
    };
    map(vec![
        ("c", Wire::Array(clauses)),
        ("s", sorts),
        ("l", Wire::Nil),
        ("bs", Wire::Int(b_start)),
        ("bz", b_size.map_or(Wire::Nil, Wire::Int)),
        ("cols", Wire::Nil),
        ("rn", Wire::Int(0)),
    ])
}

fn bin_ints(wire: &Wire) -> Vec<i32> {
    match wire {
        Wire::Bin(b) => b
            .chunks(4)
            .map(|c| i32::from_be_bytes(c.try_into().unwrap()))
            .collect(),
        other => panic!("{other:?}"),
    }
}

fn get<'w>(wire: &'w Wire, key: &str) -> Option<&'w Wire> {
    match wire {
        Wire::Map(items) => items
            .iter()
            .find(|(k, _)| *k == Wire::Str(key.into()))
            .map(|(_, v)| v),
        _ => None,
    }
}

#[test]
fn a_text_clause_with_a_filter_gives_the_hits_filtered_after_the_scores() {
    let m = mirrored("text-filters", 800);
    let site = &m.site;
    let now = site.now();
    // Every hit of each query, by score then rid descending, with no
    // filter: the reference.
    let all = |q: &Wire| -> Vec<(i64, i32)> {
        let c = zoe_catalog::text::clause::decode_clause(&clause_of("Title", q.clone()), false)
            .unwrap();
        let query = Query {
            catalog: site.catalog,
            tid: now,
            clause: &c,
            filter: None,
            want: Want::Top(None),
            forced: None,
        };
        engine::run(site.mirrors.texts(), &site.store, &query)
            .unwrap()
            .found
            .hits
    };
    let of_kind = |kind: &str| -> Vec<i32> {
        m.kinds
            .iter()
            .filter(|(_, k)| **k == kind)
            .map(|(r, _)| *r)
            .collect()
    };
    // The three kinds of filter: a small filter drives, a rare word
    // drives, otherwise a bitset.
    let cases = [
        ("rare", atom(&["kalo"]), Strategy::FilterDrives),
        ("even", atom(&["ruso"]), Strategy::HitsDrive),
        ("odd", atom(&["kalo"]), Strategy::Bitset),
        (
            "even",
            or(vec![atom(&["mite"]), atom(&["kalo"])]),
            Strategy::Bitset,
        ),
    ];
    for (kind, q, strategy) in cases {
        let filter = of_kind(kind);
        let reference: Vec<(i64, i32)> = all(&q)
            .into_iter()
            .filter(|h| filter.binary_search(&h.1).is_ok())
            .collect();
        assert!(!reference.is_empty(), "{kind}");
        for limit in [None, Some(5)] {
            let c = zoe_catalog::text::clause::decode_clause(&clause_of("Title", q.clone()), false)
                .unwrap();
            let query = Query {
                catalog: site.catalog,
                tid: now,
                clause: &c,
                filter: Some(&filter),
                want: Want::Top(limit),
                forced: None,
            };
            let answer = engine::run(site.mirrors.texts(), &site.store, &query).unwrap();
            assert_eq!(answer.strategy, Some(strategy), "{kind}");
            assert_eq!(answer.found.count, reference.len() as u64, "{kind}");
            let expected: Vec<(i64, i32)> = match limit {
                Some(l) => reference.iter().take(l).copied().collect(),
                None => reference.clone(),
            };
            assert_eq!(answer.found.hits, expected, "{kind} {limit:?}");
        }
        // Through `catalog_query`, with and without `b_size`: each other
        // clause adds 1 to the score.
        for (b_start, b_size) in [(0, None), (0, Some(4)), (2, Some(3))] {
            let plan = text_plan(
                Some(kind),
                plan_clause("Title", q.clone()),
                false,
                b_start,
                b_size,
            );
            let reply = site
                .mirrors
                .query_wire(site.catalog, now, &plan)
                .unwrap()
                .unwrap();
            let reply = zoe_catalog::encode_reply(now, &Ok(reply));
            let page: Vec<(i64, i32)> = match b_size {
                Some(size) => reference
                    .iter()
                    .skip(b_start as usize)
                    .take(size as usize)
                    .copied()
                    .collect(),
                None => reference.clone(),
            };
            assert_eq!(
                bin_ints(get(&reply, "r").unwrap()),
                page.iter().map(|h| h.1).collect::<Vec<_>>()
            );
            assert_eq!(
                bin_ints(get(&reply, "sc").unwrap()),
                page.iter().map(|h| h.0 as i32 + 1).collect::<Vec<_>>()
            );
            assert_eq!(get(&reply, "a"), Some(&Wire::Int(reference.len() as i64)));
        }
        // With a sort: the hits are a set, in the order of `num`, which
        // gives each entry its own key.
        let plan = text_plan(
            Some(kind),
            plan_clause("Title", q.clone()),
            true,
            0,
            Some(6),
        );
        let reply = site
            .mirrors
            .query_wire(site.catalog, now, &plan)
            .unwrap()
            .unwrap();
        let reply = zoe_catalog::encode_reply(now, &Ok(reply));
        assert!(get(&reply, "sc").is_none());
        let mut sorted: Vec<i32> = reference.iter().map(|h| h.1).collect();
        // `num` falls as the rid grows.
        sorted.sort_unstable_by(|a, b| b.cmp(a));
        sorted.truncate(6);
        assert_eq!(bin_ints(get(&reply, "r").unwrap()), sorted);
    }
    // The forced strategies give the same hits.
    let filter = of_kind("odd");
    let c = zoe_catalog::text::clause::decode_clause(&clause_of("Title", atom(&["mite"])), false)
        .unwrap();
    let mut results = Vec::new();
    for forced in [
        Strategy::FilterDrives,
        Strategy::HitsDrive,
        Strategy::Bitset,
    ] {
        let query = Query {
            catalog: site.catalog,
            tid: now,
            clause: &c,
            filter: Some(&filter),
            want: Want::Top(Some(7)),
            forced: Some(forced),
        };
        results.push(
            engine::run(site.mirrors.texts(), &site.store, &query)
                .unwrap()
                .found,
        );
    }
    assert_eq!(results[0], results[1]);
    assert_eq!(results[1], results[2]);
    let _ = &m.model;
}

#[test]
fn a_hit_never_names_a_rid_that_the_snapshot_does_not_hold() {
    let m = mirrored("text-rows", 80);
    let site = &m.site;
    // A text for a rid that the catalog does not hold.
    let tid = site.commit(&[set("Title", 7, "kalo kalo kalo")]);
    site.ready_at(tid);
    let now = site.now();
    assert!(site.mirrors.wait_for(site.catalog, WAIT, |s| {
        s.is_some_and(|s| s.applied.is_some_and(|a| a >= tid))
    }));
    let plan = text_plan(None, plan_clause("Title", atom(&["kalo"])), false, 0, None);
    let reply = site
        .mirrors
        .query_wire(site.catalog, now, &plan)
        .unwrap()
        .unwrap();
    assert!(!reply.rids.contains(&7));
    assert_eq!(reply.rids.len(), 80);
    // `text_query` answers from the texts: the rid has one.
    assert!(
        site.rids(now, &clause_of("Title", atom(&["kalo"])))
            .contains(&7)
    );
}

#[test]
fn text_query_gives_every_hit_in_rid_order_and_text_info_the_counts() {
    let site = Site::new("text-info", &settings(&["en"]));
    let tid = site.commit(&[
        set("Title", 5, "kalo mite"),
        set("Title", -3, "kalo"),
        set("Title", 12, "kalo kalo ruso"),
        set("Title", 8, "mite"),
    ]);
    site.ready_at(tid);
    let t2 = site.commit_quietly(&[unindex("Title", 8), set("Title", 20, "vite")]);
    let now = site.now();
    let hits = site
        .query(
            now,
            &clause_of("Title", atom(&["kalo"])),
            Some(&[-3, 12, 99]),
        )
        .unwrap();
    assert_eq!(hits.rids, vec![-3, 12]);
    assert!(hits.scores.iter().all(|s| *s > 0));
    let info = engine::text_info(
        site.mirrors.texts(),
        &site.store,
        site.catalog,
        "Title",
        now,
        Some(5),
    )
    .unwrap()
    .unwrap();
    assert_eq!(info.docs, 4);
    assert_eq!(info.text, Some(("en".into(), "kalo mite".into())));
    // The words of the index at its tid: `vite` waits for the index.
    assert_eq!(info.words, 3);
    let info = engine::text_info(
        site.mirrors.texts(),
        &site.store,
        site.catalog,
        "Title",
        t2,
        None,
    )
    .unwrap()
    .unwrap();
    assert_eq!(info.docs, 4);
    assert_eq!(info.text, None);
    let bad = engine::text_info(
        site.mirrors.texts(),
        &site.store,
        site.catalog,
        "Title",
        now,
        Some(1 << 40),
    );
    assert!(matches!(bad, Err(CallError::Malformed(_))));
}

#[test]
fn a_clause_of_another_form_is_malformed() {
    let site = Site::new("text-malformed", &settings(&["en"]));
    let now = site.now();
    for bad in [
        map(vec![("i", s("Title")), ("q", atom(&["kalo"]))]),
        clause_of("Title", map(vec![("n", s("near")), ("w", strs(&["kalo"]))])),
        clause_with(
            "Title",
            atom(&["kalo"]),
            vec![],
            vec![staged("Title", 1, Some("x"))],
            s("on"),
        ),
        plan_clause("Title", atom(&["kalo"])),
        clause_with(
            "Title",
            atom(&["kalo"]),
            vec![],
            vec![Wire::Array(vec![
                s("Title"),
                Wire::Int(1 << 33),
                s("en"),
                Wire::Nil,
            ])],
            Wire::Nil,
        ),
    ] {
        let found = engine::text_query(
            site.mirrors.texts(),
            &site.store,
            site.catalog,
            now,
            &bad,
            &Wire::Nil,
        );
        assert!(matches!(found, Err(CallError::Malformed(_))), "{bad:?}");
    }
    // A plan with a text clause of another form.
    let plan = map(vec![
        (
            "c",
            Wire::Array(vec![map(vec![("k", s("t")), ("i", s("Title"))])]),
        ),
        ("s", Wire::Array(vec![])),
        ("l", Wire::Nil),
        ("bs", Wire::Int(0)),
        ("bz", Wire::Nil),
        ("cols", Wire::Nil),
        ("rn", Wire::Int(0)),
    ]);
    assert!(site.mirrors.query_wire(site.catalog, now, &plan).is_err());
    // The compatibility mode is served: an index in `cl` with no staged
    // text has no hit.
    let compat = map(vec![
        ("mode", s("compat")),
        ("language", strs(&["en"])),
        ("boosts", Wire::Map(vec![])),
    ]);
    let q = clause_with("Title", atom(&["kalo"]), vec!["Title"], vec![], compat);
    let found = engine::text_query(
        site.mirrors.texts(),
        &site.store,
        site.catalog,
        now,
        &q,
        &Wire::Nil,
    );
    assert!(matches!(found, Ok(Ok(hits)) if hits.rids.is_empty()));
}

#[test]
fn a_glob_of_any_form_answers_in_bounded_time() {
    let site = Site::new("text-globs", &settings(&["en"]));
    let tid = site.commit(&[set("Title", 1, "kalo")]);
    site.ready_at(tid);
    let now = site.now();
    let word = "e".repeat(39);
    let st = || {
        vec![
            staged("Title", 2, Some(&word)),
            staged("Title", 3, Some("abz")),
        ]
    };
    let started = Instant::now();
    // `*e` 120 times, then `z`: a backtracking match takes exponential
    // time on a long word of `e`.
    let slow = clause_with(
        "Title",
        glob(&format!("{}z", "*e".repeat(120))),
        vec![],
        st(),
        Wire::Nil,
    );
    assert_eq!(
        site.query(now, &slow, None).unwrap().rids,
        Vec::<i32>::new()
    );
    // A long run of `*` is one `*`.
    let stars = clause_with(
        "Title",
        glob(&format!("a{}z", "*".repeat(253))),
        vec![],
        st(),
        Wire::Nil,
    );
    assert_eq!(site.query(now, &stars, None).unwrap().rids, vec![3]);
    assert!(started.elapsed() < Duration::from_secs(10));
    // A pattern past 255 characters does not have the form of a clause.
    let long = clause_with("Title", glob(&"a".repeat(256)), vec![], st(), Wire::Nil);
    let found = engine::text_query(
        site.mirrors.texts(),
        &site.store,
        site.catalog,
        now,
        &long,
        &Wire::Nil,
    );
    assert!(matches!(found, Err(CallError::Malformed(_))));
}

#[test]
fn a_glob_matches_the_analysed_and_the_folded_literal_part() {
    let site = Site::new("text-glob-stems", &settings(&["en"]));
    let tid = site.commit(&[set("Title", 1, "documents"), set("Title", 2, "docker")]);
    site.ready_at(tid);
    let now = site.now();
    // The stemmer makes `document` of `documents`: the pattern finds the
    // stem, in the index and in the text overlay.
    assert_eq!(
        site.rids(now, &clause_of("Title", glob("documents*"))),
        BTreeSet::from([1])
    );
    let staging = clause_with(
        "Title",
        glob("documents*"),
        vec![],
        vec![staged("Title", 3, Some("Documents"))],
        Wire::Nil,
    );
    let found: BTreeSet<i32> = site
        .query(now, &staging, None)
        .unwrap()
        .rids
        .into_iter()
        .collect();
    assert_eq!(found, BTreeSet::from([1, 3]));
    // The folded literal part as it is still finds each term that starts
    // with it.
    assert_eq!(
        site.rids(now, &clause_of("Title", glob("doc*"))),
        BTreeSet::from([1, 2])
    );
}

/// The scores of the hits of the glob `pattern` on Title, by rid: in the
/// index, and in the text overlay with `texts` staged again at the rids 1,
/// 2 and on. The two give the same scores.
fn glob_scores(site: &Site, texts: &[&str], pattern: &str) -> BTreeMap<i32, i32> {
    let now = site.now();
    let scores = |clause: &Wire| -> BTreeMap<i32, i32> {
        let hits = site.query(now, clause, None).unwrap();
        hits.rids.into_iter().zip(hits.scores).collect()
    };
    let indexed = scores(&clause_of("Title", glob(pattern)));
    let st = (1..)
        .zip(texts)
        .map(|(rid, text)| staged("Title", rid, Some(text)))
        .collect();
    let overlaid = scores(&clause_with("Title", glob(pattern), vec![], st, Wire::Nil));
    assert_eq!(indexed, overlaid, "{pattern}");
    indexed
}

#[test]
fn a_glob_finds_the_exact_stem_of_its_literal_and_not_the_other_words_of_the_stem() {
    let site = Site::new("text-glob-exact-stem", &settings(&["en"]));
    let mut words = vec!["run", "runs", "running", "runner", "runway"];
    let ops: Vec<TextOp<'_>> = (1..)
        .zip(&words)
        .map(|(rid, word)| set("Title", rid, word))
        .collect();
    let tid = site.commit(&ops);
    site.ready_at(tid);
    // `running*` finds the stem `run` of `running` as an exact term, and
    // the terms that start with `running`: not `runner` or `runway`.
    let running = glob_scores(&site, &words, "running*");
    assert_eq!(
        running.keys().copied().collect::<BTreeSet<_>>(),
        BTreeSet::from([1, 2, 3])
    );
    // `run*` finds each term that starts with `run`. The exact stem adds
    // its BM25 score to the constant score of the glob, so the documents
    // of the stem rank above those of the prefix alone.
    let run = glob_scores(&site, &words, "run*");
    assert_eq!(
        run.keys().copied().collect::<BTreeSet<_>>(),
        BTreeSet::from([1, 2, 3, 4, 5])
    );
    let stem = [run[&1], run[&2], run[&3]];
    assert!(stem.iter().all(|s| *s > run[&4]), "{run:?}");
    assert_eq!(run[&4], run[&5]);
    // A term that starts with `running` and is not the stem: the prefix
    // finds it, below the documents of the stem. The stem is in half of
    // the documents, so its BM25 score alone is below the constant score.
    words.push("runningboard");
    let tid = site.commit(&[set("Title", 6, "runningboard")]);
    site.ready_at(tid);
    let running = glob_scores(&site, &words, "running*");
    assert_eq!(
        running.keys().copied().collect::<BTreeSet<_>>(),
        BTreeSet::from([1, 2, 3, 6])
    );
    let stem = [running[&1], running[&2], running[&3]];
    assert!(stem.iter().all(|s| *s > running[&6]), "{running:?}");
    assert_eq!(running[&6], 1024);
    assert!(running[&1] - running[&6] < running[&6], "{running:?}");
}

#[test]
fn an_index_in_cl_leaves_the_committed_hits_of_the_other_indexes() {
    let site = Site::new("text-cleared-boost", &settings(&["en"]));
    let tid = site.commit(&[
        set("SearchableText", 1, "kalo"),
        set("SearchableText", 2, "kalo mite"),
        set("Title", 2, "kalo"),
        set("Title", 4, "kalo"),
    ]);
    site.ready_at(tid);
    let now = site.now();
    let q = || atom(&["kalo"]);
    assert_eq!(
        site.rids(now, &clause_of("SearchableText", q())),
        BTreeSet::from([1, 2, 4])
    );
    // The transaction cleared Title and staged one title: the committed
    // hits of SearchableText stay, and the committed titles leave.
    let cleared = clause_with(
        "SearchableText",
        q(),
        vec!["Title"],
        vec![staged("Title", 5, Some("kalo"))],
        Wire::Nil,
    );
    let found: BTreeSet<i32> = site
        .query(now, &cleared, None)
        .unwrap()
        .rids
        .into_iter()
        .collect();
    assert_eq!(found, BTreeSet::from([1, 2, 5]));
}

#[test]
fn a_text_index_caps_a_pack_at_its_tid() {
    let site = Site::without_gc("text-cap", &settings(&["en"]));
    let t1 = site.commit(&[set("Title", 1, "kalo"), set("Title", 2, "kalo")]);
    site.ready_at(t1);
    // The index stays at t1: the unindex is after its cap.
    site.commit_quietly(&[unindex("Title", 1)]);
    let last = site.store.last_transaction();
    site.store.pack(last).unwrap();
    let watermark = site.store.pack_watermark().unwrap().0;
    assert!(watermark <= t1, "{watermark:#x} {t1:#x}");
    assert_eq!(
        site.rids(site.now(), &clause_of("Title", atom(&["kalo"]))),
        BTreeSet::from([2])
    );
}

/// Count the rebuilds of the text indexes.
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

#[test]
fn a_restart_below_the_pack_watermark_rebuilds_the_index() {
    let mut site = Site::without_gc("text-restart-packed", &settings(&["en"]));
    let t1 = site.commit(&[set("Title", 1, "kalo"), set("Title", 2, "kalo")]);
    site.ready_at(t1);
    site.mirrors.stop();
    let t2 = testing::commit_text(
        &site.store,
        site.catalog,
        &[unindex("Title", 1)],
        None,
        false,
    )
    .unwrap();
    // No cap holds this pack: it removes the unindex and its text.
    site.store.pack(Tid(t2)).unwrap();
    assert!(site.store.pack_watermark().unwrap().0 >= t2);
    let (builds, hooks) = counting();
    site.restart(hooks);
    site.ready_at(t2);
    assert_eq!(builds.load(Ordering::SeqCst), 1);
    assert_eq!(
        site.rids(site.now(), &clause_of("Title", atom(&["kalo"]))),
        BTreeSet::from([2])
    );
}

#[test]
fn a_pack_that_started_before_the_cap_rebuilds_the_index() {
    let mut site = Site::without_gc("text-pack-race", &settings(&["en"]));
    let t1 = site.commit(&[set("Title", 1, "kalo"), set("Title", 2, "kalo")]);
    site.ready_at(t1);
    site.mirrors.stop();
    let t2 = testing::commit_text(
        &site.store,
        site.catalog,
        &[unindex("Title", 1)],
        None,
        false,
    )
    .unwrap();
    // The open checks the index, then a pack that holds no cap runs before
    // the open takes its cap: the pack removes the unindex after the tid of
    // the index.
    let (builds, mut hooks) = counting();
    let store = Arc::clone(&site.store);
    let packed = Arc::new(AtomicUsize::new(0));
    let ran = Arc::clone(&packed);
    hooks.before_text_cap = Some(Arc::new(move |_| {
        if ran.fetch_add(1, Ordering::SeqCst) == 0 {
            store.pack(Tid(t2)).unwrap();
        }
    }));
    site.restart(hooks);
    let texts = site.mirrors.texts().unwrap();
    let now = site.now();
    let q = clause_of("Title", atom(&["kalo"]));
    // No answer names the document whose unindex the pack removed.
    let deadline = Instant::now() + WAIT;
    loop {
        match site.query(now, &q, None) {
            Ok(hits) => {
                assert_eq!(hits.rids, vec![2]);
                if builds.load(Ordering::SeqCst) == 1 {
                    break;
                }
            }
            Err(e) => assert_eq!(e.kind, TextErrorKind::Unavailable),
        }
        assert!(Instant::now() < deadline, "{:?}", texts.counts());
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(packed.load(Ordering::SeqCst), 1);
    site.ready_at(t2);
}

/// A pack that reads the caps of the store, then waits until the test
/// releases it, from a hook of the open of the text index: the cap that the
/// open takes after it does not hold the pack.
struct HeldPack {
    release: Arc<(Mutex<bool>, std::sync::Condvar)>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl HeldPack {
    fn new() -> Arc<HeldPack> {
        Arc::new(HeldPack {
            release: Arc::default(),
            thread: Mutex::new(None),
        })
    }

    /// Start the pack of `store` at `at`, and return once it read the caps.
    fn start(&self, store: &Arc<Store>, at: Tid) {
        let (tx, rx) = mpsc::channel();
        let store = Arc::clone(store);
        let release = Arc::clone(&self.release);
        let thread = std::thread::spawn(move || {
            store
                .pack_after_caps(at, || {
                    tx.send(()).unwrap();
                    let (lock, cv) = &*release;
                    let mut open = lock.lock().unwrap();
                    while !*open {
                        open = cv.wait(open).unwrap();
                    }
                })
                .unwrap();
        });
        rx.recv_timeout(WAIT).expect("the pack reads the caps");
        *self.thread.lock().unwrap() = Some(thread);
    }

    /// Release the pack and wait for its end.
    fn finish(&self) {
        let (lock, cv) = &*self.release;
        *lock.lock().unwrap() = true;
        cv.notify_all();
        if let Some(thread) = self.thread.lock().unwrap().take() {
            thread.join().unwrap();
        }
    }
}

/// The hooks of a test of a pack that passes the cap of the index after its
/// open: the pack starts before the cap, and ends at the first check of the
/// cap after `armed`. Then the hook signals `checked` and waits for `gate`.
fn passed_cap_hooks(
    store: &Arc<Store>,
    at: Tid,
    armed: &Arc<std::sync::atomic::AtomicBool>,
    checked: mpsc::Sender<()>,
    gate: &Gate,
) -> (Arc<AtomicUsize>, Hooks) {
    let (builds, mut hooks) = counting();
    let pack = HeldPack::new();
    let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (store, starting) = (Arc::clone(store), Arc::clone(&pack));
    hooks.before_text_cap = Some(Arc::new(move |_| {
        if !started.swap(true, Ordering::SeqCst) {
            starting.start(&store, at);
        }
    }));
    let armed = Arc::clone(armed);
    let checked = Mutex::new(checked);
    let (tx, _rx) = mpsc::channel();
    let hold = gate.hook(tx);
    hooks.before_text_check = Some(Arc::new(move |catalog| {
        if armed.swap(false, Ordering::SeqCst) {
            pack.finish();
            let _ = checked.lock().unwrap().send(());
            hold(catalog);
        }
    }));
    (builds, hooks)
}

#[test]
fn a_commit_checks_the_cap_before_it_writes_and_a_query_waits_below_the_watermark() {
    let mut site = Site::without_gc("text-commit-cap", &settings(&["en"]));
    let t1 = site.commit(&[set("Title", 1, "kalo"), set("Title", 2, "kalo")]);
    site.ready_at(t1);
    site.mirrors.stop();
    let t2 = testing::commit_text(
        &site.store,
        site.catalog,
        &[unindex("Title", 1)],
        None,
        false,
    )
    .unwrap();
    let armed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (checked, on_check) = mpsc::channel();
    let gate = Gate::default();
    // A failed assertion opens the gate, so the thread of the index ends
    // and the test fails in place of a hang.
    let _open = ReleaseOnDrop(gate.clone());
    let (builds, mut hooks) = passed_cap_hooks(&site.store, Tid(t2), &armed, checked, &gate);
    // At the start of the rebuild, the payload on disk still gives the tid
    // of the index before the lost cap: the commit wrote nothing.
    let on_disk = Arc::new(Mutex::new(None));
    let (seen, dir, count) = (
        Arc::clone(&on_disk),
        site.text_dir(),
        hooks.before_text_build.take(),
    );
    hooks.before_text_build = Some(Arc::new(move |catalog| {
        *seen.lock().unwrap() = Some(payload_tid(&dir));
        if let Some(count) = &count {
            count(catalog);
        }
    }));
    site.restart_at(hooks, t1);
    site.ready_at(t1);
    assert_eq!(builds.load(Ordering::SeqCst), 0);
    // A commit to apply: the writer commits it, and the check of its cap
    // runs before the write, after the pack removed the unindex.
    armed.store(true, Ordering::SeqCst);
    let t3 = site.commit(&[set("Title", 3, "kalo")]);
    on_check
        .recv_timeout(WAIT)
        .expect("the commit checks its cap");
    assert!(site.store.pack_watermark().unwrap().0 >= t2);
    // The index is below the watermark: a query waits for its rebuild,
    // then gets `unavailable`.
    let q = clause_of("Title", atom(&["kalo"]));
    let started = Instant::now();
    let failed = site.query(site.now(), &q, None).unwrap_err();
    assert_eq!(failed.kind, TextErrorKind::Unavailable);
    assert!(started.elapsed() >= Duration::from_millis(900));
    gate.release();
    site.ready_at(t3);
    assert_eq!(builds.load(Ordering::SeqCst), 1);
    let found = on_disk
        .lock()
        .unwrap()
        .take()
        .expect("the rebuild reads the payload");
    assert_eq!(
        found,
        Some(t1),
        "the payload before the rebuild, t3 is {t3:#x}"
    );
    assert_eq!(site.rids(site.now(), &q), BTreeSet::from([2, 3]));
}

/// Open the gate at the drop.
struct ReleaseOnDrop(Gate);

impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        self.0.release();
    }
}

/// The tid of the payload of the last commit of the index in `dir`, or
/// `None` when it does not read.
fn payload_tid(dir: &Path) -> Option<u64> {
    let meta: Json =
        serde_json::from_str(&std::fs::read_to_string(dir.join("meta.json")).ok()?).ok()?;
    let payload = meta.get("payload")?.as_str()?;
    zoe_catalog::text::index::read_payload(payload).map(|(tid, _)| tid)
}

#[test]
fn an_advance_checks_the_cap_while_a_pack_runs() {
    let mut site = Site::without_gc("text-advance-cap", &settings(&["en"]));
    let t1 = site.commit(&[set("Title", 1, "kalo"), set("Title", 2, "kalo")]);
    site.ready_at(t1);
    site.mirrors.stop();
    // A commit with no delta of the catalog: the index only advances its
    // tid past it.
    let t2 = testing::commit_text(
        &site.store,
        CATALOG + 1,
        &[set("Title", 9, "x")],
        None,
        false,
    )
    .unwrap();
    let armed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (checked, on_check) = mpsc::channel();
    let gate = Gate::default();
    let (builds, hooks) = passed_cap_hooks(&site.store, Tid(t2), &armed, checked, &gate);
    site.restart_at(hooks, t1);
    site.ready_at(t1);
    armed.store(true, Ordering::SeqCst);
    site.mirrors.committed(t2);
    on_check
        .recv_timeout(WAIT)
        .expect("the advance checks its cap");
    gate.release();
    site.ready_at(t2);
    assert_eq!(builds.load(Ordering::SeqCst), 1);
    assert_eq!(
        site.rids(site.now(), &clause_of("Title", atom(&["kalo"]))),
        BTreeSet::from([1, 2])
    );
}

#[test]
fn a_security_clause_and_a_text_clause_run_in_one_plan() {
    let dir = fresh_dir("text-security");
    let store = Arc::new(Store::open(dir.path()).unwrap());
    let index = "allowedRolesAndUsers";
    let mut model = CatalogModel::new(0x2000, &[(index, IndexKind::Security)], &[]);
    // An entry whose inputs give its value as it is: the roles that may
    // view it.
    let allowed = |role: &str| {
        Val::Tuple(
            vec![
                Val::Str("v".into()),
                Val::Tuple(vec![Val::Str(role.into())].into()),
                Val::Tuple(Vec::new().into()),
                Val::Bool(false),
                Val::Tuple(Vec::new().into()),
            ]
            .into(),
        )
    };
    let mut ops_texts = Vec::new();
    for i in 0..6 {
        let rid = 100 + i;
        let role = if i % 2 == 0 { "Anonymous" } else { "Reader" };
        model.catalog(rid, &format!("/site/item-{i}"), &[(index, allowed(role))]);
        ops_texts.push((rid, if i < 4 { "kalo" } else { "mite" }));
    }
    let catalog = model.oid();
    testing::commit(&store, &model.records(), &[model.key()], &[]).unwrap();
    let ops: Vec<TextOp<'_>> = ops_texts
        .iter()
        .map(|(rid, text)| set("Title", *rid, text))
        .collect();
    let tid = testing::commit_text(&store, catalog, &ops, Some(&settings(&["en"])), false).unwrap();
    let mirrors = start(
        &store,
        dir.path(),
        false,
        Duration::from_secs(5),
        Hooks::default(),
    );
    assert!(mirrors.wait_for(catalog, WAIT, |s| {
        s.is_some_and(|s| s.state == "ready" && s.applied.is_some_and(|a| a >= tid))
    }));
    let texts = mirrors.texts().unwrap();
    assert!(texts.wait_for(catalog, WAIT, |c| {
        c.is_some_and(|c| c.ready && c.tid >= tid)
    }));
    let security = map(vec![
        ("k", s("v")),
        ("i", s(index)),
        ("q", Wire::Array(vec![s("Anonymous")])),
        ("and", Wire::Bool(false)),
        ("r", Wire::Nil),
        ("not", Wire::Array(vec![])),
    ]);
    let plan = map(vec![
        (
            "c",
            Wire::Array(vec![security, plan_clause("Title", atom(&["kalo"]))]),
        ),
        ("s", Wire::Array(vec![])),
        ("l", Wire::Nil),
        ("bs", Wire::Int(0)),
        ("bz", Wire::Nil),
        ("cols", Wire::Nil),
        ("rn", Wire::Int(0)),
    ]);
    let reply = mirrors
        .query_wire(catalog, tid + 1, &plan)
        .unwrap()
        .unwrap();
    let mut rids = reply.rids.clone();
    rids.sort_unstable();
    // The entries that Anonymous may view, and whose title holds the word.
    assert_eq!(rids, vec![100, 102]);
    let scores = reply.scores.expect("the scores of the text clause");
    assert!(scores.iter().all(|s| *s > 1));
    mirrors.stop();
}
