"""Spike probe: the steps of one call of ZEO, on the clock of the Rust probes.

ZOE_SPIKE_TRACE names the prefix of the dump files. The probes:

- 17, 18: a `loadBefore` of the storage of a DB starts and ends (both
  clients);
- 11, 12: the IO thread of ZEO's client writes a call: before, after;
- 13, 14: the IO thread of ZEO's client gets a message: before, after;
- 21: ZEO's server gets a message; 22, 23: its handler starts and ends;
- 24: ZEO's server wrote the reply.
"""

import atexit
import os
import sys
import time

PREFIX = os.environ.get("ZOE_SPIKE_TRACE")


def install():
    import importlib.abc
    import importlib.machinery

    clock = getattr(time, "CLOCK_UPTIME_RAW", time.CLOCK_MONOTONIC)
    now = time.clock_gettime_ns
    events = []
    add = events.append
    role = []

    def wrap_method(cls, name, before, after):
        original = getattr(cls, name)

        def probed(*args, **kw):
            add((before, now(clock)))
            try:
                return original(*args, **kw)
            finally:
                add((after, now(clock)))

        setattr(cls, name, probed)

    def patch_db(module):
        DB = module.DB
        init = DB.__init__

        def __init__(self, storage, *args, **kw):
            method = storage.loadBefore

            def loadBefore(*a, **k):
                add((17, now(clock)))
                try:
                    return method(*a, **k)
                finally:
                    add((18, now(clock)))

            storage.loadBefore = loadBefore
            role.append("pyclient")
            init(self, storage, *args, **kw)

        DB.__init__ = __init__

    def patch_base(module):
        made = module.Protocol.connection_made

        def connection_made(self, transport):
            made(self, transport)
            write = self.write_message

            def write_message(message):
                add((11, now(clock)))
                try:
                    return write(message)
                finally:
                    add((12, now(clock)))

            self.write_message = write_message

        module.Protocol.connection_made = connection_made

    def patch_client(module):
        wrap_method(module.Protocol, "call_sync", 11, 12)
        wrap_method(module.Protocol, "message_received", 13, 14)

    def patch_server(module):
        role.append("zeoserver")
        wrap_method(module.ServerProtocol, "message_received", 21, 24)

    def patch_storage_server(module):
        wrap_method(module.ZEOStorage, "loadBefore", 22, 23)
        wrap_method(module.ZEOStorage, "vote", 31, 32)
        wrap_method(module.ZEOStorage, "tpc_finish", 33, 34)

    def patch_file_storage(module):
        fsync = module.fsync

        def probed(fd):
            add((35, now(clock)))
            try:
                return fsync(fd)
            finally:
                add((36, now(clock)))

        module.fsync = probed

    patches = {
        "ZODB.DB": patch_db,
        "ZEO.asyncio.base": patch_base,
        "ZEO.asyncio.client": patch_client,
        "ZEO.asyncio.server": patch_server,
        "ZEO.StorageServer": patch_storage_server,
        "ZODB.FileStorage.FileStorage": patch_file_storage,
    }

    class Finder(importlib.abc.MetaPathFinder):
        def find_spec(self, name, path, target=None):
            patch = patches.pop(name, None)
            if patch is None:
                return None
            if not patches:
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
        if not events or not role:
            return
        with open(f"{PREFIX}.{role[0]}.{os.getpid()}", "w") as f:
            for probe, t in events:
                f.write(f"{probe} {t}\n")
        events.clear()

    atexit.register(write)
    exit_now = os._exit

    def _exit(code):
        try:
            write()
        finally:
            exit_now(code)

    os._exit = _exit


if PREFIX:
    install()
