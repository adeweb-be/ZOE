//! Rank the ConflictErrors of Zope logs by class and by oid.
//!
//! The parser reads the text of ZODB's `ConflictError` and
//! `ReadConflictError`, for example `database conflict error (oid 0x7a3c,
//! class BTrees.OOBTree.OOBucket, serial ...)`, anywhere in a line. A
//! conflict without a class counts under the class `unknown`. A conflict
//! without an oid counts under the oid `unknown`.

use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::io::{self, BufRead};

use serde::Serialize;

/// The name that replaces a missing class or oid.
pub const UNKNOWN: &str = "unknown";

/// The texts that start a ZODB conflict error, before its details.
const MARKERS: [&str; 2] = [
    "database conflict error (",
    "database read conflict error (",
];

/// One conflict that a log line reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conflict {
    /// The oid as ZODB writes it, for example `0x7a3c`.
    pub oid: String,
    /// The dotted name of the class.
    pub class: String,
}

/// Return the conflict that `line` reports, or `None`.
///
/// The details in parentheses must name an oid or a class. Other text in
/// parentheses is not a ZODB conflict.
pub fn parse_line(line: &str) -> Option<Conflict> {
    let start = MARKERS
        .iter()
        .filter_map(|marker| line.find(marker).map(|i| i + marker.len()))
        .min()?;
    let end = start + line[start..].find(')')?;

    let mut oid = None;
    let mut class = None;
    for part in line[start..end].split(',').map(str::trim) {
        if let Some(value) = part.strip_prefix("oid ") {
            oid = Some(value.trim().to_owned());
        } else if let Some(value) = part.strip_prefix("class ") {
            class = Some(value.trim().to_owned());
        }
    }
    if oid.is_none() && class.is_none() {
        return None;
    }
    Some(Conflict {
        oid: oid.unwrap_or_else(|| UNKNOWN.to_owned()),
        class: class.unwrap_or_else(|| UNKNOWN.to_owned()),
    })
}

/// The counts of the conflicts read so far.
#[derive(Debug, Default)]
pub struct Ranking {
    total: u64,
    by_class: BTreeMap<String, u64>,
    by_oid: BTreeMap<(String, String), u64>,
}

impl Ranking {
    /// Count one conflict.
    pub fn add(&mut self, conflict: Conflict) {
        self.total += 1;
        *self.by_class.entry(conflict.class.clone()).or_default() += 1;
        *self
            .by_oid
            .entry((conflict.oid, conflict.class))
            .or_default() += 1;
    }

    /// Count every conflict that `reader` reports, line by line. Bytes that
    /// are not UTF-8 do not stop the read.
    pub fn read(&mut self, mut reader: impl BufRead) -> io::Result<()> {
        let mut buf = Vec::new();
        loop {
            buf.clear();
            if reader.read_until(b'\n', &mut buf)? == 0 {
                return Ok(());
            }
            if let Some(conflict) = parse_line(&String::from_utf8_lossy(&buf)) {
                self.add(conflict);
            }
        }
    }

    /// Return the report. Rows go from the highest count to the lowest; equal
    /// counts go in ascending order of class, or of oid then class.
    pub fn report(&self) -> Report {
        let mut by_class: Vec<ClassCount> = self
            .by_class
            .iter()
            .map(|(class, &count)| ClassCount {
                class: class.clone(),
                count,
            })
            .collect();
        // The map is in key order, and the sort is stable.
        by_class.sort_by_key(|row| Reverse(row.count));

        let mut by_oid: Vec<OidCount> = self
            .by_oid
            .iter()
            .map(|((oid, class), &count)| OidCount {
                oid: oid.clone(),
                class: class.clone(),
                count,
            })
            .collect();
        by_oid.sort_by_key(|row| Reverse(row.count));

        Report {
            total: self.total,
            by_class,
            by_oid,
        }
    }
}

/// The ranking as the command prints it.
#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct Report {
    /// The number of conflicts.
    pub total: u64,
    /// The conflicts per class.
    pub by_class: Vec<ClassCount>,
    /// The conflicts per oid.
    pub by_oid: Vec<OidCount>,
}

/// The number of conflicts of one class.
#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct ClassCount {
    /// The dotted name of the class.
    pub class: String,
    /// The number of conflicts.
    pub count: u64,
}

/// The number of conflicts of one oid.
#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct OidCount {
    /// The oid as ZODB writes it.
    pub oid: String,
    /// The dotted name of the class of the object.
    pub class: String,
    /// The number of conflicts.
    pub count: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conflict(oid: &str, class: &str) -> Conflict {
        Conflict {
            oid: oid.to_owned(),
            class: class.to_owned(),
        }
    }

    #[test]
    fn a_write_conflict_gives_its_oid_and_class() {
        let line = "ZODB.POSException.ConflictError: database conflict error (oid 0x7a3c, \
                    class BTrees.OOBTree.OOBucket, serial this txn started with \
                    0x0000000000000001 1900-01-01 00:00:00.000000, serial currently \
                    committed 0x0000000000000002 1900-01-01 00:00:00.000000)";
        assert_eq!(
            parse_line(line),
            Some(conflict("0x7a3c", "BTrees.OOBTree.OOBucket"))
        );
    }

    #[test]
    fn a_read_conflict_counts_too() {
        let line = "ReadConflictError: database read conflict error (oid 0x10, \
                    class persistent.mapping.PersistentMapping)";
        assert_eq!(
            parse_line(line),
            Some(conflict("0x10", "persistent.mapping.PersistentMapping"))
        );
    }

    #[test]
    fn a_missing_class_is_unknown() {
        let line = "database conflict error (oid 0x0abc, serial this txn started with 0x01)";
        assert_eq!(parse_line(line), Some(conflict("0x0abc", UNKNOWN)));
    }

    #[test]
    fn a_missing_oid_is_unknown() {
        let line = "database conflict error (class BTrees.Length.Length)";
        assert_eq!(
            parse_line(line),
            Some(conflict(UNKNOWN, "BTrees.Length.Length"))
        );
    }

    #[test]
    fn a_line_without_the_error_text_is_not_a_conflict() {
        assert_eq!(
            parse_line("ConflictError: http://localhost/site/edit"),
            None
        );
        assert_eq!(parse_line("a line that mentions ConflictError"), None);
    }

    #[test]
    fn parentheses_without_an_oid_or_a_class_are_not_a_conflict() {
        assert_eq!(parse_line("database conflict error (see above)"), None);
    }

    #[test]
    fn unclosed_parentheses_are_not_a_conflict() {
        assert_eq!(
            parse_line("database conflict error (oid 0x01, class X"),
            None
        );
    }

    #[test]
    fn equal_counts_sort_by_name() {
        let mut ranking = Ranking::default();
        ranking.add(conflict("0x02", "b.B"));
        ranking.add(conflict("0x01", "a.A"));
        ranking.add(conflict("0x03", "c.C"));
        ranking.add(conflict("0x03", "c.C"));

        let report = ranking.report();
        assert_eq!(report.total, 4);
        let classes: Vec<_> = report.by_class.iter().map(|r| r.class.as_str()).collect();
        assert_eq!(classes, ["c.C", "a.A", "b.B"]);
        let oids: Vec<_> = report.by_oid.iter().map(|r| r.oid.as_str()).collect();
        assert_eq!(oids, ["0x03", "0x01", "0x02"]);
    }

    #[test]
    fn one_oid_with_two_classes_gives_two_rows() {
        let mut ranking = Ranking::default();
        ranking.add(conflict("0x01", "a.A"));
        ranking.add(conflict("0x01", "b.B"));
        assert_eq!(ranking.report().by_oid.len(), 2);
    }

    #[test]
    fn bytes_that_are_not_utf8_do_not_stop_the_read() {
        let mut log = b"\xff\xfe garbage\n".to_vec();
        log.extend_from_slice(b"database conflict error (oid 0x01, class a.A)\n");
        log.extend_from_slice(b"database conflict error (oid 0x01, class a.A)");

        let mut ranking = Ranking::default();
        ranking.read(&log[..]).unwrap();
        assert_eq!(ranking.report().total, 2);
    }
}
