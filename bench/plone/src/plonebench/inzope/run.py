"""Run a script of this directory inside Zope, with the storage of a stack.

    <scenario>/parts/instance/bin/interpreter run.py ZOPE_CONF SCRIPT [ARGS...]

The interpreter of a buildout scenario has the eggs of the instance. This
script configures Zope from ZOPE_CONF, opens the application, and runs
SCRIPT with the application as `app`, as `bin/instance run` does. It puts
the source directories of plonebench and of benchcommon on sys.path, so
SCRIPT can import `plonebench.corpus` and `plonebench.stack`, which need
only the standard library and benchcommon.
"""

import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
# HERE is bench/plone/src/plonebench/inzope.
SRC = os.path.dirname(os.path.dirname(HERE))
BENCH = os.path.dirname(os.path.dirname(SRC))
sys.path.insert(0, os.path.join(BENCH, "common", "src"))
sys.path.insert(0, SRC)

conf, script, *arguments = sys.argv[1:]

from Testing.makerequest import makerequest  # noqa: E402
from Zope2.Startup.run import configure_wsgi  # noqa: E402

configure_wsgi(conf)

import Zope2  # noqa: E402

app = makerequest(Zope2.app())
sys.argv = [script, *arguments]
path = script if os.path.isabs(script) else os.path.join(HERE, script)
with open(path) as file:
    code = compile(file.read(), path, "exec")
exec(code, {"app": app, "__name__": "__main__", "__file__": path})
