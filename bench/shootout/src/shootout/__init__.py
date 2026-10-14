"""Run zodbshootout against the reference storages and ZOE.

`run` measures one storage and returns one result document. The document
validates against `bench/schema/results.schema.json`. `disk_needed` gives
the free disk space that one value of a cell needs.
"""

import datetime
import os
import shutil
import statistics
import subprocess
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path

import pyperf

from shootout import environment
from shootout.storages import STORAGES, empty_database, server_versions, zconfig

SCHEMA_VERSION = 1
OBJECTS_PER_TRANSACTION = 100
KB = 1024
# The bound of disk_needed counts this much more than the object size for
# each record: the excess of the pickle, the record header and the index.
RECORD_OVERHEAD = 8 * KB
# The bound of disk_needed adds this much for an empty store, the index
# files and the logs.
STORE_OVERHEAD = 64 * 1024 * KB

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


def disk_needed(storage: str, cell: Cell) -> int:
    """Return an upper bound of the bytes that one value of a cell takes.

    A value holds the populate of zodbshootout and two calls of one
    scenario: the warmup and the value. The calls of update, or of add on a
    store that zodbshootout cannot zap, write the most. The data of
    relstorage goes to PostgreSQL, so only the logs and the pyperf output
    of a value go to the work directory.
    """
    if storage == "relstorage":
        return STORE_OVERHEAD
    c = cell.concurrency
    n = OBJECTS_PER_TRANSACTION
    # The populate writes n objects per worker and n more. The root, the
    # tree and the mappings of the workers take c + 3 records. Each call of
    # update or add writes c × L transactions of at most n + 1 records: add
    # also writes the mapping of the worker again.
    records = (c * n + n) + (c + 3) + 2 * c * _inner_loops(c) * (n + 1)
    return records * (cell.object_size + RECORD_OVERHEAD) + STORE_OVERHEAD


def _inner_loops(concurrency: int) -> int:
    """Return the transactions of one worker in one call of add or update."""
    # zodbshootout sets 100 at a concurrency of 10 or less, and 30 above.
    return 100 if concurrency <= 10 else 30


def run(
    storage: str,
    quick: bool = False,
    workdir: Path | None = None,
    dsn: str | None = None,
    machine: str | None = None,
) -> dict:
    """Run zodbshootout against one storage and return the result document.

    Each value of a scenario runs in its own zodbshootout run, on a fresh
    store in a new directory under workdir. When the value succeeds, the
    function removes that directory. Before each cell, the function checks
    that the disk of workdir has the free space of one value.
    """
    store = STORAGES[storage]
    profile = QUICK if quick else FULL
    cells = [c for c in profile.cells if c.mode == "threads" or store.multi_process]
    workdir = Path(workdir or tempfile.gettempdir())
    workdir.mkdir(parents=True, exist_ok=True)
    started = datetime.datetime.now(datetime.UTC)
    software = environment.SOFTWARE
    if storage == "relstorage":
        software += environment.RELSTORAGE_SOFTWARE
    elif storage == "zoe":
        software += environment.ZOE_SOFTWARE

    root = Path(tempfile.mkdtemp(prefix=f"{storage}-", dir=workdir))
    scenarios = []
    for index, cell in enumerate(cells):
        needed = disk_needed(storage, cell)
        free = shutil.disk_usage(workdir).free
        if free < needed:
            # The cells before left nothing in root.
            shutil.rmtree(root)
            raise RuntimeError(
                f"not enough free disk for {storage} in {cell}: one value needs"
                f" {needed} bytes, and the disk of {workdir} has {free} bytes free"
            )
        name = f"{index}-{cell.mode}-{cell.concurrency}-{cell.object_size}"
        scenarios += _run_cell(store, cell, profile, root, name, dsn)
    shutil.rmtree(root)

    return {
        "schema_version": SCHEMA_VERSION,
        "storage": storage,
        "machine": environment.machine(workdir, machine),
        "python": environment.python(),
        "software": environment.software(software) | server_versions(store, dsn),
        "run": {
            "quick": quick,
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
        for number in range(profile.processes * profile.values):
            valuedir = root / f"{name}-{bench}-{number}"
            valuedir.mkdir()
            empty_database(store, dsn)
            # _run_value raises when the value fails. Then the directory and
            # its logs stay.
            values += _run_value(store, cell, bench, valuedir, dsn)
            shutil.rmtree(valuedir)
            empty_database(store, dsn)
        scenarios.append(_scenario(scenario, cell, values))
    return scenarios


def _run_value(store, cell: Cell, bench: str, workdir: Path, dsn) -> list[float]:
    """Run one value of a scenario in workdir and return its time."""
    output = workdir / "pyperf.json"
    log = workdir / "zodbshootout.log"
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
        if "LD_LIBRARY_PATH" in os.environ:
            # pyperf starts its workers with a reduced environment.
            args += ["--inherit-environ", "LD_LIBRARY_PATH"]
        args += [str(config), bench]
        with log.open("wb") as out:
            done = subprocess.run(args, stdout=out, stderr=subprocess.STDOUT)
    if done.returncode != 0 or not output.exists():
        name = SCENARIOS[bench][0]
        raise RuntimeError(f"zodbshootout failed for {name} in {cell}:\n{_tail(log)}")
    return _values(pyperf.BenchmarkSuite.load(str(output)), bench, cell, log)


def _values(suite, bench: str, cell: Cell, log: Path) -> list[float]:
    """Return the values of one scenario from the pyperf output of a run."""
    name, description = SCENARIOS[bench]
    text = description.format(n=OBJECTS_PER_TRANSACTION)
    found = [b for b in suite.get_benchmarks() if b.get_name().endswith(f": {text}")]
    if len(found) != 1:
        raise RuntimeError(
            f"zodbshootout gave no result for {name} in {cell}:\n{_tail(log)}"
        )
    return list(found[0].get_values())


def _scenario(name: str, cell: Cell, values: list[float]) -> dict:
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
    }


def _percentile(values: list[float], p: int) -> float:
    if len(values) == 1:
        return values[0]
    return statistics.quantiles(values, n=100, method="inclusive")[p - 1]


def _tail(log: Path, lines: int = 40) -> str:
    return "\n".join(log.read_text(errors="replace").splitlines()[-lines:])
