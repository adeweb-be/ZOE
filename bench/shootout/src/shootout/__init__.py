"""Run zodbshootout against the reference storages.

`run` measures one storage and returns one result document. The document
validates against `bench/schema/results.schema.json`.
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
from shootout.storages import STORAGES, server_versions, zconfig

SCHEMA_VERSION = 1
OBJECTS_PER_TRANSACTION = 100
KB = 1024

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


def run(
    storage: str,
    quick: bool = False,
    workdir: Path | None = None,
    dsn: str | None = None,
    machine: str | None = None,
) -> dict:
    """Run zodbshootout against one storage and return the result document.

    The storage files and the logs go to a new directory under workdir.
    The function removes that directory when every run succeeds.
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

    root = Path(tempfile.mkdtemp(prefix=f"{storage}-", dir=workdir))
    scenarios = []
    for index, cell in enumerate(cells):
        celldir = root / f"{index}-{cell.mode}-{cell.concurrency}-{cell.object_size}"
        celldir.mkdir()
        scenarios += _run_cell(store, cell, profile, celldir, dsn)
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


def _run_cell(store, cell: Cell, profile: Profile, workdir: Path, dsn) -> list[dict]:
    output = workdir / "pyperf.json"
    log = workdir / "zodbshootout.log"
    with zconfig(store, workdir, dsn) as text:
        config = workdir / "zodb.conf"
        config.write_text(text)
        args = [
            sys.executable,
            "-m",
            "zodbshootout",
            "--processes",
            str(profile.processes),
            "--values",
            str(profile.values),
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
            "--zap",
            "force",
            "--include-mapping",
            "no",
            "--quiet",
            "--output",
            str(output),
        ]
        if cell.mode == "threads":
            # One DB that all threads share, as in one Zope process.
            args += ["--threads", "shared"]
        if "LD_LIBRARY_PATH" in os.environ:
            # pyperf starts its workers with a reduced environment.
            args += ["--inherit-environ", "LD_LIBRARY_PATH"]
        args += [str(config), *SCENARIOS]
        with log.open("wb") as out:
            done = subprocess.run(args, stdout=out, stderr=subprocess.STDOUT)
    if done.returncode != 0 or not output.exists():
        raise RuntimeError(f"zodbshootout failed for {cell}:\n{_tail(log)}")
    return _scenarios(pyperf.BenchmarkSuite.load(str(output)), cell, log)


def _scenarios(suite, cell: Cell, log: Path) -> list[dict]:
    """Turn the pyperf benchmarks of one cell into scenario entries."""
    benchmarks = {b.get_name(): b for b in suite.get_benchmarks()}
    scenarios = []
    for name, description in SCENARIOS.values():
        text = description.format(n=OBJECTS_PER_TRANSACTION)
        found = [b for full, b in benchmarks.items() if full.endswith(f": {text}")]
        if len(found) != 1:
            raise RuntimeError(
                f"zodbshootout gave no result for {name} in {cell}:\n{_tail(log)}"
            )
        values = found[0].get_values()
        # A sample is the time for the slowest worker to do one transaction.
        mean = statistics.fmean(values)
        scenarios.append(
            {
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
        )
    return scenarios


def _percentile(values: list[float], p: int) -> float:
    if len(values) == 1:
        return values[0]
    return statistics.quantiles(values, n=100, method="inclusive")[p - 1]


def _tail(log: Path, lines: int = 40) -> str:
    return "\n".join(log.read_text(errors="replace").splitlines()[-lines:])
