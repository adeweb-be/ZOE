"""The cost of a cache hit through ZODB: ZEO.cache against the prototype.

Not for merge. One process runs a ZEO server in a thread and a
`ZEO.ClientStorage` whose `cache` is either `ZEO.cache.ClientCache` (a
temporary file, as the `<zeoclient>` of the harness without `client`) or the
prototype. The objects are zodbshootout's `PObject` with its data. A round
does what `read_hot` does: `cacheMinimize`, then read `attr` of each object,
so each object costs one `loadBefore` that the cache of the client answers.

The kinds run in turns inside one round, so they share the state of the
machine. Output: one JSON line per kind with the µs per hit of each round.

Usage: python hit.py --objects 1000 --rounds 30 [--threads 1] [--size 1024]
"""

import argparse
import json
import os
import statistics
import sys
import tempfile
import threading
import time

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "cachex", "python"))

import transaction  # noqa: E402
import ZEO  # noqa: E402
import ZEO.cache  # noqa: E402
import ZODB  # noqa: E402
from ZEO.ClientStorage import ClientStorage  # noqa: E402
from zodbshootout._pobject import PObject  # noqa: E402
from zodbshootout._pobject import pobject_base_size  # noqa: E402
from zodbshootout.speedtest import SpeedTestData  # noqa: E402

import _cachex  # noqa: E402

MB = 1024 * 1024


class Proto:
    """The prototype with the attributes that ZEO.ClientStorage uses."""

    def __init__(self, **kw):
        c = _cachex.Cache(**kw)
        self.inner = c
        # Bound methods of the extension: no Python frame on a hit.
        self.loadBefore = c.loadBefore
        self.store = c.store
        self.invalidate = c.invalidate
        self.clear = c.clear
        self.contents = c.contents
        self._lock = threading.RLock()

    # ZEO's tpc_finish_co holds `_lock` across `setLastTid` and the call that
    # gives the invalidations to the DB, and ZEO's `getLastTid` takes the same
    # lock. So `lastTransaction` never gives T before the DB got T.
    def setLastTid(self, tid):
        with self._lock:
            self.inner.setLastTid(tid)

    def getLastTid(self):
        with self._lock:
            return self.inner.getLastTid()

    def __len__(self):
        return len(self.inner)

    def close(self):
        self.inner.close()


def make_cache(kind, tmp, mb=20):
    size = mb * MB
    if kind == "zeo":
        return ZEO.cache.ClientCache(None, size)
    if kind == "zeo-file":
        return ZEO.cache.ClientCache(os.path.join(tmp, "zeo.zec"), size)
    if kind == "proto-memory":
        return Proto(size=size)
    if kind == "proto-file":
        return Proto(path=os.path.join(tmp, "private.zoec"), size=size)
    if kind == "proto-shared":
        return Proto(path=os.path.join(tmp, "shared.zoec"), size=size, shared=True)
    if kind == "proto-shared-zstd":
        return Proto(
            path=os.path.join(tmp, "sharedz.zoec"),
            size=size,
            shared=True,
            compress_min=0,
            level=1,
        )
    raise ValueError(kind)


def populate(db, n, size, threads, kind):
    st = SpeedTestData(1, n, size)
    gen = st._random_data(max(10, size - pobject_base_size))
    conn = db.open()
    root = conn.root()
    for t in range(threads):
        root[f"{kind}-{t}"] = [PObject(next(gen)) for _ in range(n)]
    transaction.commit()
    conn.close()


def round_one(db, t, n):
    """One read_hot round in a connection of its own; returns seconds."""
    conn = db.open()
    objs = conn.root()[t]
    for o in objs:
        o.attr  # noqa: B018 the load is the point
    conn.cacheMinimize()
    begin = time.perf_counter()
    got = 0
    for o in objs:
        got += o.attr
    end = time.perf_counter()
    assert got == n
    conn.close()
    return end - begin


def run_threads(db, threads, n, kind):
    """Every thread reads its own objects. Returns the wall time from the
    first start to the last end, and the mean time of one thread, which is
    what zodbshootout counts."""
    barrier = threading.Barrier(threads)
    times = []

    def work(t):
        conn = db.open()
        objs = conn.root()[f"{kind}-{t}"]
        conn.cacheMinimize()
        barrier.wait()
        begin = time.perf_counter()
        got = 0
        for o in objs:
            got += o.attr
        assert got == n
        times.append((begin, time.perf_counter()))
        conn.close()

    ts = [threading.Thread(target=work, args=(t,)) for t in range(threads)]
    for x in ts:
        x.start()
    for x in ts:
        x.join()
    wall = max(e for _, e in times) - min(b for b, _ in times)
    return wall, sum(e - b for b, e in times) / len(times)


def bare(cache, oids, before):
    lb = cache.loadBefore
    begin = time.perf_counter()
    for oid in oids:
        lb(oid, before)
    return time.perf_counter() - begin


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--objects", type=int, default=1000)
    ap.add_argument("--size", type=int, default=1024)
    ap.add_argument("--rounds", type=int, default=30)
    ap.add_argument("--threads", type=int, default=1)
    ap.add_argument("--cache-mb", type=int, default=20)
    ap.add_argument(
        "--kinds",
        default="zeo,zeo-file,proto-memory,proto-file,proto-shared,proto-shared-zstd,mapping",
    )
    a = ap.parse_args()
    kinds = a.kinds.split(",")
    tmp = tempfile.mkdtemp(prefix="hit-")
    addr, stop = ZEO.server(threaded=True)
    dbs = {}
    for k in kinds:
        if k == "mapping":
            from ZODB.MappingStorage import MappingStorage

            db = ZODB.DB(MappingStorage(), pool_size=a.threads + 4)
        else:
            cs = ClientStorage(addr, cache=make_cache(k, tmp, a.cache_mb), wait=True)
            db = ZODB.DB(cs, pool_size=a.threads + 4)
        populate(db, a.objects, a.size, a.threads, k)
        dbs[k] = db
    # Warm every cache once: the populate commits wrote the root again.
    for k, db in dbs.items():
        for t in range(a.threads):
            round_one(db, f"{k}-{t}", a.objects)
    per_hit = {k: [] for k in kinds}
    per_thread = {k: [] for k in kinds}
    bare_hit = {k: [] for k in kinds}
    for _ in range(a.rounds):
        for k, db in dbs.items():
            if a.threads == 1:
                s = round_one(db, f"{k}-0", a.objects)
            else:
                s, mean_thread = run_threads(db, a.threads, a.objects, k)
                per_thread[k].append(mean_thread / a.objects * 1e6)
            per_hit[k].append(s / (a.objects * a.threads) * 1e6)
            if k != "mapping":
                cache = db.storage._cache
                conn = db.open()
                oids = [o._p_oid for o in conn.root()[f"{k}-0"]]
                before = ZODB.utils.p64(ZODB.utils.u64(db.storage.lastTransaction()) + 1)
                conn.close()
                bare_hit[k].append(bare(cache, oids, before) / len(oids) * 1e6)
    for k in kinds:
        v = per_hit[k]
        out = {
            "kind": k,
            "objects": a.objects,
            "size": a.size,
            "threads": a.threads,
            "rounds": a.rounds,
            "python": sys.version.split()[0] + ("t" if not sys._is_gil_enabled() else ""),
            "us_per_hit_median": round(statistics.median(v), 3),
            "us_per_hit_min": round(min(v), 3),
            "us_per_hit_max": round(max(v), 3),
            "cache_mb": a.cache_mb,
        }
        if per_thread[k]:
            t = per_thread[k]
            out["us_per_hit_one_thread_median"] = round(statistics.median(t), 3)
            out["us_per_hit_one_thread_min"] = round(min(t), 3)
            out["us_per_hit_one_thread_max"] = round(max(t), 3)
        if bare_hit[k]:
            b = bare_hit[k]
            out["us_bare_loadBefore_median"] = round(statistics.median(b), 3)
            out["us_bare_loadBefore_min"] = round(min(b), 3)
            out["us_bare_loadBefore_max"] = round(max(b), 3)
        if k != "mapping":
            db = dbs[k]
            conn = db.open()
            o = conn.root()[f"{k}-0"][0]
            o._p_activate()
            out["pickle_bytes"] = len(db.storage.loadBefore(o._p_oid, ZODB.utils.p64(ZODB.utils.u64(db.storage.lastTransaction()) + 1))[0])
            conn.close()
        if k.startswith("proto"):
            out["stats"] = dbs[k].storage._cache.inner.stats()
        print(json.dumps(out), flush=True)
    for db in dbs.values():
        db.close()
    stop()


if __name__ == "__main__":
    main()
