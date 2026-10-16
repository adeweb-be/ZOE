"""Acceptance test of the spike on the cells of one call at a time.

In some cells of the phase 3 gate, each worker has one connection and
waits for each reply: the threads cells at concurrency 1, and the
processes cells at concurrency 16 and 1 KB. There the time of one call
decides. The spike splits the time of one call of `zoe serve` through the
native client and of ZEO into its terms, measures the floor of the
harness, finds why `read_warm` gains least, and ranks the remedies. It
writes the finding `docs/spikes/call-latency.md`.

This test checks the finding:

- a row in `docs/spikes/README.md`;
- the sections `## Verdict`, `## Method`, `## The time of one call`,
  `## The floor`, `## read_warm` and `## Cards`;
- in `## The time of one call`, a table with the columns `scenario`,
  `storage`, `term` and `µs`, with rows for `zoe-native` and for `zeo`;
- in `## Cards`, each proposed card names the cells that it moves.

`just bench test` runs it.
"""

import re
from pathlib import Path

SPIKES = Path(__file__).resolve().parents[3] / "docs" / "spikes"
FINDING = SPIKES / "call-latency.md"
SECTIONS = [
    "## Verdict",
    "## Method",
    "## The time of one call",
    "## The floor",
    "## read_warm",
    "## Cards",
]
COLUMNS = ["scenario", "storage", "term", "µs"]
SCENARIOS = ["add", "update", "read_cold", "read_warm", "read_hot"]
# A cell of the gate set: a mode and a concurrency, as `threads 1` or
# `processes 16`.
CELL = re.compile(r"(threads|processes)\s+(c\s*)?\d+", re.IGNORECASE)


def cells_of(line):
    return [
        cell.strip().strip("`").strip() for cell in line.strip().strip("|").split("|")
    ]


def table_rows(text, columns):
    """Return the rows of every table with the columns `columns`.

    Each row is a dict from the column name to the text of its cell. The
    micro sign and the Greek mu look the same, so a header can use either.
    """
    wanted = [column.replace("μ", "µ") for column in columns]
    rows = []
    lines = text.splitlines()
    for index, line in enumerate(lines):
        header = [cell.lower().replace("μ", "µ") for cell in cells_of(line)]
        if header != wanted:
            continue
        for row in lines[index + 1 :]:
            if not row.strip().startswith("|"):
                break
            cells = cells_of(row)
            if len(cells) == len(columns) and not set("".join(cells)) <= set("-: "):
                rows.append(dict(zip(columns, cells, strict=True)))
    return rows


def section(text, heading):
    """Return the body of the section `heading`, up to the next section."""
    match = re.search(rf"^{re.escape(heading)}\s*$", text, re.MULTILINE)
    assert match, f"no section {heading!r}"
    return text[match.end() :].split("\n## ", 1)[0]


def test_the_finding_exists_and_has_a_row_in_the_index():
    assert FINDING.is_file(), f"no finding {FINDING}"
    index = (SPIKES / "README.md").read_text()
    assert "(call-latency.md)" in index, "no row for call-latency.md in the index"


def test_the_finding_has_every_section():
    text = FINDING.read_text()
    for heading in SECTIONS:
        assert section(text, heading).strip(), f"the section {heading!r} is empty"


def test_the_time_of_one_call_is_a_table_of_terms_for_both_storages():
    body = section(FINDING.read_text(), "## The time of one call")
    rows = table_rows(body, COLUMNS)
    assert rows, f"no table with the columns {COLUMNS}"
    storages = {row["storage"] for row in rows}
    assert {"zoe-native", "zeo"} <= storages, storages
    for row in rows:
        assert row["scenario"] in SCENARIOS, row
        assert row["term"] not in ("", "-", "—"), row
        assert re.search(r"\d", row["µs"]), row


def test_each_proposed_card_names_the_cells_that_it_moves():
    cards = section(FINDING.read_text(), "## Cards")
    # Each card is a heading or an item of a list.
    entries = re.split(r"\n(?=###\s|[-*]\s|\d+\.\s)", cards)
    proposed = [entry for entry in entries if re.match(r"(###\s|[-*]\s|\d+\.\s)", entry)]
    assert proposed, "the section ## Cards proposes no card"
    for entry in proposed:
        assert any(scenario in entry for scenario in SCENARIOS), entry
        assert CELL.search(entry), entry
