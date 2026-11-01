"""Acceptance test of the spike on several transactions at once in one client.

A ZEO connection runs one transaction at a time. The spike compares two
designs for a client process that runs several transactions at the same
time: a pool of connections with one ordered stream of invalidations, and
several transactions on one connection. It writes the rule that keeps the
contract of ZODB for invalidations: `lastTransaction()` gives a tid only
after the DB got the invalidations of that tid. It writes the finding
`docs/spikes/concurrent-commits.md` and an ADR draft.

This test checks the finding:

- the sections `## The rule`, `## Evidence`, `## Cost`, `## Options` and
  `## Recommendation`, none of them empty. The rule names
  `lastTransaction` and the invalidations. The options name the pool and
  one connection;
- `## Evidence` names the race tests of ZODB, by the names of
  `ZODB.tests.racetest` with or without their prefix:
  `race_loadopen_vs_local_invalidate` and
  `race_load_vs_external_invalidate`. It names both Python builds (`3.13`,
  `3.14t`) and the number of runs, and gives one run with the rule turned
  off that fails them;
- `## Cost` has a table of the threads cells `add` and `update` at
  concurrency 16 and 64 on 3.14t, with a column for one connection, one for
  the pool without the rule and one for the pool with the rule;
- the ADR draft `docs/adr/NNNN-concurrent-transactions-in-a-client.md`
  exists with `Status: proposed`. The number is the next free one when the
  spike merges.

The test also checks two invariants of the spike: the runs on 3.14t use
the fork of `persistent`, and they name the modules that turned the GIL
back on.

`just test` runs it.
"""

import re
from pathlib import Path

from spike_findings import number, section, tables

ROOT = Path(__file__).resolve().parents[4]
FINDING = ROOT / "docs" / "spikes" / "concurrent-commits.md"
ADR_NAME = "concurrent-transactions-in-a-client.md"
SECTIONS = [
    "## The rule",
    "## Evidence",
    "## Cost",
    "## Options",
    "## Recommendation",
]
# The race tests of `ZODB.tests.racetest`. ZODB names the test methods
# `test_race_…` and their bodies `_check_race_…`.
RACE_TESTS = [
    "race_loadopen_vs_local_invalidate",
    "race_load_vs_external_invalidate",
]
COST_CELLS = [(scenario, c) for scenario in ("add", "update") for c in (16, 64)]


def column(header, *words, without=()):
    """Return the index of the column whose name holds every word."""
    for index, name in enumerate(header):
        if all(word in name for word in words) and not any(w in name for w in without):
            return index
    return None


def test_the_finding_has_every_section():
    text = FINDING.read_text()
    missing = [heading for heading in SECTIONS if heading not in text]
    assert not missing, missing
    for heading in SECTIONS:
        assert section(text, heading).strip(), f"the section {heading} is empty"


def test_the_rule_is_about_the_invalidations():
    rule = section(FINDING.read_text(), "## The rule")
    assert "invalidation" in rule.lower()
    assert "lastTransaction" in rule


def test_the_options_compare_a_pool_and_one_connection():
    options = section(FINDING.read_text(), "## Options").lower()
    assert "pool" in options
    assert "one connection" in options


def test_the_evidence_names_the_race_tests_the_builds_and_the_runs():
    evidence = section(FINDING.read_text(), "## Evidence")
    missing = [name for name in RACE_TESTS if name not in evidence]
    assert not missing, missing
    assert "3.13" in evidence
    assert "3.14t" in evidence
    assert re.search(r"\d[\d,]*\s+runs?\b", evidence), "no number of runs"


def test_the_evidence_gives_a_run_without_the_rule_that_fails():
    evidence = section(FINDING.read_text(), "## Evidence").lower()
    assert re.search(r"without the rule|rule (turned )?off", evidence), (
        "no run with the rule turned off"
    )
    assert "fail" in evidence


def test_the_evidence_follows_the_invariants_of_the_spike():
    evidence = section(FINDING.read_text(), "## Evidence")
    # The free-threaded runs use the fork of persistent.
    assert "persistent" in evidence
    # Each run on 3.14t names the modules that turned the GIL back on.
    assert "GIL" in evidence


def test_the_cost_compares_one_connection_and_the_pool_with_and_without_the_rule():
    cost = section(FINDING.read_text(), "## Cost")
    assert "3.14t" in cost
    assert "threads" in cost.lower()
    for header, rows in tables(cost):
        scenario = column(header, "scenario")
        concurrency = column(header, "concurrency")
        one = column(header, "one connection")
        without = column(header, "without the rule")
        with_rule = column(header, "with the rule", without=("without",))
        if None in (scenario, concurrency, one, without, with_rule):
            continue
        found = {}
        for row in rows:
            key = (row[scenario], int(number(row[concurrency])))
            found[key] = [number(row[i]) for i in (one, without, with_rule)]
        missing = [cell for cell in COST_CELLS if cell not in found]
        assert not missing, missing
        for cell in COST_CELLS:
            assert all(ops > 0 for ops in found[cell]), (cell, found[cell])
        return
    raise AssertionError(
        "no table in ## Cost with the columns scenario, concurrency, "
        "one connection, without the rule and with the rule"
    )


def test_the_adr_has_a_status():
    drafts = sorted((ROOT / "docs" / "adr").glob(f"[0-9][0-9][0-9][0-9]-{ADR_NAME}"))
    assert len(drafts) == 1, drafts
    # The draft takes a number after the ADRs that exist: 0031 or later.
    assert int(drafts[0].name[:4]) >= 31, drafts[0].name
    text = drafts[0].read_text()
    assert re.search(r"^Status: (proposed|accepted)", text, re.MULTILINE), "no status"
