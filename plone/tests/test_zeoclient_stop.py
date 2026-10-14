"""`just plone zeoclient check` stops `zoe serve` also when it fails.

Each test makes the check fail at one step, then looks for a `zoe serve`
process that the recipe left. Run them inside `devenv shell`: `uv run
--directory python pytest ../plone/tests`. The first build needs the
network.
"""

import functools
import http.server
import os
import socket
import subprocess
import threading
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
STORE = REPO / "target" / "plone" / "zeoclient" / "var" / "zoe"
ZEO_ADDRESS = ("127.0.0.1", 8100)


def zoe_serve_processes():
    """Return the pids of the `zoe serve` processes of this user."""
    result = subprocess.run(
        ["pgrep", "-u", str(os.getuid()), "-f", "zoe serve"],
        capture_output=True,
        text=True,
        check=False,
    )
    return {int(pid) for pid in result.stdout.split()}


def check(env=None):
    """Run `just plone zeoclient check` and return its result."""
    return subprocess.run(
        ["just", "plone", "zeoclient", "check"],
        cwd=REPO,
        env={**os.environ, **(env or {})},
        capture_output=True,
        text=True,
        check=False,
    )


def test_a_busy_zeo_port_fails_the_check_and_leaves_no_server():
    before = zoe_serve_processes()
    with socket.socket() as busy:
        busy.bind(ZEO_ADDRESS)
        busy.listen()
        result = check()
    assert result.returncode != 0, result.stdout[-5000:]
    assert "zoe serve does not listen" in result.stderr
    assert zoe_serve_processes() <= before


def test_a_failed_read_of_the_site_stops_the_server():
    # A web server that answers 404 holds the port of the instance. The
    # instance does not start, and read_site.py fails on the 404, after
    # create_site.py wrote the site through zoe serve.
    handler = functools.partial(
        http.server.SimpleHTTPRequestHandler, directory=str(REPO / "plone" / "tests")
    )
    before = zoe_serve_processes()
    with http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler) as web:
        thread = threading.Thread(target=web.serve_forever, daemon=True)
        thread.start()
        try:
            result = check({"ZOE_PLONE_PORT": str(web.server_address[1])})
        finally:
            web.shutdown()
    assert result.returncode != 0, result.stdout[-5000:]
    assert "gives the status 404" in result.stderr
    assert (STORE / "zoe.redb").exists()
    assert zoe_serve_processes() <= before
