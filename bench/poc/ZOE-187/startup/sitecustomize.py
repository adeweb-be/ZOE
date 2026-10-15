"""POC ZOE-187 (not for merge): hooks of a zodbshootout run of read_hot.

`drive.py` puts this directory on PYTHONPATH after the startup directory
of the harness, which imports this module. With ZOE187_TRACE or
ZOE187_DIAG set, it replaces `bench_hot_read` of zodbshootout with a copy
that times the same reads and, outside the timed reads, records each loop.

- ZOE187_TRACE=DIR: each worker writes a JSON file to DIR: the times of
  its open, prime and loops (CLOCK_MONOTONIC, comparable across processes),
  the CPU of each loop, the rusage of the thread for each loop, and the
  counters of the client cache before and after each loop.
- ZOE187_BARRIER=DIR:N: each worker waits after its prime until N workers
  primed, so the timed loops of the workers run together.
- ZOE187_PIN=CPUS: each worker runs on these CPUs only (a list for
  `os.sched_setaffinity`, such as 0-11).
- ZOE187_TERMS=1: after the loops, each worker times the terms of a hit
  per object: loadBefore of the storage, the call of the native client or
  of ZEO's cache, `getState` of the reader and a copy of the bytes.
- ZOE187_DIAG=same|copy: after the prime, `loadBefore` of the storage
  instance serves current records from a dict of the process that holds
  the bytes and the serials that the real `loadBefore` gave. `same` gives
  the same bytes object, `copy` a new copy of it. A record serves only a
  snapshot after its start. At the end, each record is checked against the
  real `loadBefore`, and the last tid must not have moved.
"""

import ctypes
import json
import os
import resource
import threading
import time

_TRACE = os.environ.get("ZOE187_TRACE")
_BARRIER = os.environ.get("ZOE187_BARRIER")
_PIN = os.environ.get("ZOE187_PIN")
_TERMS = os.environ.get("ZOE187_TERMS")
_DIAG = os.environ.get("ZOE187_DIAG")
_STARTED = time.perf_counter()
_getcpu = ctypes.CDLL(None).sched_getcpu


def _stats(storage):
    """Return the counters of the cache of the storage client."""
    out = {"last": storage.lastTransaction().hex()}
    native = getattr(storage, "_native", None)
    if native is not None:
        poc = getattr(native, "_poc_cache_stats", None)
        if poc is not None:
            out.update(poc())
        return out
    cache = getattr(storage, "_cache", None)
    if cache is not None and hasattr(cache, "getStats"):
        adds, added, evicts, evicted, accesses = cache.getStats()
        out.update(adds=adds, added_bytes=added, evicts=evicts, evicted_bytes=evicted,
                   accesses=accesses)
    return out


def _usage():
    r = resource.getrusage(resource.RUSAGE_THREAD)
    return (r.ru_utime, r.ru_stime, r.ru_nvcsw, r.ru_nivcsw, r.ru_minflt, r.ru_majflt)


_diag_lock = threading.Lock()


def _install_diag(storage):
    """Make `loadBefore` of storage serve from a dict, once per instance."""
    with _diag_lock:
        if "_zoe187_table" in storage.__dict__:
            return
        real = storage.loadBefore
        table = {}
        copy = _DIAG == "copy"

        def loadBefore(oid, tid):
            hit = table.get(oid)
            if hit is not None and hit[1] < tid:
                data = bytes(memoryview(hit[0])) if copy else hit[0]
                return data, hit[1], None
            result = real(oid, tid)
            if result is not None and result[2] is None:
                table[oid] = result
            return result

        storage._zoe187_table = table
        storage._zoe187_real = real
        storage._zoe187_last = storage.lastTransaction()
        storage.loadBefore = loadBefore


def _check_diag(storage):
    """Check each record of the dict against the real loadBefore."""
    table = storage._zoe187_table
    real = storage._zoe187_real
    assert storage.lastTransaction() == storage._zoe187_last, "a commit came"
    before = (int.from_bytes(storage._zoe187_last, "big") + 1).to_bytes(8, "big")
    for oid, (data, start, end) in list(table.items()):
        got = real(oid, before)
        assert got[0] == data and got[1] == start and got[2] is None, oid
    return len(table)


def _cpus(text):
    cpus = set()
    for part in text.split(","):
        first, _, last = part.partition("-")
        cpus.update(range(int(first), int(last or first) + 1))
    return cpus


def _barrier(name):
    """Wait until the next N workers reached the barrier `name`.

    The warmup and the value of a run pass each barrier in turn, so a
    worker waits for the next multiple of N arrivals.
    """
    folder, count = _BARRIER.rsplit(":", 1)
    count = int(count)
    folder = os.path.join(folder, name)
    os.makedirs(folder, exist_ok=True)
    open(os.path.join(folder, f"{os.getpid()}-{threading.get_native_id()}-{os.urandom(3).hex()}"), "x").close()
    target = ((len(os.listdir(folder)) - 1) // count + 1) * count
    while len(os.listdir(folder)) < target:
        time.sleep(0.0005)


def _per_object(call, items, reps=7):
    """Return the median time of call per item over reps passes, in seconds."""
    times = []
    for _ in range(reps):
        begin = time.perf_counter()
        for item in items:
            call(item)
        times.append((time.perf_counter() - begin) / len(items))
    times.sort()
    return times[len(times) // 2]


def _terms(storage, conn, m):
    oids = [obj._p_oid for obj in m.values()]
    before = conn._storage._start
    load = storage.__dict__.get("_zoe187_real", storage.loadBefore)
    out = {"loadBefore": _per_object(lambda oid: load(oid, before), oids)}
    native = getattr(storage, "_native", None)
    if native is not None:
        out["native"] = _per_object(lambda oid: native.load_before(oid, before), oids)
    else:
        cache = storage._cache
        out["zeo_cache"] = _per_object(lambda oid: cache.loadBefore(oid, before), oids)
    out["mvcc_load"] = _per_object(conn._storage.load, oids)
    datas = [load(oid, before)[0] for oid in oids]
    reader = conn._reader
    out["getState"] = _per_object(reader.getState, datas)
    out["copy"] = _per_object(lambda d: bytes(memoryview(d)), datas)
    out["bytes"] = sum(map(len, datas)) / len(datas)
    return out


def _bench_hot_read(self, loops, db_factory):
    """`bench_hot_read` of zodbshootout 0.8.0, with records outside the clock."""
    perf_counter = time.perf_counter
    record = {"pid": os.getpid(), "thread": threading.get_native_id(),
              "worker": self.worker_number, "started": _STARTED,
              "diag": _DIAG, "loops": []}
    if _PIN:
        os.sched_setaffinity(0, _cpus(_PIN))
    record["open0"] = perf_counter()
    db = db_factory()
    record["open1"] = perf_counter()
    conn, root = self._SpeedTestWorker__prime_caches(db)
    record["primed"] = perf_counter()
    storage = db.storage
    if _DIAG:
        _install_diag(storage)
    if _BARRIER:
        _barrier("loops")
        record["released"] = perf_counter()

    duration = 0
    got = 0
    for i in range(loops * self.inner_loops):
        conn.cacheMinimize()
        self.sync_before_timing_loop("begin hot " + str(i))
        m = self.data.data_for_worker(root, self)
        if _TRACE:
            s0 = _stats(storage)
            u0 = _usage()
            c0 = _getcpu()
        begin = perf_counter()
        got += self.data.read_test_read_values(m.values())
        end = perf_counter()
        duration += end - begin
        if _TRACE:
            u1 = _usage()
            c1 = _getcpu()
            with open(f"/sys/devices/system/cpu/cpu{c1}/cpufreq/scaling_cur_freq") as f:
                mhz = int(f.read()) // 1000
            s1 = _stats(storage)
            record["loops"].append({
                "t0": begin, "t1": end, "cpu0": c0, "cpu1": c1, "mhz": mhz,
                "usage": [b - a for a, b in zip(u0, u1)],
                "stats0": s0, "stats1": s1,
            })

    if _TERMS:
        if _BARRIER:
            _barrier("terms")
        record["terms"] = _terms(storage, conn, m)
    self._SpeedTestWorker__check_access_count(got, loops * self.inner_loops)
    self._SpeedTestWorker__conn_did_load_objects(conn, loops)
    if _DIAG:
        with _diag_lock:
            if not storage.__dict__.get("_zoe187_checked"):
                storage._zoe187_checked = True
                record["diag_checked"] = _check_diag(storage)
    conn.close()
    db.close()
    record["end"] = perf_counter()
    record["maxrss_kb"] = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
    if _TRACE:
        record["final_stats"] = _stats(storage)
    if _TRACE:
        name = f"{os.getpid()}-{threading.get_native_id()}-{os.urandom(3).hex()}.json"
        with open(os.path.join(_TRACE, name), "x") as out:
            json.dump(record, out)
    return duration


if _TRACE or _DIAG:
    from zodbshootout import speedtest

    _bench_hot_read.inner_loops = True
    speedtest.SpeedTestWorker.bench_hot_read = _bench_hot_read
