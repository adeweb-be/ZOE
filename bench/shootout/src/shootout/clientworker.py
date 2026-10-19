"""One round of one row of the client benchmark, for one client.

`python -m shootout.clientworker SPEC` reads the JSON file SPEC, runs the
workload, and writes one JSON line to stdout: the start and the end of the
round on the monotonic clock, the client CPU in seconds, the calls, the
objects, and the time of each call.

In the threads mode, the workers are threads of this process on one
client. In the processes mode, this process starts one process per worker,
each with its own client, and waits for them. It opens no client itself.

Each round runs in a fresh process. The native client imports ZEO too,
so the processes of both clients load ZEO and ZODB. Each process imports
its client and the modules of the calls before the clock starts. So the
clock of a warm round covers the open of the client and the verification
of its cache, and no import.
"""

import functools
import importlib
import json
import logging
import multiprocessing
import resource
import sys
import threading
import time
from queue import Empty

# The records of one commit.
RECORDS_PER_COMMIT = 100
# The step between the starts of two records in the random buffer.
RECORD_STEP = 977
ZEO_CLIENT = "ZEO.ClientStorage"
NATIVE_CLIENT = "zoe.zeo.ClientStorage"
WAIT_TIMEOUT = 120
# For each client: its module, its class, the other modules that its open
# and its calls import, and the arguments that select it.
CLIENT_CLASSES = {
    # ZEO imports msgpack in its handshake, and asyncio encodes the host
    # with the idna codec when it connects. The native client imports
    # zoe.zodb when it opens. A warm round would count these imports.
    ZEO_CLIENT: (
        "ZEO.ClientStorage",
        "ClientStorage",
        ("msgpack", "encodings.idna"),
        {},
    ),
    NATIVE_CLIENT: (
        "zoe.zeo",
        "ClientStorage",
        ("zoe._zoe", "zoe.zodb"),
        {"native": True},
    ),
}
# The modules that the calls of both clients import.
CALL_MODULES = ("ZODB.utils", "ZODB.Connection")


def cpu() -> float:
    """Return the CPU time of this process and its threads, in seconds."""
    usage = resource.getrusage(resource.RUSAGE_SELF)
    return usage.ru_utime + usage.ru_stime


def default_commits(objects: int) -> int:
    """Return the commits of one worker in a commit round, by default."""
    return max(1, objects // 50)


def client_class(spec: dict):
    """Import the client of the spec and the modules of its calls.

    Return a callable that opens the client. The caller calls this function
    before the clock starts.
    """
    module, name, others, kw = CLIENT_CLASSES[spec["client"]]
    for other in CALL_MODULES + others:
        importlib.import_module(other)
    return functools.partial(getattr(importlib.import_module(module), name), **kw)


def check_no_imports(before: set[str]) -> None:
    """Fail when a module was imported after `before` was taken.

    A warm round calls this at the end of its clock: an import inside the
    clock would count in the time of one client only.
    """
    imported = sorted(set(sys.modules) - before)
    if imported:
        raise RuntimeError(
            "the clock of a warm round covers imports: " + ", ".join(imported)
        )


def open_client(spec: dict, worker: int, opener):
    """Open the client of the spec with opener, with the cache file of the
    worker."""
    host, port = spec["address"]
    kw = {
        "storage": "1",
        "cache_size": spec["cache_size"],
        "wait": True,
        "wait_timeout": WAIT_TIMEOUT,
    }
    if spec["workload"] != "commit":
        # The cache is a file. The cold round starts with none and writes
        # it. The warm round reopens it.
        kw["client"] = f"w{worker}"
        kw["var"] = spec["var"]
    return opener((host, port), **kw)


def oids_of(spec: dict, worker: int) -> list[bytes]:
    """Return the oids that the worker loads."""
    objects = spec["objects"]
    with open(spec["oids"], "rb") as file:
        file.seek(worker * objects * 8)
        raw = file.read(objects * 8)
    return [raw[i : i + 8] for i in range(0, len(raw), 8)]


def records_of(size: int, seed: int) -> list[bytes]:
    """Return the records of one commit: distinct random bytes of size."""
    import random

    buffer = random.Random(seed).randbytes(2 * size)
    starts = [(k * RECORD_STEP) % size for k in range(RECORDS_PER_COMMIT)]
    return [buffer[s : s + size] for s in starts]


def load(storage, oids: list[bytes]) -> list[float]:
    """Load each oid once. Return the time of each load.

    Each load is current before the tid after the last transaction, as the
    loads of a ZODB connection are. Both caches serve such a load from a
    current entry.
    """
    from ZODB.utils import p64, u64

    before = p64(u64(storage.lastTransaction()) + 1)
    times = []
    clock = time.perf_counter
    for oid in oids:
        start = clock()
        storage.loadBefore(oid, before)
        times.append(clock() - start)
    return times


def commit(storage, records: list[bytes], commits: int) -> list[float]:
    """Commit `commits` transactions of new records. Return their times.

    A call is new_oid for each record, tpc_begin, store for each record,
    tpc_vote and tpc_finish.
    """
    from ZODB.Connection import TransactionMetaData
    from ZODB.utils import z64

    times = []
    clock = time.perf_counter
    for _ in range(commits):
        start = clock()
        oids = [storage.new_oid() for _ in records]
        txn = TransactionMetaData()
        storage.tpc_begin(txn)
        for oid, data in zip(oids, records, strict=True):
            storage.store(oid, z64, data, "", txn)
        storage.tpc_vote(txn)
        storage.tpc_finish(txn)
        times.append(clock() - start)
    return times


def work(spec: dict, storage, worker: int, prepared) -> list[float]:
    """Run the workload of one worker on an open client."""
    if spec["workload"] == "commit":
        return commit(storage, prepared, spec["commits"])
    return load(storage, prepared)


def prepare(spec: dict, worker: int):
    """Return the oids or the records of one worker, before the clock starts."""
    if spec["workload"] == "commit":
        return records_of(spec["record_size"], worker)
    return oids_of(spec, worker)


def run_threads(spec: dict) -> dict:
    workers = spec["concurrency"]
    prepared = [prepare(spec, w) for w in range(workers)]
    times = [None] * workers
    opener = client_class(spec)
    warm = spec["workload"] == "warm"
    storage = None if warm else open_client(spec, 0, opener)
    modules = set(sys.modules)
    start, cpu_start = time.monotonic(), cpu()
    if warm:
        # The open and its verification count in a warm round.
        storage = open_client(spec, 0, opener)

    def target(w):
        times[w] = work(spec, storage, w, prepared[w])

    threads = [threading.Thread(target=target, args=(w,)) for w in range(workers)]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()
    end, cpu_end = time.monotonic(), cpu()
    if warm:
        check_no_imports(modules)
    storage.close()
    if any(t is None for t in times):
        raise RuntimeError("a worker thread failed")
    return _result(
        spec, start, end, cpu_end - cpu_start, [t for ts in times for t in ts]
    )


def _process(spec: dict, worker: int, barrier, queue) -> None:
    """Run one worker process: open its client, wait for the others, work."""
    logging.basicConfig(level=logging.WARNING)
    try:
        prepared = prepare(spec, worker)
        opener = client_class(spec)
        warm = spec["workload"] == "warm"
        storage = None if warm else open_client(spec, worker, opener)
        modules = set(sys.modules)
        barrier.wait()
        start, cpu_start = time.monotonic(), cpu()
        if warm:
            storage = open_client(spec, worker, opener)
        times = work(spec, storage, worker, prepared)
        end, cpu_end = time.monotonic(), cpu()
        if warm:
            check_no_imports(modules)
        storage.close()
        queue.put((worker, start, end, cpu_end - cpu_start, times, None))
    except BaseException as error:
        barrier.abort()
        queue.put((worker, 0.0, 0.0, 0.0, [], repr(error)))
        raise


def run_processes(spec: dict) -> dict:
    workers = spec["concurrency"]
    # A spawned process starts with no thread and no runtime of a client.
    context = multiprocessing.get_context("spawn")
    barrier = context.Barrier(workers)
    queue = context.Queue()
    processes = [
        context.Process(target=_process, args=(spec, w, barrier, queue))
        for w in range(workers)
    ]
    for process in processes:
        process.start()
    reports = []
    while len(reports) < workers:
        try:
            reports.append(queue.get(timeout=1))
        except Empty:
            # A process that dies before its report never sends one.
            dead = [p.exitcode for p in processes if p.exitcode not in (None, 0)]
            if dead:
                raise RuntimeError(
                    f"a worker process ended with the code {dead[0]}"
                ) from None
    for process in processes:
        process.join()
    errors = [report[5] for report in reports if report[5]]
    if errors:
        raise RuntimeError(f"a worker process failed: {errors[0]}")
    return _result(
        spec,
        min(r[1] for r in reports),
        max(r[2] for r in reports),
        sum(r[3] for r in reports),
        [t for r in reports for t in r[4]],
    )


def _result(spec: dict, start: float, end: float, cpu_seconds: float, times) -> dict:
    calls = len(times)
    if spec["workload"] == "commit":
        objects = calls * RECORDS_PER_COMMIT
    else:
        objects = calls
    return {
        "start": start,
        "end": end,
        "cpu_seconds": cpu_seconds,
        "calls": calls,
        "objects": objects,
        "times": times,
    }


def main(argv: list[str]) -> int:
    logging.basicConfig(level=logging.WARNING)
    with open(argv[0]) as file:
        spec = json.load(file)
    run = run_threads if spec["mode"] == "threads" else run_processes
    print(json.dumps(run(spec)), flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
