"""Acceptance test: the Plone test suites pass with `ZoeTextIndex` in the
mode `compat` in the place of ZCTextIndex.

`just plone zoe catalog text` runs the suites of `just plone zoe catalog`
with the three ZCTextIndex indexes of each site, `SearchableText`, `Title`
and `Description`, replaced by `ZoeTextIndex` indexes in the mode `compat`
(ADR 0060, section 11). The suites run on test storages, so each text
query sends the texts of the stack to the ZOE storage below. Exactly the
tests of `KNOWN` fail: they check that the class name of the index is
`ZCTextIndex`. No other test fails or errors. The recipe then exits with
1, so the test reads the summary lines and not the exit status. The recipe
writes the texts of the suites and their query log in
`target/plone/zoe/var/catalog-text`, for the replay of the text queries.

Run it inside `devenv shell`: `just plone test
plone/tests/test_plone_catalog_text.py`. It takes the time of the suites.
"""

import json
import re
import subprocess
from pathlib import Path

from test_plone_suites import FLOOR, SUMMARY

REPO = Path(__file__).resolve().parents[2]
COUNT = re.compile(r"^catalog: (?P<name>[a-z_]+): (?P<n>\d+)$", re.MULTILINE)
LOG = REPO / "target" / "plone" / "zoe" / "var" / "catalog-text"
# A test that fails or errors, as zope.testrunner reports it.
FAILED = re.compile(r"^(?:Failure|Error) in test \S+ \((?P<id>[^)]+)\)$", re.MULTILINE)
# The tests that fail with `ZoeTextIndex`.
KNOWN = {
    f"Products.CMFPlone.tests.testCatalogTool.TestCatalogSetup.test{name}IsZCTextIndex"
    for name in ("SearchableText", "Description", "Title")
}


def test_the_suites_pass_with_zoe_text_index_in_the_mode_compat():
    result = subprocess.run(
        ["just", "plone", "zoe", "catalog", "text"],
        cwd=REPO,
        capture_output=True,
        text=True,
        check=False,
    )
    output = result.stdout + result.stderr
    totals = SUMMARY.findall(output)
    assert totals, output[-5000:]
    tests, failures, errors = (int(n) for n in totals[-1])
    assert set(FAILED.findall(output)) == KNOWN, output[-20000:]
    assert (failures, errors) == (len(KNOWN), 0), output[-20000:]
    assert tests >= FLOOR
    counts = {m["name"]: int(m["n"]) for m in COUNT.finditer(output)}
    assert counts.get("mismatches") == 0, output[-5000:]
    # The suites searched the `ZoeTextIndex` indexes, and the indexes
    # staged the texts of the suites.
    classes = set()
    for path in LOG.glob("catalog-*.jsonl"):
        for line in path.read_text().splitlines():
            found = json.loads(line)
            classes.update((found.get("indexes") or {}).values())
    assert "ZoeTextIndex" in classes
    texts = [p for p in LOG.glob("texts-*.jsonl") if p.stat().st_size]
    assert texts
