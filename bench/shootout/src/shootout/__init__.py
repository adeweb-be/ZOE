"""Run zodbshootout against the reference storages and ZOE.

`run` measures one storage and returns one result document. The document
validates against `bench/schema/results.schema.json`. `disk_needed` gives
the free disk space that one value of a cell needs. `check_cold_loads`
checks that the server of `zoe-native` loaded each object that a
`read_cold` value read.
"""

import datetime
import os
import re
import shutil
import statistics
import subprocess
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path

import pyperf

from shootout import environment, gil
from shootout.storages import (
    COUNTS_COLD_LOADS,
    RELSTORAGE,
    STORAGES,
    admin_address,
    empty_database,
    reset_database,
    server_loads,
    server_versions,
    zconfig,
)

SCHEMA_VERSION = 1
OBJECTS_PER_TRANSACTION = 100
KB = 1024
# The bound of disk_needed counts this much more than the object size for
# each record: the excess of the pickle, the record header and the index.
RECORD_OVERHEAD = 8 * KB
# The bound of disk_needed adds this much for an empty store, the index
# files and the logs.
STORE_OVERHEAD = 64 * 1024 * KB

# The start of a record of the log of zodbshootout: its date, its time
# and its level.
RECORD = re.compile(r"\d{4}-\d\d-\d\d \d\d:\d\d:\d\d,\d+ (\w+)")

# The zodbshootout benchmarks, the scenario names in the results, and the
# text that zodbshootout puts in the benchmark name.
SCENARIOS = {
    "add": ("add", "add {n} objects"),
    "update": ("update", "update {n} objects"),
    "cold": ("read_cold", "read {n} cold objects"),
    "warm": ("read_warm", "write/read {n} objects"),
    "hot": ("read_hot", "read {n} hot objects"),
}


@dataclass(frozen=True)
class Cell:
    """One combination of concurrency and object size."""

    mode: str
    concurrency: int
    object_size: int


@dataclass(frozen=True)
class Profile:
    """The cells of a run and the number of samples per scenario."""

    cells: tuple[Cell, ...]
    processes: int
    values: int


def _cells(modes, concurrencies, sizes):
    return tuple(
        Cell(mode, c, size) for mode in modes for size in sizes for c in concurrencies
    )


# The quick set is the regression gate of every pull request.
QUICK = Profile(cells=_cells(["threads"], [1, 16], [KB]), processes=1, values=5)

# The full set is the nightly baseline. Concurrency 1 runs only in the
# threads mode: one process with one thread.
FULL = Profile(
    cells=_cells(["threads"], [1, 4, 16, 64], [KB, 100 * KB])
    + _cells(["processes"], [4, 16, 64], [KB, 100 * KB]),
    processes=2,
    values=10,
)

# The gate set is the head-to-head of the phase gates of N4: one thread,
# 16 threads in one process and 16 processes, at 1 KB and 100 KB. Each
# cell runs 7 rounds: one value of each storage per round (ADR 0037).
GATE = Profile(
    cells=_cells(["threads"], [1, 16], [KB, 100 * KB])
    + _cells(["processes"], [16], [KB, 100 * KB]),
    processes=1,
    values=7,
)


def set_name(quick: bool, gate: bool) -> str:
    """Return the name of the set of a run: quick, gate or full."""
    return "quick" if quick else "gate" if gate else "full"


def disk_needed(storage: str, cell: Cell) -> int:
    """Return an upper bound of the bytes that one value of a cell takes.

    A value holds the populate of zodbshootout and two calls of one
    scenario: the warmup and the value. The calls of update, or of add on a
    store that zodbshootout cannot zap, write the most. The data of
    relstorage and relstorage-hf goes to PostgreSQL, so only the logs and
    the pyperf output of a value go to the work directory. `floor` writes
    only the records of the populate, and two files of them at most while
    it replaces one.
    """
    if storage in RELSTORAGE:
        return STORE_OVERHEAD
    c = cell.concurrency
    n = OBJECTS_PER_TRANSACTION
    # The populate writes n objects per worker and n more. The root, the
    # tree and the mappings of the workers take c + 3 records.
    populate = (c * n + n) + (c + 3)
    if storage == "floor":
        records = 2 * populate
    else:
        # Each call of update or add writes c × L transactions of at most
        # n + 1 records: add also writes the mapping of the worker again.
        records = populate + 2 * c * _inner_loops(c) * (n + 1)
    return records * (cell.object_size + RECORD_OVERHEAD) + STORE_OVERHEAD


def _inner_loops(concurrency: int) -> int:
    """Return the transactions of one worker in one call of add or update."""
    # zodbshootout sets 100 at a concurrency of 10 or less, and 30 above.
    return 100 if concurrency <= 10 else 30


def check_cold_loads(storage: str, objects_read: int, server_loads: int) -> None:
    """Fail a read_cold value of zoe-native that read from the client cache.

    zodbshootout empties the cache of the client with `_cache.clear()`
    before each read. So the server loads each object that the value
    reads, at least once. Fewer loads mean that a cache of the client
    served some reads, and the value measures that cache, not cold reads.
    The function does nothing for the other storages.
    """
    if storage not in COUNTS_COLD_LOADS or server_loads >= objects_read:
        return
    raise RuntimeError(
        f"read_cold of {storage}: the server counted {server_loads} loads for "
        f"{objects_read} objects read, so a cache of the client served cold "
        "reads: _cache.clear() does not empty it"
    )


def run(
    storage: str,
    quick: bool = False,
    gate: bool = False,
    workdir: Path | None = None,
    dsn: str | None = None,
    machine: str | None = None,
) -> dict:
    """Run zodbshootout against one storage and return the result document.

    Each value of a scenario runs in its own zodbshootout run, on a fresh
    store in a new directory under workdir. When the value succeeds, the
    function removes that directory. Before each cell, the function checks
    that the disk of workdir has the free space of one value. Each scenario
    entry records the GIL of each worker process of its values. With gate,
    the function runs the gate set as `run_gate` does.
    """
    if gate:
        [result] = run_gate([storage], workdir=workdir, dsn=dsn, machine=machine)
        return result
    store = STORAGES[storage]
    profile = QUICK if quick else FULL
    cells = [c for c in profile.cells if c.mode == "threads" or store.multi_process]
    workdir = _workdir(workdir)
    started = _now()
    # relstorage and relstorage-hf share the database, and each needs the
    # schema of its own history mode.
    reset_database(store, dsn)
    root = Path(tempfile.mkdtemp(prefix=f"{storage}-", dir=workdir))
    scenarios = []
    for index, cell in enumerate(cells):
        _check_disk([store], cell, workdir, [root])
        name = f"{index}-{cell.mode}-{cell.concurrency}-{cell.object_size}"
        scenarios += _run_cell(store, cell, profile, root, name, dsn)
    shutil.rmtree(root)
    return _document(
        store, set_name(quick, False), started, scenarios, workdir, dsn, machine
    )


def run_gate(
    storages: list[str],
    workdir: Path | None = None,
    dsn: str | None = None,
    machine: str | None = None,
    cells: list[Cell] | None = None,
) -> list[dict]:
    """Run the gate set against storages and return one document for each.

    Each scenario of a cell runs in rounds. A round runs one value of each
    storage, each on a fresh store, in an order that rotates from round to
    round: round r starts with the storage at index r modulo the number of
    storages. The documents have one start, which pairs their rounds, and
    each scenario entry keeps the round and the time of each value. cells
    picks some cells of the gate set, so that one job can run one cell.
    """
    stores = [STORAGES[name] for name in storages]
    cells = list(GATE.cells if cells is None else cells)
    if outside := [cell for cell in cells if cell not in GATE.cells]:
        raise ValueError(f"not cells of the gate set: {outside}")
    workdir = _workdir(workdir)
    started = _now()
    for store in stores:
        reset_database(store, dsn)
    roots = {
        store.name: Path(tempfile.mkdtemp(prefix=f"{store.name}-", dir=workdir))
        for store in stores
    }
    scenarios = {store.name: [] for store in stores}
    for cell in cells:
        index = GATE.cells.index(cell)
        name = f"{index}-{cell.mode}-{cell.concurrency}-{cell.object_size}"
        runs = [s for s in stores if cell.mode == "threads" or s.multi_process]
        _check_disk(runs, cell, workdir, roots.values())
        for bench, (scenario, _) in SCENARIOS.items():
            values = {store.name: [] for store in runs}
            rounds = {store.name: [] for store in runs}
            workers = {store.name: [] for store in runs}
            for number in range(GATE.processes * GATE.values):
                shift = number % len(runs)
                for store in runs[shift:] + runs[:shift]:
                    valuedir = roots[store.name] / f"{name}-{bench}-{number}"
                    times, records = _value(store, cell, bench, valuedir, dsn)
                    values[store.name] += times
                    rounds[store.name] += [number] * len(times)
                    workers[store.name] += records
            for store in runs:
                entry = _scenario(
                    scenario, cell, values[store.name], workers[store.name]
                )
                entry["values"] = [
                    {"round": number, "seconds": seconds}
                    for number, seconds in zip(
                        rounds[store.name], values[store.name], strict=True
                    )
                ]
                scenarios[store.name].append(entry)
    for root in roots.values():
        shutil.rmtree(root)
    return [
        _document(store, "gate", started, scenarios[store.name], workdir, dsn, machine)
        for store in stores
    ]


def _workdir(workdir: Path | None) -> Path:
    workdir = Path(workdir or tempfile.gettempdir())
    workdir.mkdir(parents=True, exist_ok=True)
    return workdir


def _now() -> datetime.datetime:
    return datetime.datetime.now(datetime.UTC)


def _check_disk(stores, cell: Cell, workdir: Path, roots) -> None:
    """Fail before a cell when the disk has not the free space of one value."""
    for store in stores:
        needed = disk_needed(store.name, cell)
        free = shutil.disk_usage(workdir).free
        if free < needed:
            # The cells before left nothing in the roots.
            for root in roots:
                shutil.rmtree(root)
            raise RuntimeError(
                f"not enough free disk for {store.name} in {cell}: one value needs"
                f" {needed} bytes, and the disk of {workdir} has {free} bytes free"
            )


def _document(
    store, kind: str, started: datetime.datetime, scenarios, workdir, dsn, machine
) -> dict:
    """Return the result document of a run of store."""
    software = environment.SOFTWARE
    if store.name in RELSTORAGE:
        software += environment.RELSTORAGE_SOFTWARE
    elif store.name in ("zoe", "zoe-native"):
        software += environment.ZOE_SOFTWARE
    return {
        "schema_version": SCHEMA_VERSION,
        "storage": store.name,
        "machine": environment.machine(workdir, machine),
        "python": environment.python(),
        "software": environment.software(software) | server_versions(store, dsn),
        "run": {
            "quick": kind == "quick",
            "set": kind,
            "started": started.isoformat(timespec="seconds").replace("+00:00", "Z"),
            "commit": environment.commit(),
        },
        "scenarios": scenarios,
    }


def _run_cell(
    store, cell: Cell, profile: Profile, root: Path, name: str, dsn
) -> list[dict]:
    """Run each value of each scenario of a cell on a fresh store."""
    scenarios = []
    for bench, (scenario, _) in SCENARIOS.items():
        values = []
        workers = []
        for number in range(profile.processes * profile.values):
            valuedir = root / f"{name}-{bench}-{number}"
            times, records = _value(store, cell, bench, valuedir, dsn)
            values += times
            workers += records
        scenarios.append(_scenario(scenario, cell, values, workers))
    return scenarios


def _value(store, cell: Cell, bench: str, valuedir: Path, dsn):
    """Run one value in valuedir on a fresh store, then remove valuedir."""
    valuedir.mkdir()
    empty_database(store, dsn)
    # _run_value raises when the value fails. Then the directory and its
    # logs stay.
    times, records = _run_value(store, cell, bench, valuedir, dsn)
    shutil.rmtree(valuedir)
    empty_database(store, dsn)
    return times, records


def _run_value(
    store, cell: Cell, bench: str, workdir: Path, dsn
) -> tuple[list[float], list[dict]]:
    """Run one value of a scenario in workdir.

    Return its time and the GIL record of each worker process.
    """
    output = workdir / "pyperf.json"
    log = workdir / "zodbshootout.log"
    records = workdir / "gil"
    records.mkdir()
    # The loads of the server before and after the run, for the check of
    # read_cold.
    loads = []
    count_loads = bench == "cold" and store.name in COUNTS_COLD_LOADS
    with zconfig(store, workdir, dsn) as text:
        config = workdir / "zodb.conf"
        config.write_text(text)
        args = [
            sys.executable,
            "-m",
            "zodbshootout",
            # One pyperf worker process runs the warmup, then the value, on
            # the same store.
            "--processes",
            "1",
            "--values",
            "1",
            "--warmups",
            "1",
            # One loop per sample, so that the percentiles are of single runs.
            "--loops",
            "1",
            "--object-counts",
            str(OBJECTS_PER_TRANSACTION),
            "--object-size",
            str(cell.object_size),
            "--concurrency",
            str(cell.concurrency),
            "--include-mapping",
            "no",
            "--quiet",
            "--output",
            str(output),
        ]
        if bench == "add":
            # zodbshootout zaps the store before each call of add, and it
            # refuses a zap without add. Without a zap, its master populates
            # the store before the calls of the other scenarios.
            args += ["--zap", "force"]
        if cell.mode == "threads":
            # One DB that all threads share, as in one Zope process.
            args += ["--threads", "shared"]
        # pyperf starts its workers with a reduced environment. The variables
        # of the GIL record reach them, and change nothing else.
        inherit = list(gil.INHERIT)
        if "LD_LIBRARY_PATH" in os.environ:
            inherit.append("LD_LIBRARY_PATH")
        args += ["--inherit-environ", ",".join(inherit)]
        args += [str(config), bench]
        if count_loads:
            loads.append(server_loads(admin_address(workdir)))
        with log.open("wb") as out:
            done = subprocess.run(
                args, stdout=out, stderr=subprocess.STDOUT, env=gil.environ(records)
            )
        if count_loads:
            loads.append(server_loads(admin_address(workdir)))
    name = SCENARIOS[bench][0]
    if done.returncode != 0 or not output.exists():
        raise RuntimeError(f"zodbshootout failed for {name} in {cell}:\n{_tail(log)}")
    if errors := _errors(log):
        # zodbshootout logs the exception of a benchmark, and reports a time
        # for it all the same.
        raise RuntimeError(
            f"zodbshootout logged an error for {name} in {cell}:\n{errors}"
        )
    benchmark = _benchmark(pyperf.BenchmarkSuite.load(str(output)), bench, cell, log)
    if count_loads:
        check_cold_loads(store.name, _objects(benchmark, cell), loads[1] - loads[0])
    workers = gil.read(records)
    if not workers:
        raise RuntimeError(f"no worker of {name} in {cell} wrote its GIL record")
    return list(benchmark.get_values()), workers


def _benchmark(suite, bench: str, cell: Cell, log: Path) -> pyperf.Benchmark:
    """Return the benchmark of one scenario from the pyperf output of a run."""
    name, description = SCENARIOS[bench]
    text = description.format(n=OBJECTS_PER_TRANSACTION)
    found = [b for b in suite.get_benchmarks() if b.get_name().endswith(f": {text}")]
    if len(found) != 1:
        raise RuntimeError(
            f"zodbshootout gave no result for {name} in {cell}:\n{_tail(log)}"
        )
    return found[0]


def _objects(benchmark: pyperf.Benchmark, cell: Cell) -> int:
    """Return the objects that the workers read or wrote in a run.

    Each call of a scenario, warmup or value, runs loops × inner loops
    transactions in each worker.
    """
    transactions = sum(
        (sum(loops for loops, _ in run.warmups) + len(run.values) * run.get_loops())
        * run.get_inner_loops()
        for run in benchmark.get_runs()
    )
    return cell.concurrency * OBJECTS_PER_TRANSACTION * transactions


def _scenario(name: str, cell: Cell, values: list[float], workers: list[dict]) -> dict:
    """Turn the values of one scenario of a cell into a scenario entry."""
    # A sample is the time for the slowest worker to do one transaction.
    mean = statistics.fmean(values)
    return {
        "name": name,
        "mode": cell.mode,
        "concurrency": cell.concurrency,
        "object_size": cell.object_size,
        "objects_per_transaction": OBJECTS_PER_TRANSACTION,
        "samples": len(values),
        "ops_per_second": cell.concurrency * OBJECTS_PER_TRANSACTION / mean,
        "mean_seconds": mean,
        "p50_seconds": _percentile(values, 50),
        "p99_seconds": _percentile(values, 99),
        "workers": workers,
    }


def _percentile(values: list[float], p: int) -> float:
    if len(values) == 1:
        return values[0]
    return statistics.quantiles(values, n=100, method="inclusive")[p - 1]


def _errors(log: Path) -> str:
    """Return the records of level ERROR or CRITICAL of a zodbshootout log.

    A record starts with its date, its time and its level, cut to 5
    characters. The lines that follow it, such as a traceback, are part of
    it.
    """
    out = []
    inside = False
    for line in log.read_text(errors="replace").splitlines():
        if record := RECORD.match(line):
            inside = record[1] in ("ERROR", "CRITI")
        if inside:
            out.append(line)
    return "\n".join(out)


def _tail(log: Path, lines: int = 40) -> str:
    return "\n".join(log.read_text(errors="replace").splitlines()[-lines:])
