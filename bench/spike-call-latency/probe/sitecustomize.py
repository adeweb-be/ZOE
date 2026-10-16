"""Spike probe: time each storage call of each DB of a zodbshootout worker.

SPIKE_PROBE_OUT names a directory. Each process that opens a DB writes
<pid>.json there at its end: the calls in order, as (name, start, duration).
"""

import atexit
import os
import sys
import time

OUT = os.environ.get("SPIKE_PROBE_OUT")
NAMES = (
    "load loadBefore loadSerial tpc_begin store tpc_vote tpc_finish tpc_abort "
    "lastTransaction new_oid sync checkCurrentSerialInTransaction prefetch "
    "getTid history ping release poll_invalidations"
).split()


def install():
    import importlib.abc
    import importlib.machinery

    events = []
    pc = time.perf_counter

    def wrap(storage):
        for name in NAMES:
            method = getattr(storage, name, None)
            if method is None:
                continue

            def timed(*args, _m=method, _n=name, **kw):
                begin = pc()
                try:
                    return _m(*args, **kw)
                finally:
                    events.append((_n, begin, pc() - begin))

            try:
                setattr(storage, name, timed)
            except AttributeError:
                pass

    def patch(module):
        DB = module.DB
        init = DB.__init__

        def __init__(self, storage, *args, **kw):
            wrap(storage)
            events.append(("DB.__init__", pc(), 0.0))
            init(self, storage, *args, **kw)

        DB.__init__ = __init__

    class Finder(importlib.abc.MetaPathFinder):
        def find_spec(self, name, path, target=None):
            if name != "ZODB.DB":
                return None
            sys.meta_path.remove(self)
            spec = importlib.machinery.PathFinder.find_spec(name, path)
            loader = spec.loader
            exec_module = loader.exec_module

            def run(module):
                exec_module(module)
                patch(module)

            loader.exec_module = run
            return spec

    sys.meta_path.insert(0, Finder())

    def write():
        if not events:
            return
        import json

        with open(os.path.join(OUT, f"{os.getpid()}.json"), "w") as f:
            json.dump(events, f)

    atexit.register(write)
    exit_now = os._exit

    def _exit(code):
        try:
            write()
        finally:
            exit_now(code)

    os._exit = _exit


if OUT:
    install()
