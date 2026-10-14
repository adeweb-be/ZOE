"""Import the stacks of the runs and say which modules turned the GIL back
on. Not for merge. Run it with -W always, and with the POC client on
PYTHONPATH."""

import json
import sys
import sysconfig
import warnings

found = []
with warnings.catch_warnings(record=True) as caught:
    warnings.simplefilter("always")
    import BTrees.OOBTree  # noqa: F401
    import persistent  # noqa: F401
    import pyperf  # noqa: F401
    import transaction  # noqa: F401
    import zodbpickle.pickle  # noqa: F401
    import ZEO.ClientStorage  # noqa: F401
    import ZODB  # noqa: F401
    import zodbshootout.main  # noqa: F401
    import zoeclientx  # noqa: F401
    import msgpack  # noqa: F401
    import zope.interface  # noqa: F401
for w in caught:
    text = str(w.message)
    if "GIL" in text:
        found.append(text)
print(json.dumps({
    "python": sys.version.split()[0] + ("t" if sysconfig.get_config_var("Py_GIL_DISABLED") else ""),
    "free_threaded_build": bool(sysconfig.get_config_var("Py_GIL_DISABLED")),
    "gil_enabled": sys._is_gil_enabled() if hasattr(sys, "_is_gil_enabled") else True,
    "warnings": found,
}))
