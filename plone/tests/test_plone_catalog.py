"""Acceptance test: the Plone test suites in the mode `compare` of the
catalog backend of zoe.plone find no mismatch.

`just plone zoe catalog` runs the suites of `just plone zoe suites` with
the backend `catalog` in the mode `compare` (ADR 0059, sections 7 and 9).
The suites run on test storages, so each search builds a throwaway mirror
of what its Connection sees, compares its answer with the stock search,
and returns the stock result. The recipe prints the counts by path, by
reason of the Python path and of the mismatches, and fails on a mismatch.

Run it inside `devenv shell`: `just plone test
plone/tests/test_plone_catalog.py`. It takes the time of the suites.
"""

import re
import subprocess
from pathlib import Path

from test_plone_suites import FLOOR, SUMMARY

REPO = Path(__file__).resolve().parents[2]
COUNT = re.compile(r"^catalog: (?P<name>[a-z_]+): (?P<n>\d+)$", re.MULTILINE)


def test_the_suites_in_the_mode_compare_find_no_mismatch():
    result = subprocess.run(
        ["just", "plone", "zoe", "catalog"],
        cwd=REPO,
        capture_output=True,
        text=True,
        check=False,
    )
    output = result.stdout + result.stderr
    totals = SUMMARY.findall(output)
    assert totals, output[-5000:]
    tests, failures, errors = (int(n) for n in totals[-1])
    assert (failures, errors) == (0, 0), output[-20000:]
    assert tests >= FLOOR
    counts = {m["name"]: int(m["n"]) for m in COUNT.finditer(output)}
    assert counts.get("mismatches") == 0, output[-5000:]
    # The suites run on test storages: each comparison went through a
    # throwaway mirror.
    assert counts["compared"] > 0
    assert counts["throwaways"] >= counts["compared"]
    assert "catalog: path native" in output
    assert result.returncode == 0, output[-5000:]
