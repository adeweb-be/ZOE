"""Acceptance test of the spike on a write-ahead log with a background
checkpoint.

The spike measures a prototype of the write path where a commit appends
its records to a log and syncs only the log, and a checkpoint folds many
commits at once into the `(oid, tid)` tree. It writes the finding
`docs/spikes/write-log.md`.

This test checks the finding:

- the sections `## Verdict`, `## Method`, `## The commit`,
  `## The checkpoint`, `## The reads`, `## Recovery`,
  `## The rest of the store` and `## Cards`, none of them empty;
- `## The commit` has a table with the columns `scenario`, `object size`,
  `concurrency`, `layout`, `bytes per commit`, `finish p50` and
  `finish p99`, whose `layout` column holds `main`, `index` and `log`,
  with `add` and `update` rows, at 1 KB and 100 KB, at concurrency 1, 16
  and 64, for `main` and `log`;
- `## The checkpoint` gives its throughput and its write amplification;
- `## The reads` gives the cost of a load with a table of 0, 10k and 1M
  records;
- `## Recovery` gives the replay time per MB of log;
- `## Cards` names at least one card and the rows it moves;
- the verdict is go, no-go, or go with conditions.

`just test` runs it.
"""

import re
from pathlib import Path

ROOT = Path(__file__).resolve().parents[4]
FINDING = ROOT / "docs" / "spikes" / "write-log.md"
SECTIONS = [
    "## Verdict",
    "## Method",
    "## The commit",
    "## The checkpoint",
    "## The reads",
    "## Recovery",
    "## The rest of the store",
    "## Cards",
]
COMMIT_COLUMNS = [
    "scenario",
    "object size",
    "concurrency",
    "layout",
    "bytes per commit",
    "finish p50",
    "finish p99",
]


def section(text, heading):
    """Return the body of the section `heading`, up to the next section."""
    assert heading in text, f"no section {heading!r}"
    return text.split(heading, 1)[1].split("\n## ", 1)[0]


def cells_of(line):
    return [
        cell.strip().strip("`").strip() for cell in line.strip().strip("|").split("|")
    ]


def tables(text):
    """Return each table of text as a header and a list of rows."""
    found = []
    lines = text.splitlines()
    index = 0
    while index < len(lines):
        if not lines[index].strip().startswith("|"):
            index += 1
            continue
        block = []
        while index < len(lines) and lines[index].strip().startswith("|"):
            block.append(cells_of(lines[index]))
            index += 1
        header, rows = block[0], block[1:]
        rows = [row for row in rows if not set("".join(row)) <= set("-: ")]
        found.append(([cell.lower() for cell in header], rows))
    return found


def commit_table():
    text = section(FINDING.read_text(), "## The commit")
    for header, rows in tables(text):
        if all(name in header for name in COMMIT_COLUMNS):
            return header, rows
    raise AssertionError(f"no commit table with the columns {COMMIT_COLUMNS}")


def number(text):
    return float(re.sub(r"[^0-9.]", "", text))


def test_the_finding_has_every_section():
    text = FINDING.read_text()
    missing = [heading for heading in SECTIONS if heading not in text]
    assert not missing, missing
    for heading in SECTIONS:
        assert section(text, heading).strip(), f"the section {heading} is empty"


def test_the_verdict_is_go_no_go_or_go_with_conditions():
    verdict = section(FINDING.read_text(), "## Verdict").lower()
    assert re.search(r"\bno-go\b|\bgo with conditions\b|\bgo\b", verdict)


def test_the_commit_table_has_every_row():
    header, rows = commit_table()
    columns = {name: header.index(name) for name in COMMIT_COLUMNS}
    layouts = {row[columns["layout"]] for row in rows}
    assert {"main", "index", "log"} <= layouts, layouts
    for layout in ("main", "log"):
        for scenario in ("add", "update"):
            for size in ("1 KB", "100 KB"):
                for concurrency in ("1", "16", "64"):
                    matching = [
                        row
                        for row in rows
                        if row[columns["layout"]] == layout
                        and row[columns["scenario"]] == scenario
                        and row[columns["object size"]] == size
                        and row[columns["concurrency"]] == concurrency
                    ]
                    cell = f"{layout} {scenario} {size} c={concurrency}"
                    assert matching, f"no row {cell}"
                    row = matching[0]
                    for name in ("bytes per commit", "finish p50", "finish p99"):
                        assert number(row[columns[name]]) > 0, (row, name)


def test_the_checkpoint_gives_its_throughput_and_amplification():
    text = section(FINDING.read_text(), "## The checkpoint").lower()
    assert re.search(r"records (per|/) ?s", text) or "records per second" in text
    assert "amplification" in text


def test_the_reads_cover_three_table_sizes():
    text = section(FINDING.read_text(), "## The reads")
    for size in ("0", "10,000", "1,000,000"):
        assert re.search(rf"(^|[^\d,]){re.escape(size)}([^\d,]|$)", text), size


def test_recovery_gives_the_replay_time_per_mb():
    text = section(FINDING.read_text(), "## Recovery").lower()
    assert re.search(r"per mb|/ ?mb", text)


def test_the_cards_name_the_rows_they_move():
    text = section(FINDING.read_text(), "## Cards")
    assert re.search(r"\bcard\b", text, re.IGNORECASE)
    assert re.search(r"\bmoves?\b", text, re.IGNORECASE)
