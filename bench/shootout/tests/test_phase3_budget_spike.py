"""Acceptance test of the spike on the budget of each cell of the phase 3 gate.

The phase 3 gate of N4 asks `zoe serve` through the native client for at
least 3× ZEO 6 through `ZEO.ClientStorage`, in every scenario. Before the
native client is built, the spike runs zodbshootout on the reference
machine, the full set, on CPython 3.13 and 3.14t: `zeo` through
`ZEO.ClientStorage`, and `zoe serve` through the proof-of-concept client.
It splits the time of one transaction of each cell into its terms, and
writes the finding `docs/spikes/phase3-budget.md`.

This test checks the finding:

- a table with one row per scenario, cell of the full set and Python build
  (`3.13`, `3.14t`), with the columns `scenario`, `mode`, `concurrency`,
  `object size`, `python`, `zeo ops/s`, `poc ops/s`, `ratio` and `term`,
  where the ratio is the quotient of the two numbers within 5 %, and each
  row under 3× names its term;
- a section `## Method`: the governor and the frequency of the CPU, the
  cores of the server and of the clients, the rounds and their spread;
- a section `## Floors`: the fsync of the reference disk, the round trip,
  the client CPU per object of each client and ZODB per object;
- a section `## The client cache`: the share of `ZEO.cache` in a
  `read_hot` hit and the cost of a hit that 3× allows;
- a section `## Cells under 3×`: each cell under 3× in the table names the
  card that moves it, or says why no card can;
- a section `## Server work`: the server changes that phase 3 needs, each
  with the cells that it moves.

The test also checks two invariants of the method: the rounds are
interleaved, and the finding names the modules that turned the GIL back on.

`just bench test` runs it.
"""

import re
from pathlib import Path

FINDING = Path(__file__).resolve().parents[3] / "docs" / "spikes" / "phase3-budget.md"
SCENARIOS = ["add", "update", "read_cold", "read_warm", "read_hot"]
PYTHONS = ["3.13", "3.14t"]
# The cells of the full set where ZEO runs.
CELLS = [
    (mode, concurrency, size)
    for mode, concurrencies in (("threads", (1, 4, 16, 64)), ("processes", (4, 16, 64)))
    for concurrency in concurrencies
    for size in (1024, 102400)
]
COLUMNS = [
    "scenario",
    "mode",
    "concurrency",
    "object size",
    "python",
    "zeo ops/s",
    "poc ops/s",
    "ratio",
    "term",
]
GATE = 3.0
# A reference to the card that moves a cell, or the words that say that no
# card can.
CARD = re.compile(r"ZOE-\d+|#\d+|new card|no card", re.IGNORECASE)
# A time per transaction, per object or per hit. The micro sign and the
# Greek mu look the same.
TIME = re.compile(r"\d\s*(µs|μs|us|ms|ns)\b")


def cells_of(line):
    return [
        cell.strip().strip("`").strip() for cell in line.strip().strip("|").split("|")
    ]


def table_rows(text):
    """Return the rows of every table with the columns of COLUMNS.

    Each row is a dict from the column name to the text of its cell.
    """
    rows = []
    lines = text.splitlines()
    for index, line in enumerate(lines):
        if [cell.lower() for cell in cells_of(line)] != COLUMNS:
            continue
        for row in lines[index + 1 :]:
            if not row.strip().startswith("|"):
                break
            cells = cells_of(row)
            if len(cells) == len(COLUMNS) and not set("".join(cells)) <= set("-: "):
                rows.append(dict(zip(COLUMNS, cells, strict=True)))
    return rows


def number(text):
    return float(re.sub(r"[^0-9.]", "", text))


def size_of(text):
    """Return an object size in bytes, from `1 KB`, `100 KB` or `1024`."""
    value = number(text)
    if "kb" in text.lower() or "kib" in text.lower():
        value *= 1024
    return int(value)


def section(text, heading):
    """Return the body of the section `heading`, up to the next section."""
    assert heading in text, f"no section {heading!r}"
    return text.split(heading, 1)[1].split("\n## ", 1)[0]


def test_the_finding_has_a_row_for_every_cell_scenario_and_python_build():
    rows = table_rows(FINDING.read_text())
    assert rows, f"no table with the columns {COLUMNS}"
    found = {
        (
            row["scenario"],
            row["mode"],
            int(number(row["concurrency"])),
            size_of(row["object size"]),
            row["python"],
        )
        for row in rows
    }
    expected = {
        (scenario, mode, concurrency, size, python)
        for scenario in SCENARIOS
        for mode, concurrency, size in CELLS
        for python in PYTHONS
    }
    assert expected <= found, sorted(expected - found)


def test_each_ratio_is_the_quotient_and_each_cell_under_3x_names_its_term():
    rows = table_rows(FINDING.read_text())
    assert rows, f"no table with the columns {COLUMNS}"
    for row in rows:
        zeo, poc = number(row["zeo ops/s"]), number(row["poc ops/s"])
        ratio = number(row["ratio"])
        assert zeo > 0 and poc > 0, row
        assert abs(ratio - poc / zeo) <= 0.05 * ratio, row
        if ratio < GATE:
            assert row["term"] not in ("", "-", "—"), row


def test_the_method_gives_the_machine_and_the_rounds():
    method = section(FINDING.read_text(), "## Method").lower()
    missing = [
        term
        for term in ("governor", "frequency", "cores", "server", "client", "rounds")
        if term not in method
    ]
    assert not missing, missing
    assert "spread" in method


def test_the_method_follows_the_invariants_of_the_spike():
    method = section(FINDING.read_text(), "## Method")
    # The rounds alternate between the two clients.
    assert "interleaved" in method.lower()
    # Each run on 3.14t names the modules that turned the GIL back on.
    assert "GIL" in method


def test_the_floors_give_each_term_with_a_number():
    floors = section(FINDING.read_text(), "## Floors")
    lower = floors.lower()
    missing = [
        term
        for term in ("fsync", "round trip", "client cpu", "per object", "zodb")
        if term not in lower
    ]
    assert not missing, missing
    # The client CPU per object of each client.
    assert "zeo.clientstorage" in lower
    assert "poc" in lower
    assert TIME.search(floors), "no time in the floors"


def test_the_client_cache_gives_the_share_of_zeo_cache_and_the_cost_of_a_hit():
    cache = section(FINDING.read_text(), "## The client cache")
    assert "ZEO.cache" in cache
    assert "read_hot" in cache
    assert re.search(r"\d\s*%", cache), "no share of ZEO.cache"
    assert TIME.search(cache), "no cost of a hit"


def test_each_cell_under_3x_names_the_card_that_moves_it():
    text = FINDING.read_text()
    match = re.search(r"^## Cells under 3[×x]\s*$", text, re.MULTILINE)
    assert match, "no section ## Cells under 3×"
    under = text[match.end() :].split("\n## ", 1)[0]
    lines = under.splitlines()
    for row in table_rows(text):
        if number(row["ratio"]) >= GATE:
            continue
        named = [line for line in lines if row["scenario"] in line]
        assert named, f"{row['scenario']} is under 3× and the section does not name it"
        assert any(CARD.search(line) for line in named), row


def test_the_server_work_names_the_cells_that_it_moves():
    work = section(FINDING.read_text(), "## Server work")
    assert work.strip(), "the section ## Server work is empty"
    assert any(scenario in work for scenario in SCENARIOS) or "none" in work.lower()
