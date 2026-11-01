"""Where a load spends its time between the ghost and the client cache.

`python -m shootout.loadpath terms` opens a `DB` on a client of a server,
writes the corpus of zodbshootout (a `PersistentMapping` of `PObject`s of
1 KB or 100 KB), then times one load of each object through ZODB, and
each layer of that load alone, in rounds. A hit round reads the objects
from the warm client cache. A miss round empties the client cache first.
Each term is the difference of two layers, per load, in µs. The clients
are the native client (`zoe.zeo.ClientStorage` against `zoe serve`) and
`ZEO.ClientStorage` against a ZEO server on a FileStorage. Both get the
same `cache_size`, large enough for the corpus. No other setting of a
client or a server changes.

`terms` also counts the Python calls of one load with `sys.setprofile`,
and the `persistent_load` callbacks of each record of the corpus.

`python -m shootout.loadpath loop` repeats the hit or the miss round of
one client for a number of seconds, for a sampling profiler: with
`--py-spy FILE`, it runs itself under `py-spy record --native`; with
`--cprofile FILE`, under cProfile. `frames FILE` prints the time per
frame of a py-spy file in its raw format, inside the timed loop only.

`scan FILE` reads each current record of a FileStorage and counts its
references by class: the `persistent_load` callbacks of one load of the
record. It imports nothing but ZODB, so the interpreter of a Plone
instance runs it by path. With `--classes`, it also imports each class
and says whether its `__setstate__` is Python code.
"""

import argparse
import contextlib
import gc
import inspect
import io
import json
import os
import platform
import statistics
import subprocess
import sys
import sysconfig
import tempfile
import time
from collections import Counter, defaultdict
from pathlib import Path
from time import perf_counter

MB = 1024 * 1024
# The client cache of both clients holds the whole corpus.
CACHE_SIZE = 256 * MB
# The objects and the rounds of each record size, as in the hit cells of
# docs/spikes/client-cache.md.
OBJECTS = {1024: 1000, 102400: 200}
ROUNDS = {1024: 30, 102400: 15}
# The decodes of the mapping in one pass: one decode is too short to time.
MAPPING_REPEAT = 20
CLIENTS = ("native", "zeo")
# The function of this module that runs the timed loop of a round. The
# profiles count only the samples under it.
TIMED = "_touch"


# The corpus and the clients.


@contextlib.contextmanager
def opened(client: str, size: int, objects: int, workdir: Path):
    """Start a server, open a `DB` on a client of it, write the corpus, and
    yield the `Fixture`. Stop all at the exit."""
    import ZODB
    from benchcommon import servers

    with contextlib.ExitStack() as stack:
        if client == "native":
            import zoe.zeo

            from shootout.storages import admin_address

            address = stack.enter_context(
                servers.zoe_serve(workdir, ("--admin-listen", "127.0.0.1:0"))
            )
            admin = admin_address(workdir)
            storage = zoe.zeo.ClientStorage(
                servers.host_and_port(address), cache_size=CACHE_SIZE, wait=True
            )
        else:
            import ZEO.ClientStorage

            port = stack.enter_context(servers.zeo_server(workdir))
            admin = None
            storage = ZEO.ClientStorage.ClientStorage(
                ("127.0.0.1", port), cache_size=CACHE_SIZE, wait=True
            )
        # The object cache of the connection holds the corpus: no object
        # leaves it between two rounds but by `cacheMinimize`.
        db = ZODB.DB(storage, cache_size=10 * objects)
        stack.callback(db.close)
        conn = db.open()
        stack.callback(conn.close)
        yield Fixture(client, size, objects, db, conn, admin)


def corpus(objects: int, size: int) -> dict:
    """Return the objects of zodbshootout: `PObject`s with its data."""
    from zodbshootout.speedtest import SpeedTestData

    return SpeedTestData(1, objects, size).data_to_store()


class Fixture:
    """The corpus in one connection, and the callables of each layer."""

    def __init__(self, client, size, objects, db, conn, admin):
        import transaction
        from persistent.mapping import PersistentMapping

        self.client, self.size, self.db, self.conn = client, size, db, conn
        self.admin = admin
        root = conn.root()
        root["m"] = PersistentMapping(corpus(objects, size))
        transaction.commit()
        conn.transaction_manager.begin()
        self.mapping = root["m"]
        self.objs = list(self.mapping.values())
        self.oids = [obj._p_oid for obj in self.objs]
        self.storage = db.storage
        self.inst = conn._storage
        self.start = self.inst._start
        self.reader = conn._reader
        self.cache = conn._cache
        records = [self.inst.load(oid) for oid in self.oids]
        self.pickles = [p for p, _ in records]
        self.serials = [s for _, s in records]
        self.states = [self.reader.getState(p) for p in self.pickles]
        self.pairs = list(zip(self.objs, self.states, strict=True))
        self.book = [
            (obj, oid, serial, len(p))
            for obj, oid, serial, p in zip(
                self.objs, self.oids, self.serials, self.pickles, strict=True
            )
        ]
        klass = type(self.objs[0])
        self.class_name = (klass.__module__, klass.__name__)
        if client == "native":
            self.raw = self.storage._native.load_before
        else:
            self.raw = self.storage._cache.loadBefore
        self.mapping_pickle, _ = self.inst.load(self.mapping._p_oid)
        self.refs = self.references(self.mapping_pickle)
        # Each round reads the cache once before its first pass.
        for oid in self.oids:
            self.raw(oid, self.start)

    def references(self, pickle: bytes) -> list:
        """Return the persistent ids that one decode of `pickle` gives to
        `persistent_load`, in their order."""
        refs = []
        load = self.reader._persistent_load
        unpickler = self.reader._get_unpickler(pickle)

        def counted(ref):
            refs.append(ref)
            return load(ref)

        unpickler.persistent_load = counted
        unpickler.load()
        unpickler.load()
        return refs

    def clear(self):
        """Empty the client cache, as zodbshootout does before a cold read."""
        self.storage._cache.clear()

    def server_loads(self) -> int | None:
        if self.admin is None:
            return None
        from shootout.storages import server_loads

        return server_loads(self.admin)

    def entries(self) -> int:
        return len(self.storage._cache)


# The layers. Each pass runs one layer over the corpus and returns its time.


def _touch(objs):
    for obj in objs:
        obj.attr  # noqa: B018 - the access loads a ghost


def pass_touch(f):
    t = perf_counter()
    _touch(f.objs)
    return perf_counter() - t


def pass_empty(f):
    t = perf_counter()
    for _ in f.oids:
        pass
    return perf_counter() - t


def pass_mvcc(f):
    load = f.inst.load
    t = perf_counter()
    for oid in f.oids:
        load(oid)
    return perf_counter() - t


def pass_client(f):
    load_before, start = f.storage.loadBefore, f.start
    t = perf_counter()
    for oid in f.oids:
        load_before(oid, start)
    return perf_counter() - t


def pass_cache(f):
    raw, start = f.raw, f.start
    t = perf_counter()
    for oid in f.oids:
        raw(oid, start)
    return perf_counter() - t


def pass_getstate(f):
    get_state = f.reader.getState
    t = perf_counter()
    for p in f.pickles:
        get_state(p)
    return perf_counter() - t


def pass_unpickler(f):
    get_unpickler = f.reader._get_unpickler
    t = perf_counter()
    for p in f.pickles:
        get_unpickler(p)
    return perf_counter() - t


def pass_class(f):
    get_unpickler = f.reader._get_unpickler
    t = perf_counter()
    for p in f.pickles:
        get_unpickler(p).load()
    return perf_counter() - t


def pass_find_global(f):
    factory, conn = f.reader._factory, f.conn
    module, name = f.class_name
    t = perf_counter()
    for _ in f.oids:
        factory(conn, module, name)
    return perf_counter() - t


def pass_setstate(f):
    t = perf_counter()
    for obj, state in f.pairs:
        obj.__setstate__(state)
    return perf_counter() - t


def pass_book(f):
    """The lines of `Connection.setstate` after the decode."""
    from ZODB.blob import Blob

    conn = f.conn
    estimate = f.cache.update_object_size_estimation
    t = perf_counter()
    for obj, oid, serial, size in f.book:
        conn._load_count += 1
        obj._p_serial = serial
        estimate(oid, size)
        obj._p_estimated_size = size
        isinstance(obj, Blob)
    return perf_counter() - t


def pass_mapping(f):
    """Decode the mapping with each child in the object cache."""
    get_state, p = f.reader.getState, f.mapping_pickle
    t = perf_counter()
    for _ in range(MAPPING_REPEAT):
        get_state(p)
    return perf_counter() - t


def pass_mapping_new(f):
    """Decode the mapping into an empty object cache: each child becomes a
    new ghost."""
    from persistent import PickleCache
    from ZODB.serialize import ObjectReader

    readers = [
        ObjectReader(f.conn, PickleCache(f.conn, 10 * len(f.oids)), f.reader._factory)
        for _ in range(MAPPING_REPEAT)
    ]
    p = f.mapping_pickle
    t = perf_counter()
    for reader in readers:
        reader.getState(p)
    return perf_counter() - t


def pass_callbacks(f):
    load = f.reader._persistent_load
    t = perf_counter()
    for ref in f.refs:
        load(ref)
    return perf_counter() - t


def pass_cache_get(f):
    get = f.cache.get
    t = perf_counter()
    for oid in f.oids:
        get(oid, None)
    return perf_counter() - t


def pass_minimize(f):
    t = perf_counter()
    f.conn.cacheMinimize()
    return perf_counter() - t


# The rounds.


def hit_round(f) -> dict:
    """Time each layer once on a warm client cache. Return seconds per
    item."""
    n = len(f.oids)
    out = {}
    out["access"] = pass_touch(f) / n
    out["minimize"] = pass_minimize(f) / n
    out["touch"] = pass_touch(f) / n
    for name, run in (
        ("empty", pass_empty),
        ("mvcc", pass_mvcc),
        ("client", pass_client),
        ("cache", pass_cache),
        ("getstate", pass_getstate),
        ("unpickler", pass_unpickler),
        ("class", pass_class),
        ("find_global", pass_find_global),
        ("setstate", pass_setstate),
        ("book", pass_book),
        ("callback", pass_callbacks),
        ("cache_get", pass_cache_get),
    ):
        out[name] = run(f) / n
    refs = len(f.refs) * MAPPING_REPEAT
    out["mapping"] = pass_mapping(f) / refs
    out["mapping_new"] = pass_mapping_new(f) / refs
    return out


def miss_round(f) -> dict:
    """Time the layers that reach the client cache on an empty one, and
    the prefetch of the native client. Return seconds per item."""
    n = len(f.oids)
    out = {}
    f.conn.cacheMinimize()
    f.clear()
    before = f.server_loads()
    out["touch"] = pass_touch(f) / n
    after = f.server_loads()
    out["touch_loads"] = None if before is None else (after - before) / n
    # The cache of ZEO answers None on a miss, so its layer of the cache
    # runs on a hit only.
    layers = [("mvcc", pass_mvcc), ("client", pass_client)]
    if f.client == "native":
        layers.append(("cache", pass_cache))
    for name, run in layers:
        f.clear()
        out[name] = run(f) / n
    if f.client == "native":
        out.update(prefetch_round(f))
    return out


def prefetch_round(f) -> dict:
    """The miss round with a prefetch of every oid first: once with a wait
    for the replies before the touch, once with the touch at once."""
    n = len(f.oids)
    out = {}
    f.conn.cacheMinimize()
    f.clear()
    t = perf_counter()
    f.storage.prefetch(f.oids, f.start)
    while f.entries() < n:
        pass
    waited = perf_counter() - t
    out["prefetch_touch_wait"] = (waited + pass_touch(f)) / n
    out["prefetch_wait"] = waited / n
    f.conn.cacheMinimize()
    f.clear()
    before = f.server_loads()
    t = perf_counter()
    f.storage.prefetch(f.oids, f.start)
    _touch(f.objs)
    out["prefetch_touch"] = (perf_counter() - t) / n
    # Wait for the prefetches still in flight before the count.
    deadline = time.monotonic() + 10
    while f.entries() < n and time.monotonic() < deadline:
        time.sleep(0.001)
    time.sleep(0.05)
    after = f.server_loads()
    out["prefetch_touch_loads"] = (after - before) / n
    return out


def terms_of(hit: dict, miss: dict | None) -> dict:
    """Return the terms of one load, in seconds, from the layers of one
    round."""
    net = {k: hit[k] - hit["empty"] for k in ("mvcc", "client", "cache", "getstate")}
    net.update(
        {
            k: hit[k] - hit["empty"]
            for k in ("unpickler", "class", "find_global", "setstate", "book")
        }
    )
    load = hit["touch"] - hit["access"]
    terms = {
        "load": load,
        "client cache": net["cache"],
        "ClientStorage.loadBefore": net["client"] - net["cache"],
        "MVCCAdapterInstance.load": net["mvcc"] - net["client"],
        "unpickler setup": net["unpickler"],
        "class (find_global)": net["class"] - net["unpickler"],
        "state decode": net["getstate"] - net["class"],
        "__setstate__": net["setstate"],
        "bookkeeping": net["book"],
        "unghostify, setstate frames": load
        - net["mvcc"]
        - net["getstate"]
        - net["setstate"]
        - net["book"],
        "find_global alone": net["find_global"],
        "access of a loaded object": hit["access"],
        "cacheMinimize per object": hit["minimize"],
        "callback per reference": hit["callback"] - hit["empty"],
        "cache.get per reference": hit["cache_get"] - hit["empty"],
        "mapping decode per reference": hit["mapping"],
        "mapping decode per new ghost": hit["mapping_new"],
    }
    if miss is not None:
        terms["miss load"] = miss["touch"] - hit["access"]
        terms["miss: round trip and store"] = miss["client"] - hit["client"]
        terms["miss: MVCCAdapterInstance.load"] = miss["mvcc"] - miss["client"]
        for key in ("prefetch_touch_wait", "prefetch_wait", "prefetch_touch"):
            if key in miss:
                terms[key.replace("_", " ")] = miss[key] - (
                    hit["access"] if "touch" in key else 0
                )
    return terms


# The counts.


def call_counts(f, miss: bool) -> dict:
    """Count the Python calls and the C calls of one touch pass, per load."""
    n = len(f.oids)
    f.conn.cacheMinimize()
    if miss:
        f.clear()
    calls = Counter()
    c_calls = Counter()

    def profile(frame, event, arg):
        if event == "call":
            code = frame.f_code
            calls[f"{Path(code.co_filename).name}:{code.co_qualname}"] += 1
        elif event == "c_call":
            c_calls[getattr(arg, "__qualname__", repr(arg))] += 1

    sys.setprofile(profile)
    try:
        _touch(f.objs)
    finally:
        sys.setprofile(None)
    own = f"{Path(__file__).name}:{TIMED}"
    calls.pop(own, None)
    return {
        "python_calls_per_load": sum(calls.values()) / n,
        "c_calls_per_load": sum(c_calls.values()) / n,
        "python": {k: v / n for k, v in calls.most_common()},
        "c": {k: v / n for k, v in c_calls.most_common(25)},
    }


def record_counts(f) -> dict:
    """The callbacks of one load of each record of the corpus, by class."""
    out = {}
    out[".".join(f.class_name)] = {
        "records": len(f.pickles),
        "callbacks_per_record": sum(len(f.references(p)) for p in f.pickles)
        / len(f.pickles),
        "pickle_bytes": statistics.median(len(p) for p in f.pickles),
        "python_setstate": python_setstate(type(f.objs[0])),
    }
    klass = type(f.mapping)
    out[f"{klass.__module__}.{klass.__name__}"] = {
        "records": 1,
        "callbacks_per_record": len(f.refs),
        "pickle_bytes": len(f.mapping_pickle),
        "python_setstate": python_setstate(klass),
    }
    return out


def python_setstate(klass) -> bool:
    """Whether `klass.__setstate__` is Python code."""
    return inspect.isfunction(getattr(klass, "__setstate__", None))


# The command `terms`.


def machine() -> dict:
    from benchcommon.provenance import cpu_model

    return {
        "cpu": cpu_model(),
        "logical_cpus": os.cpu_count(),
        "kernel": platform.release(),
        "python": platform.python_version(),
        "free_threaded": bool(sysconfig.get_config_var("Py_GIL_DISABLED")),
        "gil_enabled": sys._is_gil_enabled(),
    }


def summary(values: list[float]) -> dict:
    """The median and the range, in µs."""
    us = [v * 1e6 for v in values]
    return {"median": statistics.median(us), "low": min(us), "high": max(us)}


def run_terms(args) -> dict:
    result = {"machine": machine(), "cache_size": CACHE_SIZE, "rows": []}
    for client in args.client:
        for size in args.size:
            objects = args.objects or OBJECTS[size]
            rounds = args.rounds or ROUNDS[size]
            with tempfile.TemporaryDirectory(dir=args.workdir) as tmp:
                result["rows"].append(one_row(client, size, objects, rounds, Path(tmp)))
            print_row(result["rows"][-1])
    result["machine"]["gil_enabled_at_end"] = machine()["gil_enabled"]
    return result


def one_row(client: str, size: int, objects: int, rounds: int, workdir: Path) -> dict:
    load_before = os.getloadavg()
    with opened(client, size, objects, workdir) as f:
        # One round of each before the timed rounds warms them up.
        hit_round(f)
        miss_round(f)
        hits, misses, terms = [], [], defaultdict(list)
        for _ in range(rounds):
            hit = hit_round(f)
            miss = miss_round(f)
            hits.append(hit)
            misses.append(miss)
            for name, value in terms_of(hit, miss).items():
                terms[name].append(value)
        counts = {
            "hit": call_counts(f, miss=False),
            "miss": call_counts(f, miss=True),
            "records": record_counts(f),
        }
        loads = [m["touch_loads"] for m in misses if m.get("touch_loads") is not None]
        prefetch_loads = [
            m["prefetch_touch_loads"] for m in misses if "prefetch_touch_loads" in m
        ]
        pickle_bytes = statistics.median(len(p) for p in f.pickles)
    return {
        "client": client,
        "size": size,
        "objects": objects,
        "rounds": rounds,
        "pickle_bytes": pickle_bytes,
        "loadavg_before": load_before,
        "loadavg_after": os.getloadavg(),
        "terms": {name: summary(values) for name, values in terms.items()},
        "hit_layers": {k: summary([h[k] for h in hits]) for k in hits[0]},
        "miss_layers": {
            k: summary([m[k] for m in misses])
            for k in misses[0]
            if not k.endswith("_loads")
        },
        "server_loads_per_miss": statistics.median(loads) if loads else None,
        "server_loads_per_prefetch_touch": (
            statistics.median(prefetch_loads) if prefetch_loads else None
        ),
        "counts": counts,
    }


def print_row(row: dict) -> None:
    print(
        f"\n{row['client']} {row['size']} B ({row['pickle_bytes']} B pickles), "
        f"{row['objects']} objects, {row['rounds']} rounds, "
        f"load {row['loadavg_before'][0]:.2f} -> {row['loadavg_after'][0]:.2f}"
    )
    for name, s in row["terms"].items():
        print(f"  {name:34} {s['median']:9.3f} µs  ({s['low']:.3f}-{s['high']:.3f})")
    hit, miss = row["counts"]["hit"], row["counts"]["miss"]
    print(
        f"  python calls per load: hit {hit['python_calls_per_load']:.1f}, "
        f"miss {miss['python_calls_per_load']:.1f}; "
        f"server loads per miss {row['server_loads_per_miss']}, "
        f"per prefetch touch {row['server_loads_per_prefetch_touch']}"
    )


# The command `loop`.


def run_loop(args) -> int:
    if args.py_spy:
        command = [
            "py-spy",
            "record",
            "--native",
            "--format",
            "raw",
            "--rate",
            str(args.rate),
            "--output",
            str(args.py_spy),
            "--",
            sys.executable,
            "-m",
            "shootout.loadpath",
            *loop_args(args),
        ]
        return subprocess.call(command)
    with tempfile.TemporaryDirectory(dir=args.workdir) as tmp:
        with opened(
            args.client[0],
            args.size[0],
            args.objects or OBJECTS[args.size[0]],
            Path(tmp),
        ) as f:
            profiler = None
            if args.cprofile:
                import cProfile

                profiler = cProfile.Profile()
            deadline = time.monotonic() + args.seconds
            rounds = 0
            while time.monotonic() < deadline:
                f.conn.cacheMinimize()
                if args.kind == "miss":
                    f.clear()
                if profiler is not None:
                    profiler.enable()
                _touch(f.objs)
                if profiler is not None:
                    profiler.disable()
                rounds += 1
            if profiler is not None:
                profiler.dump_stats(args.cprofile)
    print(f"{rounds} rounds of {args.kind}", file=sys.stderr)
    return 0


def loop_args(args) -> list[str]:
    out = ["loop", "--kind", args.kind, "--seconds", str(args.seconds)]
    out += ["--client", args.client[0], "--size", str(args.size[0])]
    out += ["--workdir", str(args.workdir)]
    if args.objects:
        out += ["--objects", str(args.objects)]
    return out


# The command `frames`.

# The frames of a load: a label, the function that py-spy names, and the
# end of the path of its file. A function that starts with `*` matches any
# function whose name holds the rest, as a mangled Rust name does.
FRAMES = (
    ("unghostify (C)", "unghostify", "cPersistence.c"),
    ("Connection.setstate", "setstate", "ZODB/Connection.py"),
    ("MVCCAdapterInstance.load", "load", "ZODB/mvccadapter.py"),
    ("ClientStorage.loadBefore", "loadBefore", "zoe/zeo/_native.py"),
    ("ClientStorage._with_server", "_with_server", "zoe/zeo/_native.py"),
    ("ZEO ClientStorage.loadBefore", "loadBefore", "ZEO/ClientStorage.py"),
    ("ZEO.cache loadBefore", "loadBefore", "ZEO/cache.py"),
    ("ObjectReader.setGhostState", "setGhostState", "ZODB/serialize.py"),
    ("ObjectReader.getState", "getState", "ZODB/serialize.py"),
    ("ObjectReader._get_unpickler", "_get_unpickler", "ZODB/serialize.py"),
    ("find_global (closure)", "find_global", "ZODB/serialize.py"),
    ("DB.classFactory", "classFactory", "ZODB/DB.py"),
    ("broken.find_global", "find_global", "ZODB/broken.py"),
    ("ObjectReader._persistent_load", "_persistent_load", "ZODB/serialize.py"),
    ("PersistentUnpickler", "PersistentUnpickler", "ZODB/_compat.py"),
    ("Unpickler.__init__", "__init__", "ZODB/_compat.py"),
    ("C unpickler load", "load", "_pickle_33.c"),
    ("Persistent.__setstate__ (C)", "pickle___setstate__", "cPersistence.c"),
    ("Client.load_before (Rust)", "*___pymethod_load_before__", ""),
    ("Cache.load_before_with (Rust)", "*Cache16load_before_with", ""),
    ("getpid", "getpid", "libc.so.6"),
)


def run_frames(args) -> int:
    """Print the share of the samples under the root function of each
    frame of a load, inclusive, and the frames with the most own samples."""
    root = args.root
    total = 0
    inclusive = Counter()
    own = Counter()
    for line in Path(args.file).read_text().splitlines():
        stack, _, count = line.rpartition(" ")
        if not stack:
            continue
        frames = stack.split(";")
        inside = [i for i, frame in enumerate(frames) if frame.startswith(f"{root} ")]
        if not inside:
            continue
        frames = frames[inside[0] + 1 :]
        n = int(count)
        total += n
        seen = set()
        for frame in frames:
            name, where = split_frame(frame)
            for label, function, path in FRAMES:
                if function.startswith("*"):
                    match = function[1:] in name
                else:
                    match = name == function and where.endswith(path)
                if match and label not in seen:
                    seen.add(label)
                    inclusive[label] += n
        own[short(frames[-1]) if frames else root] += n
    print(f"{total} samples under {root}")
    for label, _, _ in FRAMES:
        if inclusive[label]:
            print(f"  {label:34} {100 * inclusive[label] / total:5.1f} %")
    print("own samples:")
    for frame, n in own.most_common(args.top):
        print(f"  {100 * n / total:5.1f} %  {frame}")
    return 0


def split_frame(frame: str) -> tuple[str, str]:
    """The function and the file of a frame of py-spy, without the line."""
    name, _, where = frame.partition(" (")
    return name, where.rstrip(")").rsplit(":", 1)[0]


def short(frame: str) -> str:
    name, where = split_frame(frame)
    return f"{name} ({'/'.join(where.split('/')[-2:])})"


# The command `scan`.


def run_scan(args) -> int:
    """Count the references of each current record of a FileStorage."""
    from ZODB._compat import PersistentUnpickler
    from ZODB.FileStorage import FileStorage
    from ZODB.utils import get_pickle_metadata

    storage = FileStorage(str(args.file), read_only=True)
    by_class = defaultdict(lambda: {"records": 0, "bytes": 0, "refs": Counter()})
    try:
        for oid in storage._index.keys():
            pickle, _ = storage.load(oid)
            module, name = get_pickle_metadata(pickle)
            entry = by_class[f"{module}.{name}"]
            entry["records"] += 1
            entry["bytes"] += len(pickle)
            refs = []
            unpickler = PersistentUnpickler(None, refs.append, io.BytesIO(pickle))
            unpickler.noload()
            unpickler.noload()
            for ref in refs:
                entry["refs"][kind_of(ref)] += 1
    finally:
        storage.close()
    rows = []
    for name, entry in by_class.items():
        n = entry["records"]
        row = {
            "class": name,
            "records": n,
            "bytes_per_record": entry["bytes"] / n,
            "callbacks_per_record": sum(entry["refs"].values()) / n,
            "refs": dict(entry["refs"]),
        }
        if args.classes:
            row["python_setstate"] = setstate_of(name)
        rows.append(row)
    rows.sort(key=lambda r: -r["records"])
    records = sum(r["records"] for r in rows)
    callbacks = sum(r["callbacks_per_record"] * r["records"] for r in rows)
    out = {
        "records": records,
        "callbacks_per_record": callbacks / records if records else 0,
        "classes": rows,
    }
    text = json.dumps(out, indent=1)
    if args.out:
        Path(args.out).write_text(text + "\n")
    else:
        print(text)
    return 0


def kind_of(ref) -> str:
    """The kind of a persistent id, as `ObjectReader._persistent_load`
    reads it."""
    if isinstance(ref, tuple):
        return "oid and class"
    if isinstance(ref, (bytes, str)):
        return "oid"
    if isinstance(ref, list):
        if len(ref) == 1:
            return "weak"
        return {"w": "weak", "m": "other database", "n": "other database"}.get(
            ref[0], "other"
        )
    return "other"


def setstate_of(name: str) -> bool | None:
    """Whether the class `name` has a `__setstate__` in Python, or None
    when it does not import."""
    from ZODB.broken import Broken, find_global

    module, _, attr = name.rpartition(".")
    klass = find_global(module, attr)
    if isinstance(klass, type) and issubclass(klass, Broken):
        return None
    return python_setstate(klass)


# The command line.


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="shootout.loadpath", description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    for name in ("terms", "loop"):
        cmd = commands.add_parser(name)
        cmd.add_argument("--client", nargs="+", choices=CLIENTS, default=list(CLIENTS))
        cmd.add_argument(
            "--size",
            nargs="+",
            type=int,
            choices=sorted(OBJECTS),
            default=sorted(OBJECTS),
        )
        cmd.add_argument(
            "--objects",
            type=int,
            help="objects per row (default: 1000 at 1 KB, 200 at 100 KB)",
        )
        cmd.add_argument("--workdir", type=Path, default=Path("target/bench/work"))
    terms = commands.choices["terms"]
    terms.add_argument(
        "--rounds", type=int, help="rounds per row (default: 30 at 1 KB, 15 at 100 KB)"
    )
    terms.add_argument("--out", type=Path, help="write the result as JSON there")
    loop = commands.choices["loop"]
    loop.add_argument("--kind", choices=("hit", "miss"), default="hit")
    loop.add_argument("--seconds", type=float, default=20)
    loop.add_argument("--py-spy", type=Path, help="run under py-spy, raw samples there")
    loop.add_argument("--rate", type=int, default=997, help="py-spy samples per second")
    loop.add_argument(
        "--cprofile", type=Path, help="run the touch under cProfile, stats there"
    )
    frames = commands.add_parser("frames")
    frames.add_argument("file", type=Path)
    frames.add_argument("--top", type=int, default=25)
    frames.add_argument(
        "--root", default=TIMED, help="count the samples under this function only"
    )
    scan = commands.add_parser("scan")
    scan.add_argument("file", type=Path, help="a FileStorage Data.fs")
    scan.add_argument("--classes", action="store_true")
    scan.add_argument("--out", type=Path)
    args = parser.parse_args(argv)
    if args.command == "terms":
        args.workdir.mkdir(parents=True, exist_ok=True)
        gc.collect()
        result = run_terms(args)
        if args.out:
            args.out.parent.mkdir(parents=True, exist_ok=True)
            args.out.write_text(json.dumps(result, indent=1) + "\n")
        return 0
    if args.command == "loop":
        args.workdir.mkdir(parents=True, exist_ok=True)
        return run_loop(args)
    if args.command == "frames":
        return run_frames(args)
    return run_scan(args)


if __name__ == "__main__":
    sys.exit(main())
