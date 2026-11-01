"""The helpers that the tests of plone/ share."""

import os
import subprocess


def zoe_serve_processes(pattern="zoe serve"):
    """Return the pids of the processes of this user whose command line
    matches `pattern`, by default every `zoe serve`."""
    result = subprocess.run(
        ["pgrep", "-u", str(os.getuid()), "-f", pattern],
        capture_output=True,
        text=True,
        check=False,
    )
    return {int(pid) for pid in result.stdout.split()}
