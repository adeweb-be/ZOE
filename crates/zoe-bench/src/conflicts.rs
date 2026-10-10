//! Rank the ConflictErrors of Zope logs by class and by oid.
//!
//! The parser reads the text of ZODB's `ConflictError` and
//! `ReadConflictError`, for example `database conflict error (oid 0x7a3c,
//! class BTrees.OOBTree.OOBucket, serial ...)`, anywhere in a line. The
//! details must have the ZODB form: only `oid`, `class` and `serial` parts,
//! a hexadecimal oid and a dotted class name. Other text is not a conflict,
//! so free text never reaches the report.
//!
//! The readCurrent conflicts of ZODB 6 name no class. A conflict that names
//! no class takes the class that other lines name for the same oid, or
//! `unknown`. A conflict without an oid counts in the total and in the
//! classes, not in the oids.

use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::io::{self, BufRead};

use serde::Serialize;

/// The name that replaces a missing class.
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
    pub oid: Option<String>,
    /// The dotted name of the class.
    pub class: Option<String>,
}

/// Return the first conflict that `line` reports, or `None`.
///
/// The parser tries each marker in the line, from left to right, and keeps
/// the first one whose details have the ZODB form.
pub fn parse_line(line: &str) -> Option<Conflict> {
    let mut starts: Vec<usize> = MARKERS
        .iter()
        .flat_map(|marker| {
            line.match_indices(marker)
                .map(move |(i, _)| i + marker.len())
        })
        .collect();
    starts.sort_unstable();
    starts
        .into_iter()
        .find_map(|start| parse_details(&line[start..]))
}

/// Parse the details that follow a marker, up to the closing parenthesis.
fn parse_details(rest: &str) -> Option<Conflict> {
    let details = &rest[..rest.find(')')?];
    let mut conflict = Conflict {
        oid: None,
        class: None,
    };
    for part in details.split(',').map(str::trim) {
        if let Some(oid) = part.strip_prefix("oid ") {
            if !is_oid(oid) {
                return None;
            }
            conflict.oid = Some(oid.to_owned());
        } else if let Some(class) = part.strip_prefix("class ") {
            if !is_class_name(class) {
                return None;
            }
            conflict.class = Some(class.to_owned());
        } else if !part.starts_with("serial ") {
            return None;
        }
    }
    (conflict.oid.is_some() || conflict.class.is_some()).then_some(conflict)
}

/// Tell if `text` is an oid as ZODB writes it: `0x` and hexadecimal digits.
fn is_oid(text: &str) -> bool {
    text.strip_prefix("0x")
        .is_some_and(|hex| !hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Tell if `text` is a dotted Python name with at least one dot, such as
/// `BTrees.OOBTree.OOBucket`.
fn is_class_name(text: &str) -> bool {
    let mut names = 0;
    for name in text.split('.') {
        let mut chars = name.chars();
        let Some(first) = chars.next() else {
            return false;
        };
        if !(first == '_' || first.is_ascii_alphabetic())
            || !chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
        {
            return false;
        }
        names += 1;
    }
    names >= 2
}

/// The counts of the conflicts read so far.
#[derive(Debug, Default)]
pub struct Ranking {
    total: u64,
    /// For each oid, the number of conflicts per named class. The key `None`
    /// counts the conflicts that name no class.
    by_oid: BTreeMap<String, BTreeMap<Option<String>, u64>>,
    /// The conflicts without an oid, per named class.
    without_oid: BTreeMap<Option<String>, u64>,
}

impl Ranking {
    /// Count one conflict.
    pub fn add(&mut self, conflict: Conflict) {
        self.total += 1;
        let classes = match conflict.oid {
            Some(oid) => self.by_oid.entry(oid).or_default(),
            None => &mut self.without_oid,
        };
        *classes.entry(conflict.class).or_default() += 1;
    }

    /// Count the first conflict of every line that `reader` gives. Bytes
    /// that are not UTF-8 do not stop the read.
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

    /// Return the report. Rows go from the highest count to the lowest;
    /// equal counts go in ascending order of class or of oid.
    pub fn report(&self) -> Report {
        let mut class_counts: BTreeMap<String, u64> = BTreeMap::new();
        let mut by_oid = Vec::new();

        for (oid, classes) in &self.by_oid {
            let class = oid_class(classes);
            for (named, &count) in classes {
                let name = named.as_ref().unwrap_or(&class);
                *class_counts.entry(name.clone()).or_default() += count;
            }
            by_oid.push(OidCount {
                oid: oid.clone(),
                class,
                count: classes.values().sum(),
            });
        }
        for (named, &count) in &self.without_oid {
            let name = named.as_deref().unwrap_or(UNKNOWN);
            *class_counts.entry(name.to_owned()).or_default() += count;
        }

        let mut by_class: Vec<ClassCount> = class_counts
            .into_iter()
            .map(|(class, count)| ClassCount { class, count })
            .collect();
        // Both lists are in name order here, and the sorts are stable.
        by_class.sort_by_key(|row| Reverse(row.count));
        by_oid.sort_by_key(|row| Reverse(row.count));

        Report {
            total: self.total,
            by_class,
            by_oid,
        }
    }
}

/// Return the class of an oid: the class that its conflicts name most
/// often, the first name on a tie, or `unknown`.
fn oid_class(classes: &BTreeMap<Option<String>, u64>) -> String {
    classes
        .iter()
        .filter_map(|(named, &count)| named.as_ref().map(|name| (count, name)))
        .max_by(|a, b| a.0.cmp(&b.0).then_with(|| b.1.cmp(a.1)))
        .map_or_else(|| UNKNOWN.to_owned(), |(_, name)| name.clone())
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

    const SERIALS: &str = "serial this txn started with 0x0000000000000001 \
                           1900-01-01 00:00:00.000000, serial currently committed \
                           0x0000000000000002 1900-01-01 00:00:00.000000";

    fn conflict(oid: Option<&str>, class: Option<&str>) -> Conflict {
        Conflict {
            oid: oid.map(str::to_owned),
            class: class.map(str::to_owned),
        }
    }

    fn rank(conflicts: &[(Option<&str>, Option<&str>)]) -> Report {
        let mut ranking = Ranking::default();
        for &(oid, class) in conflicts {
            ranking.add(conflict(oid, class));
        }
        ranking.report()
    }

    #[test]
    fn a_write_conflict_gives_its_oid_and_class() {
        let line = format!(
            "ZODB.POSException.ConflictError: database conflict error \
             (oid 0x7a3c, class BTrees.OOBTree.OOBucket, {SERIALS})"
        );
        assert_eq!(
            parse_line(&line),
            Some(conflict(Some("0x7a3c"), Some("BTrees.OOBTree.OOBucket")))
        );
    }

    #[test]
    fn a_read_current_conflict_names_no_class() {
        let line =
            format!("ReadConflictError: database read conflict error (oid 0x7a3c, {SERIALS})");
        assert_eq!(parse_line(&line), Some(conflict(Some("0x7a3c"), None)));
    }

    #[test]
    fn a_read_conflict_with_a_class_counts_too() {
        let line =
            "database read conflict error (oid 0x10, class persistent.mapping.PersistentMapping)";
        assert_eq!(
            parse_line(line),
            Some(conflict(
                Some("0x10"),
                Some("persistent.mapping.PersistentMapping")
            ))
        );
    }

    #[test]
    fn a_retry_line_of_the_site_error_log_counts() {
        let line = format!(
            "1900-01-01 00:00:01,000 INFO    [Zope.SiteErrorLog:17][waitress-1] ConflictError \
             at /site/doc/edit: database conflict error (oid 0x01, class a.A, {SERIALS}). \
             Request will be retried."
        );
        assert_eq!(parse_line(&line), Some(conflict(Some("0x01"), Some("a.A"))));
    }

    #[test]
    fn a_conflict_without_an_oid_keeps_its_class() {
        let line = "database conflict error (class BTrees.Length.Length)";
        assert_eq!(
            parse_line(line),
            Some(conflict(None, Some("BTrees.Length.Length")))
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
    fn free_text_in_the_details_is_not_a_conflict() {
        assert_eq!(parse_line("database conflict error (see above)"), None);
        assert_eq!(
            parse_line("database conflict error (see log; user jdoe, class editors)"),
            None
        );
    }

    #[test]
    fn a_class_must_be_a_dotted_name() {
        assert_eq!(
            parse_line("database conflict error (oid 0xdead, class jdoe)"),
            None
        );
        assert_eq!(
            parse_line("database conflict error (oid 0xdead, class a..b)"),
            None
        );
    }

    #[test]
    fn an_oid_must_be_hexadecimal() {
        assert_eq!(
            parse_line("database conflict error (oid jdoe, class a.A)"),
            None
        );
        assert_eq!(
            parse_line("database conflict error (oid 0x, class a.A)"),
            None
        );
    }

    #[test]
    fn a_bad_marker_gives_way_to_a_good_one_later_in_the_line() {
        let line = "at /x/database conflict error (oid 0x1, class jdoe)/edit: \
                    database conflict error (oid 0x02, class a.A)";
        assert_eq!(parse_line(line), Some(conflict(Some("0x02"), Some("a.A"))));
    }

    #[test]
    fn unclosed_parentheses_are_not_a_conflict() {
        assert_eq!(
            parse_line("database conflict error (oid 0x01, class a.A"),
            None
        );
    }

    #[test]
    fn unnamed_conflicts_take_the_class_of_their_oid() {
        let report = rank(&[
            (Some("0x7a3c"), Some("b.OOBucket")),
            (Some("0x7a3c"), None),
            (Some("0x7a3c"), None),
            (Some("0x99"), Some("b.Length")),
            (Some("0x99"), Some("b.Length")),
        ]);
        assert_eq!(report.total, 5);
        assert_eq!(
            report.by_oid[0],
            OidCount {
                oid: "0x7a3c".into(),
                class: "b.OOBucket".into(),
                count: 3
            }
        );
        assert_eq!(
            report.by_class[0],
            ClassCount {
                class: "b.OOBucket".into(),
                count: 3
            }
        );
        assert_eq!(report.by_class.len(), 2);
    }

    #[test]
    fn an_oid_with_no_named_class_is_unknown() {
        let report = rank(&[(Some("0x01"), None)]);
        assert_eq!(report.by_oid[0].class, UNKNOWN);
        assert_eq!(report.by_class[0].class, UNKNOWN);
    }

    #[test]
    fn an_oid_named_with_two_classes_takes_the_most_frequent() {
        let report = rank(&[
            (Some("0x01"), Some("b.B")),
            (Some("0x01"), Some("a.A")),
            (Some("0x01"), Some("b.B")),
        ]);
        assert_eq!(report.by_oid.len(), 1);
        assert_eq!(report.by_oid[0].class, "b.B");
        assert_eq!(report.by_class.len(), 2);
    }

    #[test]
    fn conflicts_without_an_oid_count_in_the_classes_only() {
        let report = rank(&[(None, Some("a.A")), (None, None)]);
        assert_eq!(report.total, 2);
        assert!(report.by_oid.is_empty());
        let classes: Vec<_> = report.by_class.iter().map(|r| r.class.as_str()).collect();
        assert_eq!(classes, ["a.A", UNKNOWN]);
    }

    #[test]
    fn equal_counts_sort_by_name() {
        let report = rank(&[
            (Some("0x02"), Some("b.B")),
            (Some("0x01"), Some("a.A")),
            (Some("0x03"), Some("c.C")),
            (Some("0x03"), Some("c.C")),
        ]);
        let classes: Vec<_> = report.by_class.iter().map(|r| r.class.as_str()).collect();
        assert_eq!(classes, ["c.C", "a.A", "b.B"]);
        let oids: Vec<_> = report.by_oid.iter().map(|r| r.oid.as_str()).collect();
        assert_eq!(oids, ["0x03", "0x01", "0x02"]);
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
