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
use std::sync::Arc;

use crate::base::Rows;
use crate::value::Val;

/// The code of a row without a value.
const ABSENT: u32 = 0;
/// The code of a value of another type.
const OTHER: u32 = 1;
/// The code of the first distinct cell.
const FIRST: u32 = 2;

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
        let place = u32::try_from(self.ends.len()).map_err(|_| FULL)?;
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
    /// The distinct cells: the code `FIRST + k` is the cell at `k`.
    cells: ByteArena,
}

impl Column {
    /// The cell of `row`.
    pub fn cell(&self, row: u32) -> Cell<'_> {
        match self.codes.get(row) {
            ABSENT => Cell::Absent,
            OTHER => Cell::Other,
            code => Cell::Encoded(self.cells.get(code - FIRST)),
        }
    }

    /// The bytes that the column takes.
    pub fn heap_bytes(&self) -> usize {
        self.codes.heap_bytes() + self.cells.heap_bytes()
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
        Ok(Column {
            codes: Codes::of(codes, cells.len()),
            cells,
        })
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
        let first = arena.extend(&self.cells)?;
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

/// The cells of a record of `data` as a build collects them: one arena,
/// and the row and the place of each cell of each column.
#[derive(Debug, Default)]
pub struct TablePart {
    /// The encoded cells.
    pub arena: ByteArena,
    /// For each column, the row and the place of each cell, `None` for a
    /// value of another type.
    pub columns: Vec<Vec<(u32, Option<u32>)>>,
    /// The row and the length of each record.
    pub widths: Vec<(u32, i32)>,
}

impl TablePart {
    /// The part of a table of `width` columns.
    pub fn new(width: usize) -> TablePart {
        TablePart {
            columns: vec![Vec::new(); width],
            ..TablePart::default()
        }
    }

    /// Adds the record `value` of `row`, as `record_of` reads it.
    pub fn push(&mut self, row: u32, value: &Val) -> Result<(), String> {
        let cells: &[Val] = match value {
            Val::Tuple(cells) => cells,
            _ => &[],
        };
        let width = match value {
            Val::Tuple(cells) => i32::try_from(cells.len()).unwrap_or(i32::MAX),
            _ => NOT_A_TUPLE,
        };
        self.widths.push((row, width));
        for (column, cell) in self.columns.iter_mut().zip(cells) {
            match cell {
                Val::Cell(Some(bytes)) => column.push((row, Some(self.arena.push(bytes)?))),
                _ => column.push((row, None)),
            }
        }
        Ok(())
    }

    /// Adds the cells of `other`.
    pub fn append(&mut self, other: TablePart) -> Result<(), String> {
        let first = self.arena.extend(&other.arena)?;
        for (mine, theirs) in self.columns.iter_mut().zip(other.columns) {
            mine.extend(
                theirs
                    .into_iter()
                    .map(|(row, place)| (row, place.map(|p| p + first))),
            );
        }
        self.widths.extend(other.widths);
        Ok(())
    }

    /// The bytes that the part takes.
    pub fn heap_bytes(&self) -> usize {
        self.arena.heap_bytes()
            + self
                .columns
                .iter()
                .map(|c| c.capacity() * 12)
                .sum::<usize>()
            + self.widths.capacity() * 8
    }

    /// Builds the table of `rows` rows with the positions `schema`.
    pub fn build(self, schema: HashMap<String, usize>, rows: usize) -> Result<Table, String> {
        let columns = self
            .columns
            .iter()
            .map(|items| Column::build(&self.arena, items, rows).map(Arc::new))
            .collect::<Result<_, _>>()?;
        let mut widths = vec![NO_RECORD; rows];
        for (row, width) in self.widths {
            widths[row as usize] = width;
        }
        Ok(Table {
            schema,
            columns,
            widths: Arc::new(widths),
        })
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
