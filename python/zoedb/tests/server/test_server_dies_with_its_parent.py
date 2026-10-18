"""Acceptance test: a `zoe serve` of the test harness ends with its parent.

`zoe.testing.ZoeServer` and `zoe.testing.ZEO_LAYER` start `zoe serve` for
the tests. When the test process dies (a hang that the runner kills, a
crash), its `tearDown` never runs. The server must not stay up: the kernel
kills it when the process that started it dies, so it never outlives its
test process on a shared machine.

Each test runs a child Python process that starts a server and then
waits. The test finds the server among the children of that process,
checks that it runs while its parent runs, kills the parent with SIGKILL,
and checks that the server ends.

The kernel kills the server with the parent-death signal of Linux
(`PR_SET_PDEATHSIG`), and the test finds the processes in `/proc`. Other
platforms have neither, so the tests skip there.
"""

import contextlib
import os
import pathlib
import signal
import subprocess
import sys
import textwrap
import time

import pytest

pytestmark = pytest.mark.skipif(
    sys.platform != "linux",
    reason="Linux only: the parent-death signal and /proc",
)

TIMEOUT = 30

# The child starts a server with `ZoeServer`, prints its port and waits.
ZOE_SERVER = textwrap.dedent(
    """
    import sys
    import time

    from zoe.testing import ZoeServer

    server = ZoeServer(sys.argv[1]).start(30)
    print(server.address[1], flush=True)
    time.sleep(600)
    """
)

# The child starts a server through the forker of ZEO under ZEO_LAYER,
# prints its port and waits.
ZEO_FORKER = textwrap.dedent(
    """
    import time

    from ZEO.tests import forker

    from zoe.testing import ZEO_LAYER

    ZEO_LAYER.setUp()
    address, stop = forker.start_zeo_server(path="Data.fs")
    print(address[1], flush=True)
    time.sleep(600)
    """
)


@pytest.fixture(autouse=True)
def no_zoe_environment(monkeypatch):
    """Keep only ZOE_BINARY of the ZOE_* variables."""
    for name in list(os.environ):
        if name.startswith("ZOE_") and name != "ZOE_BINARY":
            monkeypatch.delenv(name)


def children(pid):
    """Return the ids of the processes whose parent is `pid`."""
    found = []
    for entry in pathlib.Path("/proc").iterdir():
        if not entry.name.isdigit():
            continue
        try:
            stat = (entry / "stat").read_text()
        except OSError:
            continue
        # The command name is in parentheses and can hold spaces.
        fields = stat.rsplit(")", 1)[1].split()
        if int(fields[1]) == pid:
            found.append(int(entry.name))
    return found


def alive(pid):
    """Tell if the process `pid` runs.

    A process that ended does not run. Until its parent reaps it, it is a
    zombie (state Z). While the parent reaps it, its state is X. Then its
    entry goes from `/proc`.
    """
    try:
        stat = pathlib.Path(f"/proc/{pid}/stat").read_text()
    except OSError:
        return False
    return stat.rsplit(")", 1)[1].split()[0] not in ("Z", "X")


def name_of(pid):
    try:
        return pathlib.Path(f"/proc/{pid}/comm").read_text().strip()
    except OSError:
        return ""


def start_child(script, *args, cwd):
    """Run `script` in a child Python process. Return the process and the
    id of the `zoe serve` process that it started."""
    child = subprocess.Popen(
        [sys.executable, "-c", script, *args],
        cwd=cwd,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        text=True,
    )
    line = child.stdout.readline()
    assert line.strip().isdigit(), f"the child did not start a server: {line!r}"
    servers = [pid for pid in children(child.pid) if name_of(pid) == "zoe"]
    assert len(servers) == 1, f"no single zoe serve among {children(child.pid)}"
    return child, servers[0]


def assert_dies_with_its_parent(child, server):
    try:
        # The server runs while its parent runs.
        time.sleep(1)
        assert alive(server), "zoe serve ended while its parent runs"
        child.send_signal(signal.SIGKILL)
        child.wait(TIMEOUT)
        deadline = time.monotonic() + 10
        while alive(server) and time.monotonic() < deadline:
            time.sleep(0.05)
        assert not alive(server), "zoe serve outlives the process that started it"
    finally:
        if alive(server):
            # The server can end between the check and the kill.
            with contextlib.suppress(ProcessLookupError):
                os.kill(server, signal.SIGKILL)
        if child.poll() is None:
            child.kill()
            child.wait()
        child.stdout.close()


def test_a_server_of_zoe_server_ends_when_its_test_process_dies(tmp_path):
    child, server = start_child(ZOE_SERVER, str(tmp_path / "store"), cwd=tmp_path)
    assert_dies_with_its_parent(child, server)


def test_a_server_of_the_zeo_layer_ends_when_its_test_process_dies(tmp_path):
    child, server = start_child(ZEO_FORKER, cwd=tmp_path)
    assert_dies_with_its_parent(child, server)
