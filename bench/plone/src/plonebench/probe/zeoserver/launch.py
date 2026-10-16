"""Start the ZEO server with the eggs of a buildout interpreter.

    <scenario>/parts/instance/bin/interpreter launch.py ZEO_CONF

The runzeo script of plone.recipe.zeoserver sets PYTHONPATH to the eggs,
which does not import the `zc` namespace packages on this interpreter. The
interpreter of the instance puts the same eggs on sys.path the way
buildout does. This script loads the probe of plonebench_zeoprobe.py, then
runs ZEO.runzeo with the configuration file.

On macOS, os.fsync leaves the data in the cache of the drive, as the
fsync(2) of `zoe serve` does (ADR 0040). When PLONEBENCH_ZEO_FSYNC is
`full`, this script makes os.fsync use F_FULLFSYNC, which also flushes the
cache of the drive: a stronger sync than the one of `zoe serve`.
"""

import os
import sys

if sys.platform == "darwin" and os.environ.get("PLONEBENCH_ZEO_FSYNC") == "full":
    import fcntl

    def _full_fsync(fd):
        if hasattr(fd, "fileno"):
            fd = fd.fileno()
        fcntl.fcntl(fd, fcntl.F_FULLFSYNC)

    os.fsync = _full_fsync

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import plonebench_zeoprobe  # noqa: E402,F401  the probe
from ZEO.runzeo import main  # noqa: E402

main(["-C", sys.argv[1]])
