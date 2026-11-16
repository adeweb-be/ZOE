//! The table that a build makes from the parts of the threads of the walk of
//! `data`: each thread keeps the distinct cells of each column, and the grid
//! keeps the code of each cell. A cell that several parts hold is one cell
//! of the column, and each row gives its own cell, its width and its kind.
//! A column of several arenas rebuilds with its cells and the changes, and a
//! row with two records fails the walk.

use std::collections::{BTreeMap, HashMap};

use zoe_catalog::base::Rows;
use zoe_catalog::table::{Cell, Grid, NOT_A_TUPLE, TablePart};
use zoe_catalog::value::Val;

fn cell(bytes: &[u8]) -> Val {
    Val::Cell(Some(bytes.into()))
}

fn record(cells: Vec<Val>) -> Val {
    Val::Tuple(cells.into())
}

#[test]
fn the_table_of_several_parts_gives_the_cell_and_the_width_of_each_row() {
    let grid = Grid::new(2, 8);
    let mut parts: Vec<TablePart> = (0..3).map(|id| TablePart::new(id, 2)).collect();
    // The part, the row and the record. The row 7 has no record.
    let records = [
        (0, 0, record(vec![cell(b"a"), cell(b"x0")])),
        (0, 1, record(vec![cell(b"b"), cell(b"x1")])),
        (1, 2, record(vec![cell(b"a"), cell(b"x2")])),
        (1, 3, record(vec![cell(b"c"), Val::Cell(None)])),
        (2, 4, record(vec![cell(b"b")])),
        (2, 5, Val::Cell(None)),
        (1, 6, record(vec![cell(b"a"), cell(b"x0"), cell(b"more")])),
    ];
    for (part, row, value) in &records {
        parts[*part]
            .push(&grid, *row, value)
            .expect("the record goes in");
    }
    // The parts come in another order than their numbers.
    parts.reverse();
    let schema = HashMap::from([("k".to_owned(), 0), ("v".to_owned(), 1)]);
    let table = grid.build(parts, schema, 2).expect("the table builds");
    assert_eq!(table.width(), 2);
    let keys = [
        Cell::Encoded(b"a"),
        Cell::Encoded(b"b"),
        Cell::Encoded(b"a"),
        Cell::Encoded(b"c"),
        Cell::Encoded(b"b"),
        Cell::Absent,
        Cell::Encoded(b"a"),
        Cell::Absent,
    ];
    let values = [
        Cell::Encoded(b"x0"),
        Cell::Encoded(b"x1"),
        Cell::Encoded(b"x2"),
        Cell::Other,
        Cell::Absent,
        Cell::Absent,
        Cell::Encoded(b"x0"),
        Cell::Absent,
    ];
    for row in 0..8 {
        assert_eq!(table.columns[0].cell(row), keys[row as usize], "row {row}");
        assert_eq!(
            table.columns[1].cell(row),
            values[row as usize],
            "row {row}"
        );
    }
    let widths: Vec<Option<i32>> = (0..8).map(|row| table.record_width(row)).collect();
    assert_eq!(
        widths,
        [
            Some(2),
            Some(2),
            Some(2),
            Some(2),
            Some(1),
            Some(NOT_A_TUPLE),
            Some(3),
            None
        ]
    );
}

#[test]
fn a_part_that_repeats_the_cells_of_another_adds_no_cell() {
    // Many rows over two parts with the same three cells: the codes of the
    // column fit in one byte.
    let rows = 3000;
    let grid = Grid::new(1, rows);
    let mut parts: Vec<TablePart> = (0..2).map(|id| TablePart::new(id, 1)).collect();
    let texts: [&[u8]; 3] = [b"one", b"two", b"three"];
    for row in 0..rows as u32 {
        let value = record(vec![cell(texts[row as usize % 3])]);
        parts[row as usize % 2]
            .push(&grid, row, &value)
            .expect("the record goes in");
    }
    let schema = HashMap::from([("k".to_owned(), 0)]);
    let table = grid.build(parts, schema, 4).expect("the table builds");
    for row in 0..rows as u32 {
        assert_eq!(
            table.columns[0].cell(row),
            Cell::Encoded(texts[row as usize % 3])
        );
    }
    // One byte for each row, and the three cells.
    assert!(table.columns[0].heap_bytes() < rows + 64);
}

#[test]
fn a_table_without_rows_keeps_its_columns() {
    let grid = Grid::new(3, 0);
    let schema = HashMap::from([
        ("a".to_owned(), 0),
        ("b".to_owned(), 1),
        ("c".to_owned(), 2),
    ]);
    let table = grid.build(Vec::new(), schema, 2).expect("the table builds");
    assert_eq!(table.width(), 3);
}

#[test]
fn a_rebuilt_column_of_several_arenas_keeps_its_cells_and_takes_the_changes() {
    let rows = 6;
    let grid = Grid::new(1, rows);
    let mut parts: Vec<TablePart> = (0..2).map(|id| TablePart::new(id, 1)).collect();
    // Each part holds cells of its own, so each is an arena of the column.
    let texts: [&[u8]; 6] = [b"p0-a", b"p1-a", b"p0-b", b"p1-b", b"p0-c", b"p1-c"];
    for row in 0..rows as u32 {
        let value = record(vec![cell(texts[row as usize])]);
        parts[row as usize % 2]
            .push(&grid, row, &value)
            .expect("the record goes in");
    }
    let schema = HashMap::from([("k".to_owned(), 0)]);
    let table = grid.build(parts, schema, 2).expect("the table builds");
    let column = &table.columns[0];
    // The rid of the row `r` is `10 + r`.
    let entries: Vec<(i32, String)> = (0..rows as i32)
        .map(|r| (10 + r, format!("/site/item-{r}")))
        .collect();
    let all = Rows::from_sorted(&entries);
    let changed = BTreeMap::from([
        (11, Some(cell(b"new"))),
        (12, None),
        (13, Some(cell(b"p0-a"))),
    ]);
    let rebuilt = column.rebuilt(&changed, &all).expect("the rebuild");
    let expected = [
        Cell::Encoded(b"p0-a"),
        Cell::Encoded(b"new"),
        Cell::Absent,
        Cell::Encoded(b"p0-a"),
        Cell::Encoded(b"p0-c"),
        Cell::Encoded(b"p1-c"),
    ];
    for row in 0..rows as u32 {
        assert_eq!(column.cell(row), Cell::Encoded(texts[row as usize]));
        assert_eq!(rebuilt.cell(row), expected[row as usize], "row {row}");
    }
}

#[test]
fn a_row_with_two_records_fails_the_part_that_takes_the_second() {
    let grid = Grid::new(1, 2);
    let mut parts: Vec<TablePart> = (0..2).map(|id| TablePart::new(id, 1)).collect();
    parts[0]
        .push(&grid, 1, &record(vec![cell(b"a")]))
        .expect("the first record goes in");
    let twice = parts[1].push(&grid, 1, &record(vec![cell(b"b")]));
    assert!(twice.is_err(), "{twice:?}");
}
