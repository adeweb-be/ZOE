"""Record the GIL of each Python process of a zodbshootout run.

The harness puts this directory first on PYTHONPATH and names a directory
in ZOE_BENCH_GIL_RECORDS. Python imports this module when it starts. Each
process that has imported ZODB when it ends writes one JSON file to that
directory: `gil_enabled`, the value of `sys._is_gil_enabled()` at the end
(true on a build with the GIL), and `gil_enabled_by`, the modules that
turned the GIL back on. CPython warns when a module turns the GIL back on,
and the record takes the module names from these warnings.

The record changes no setting of the process. It shows each warning as
Python does, and it removes this directory from `sys.path`. Then it
imports the `sitecustomize` module that this one hides, if there is one.

When Python starts, the module imports only atexit, os and warnings. It
imports json when a process writes its record.
"""

import atexit
import os
import sys
import warnings

# The text of the warning that comes before the name of the module.
MARKER = "has been enabled to load module '"


def _install(records):
    enabled_by = set()
    written = set()
    show = warnings.showwarning
    exit_now = os._exit

    def showwarning(message, category, *args, **kwargs):
        text = str(message)
        if category is RuntimeWarning and MARKER in text:
            enabled_by.add(text.split(MARKER, 1)[1].split("'", 1)[0])
        return show(message, category, *args, **kwargs)

    def write():
        # A process forked after the record started writes its own file.
        pid = os.getpid()
        if pid in written or "ZODB" not in sys.modules:
            return
        written.add(pid)
        import json

        is_gil_enabled = getattr(sys, "_is_gil_enabled", None)
        record = {
            "gil_enabled": True if is_gil_enabled is None else bool(is_gil_enabled()),
            "gil_enabled_by": sorted(enabled_by),
        }
        name = f"{pid}-{os.urandom(4).hex()}.json"
        with open(os.path.join(records, name), "x") as file:
            json.dump(record, file)

    def _exit(code):
        # multiprocessing ends its children with os._exit, which runs no
        # atexit function.
        try:
            write()
        finally:
            exit_now(code)

    warnings.showwarning = showwarning
    atexit.register(write)
    os._exit = _exit


def _import_hidden():
    """Import the sitecustomize module that this one hides, if any."""
    here = os.path.realpath(os.path.dirname(__file__))
    sys.path[:] = [p for p in sys.path if os.path.realpath(p or ".") != here]
    this = sys.modules.pop(__name__)
    try:
        import sitecustomize  # noqa: F401
    except ImportError as error:
        if error.name != "sitecustomize":
            raise
    finally:
        # The import system takes the module from sys.modules when this
        # module ends.
        sys.modules.setdefault(__name__, this)


if records := os.environ.get("ZOE_BENCH_GIL_RECORDS"):
    _install(records)
_import_hidden()
