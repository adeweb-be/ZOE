//! The answer of a text clause (ADR 0060, sections 12 and 13): the tantivy
//! index of the catalog, the snapshot, the staged texts and the refusals.
//!
//! - The snapshot. When the index holds commits at or after the tid of the
//!   clause, the documents that changed in them get their live text at the
//!   snapshot. When the index lacks commits before the snapshot, the
//!   documents that changed in them get their live text at the snapshot
//!   too. So the matched set is the set of the live texts at the snapshot.
//! - The transaction. The staged texts of the clause replace the documents
//!   of their keys, after the snapshot. When the clause lists an index that
//!   the query reads in `cl`, the answer comes from the staged texts alone.
//! - The statistics are those of the current index, or those of the
//!   staged texts when no index holds a document.
//! - The refusals: a snapshot at or before the pack watermark is `packed`;
//!   a snapshot before a clear delta that the index holds is `cleared`. A
//!   clear delta before the snapshot that the index does not hold yet means
//!   a rebuild: the query waits for it.
//!
//! The replaced documents leave the result of the index, and the text
//! overlay scores their other version.

use std::collections::{BTreeSet, HashMap, HashSet};

use tantivy::query::Bm25StatisticsProvider;
use zoe_core::{Oid, Store, Tid};

use super::clause::{MAX_BYTES, TextClause, TextErrorKind, TextInfo};
use super::index::{self, Fields};
use super::overlay::{Doc, OwnStats, Scorer};
use super::search::{self, Found, Scope, Strategy, Want, to_int};
use super::settings::{IndexSettings, Mode};
use super::texts::{Failure, Snap, Texts};
use super::translate::{Read, Translator};
use std::sync::Arc;
use std::time::Instant;

use super::clause::TextError;

/// A text query.
pub struct Query<'a> {
    /// The oid of the catalog.
    pub catalog: u64,
    /// The exclusive tid of the snapshot.
    pub tid: u64,
    /// The clause.
    pub clause: &'a TextClause,
    /// The rids of the filter, sorted, each once, or `None`.
    pub filter: Option<&'a [i32]>,
    /// What the caller wants.
    pub want: Want,
    /// A strategy that the tests force; `None` picks it.
    pub forced: Option<Strategy>,
}

/// What a text query found, and the strategy of the search of the index.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Answer {
    /// The hits.
    pub found: Found,
    /// The strategy, when the index answered.
    pub strategy: Option<Strategy>,
}

fn error(kind: TextErrorKind, why: impl Into<String>) -> Failure {
    Failure::Text(TextError::new(kind, why))
}

/// Refuse a snapshot at or before the pack watermark. Return the
/// watermark.
fn check_packed(store: &Store, tid: u64) -> Result<u64, Failure> {
    let watermark = store.pack_watermark().map_err(|e| Failure::of(&e))?;
    if watermark.0 > 0 && tid <= watermark.0 {
        return Err(error(
            TextErrorKind::Packed,
            format!(
                "the snapshot {tid:#018x} is at or before the pack watermark {:#018x}",
                watermark.0
            ),
        ));
    }
    Ok(watermark.0)
}

/// The settings of the index of `clause`: those of the clause, else those
/// of `zoe.text`. The compatibility mode is `Unsupported`.
fn settings_of(clause: &TextClause, snap: Option<&Snap>) -> Result<IndexSettings, Failure> {
    let settings = match (&clause.settings, snap) {
        (Some(s), _) => s.clone(),
        (None, Some(snap)) => snap
            .settings
            .indexes
            .get(&clause.index)
            .cloned()
            .ok_or_else(|| {
                Failure::unavailable(format!("the catalog has no text index {}", clause.index))
            })?,
        (None, None) => {
            return Err(Failure::unavailable("the catalog has no text index"));
        }
    };
    if settings.mode == Mode::Compat {
        return Err(Failure::Unsupported(
            "the compatibility mode of the text index is not served".into(),
        ));
    }
    Ok(settings)
}

/// Answer the text query `q`. `texts` is the text indexes of the store,
/// `None` when the store has none.
///
/// An index whose tid is below the pack watermark may lack an unindex that
/// pack removed: the query waits for its rebuild, as for a clear delta
/// that the index lacks. All the waits of one query take at most
/// `catalog-wait` together.
pub fn run(texts: Option<&Texts>, store: &Store, q: &Query<'_>) -> Result<Answer, Failure> {
    let watermark = check_packed(store, q.tid)?;
    let deadline = texts.map(|t| Instant::now() + t.wait());
    let mut at = (watermark > 0).then_some(watermark);
    for _ in 0..3 {
        let snap = match (texts, deadline) {
            (Some(t), Some(deadline)) => t.snap_by(q.catalog, at, deadline),
            _ => Err(Failure::unavailable("the catalog has no text index")),
        };
        match run_on(store, q, snap)? {
            Step::Done(answer) => return Ok(answer),
            Step::Wait(tid) => at = Some(at.map_or(tid, |a| a.max(tid))),
        }
    }
    Err(Failure::unavailable(
        "the text index did not reach the tid that the snapshot needs",
    ))
}

/// The pack watermark, `None` before the first pack.
fn q_watermark(store: &Store) -> Result<Option<u64>, Failure> {
    let watermark = store.pack_watermark().map_err(|e| Failure::of(&e))?;
    Ok((watermark.0 > 0).then_some(watermark.0))
}

enum Step {
    Done(Answer),
    /// The index must hold the deltas up to this tid.
    Wait(u64),
}

fn run_on(store: &Store, q: &Query<'_>, snap: Result<Arc<Snap>, Failure>) -> Result<Step, Failure> {
    let c = q.clause;
    let settings =
        settings_of(c, snap.as_ref().ok().map(|s| &**s)).map_err(|e| match (&snap, e) {
            (Err(f), Failure::Text(_)) => f.clone(),
            (_, e) => e,
        })?;
    let weights = settings.read(&c.index);
    let cleared = |i: &str| c.cleared.iter().any(|x| x == i);
    // Each index in `cl` answers from the staged texts alone; the others
    // answer from the index too. With every index of the query in `cl`,
    // the query needs no index.
    let alone = weights.iter().all(|(i, _)| cleared(i));
    let snap = match snap {
        Ok(s) => Some(s),
        Err(_) if alone => None,
        Err(f) => return Err(f),
    };
    let fields: Fields = match &snap {
        Some(s) => s.fields.clone(),
        None => {
            let layout = weights
                .iter()
                .map(|(i, _)| (i.clone(), Mode::Enhanced, settings.language.clone()))
                .collect();
            index::schema(&layout).1
        }
    };
    let reads: Vec<Read> = weights
        .iter()
        .flat_map(|(i, w)| {
            fields
                .of(i, c.languages.as_deref())
                .into_iter()
                .map(move |field| Read { field, weight: *w })
        })
        .collect();
    let read_indexes: Vec<&str> = weights.iter().map(|(i, _)| i.as_str()).collect();
    // The fields that the search of the index reads: those of the indexes
    // that the transaction did not clear.
    let committed: Vec<Read> = weights
        .iter()
        .filter(|(i, _)| !cleared(i))
        .flat_map(|(i, w)| {
            fields
                .of(i, c.languages.as_deref())
                .into_iter()
                .map(move |field| Read { field, weight: *w })
        })
        .collect();
    let catalog = Oid(q.catalog);
    let tid = q.tid;
    // The documents that the snapshot replaces.
    let mut replaced: BTreeSet<i32> = BTreeSet::new();
    if !alone && let Some(s) = &snap {
        if let Some(at) = q_watermark(store)?
            && s.tid < at
        {
            // A pack may have removed an unindex that the index lacks: it
            // rebuilds.
            return Ok(Step::Wait(at));
        }
        for i in read_indexes.iter().filter(|i| !cleared(i)) {
            if s.cleared.get(*i).is_some_and(|&clear| clear >= tid) {
                return Err(error(
                    TextErrorKind::Cleared,
                    format!("the snapshot is before a clear delta of {i} that the index holds"),
                ));
            }
        }
        let before = tid.saturating_sub(1);
        let (lo, hi) = (s.tid.min(before), s.tid.max(before));
        let changes = store
            .text_changed(Tid(lo), Tid(hi))
            .map_err(|e| Failure::of(&e))?;
        for change in changes {
            if change.catalog != catalog
                || !read_indexes.contains(&change.index.as_str())
                || cleared(&change.index)
            {
                continue;
            }
            match change.rid {
                Some(rid) => {
                    replaced.insert(rid);
                }
                // The index lacks a clear delta of the snapshot: it
                // rebuilds.
                None if change.tid.0 > s.tid => return Ok(Step::Wait(change.tid.0)),
                None => {}
            }
        }
    }
    // The staged texts, the last one of each key.
    let mut staged: HashMap<(&str, i32), Option<(&str, &str)>> = HashMap::new();
    for st in &c.staged {
        if let Some(i) = read_indexes.iter().find(|i| **i == st.index) {
            staged.insert(
                (i, st.rid),
                st.text.as_deref().map(|t| (st.language.as_str(), t)),
            );
        }
    }
    let staged_rids: BTreeSet<i32> = staged.keys().map(|(_, r)| *r).collect();
    let overlay_rids: BTreeSet<i32> = if alone {
        staged_rids
    } else {
        replaced.union(&staged_rids).copied().collect()
    };
    let mut translator = Translator::new(&reads, settings.fuzzy);
    let mut docs = Vec::new();
    for rid in &overlay_rids {
        let mut texts = Vec::new();
        for i in &read_indexes {
            if let Some(found) = staged.get(&(*i, *rid)) {
                if let Some((language, text)) = found {
                    texts.push(((*i).to_owned(), (*language).to_owned(), (*text).to_owned()));
                }
                continue;
            }
            if cleared(i) {
                continue;
            }
            if let Some(t) = store
                .text_before(catalog, i, *rid, Tid(tid))
                .map_err(|e| Failure::of(&e))?
            {
                texts.push(((*i).to_owned(), t.language, t.text));
            }
        }
        docs.push(Doc::new(*rid, &texts, &fields, &reads, &mut translator));
    }
    let own;
    let stats: &dyn Bm25StatisticsProvider = match &snap {
        Some(s) if s.searcher.num_docs() > 0 => &s.searcher,
        _ => {
            own = OwnStats::of(&docs, &reads);
            &own
        }
    };
    let mut scorer = Scorer::new(stats, settings.fuzzy);
    let in_filter = |rid: i32| q.filter.is_none_or(|f| f.binary_search(&rid).is_ok());
    let mut overlay_hits = Vec::new();
    for doc in &docs {
        if !in_filter(doc.rid) {
            continue;
        }
        if let Some(score) = scorer.score(doc, &c.node, &mut translator, &reads) {
            overlay_hits.push((to_int(score), doc.rid));
        }
    }
    let mut found = Found::default();
    let mut strategy = None;
    let mut index_translator = Translator::new(&committed, settings.fuzzy);
    if !alone
        && !committed.is_empty()
        && let Some(s) = &snap
        && let Some(query) = index_translator.translate(&c.node)
    {
        let bound = index_translator.bound(&s.searcher, &c.node);
        let replaced_all: HashSet<i32> = overlay_rids.iter().copied().collect();
        let scope = Scope {
            filter: q.filter,
            replaced: &replaced_all,
        };
        let (f, chosen) = search::search(
            &s.searcher,
            &s.maps,
            query.as_ref(),
            bound,
            &scope,
            q.want,
            q.forced,
        )
        .map_err(|e| Failure::unavailable(format!("the search of the text index fails: {e}")))?;
        found = f;
        strategy = Some(chosen);
    }
    found.count += overlay_hits.len() as u64;
    match q.want {
        Want::Set => found
            .hits
            .extend(overlay_hits.into_iter().map(|(_, rid)| (0, rid))),
        Want::Top(limit) => {
            found.hits.extend(overlay_hits);
            found.hits.sort_unstable_by(|a, b| b.cmp(a));
            if let Some(limit) = limit {
                found.hits.truncate(limit);
            }
        }
    }
    // A reply of hits with their scores holds 8 bytes for each; a set
    // goes through a sort, and the reply caps what the sort keeps.
    if matches!(q.want, Want::Top(_)) && found.hits.len().saturating_mul(8) > MAX_BYTES {
        return Err(error(
            TextErrorKind::TooLarge,
            format!("{} hits pass the cap of a reply", found.hits.len()),
        ));
    }
    Ok(Step::Done(Answer { found, strategy }))
}

/// Answer `text_info`: the documents of `index` at the snapshot before
/// `tid`, the distinct words of the index at its tid, and the live text of
/// `rid` at the snapshot.
pub fn info(
    texts: Option<&Texts>,
    store: &Store,
    catalog: u64,
    index_id: &str,
    tid: u64,
    rid: Option<i32>,
) -> Result<TextInfo, Failure> {
    let watermark = check_packed(store, tid)?;
    let Some(texts) = texts else {
        return Err(Failure::unavailable("the catalog has no text index"));
    };
    let deadline = Instant::now() + texts.wait();
    // An index below the pack watermark rebuilds first.
    let mut at = (watermark > 0).then_some(watermark);
    for _ in 0..2 {
        let snap = texts.snap_by(catalog, at, deadline)?;
        let Some(fields) = snap.fields.indexes.get(index_id) else {
            return Err(Failure::unavailable(format!(
                "the catalog has no text index {index_id}"
            )));
        };
        if snap
            .cleared
            .get(index_id)
            .is_some_and(|&clear| clear >= tid)
        {
            return Err(error(
                TextErrorKind::Cleared,
                format!("the snapshot is before a clear delta of {index_id} that the index holds"),
            ));
        }
        let has_text = |ord: usize, doc: u32| -> bool {
            let seg = &snap.searcher.segment_readers()[ord];
            fields.iter().any(|f| {
                seg.get_fieldnorms_reader(f.text)
                    .is_ok_and(|r| r.fieldnorm(doc) > 0)
            })
        };
        let mut docs: i64 = 0;
        for (ord, seg) in snap.searcher.segment_readers().iter().enumerate() {
            for doc in 0..seg.max_doc() {
                if seg.alive_bitset().is_none_or(|a| a.is_alive(doc)) && has_text(ord, doc) {
                    docs += 1;
                }
            }
        }
        let before = tid.saturating_sub(1);
        let (lo, hi) = (snap.tid.min(before), snap.tid.max(before));
        let changes = store
            .text_changed(Tid(lo), Tid(hi))
            .map_err(|e| Failure::of(&e))?;
        let mut wait = None;
        let mut changed = BTreeSet::new();
        for change in changes {
            if change.catalog.0 != catalog || change.index != index_id {
                continue;
            }
            match change.rid {
                Some(r) => {
                    changed.insert(r);
                }
                None if change.tid.0 > snap.tid => wait = Some(change.tid.0),
                None => {}
            }
        }
        if let Some(w) = wait {
            at = Some(w);
            continue;
        }
        for r in changed {
            let in_index = snap
                .maps
                .doc(r)
                .is_some_and(|(ord, doc)| has_text(ord as usize, doc));
            let at_snapshot = store
                .text_before(Oid(catalog), index_id, r, Tid(tid))
                .map_err(|e| Failure::of(&e))?
                .is_some();
            docs += i64::from(at_snapshot) - i64::from(in_index);
        }
        let mut words = 0u64;
        for f in fields {
            let segs = snap.searcher.segment_readers();
            if segs.len() == 1 {
                let inv = segs[0]
                    .inverted_index(f.text)
                    .map_err(|e| Failure::unavailable(e.to_string()))?;
                words += inv.terms().num_terms() as u64;
                continue;
            }
            let mut all = BTreeSet::new();
            for seg in segs {
                let inv = seg
                    .inverted_index(f.text)
                    .map_err(|e| Failure::unavailable(e.to_string()))?;
                let mut stream = inv
                    .terms()
                    .stream()
                    .map_err(|e| Failure::unavailable(e.to_string()))?;
                while stream.advance() {
                    all.insert(stream.key().to_vec());
                }
            }
            words += all.len() as u64;
        }
        let text = match rid {
            Some(r) => store
                .text_before(Oid(catalog), index_id, r, Tid(tid))
                .map_err(|e| Failure::of(&e))?
                .map(|t| (t.language, t.text)),
            None => None,
        };
        return Ok(TextInfo {
            docs: docs.max(0) as u64,
            words,
            text,
        });
    }
    Err(Failure::unavailable(
        "the text index did not reach a clear delta of the snapshot",
    ))
}

/// Answer the call `text_query(catalog, tid, clause, filter)` (ADR 0060,
/// section 12): every hit in the filter, in rid order, with its integer
/// score. A clause or a filter of another form is `Malformed`; the
/// compatibility mode is `Unsupported`. A call past the cap of a call gives
/// `too_large`.
pub fn text_query(
    texts: Option<&Texts>,
    store: &Store,
    catalog: u64,
    tid: u64,
    clause: &crate::plan::Wire,
    filter: &crate::plan::Wire,
) -> Result<Result<super::TextHits, TextError>, super::CallError> {
    use super::CallError;
    let parsed = super::clause::decode_clause(clause, false).map_err(CallError::Malformed)?;
    let filter = super::clause::decode_filter(filter).map_err(CallError::Malformed)?;
    if super::clause::too_large(clause) {
        return Ok(Err(TextError::new(
            TextErrorKind::TooLarge,
            "the clause passes the cap of a call",
        )));
    }
    let query = Query {
        catalog,
        tid,
        clause: &parsed,
        filter: filter.as_deref(),
        want: Want::Top(None),
        forced: None,
    };
    match run(texts, store, &query) {
        Ok(answer) => {
            let mut hits = answer.found.hits;
            hits.sort_unstable_by_key(|h| h.1);
            Ok(Ok(super::TextHits {
                rids: hits.iter().map(|h| h.1).collect(),
                scores: hits
                    .iter()
                    .map(|h| h.0.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32)
                    .collect(),
            }))
        }
        Err(Failure::Text(e)) => Ok(Err(e)),
        Err(Failure::Unsupported(why)) => Err(CallError::Unsupported(why)),
    }
}

/// Answer the call `text_info(catalog, index, tid, rid)`. A bad index id or
/// a rid outside the signed 32-bit range is `Malformed`.
pub fn text_info(
    texts: Option<&Texts>,
    store: &Store,
    catalog: u64,
    index_id: &str,
    tid: u64,
    rid: Option<i64>,
) -> Result<Result<TextInfo, TextError>, super::CallError> {
    use super::CallError;
    super::settings::check_index(index_id).map_err(CallError::Malformed)?;
    let rid = rid
        .map(zoe_core::check_rid)
        .transpose()
        .map_err(|e| CallError::Malformed(e.to_string()))?;
    match info(texts, store, catalog, index_id, tid, rid) {
        Ok(found) => Ok(Ok(found)),
        Err(Failure::Text(e)) => Ok(Err(e)),
        Err(Failure::Unsupported(why)) => Err(CallError::Unsupported(why)),
    }
}
