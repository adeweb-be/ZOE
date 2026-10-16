"""Time the phases of one loop of the zodbshootout scenarios, threads c1.

Usage: python phases.py STORAGE SIZE LOOPS [SCENARIO...]

It starts the server of the harness for STORAGE, populates 100 objects
of SIZE bytes, and runs LOOPS loops of each scenario, as zodbshootout
does. It wraps the storage to count and time each call.
"""

import gc
import json
import os
import sys
import tempfile
import time
from collections import defaultdict
from pathlib import Path

import transaction
import ZODB.config
from persistent import Persistent
from persistent.mapping import PersistentMapping
from shootout.storages import STORAGES, zconfig

N = 100
pc = time.perf_counter


class PObject(Persistent):
    attr = 0

    def __init__(self, data):
        self.data = data


CALLS = defaultdict(lambda: [0, 0.0])
TIMING = [True]


def wrap(storage):
    """Time the calls of the storage that the DB makes."""
    if os.environ.get("SPIKE_NOWRAP"):
        return storage
    names = [
        "load",
        "loadBefore",
        "loadSerial",
        "tpc_begin",
        "store",
        "tpc_vote",
        "tpc_finish",
        "tpc_abort",
        "lastTransaction",
        "new_oid",
        "sync",
        "checkCurrentSerialInTransaction",
        "prefetch",
    ]
    for name in names:
        method = getattr(storage, name, None)
        if method is None:
            continue

        def timed(*args, _m=method, _n=name, **kw):
            begin = pc()
            try:
                return _m(*args, **kw)
            finally:
                if TIMING[0]:
                    cell = CALLS[_n]
                    cell[0] += 1
                    cell[1] += pc() - begin

        setattr(storage, name, timed)
    return storage


def open_db(text, wrapped=True):
    import io

    import ZConfig
    import ZODB

    config, _ = ZConfig.loadConfigFile(ZODB.config.getDbSchema(), io.StringIO(text))
    storage = config.database[0].config.storage.open()
    return ZODB.DB(wrap(storage) if wrapped else storage)


def populate(text, size):
    db = open_db(text)
    conn = db.open()
    root = conn.root()
    m = PersistentMapping()
    for i in range(N):
        m[i] = PObject(os.urandom(size))
    root["m"] = m
    transaction.commit()
    conn.close()
    db.close()


def phases_warm(text, loops):
    """bench_read_after_write with one DB, as zodbshootout's shared mode."""
    out = defaultdict(float)
    db = open_db(text)
    for _ in range(loops):
        TIMING[0] = True
        b = pc()
        conn = db.open()
        t1 = pc()
        root = conn.root()
        m = root["m"]
        values = list(m.values())
        t2 = pc()
        for obj in values:
            obj.attr = 1
        t3 = pc()
        transaction.commit()
        t4 = pc()
        got = sum(obj.attr for obj in values)
        transaction.commit()
        e = pc()
        assert got == N
        out["db.open"] += t1 - b
        out["root and mapping"] += t2 - t1
        out["100 loads (unghost)"] += t3 - t2
        out["commit of 100"] += t4 - t3
        out["read back + empty commit"] += e - t4
        out["total"] += e - b
        TIMING[0] = False
        conn.close()
        t = pc()
        conn.cacheMinimize()
        db.storage._cache.clear()
        gc.collect()
        out["clear (untimed)"] += pc() - t
    db.close()
    return out


def phases_cold(text, loops):
    out = defaultdict(float)
    db = open_db(text)
    for _ in range(loops):
        b = pc()
        conn = db.open()
        t1 = pc()
        m = conn.root()["m"]
        values = list(m.values())
        t2 = pc()
        got = sum(obj.attr >= 0 for obj in values)
        e = pc()
        assert got == N
        out["db.open"] += t1 - b
        out["root and mapping"] += t2 - t1
        out["100 loads (unghost)"] += e - t2
        out["total"] += e - b
        conn.close()
        conn.cacheMinimize()
        db.storage._cache.clear()
        gc.collect()
    db.close()
    return out


def phases_update(text, loops):
    out = defaultdict(float)
    db = open_db(text)
    tm = transaction.TransactionManager(explicit=True)
    conn = db.open(tm)
    for _ in range(loops):
        b = pc()
        tm.begin()
        m = conn.root()["m"]
        for obj in m.values():
            obj.attr = 1
        t = pc()
        tm.commit()
        e = pc()
        out["update values"] += t - b
        out["commit of 100"] += e - t
        out["total"] += e - b
    conn.close()
    db.close()
    return out


def phases_hot(text, loops):
    out = defaultdict(float)
    db = open_db(text)
    conn = db.open()
    m = conn.root()["m"]
    sum(obj.attr >= 0 for obj in m.values())
    for _ in range(loops):
        conn.cacheMinimize()
        b = pc()
        got = sum(obj.attr >= 0 for obj in m.values())
        e = pc()
        out["total"] += e - b
    conn.close()
    db.close()
    return out


def phases_add(text, loops, size):
    out = defaultdict(float)
    db = open_db(text)
    conn = db.open()
    for _ in range(loops):
        b = pc()
        transaction.begin()
        m = conn.root()["m"]
        for i in range(N):
            m[i] = PObject(os.urandom(size))
        t = pc()
        transaction.commit()
        e = pc()
        out["build"] += t - b
        out["commit of 100 new + mapping"] += e - t
        out["total"] += e - b
    conn.close()
    db.close()
    return out


def phases_fresh(text, loops):
    """bench_read_after_write as zodbshootout times it: a fresh DB for each
    sample, untimed, then the open, the update, the commit and the read."""
    out = defaultdict(float)
    for _ in range(loops):
        db = open_db(text, wrapped=False)
        b = pc()
        conn = db.open()
        m = conn.root()["m"]
        values = list(m.values())
        for obj in values:
            obj.attr = 1
        t3 = pc()
        tm = transaction.get()
        # The phases of the commit: sortKey (the first read of the address),
        # then the two-phase commit.
        t = pc()
        conn._storage.sortKey()
        t4 = pc()
        transaction.commit()
        t5 = pc()
        got = sum(obj.attr for obj in values)
        transaction.commit()
        e = pc()
        assert got == N
        out["open and 100 loads"] += t3 - b
        out["first sortKey"] += t4 - t
        out["commit of 100"] += t5 - t4
        out["total"] += e - b - (t4 - t)
        out["total with sortKey"] += e - b
        conn.close()
        db.close()
    return out


SCEN = {
    "warm": phases_warm,
    "fresh": phases_fresh,
    "update": phases_update,
    "cold": phases_cold,
    "hot": phases_hot,
}


def server_times(admin):
    """Return the sum and the count of the latency histograms of zoe serve."""
    if admin is None:
        return None
    import urllib.request

    text = urllib.request.urlopen(f"http://{admin}/metrics").read().decode()
    out = {}
    for line in text.splitlines():
        for kind in ("load", "vote", "finish"):
            base = f"zoe_{kind}_latency_seconds"
            for suffix, slot in (("_sum", 0), ("_count", 1)):
                if line.split("{")[0].split(" ")[0] == base + suffix:
                    out.setdefault(kind, [0.0, 0.0])[slot] += float(line.rsplit(" ", 1)[1])
    return out


def main():
    name, size, loops = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
    scenarios = sys.argv[4:] or ["warm", "update", "cold", "hot", "add"]
    with tempfile.TemporaryDirectory(dir=os.environ.get("SPIKE_DIR")) as d:
        with zconfig(STORAGES[name], Path(d), None) as text:
            populate(text, size)
            result = {}
            admin = None
            if name == "zoe-native":
                from shootout.storages import admin_address

                admin = admin_address(Path(d))
            for scenario in scenarios:
                before = server_times(admin)
                CALLS.clear()
                TIMING[0] = True
                if scenario == "add":
                    out = phases_add(text, loops, size)
                else:
                    out = SCEN[scenario](text, loops)
                per = {k: round(v / loops / N * 1e6, 1) for k, v in out.items()}
                calls = {
                    k: {"n": round(c / loops, 1), "us": round(t / c * 1e6, 1)}
                    for k, (c, t) in CALLS.items()
                }
                result[scenario] = {"per_object_us": per, "calls_per_loop": calls}
                after = server_times(admin)
                if after:
                    result[scenario]["server_mean_us"] = {
                        k: round((after[k][0] - before[k][0]) / max(after[k][1] - before[k][1], 1) * 1e6, 1)
                        for k in after
                    }
            print(json.dumps({"storage": name, "size": size, "loops": loops, **result}, indent=1))


if __name__ == "__main__":
    main()
