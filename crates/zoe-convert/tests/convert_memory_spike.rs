//! Acceptance test of the spike on the memory of a large conversion (F14).
//!
//! A conversion of a production copy of 6,064,442 records peaked at about
//! 3.2 GiB of resident memory, 2.2 GiB above the default page cache of
//! redb (1 GiB). The spike finds what holds that memory, and writes the
//! finding `docs/spikes/convert-memory.md`.
//!
//! This test checks the finding:
//!
//! - a table with the columns `source`, `records`, `store MiB`,
//!   `page cache MiB` and `peak RSS MiB`, all numbers but the source;
//! - at least two rows of one synthetic source with more records than the
//!   production copy and a store larger than the page cache: one with the
//!   default page cache of 1024 MiB, one with a smaller page cache;
//! - a section `## Method` that names the heap profiler;
//! - a section `## Where the memory goes`, from the heap profile, with the
//!   pass of the conversion (copy, resume, verification) that holds each
//!   part;
//! - a section `## Verdict`: either the memory of a conversion is bounded
//!   by the page cache plus a fixed amount, or a build card bounds the
//!   structure that grows with the records.

use std::path::PathBuf;

/// The records of the production copy.
const PRODUCTION_RECORDS: u64 = 6_064_442;

/// The default page cache of redb, in MiB.
const DEFAULT_CACHE_MIB: f64 = 1024.0;

fn finding() -> String {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/spikes/convert-memory.md");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// One row of the table of the finding.
#[derive(Debug)]
struct Row {
    source: String,
    records: u64,
    store_mib: f64,
    cache_mib: f64,
    peak_mib: f64,
}

fn number(cell: &str) -> Option<f64> {
    let digits: String = cell
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    digits.parse().ok()
}

/// Return the rows of the tables of `text` that have five cells, with a
/// number in each cell but the first.
fn rows(text: &str) -> Vec<Row> {
    let mut rows = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if !line.starts_with('|') {
            continue;
        }
        let cells: Vec<&str> = line.trim_matches('|').split('|').map(str::trim).collect();
        if cells.len() != 5 {
            continue;
        }
        let numbers: Option<Vec<f64>> = cells[1..].iter().copied().map(number).collect();
        if let Some(numbers) = numbers {
            rows.push(Row {
                source: cells[0].to_owned(),
                records: numbers[0] as u64,
                store_mib: numbers[1],
                cache_mib: numbers[2],
                peak_mib: numbers[3],
            });
        }
    }
    rows
}

#[test]
fn the_finding_has_the_table_of_peaks() {
    let text = finding();
    assert!(
        text.contains("| source | records | store MiB | page cache MiB | peak RSS MiB |"),
        "no table with the five columns"
    );
    let rows = rows(&text);
    let large: Vec<&Row> = rows
        .iter()
        .filter(|row| {
            row.records > PRODUCTION_RECORDS
                && row.store_mib > DEFAULT_CACHE_MIB
                && row.source.contains("synthetic")
        })
        .collect();
    assert!(
        large
            .iter()
            .any(|row| (row.cache_mib - DEFAULT_CACHE_MIB).abs() < 0.5),
        "no row of a large synthetic source with the default page cache: {rows:?}"
    );
    assert!(
        large.iter().any(|row| row.cache_mib < DEFAULT_CACHE_MIB),
        "no row of a large synthetic source with a smaller page cache: {rows:?}"
    );
    assert!(
        large.iter().all(|row| row.peak_mib > 0.0),
        "a peak is not measured: {rows:?}"
    );
}

#[test]
fn the_finding_explains_the_memory_and_gives_a_verdict() {
    let text = finding();
    for heading in ["## Method", "## Where the memory goes", "## Verdict"] {
        assert!(text.contains(heading), "no section {heading}");
    }
    let method = section(&text, "## Method").to_lowercase();
    assert!(
        method.contains("heap profil"),
        "the method names no heap profiler"
    );
    let memory = section(&text, "## Where the memory goes").to_lowercase();
    for pass in ["copy", "verification"] {
        assert!(
            memory.contains(pass),
            "the memory of the {pass} pass is not explained"
        );
    }
    let verdict = section(&text, "## Verdict").to_lowercase();
    assert!(
        verdict.contains("bounded") || verdict.contains("build card"),
        "the verdict says neither that the memory is bounded nor which build card bounds it"
    );
}

/// Return the text of the section under `heading`, up to the next
/// heading of the same level.
fn section<'a>(text: &'a str, heading: &str) -> &'a str {
    let start = text.find(heading).unwrap() + heading.len();
    let rest = &text[start..];
    let end = rest.find("\n## ").unwrap_or(rest.len());
    &rest[..end]
}
