"""Acceptance test: the Plone test suites pass in both modes.

`just plone <scenario> suites` runs the test suites of `Products.CMFPlone`,
`plone.app.contenttypes` and `plone.restapi` with `bin/test` of the
scenario, on ZOE:

- the zoe scenario: the base storage of the test layers of plone.testing
  is a `zoe.Storage` in a new directory;
- the zeoclient scenario: it is a `ZEO.ClientStorage` of a `zoe serve`
  that the recipe starts and stops.

`zoe.plone.testing` gives the layer that swaps the base storage, so the
suites run unchanged. The recipe prints the summary of zope.testrunner.
No test is skipped because of the storage.

Run it inside `devenv shell`: `uv run --directory python pytest
../plone/tests/test_plone_suites.py`. It takes the time of the suites.
"""

import re
import subprocess
from pathlib import Path

import pytest

REPO = Path(__file__).resolve().parents[2]
# The suites hold thousands of tests. A run that collects fewer did not
# run them all. The floor leaves out the robot tests, which need a browser.
FLOOR = 2900
SUMMARY = re.compile(
    r"Total: (?P<tests>\d+) tests, (?P<failures>\d+) failures?, "
    r"(?P<errors>\d+) errors?"
)


@pytest.mark.parametrize("scenario", ["zoe", "zeoclient"])
def test_the_plone_suites_pass(scenario):
    result = subprocess.run(
        ["just", "plone", scenario, "suites"],
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
    assert result.returncode == 0, output[-5000:]
    # The layer of ZOE ran, with the storage of the scenario.
    assert "zoe.plone.testing" in output
