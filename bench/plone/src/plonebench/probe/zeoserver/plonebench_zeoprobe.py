"""The probe of the ZEO server process.

launch.py imports this module before it starts the server, and the runner
sets PLONEBENCH_PROBE_OUT. The ZEO server resolves conflicts in its own process,
with ZODB's resolver. The probe counts these resolutions, their outcome and
their time, as the probe of the Zope processes counts the resolutions of a
client, and the reason of each failure. It also counts the resolutions of
the votes that fail: their work is lost. It writes the counts as one JSON
line every 2 seconds and at the exit.

A finder of the import system patches `ZODB.ConflictResolution` and
`ZEO.StorageServer` when the server imports them, so the probe works
whether they are imported yet or not.
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
_counts = {
    "resolved": 0,
    "resolve_failed": 0,
    "resolve_seconds": 0.0,
    "resolved_in_failed_votes": 0,
}
_reasons = {}
_local = threading.local()
_TARGETS = ("ZODB.ConflictResolution", "ZEO.StorageServer")


def _dump():
    if not _OUT:
        return
    with _lock:
        record = dict(_counts, t=time.time(), pid=os.getpid(), kind="zeoserver")
        record["resolve_reasons"] = dict(_reasons)
    with open(_OUT, "a") as out:
        out.write(json.dumps(record) + "\n")


def _failure_reason(args):
    """Return the reason of a failed resolution, as the probe of the Zope
    processes writes it."""
    klass, error, message = args
    name = getattr(klass, "__name__", str(klass))
    if error == "BTreesConflictError" and ": " in message:
        message = message.split(": ", 1)[1]
    return f"{name}: {error}: {message}"


class _ResolverLogger:
    """Stand in for the logger of `ZODB.ConflictResolution`: keep the
    reason of a failed resolution for the thread, pass every other call
    to the real logger."""

    def __init__(self, logger):
        self._logger = logger

    def debug(self, message, *args, **kwargs):
        if len(args) == 3:
            _local.reason = _failure_reason(args)
        else:
            self._logger.debug(message, *args, **kwargs)

    def __getattr__(self, name):
        return getattr(self._logger, name)


def _patch(module):
    cls = module.ConflictResolvingStorage
    original = cls.tryToResolveConflict
    module.logger = _ResolverLogger(module.logger)

    def tryToResolveConflict(self, *args, **kwargs):
        start = time.perf_counter()
        _local.reason = None
        try:
            result = original(self, *args, **kwargs)
        except Exception:
            reason = getattr(_local, "reason", None) or "unexpected error"
            with _lock:
                _counts["resolve_failed"] += 1
                _reasons[reason] = _reasons.get(reason, 0) + 1
                _counts["resolve_seconds"] += time.perf_counter() - start
            raise
        with _lock:
            _counts["resolved"] += 1
            _counts["resolve_seconds"] += time.perf_counter() - start
        _local.vote_resolved = getattr(_local, "vote_resolved", 0) + 1
        return result

    cls.tryToResolveConflict = tryToResolveConflict


def _patch_server(module):
    cls = module.ZEOStorage
    original = cls._vote

    def _vote(self, *args, **kwargs):
        # The resolutions of a vote run in the thread of the vote.
        _local.vote_resolved = 0
        try:
            result = original(self, *args, **kwargs)
        except Exception:
            _lost(_local.vote_resolved)
            raise
        # A list of conflicts for the client also ends the vote.
        if self.conflicts:
            _lost(_local.vote_resolved)
        return result

    cls._vote = _vote


def _lost(resolved):
    with _lock:
        _counts["resolved_in_failed_votes"] += resolved


class _Loader(importlib.abc.Loader):
    def __init__(self, loader):
        self.loader = loader

    def create_module(self, spec):
        return self.loader.create_module(spec)

    def exec_module(self, module):
        self.loader.exec_module(module)
        if module.__name__ == "ZEO.StorageServer":
            _patch_server(module)
        else:
            _patch(module)


class _Finder(importlib.abc.MetaPathFinder):
    def find_spec(self, name, path, target=None):
        if name not in _TARGETS:
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
