//! The text deltas (ADR 0060): the reads of the store, the reservations of
//! the votes, the clear deltas in memory and the check of `zoe verify`.
//!
//! A text delta is the text of one document in one text index of one
//! catalog at one tid. A clear delta empties one text index of one catalog
//! at one tid. `format::text` gives their keys and payloads, and the
//! commit path (`commit`) stages, checks and writes them.
//!
//! The live text of a document at a snapshot is the newest text delta of
//! its key before the snapshot, when that delta is not an unindex and not
//! before the newest clear delta of its index before the snapshot. A text
//! delta at the tid of that clear is live: its transaction staged it after
//! the clear.
//!
//! The store keeps the clear deltas of each index that a read asked for in
//! memory, as `Clears` tells. A read never looks past the last tid of the
//! store, so a commit whose clear is in memory and whose deltas are not
//! visible yet changes no read.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Bound::{self, Excluded, Included, Unbounded};
use std::sync::Mutex;

use crate::error::{Result, ZoeError};
use crate::format::{self, KeySpace, text};
use crate::store::Store;
use crate::types::{Oid, Tid};

/// The live text of a document at a snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveText {
    /// The language: a BCP 47 tag, or empty.
    pub language: String,
    /// The text.
    pub text: String,
}

/// The key of one delta that `Store::text_changed` gives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextChange {
    /// The tid of the delta.
    pub tid: Tid,
    /// The oid of the catalog.
    pub catalog: Oid,
    /// The index id.
    pub index: String,
    /// The rid of a text delta. A clear delta has none.
    pub rid: Option<i32>,
}

/// One delta, as `Store::text_deltas_at` gives it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextDelta {
    /// The oid of the catalog.
    pub catalog: Oid,
    /// The index id.
    pub index: String,
    /// The rid of a text delta. A clear delta has none.
    pub rid: Option<i32>,
    /// The language and the text of a text delta. An unindex and a clear
    /// delta have none.
    pub text: Option<LiveText>,
}

/// A problem that `Store::verify_text` found. It names the catalog, the
/// index, the rid and the tid of the delta, never the text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextFinding {
    /// The oid of the catalog, when the key gives it.
    pub catalog: Option<Oid>,
    /// The index id, when the key gives it.
    pub index: Option<String>,
    /// The rid of a text delta. A clear delta, and a key that gives none,
    /// have none.
    pub rid: Option<i32>,
    /// The tid of the delta, when the key gives it.
    pub tid: Option<Tid>,
    /// What is wrong.
    pub problem: String,
}

/// What `Store::verify_text` found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TextCheck {
    /// The text deltas, unindexes included.
    pub text_deltas: u64,
    /// The clear deltas.
    pub clear_deltas: u64,
    /// The problems.
    pub findings: Vec<TextFinding>,
}

/// Check the index id of a delta: 1 to 255 bytes. A bad one is
/// `InvalidText`.
pub(crate) fn check_index(index: &str) -> Result<()> {
    match index.len() {
        0 => Err(ZoeError::InvalidText("an empty index id")),
        len if len > text::MAX_INDEX_LEN => {
            Err(ZoeError::InvalidText("an index id longer than 255 bytes"))
        }
        _ => Ok(()),
    }
}

/// Check the language of a text delta: at most 35 bytes of ASCII. A bad
/// one is `InvalidText`.
pub(crate) fn check_language(language: &str) -> Result<()> {
    if language.len() > text::MAX_LANGUAGE_LEN {
        return Err(ZoeError::InvalidText("a language longer than 35 bytes"));
    }
    if !language.is_ascii() {
        return Err(ZoeError::InvalidText("a language that is not ASCII"));
    }
    Ok(())
}

/// Return `rid` as the rid of a delta: a signed 32-bit int, as the rids of
/// ZCatalog. A rid outside that range is `InvalidText`. A skin that reads a
/// rid of another width checks it with this call.
pub fn check_rid(rid: i64) -> Result<i32> {
    i32::try_from(rid).map_err(|_| ZoeError::InvalidText("a rid outside the signed 32-bit range"))
}

/// Return the error of a delta whose frame or key fails: `CorruptText`,
/// with the parts of `key`, a key of `text_deltas`, as far as it reads.
pub(crate) fn corrupt_key(key: &[u8]) -> ZoeError {
    let tid = text::key_tid(key).unwrap_or(Tid::ZERO);
    match text::split_key(key) {
        Some((parts, _, tid)) => ZoeError::CorruptText {
            catalog: parts.catalog,
            index: parts.index.to_owned(),
            rid: parts.rid,
            tid,
        },
        None => {
            let catalog = key
                .get(..8)
                .map_or(0, |bytes| u64::from_be_bytes(bytes.try_into().unwrap()));
            ZoeError::CorruptText {
                catalog: Oid(catalog),
                index: String::new(),
                rid: None,
                tid,
            }
        }
    }
}

/// Return the error of a read of `prefix`, a key of `text_deltas` without
/// its tid, that met the damaged key or frame `key`: `CorruptText` with the
/// parts of `prefix` and the tid of `key`.
pub(crate) fn corrupt(prefix: &[u8], key: &[u8]) -> ZoeError {
    let tid = text::key_tid(key).unwrap_or(Tid::ZERO);
    match text::split_prefix(prefix) {
        Some(parts) => ZoeError::CorruptText {
            catalog: parts.catalog,
            index: parts.index.to_owned(),
            rid: parts.rid,
            tid,
        },
        None => corrupt_key(key),
    }
}

/// Decode the stored frame `frame` of the delta `key`, a key of the delta
/// `prefix`. Return its previous serial and its payload. A frame that
/// fails is `CorruptText`.
pub(crate) fn decode(prefix: &[u8], key: &[u8], frame: &[u8]) -> Result<(Tid, Vec<u8>)> {
    match format::decode_frame(KeySpace::TextDeltas, key, frame) {
        Ok(frame) => Ok((frame.prev, frame.data.into_owned())),
        Err(_) => Err(corrupt(prefix, key)),
    }
}

/// The keys of the text deltas that the votes of the store reserve, under
/// one mutex. The mutex is held with no other lock taken under it.
#[derive(Default)]
pub(crate) struct Reservations {
    keys: Mutex<HashSet<Vec<u8>>>,
}

impl Reservations {
    /// Reserve each key of `keys`, keys of `text_deltas` without their
    /// tid. When another transaction reserved one of them, reserve none and
    /// return the catalog of the first such key.
    pub(crate) fn reserve(&self, keys: &[Vec<u8>]) -> std::result::Result<(), Oid> {
        let mut reserved = crate::lock(&self.keys);
        if let Some(taken) = keys.iter().find(|key| reserved.contains(*key)) {
            return Err(catalog_of(taken));
        }
        reserved.extend(keys.iter().cloned());
        Ok(())
    }

    /// Release each key of `keys`.
    pub(crate) fn release(&self, keys: &[Vec<u8>]) {
        if keys.is_empty() {
            return;
        }
        let mut reserved = crate::lock(&self.keys);
        for key in keys {
            reserved.remove(key);
        }
    }
}

/// Return the catalog of `key`, a key of `text_deltas` with or without
/// its tid.
pub(crate) fn catalog_of(key: &[u8]) -> Oid {
    Oid(key
        .get(..8)
        .map_or(0, |bytes| u64::from_be_bytes(bytes.try_into().unwrap())))
}

/// The clear deltas of the indexes that the reads asked for, in memory.
///
/// A read that finds no list of its index reads the clear deltas from the
/// store, then keeps the list only when no commit with a clear delta ran
/// meanwhile: `pending` counts the commits that write one, and `generation`
/// grows at the end of each. A commit adds its clear deltas to the lists
/// before its backend write, under the commit sequencer.
///
/// The memory stays bounded: an index with no clear delta keeps no list, so
/// an index id that a client invents leaves nothing; the struct keeps at
/// most `CLEAR_LISTS` lists of at most `CLEAR_TIDS` tids each. A list past
/// its bound leaves, and its reads go to the store.
#[derive(Default)]
pub(crate) struct Clears {
    inner: Mutex<ClearsInner>,
}

#[derive(Default)]
struct ClearsInner {
    /// The tids of the clear deltas of each index, in ascending order, by
    /// key of the clear delta without its tid.
    lists: HashMap<Vec<u8>, Vec<Tid>>,
    pending: u64,
    generation: u64,
}

impl Clears {
    /// Add the clear deltas `clears` of a commit to the lists, before the
    /// backend write of the commit. `end` follows, whatever the outcome.
    pub(crate) fn begin(&self, clears: &[(Vec<u8>, Tid)]) {
        let mut inner = crate::lock(&self.inner);
        inner.pending += 1;
        for (prefix, tid) in clears {
            if let Some(list) = inner.lists.get_mut(prefix)
                && list.last().is_none_or(|last| last < tid)
            {
                list.push(*tid);
                if list.len() > CLEAR_TIDS {
                    inner.lists.remove(prefix);
                }
            }
        }
    }

    /// End the commit that `begin` started.
    pub(crate) fn end(&self) {
        let mut inner = crate::lock(&self.inner);
        inner.pending -= 1;
        inner.generation += 1;
    }

    /// Return the tid of the newest clear delta of `prefix`, the key of a
    /// clear delta without its tid, before `before`.
    fn newest_before(&self, store: &Store, prefix: &[u8], before: Tid) -> Result<Option<Tid>> {
        let generation = {
            let inner = crate::lock(&self.inner);
            if let Some(list) = inner.lists.get(prefix) {
                return Ok(newest_of(list, before));
            }
            inner.generation
        };
        let list = store.read_clears(prefix)?;
        let newest = newest_of(&list, before);
        let mut inner = crate::lock(&self.inner);
        if inner.pending == 0
            && inner.generation == generation
            && !list.is_empty()
            && list.len() <= CLEAR_TIDS
            && inner.lists.len() < CLEAR_LISTS
        {
            inner.lists.insert(prefix.to_vec(), list);
        }
        Ok(newest)
    }
}

/// Return the newest tid of `list`, in ascending order, before `before`.
fn newest_of(list: &[Tid], before: Tid) -> Option<Tid> {
    let at = list.partition_point(|&tid| tid < before);
    at.checked_sub(1).map(|at| list[at])
}

/// The rows that one backend read of a scan gives.
const SCAN_CHUNK: usize = 256;

/// The distinct tids that `Store::verify_text` keeps before it checks
/// their transaction rows.
const VERIFY_TIDS: usize = 65_536;

/// The most lists of clear deltas that `Clears` keeps.
const CLEAR_LISTS: usize = 4_096;

/// The most clear deltas of one index that `Clears` keeps.
const CLEAR_TIDS: usize = 4_096;

/// Return the bounds of the keys of `text_deltas` of `prefix`, a key
/// without its tid, at every tid.
fn every_tid(prefix: &[u8]) -> (Vec<u8>, Vec<u8>) {
    (
        text::delta_key(prefix, Tid::ZERO),
        text::delta_key(prefix, Tid::MAX),
    )
}

impl Store {
    /// Return the language and the live text of `rid` in the text index
    /// `index` of the catalog `catalog` before `tid`, or `None` when the
    /// document has no live text there. `tid` is exclusive, as the tid of
    /// `loadBefore`. A tid after the last tid reads at the last tid.
    ///
    /// The read takes the newest clear delta of the index before the
    /// snapshot, then makes one reverse range scan of the key from the
    /// snapshot. A text delta before that clear is dead. A frame that
    /// fails is `CorruptText`, never an absent text. A bad index id is
    /// `InvalidText`.
    pub fn text_before(
        &self,
        catalog: Oid,
        index: &str,
        rid: i32,
        tid: Tid,
    ) -> Result<Option<LiveText>> {
        check_index(index)?;
        let snapshot = self.text_snapshot(tid);
        let payload = self.live_payload(catalog, index, rid, snapshot)?;
        Ok(payload.map(|payload| {
            let (language, text) = text::decode_payload(&payload)
                .ok()
                .flatten()
                .expect("a live payload decodes");
            LiveText {
                language: language.to_owned(),
                text: text.to_owned(),
            }
        }))
    }

    /// Return the key of each delta of the tids after `after` and at or
    /// before `last`, in tid order, then in key order: a scan of
    /// `text_txn`. The deltas of the backlog of the write-ahead log are in
    /// it. The scan reads no frame.
    pub fn text_changed(&self, after: Tid, last: Tid) -> Result<Vec<TextChange>> {
        let mut changes = Vec::new();
        if after >= last {
            return Ok(changes);
        }
        let from = Tid(after.0 + 1).0.to_be_bytes().to_vec();
        let end = last.0.checked_add(1).map(|end| end.to_be_bytes().to_vec());
        let mut low: Bound<Vec<u8>> = Included(from);
        loop {
            let to = match &end {
                Some(end) => Excluded(end.as_slice()),
                None => Unbounded,
            };
            let keys = self.backend.text_txn_keys(bound(&low), to, SCAN_CHUNK)?;
            let full = keys.len() == SCAN_CHUNK;
            for key in &keys {
                let (tid, prefix) =
                    text::split_txn_key(key).ok_or(ZoeError::CorruptTxn { tid: Tid::ZERO })?;
                let parts = text::split_prefix(prefix).ok_or(ZoeError::CorruptTxn { tid })?;
                changes.push(TextChange {
                    tid,
                    catalog: parts.catalog,
                    index: parts.index.to_owned(),
                    rid: parts.rid,
                });
            }
            match keys.last() {
                Some(last) if full => low = Excluded(last.clone()),
                _ => return Ok(changes),
            }
        }
    }

    /// Return the deltas of the transaction `tid`, in the key order of
    /// `text_deltas`: the clear deltas of a catalog before its text
    /// deltas. A transaction without a delta gives none. Each frame passes
    /// its checksum, or the read is `CorruptText`. A key of `text_txn`
    /// without its delta is `CorruptText` too.
    ///
    /// The call holds the deltas of the transaction in memory at once, as
    /// the transaction iterator holds its records.
    pub fn text_deltas_at(&self, tid: Tid) -> Result<Vec<TextDelta>> {
        let Some(after) = tid.0.checked_sub(1).map(Tid) else {
            return Ok(Vec::new());
        };
        let mut deltas = Vec::new();
        for change in self.text_changed(after, tid)? {
            let prefix = match change.rid {
                Some(rid) => text::text_prefix(change.catalog, &change.index, rid),
                None => text::clear_prefix(change.catalog, &change.index),
            };
            let key = text::delta_key(&prefix, tid);
            let at = Included(key.as_slice());
            let Some((_, frame)) = self.backend.text_rows(at, at, 1)?.pop() else {
                return Err(corrupt(&prefix, &key));
            };
            let (_, payload) = decode(&prefix, &key, &frame)?;
            let text = match (change.rid, text::decode_payload(&payload)) {
                (None, _) if !payload.is_empty() => return Err(corrupt(&prefix, &key)),
                (_, Err(())) => return Err(corrupt(&prefix, &key)),
                (Some(_), Ok(Some((language, text)))) => Some(LiveText {
                    language: language.to_owned(),
                    text: text.to_owned(),
                }),
                _ => None,
            };
            deltas.push(TextDelta {
                catalog: change.catalog,
                index: change.index,
                rid: change.rid,
                text,
            });
        }
        Ok(deltas)
    }

    /// Return the key of each delta after the delta `after`, or from the
    /// first one for `None`, at most `limit` of them, in the key order of
    /// `text_deltas`: by catalog, the clear deltas of a catalog before its
    /// text deltas, by index id and rid, and the deltas of one key from the
    /// oldest. The deltas of the backlog of the write-ahead log are in it.
    /// The scan checks no frame. A key that does not read is
    /// `CorruptText`.
    pub fn text_keys(&self, after: Option<&TextChange>, limit: usize) -> Result<Vec<TextChange>> {
        let from = after.map(|after| {
            let prefix = match after.rid {
                Some(rid) => text::text_prefix(after.catalog, &after.index, rid),
                None => text::clear_prefix(after.catalog, &after.index),
            };
            text::delta_key(&prefix, after.tid)
        });
        let low = match &from {
            Some(key) => Excluded(key.as_slice()),
            None => Unbounded,
        };
        self.backend
            .text_rows(low, Unbounded, limit)?
            .iter()
            .map(|(key, _)| {
                let (parts, _, tid) = text::split_key(key).ok_or_else(|| corrupt_key(key))?;
                Ok(TextChange {
                    tid,
                    catalog: parts.catalog,
                    index: parts.index.to_owned(),
                    rid: parts.rid,
                })
            })
            .collect()
    }

    /// Return the snapshot of a read of the deltas before `tid`: `tid`,
    /// or the tid after the last tid when `tid` is after it.
    pub(crate) fn text_snapshot(&self, tid: Tid) -> Tid {
        let last = self.last_transaction();
        tid.min(Tid(last.0.saturating_add(1)))
    }

    /// Return the payload of the live text of `rid` in `index` of
    /// `catalog` before `snapshot`, or `None`. `snapshot` is at most the
    /// tid after the last tid.
    pub(crate) fn live_payload(
        &self,
        catalog: Oid,
        index: &str,
        rid: i32,
        snapshot: Tid,
    ) -> Result<Option<Vec<u8>>> {
        let clear =
            self.text_clears
                .newest_before(self, &text::clear_prefix(catalog, index), snapshot)?;
        let prefix = text::text_prefix(catalog, index, rid);
        let Some((key, frame)) = self.backend.text_newest(&prefix, snapshot)? else {
            return Ok(None);
        };
        let tid = text::key_tid(&key).ok_or_else(|| corrupt(&prefix, &key))?;
        let (_, payload) = decode(&prefix, &key, &frame)?;
        if text::decode_payload(&payload).is_err() {
            return Err(corrupt(&prefix, &key));
        }
        if clear.is_some_and(|clear| tid < clear) || payload.is_empty() {
            return Ok(None);
        }
        Ok(Some(payload))
    }

    /// Return the payload of the live text of the key `prefix` now, at the
    /// last tid, or `None`.
    pub(crate) fn current_payload(&self, prefix: &[u8]) -> Result<Option<Vec<u8>>> {
        let parts = text::split_prefix(prefix).ok_or_else(|| corrupt_key(prefix))?;
        let rid = parts.rid.ok_or_else(|| corrupt_key(prefix))?;
        let snapshot = self.text_snapshot(Tid::MAX);
        self.live_payload(parts.catalog, parts.index, rid, snapshot)
    }

    /// Tell if a delta of the key `prefix`, or a clear delta of its index,
    /// committed at or after `before`.
    pub(crate) fn text_committed_since(&self, prefix: &[u8], before: Tid) -> Result<bool> {
        let parts = text::split_prefix(prefix).ok_or_else(|| corrupt_key(prefix))?;
        let snapshot = self.text_snapshot(Tid::MAX);
        let clear = self.text_clears.newest_before(
            self,
            &text::clear_prefix(parts.catalog, parts.index),
            snapshot,
        )?;
        if clear.is_some_and(|clear| clear >= before) {
            return Ok(true);
        }
        let newest = self.backend.text_newest(prefix, snapshot)?;
        Ok(newest
            .and_then(|(key, _)| text::key_tid(&key))
            .is_some_and(|tid| tid >= before))
    }

    /// Return the tid of the newest delta of `prefix`, a key without its
    /// tid, from the key alone: the previous serial of a new delta of the
    /// key. The finish reads it under the commit sequencer and checks no
    /// frame, so it never fails for a damaged delta.
    pub(crate) fn text_previous(&self, prefix: &[u8]) -> Result<Tid> {
        Ok(self
            .backend
            .text_newest_key(prefix, Tid::MAX)?
            .and_then(|key| text::key_tid(&key))
            .unwrap_or(Tid::ZERO))
    }

    /// Read the tids of the clear deltas of `prefix`, the key of a clear
    /// delta without its tid, from the store, in ascending order. Each frame
    /// passes its checksum, or the read is `CorruptText`.
    fn read_clears(&self, prefix: &[u8]) -> Result<Vec<Tid>> {
        let (start, end) = every_tid(prefix);
        let mut tids = Vec::new();
        let mut low: Bound<Vec<u8>> = Included(start);
        loop {
            let rows = self
                .backend
                .text_rows(bound(&low), Included(end.as_slice()), SCAN_CHUNK)?;
            let full = rows.len() == SCAN_CHUNK;
            for (key, frame) in &rows {
                let tid = text::key_tid(key)
                    .filter(|_| key.len() == prefix.len() + 8 && key.starts_with(prefix))
                    .ok_or_else(|| corrupt(prefix, key))?;
                let (_, payload) = decode(prefix, key, frame)?;
                if !payload.is_empty() {
                    return Err(corrupt(prefix, key));
                }
                tids.push(tid);
            }
            match rows.last() {
                Some((last, _)) if full => low = Excluded(last.clone()),
                _ => return Ok(tids),
            }
        }
    }

    /// Check every row of `text_deltas` and `text_txn` for `zoe verify`:
    /// each frame and each key, the previous serial of each delta, the
    /// transaction row of each tid, and that each delta has its key in
    /// `text_txn` and each key of `text_txn` its delta. A missing previous
    /// delta at or before `pack_watermark` passes: a pack removed it. A
    /// finding names the catalog, the index, the rid and the tid, never the
    /// text. A scan that fails gives one finding and ends its part of the
    /// check.
    pub fn verify_text(&self) -> Result<TextCheck> {
        let mut check = TextCheck::default();
        let watermark = self.pack_watermark()?;
        // The first delta of each tid since the last check of the rows of
        // the transactions, and the number of its deltas. The check runs
        // when the map holds `VERIFY_TIDS` tids, so its memory stays
        // bounded on a store with a long history.
        let mut tids: BTreeMap<Tid, (TextFinding, u64)> = BTreeMap::new();
        // The tids without a transaction row, with their finding and the
        // number of their deltas: a tid that comes back after a check adds
        // its deltas to its finding.
        let mut missing: HashMap<Tid, (usize, u64)> = HashMap::new();
        // The prefix and the tid of the row before, in key order.
        let mut previous: Option<(Vec<u8>, Tid)> = None;
        let mut low: Bound<Vec<u8>> = Unbounded;
        loop {
            let rows = match self.backend.text_rows(bound(&low), Unbounded, SCAN_CHUNK) {
                Ok(rows) => rows,
                Err(error) => {
                    check.findings.push(finding(
                        None,
                        format!("the scan of the deltas failed: {error}"),
                    ));
                    break;
                }
            };
            let full = rows.len() == SCAN_CHUNK;
            for (key, frame) in &rows {
                self.verify_row(key, frame, watermark, &mut previous, &mut tids, &mut check);
            }
            if tids.len() >= VERIFY_TIDS {
                self.verify_txn_rows(&mut tids, &mut missing, &mut check);
            }
            match rows.last() {
                Some((last, _)) if full => low = Excluded(last.clone()),
                _ => break,
            }
        }
        self.verify_txn_rows(&mut tids, &mut missing, &mut check);
        self.verify_index(&mut check);
        Ok(check)
    }

    /// Check that each tid of `tids` has its transaction row, for
    /// `verify_text`, and empty `tids`.
    fn verify_txn_rows(
        &self,
        tids: &mut BTreeMap<Tid, (TextFinding, u64)>,
        missing: &mut HashMap<Tid, (usize, u64)>,
        check: &mut TextCheck,
    ) {
        let problem = |deltas: u64| match deltas {
            1 => "a delta without its transaction row".to_owned(),
            n => format!("a delta without its transaction row, the first of {n} deltas of the tid"),
        };
        for (tid, (first, deltas)) in std::mem::take(tids) {
            if let Some((at, total)) = missing.get_mut(&tid) {
                *total += deltas;
                check.findings[*at].problem = problem(*total);
                continue;
            }
            let row = self.backend.iter_txns(tid, tid).next();
            if !matches!(row, Some(Ok(ref meta)) if meta.tid == tid) {
                missing.insert(tid, (check.findings.len(), deltas));
                check.findings.push(TextFinding {
                    problem: problem(deltas),
                    ..first
                });
            }
        }
    }

    /// Check one row of `text_deltas` for `verify_text`.
    fn verify_row(
        &self,
        key: &[u8],
        frame: &[u8],
        watermark: Tid,
        previous: &mut Option<(Vec<u8>, Tid)>,
        tids: &mut BTreeMap<Tid, (TextFinding, u64)>,
        check: &mut TextCheck,
    ) {
        let Some((parts, prefix, tid)) = text::split_key(key) else {
            let tid = text::key_tid(key);
            check.findings.push(TextFinding {
                catalog: key.get(..8).map(|_| catalog_of(key)),
                tid,
                ..finding(None, "the key of a delta does not read".to_owned())
            });
            return;
        };
        let named = |problem: &str| TextFinding {
            catalog: Some(parts.catalog),
            index: Some(parts.index.to_owned()),
            rid: parts.rid,
            tid: Some(tid),
            problem: problem.to_owned(),
        };
        match parts.rid {
            Some(_) => check.text_deltas += 1,
            None => check.clear_deltas += 1,
        }
        tids.entry(tid).or_insert_with(|| (named(""), 0)).1 += 1;
        let expected = match previous.take() {
            Some((last, last_tid)) if last == prefix => last_tid,
            _ => Tid::ZERO,
        };
        *previous = Some((prefix.to_vec(), tid));
        match format::decode_frame(KeySpace::TextDeltas, key, frame) {
            Err(_) => check
                .findings
                .push(named("the frame fails its checksum or does not decode")),
            Ok(decoded) => {
                let payload_ok = match parts.rid {
                    Some(_) => text::decode_payload(&decoded.data).is_ok(),
                    None => decoded.data.is_empty(),
                };
                if !payload_ok {
                    check.findings.push(named("the payload does not decode"));
                }
                let prev = decoded.prev;
                // A pack removes deltas at or before its watermark, so a
                // previous delta that is gone there passes.
                let skipped = prev > expected && prev <= watermark && prev < tid;
                if prev != expected && !skipped {
                    check.findings.push(named(&format!(
                        "the previous serial is {:#018x}, and the previous delta of the key is {:#018x}",
                        prev.0, expected.0
                    )));
                }
            }
        }
        let index_key = text::txn_key(tid, prefix);
        let to = Included(index_key.as_slice());
        match self.backend.text_txn_keys(to, to, 1) {
            Ok(keys) if keys.len() == 1 => {}
            Ok(_) => check
                .findings
                .push(named("a delta without its key in text_txn")),
            Err(error) => check.findings.push(named(&format!(
                "the read of its key in text_txn failed: {error}"
            ))),
        }
    }

    /// Check that each key of `text_txn` has its delta, for `verify_text`.
    fn verify_index(&self, check: &mut TextCheck) {
        let mut low: Bound<Vec<u8>> = Unbounded;
        loop {
            let keys = match self
                .backend
                .text_txn_keys(bound(&low), Unbounded, SCAN_CHUNK)
            {
                Ok(keys) => keys,
                Err(error) => {
                    check.findings.push(finding(
                        None,
                        format!("the scan of text_txn failed: {error}"),
                    ));
                    return;
                }
            };
            let full = keys.len() == SCAN_CHUNK;
            for key in &keys {
                let Some((tid, prefix)) = text::split_txn_key(key) else {
                    check.findings.push(finding(
                        None,
                        "a key of text_txn shorter than a tid".to_owned(),
                    ));
                    continue;
                };
                let parts = text::split_prefix(prefix);
                let named = |problem: &str| TextFinding {
                    catalog: parts.as_ref().map(|parts| parts.catalog),
                    index: parts.as_ref().map(|parts| parts.index.to_owned()),
                    rid: parts.as_ref().and_then(|parts| parts.rid),
                    tid: Some(tid),
                    problem: problem.to_owned(),
                };
                if parts.is_none() {
                    check
                        .findings
                        .push(named("a key of text_txn that does not read"));
                    continue;
                }
                let row = text::delta_key(prefix, tid);
                let at = Included(row.as_slice());
                match self.backend.text_rows(at, at, 1) {
                    Ok(rows) if rows.len() == 1 => {}
                    Ok(_) => check
                        .findings
                        .push(named("a key of text_txn without its delta")),
                    Err(error) => check
                        .findings
                        .push(named(&format!("the read of its delta failed: {error}"))),
                }
            }
            match keys.last() {
                Some(last) if full => low = Excluded(last.clone()),
                _ => return,
            }
        }
    }
}

/// Return a finding that names no delta.
fn finding(catalog: Option<Oid>, problem: String) -> TextFinding {
    TextFinding {
        catalog,
        index: None,
        rid: None,
        tid: None,
        problem,
    }
}

/// Borrow the bound `bound`.
fn bound(bound: &Bound<Vec<u8>>) -> Bound<&[u8]> {
    match bound {
        Included(key) => Included(key.as_slice()),
        Excluded(key) => Excluded(key.as_slice()),
        Unbounded => Unbounded,
    }
}
