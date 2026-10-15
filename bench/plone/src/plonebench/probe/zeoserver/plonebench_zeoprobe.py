"""The probe of the ZEO server process.

launch.py imports this module before it starts the server, and the runner
sets PLONEBENCH_PROBE_OUT. The ZEO server resolves conflicts in its own process,
with ZODB's resolver. The probe counts these resolutions, their outcome and
their time, as the probe of the Zope processes counts the resolutions of a
client. It writes the counts as one JSON line every 2 seconds and at the
exit.

A finder of the import system patches `ZODB.ConflictResolution` when the
server imports it, so the probe works whether ZODB is imported yet or not.
"""

import atexit
import importlib.abc
import importlib.util
import json
import os
import sys
import threading
import time

_OUT = os.environ.get("PLONEBENCH_PROBE_OUT")
_lock = threading.Lock()
_counts = {"resolved": 0, "resolve_failed": 0, "resolve_seconds": 0.0}
_TARGET = "ZODB.ConflictResolution"


def _dump():
    if not _OUT:
        return
    with _lock:
        record = dict(_counts, t=time.time(), pid=os.getpid(), kind="zeoserver")
    with open(_OUT, "a") as out:
        out.write(json.dumps(record) + "\n")


def _patch(module):
    cls = module.ConflictResolvingStorage
    original = cls.tryToResolveConflict

    def tryToResolveConflict(self, *args, **kwargs):
        start = time.perf_counter()
        try:
            result = original(self, *args, **kwargs)
        except Exception:
            with _lock:
                _counts["resolve_failed"] += 1
                _counts["resolve_seconds"] += time.perf_counter() - start
            raise
        with _lock:
            _counts["resolved"] += 1
            _counts["resolve_seconds"] += time.perf_counter() - start
        return result

    cls.tryToResolveConflict = tryToResolveConflict


class _Loader(importlib.abc.Loader):
    def __init__(self, loader):
        self.loader = loader

    def create_module(self, spec):
        return self.loader.create_module(spec)

    def exec_module(self, module):
        self.loader.exec_module(module)
        _patch(module)


class _Finder(importlib.abc.MetaPathFinder):
    def find_spec(self, name, path, target=None):
        if name != _TARGET:
            return None
        sys.meta_path.remove(self)
        try:
            spec = importlib.util.find_spec(name)
        finally:
            sys.meta_path.insert(0, self)
        if spec is None or spec.loader is None:
            return None
        spec.loader = _Loader(spec.loader)
        return spec


if _OUT:
    sys.meta_path.insert(0, _Finder())
    atexit.register(_dump)

    def _loop():
        while True:
            time.sleep(2.0)
            _dump()

    threading.Thread(target=_loop, daemon=True, name="probe").start()
