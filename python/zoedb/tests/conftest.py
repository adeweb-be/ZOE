"""The fixtures that the tests of the zoe package share.

`support.py` holds the shared helpers that are not fixtures.
"""

import os

import pytest

# `just test` does not collect the conformance suites and the kill loop.
# `just conformance` and `just crash` name their directories, so pytest
# collects them there.
collect_ignore = ["conformance", "crash"]

# The switch of the reader of ZOE (ADR 0057): with ZOE_TEST_READER=on, each
# Connection of the run makes a `zoe.reader.Reader` in place of ZODB's
# `ObjectReader`, also a Connection that a test of ZODB makes on a storage
# of its own. This is a switch of the tests only. The end of the run prints
# the counts of the reader.
if os.environ.get("ZOE_TEST_READER") == "on":
    import ZODB.Connection

    import zoe.reader

    ZODB.Connection.ObjectReader = zoe.reader.Reader

    def pytest_terminal_summary(terminalreporter):
        terminalreporter.write_line(f"zoe.reader counts: {zoe.reader.counts()}")


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
