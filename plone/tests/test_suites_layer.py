"""`just plone <scenario> suites` runs the Plone suites on the layer of ZOE.

Each test runs a few tests of the three suites, selected with `-t`, and
reads the summary of zope.testrunner. The layer of zoe.plone.testing must
set up under `plone.testing.zope.Startup`, and the zeoclient run must stop
its `zoe serve`, also when `bin/test` fails. Run them inside `devenv shell`:
`uv run --directory python pytest ../plone/tests/test_suites_layer.py`.
The first build needs the network.
"""

import os
import re
import subprocess
from pathlib import Path

import pytest

REPO = Path(__file__).resolve().parents[2]
TARGET = REPO / "target" / "plone"
STORE = TARGET / "zeoclient" / "var" / "suites"
# One test of each suite: an integration test of Products.CMFPlone, a
# functional test of plone.app.contenttypes and functional tests of
# plone.restapi over the WSGI server of plone.testing.
SELECTION = [
    "-t",
    "testExpectedPermissions",
    "-t",
    "test_folder_view",
    "-t",
    "test_get_content_returns_fullobjects",
]
LAYERS = {"zoe": "ZoeStorage", "zeoclient": "ZoeServe"}
# The probe runs in the interpreter of the zoe instance. Startup gets a
# DemoStorage on a zoe.Storage. The base stays open when the DemoStorage
# and a DemoStorage above it close, and tearDown restores plone.testing.
PROBE = """
import transaction
import zoe
import zoe.plone.testing
from plone.testing import zodb, zope

stack = zodb.stackDemoStorage
layer = zoe.plone.testing.install()
assert zope.STARTUP.__bases__[0] is layer
layer.setUp()
db = zodb.stackDemoStorage(None, name="Startup")
base = db.storage.base
assert isinstance(base, zoe.Storage)
assert db.storage.getName() == "Startup"
upper = zodb.stackDemoStorage(db, name="Upper")
connection = upper.open()
connection.root()["key"] = 1
transaction.commit()
connection.close()
upper.close()
db.close()
base.lastTransaction()
layer.tearDown()
assert zodb.stackDemoStorage is stack
print("probe ok")
"""


def zoe_serve_processes():
    """Return the pids of the `zoe serve` processes on the store of the
    suites.

    The pattern names the store, so the servers of other worktrees and of
    other tests do not match.
    """
    pattern = f"zoe serve --path {re.escape(str(STORE))}( |$)"
    result = subprocess.run(
        ["pgrep", "-u", str(os.getuid()), "-f", pattern],
        capture_output=True,
        text=True,
        check=False,
    )
    return {int(pid) for pid in result.stdout.split()}


def suites(scenario, *args):
    """Run `just plone <scenario> suites` with the options `args`."""
    result = subprocess.run(
        ["just", "plone", scenario, "suites", *args],
        cwd=REPO,
        capture_output=True,
        text=True,
        check=False,
    )
    return result, result.stdout + result.stderr


@pytest.mark.parametrize("scenario", ["zoe", "zeoclient"])
def test_a_selection_of_the_suites_runs_on_the_layer(scenario):
    result, output = suites(scenario, *SELECTION)
    assert result.returncode == 0, output[-5000:]
    assert "Total: 5 tests, 0 failures, 0 errors" in output, output[-5000:]
    layer = f"zoe.plone.testing.{LAYERS[scenario]}"
    assert f"Set up {layer} in" in output
    assert f"Tear down {layer} in" in output
    # zope.testrunner sets up the layer before Startup, and tears it down
    # after Startup.
    assert output.index(f"Set up {layer}") < output.index(
        "Set up plone.testing.zope.Startup"
    )
    assert output.index("Tear down plone.testing.zope.Startup") < output.index(
        f"Tear down {layer}"
    )
    if scenario == "zeoclient":
        assert re.search(r"zoe serve listens on 127\.0\.0\.1:\d+", output)
        assert (STORE / "zoe.redb").exists()
        assert not zoe_serve_processes()


def test_a_failed_test_run_stops_the_server():
    # zope.testrunner refuses an unknown option and exits with an error.
    result, output = suites("zeoclient", "--no-such-option")
    assert result.returncode != 0, output[-5000:]
    assert "zoe serve listens on" in output
    assert not zoe_serve_processes()


def test_the_layer_gives_startup_a_zoe_base(tmp_path):
    suites("zoe", "-t", "no-such-test")
    interpreter = TARGET / "zoe" / "parts" / "instance" / "bin" / "interpreter"
    result = subprocess.run(
        [str(interpreter), "-c", PROBE],
        cwd=tmp_path,
        env={**os.environ, "TMPDIR": str(tmp_path)},
        capture_output=True,
        text=True,
        check=False,
    )
    assert result.returncode == 0, result.stderr[-5000:]
    assert "probe ok" in result.stdout
    # tearDown removes the directory of the layer.
    assert not list(tmp_path.glob("zoe-plone-*"))
