"""Acceptance test: 0 conflicts on id allocation with 50 concurrent creators.

This is an item of the phase 2 gate (F29). In Plone 6.1, the one shared
counter that new content writes is `portal_uidgenerator`, the
`UniqueIdGeneratorTool` of Products.CMFUid. Its `__call__` adds 1 to the
int `_uid_counter` of the tool. CMFEditions calls it, through
`portal_historyidhandler`, when it saves the first version of a new
Document. So two creators that commit at the same time write the same
tool, and one of them gets a `ConflictError`.

`zoe.plone` takes the ids of the tool from `storage.sequence(name).next()`,
outside the two-phase commit, on `zoe.Storage` and `zoe.zeo.ClientStorage`
only. The ids stay unique and stay above the `_uid_counter` of the site.

`just plone <scenario> creators THREADS ITEMS SEED` builds the scenario,
starts its storage (and `zoe serve` for zeoclient and zoeclient), and
creates a Plone site. It sets `_uid_counter` of `portal_uidgenerator` to
SEED and commits: the site has given ids up to SEED. Then it runs THREADS
threads in the instance, each with its own connection. Each thread
creates ITEMS Documents in one folder, one transaction for each Document,
and retries a transaction on `ConflictError` until it commits. The recipe
stops the server that it started. It prints a JSON report as its last
line of stdout:

    {"created": <Documents committed>,
     "seed": <_uid_counter of the tool before the load>,
     "history_ids": [<history id of each Document that it created>],
     "raised": {"<module.Class>": <ConflictErrors raised on that class>}}

The scenarios:

- zoe: `zoe.Storage` in the instance, with `zoedb[plone]`.
- zoeclient: `zoe.zeo.ClientStorage` (`<zoeclient>`) against `zoe serve`,
  with `zoedb[plone]` in the instance.
- zeoclient: stock Plone with the stock ZEO client against `zoe serve`,
  without zoedb. It shows that the load meets these conflicts.

Run it inside `devenv shell`: `uv run --directory python pytest
../plone/tests/test_id_allocation.py`. The first build needs the network.
"""

import json
import subprocess
from pathlib import Path

import pytest

REPO = Path(__file__).resolve().parents[2]
THREADS = 50
ITEMS = 4
SEED = 100_000

# The one shared id counter that new content writes in Plone 6.1.
GENERATOR = "Products.CMFUid.UniqueIdGeneratorTool.UniqueIdGeneratorTool"


def creators(scenario):
    result = subprocess.run(
        [
            "just",
            "plone",
            scenario,
            "creators",
            str(THREADS),
            str(ITEMS),
            str(SEED),
        ],
        cwd=REPO,
        capture_output=True,
        text=True,
        check=False,
    )
    assert result.returncode == 0, result.stdout[-5000:] + result.stderr[-5000:]
    return json.loads(result.stdout.strip().splitlines()[-1])


def assert_ids_are_unique_and_above_the_seed(report):
    ids = report["history_ids"]
    assert len(ids) == THREADS * ITEMS
    assert len(set(ids)) == len(ids), "two Documents have the same history id"
    assert report["seed"] == SEED
    assert min(ids) > SEED, "an id is not above the ids that the site gave"


@pytest.mark.parametrize("scenario", ["zoeclient", "zoe"])
def test_no_conflict_on_portal_uidgenerator_with_50_creators(scenario):
    report = creators(scenario)
    assert report["created"] == THREADS * ITEMS
    assert report["raised"].get(GENERATOR, 0) == 0, report["raised"]
    assert_ids_are_unique_and_above_the_seed(report)


def test_stock_plone_meets_conflicts_on_portal_uidgenerator():
    report = creators("zeoclient")
    assert report["created"] == THREADS * ITEMS
    assert report["raised"].get(GENERATOR, 0) > 0, (
        "the load is too light to show the conflicts"
    )
    assert_ids_are_unique_and_above_the_seed(report)
