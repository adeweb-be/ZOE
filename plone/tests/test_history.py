"""The merge policy of zoe.plone and its append-only workflow history, in
a Plone site (ADR 0046, section 9).

`just plone <scenario> history THREADS` builds the scenario, starts its
storage (and `zoe serve` for zoeclient), creates a Plone site and runs
plone/history.py in the instance. The script imports the profile
`zoe.plone:default`, whose import step derives the merge policy of the site
from its Dexterity types and writes it into the store; gives the type
Document the behaviour `zoe.plone.history` and runs the upgrade step; runs
THREADS transitions of one Document from one snapshot, which commit in
turn without a retry; then writes a policy with the Document class on and
lets two connections change two fields of the Document from one snapshot.
Its last line of stdout is a JSON report.

The scenarios: zoe (`zoe.Storage` in the instance) and zoeclient
(`zoe.zeo.ClientStorage` against `zoe serve`).

Run it inside `devenv shell`: `uv run --directory python pytest
../plone/tests/test_history.py`. The first build needs the network.
"""

import json
import subprocess
from pathlib import Path

import pytest

REPO = Path(__file__).resolve().parents[2]
THREADS = 8
DOCUMENT = "plone.app.contenttypes.content.Document"
HISTORY = "zoe.plone.history.WorkflowHistory"


def history(scenario):
    result = subprocess.run(
        ["just", "plone", scenario, "history", str(THREADS)],
        cwd=REPO,
        capture_output=True,
        text=True,
        check=False,
    )
    assert result.returncode == 0, result.stdout[-5000:] + result.stderr[-5000:]
    return json.loads(result.stdout.strip().splitlines()[-1])


@pytest.mark.parametrize("scenario", ["zoe", "zoeclient"])
def test_the_policy_and_the_history_of_zoe_plone(scenario):
    report = history(scenario)
    # The profile derived an entry for each class of the Dexterity types.
    assert DOCUMENT in report["classes"]
    assert HISTORY in report["classes"]
    assert report["document"]["tier2"] == "shadow"
    assert report["document"]["fields"] == {"modification_date": "max"}
    # The invariant of the publication behaviour couples its fields, under
    # their names and the names that they are stored under.
    assert ["effective", "effective_date", "expiration_date", "expires"] in report[
        "document"
    ]["coupled"]
    assert ["end", "open_end", "start"] in report["event"]["coupled"]
    # The upgrade step moved the history of the Document from before the
    # behaviour, and the new Document got its history when it was added.
    assert report["moved"] == 1
    assert report["before"] == HISTORY
    assert report["before_entries"] == 1
    assert report["after"] == HISTORY
    # The transitions from one snapshot all committed, with every status.
    assert report["conflicts"] == 0
    assert report["committed"] == THREADS
    assert report["entries"] == THREADS + 1
    # Two changes of two fields of one Document both committed.
    assert report["fields"] == ["committed", "committed"]
    assert (report["title"], report["description"]) == ("After, edited", "two")
