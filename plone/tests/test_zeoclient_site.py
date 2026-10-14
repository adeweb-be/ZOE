"""Acceptance test: a Plone 6 site runs on `<zeoclient>` against `zoe serve`.

This is the first item of the phase 2 gate. `just plone zeoclient check`
builds the zeoclient scenario, starts `zoe serve` on the store of the
scenario, `target/plone/zeoclient/var/zoe`, with the address of its
`zeo-address`, creates a Plone site by script through the ZEO client,
starts the instance, reads the site over HTTP, then stops the instance
and the server. The recipe starts and stops the server itself.

Run it inside `devenv shell`: `uv run --directory python pytest
../plone/tests/test_zeoclient_site.py`. The first build needs the network.
"""

import os
import subprocess
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
STORE = REPO / "target" / "plone" / "zeoclient" / "var" / "zoe"


def zoe_serve_processes():
    """Return the pids of the `zoe serve` processes of this user."""
    result = subprocess.run(
        ["pgrep", "-u", str(os.getuid()), "-f", "zoe serve"],
        capture_output=True,
        text=True,
        check=False,
    )
    return {int(pid) for pid in result.stdout.split()}


def test_a_plone_site_runs_on_zeoclient_against_zoe_serve():
    before = zoe_serve_processes()
    result = subprocess.run(
        ["just", "plone", "zeoclient", "check"],
        cwd=REPO,
        capture_output=True,
        text=True,
        check=False,
    )
    assert result.returncode == 0, result.stdout[-5000:] + result.stderr[-5000:]
    # The site lives in a ZOE store, written through the server.
    assert (STORE / "zoe.redb").exists()
    # The recipe stopped the server that it started.
    assert zoe_serve_processes() <= before
