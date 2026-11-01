"""The fixtures that the tests of the zoe package share.

`support.py` holds the shared helpers that are not fixtures.
"""

import os

import pytest

# `just test` does not collect the conformance suites and the kill loop.
# `just conformance` and `just crash` name their directories, so pytest
# collects them there.
collect_ignore = ["conformance", "crash"]


@pytest.fixture
def server(tmp_path):
    """A `zoe serve` on a new store in `tmp_path`. It is killed at the end
    of the test."""
    from zoe.testing import ZoeServer

    with ZoeServer(str(tmp_path / "store")) as server:
        yield server


@pytest.fixture
def no_zoe_environment(monkeypatch):
    """Remove each ZOE_* variable, so that only the test sets them. A module
    takes it with `pytestmark = pytest.mark.usefixtures(...)`."""
    for name in list(os.environ):
        if name.startswith("ZOE_"):
            monkeypatch.delenv(name)
