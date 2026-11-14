//! The acceptance test of the compatibility mode of the text index (ADR
//! 0060, sections 11 and 13): the rids, the order, the counts and the
//! integer scores of ZCTextIndex with the lexicon `plone_lexicon` of Plone
//! 6.1.
//!
//! `tests/data/text_compat.json` keeps the results of ZCTextIndex on a
//! corpus of generated words and of the character classes of the splitter
//! (Latin with accents, Latin extended, Greek, Cyrillic, CJK, kana, Hangul,
//! Thai, Arabic, Devanagari, numbers, compatibility characters, combining
//! marks, and the separators of the finding), for a set of queries on each
//! index (words, words in another case and with accents, `AND`, `OR`,
//! `NOT`, `AND NOT`, groups, phrases of two and three words, globs with `*`
//! and `?`, an empty query, a query of separators only), at each state of
//! the corpus: the base, edits and unindexes, more edits, a clear, and the
//! texts that a transaction stages over the edits. Its generator,
//! `tests/data/text_compat.py`, runs a real ZCTextIndex.
//!
//! The store holds a catalog whose three text indexes are in the mode
//! `compat`. The test commits the states as deltas and checks each query
//! at each snapshot: with the index at the snapshot, behind it and ahead of
//! it, at an older snapshot with the staged texts, with a limit, and with
//! filters under each strategy of the search.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value as Json;
use zoe_catalog::testing::{self, TextOp};
use zoe_catalog::text::clause::{self, TextHits};
use zoe_catalog::text::{
    CatalogSettings, IndexSettings, Mode, Query, Strategy, TextConfig, TextErrorKind, Want, engine,
};
use zoe_catalog::{Config, Hooks, Mirrors, Opener, Reads, Wire};
use zoe_core::Store;
use zoe_core::testing::{TempDir, fresh_dir};

const WAIT: Duration = Duration::from_secs(60);

/// The oid of the catalog.
const CATALOG: u64 = 0x297;

const INDEXES: [&str; 3] = ["SearchableText", "Title", "Description"];

fn data() -> Json {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/text_compat.json");
    serde_json::from_slice(&std::fs::read(path).expect("the test data")).expect("JSON")
}

/// The settings of the three indexes in the mode `compat`.
fn settings() -> CatalogSettings {
    let mut out = CatalogSettings::default();
    for name in INDEXES {
        let mut s = IndexSettings::enhanced(&["en", "fr"]);
        s.mode = Mode::Compat;
        if name == "SearchableText" {
            // The boosts do not apply in the mode `compat`.
            s.boosts = vec![("Title".into(), 3.0), ("Description".into(), 1.5)];
        }
        out.indexes.insert(name.into(), s);
    }
    out
}

/// A JSON value of the plan as a msgpack value of the wire.
fn wire(json: &Json) -> Wire {
    match json {
        Json::Null => Wire::Nil,
        Json::Bool(b) => Wire::Bool(*b),
        Json::Number(n) => Wire::Int(n.as_i64().expect("an int")),
        Json::String(s) => Wire::Str(s.clone()),
        Json::Array(items) => Wire::Array(items.iter().map(wire).collect()),
        Json::Object(items) => Wire::Map(
            items
                .iter()
                .map(|(k, v)| (Wire::Str(k.clone()), wire(v)))
                .collect(),
        ),
    }
}

fn s(v: &str) -> Wire {
    Wire::Str(v.into())
}

/// The text clause of `text_query` on `index` with the node `q` and the
/// staged texts `st`.
fn clause_of(index: &str, q: Wire, st: Vec<Wire>) -> Wire {
    Wire::Map(vec![
        (s("i"), s(index)),
        (s("q"), q),
        (s("lg"), Wire::Nil),
        (s("cl"), Wire::Array(vec![])),
        (s("st"), Wire::Array(st)),
        (s("set"), Wire::Nil),
    ])
}

/// The operations of a state as deltas.
fn ops(state: &Json) -> Vec<TextOp<'_>> {
    state
        .as_array()
        .expect("operations")
        .iter()
        .map(|op| {
            let op = op.as_array().expect("an operation");
            let index = op[1].as_str().expect("an index");
            match op[0].as_str().expect("a kind") {
                "set" => {
                    let rid = op[2].as_i64().expect("a rid") as i32;
                    TextOp::Set {
                        index,
                        rid,
                        // The language does not change the words of the
                        // mode `compat`.
                        language: if rid % 2 == 0 { "en" } else { "fr" },
                        text: op[3].as_str().expect("a text"),
                    }
                }
                "unindex" => TextOp::Unindex {
                    index,
                    rid: op[2].as_i64().expect("a rid") as i32,
                },
                "clear" => TextOp::Clear { index },
                other => panic!("an operation {other}"),
            }
        })
        .collect()
}

/// The operations of a state as the staged texts of a clause.
fn staged(state: &Json) -> Vec<Wire> {
    ops(state)
        .into_iter()
        .map(|op| match op {
            TextOp::Set {
                index, rid, text, ..
            } => Wire::Array(vec![s(index), Wire::Int(i64::from(rid)), s("en"), s(text)]),
            TextOp::Unindex { index, rid } => {
                Wire::Array(vec![s(index), Wire::Int(i64::from(rid)), s(""), Wire::Nil])
            }
            TextOp::Clear { .. } => panic!("no clear is staged"),
        })
        .collect()
}

/// A store with the catalog, its settings and its text index.
struct Site {
    mirrors: Mirrors,
    store: Arc<Store>,
    _dir: TempDir,
}

impl Drop for Site {
    fn drop(&mut self) {
        self.mirrors.stop();
    }
}

impl Site {
    fn new(name: &str) -> Site {
        let dir = fresh_dir(name);
        let store = Arc::new(Store::open(dir.path()).unwrap());
        testing::commit_text(&store, CATALOG, &[], Some(&settings()), true).unwrap();
        let weak = Arc::downgrade(&store);
        let open: Opener = Arc::new(move || weak.upgrade().map(|s| s as Arc<dyn Reads>));
        let config = Config {
            wait: Duration::from_secs(5),
            hooks: Hooks::default(),
            text: TextConfig {
                dir: Some(dir.path().to_owned()),
                read_only: false,
            },
            ..Config::default()
        };
        let mirrors = Mirrors::start(open, store.last_transaction().0, config);
        Site {
            mirrors,
            store,
            _dir: dir,
        }
    }

    /// Commit `ops`. `hand` hands the commit to the mirrors; otherwise the
    /// index stays behind.
    fn commit(&self, ops: &[TextOp<'_>], hand: bool) -> u64 {
        let tid = testing::commit_text(&self.store, CATALOG, ops, None, false).unwrap();
        if hand {
            self.mirrors.committed(tid);
        }
        tid
    }

    fn ready_at(&self, tid: u64) {
        let texts = self.mirrors.texts().expect("the text indexes");
        assert!(
            texts.wait_for(CATALOG, WAIT, |c| c
                .is_some_and(|c| c.ready && c.tid >= tid)),
            "the index reaches {tid:#x}: {:?}",
            texts.counts()
        );
    }

    /// `text_query` at the exclusive tid `tid`.
    fn query(&self, tid: u64, clause: &Wire) -> Result<TextHits, zoe_catalog::text::TextError> {
        engine::text_query(
            self.mirrors.texts(),
            &self.store,
            CATALOG,
            tid,
            clause,
            &Wire::Nil,
        )
        .expect("a clause of the form")
    }

    /// `engine::run` at `tid`, with a filter, for `want`.
    fn run(
        &self,
        tid: u64,
        clause: &Wire,
        filter: Option<&[i32]>,
        want: Want,
        forced: Option<Strategy>,
    ) -> zoe_catalog::text::Answer {
        let c = clause::decode_clause(clause, false).expect("a clause");
        let query = Query {
            catalog: CATALOG,
            tid,
            clause: &c,
            filter,
            want,
            forced,
        };
        engine::run(self.mirrors.texts(), &self.store, &query).expect("an answer")
    }
}

/// The node of a query of the data: its plan, or an atom with no word for
/// a query with no plan, which matches every document.
fn node_of(query: &Json) -> Wire {
    match &query["plan"] {
        Json::Null => Wire::Map(vec![(s("n"), s("atom")), (s("w"), Wire::Array(vec![]))]),
        plan => wire(plan),
    }
}

/// The expected hits of `query` at `state`, in the order of
/// `Catalog.search`, or `None` for a query that ZCTextIndex refuses.
fn expected(query: &Json, state: &str) -> Option<Vec<(i64, i32)>> {
    let hits = &query["hits"][state];
    let list = hits.as_array()?;
    Some(
        list.iter()
            .map(|h| {
                (
                    h[1].as_i64().expect("a score"),
                    h[0].as_i64().expect("a rid") as i32,
                )
            })
            .collect(),
    )
}

/// The filter of a check: every `step`th rid of the hits, and rids that no
/// document has.
fn filter_of(hits: &[(i64, i32)], step: usize) -> Vec<i32> {
    let mut out: Vec<i32> = hits.iter().step_by(step).map(|h| h.1).collect();
    out.extend([i32::MIN, -7, 0, 11, i32::MAX]);
    out.sort_unstable();
    out.dedup();
    out
}

/// Check each query of each index at the snapshot `tid` against the state
/// `state`, with the staged texts `st` of `staged`. `deep` also checks a
/// limit, filters and the strategies.
fn check(
    site: &Site,
    data: &Json,
    tid: u64,
    state: &str,
    indexes: &[&str],
    st: &[Wire],
    deep: bool,
) -> usize {
    let mut checked = 0;
    for index in indexes {
        for query in data["queries"][*index].as_array().expect("queries") {
            let Some(want) = expected(query, state) else {
                continue;
            };
            let what = format!(
                "{index} {} {:?} at {state}",
                query["family"], query["query"]
            );
            let c = clause_of(index, node_of(query), st.to_vec());
            // `text_query`: every hit in rid order, with its score.
            let hits = site
                .query(tid, &c)
                .unwrap_or_else(|e| panic!("{what}: {e:?}"));
            let mut by_rid: Vec<(i32, i64)> = want.iter().map(|h| (h.1, h.0)).collect();
            by_rid.sort_unstable();
            let got: Vec<(i32, i64)> = hits
                .rids
                .iter()
                .zip(&hits.scores)
                .map(|(r, sc)| (*r, i64::from(*sc)))
                .collect();
            assert_eq!(got, by_rid, "{what}");
            // The order of `Catalog.search`, and its count.
            let answer = site.run(tid, &c, None, Want::Top(None), None);
            assert_eq!(answer.found.hits, want, "{what}");
            assert_eq!(answer.found.count, want.len() as u64, "{what}");
            checked += 1;
            if !deep || want.is_empty() {
                continue;
            }
            let limit = (want.len() / 2).max(1);
            let answer = site.run(tid, &c, None, Want::Top(Some(limit)), None);
            assert_eq!(answer.found.hits, want[..limit], "{what} limit {limit}");
            assert_eq!(answer.found.count, want.len() as u64, "{what} limit");
            let filter = filter_of(&want, 2);
            let kept: Vec<(i64, i32)> = want
                .iter()
                .filter(|h| filter.binary_search(&h.1).is_ok())
                .copied()
                .collect();
            for forced in [
                None,
                Some(Strategy::FilterDrives),
                Some(Strategy::HitsDrive),
                Some(Strategy::Bitset),
            ] {
                let answer = site.run(tid, &c, Some(&filter), Want::Top(None), forced);
                assert_eq!(answer.found.hits, kept, "{what} filter {forced:?}");
                assert_eq!(answer.found.count, kept.len() as u64, "{what} {forced:?}");
                let set = site.run(tid, &c, Some(&filter), Want::Set, forced);
                let mut rids: Vec<i32> = set.found.hits.iter().map(|h| h.1).collect();
                rids.sort_unstable();
                let mut kept_rids: Vec<i32> = kept.iter().map(|h| h.1).collect();
                kept_rids.sort_unstable();
                assert_eq!(rids, kept_rids, "{what} set {forced:?}");
            }
        }
    }
    checked
}

#[test]
fn the_compatibility_mode_gives_the_results_of_zctextindex() {
    let data = data();
    let states = &data["states"];
    let site = Site::new("text-compat");
    // The base and the edits: the index follows them.
    let base = site.commit(&ops(&states["base"]), true);
    let edits = site.commit(&ops(&states["edits"]), true);
    site.ready_at(edits);
    // More edits that the index does not follow: it stays behind.
    let more = site.commit(&ops(&states["more"]), false);
    let mut checked = 0;
    for (tid, state) in [(base, "base"), (edits, "edits"), (more, "more")] {
        checked += check(
            &site,
            &data,
            tid + 1,
            state,
            &INDEXES,
            &[],
            state == "edits",
        );
    }
    // The texts that a transaction stages, at the older snapshot of the
    // edits, with the index behind and then ahead.
    let st = staged(&states["staged"]);
    checked += check(&site, &data, edits + 1, "staged", &INDEXES, &st, true);
    site.mirrors.committed(more);
    site.ready_at(more);
    for (tid, state) in [(base, "base"), (edits, "edits"), (more, "more")] {
        checked += check(&site, &data, tid + 1, state, &INDEXES, &[], state == "base");
    }
    checked += check(&site, &data, edits + 1, "staged", &INDEXES, &st, false);
    // A clear of `Description` and its texts again, in one commit.
    let clear = site.commit(&ops(&states["clear"]), true);
    site.ready_at(clear);
    checked += check(&site, &data, clear + 1, "clear", &INDEXES, &[], true);
    // An older snapshot still reads the other indexes; `Description`
    // refuses it.
    checked += check(
        &site,
        &data,
        edits + 1,
        "edits",
        &["SearchableText", "Title"],
        &[],
        false,
    );
    let q = clause_of(
        "Description",
        node_of(&data["queries"]["Description"][0]),
        vec![],
    );
    assert_eq!(
        site.query(edits + 1, &q).unwrap_err().kind,
        TextErrorKind::Cleared
    );
    assert!(checked > 3000, "{checked} checks");
}

#[test]
fn a_rebuilt_index_and_a_change_of_mode_give_the_same_results() {
    let data = data();
    let states = &data["states"];
    let site = Site::new("text-compat-rebuild");
    site.commit(&ops(&states["base"]), true);
    let edits = site.commit(&ops(&states["edits"]), true);
    site.ready_at(edits);
    // The enhanced mode first, then the mode `compat` again: each change
    // of the mode rebuilds the index from the deltas, which do not change.
    let mut enhanced = settings();
    for s in enhanced.indexes.values_mut() {
        s.mode = Mode::Enhanced;
    }
    let tid = testing::commit_text(&site.store, CATALOG, &[], Some(&enhanced), false).unwrap();
    site.mirrors.committed(tid);
    site.ready_at(tid);
    let tid = testing::commit_text(&site.store, CATALOG, &[], Some(&settings()), false).unwrap();
    site.mirrors.committed(tid);
    site.ready_at(tid);
    let checked = check(&site, &data, tid + 1, "edits", &INDEXES, &[], false);
    assert!(checked > 300, "{checked} checks");
}

#[test]
fn an_index_in_cl_answers_from_the_staged_texts_alone() {
    let data = data();
    let site = Site::new("text-compat-alone");
    let base = site.commit(&ops(&data["states"]["base"]), true);
    site.ready_at(base);
    // The clause lists each index in `cl` with the settings of `set`: the
    // staged texts of the state `staged` alone are the documents.
    let set = Wire::Map(vec![
        (s("mode"), s("compat")),
        (s("language"), Wire::Array(vec![s("en")])),
        (s("boosts"), Wire::Map(vec![])),
    ]);
    let mut texts: BTreeMap<&str, Vec<Wire>> = BTreeMap::new();
    let all = ops(&data["states"]["base"]);
    let edits = ops(&data["states"]["edits"]);
    let staged_ops = ops(&data["states"]["staged"]);
    // The texts of the state `staged`: those of `edits` with its changes.
    let mut model: BTreeMap<(&str, i32), &str> = BTreeMap::new();
    for op in all.iter().chain(&edits).chain(&staged_ops) {
        match op {
            TextOp::Set {
                index, rid, text, ..
            } => {
                model.insert((index, *rid), text);
            }
            TextOp::Unindex { index, rid } => {
                model.remove(&(*index, *rid));
            }
            TextOp::Clear { .. } => unreachable!(),
        }
    }
    for ((index, rid), text) in &model {
        texts.entry(index).or_default().push(Wire::Array(vec![
            s(index),
            Wire::Int(i64::from(*rid)),
            s("en"),
            s(text),
        ]));
    }
    let mut checked = 0;
    for index in INDEXES {
        for query in data["queries"][index].as_array().unwrap() {
            let Some(want) = expected(query, "staged") else {
                continue;
            };
            let c = Wire::Map(vec![
                (s("i"), s(index)),
                (s("q"), node_of(query)),
                (s("lg"), Wire::Nil),
                (s("cl"), Wire::Array(vec![s(index)])),
                (s("st"), Wire::Array(texts[index].clone())),
                (s("set"), set.clone()),
            ]);
            let answer = site.run(base + 1, &c, None, Want::Top(None), None);
            assert_eq!(answer.found.hits, want, "{index} {:?}", query["query"]);
            checked += 1;
        }
    }
    assert!(checked > 300, "{checked} checks");
}
