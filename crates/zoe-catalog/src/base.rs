//! The base of a mirror: the rows and the indexes of one catalog at one
//! tid. A base never changes. The commits after its tid are change sets
//! (`change`).

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use roaring::RoaringBitmap;

use crate::security::{SECURITY_INDEX, SecurityIndex};
use crate::table::Table;
use crate::value::{StrArena, Val, py_cmp};

/// The marker of a row without a key.
pub const NONE: u32 = u32::MAX;
/// The high bit of a posting that names a bitmap.
const MANY: u32 = 0x8000_0000;

/// The class of a FieldIndex.
pub const FIELD_INDEX: (&str, &str) =
    ("Products.PluginIndexes.FieldIndex.FieldIndex", "FieldIndex");
/// The class of a KeywordIndex.
pub const KEYWORD_INDEX: (&str, &str) = (
    "Products.PluginIndexes.KeywordIndex.KeywordIndex",
    "KeywordIndex",
);
/// The class of a DateIndex.
pub const DATE_INDEX: (&str, &str) = ("Products.PluginIndexes.DateIndex.DateIndex", "DateIndex");
/// The class of a DateRangeIndex.
pub const DATE_RANGE_INDEX: (&str, &str) = (
    "Products.PluginIndexes.DateRangeIndex.DateRangeIndex",
    "DateRangeIndex",
);
/// The class of a BooleanIndex.
pub const BOOLEAN_INDEX: (&str, &str) = (
    "Products.PluginIndexes.BooleanIndex.BooleanIndex",
    "BooleanIndex",
);
/// The class of a UUIDIndex.
pub const UUID_INDEX: (&str, &str) = ("Products.PluginIndexes.UUIDIndex.UUIDIndex", "UUIDIndex");
/// The class of an ExtendedPathIndex.
pub const EXTENDED_PATH_INDEX: (&str, &str) = (
    "Products.ExtendedPathIndex.ExtendedPathIndex",
    "ExtendedPathIndex",
);

/// The class of a mirrored index. Each reads its `_unindex` tree and its
/// `_length`, and no other tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    /// A FieldIndex: `_unindex` maps a rid to its key.
    Field,
    /// A KeywordIndex: `_unindex` maps a rid to the list of its keys.
    Keyword,
    /// A DateIndex: `_unindex` maps a rid to its date in minutes.
    Date,
    /// A DateRangeIndex: `_unindex` maps a rid to `(since, until)`, each
    /// in minutes or `None`.
    DateRange,
    /// A BooleanIndex: `_unindex` maps a rid to 0 or 1.
    Boolean,
    /// A UUIDIndex: `_unindex` maps a rid to its UUID.
    Uuid,
    /// An ExtendedPathIndex: `_unindex` maps a rid to its path, a str.
    Path,
    /// The `ZoeSecurityIndex` of `zoe.plone`: `_inputs` maps a rid to the
    /// inputs of its value (`security`). It reads `_inputs` in the place
    /// of `_unindex`.
    Security,
}

impl Kind {
    /// The kind of the class `module.name`, `None` for a class that the
    /// mirror does not serve.
    pub fn of(module: &str, name: &str) -> Option<Kind> {
        [
            (FIELD_INDEX, Kind::Field),
            (KEYWORD_INDEX, Kind::Keyword),
            (DATE_INDEX, Kind::Date),
            (DATE_RANGE_INDEX, Kind::DateRange),
            (BOOLEAN_INDEX, Kind::Boolean),
            (UUID_INDEX, Kind::Uuid),
            (EXTENDED_PATH_INDEX, Kind::Path),
            (SECURITY_INDEX, Kind::Security),
        ]
        .into_iter()
        .find(|((m, n), _)| *m == module && *n == name)
        .map(|(_, kind)| kind)
    }

    /// The tree of the rows of an index of this kind.
    pub fn tree(self) -> &'static str {
        match self {
            Kind::Security => "_inputs",
            _ => "_unindex",
        }
    }

    /// Tells if the mirror serves a sort on an index of this kind.
    pub fn sorts(self) -> bool {
        matches!(self, Kind::Field | Kind::Date | Kind::Uuid)
    }
}

/// The rows of a base. A row is one entry of the catalog, one rid of
/// `paths`.
///
/// The build numbers the rows in ascending order of their rids, as signed
/// 32-bit ints: the order of ZCatalog's `IISet`. A rebase keeps the number
/// of each row, so the structures that it does not build again stay
/// valid. It gives each new rid a row after the others, and a removed rid
/// keeps its row, out of `live`.
#[derive(Clone, Debug, Default)]
pub struct Rows {
    /// The rid of each row.
    rids: Vec<i32>,
    /// The first rows, in ascending order of their rids.
    sorted: usize,
    /// The rows after `sorted`, by rid.
    later: Vec<(i32, u32)>,
    /// The rows that are entries of the catalog.
    pub live: RoaringBitmap,
    /// The path of each row. A removed row keeps its last path.
    pub paths: StrArena,
}

impl Rows {
    /// The rows of the rids and paths of `pairs`, in ascending rid order.
    pub fn from_sorted(pairs: &[(i32, String)]) -> Rows {
        let mut paths = StrArena::default();
        for (_, path) in pairs {
            paths.push(path);
        }
        paths.shrink();
        Rows {
            rids: pairs.iter().map(|(rid, _)| *rid).collect(),
            sorted: pairs.len(),
            later: Vec::new(),
            live: (0..pairs.len() as u32).collect(),
            paths,
        }
    }

    /// The row of `rid`, live or not.
    pub fn row(&self, rid: i32) -> Option<u32> {
        if let Ok(row) = self.rids[..self.sorted].binary_search(&rid) {
            return Some(row as u32);
        }
        self.later
            .binary_search_by_key(&rid, |(r, _)| *r)
            .ok()
            .map(|i| self.later[i].1)
    }

    /// The rid of `row`.
    pub fn rid(&self, row: u32) -> i32 {
        self.rids[row as usize]
    }

    /// The number of rows, the removed ones included.
    pub fn len(&self) -> usize {
        self.rids.len()
    }

    /// Tells if the base has no row.
    pub fn is_empty(&self) -> bool {
        self.rids.is_empty()
    }

    /// Tells if the order of the rows is the order of their rids.
    pub fn in_rid_order(&self) -> bool {
        self.later.is_empty()
    }

    /// The rids from `low` to `high`, both included, with their rows, live
    /// or not.
    pub fn between(&self, low: i32, high: i32) -> Vec<(i32, u32)> {
        let sorted = &self.rids[..self.sorted];
        let start = sorted.partition_point(|r| *r < low);
        let end = sorted.partition_point(|r| *r <= high);
        let mut out: Vec<(i32, u32)> = (start..end).map(|row| (sorted[row], row as u32)).collect();
        let start = self.later.partition_point(|(r, _)| *r < low);
        let end = self.later.partition_point(|(r, _)| *r <= high);
        out.extend_from_slice(&self.later[start..end]);
        out
    }

    /// The rows of a new base: these rows, with the paths of `changed`. A
    /// rid of `changed` without a row gets a new row; a rid whose path is
    /// `None` leaves `live`.
    pub fn with_changes(&self, changed: &BTreeMap<i32, Option<Val>>) -> Rows {
        let mut rows = self.clone();
        let mut paths: Vec<String> = (0..self.len() as u32)
            .map(|row| self.paths.get(row).to_owned())
            .collect();
        for (rid, path) in changed {
            // The rows of `self`: `rows.later` is in rid order only after
            // the sort below.
            let row = match self.row(*rid) {
                Some(row) => row,
                None => {
                    let row = rows.rids.len() as u32;
                    rows.rids.push(*rid);
                    rows.later.push((*rid, row));
                    paths.push(String::new());
                    row
                }
            };
            match path.as_ref().and_then(Val::text) {
                Some(text) => {
                    rows.live.insert(row);
                    paths[row as usize] = text.to_owned();
                }
                None => {
                    rows.live.remove(row);
                }
            }
        }
        rows.later.sort_unstable();
        let mut arena = StrArena::default();
        for path in &paths {
            arena.push(path);
        }
        arena.shrink();
        rows.paths = arena;
        rows
    }

    /// The bytes that the rows take.
    pub fn heap_bytes(&self) -> usize {
        self.rids.capacity() * 4
            + self.later.capacity() * 8
            + self.live.serialized_size()
            + self.paths.heap_bytes()
    }
}

/// The distinct keys of an index, in ascending order.
#[derive(Debug)]
pub enum Keys {
    /// Ints and bools.
    Int(Vec<i64>),
    /// Strs and `Message`s, by their text.
    Str(StrArena),
    /// Other values.
    Any(Vec<Val>),
}

impl Keys {
    /// The number of keys.
    pub fn len(&self) -> usize {
        match self {
            Keys::Int(v) => v.len(),
            Keys::Str(a) => a.len(),
            Keys::Any(v) => v.len(),
        }
    }

    /// Tells if there is no key.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Compares the key at `slot` with `value`. `None` when Python raises
    /// or when the model does not know.
    pub fn cmp_at(&self, slot: u32, value: &Val) -> Option<Ordering> {
        match self {
            Keys::Int(v) => py_cmp(&Val::Int(v[slot as usize]), value),
            Keys::Str(a) => Some(a.get(slot).cmp(value.text()?)),
            Keys::Any(v) => py_cmp(&v[slot as usize], value),
        }
    }

    /// Finds `value`: `Ok(slot)` when a key equals it, `Err(place)` where
    /// it would go, `None` when a comparison has no answer.
    pub fn search(&self, value: &Val) -> Option<Result<u32, u32>> {
        let (mut low, mut high) = (0u32, self.len() as u32);
        while low < high {
            let mid = low + (high - low) / 2;
            match self.cmp_at(mid, value)? {
                Ordering::Less => low = mid + 1,
                Ordering::Greater => high = mid,
                Ordering::Equal => return Some(Ok(mid)),
            }
        }
        Some(Err(low))
    }

    /// The key at `slot`, as a value.
    pub fn get(&self, slot: u32) -> Val {
        match self {
            Keys::Int(v) => Val::Int(v[slot as usize]),
            Keys::Str(a) => Val::Str(a.get(slot).into()),
            Keys::Any(v) => v[slot as usize].clone(),
        }
    }

    /// Adds the key at `slot` to `pairs`, for the row `row`.
    fn push_to(&self, slot: u32, row: u32, pairs: &mut Pairs) {
        match self {
            Keys::Int(v) => pairs.push(Val::Int(v[slot as usize]), row),
            Keys::Str(a) => pairs.push_str(a.get(slot), row),
            Keys::Any(v) => pairs.push(v[slot as usize].clone(), row),
        }
    }

    /// The bytes that the keys take.
    pub fn heap_bytes(&self) -> usize {
        match self {
            Keys::Int(v) => v.capacity() * 8,
            Keys::Str(a) => a.heap_bytes(),
            Keys::Any(v) => {
                v.capacity() * std::mem::size_of::<Val>()
                    + v.iter().map(Val::heap_bytes).sum::<usize>()
            }
        }
    }
}

/// The rows of each key: one row inline, or a bitmap.
#[derive(Debug, Default)]
pub struct Postings {
    /// For each key, its one row, or `MANY` and the place of its bitmap.
    heads: Vec<u32>,
    /// The bitmaps of the keys of more than one row.
    bitmaps: Vec<RoaringBitmap>,
}

/// The rows of one key.
pub enum Posting<'a> {
    /// One row.
    One(u32),
    /// A bitmap of rows.
    Many(&'a RoaringBitmap),
}

impl Posting<'_> {
    /// The rows as a bitmap.
    pub fn to_bitmap(&self) -> RoaringBitmap {
        match self {
            Posting::One(row) => std::iter::once(*row).collect(),
            Posting::Many(b) => (*b).clone(),
        }
    }

    /// Adds the rows to `out`.
    pub fn add_to(&self, out: &mut RoaringBitmap) {
        match self {
            Posting::One(row) => {
                out.insert(*row);
            }
            Posting::Many(b) => *out |= *b,
        }
    }
}

impl Postings {
    /// Appends the posting of the next key: its rows, in ascending order.
    fn push(&mut self, rows: &[u32]) {
        if let [row] = rows {
            self.heads.push(*row);
        } else {
            self.heads.push(MANY | self.bitmaps.len() as u32);
            self.bitmaps.push(rows.iter().copied().collect());
        }
    }

    /// The rows of the key at `slot`.
    pub fn get(&self, slot: u32) -> Posting<'_> {
        let head = self.heads[slot as usize];
        if head & MANY != 0 {
            Posting::Many(&self.bitmaps[(head & !MANY) as usize])
        } else {
            Posting::One(head)
        }
    }

    /// Releases the spare capacity.
    fn shrink(&mut self) {
        self.heads.shrink_to_fit();
        self.bitmaps.shrink_to_fit();
    }

    /// The bitmaps of the keys of more than one row.
    pub fn many(&self) -> &[RoaringBitmap] {
        &self.bitmaps
    }

    /// The bytes that the postings take.
    pub fn heap_bytes(&self) -> usize {
        self.heads.capacity() * 4
            + self
                .bitmaps
                .iter()
                .map(|b| b.serialized_size() + 48)
                .sum::<usize>()
    }
}

/// Sorts the keys of `pairs` and groups their rows: the distinct keys in
/// ascending order and the rows of each. `each` gets the row and the slot
/// of the key of each pair.
fn group(pairs: Pairs, mut each: impl FnMut(u32, u32)) -> Result<(Keys, Postings), String> {
    let mut postings = Postings::default();
    let mut group: Vec<u32> = Vec::new();
    let flush = |group: &mut Vec<u32>, postings: &mut Postings| {
        if !group.is_empty() {
            group.sort_unstable();
            group.dedup();
            postings.push(group);
            group.clear();
        }
    };
    let keys = match pairs {
        Pairs::Empty => Keys::Int(Vec::new()),
        Pairs::Int(mut v) => {
            v.sort_unstable();
            let mut keys = Vec::new();
            for (key, row) in v {
                if keys.last() != Some(&key) {
                    flush(&mut group, &mut postings);
                    keys.push(key);
                }
                each(row, (keys.len() - 1) as u32);
                group.push(row);
            }
            keys.shrink_to_fit();
            Keys::Int(keys)
        }
        Pairs::Str(arena, mut v) => {
            // Only the distinct texts sort as strs. Each pair takes the
            // number of its text, then the slot of that text, and the pairs
            // sort as numbers.
            let mut ids: HashMap<&str, u32> = HashMap::new();
            let mut texts: Vec<&str> = Vec::new();
            for pair in &mut v {
                let text = arena.get(pair.0);
                let next = texts.len() as u32;
                let id = *ids.entry(text).or_insert(next);
                if id == next {
                    texts.push(text);
                }
                pair.0 = id;
            }
            drop(ids);
            let mut order: Vec<u32> = (0..texts.len() as u32).collect();
            order.sort_unstable_by(|a, b| texts[*a as usize].cmp(texts[*b as usize]));
            let mut slots = vec![0u32; texts.len()];
            let mut keys = StrArena::default();
            for (slot, id) in order.iter().enumerate() {
                slots[*id as usize] = slot as u32;
                keys.push(texts[*id as usize]);
            }
            for pair in &mut v {
                pair.0 = slots[pair.0 as usize];
            }
            v.sort_unstable();
            let mut last: Option<u32> = None;
            for (slot, row) in v {
                if last != Some(slot) {
                    flush(&mut group, &mut postings);
                    last = Some(slot);
                }
                each(row, slot);
                group.push(row);
            }
            keys.shrink();
            Keys::Str(keys)
        }
        Pairs::Any(mut v) => {
            let unordered = || Err("keys that the mirror does not order".into());
            if v.len() <= ORDERED_CHECK && !ordered(v.iter().map(|(k, _)| k)) {
                return unordered();
            }
            let mut failed = false;
            v.sort_by(|a, b| {
                py_cmp(&a.0, &b.0)
                    .unwrap_or_else(|| {
                        failed = true;
                        Ordering::Equal
                    })
                    .then(a.1.cmp(&b.1))
            });
            if failed {
                return unordered();
            }
            let mut keys: Vec<Val> = Vec::new();
            for (key, row) in v {
                if keys
                    .last()
                    .is_none_or(|l| py_cmp(l, &key) != Some(Ordering::Equal))
                {
                    flush(&mut group, &mut postings);
                    keys.push(key);
                }
                each(row, (keys.len() - 1) as u32);
                group.push(row);
            }
            keys.shrink_to_fit();
            Keys::Any(keys)
        }
    };
    flush(&mut group, &mut postings);
    postings.heads.shrink_to_fit();
    postings.bitmaps.shrink_to_fit();
    Ok((keys, postings))
}

/// The most keys of several kinds whose pairs `ordered` compares.
const ORDERED_CHECK: usize = 2048;

/// Tells if Python orders each pair of `values`: a sort of them then has
/// one answer. Past `ORDERED_CHECK` values of several kinds, it does not
/// look and says no.
fn ordered<'a>(values: impl Iterator<Item = &'a Val> + Clone) -> bool {
    let mut first = values.clone();
    let Some(head) = first.next() else {
        return true;
    };
    // Values of one kind of atom order with each other.
    let atom = |v: &Val| match v {
        Val::Bool(_) | Val::Int(_) => Some(0),
        Val::Float(f) if !f.is_nan() => Some(0),
        Val::Str(_) | Val::Message(_) => Some(1),
        Val::Bytes(_) => Some(2),
        Val::DateTime(..) => Some(3),
        _ => None,
    };
    if let Some(kind) = atom(head)
        && first.all(|v| atom(v) == Some(kind))
    {
        return true;
    }
    let all: Vec<&Val> = values.collect();
    if all.len() > ORDERED_CHECK {
        return false;
    }
    all.iter()
        .enumerate()
        .all(|(i, a)| all[i + 1..].iter().all(|b| py_cmp(a, b).is_some()))
}

/// A FieldIndex: one key per row. A DateIndex, a BooleanIndex and a
/// UUIDIndex keep this structure too.
#[derive(Debug)]
pub struct FieldIndex {
    /// The distinct keys.
    pub keys: Keys,
    /// The rows of each key.
    pub postings: Postings,
    /// The slot of the key of each row, or `NONE`. A row past its end has
    /// no key.
    reverse: Vec<u32>,
    /// The rows with a key.
    pub indexed: RoaringBitmap,
}

impl FieldIndex {
    /// The slot of the key of `row`.
    pub fn slot(&self, row: u32) -> Option<u32> {
        self.reverse
            .get(row as usize)
            .copied()
            .filter(|s| *s != NONE)
    }

    /// The bytes that the index takes.
    pub fn heap_bytes(&self) -> usize {
        self.keys.heap_bytes()
            + self.postings.heap_bytes()
            + self.reverse.capacity() * 4
            + self.indexed.serialized_size()
    }

    /// Builds the index from the keys of `pairs`, for `rows` rows.
    pub fn build(pairs: Pairs, rows: usize) -> Result<FieldIndex, String> {
        let mut reverse = vec![NONE; rows];
        let (keys, postings) = group(pairs, |row, slot| reverse[row as usize] = slot)?;
        let indexed = reverse
            .iter()
            .enumerate()
            .filter(|(_, s)| **s != NONE)
            .map(|(row, _)| row as u32)
            .collect();
        Ok(FieldIndex {
            keys,
            postings,
            reverse,
            indexed,
        })
    }

    /// Builds the index of a new base: the keys of this index, with the
    /// keys of `changed` over them, for `rows`.
    pub fn rebuilt(
        &self,
        changed: &BTreeMap<i32, Option<Val>>,
        rows: &Rows,
    ) -> Result<FieldIndex, String> {
        let mut pairs = Pairs::Empty;
        let touched: RoaringBitmap = changed.keys().filter_map(|rid| rows.row(*rid)).collect();
        for row in self.indexed.iter() {
            if !touched.contains(row)
                && let Some(slot) = self.slot(row)
            {
                self.keys.push_to(slot, row, &mut pairs);
            }
        }
        for (rid, key) in changed {
            if let (Some(key), Some(row)) = (key, rows.row(*rid)) {
                pairs.push(key.clone(), row);
            }
        }
        FieldIndex::build(pairs, rows.len())
    }
}

/// A KeywordIndex: the keys of each row.
#[derive(Debug)]
pub struct KeywordIndex {
    /// The distinct keys.
    pub keys: Keys,
    /// The rows of each key.
    pub postings: Postings,
    /// Where the slots of each row start in `slots`: the slots of `row`
    /// are `slots[starts[row]..starts[row + 1]]`.
    starts: Vec<u32>,
    /// The slots of the keys of each row, in ascending order.
    slots: Vec<u32>,
    /// The rows with a key.
    pub indexed: RoaringBitmap,
}

impl KeywordIndex {
    /// The slots of the keys of `row`, in ascending order.
    pub fn row_slots(&self, row: u32) -> &[u32] {
        let row = row as usize;
        if row + 1 >= self.starts.len() {
            return &[];
        }
        &self.slots[self.starts[row] as usize..self.starts[row + 1] as usize]
    }

    /// The keys of `row`, as the mirror keeps the value of a row: a list
    /// in ascending order.
    pub fn value(&self, row: u32) -> Option<Val> {
        let slots = self.row_slots(row);
        (!slots.is_empty()).then(|| Val::List(slots.iter().map(|s| self.keys.get(*s)).collect()))
    }

    /// The bytes that the index takes.
    pub fn heap_bytes(&self) -> usize {
        self.keys.heap_bytes()
            + self.postings.heap_bytes()
            + self.starts.capacity() * 4
            + self.slots.capacity() * 4
            + self.indexed.serialized_size()
    }

    /// The transient bytes of a build from `pairs`, beside the pairs: the
    /// row and the slot of each pair.
    pub fn build_bytes(pairs: &Pairs) -> usize {
        pairs.len() * 8
    }

    /// Builds the index from the keys of `pairs`, one pair for each key of
    /// a row, for `rows` rows.
    pub fn build(pairs: Pairs, rows: usize) -> Result<KeywordIndex, String> {
        let mut found: Vec<(u32, u32)> = Vec::new();
        let (keys, postings) = group(pairs, |row, slot| {
            if (row as usize) < rows {
                found.push((row, slot));
            }
        })?;
        // `group` gives the slots in ascending order. So a count of the
        // slots of each row and one pass over `found` lay out the slots of
        // each row in ascending order, and a repeated slot comes next to
        // its twin.
        let mut ends = vec![0u32; rows + 1];
        for (row, _) in &found {
            ends[*row as usize + 1] += 1;
        }
        for row in 0..rows {
            ends[row + 1] += ends[row];
        }
        let mut next = ends.clone();
        let mut slots = vec![0u32; found.len()];
        for (row, slot) in found {
            let at = &mut next[row as usize];
            slots[*at as usize] = slot;
            *at += 1;
        }
        drop(next);
        // Remove each repeated slot of a row, in place.
        let mut starts = Vec::with_capacity(rows + 1);
        let mut indexed = RoaringBitmap::new();
        let mut kept = 0usize;
        for row in 0..rows {
            starts.push(kept as u32);
            let first = kept;
            for i in ends[row] as usize..ends[row + 1] as usize {
                let slot = slots[i];
                if kept == first || slots[kept - 1] != slot {
                    slots[kept] = slot;
                    kept += 1;
                }
            }
            if kept > first {
                indexed.insert(row as u32);
            }
        }
        starts.push(kept as u32);
        slots.truncate(kept);
        slots.shrink_to_fit();
        Ok(KeywordIndex {
            keys,
            postings,
            starts,
            slots,
            indexed,
        })
    }

    /// Builds the index of a new base: the keys of this index, with the
    /// values of `changed` over them, for `rows`.
    pub fn rebuilt(
        &self,
        changed: &BTreeMap<i32, Option<Val>>,
        rows: &Rows,
    ) -> Result<KeywordIndex, String> {
        let mut pairs = Pairs::Empty;
        let touched: RoaringBitmap = changed.keys().filter_map(|rid| rows.row(*rid)).collect();
        for row in self.indexed.iter() {
            if !touched.contains(row) {
                for slot in self.row_slots(row) {
                    self.keys.push_to(*slot, row, &mut pairs);
                }
            }
        }
        for (rid, value) in changed {
            if let (Some(value), Some(row)) = (value, rows.row(*rid)) {
                for key in keywords_of(value) {
                    pairs.push(key.clone(), row);
                }
            }
        }
        KeywordIndex::build(pairs, rows.len())
    }
}

/// The keys of a value of a KeywordIndex: the items of a list or of a
/// tuple.
pub fn keywords_of(value: &Val) -> &[Val] {
    match value {
        Val::List(items) | Val::Tuple(items) => items,
        _ => &[],
    }
}

/// The value of a row of a KeywordIndex as the mirror keeps it: its
/// distinct keys in ascending order, as a list. `None` for no key.
pub fn keywords(value: Val) -> Result<Option<Val>, String> {
    let mut items = match value {
        Val::List(items) | Val::Tuple(items) => items.into_vec(),
        other => {
            return Err(format!(
                "a value of a KeywordIndex that is not a list: {other:?}"
            ));
        }
    };
    if !ordered(items.iter()) {
        return Err("keys of a KeywordIndex that the mirror does not order".into());
    }
    items.sort_by(|a, b| py_cmp(a, b).unwrap_or(Ordering::Equal));
    items.dedup_by(|a, b| py_cmp(a, b) == Some(Ordering::Equal));
    Ok((!items.is_empty()).then(|| Val::List(items.into())))
}

/// The bounds of a value of a DateRangeIndex: `since` and `until`, with
/// `i64::MIN` and `i64::MAX` for `None`. `None` for a value of another
/// form.
pub fn date_range_of(value: &Val) -> Option<(i64, i64)> {
    let bound = |v: &Val, open: i64| match v {
        Val::None => Some(open),
        Val::Int(i) => Some(*i),
        _ => None,
    };
    match value {
        Val::Tuple(items) => match &items[..] {
            [since, until] => Some((bound(since, i64::MIN)?, bound(until, i64::MAX)?)),
            _ => None,
        },
        _ => None,
    }
}

/// Tells if the range `(since, until)` holds the date `t`, as
/// `DateRangeIndex.query_index` finds it.
pub fn covers((since, until): (i64, i64), t: i64) -> bool {
    since <= t && t <= until
}

/// A DateRangeIndex: `since` and `until` of each row, and the rows sorted
/// by each.
#[derive(Debug)]
pub struct DateRangeIndex {
    /// `since` of each row, `i64::MIN` for `None`.
    since: Vec<i64>,
    /// `until` of each row, `i64::MAX` for `None`.
    until: Vec<i64>,
    /// The rows with an entry, by `since`.
    by_since: Vec<(i64, u32)>,
    /// The rows with an entry, by `until`.
    by_until: Vec<(i64, u32)>,
    /// The rows with an entry.
    pub indexed: RoaringBitmap,
}

impl DateRangeIndex {
    /// The bounds of `row`, or `None` when it has no entry.
    pub fn entry(&self, row: u32) -> Option<(i64, i64)> {
        self.indexed
            .contains(row)
            .then(|| (self.since[row as usize], self.until[row as usize]))
    }

    /// The value of `row` as `_unindex` holds it: `(since, until)`.
    pub fn value(&self, row: u32) -> Option<Val> {
        let (since, until) = self.entry(row)?;
        let bound = |v: i64, open: i64| if v == open { Val::None } else { Val::Int(v) };
        Some(Val::Tuple(
            vec![bound(since, i64::MIN), bound(until, i64::MAX)].into(),
        ))
    }

    /// The rows whose range holds the date `t`.
    pub fn matched(&self, t: i64) -> RoaringBitmap {
        let started = self.by_since.partition_point(|(s, _)| *s <= t);
        let ended = self.by_until.partition_point(|(u, _)| *u < t);
        let later = self.by_since.len() - started;
        if later + ended < started {
            // Few rows start after `t` or end before it: take them away.
            let mut out = self.indexed.clone();
            out -= self.by_since[started..]
                .iter()
                .map(|(_, row)| *row)
                .collect::<RoaringBitmap>();
            out -= self.by_until[..ended]
                .iter()
                .map(|(_, row)| *row)
                .collect::<RoaringBitmap>();
            out
        } else {
            self.by_since[..started]
                .iter()
                .filter(|(_, row)| self.until[*row as usize] >= t)
                .map(|(_, row)| *row)
                .collect()
        }
    }

    /// The bytes that the index takes.
    pub fn heap_bytes(&self) -> usize {
        self.since.capacity() * 8
            + self.until.capacity() * 8
            + (self.by_since.capacity() + self.by_until.capacity()) * 16
            + self.indexed.serialized_size()
    }

    /// Builds the index from the bounds of each row with an entry, for
    /// `rows` rows.
    pub fn build(items: Vec<(u32, i64, i64)>, rows: usize) -> DateRangeIndex {
        let mut since = vec![i64::MIN; rows];
        let mut until = vec![i64::MAX; rows];
        let mut by_since = Vec::with_capacity(items.len());
        let mut by_until = Vec::with_capacity(items.len());
        let mut indexed = RoaringBitmap::new();
        for (row, s, u) in items {
            if indexed.insert(row) {
                since[row as usize] = s;
                until[row as usize] = u;
                by_since.push((s, row));
                by_until.push((u, row));
            }
        }
        by_since.sort_unstable();
        by_until.sort_unstable();
        DateRangeIndex {
            since,
            until,
            by_since,
            by_until,
            indexed,
        }
    }

    /// Builds the index of a new base: the entries of this index, with the
    /// values of `changed` over them, for `rows`.
    pub fn rebuilt(&self, changed: &BTreeMap<i32, Option<Val>>, rows: &Rows) -> DateRangeIndex {
        let touched: RoaringBitmap = changed.keys().filter_map(|rid| rows.row(*rid)).collect();
        let mut items: Vec<(u32, i64, i64)> = self
            .indexed
            .iter()
            .filter(|row| !touched.contains(*row))
            .map(|row| (row, self.since[row as usize], self.until[row as usize]))
            .collect();
        for (rid, value) in changed {
            if let (Some(value), Some(row)) = (value, rows.row(*rid))
                && let Some((s, u)) = date_range_of(value)
            {
                items.push((row, s, u));
            }
        }
        DateRangeIndex::build(items, rows.len())
    }
}

/// The components of a path, as ExtendedPathIndex splits it: the parts
/// between the slashes that are not empty.
pub fn components(path: &str) -> Vec<&str> {
    path.split('/').filter(|part| !part.is_empty()).collect()
}

/// The parent path of a path of `comps`, as `_index_parents` keys it: `/`
/// and the components but the last, joined by slashes.
pub fn parent_of(comps: &[&str]) -> String {
    format!("/{}", comps[..comps.len().saturating_sub(1)].join("/"))
}

/// An ExtendedPathIndex: the structures that its `_index` and its
/// `_index_parents` hold, made from `_unindex`. These are not a trie.
///
/// A path of `n` components has the component `i` at the level `i`, and
/// ends at the level `n - 1`; a path without a component ends at the level
/// -1. The mirror does not keep `_index_items`: a query that reads it takes
/// the Python path.
#[derive(Debug)]
pub struct PathIndex {
    /// The path of each row, as `_unindex` holds it: its place in `texts`,
    /// or `NONE`. A row past its end has no path.
    places: Vec<u32>,
    /// The paths of the rows.
    texts: StrArena,
    /// The distinct components, in ascending order.
    comps: StrArena,
    /// The pairs (component, level) of the rows, in ascending order.
    pairs: Vec<(u32, i32)>,
    /// The rows of each pair.
    pair_rows: Postings,
    /// The rows that end at each level, from the level -1.
    ends: Vec<RoaringBitmap>,
    /// The distinct parent paths, in ascending order.
    parents: StrArena,
    /// The rows of each parent path.
    parent_rows: Postings,
    /// The rows with a path.
    pub indexed: RoaringBitmap,
    /// `_depth` of the index record: the deepest level that the index met
    /// since its last `clear`. It does not go down when rows leave.
    pub depth: i64,
    /// `multi_valued` of the index record.
    pub multi_valued: bool,
}

impl PathIndex {
    /// The path of `row`.
    pub fn path(&self, row: u32) -> Option<&str> {
        self.places
            .get(row as usize)
            .filter(|p| **p != NONE)
            .map(|p| self.texts.get(*p))
    }

    /// The rows with `comp` at `level`.
    pub fn comp_rows(&self, comp: &str, level: i64) -> Option<Posting<'_>> {
        let slot = self.comps.search(comp).ok()?;
        let level = i32::try_from(level).ok()?;
        let at = self.pairs.binary_search(&(slot, level)).ok()?;
        Some(self.pair_rows.get(at as u32))
    }

    /// The rows that end at `level`.
    pub fn ends_at(&self, level: i64) -> Option<&RoaringBitmap> {
        let at = usize::try_from(level.checked_add(1)?).ok()?;
        self.ends.get(at).filter(|rows| !rows.is_empty())
    }

    /// The deepest level at which a row ends.
    pub fn deepest(&self) -> i64 {
        self.ends.len() as i64 - 2
    }

    /// The rows whose parent path is `path`.
    pub fn parent_rows(&self, path: &str) -> Option<Posting<'_>> {
        let slot = self.parents.search(path).ok()?;
        Some(self.parent_rows.get(slot))
    }

    /// The bytes that the index takes.
    pub fn heap_bytes(&self) -> usize {
        self.places.capacity() * 4
            + self.texts.heap_bytes()
            + self.comps.heap_bytes()
            + self.pairs.capacity() * 8
            + self.pair_rows.heap_bytes()
            + self
                .ends
                .iter()
                .map(|b| b.serialized_size() + 48)
                .sum::<usize>()
            + self.parents.heap_bytes()
            + self.parent_rows.heap_bytes()
            + self.indexed.serialized_size()
    }

    /// The transient bytes of a build from the paths of `items`, beside
    /// the paths: the components of each row and their parent paths.
    pub fn build_bytes(texts: &StrArena, items: &[(u32, u32)]) -> usize {
        items.len() * 64 + texts.heap_bytes()
    }

    /// Builds the index from the path of each row of `items`, a place in
    /// `texts`, for `rows` rows, with the `_depth` and the `multi_valued`
    /// of the index record.
    pub fn build(
        mut texts: StrArena,
        items: Vec<(u32, u32)>,
        rows: usize,
        depth: i64,
        multi_valued: bool,
    ) -> PathIndex {
        texts.shrink();
        let mut places = vec![NONE; rows];
        let mut indexed = RoaringBitmap::new();
        for (row, place) in items {
            places[row as usize] = place;
            indexed.insert(row);
        }
        let mut ids: HashMap<&str, u32> = HashMap::new();
        let mut names: Vec<&str> = Vec::new();
        let mut found: Vec<(u32, i32, u32)> = Vec::new();
        let mut ends: Vec<Vec<u32>> = Vec::new();
        let mut parents: HashMap<String, Vec<u32>> = HashMap::new();
        for row in indexed.iter() {
            let comps = components(texts.get(places[row as usize]));
            for (level, comp) in comps.iter().enumerate() {
                let id = *ids.entry(comp).or_insert_with(|| {
                    names.push(comp);
                    (names.len() - 1) as u32
                });
                found.push((id, level as i32, row));
            }
            // `ends[0]` holds the level -1.
            let end = comps.len();
            if ends.len() <= end {
                ends.resize_with(end + 1, Vec::new);
            }
            ends[end].push(row);
            parents.entry(parent_of(&comps)).or_default().push(row);
        }
        // The slot of each component, in ascending order of the components.
        let mut order: Vec<u32> = (0..names.len() as u32).collect();
        order.sort_unstable_by_key(|id| names[*id as usize]);
        let mut slot_of = vec![0u32; names.len()];
        let mut comps = StrArena::default();
        for (slot, id) in order.iter().enumerate() {
            slot_of[*id as usize] = slot as u32;
            comps.push(names[*id as usize]);
        }
        comps.shrink();
        for item in &mut found {
            item.0 = slot_of[item.0 as usize];
        }
        found.sort_unstable();
        let mut pairs = Vec::new();
        let mut pair_rows = Postings::default();
        let mut group: Vec<u32> = Vec::new();
        for (k, (slot, level, row)) in found.iter().enumerate() {
            group.push(*row);
            let last = found
                .get(k + 1)
                .is_none_or(|next| (next.0, next.1) != (*slot, *level));
            if last {
                pairs.push((*slot, *level));
                pair_rows.push(&group);
                group.clear();
            }
        }
        pairs.shrink_to_fit();
        pair_rows.shrink();
        let ends = ends
            .into_iter()
            .map(|rows| rows.into_iter().collect())
            .collect();
        let mut keys: Vec<(String, Vec<u32>)> = parents.into_iter().collect();
        keys.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        let mut parent_names = StrArena::default();
        let mut parent_rows = Postings::default();
        for (path, rows) in &keys {
            parent_names.push(path);
            parent_rows.push(rows);
        }
        parent_names.shrink();
        parent_rows.shrink();
        PathIndex {
            places,
            texts,
            comps,
            pairs,
            pair_rows,
            ends,
            parents: parent_names,
            parent_rows,
            indexed,
            depth,
            multi_valued,
        }
    }

    /// Builds the index of a new base: the paths of this index, with the
    /// paths of `changed` over them, for `rows`.
    pub fn rebuilt(&self, changed: &BTreeMap<i32, Option<Val>>, rows: &Rows) -> PathIndex {
        let touched: RoaringBitmap = changed.keys().filter_map(|rid| rows.row(*rid)).collect();
        let mut texts = StrArena::default();
        let mut items = Vec::with_capacity(self.indexed.len() as usize);
        for row in self.indexed.iter() {
            if !touched.contains(row)
                && let Some(path) = self.path(row)
            {
                items.push((row, texts.push(path)));
            }
        }
        for (rid, value) in changed {
            if let (Some(Val::Str(path)), Some(row)) = (value, rows.row(*rid)) {
                items.push((row, texts.push(path)));
            }
        }
        PathIndex::build(texts, items, rows.len(), self.depth, self.multi_valued)
    }
}

/// The keys of an index as a build collects them, with their rows. A str
/// goes into an arena, so the build keeps no allocation for each key; the
/// arena is freed whole when the index is built.
pub enum Pairs {
    /// No key yet.
    Empty,
    /// Ints and bools.
    Int(Vec<(i64, u32)>),
    /// Strs and `Message`s, by their text: the place in the arena.
    Str(StrArena, Vec<(u32, u32)>),
    /// Other values, or keys of several kinds.
    Any(Vec<(Val, u32)>),
}

impl Pairs {
    /// Adds `key` for `row`.
    pub fn push(&mut self, key: Val, row: u32) {
        if let Some(i) = key.int() {
            match self {
                Pairs::Empty => return *self = Pairs::Int(vec![(i, row)]),
                Pairs::Int(v) => return v.push((i, row)),
                _ => {}
            }
        } else if let Some(text) = key.text()
            && matches!(self, Pairs::Empty | Pairs::Str(..))
        {
            return self.push_str(text, row);
        }
        match self {
            Pairs::Any(v) => v.push((key, row)),
            Pairs::Empty => *self = Pairs::Any(vec![(key, row)]),
            _ => {
                self.widen();
                self.push(key, row);
            }
        }
    }

    /// Adds the str `text` for `row`.
    pub fn push_str(&mut self, text: &str, row: u32) {
        match self {
            Pairs::Empty => {
                let mut arena = StrArena::default();
                let place = arena.push(text);
                *self = Pairs::Str(arena, vec![(place, row)]);
            }
            Pairs::Str(arena, v) => {
                let place = arena.push(text);
                v.push((place, row));
            }
            Pairs::Any(v) => v.push((Val::Str(text.into()), row)),
            Pairs::Int(_) => {
                self.widen();
                self.push_str(text, row);
            }
        }
    }

    /// Turns the pairs into `Any`.
    fn widen(&mut self) {
        let any = match std::mem::replace(self, Pairs::Empty) {
            Pairs::Empty => Vec::new(),
            Pairs::Int(v) => v.into_iter().map(|(i, r)| (Val::Int(i), r)).collect(),
            Pairs::Str(arena, v) => v
                .into_iter()
                .map(|(p, r)| (Val::Str(arena.get(p).into()), r))
                .collect(),
            Pairs::Any(v) => v,
        };
        *self = Pairs::Any(any);
    }

    /// Adds the pairs of `other`.
    pub fn append(&mut self, other: Pairs) {
        match (&mut *self, other) {
            (_, Pairs::Empty) => {}
            (Pairs::Empty, other) => *self = other,
            (Pairs::Int(a), Pairs::Int(b)) => a.extend(b),
            (Pairs::Str(arena, a), Pairs::Str(other, b)) => {
                let offset = arena.len() as u32;
                arena.extend(&other);
                a.extend(b.into_iter().map(|(p, r)| (p + offset, r)));
            }
            (_, mut other) => {
                self.widen();
                other.widen();
                if let (Pairs::Any(a), Pairs::Any(b)) = (self, other) {
                    a.extend(b);
                }
            }
        }
    }

    /// The number of pairs.
    pub fn len(&self) -> usize {
        match self {
            Pairs::Empty => 0,
            Pairs::Int(v) => v.len(),
            Pairs::Str(_, v) => v.len(),
            Pairs::Any(v) => v.len(),
        }
    }

    /// Tells if there is no pair.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The bytes that the pairs take.
    pub fn heap_bytes(&self) -> usize {
        match self {
            Pairs::Empty => 0,
            Pairs::Int(v) => v.capacity() * 16,
            Pairs::Str(arena, v) => arena.heap_bytes() + v.capacity() * 8,
            Pairs::Any(v) => {
                v.capacity() * (std::mem::size_of::<Val>() + 8)
                    + v.iter().map(|(k, _)| k.heap_bytes()).sum::<usize>()
            }
        }
    }
}

/// A mirrored index, or why the index has no structure.
#[derive(Clone, Debug)]
pub enum Index {
    /// A FieldIndex, a DateIndex, a BooleanIndex or a UUIDIndex.
    Field(Arc<FieldIndex>),
    /// A KeywordIndex.
    Keyword(Arc<KeywordIndex>),
    /// A DateRangeIndex.
    DateRange(Arc<DateRangeIndex>),
    /// An ExtendedPathIndex.
    Path(Arc<PathIndex>),
    /// A `ZoeSecurityIndex`.
    Security(Arc<SecurityIndex>),
    /// No structure: the reason.
    Unavailable(String),
}

impl Index {
    /// Tells if `row` has a value in `_unindex`.
    pub fn has(&self, row: u32) -> bool {
        match self {
            Index::Field(f) => f.slot(row).is_some(),
            Index::Keyword(k) => k.indexed.contains(row),
            Index::DateRange(d) => d.indexed.contains(row),
            Index::Path(p) => p.indexed.contains(row),
            Index::Security(s) => s.indexed.contains(row),
            Index::Unavailable(_) => false,
        }
    }

    /// The value of `row`, in the form that the change sets keep.
    pub fn value(&self, row: u32) -> Option<Val> {
        match self {
            Index::Field(f) => f.slot(row).map(|s| f.keys.get(s)),
            Index::Keyword(k) => k.value(row),
            Index::DateRange(d) => d.value(row),
            Index::Path(p) => p.path(row).map(|path| Val::Str(path.into())),
            Index::Security(s) => s.value(row),
            Index::Unavailable(_) => None,
        }
    }

    /// The value of an item of `_unindex` in the form that the change sets
    /// keep: the keys of a KeywordIndex as a sorted list, `None` for no
    /// key. Refuses a value that the index does not model.
    pub fn normalize(&self, value: Val) -> Result<Option<Val>, String> {
        match self {
            Index::Keyword(_) => keywords(value),
            Index::DateRange(_) => match date_range_of(&value) {
                Some(_) => Ok(Some(value)),
                None => Err(format!("a value of a DateRangeIndex: {value:?}")),
            },
            Index::Path(_) => match value {
                Val::Str(_) => Ok(Some(value)),
                other => Err(format!("a path that is not a str: {other:?}")),
            },
            Index::Security(_) => crate::security::normalize(&value).map(Some),
            _ => Ok(Some(value)),
        }
    }

    /// The structure of a new base with the values of `changed` over this
    /// one, for `rows`. `None` for an index without a structure.
    pub fn rebuilt(
        &self,
        changed: &BTreeMap<i32, Option<Val>>,
        rows: &Rows,
    ) -> Option<Result<Index, String>> {
        Some(match self {
            Index::Field(f) => f.rebuilt(changed, rows).map(|i| Index::Field(Arc::new(i))),
            Index::Keyword(k) => k
                .rebuilt(changed, rows)
                .map(|i| Index::Keyword(Arc::new(i))),
            Index::DateRange(d) => Ok(Index::DateRange(Arc::new(d.rebuilt(changed, rows)))),
            Index::Path(p) => Ok(Index::Path(Arc::new(p.rebuilt(changed, rows)))),
            Index::Security(s) => s
                .rebuilt(Some(changed), rows)
                .map(|i| Index::Security(Arc::new(i))),
            Index::Unavailable(_) => return None,
        })
    }

    /// Tells if `self` and `other` are one structure, shared.
    pub fn shares(&self, other: &Index) -> bool {
        match (self, other) {
            (Index::Field(a), Index::Field(b)) => Arc::ptr_eq(a, b),
            (Index::Keyword(a), Index::Keyword(b)) => Arc::ptr_eq(a, b),
            (Index::DateRange(a), Index::DateRange(b)) => Arc::ptr_eq(a, b),
            (Index::Path(a), Index::Path(b)) => Arc::ptr_eq(a, b),
            (Index::Security(a), Index::Security(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }

    /// The bytes that the structure takes.
    pub fn heap_bytes(&self) -> usize {
        match self {
            Index::Field(f) => f.heap_bytes(),
            Index::Keyword(k) => k.heap_bytes(),
            Index::DateRange(d) => d.heap_bytes(),
            Index::Path(p) => p.heap_bytes(),
            Index::Security(s) => s.heap_bytes(),
            Index::Unavailable(r) => r.len(),
        }
    }
}

/// One index of a catalog in a base.
#[derive(Clone, Debug)]
pub struct IndexEntry {
    /// The id of the index.
    pub name: String,
    /// The class of the index, as `module.name`.
    pub class: String,
    /// The kind of the class, `None` for a class that the mirror does not
    /// serve.
    pub kind: Option<Kind>,
    /// The oid of the index object.
    pub oid: u64,
    /// The oid of its `_unindex`.
    pub tree: Option<u64>,
    /// The oid of its `_length`.
    pub length_oid: Option<u64>,
    /// The value of its `_length`: the number of distinct keys, which
    /// ZCatalog's choice of a sort path reads.
    pub length: i64,
    /// The structures.
    pub index: Index,
}

/// The oids of the trees of the catalog object.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CatalogTrees {
    /// `paths`.
    pub paths: u64,
    /// `uids`.
    pub uids: u64,
    /// `data`.
    pub data: u64,
}

/// The base of a catalog: its rows and its indexes at one tid.
#[derive(Debug)]
pub struct Base {
    /// The tid of the base: it holds each commit up to this tid.
    pub tid: u64,
    /// The oid of the catalog.
    pub catalog: u64,
    /// The rows.
    pub rows: Arc<Rows>,
    /// The indexes, in the order of their names.
    pub indexes: Vec<IndexEntry>,
    /// The place of each index, by name.
    pub by_name: HashMap<String, usize>,
    /// The trees of the catalog object.
    pub trees: CatalogTrees,
    /// The metadata table.
    pub table: Table,
}

impl Base {
    /// The bytes that the base takes. A structure that another base shares
    /// counts in each.
    pub fn heap_bytes(&self) -> usize {
        self.rows.heap_bytes()
            + self.table.heap_bytes()
            + self
                .indexes
                .iter()
                .map(|e| e.index.heap_bytes() + e.name.len() + e.class.len() + 96)
                .sum::<usize>()
    }
}

/// The rid of a key of an IO tree.
pub fn rid_of(value: &Val) -> Result<i32, String> {
    value
        .int()
        .and_then(|i| i32::try_from(i).ok())
        .ok_or_else(|| format!("a rid {value:?}"))
}
