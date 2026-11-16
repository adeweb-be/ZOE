//! The table that a build makes from the parts of the threads of the walk of
//! `data`: each thread keeps the distinct cells of each column, and the grid
//! keeps the code of each cell. A cell that several parts hold is one cell
//! of the column, and each row gives its own cell, its width and its kind.

use std::collections::HashMap;

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
