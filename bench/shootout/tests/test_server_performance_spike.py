"""Acceptance test of the spike on the performance of `zoe serve`.

The spike measures `zoe-server` against `zeo` with zodbshootout on the
reference machine, the full set, and with `zoe-bench saturate` for the p99
of `loadBefore` (N3, N4). It profiles the server in each scenario where the
ratio is under 3, and writes the finding `docs/spikes/server-performance.md`.

This test checks the finding:

- a table with one row per scenario and cell of the full set, with the
  columns `scenario`, `mode`, `concurrency`, `object size`, `zeo ops/s`,
  `zoe-server ops/s` and `ratio`, where the ratio is the quotient of the two
  numbers;
- a section `## Where the time goes`, from the profiles;
- a section `## Remedies`: each remedy names the cells that it moves and
  the gain measured on a prototype or estimated from the profile. The list
  weighs at least the leads that the reviews of phase 1 left: compression
  at the vote instead of under the commit sequencer, a check of the
  checksum alone where only the tid or the size is needed, a vote that
  reads only the tid of a read-current oid, one zstd context per thread,
  and fewer pages per commit in redb.
- the p99 of `loadBefore` under load, against the 0.3 ms of N3.
"""

import re
from pathlib import Path

FINDING = (
    Path(__file__).resolve().parents[3] / "docs" / "spikes" / "server-performance.md"
)
SCENARIOS = ["add", "update", "read_cold", "read_warm", "read_hot"]
# The cells of the full set where zoe-server runs: those of ZEO.
CELLS = [
    (mode, concurrency, size)
    for mode, concurrencies in (("threads", (1, 4, 16, 64)), ("processes", (4, 16, 64)))
    for concurrency in concurrencies
    for size in (1024, 102400)
]
LEADS = [
    "compression at the vote",
    "checksum alone",
    "read-current",
    "zstd context",
    "pages per commit",
]


def table_rows(text):
    rows = []
    for line in text.splitlines():
        cells = [cell.strip() for cell in line.strip().strip("|").split("|")]
        if len(cells) == 7 and cells[0] in SCENARIOS:
            rows.append(cells)
    return rows


def number(text):
    return float(re.sub(r"[^0-9.]", "", text))


def test_the_finding_has_a_row_for_every_cell_and_scenario():
    text = FINDING.read_text()
    rows = table_rows(text)
    found = {(row[0], row[1], int(number(row[2])), int(number(row[3]))) for row in rows}
    expected = {(s, mode, c, size) for s in SCENARIOS for mode, c, size in CELLS}
    assert expected <= found, sorted(expected - found)
    for row in rows:
        zeo, zoe, ratio = number(row[4]), number(row[5]), number(row[6])
        assert zeo > 0 and zoe > 0
        assert abs(ratio - zoe / zeo) <= 0.05 * ratio, row


def test_the_finding_explains_and_ranks_the_remedies():
    text = FINDING.read_text()
    assert "## Where the time goes" in text
    assert "## Remedies" in text
    remedies = text.split("## Remedies", 1)[1].lower()
    missing = [lead for lead in LEADS if lead not in remedies]
    assert not missing, missing


def test_the_finding_gives_the_p99_of_load_before():
    text = FINDING.read_text()
    assert re.search(r"p99 of `?loadBefore`?.*\d", text), "no p99 of loadBefore"
    assert "0.3 ms" in text
