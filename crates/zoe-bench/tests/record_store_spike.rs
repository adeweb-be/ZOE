//! Acceptance test of the record-store spike: both backends report numbers
//! for every workload cell.

use zoe_bench::record_store::{BackendKind, Cell, Report, measure};

const SIZES: [usize; 2] = [1024, 100 * 1024];
const OBJECTS: [usize; 3] = [1, 10, 100];
const WRITERS: [usize; 3] = [1, 16, 64];

fn cells() -> Vec<Cell> {
    let mut cells = Vec::new();
    for size in SIZES {
        for objects in OBJECTS {
            for writers in WRITERS {
                cells.push(Cell {
                    object_size: size,
                    objects_per_txn: objects,
                    writers,
                });
            }
        }
    }
    cells
}

fn check(report: &Report) {
    assert_eq!(report.rows.len(), cells().len());
    for row in &report.rows {
        assert!(row.write_amplification >= 1.0, "{row:?}");
        assert!(row.fsyncs_per_commit > 0.0, "{row:?}");
        assert!(row.commits_per_second > 0.0, "{row:?}");
    }
}

#[test]
fn redb_reports_every_cell() {
    check(&measure(BackendKind::Redb, &cells(), 0.010).unwrap());
}

#[test]
fn the_append_only_log_reports_every_cell() {
    check(&measure(BackendKind::AppendOnlyLog, &cells(), 0.010).unwrap());
}
