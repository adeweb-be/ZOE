"""`just plone zeoclient-mount check` stops `zoe serve` also when it fails.

The test makes the check fail after create_site.py wrote the site, then
looks for a `zoe serve` process that the recipe left. Run it inside
`devenv shell`: `just plone test plone/tests/test_mount_point_stop.py`.
The first build needs the network.
"""

import functools
import http.server
import os
import re
import subprocess
import threading
from pathlib import Path

from conftest import zoe_serve_processes

REPO = Path(__file__).resolve().parents[2]
VAR = REPO / "target" / "plone" / "zeoclient-mount" / "var"


# The `zoe serve` processes on the stores of the scenario. It names the
# store of `main`, so the servers of other worktrees and of other tests do
# not match.
SERVERS = f"zoe serve --storage main={re.escape(str(VAR / 'main'))} "


def test_a_failed_read_of_the_mounted_site_stops_the_server():
    # A web server that answers 404 holds the port of the instance. The
    # instance does not start, and read_site.py fails on the 404, after
    # create_site.py wrote the site through zoe serve.
    handler = functools.partial(
        http.server.SimpleHTTPRequestHandler, directory=str(REPO / "plone" / "tests")
    )
    with http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler) as web:
        thread = threading.Thread(target=web.serve_forever, daemon=True)
        thread.start()
        try:
            result = subprocess.run(
                ["just", "plone", "zeoclient-mount", "check"],
                cwd=REPO,
                env={**os.environ, "ZOE_PLONE_PORT": str(web.server_address[1])},
                capture_output=True,
                text=True,
                check=False,
            )
        finally:
            web.shutdown()
    assert result.returncode != 0, result.stdout[-5000:]
    assert "/content/Plone gives the status 404" in result.stderr
    # One server served the two stores.
    log = (VAR / "log" / "zoe-serve.log").read_text()
    assert log.count("listening ") == 1, log
    assert (VAR / "main" / "zoe.redb").exists()
    assert (VAR / "content" / "zoe.redb").exists()
    assert not zoe_serve_processes(SERVERS)
