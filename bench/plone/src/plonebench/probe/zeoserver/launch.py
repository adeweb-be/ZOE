"""Start the ZEO server with the eggs of a buildout interpreter.

    <scenario>/parts/instance/bin/interpreter launch.py ZEO_CONF

The runzeo script of plone.recipe.zeoserver sets PYTHONPATH to the eggs,
which does not import the `zc` namespace packages on this interpreter. The
interpreter of the instance puts the same eggs on sys.path the way
buildout does. This script loads the probe of plonebench_zeoprobe.py, then
runs ZEO.runzeo with the configuration file.
"""

import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import plonebench_zeoprobe  # noqa: E402,F401  the probe
from ZEO.runzeo import main  # noqa: E402

main(["-C", sys.argv[1]])
