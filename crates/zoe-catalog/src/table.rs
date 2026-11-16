//! The metadata table of a base: the records of `data` of the catalog, one
//! column for each position of the `schema` of the catalog (ADR 0059,
//! section 3).
//!
//! A column keeps the encoded distinct cells (`cell`) and the code of each
//! row: 1, 2 or 4 bytes, by the number of distinct cells. The code 0 is a
//! row without a value: no record, or a record shorter than the column. The
//! code 1 is a value of a type that the table does not model. The table
//! also keeps the length of the record of each row: a brain comes from a
//! row only when its record has one value for each column, as the fast path
//! of `Catalog.instantiate` needs.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicI32, AtomicU32, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex, PoisonError};

use crate::base::Rows;
use crate::value::{Dedup, Spans, Val};

/// The code of a row without a value.
const ABSENT: u32 = 0;
/// The code of a value of another type.
const OTHER: u32 = 1;
/// The code of the first distinct cell.
const FIRST: u32 = 2;
/// The part of a row without a record in the grid of a build.
const NO_PART: u32 = u32::MAX;

/// The length of the record of a row without a record.
pub const NO_RECORD: i32 = i32::MIN;
/// The length of the record of a row whose record is not a tuple.
pub const NOT_A_TUPLE: i32 = -1;

/// Why an arena takes no more bytes: its offsets are 32 bits.
const FULL: &str = "the cells of data pass the 4 GiB of an arena";

/// An arena of byte strings, each found by its place. It holds at most
/// 4 GiB: past it, `push` and `extend` refuse.
#[derive(Clone, Debug, Default)]
pub struct ByteArena {
    bytes: Vec<u8>,
    ends: Vec<u32>,
}

impl ByteArena {
    /// Appends `data` and returns its place.
    pub fn push(&mut self, data: &[u8]) -> Result<u32, String> {
        let end = u32::try_from(self.bytes.len() + data.len()).map_err(|_| FULL)?;
        // `Dedup` keeps `u32::MAX` for a slot without a place.
        let place = u32::try_from(self.ends.len())
            .ok()
            .filter(|place| *place != u32::MAX)
            .ok_or(FULL)?;
        self.bytes.extend_from_slice(data);
        self.ends.push(end);
        Ok(place)
    }

    /// The bytes at `place`.
    pub fn get(&self, place: u32) -> &[u8] {
        let place = place as usize;
        let start = if place == 0 {
            0
        } else {
            self.ends[place - 1] as usize
        };
        &self.bytes[start..self.ends[place] as usize]
    }

    /// The number of byte strings.
    pub fn len(&self) -> usize {
        self.ends.len()
    }

    /// Tells if the arena holds nothing.
    pub fn is_empty(&self) -> bool {
        self.ends.is_empty()
    }

    /// Appends each byte string of `other` and returns the place of its
    /// first.
    pub fn extend(&mut self, other: &ByteArena) -> Result<u32, String> {
        u32::try_from(self.bytes.len() + other.bytes.len()).map_err(|_| FULL)?;
        u32::try_from(self.ends.len() + other.ends.len()).map_err(|_| FULL)?;
        let first = self.ends.len() as u32;
        let offset = self.bytes.len() as u32;
        self.bytes.extend_from_slice(&other.bytes);
        self.ends.extend(other.ends.iter().map(|end| end + offset));
        Ok(first)
    }

    /// The bytes that the arena takes.
    pub fn heap_bytes(&self) -> usize {
        self.bytes.capacity() + self.ends.capacity() * 4
    }

    fn shrink(&mut self) {
        self.bytes.shrink_to_fit();
        self.ends.shrink_to_fit();
    }
}

impl Spans for ByteArena {
    fn span(&self, place: u32) -> &[u8] {
        self.get(place)
    }
}

/// The codes of the rows of a column, in the fewest bytes.
#[derive(Debug)]
enum Codes {
    U8(Vec<u8>),
    U16(Vec<u16>),
    U32(Vec<u32>),
}

impl Codes {
    fn of(codes: Vec<u32>, distinct: usize) -> Codes {
        let top = distinct + FIRST as usize;
        if top <= 1 << 8 {
            Codes::U8(codes.into_iter().map(|c| c as u8).collect())
        } else if top <= 1 << 16 {
            Codes::U16(codes.into_iter().map(|c| c as u16).collect())
        } else {
            Codes::U32(codes)
        }
    }

    fn get(&self, row: u32) -> u32 {
        let row = row as usize;
        match self {
            Codes::U8(v) => v.get(row).map_or(ABSENT, |c| u32::from(*c)),
            Codes::U16(v) => v.get(row).map_or(ABSENT, |c| u32::from(*c)),
            Codes::U32(v) => v.get(row).copied().unwrap_or(ABSENT),
        }
    }

    fn heap_bytes(&self) -> usize {
        match self {
            Codes::U8(v) => v.capacity(),
            Codes::U16(v) => v.capacity() * 2,
            Codes::U32(v) => v.capacity() * 4,
        }
    }
}

/// The cell of a row in a column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cell<'a> {
    /// The row has no value: no record, or a record shorter than the
    /// column.
    Absent,
    /// A value of a type that the table does not model.
    Other,
    /// The encoded cell.
    Encoded(&'a [u8]),
}

impl Cell<'_> {
    /// The cell as a change set keeps it: `None` for no value.
    pub fn to_val(self) -> Option<Val> {
        match self {
            Cell::Absent => None,
            Cell::Other => Some(Val::Cell(None)),
            Cell::Encoded(bytes) => Some(Val::Cell(Some(bytes.into()))),
        }
    }

    /// The cell of a value of a change set.
    pub fn of_val(value: Option<&Val>) -> Cell<'_> {
        match value {
            Some(Val::Cell(Some(bytes))) => Cell::Encoded(bytes),
            Some(_) => Cell::Other,
            None => Cell::Absent,
        }
    }
}

/// One column of the table.
#[derive(Debug)]
pub struct Column {
    /// The code of each row. A row past its end has no value.
    codes: Codes,
    /// The distinct cells, in one or more arenas, one after the other: the
    /// code `FIRST + k` is the `k`-th cell of all the arenas.
    cells: Vec<ByteArena>,
    /// The number of the cells before each arena.
    starts: Vec<u32>,
}

impl Column {
    /// The column of the distinct cells of one arena.
    fn of(codes: Codes, cells: ByteArena) -> Column {
        Column {
            codes,
            cells: vec![cells],
            starts: vec![0],
        }
    }

    /// The cell of `row`.
    pub fn cell(&self, row: u32) -> Cell<'_> {
        match self.codes.get(row) {
            ABSENT => Cell::Absent,
            OTHER => Cell::Other,
            code => {
                let k = code - FIRST;
                let at = self.starts.partition_point(|start| *start <= k) - 1;
                Cell::Encoded(self.cells[at].get(k - self.starts[at]))
            }
        }
    }

    /// The bytes that the column takes.
    pub fn heap_bytes(&self) -> usize {
        self.codes.heap_bytes()
            + self.cells.iter().map(ByteArena::heap_bytes).sum::<usize>()
            + self.starts.capacity() * 4
    }

    /// Builds the column from the cell of each row of `items`, for `rows`
    /// rows: a place in `arena`, or `None` for a value of another type.
    pub fn build(
        arena: &ByteArena,
        items: &[(u32, Option<u32>)],
        rows: usize,
    ) -> Result<Column, String> {
        let mut codes = vec![ABSENT; rows];
        let mut cells = ByteArena::default();
        let mut found: HashMap<&[u8], u32> = HashMap::new();
        for (row, place) in items {
            codes[*row as usize] = match place {
                None => OTHER,
                Some(place) => {
                    let bytes = arena.get(*place);
                    match found.get(bytes) {
                        Some(code) => *code,
                        None => {
                            let code = FIRST + cells.push(bytes)?;
                            found.insert(bytes, code);
                            code
                        }
                    }
                }
            };
        }
        cells.shrink();
        Ok(Column::of(Codes::of(codes, cells.len()), cells))
    }

    /// Builds the column of a new base: the cells of this column, with the
    /// cells of `changed` over them, for `rows`.
    pub fn rebuilt(
        &self,
        changed: &BTreeMap<i32, Option<Val>>,
        rows: &Rows,
    ) -> Result<Column, String> {
        let mut arena = ByteArena::default();
        let mut items: Vec<(u32, Option<u32>)> = Vec::new();
        let mut touched = vec![false; rows.len()];
        for (rid, value) in changed {
            let Some(row) = rows.row(*rid) else {
                continue;
            };
            touched[row as usize] = true;
            match Cell::of_val(value.as_ref()) {
                Cell::Absent => {}
                Cell::Other => items.push((row, None)),
                Cell::Encoded(bytes) => items.push((row, Some(arena.push(bytes)?))),
            }
        }
        // The places of the cells of this column in `arena`.
        let first = arena.len() as u32;
        for cells in &self.cells {
            arena.extend(cells)?;
        }
        for row in 0..rows.len() as u32 {
            if touched[row as usize] {
                continue;
            }
            match self.codes.get(row) {
                ABSENT => {}
                OTHER => items.push((row, None)),
                code => items.push((row, Some(first + code - FIRST))),
            }
        }
        Column::build(&arena, &items, rows.len())
    }
}

/// The metadata table of a base.
#[derive(Clone, Debug, Default)]
pub struct Table {
    /// The position of each column, from the `schema` of the catalog. Empty
    /// when the schema does not give each position from 0 once: no brain
    /// then comes from a row.
    pub schema: HashMap<String, usize>,
    /// The columns, by position.
    pub columns: Vec<Arc<Column>>,
    /// The length of the record of each row, `NO_RECORD` for no record. A
    /// row past its end has no record.
    pub widths: Arc<Vec<i32>>,
}

impl Table {
    /// The number of columns of the schema.
    pub fn width(&self) -> usize {
        self.columns.len()
    }

    /// The length of the record of `row`, `None` for no record.
    pub fn record_width(&self, row: u32) -> Option<i32> {
        self.widths
            .get(row as usize)
            .copied()
            .filter(|w| *w != NO_RECORD)
    }

    /// The bytes that the table takes. A column that another base shares
    /// counts in each.
    pub fn heap_bytes(&self) -> usize {
        self.columns.iter().map(|c| c.heap_bytes()).sum::<usize>()
            + self.widths.capacity() * 4
            + self.schema.keys().map(|k| k.len() + 48).sum::<usize>()
    }
}

/// The codes of the cells of `data` as the threads of a build collect them:
/// for each column and row, the code of the cell in the part of the row
/// (`TablePart`); the part of each row; and the length of the record of each
/// row. The threads write to distinct rows.
pub struct Grid {
    /// The number of columns.
    width: usize,
    /// The number of rows.
    rows: usize,
    /// The codes, column after column: `ABSENT`, `OTHER`, or `FIRST` and
    /// the place of the cell in the arena of the column of its part.
    codes: Vec<AtomicU32>,
    /// The number of the part of each row, `NO_PART` for a row without a
    /// record.
    parts: Vec<AtomicU32>,
    /// The length of the record of each row, `NO_RECORD` for no record.
    widths: Vec<AtomicI32>,
}

impl Grid {
    /// The grid of a table of `width` columns and `rows` rows.
    pub fn new(width: usize, rows: usize) -> Grid {
        let atomics = |n: usize| (0..n).map(|_| AtomicU32::new(ABSENT)).collect();
        Grid {
            width,
            rows,
            codes: atomics(width * rows),
            parts: (0..rows).map(|_| AtomicU32::new(NO_PART)).collect(),
            widths: (0..rows).map(|_| AtomicI32::new(NO_RECORD)).collect(),
        }
    }

    /// The bytes that the grid takes.
    pub fn heap_bytes(&self) -> usize {
        (self.codes.capacity() + self.parts.capacity() + self.widths.capacity()) * 4
    }

    /// Builds the table with the positions `schema` from the cells of
    /// `parts`, with `threads` threads. Each column builds in one thread
    /// (`column`).
    pub fn build(
        self,
        mut parts: Vec<TablePart>,
        schema: HashMap<String, usize>,
        threads: usize,
    ) -> Result<Table, String> {
        let width = self.width;
        // The cells of each column in each part, by the number of the part.
        let mut by_column: Vec<Vec<ByteArena>> = (0..width).map(|_| Vec::new()).collect();
        parts.sort_by_key(|part| part.id);
        for part in parts {
            for (column, (cells, _)) in by_column.iter_mut().zip(part.columns) {
                column.push(cells);
            }
        }
        let todo = Mutex::new(by_column.into_iter().enumerate().collect::<Vec<_>>());
        let built = Mutex::new(Vec::with_capacity(width));
        let failed = Mutex::new(None);
        let grid = &self;
        std::thread::scope(|scope| {
            for _ in 0..threads.clamp(1, width.max(1)) {
                scope.spawn(|| {
                    loop {
                        let next = todo.lock().unwrap_or_else(PoisonError::into_inner).pop();
                        let Some((i, cells)) = next else {
                            return;
                        };
                        match grid.column(i, cells) {
                            Ok(column) => built
                                .lock()
                                .unwrap_or_else(PoisonError::into_inner)
                                .push((i, Arc::new(column))),
                            Err(e) => {
                                failed
                                    .lock()
                                    .unwrap_or_else(PoisonError::into_inner)
                                    .get_or_insert(e);
                                return;
                            }
                        }
                    }
                });
            }
        });
        if let Some(e) = failed.into_inner().unwrap_or_else(PoisonError::into_inner) {
            return Err(e);
        }
        let mut columns = built.into_inner().unwrap_or_else(PoisonError::into_inner);
        if columns.len() != width {
            return Err("a column of the table did not build".into());
        }
        columns.sort_by_key(|(i, _)| *i);
        let Grid { widths, .. } = self;
        Ok(Table {
            schema,
            columns: columns.into_iter().map(|(_, c)| c).collect(),
            widths: Arc::new(widths.into_iter().map(AtomicI32::into_inner).collect()),
        })
    }

    /// Builds the column `i` from its cells in each part, by the number of
    /// the part. The arena of a part whose cells no earlier part holds
    /// becomes an arena of the column, with no copy. Of another part, the
    /// column keeps a copy of the new cells, and the part is freed.
    fn column(&self, i: usize, parts: Vec<ByteArena>) -> Result<Column, String> {
        // The code of each cell of each part, and the first code of the new
        // cells of each part. The new cells of a part take the next codes,
        // in their order in the part.
        let mut places: Vec<Vec<u32>> = Vec::with_capacity(parts.len());
        let mut starts: Vec<u32> = Vec::with_capacity(parts.len() + 1);
        {
            let total = parts.iter().map(ByteArena::len).sum();
            let mut seen = Seen {
                parts: &parts,
                cells: Vec::with_capacity(total),
            };
            let mut dedup = Dedup::with_capacity(total);
            for (p, part) in parts.iter().enumerate() {
                starts.push(seen.cells.len() as u32);
                let mut found = Vec::with_capacity(part.len());
                for place in 0..part.len() as u32 {
                    found.push(dedup.place(&mut seen, part.get(place), |seen| {
                        let code = u32::try_from(seen.cells.len())
                            .ok()
                            .filter(|code| *code < u32::MAX - FIRST)
                            .ok_or(FULL)?;
                        seen.cells.push((p as u32, place));
                        Ok::<_, String>(code)
                    })?);
                }
                places.push(found);
            }
            starts.push(seen.cells.len() as u32);
        }
        let mut cells = Vec::new();
        let mut firsts = Vec::new();
        for (p, mut part) in parts.into_iter().enumerate() {
            let (first, end) = (starts[p], starts[p + 1]);
            if first == end {
                continue;
            }
            let new = end - first;
            if new as usize != part.len() {
                // The new cells of the part are the cells whose code is
                // the next code.
                let mut kept = ByteArena::default();
                let mut next = first;
                for (place, code) in places[p].iter().enumerate() {
                    if *code == next {
                        kept.push(part.get(place as u32))?;
                        next += 1;
                    }
                }
                part = kept;
            }
            part.shrink();
            cells.push(part);
            firsts.push(first);
        }
        // A rebase joins the arenas of a column into one, so the column
        // holds at most the 4 GiB of one arena.
        if u32::try_from(cells.iter().map(|a| a.bytes.len()).sum::<usize>()).is_err() {
            return Err(FULL.into());
        }
        let distinct = *starts.last().unwrap_or(&0) as usize;
        let codes = &self.codes[i * self.rows..(i + 1) * self.rows];
        let code = |row: usize| match codes[row].load(AtomicOrdering::Relaxed) {
            code @ (ABSENT | OTHER) => code,
            code => {
                let part = self.parts[row].load(AtomicOrdering::Relaxed) as usize;
                FIRST + places[part][(code - FIRST) as usize]
            }
        };
        let top = distinct + FIRST as usize;
        let codes = if top <= 1 << 8 {
            Codes::U8((0..self.rows).map(|row| code(row) as u8).collect())
        } else if top <= 1 << 16 {
            Codes::U16((0..self.rows).map(|row| code(row) as u16).collect())
        } else {
            Codes::U32((0..self.rows).map(code).collect())
        };
        Ok(Column {
            codes,
            cells,
            starts: firsts,
        })
    }
}

/// The distinct cells of a column in the parts of a build, as `Dedup`
/// reads them: the part and the place of each.
struct Seen<'a> {
    parts: &'a [ByteArena],
    cells: Vec<(u32, u32)>,
}

impl Spans for Seen<'_> {
    fn span(&self, code: u32) -> &[u8] {
        let (part, place) = self.cells[code as usize];
        self.parts[part as usize].get(place)
    }
}

/// The cells of the records of `data` that one thread of a build collects:
/// the distinct cells of each column. The code of each cell of a row goes
/// into the `Grid`.
#[derive(Debug, Default)]
pub struct TablePart {
    /// The number of the part, which the grid keeps for each of its rows.
    id: u32,
    /// For each column, its distinct cells and their set.
    columns: Vec<(ByteArena, Dedup)>,
}

impl TablePart {
    /// The part number `id` of a table of `width` columns.
    pub fn new(id: u32, width: usize) -> TablePart {
        TablePart {
            id,
            columns: (0..width).map(|_| Default::default()).collect(),
        }
    }

    /// Adds the record `value` of `row`, as `record_of` reads it, into this
    /// part and `grid`.
    pub fn push(&mut self, grid: &Grid, row: u32, value: &Val) -> Result<(), String> {
        let at = row as usize;
        if at >= grid.rows {
            return Err(format!("the row {row} is past the rows of the table"));
        }
        let (cells, width): (&[Val], i32) = match value {
            Val::Tuple(cells) => (cells, i32::try_from(cells.len()).unwrap_or(i32::MAX)),
            _ => (&[], NOT_A_TUPLE),
        };
        // A row whose record a part already took is in `data` twice: the
        // codes of the row would mix the cells of two parts.
        if grid.parts[at].swap(self.id, AtomicOrdering::Relaxed) != NO_PART {
            return Err(format!("the row {row} has two records in data"));
        }
        grid.widths[at].store(width, AtomicOrdering::Relaxed);
        for (i, ((arena, dedup), cell)) in self.columns.iter_mut().zip(cells).enumerate() {
            let code = match cell {
                Val::Cell(Some(bytes)) => {
                    let place = dedup.place(arena, bytes, |arena| arena.push(bytes))?;
                    place.checked_add(FIRST).ok_or(FULL)?
                }
                _ => OTHER,
            };
            grid.codes[i * grid.rows + at].store(code, AtomicOrdering::Relaxed);
        }
        Ok(())
    }

    /// The bytes that the part takes.
    pub fn heap_bytes(&self) -> usize {
        self.columns
            .iter()
            .map(|(arena, dedup)| arena.heap_bytes() + dedup.heap_bytes())
            .sum()
    }
}

/// The value of a record of `data` as the table reads it: a tuple of
/// cells, or `Val::Cell(None)` for a record that is not a tuple.
pub fn record_of(heap: &zoe_pickle::python::Heap, value: zoe_pickle::python::Value) -> Val {
    match heap.tuple(value) {
        Some(items) => Val::Tuple(
            items
                .iter()
                .map(|item| Val::Cell(crate::cell::encode(heap, *item).map(Vec::into_boxed_slice)))
                .collect(),
        ),
        None => Val::Cell(None),
    }
}

/// The length of a record of `data` in the form of `record_of`, as a
/// change set keeps it.
pub fn width_of(record: &Val) -> i32 {
    match record {
        Val::Tuple(cells) => i32::try_from(cells.len()).unwrap_or(i32::MAX),
        _ => NOT_A_TUPLE,
    }
}

/// The table of a new base: the columns and the widths of `table`, with
/// the changes of `head` over them, for `rows`. A column that no change
/// touched stays shared.
pub fn rebuilt(
    table: &Table,
    columns: &[Option<&BTreeMap<i32, Option<Val>>>],
    widths: Option<&BTreeMap<i32, Option<Val>>>,
    rows: &Rows,
) -> Result<Table, String> {
    let mut out = table.clone();
    for (i, changed) in columns.iter().enumerate() {
        if let (Some(changed), Some(column)) = (changed, out.columns.get_mut(i)) {
            *column = Arc::new(column.rebuilt(changed, rows)?);
        }
    }
    let grown = rows.len() > table.widths.len();
    if widths.is_some() || grown {
        let mut new = table.widths.as_ref().clone();
        new.resize(rows.len(), NO_RECORD);
        for (rid, width) in widths.into_iter().flatten() {
            if let Some(row) = rows.row(*rid) {
                new[row as usize] = match width {
                    Some(Val::Int(w)) => i32::try_from(*w).unwrap_or(i32::MAX),
                    _ => NO_RECORD,
                };
            }
        }
        out.widths = Arc::new(new);
    }
    Ok(out)
}
