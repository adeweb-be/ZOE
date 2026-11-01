"""Run a script of inzope/ and count the content objects that it loads.

    run.py ZOPE_CONF load_probe.py SCRIPT_PY PROBE_JSON [ARGS...]

The probe wraps `ZODB.Connection.Connection.setstate`, which loads the
state of each object that the process reads, then runs SCRIPT_PY with
ARGS in the same process. A content object is an object with a UUID of
Plone. PROBE_JSON gets the count of the content objects that the script
loaded, and the count of all the objects that it loaded. The process
starts with an empty cache, so each object counts once.
"""

import json
import sys

from plone.uuid.interfaces import ATTRIBUTE_NAME
from ZODB.Connection import Connection

script, probe_json, *arguments = sys.argv[1:]
content = set()
loads = {"objects": 0}
original = Connection.setstate


def setstate(self, obj):
    original(self, obj)
    loads["objects"] += 1
    state = getattr(obj, "__dict__", None) or {}
    if state.get(ATTRIBUTE_NAME) is not None:
        content.add(obj._p_oid)


Connection.setstate = setstate
sys.argv = [script, *arguments]
with open(script) as file:
    code = compile(file.read(), script, "exec")
try:
    exec(code, {"app": app, "__name__": "__main__", "__file__": script})  # noqa: F821
finally:
    Connection.setstate = original
    with open(probe_json, "w") as file:
        json.dump({"content_objects": len(content), "objects": loads["objects"]}, file)
