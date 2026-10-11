"""Acceptance test: 0 conflicts on id allocation with 50 concurrent creators.

This is an item of the phase 2 gate (F29). `zoe.plone` wires the id
counters of Plone to the sequences of the store: the `_nextwid` of the
ZCTextIndex lexicons, the ids of the intid utility, and the `Length`
counters that only hand out ids. Each takes its next value from
`storage.sequence(name).next()`, outside the two-phase commit, so two
creators never write the same counter object.

`just plone <scenario> creators` builds the scenario, starts its storage
(and `zoe serve` for zeoclient), creates a Plone site, then runs 50
threads in the instance, each with its own connection, that create
`ITEMS` documents each with new words in their text, retrying a
transaction on `ConflictError` as Zope does. It prints a JSON report as
its last line of stdout:

    {"created": <documents committed>,
     "raised": {"<module.Class>": <ConflictErrors raised on that class>}}

`just plone <scenario> creators-stock` runs the same load with the wiring
of `zoe.plone` off, which shows that the load does meet these conflicts.

Run it inside `devenv shell`: `uv run --directory python pytest
../plone/tests/test_id_allocation.py`.
"""

import json
import subprocess
from pathlib import Path

import pytest

REPO = Path(__file__).resolve().parents[2]
THREADS = 50
ITEMS = 4

# The classes whose objects only hand out ids.
ID_ALLOCATION = {
    "Products.ZCTextIndex.Lexicon.Lexicon",
    "five.intid.intid.IntIds",
    "zope.intid.IntIds",
}


def creators(scenario, command):
    result = subprocess.run(
        ["just", "plone", scenario, command],
        cwd=REPO,
        capture_output=True,
        text=True,
        check=False,
    )
    assert result.returncode == 0, result.stdout[-5000:] + result.stderr[-5000:]
    return json.loads(result.stdout.strip().splitlines()[-1])


@pytest.mark.parametrize("scenario", ["zeoclient", "zoe"])
def test_no_conflict_on_id_allocation_with_50_creators(scenario):
    report = creators(scenario, "creators")
    assert report["created"] == THREADS * ITEMS
    raised = {cls: n for cls, n in report["raised"].items() if cls in ID_ALLOCATION}
    assert raised == {}, raised


def test_without_the_wiring_the_load_meets_these_conflicts():
    report = creators("zeoclient", "creators-stock")
    assert report["created"] == THREADS * ITEMS
    raised = sum(n for cls, n in report["raised"].items() if cls in ID_ALLOCATION)
    assert raised > 0, "the load is too light to show the conflicts"
