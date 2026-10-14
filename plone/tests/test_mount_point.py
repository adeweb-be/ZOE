"""Acceptance test: a Plone site with a mount point runs on one server.

This is an item of the phase 2 gate (F22). The scenario `zeoclient-mount`
(`plone/zeoclient-mount.cfg`) has two databases in `zope.conf`, `main`
mounted at `/` and `content` mounted at `/content`, each a `<zeoclient>`
of the same `zoe serve` with its own storage name. The server serves the
two named storages, `target/plone/zeoclient-mount/var/main` and
`target/plone/zeoclient-mount/var/content`.

`just plone zeoclient-mount check` builds the scenario, starts the server,
creates the mount point and a Plone site inside it by script, starts the
instance, reads the site over HTTP, then stops the instance and the server.
The objects of the site are in the `content` store, not in `main`.

Run it inside `devenv shell`: `uv run --directory python pytest
../plone/tests/test_mount_point.py`. The first build needs the network.
"""

import subprocess
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
VAR = REPO / "target" / "plone" / "zeoclient-mount" / "var"


def size_of(store):
    return sum(f.stat().st_size for f in store.rglob("*") if f.is_file())


def test_a_site_in_a_mount_point_runs_on_one_server():
    result = subprocess.run(
        ["just", "plone", "zeoclient-mount", "check"],
        cwd=REPO,
        capture_output=True,
        text=True,
        check=False,
    )
    assert result.returncode == 0, result.stdout[-5000:] + result.stderr[-5000:]
    main, content = VAR / "main", VAR / "content"
    assert (main / "zoe.redb").exists()
    assert (content / "zoe.redb").exists()
    # The site, with its catalog and its content, lives in the mounted
    # store: that store holds far more than the main one.
    assert size_of(content) > size_of(main)
