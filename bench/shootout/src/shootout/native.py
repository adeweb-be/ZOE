"""The native gate: the phase 3 gate of N4 (ADR 0039).

`run` measures `zoe serve` through the native client (`zoe-native`)
against ZEO 6 through `ZEO.ClientStorage` (`zeo`), and returns one result
document. The document validates against `bench/schema/native.schema.json`.
`report` prints one row per cell and scenario, and gives the verdict.

The gate set has 30 cells: the 5 scenarios of zodbshootout, in 3 shapes
(threads 1, threads 16, processes 16), at 1 KB and 100 KB. A transaction
holds 100 objects at 1 KB and 10 objects at 100 KB.

For each cell, each storage gets one server on a fresh store, one populate
and one set of worker processes (`shootout.nativeworker`). Then each
scenario runs in pairs of batches. A batch runs the same number of
transactions in a row in each worker of one storage, as the inner loops of
zodbshootout do, and its time is the time of the slowest worker for one
transaction. The warmup pair sets that number for the scenario: a batch
lasts about `BATCH_SECONDS`, with at most `MAX_TRANSACTIONS`. A
pair is one batch of each storage. The first storage of a pair changes
from one pair to the next. Only one storage works at a time. One pair
before the first one warms the scenario up and does not count.

The rule: the ratio of a pair is `t_zeo / t_zoe`. After at least
`MIN_PAIRS` pairs, a scenario passes when the confidence interval at
`LEVEL` of the mean of the log of the ratios is above 0, and fails when it
is below 0. The run stops a scenario at the first verdict, or at
`MAX_PAIRS` pairs, or after `MAX_SECONDS` of pairs. A scenario with no
verdict then is undecided, and it fails the gate.
"""

import contextlib
import datetime
import math
import multiprocessing
import os
import shutil
import statistics
import sys
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path
from typing import TextIO

from shootout import KB, check_cold_loads, environment, gil
from shootout.nativeworker import main as worker_main
from shootout.storages import (
    STORAGES,
    admin_address,
    server_loads,
    server_versions,
    zconfig,
)

SCHEMA_VERSION = 1
SCHEMA = Path(__file__).resolve().parents[3] / "schema" / "native.schema.json"
BASELINE = "zeo"
STORAGE = "zoe-native"
PAIR = (BASELINE, STORAGE)
SCENARIOS = ("add", "update", "read_cold", "read_warm", "read_hot")
SHAPES = (("threads", 1), ("threads", 16), ("processes", 16))
SIZES = (KB, 100 * KB)
# The objects of a transaction, for each object size.
OBJECTS = {KB: 100, 100 * KB: 10}
MIN_PAIRS = 10
MAX_PAIRS = 100
MAX_SECONDS = 10.0
LEVEL = 0.99
# A batch runs about this long: its transactions run in a row in each
# worker, as the inner loops of zodbshootout do.
BATCH_SECONDS = 0.02
MAX_TRANSACTIONS = 20
SOFTWARE = ("zodbshootout", "ZODB", "ZEO", "persistent", "transaction", "zoedb")


@dataclass(frozen=True)
class Cell:
    """One shape and object size of the gate set."""

    mode: str
    concurrency: int
    object_size: int

    @property
    def objects(self) -> int:
        return OBJECTS[self.object_size]

    def __str__(self) -> str:
        return f"{self.mode}:{self.concurrency}:{self.object_size}"


CELLS = tuple(Cell(mode, c, size) for size in SIZES for mode, c in SHAPES)


@dataclass(frozen=True)
class Verdict:
    """The verdict of the pairs of one scenario."""

    # The geometric mean of the ratios, and the bounds of its interval.
    ratio: float
    low: float
    high: float
    # pass, fail, or None while the pairs give no verdict.
    verdict: str | None


def decide(ratios: list[float], level: float = LEVEL, least: int = MIN_PAIRS):
    """Return the verdict of the ratios `t_zeo / t_zoe` of the pairs."""
    logs = [math.log(r) for r in ratios]
    if not logs:
        return Verdict(math.nan, math.nan, math.nan, None)
    mean = statistics.fmean(logs)
    if len(logs) < 2:
        return Verdict(math.exp(mean), 0.0, math.inf, None)
    half = _t_quantile((1 + level) / 2, len(logs) - 1) * statistics.stdev(logs)
    half /= math.sqrt(len(logs))
    low, high = math.exp(mean - half), math.exp(mean + half)
    verdict = None
    if len(logs) >= least:
        verdict = "pass" if low > 1 else "fail" if high < 1 else None
    return Verdict(math.exp(mean), low, high, verdict)


def _t_quantile(p: float, df: int) -> float:
    """Return the quantile p of Student's t with df degrees of freedom.

    The Cornish-Fisher expansion in 1/df is within 0.1 % of the exact
    value from 9 degrees of freedom on, at the levels of the gate.
    """
    z = statistics.NormalDist().inv_cdf(p)
    terms = (
        (z**3 + z) / 4,
        (5 * z**5 + 16 * z**3 + 3 * z) / 96,
        (3 * z**7 + 19 * z**5 + 17 * z**3 - 15 * z) / 384,
        (79 * z**9 + 776 * z**7 + 1482 * z**5 - 1920 * z**3 - 945 * z) / 92160,
    )
    return z + sum(term / df ** (k + 1) for k, term in enumerate(terms))


def transactions_for(seconds: float) -> int:
    """Return the transactions of a batch whose one transaction takes seconds.

    A batch lasts about `BATCH_SECONDS`, with 1 to `MAX_TRANSACTIONS`
    transactions.
    """
    if seconds <= 0:
        return MAX_TRANSACTIONS
    return max(1, min(MAX_TRANSACTIONS, math.ceil(BATCH_SECONDS / seconds)))


def order(number: int) -> tuple[str, str]:
    """Return the storages of pair number, in the order that they run."""
    return PAIR if number % 2 == 0 else PAIR[::-1]


def ratios(pairs: list[dict]) -> list[float]:
    """Return the ratio `t_zeo / t_zoe` of each pair."""
    return [p["seconds"][BASELINE] / p["seconds"][STORAGE] for p in pairs]


def run(
    cells=CELLS,
    scenarios=SCENARIOS,
    workdir: Path | None = None,
    machine: str | None = None,
    out: TextIO = sys.stdout,
) -> dict:
    """Run the gate on cells and scenarios, and return the result document.

    The function prints the row of each scenario when it ends.
    """
    workdir = Path(workdir or tempfile.gettempdir())
    workdir.mkdir(parents=True, exist_ok=True)
    started = datetime.datetime.now(datetime.UTC)
    clock = time.monotonic()
    root = Path(tempfile.mkdtemp(prefix="native-", dir=workdir))
    _header(out)
    entries = [
        _cell(cell, scenarios, root / str(cell).replace(":", "-"), out)
        for cell in cells
    ]
    shutil.rmtree(root)
    return {
        "schema_version": SCHEMA_VERSION,
        "machine": environment.machine(workdir, machine),
        "python": environment.python(),
        "software": environment.software(SOFTWARE)
        | server_versions(STORAGES[STORAGE], None),
        "run": {
            "started": started.isoformat(timespec="seconds").replace("+00:00", "Z"),
            "commit": environment.commit(),
            "seconds": round(time.monotonic() - clock, 3),
        },
        "rule": {
            "min_pairs": MIN_PAIRS,
            "max_pairs": MAX_PAIRS,
            "max_seconds": MAX_SECONDS,
            "level": LEVEL,
            "batch_seconds": BATCH_SECONDS,
            "max_transactions": MAX_TRANSACTIONS,
        },
        "cells": entries,
    }


def _cell(cell: Cell, scenarios, directory: Path, out: TextIO) -> dict:
    """Run the scenarios of one cell, and return its entry."""
    records = {name: directory / f"gil-{name}" for name in PAIR}
    with contextlib.ExitStack() as stack:
        groups = {}
        for name in PAIR:
            store = directory / name
            store.mkdir(parents=True)
            records[name].mkdir()
            config = stack.enter_context(zconfig(STORAGES[name], store, None))
            groups[name] = stack.enter_context(_Group(config, cell, records[name]))
        admin = admin_address(directory / STORAGE)
        for group in groups.values():
            group.populate()
        entries = []
        for scenario in scenarios:
            transactions, pairs = _pairs(groups, cell, scenario, admin)
            entries.append(
                {"name": scenario, "transactions": transactions, "pairs": pairs}
            )
            print(
                _row(cell, cell.objects, scenario, pairs, MIN_PAIRS, LEVEL)[0],
                file=out,
                flush=True,
            )
    workers = {name: gil.read(records[name]) for name in PAIR}
    for name, found in workers.items():
        if not found:
            raise RuntimeError(f"no process of {name} in {cell} wrote its GIL record")
    return {
        "mode": cell.mode,
        "concurrency": cell.concurrency,
        "object_size": cell.object_size,
        "objects_per_transaction": cell.objects,
        "workers": workers,
        "scenarios": entries,
    }


def _pairs(groups, cell: Cell, scenario: str, admin: str) -> tuple[int, list[dict]]:
    """Run the pairs of one scenario until the rule stops them.

    Return the transactions of each batch and the pairs.
    """
    cold = scenario == "read_cold"
    if cold:
        before = server_loads(admin)
    # The warmup pair primes the caches of read_hot and the first commit.
    # Its slower storage sets the transactions of a batch, the same for
    # both storages.
    slowest = max(groups[name].batch(scenario, 1) for name in PAIR)
    transactions = transactions_for(slowest)
    pairs = []
    start = time.monotonic()
    while True:
        names = order(len(pairs))
        seconds = {name: groups[name].batch(scenario, transactions) for name in names}
        pairs.append({"first": names[0], "seconds": seconds})
        if (
            decide(ratios(pairs), LEVEL, MIN_PAIRS).verdict
            or len(pairs) >= MAX_PAIRS
            or time.monotonic() - start >= MAX_SECONDS
        ):
            break
    if cold:
        reads = 1 + len(pairs) * transactions
        read = reads * cell.concurrency * cell.objects
        check_cold_loads(STORAGE, read, server_loads(admin) - before)
    return transactions, pairs


class _Group:
    """The worker processes of one storage in one cell."""

    def __init__(self, config: str, cell: Cell, records: Path):
        self.config = config
        self.cell = cell
        self.records = records
        self.processes = []

    def __enter__(self):
        c = self.cell.concurrency
        shares = (
            [list(range(c))] if self.cell.mode == "threads" else [[w] for w in range(c)]
        )
        context = multiprocessing.get_context("spawn")
        # A spawned process starts with the environment of this one. The
        # environment of the GIL record reaches the workers only.
        saved = dict(os.environ)
        os.environ.update(gil.environ(self.records))
        try:
            for number, workers in enumerate(shares):
                mine, theirs = context.Pipe()
                process = context.Process(
                    target=worker_main,
                    args=(
                        theirs,
                        self.config,
                        workers,
                        c,
                        self.cell.objects,
                        self.cell.object_size,
                    ),
                    daemon=True,
                )
                process.start()
                theirs.close()
                self.processes.append((process, mine))
                if number == 0:
                    # The first DB of a store creates its root. The other
                    # processes open theirs after it, or their roots conflict.
                    self._answer(mine)
        finally:
            os.environ.clear()
            os.environ.update(saved)
        for _, pipe in self.processes[1:]:
            self._answer(pipe)
        return self

    def __exit__(self, *exc):
        for _, pipe in self.processes:
            with contextlib.suppress(OSError):
                pipe.send(("stop",))
        for process, pipe in self.processes:
            process.join(timeout=60)
            if process.is_alive():
                process.kill()
                process.join()
            pipe.close()
        return False

    def populate(self) -> None:
        """Populate the store from the first process, then open every worker."""
        _, pipe = self.processes[0]
        pipe.send(("populate",))
        self._answer(pipe)
        for _, pipe in self.processes:
            pipe.send(("open",))
        for _, pipe in self.processes:
            self._answer(pipe)

    def batch(self, scenario: str, transactions: int) -> float:
        """Run one batch of scenario, `transactions` in a row in each worker.

        Return the time of the slowest worker for one transaction: its time
        for the batch divided by `transactions`.
        """
        for _, pipe in self.processes:
            pipe.send(("batch", scenario, transactions))
        times = [t for _, pipe in self.processes for t in self._answer(pipe)]
        return max(times) / transactions

    def _answer(self, pipe):
        try:
            status, value = pipe.recv()
        except EOFError:
            raise RuntimeError(f"a worker process of {self.cell} stopped") from None
        if status != "ok":
            raise RuntimeError(f"a worker process of {self.cell} failed:\n{value}")
        return value


ROW = "{:<10} {:<9} {:>3} {:>7} {:>10} {:>10} {:>6} {:>13} {:>5}  {}"


def _header(out: TextIO) -> None:
    print(
        ROW.format(
            "scenario",
            "mode",
            "c",
            "size",
            "zeo us",
            "zoe us",
            "ratio",
            f"{LEVEL:.0%} interval",
            "pairs",
            "verdict",
        ),
        file=out,
        flush=True,
    )


def _row(cell, objects: int, scenario: str, pairs, least: int, level: float):
    """Return the row of a scenario and its verdict: pass, FAIL or UNDECIDED.

    The times are the medians per object of the slowest worker, in µs.
    """
    v = decide(ratios(pairs), level, least)
    verdict = {"pass": "pass", "fail": "FAIL"}.get(v.verdict, "UNDECIDED")

    def median(name):
        return statistics.median(p["seconds"][name] for p in pairs) / objects * 1e6

    row = ROW.format(
        scenario,
        cell.mode,
        cell.concurrency,
        cell.object_size,
        f"{median(BASELINE):.1f}",
        f"{median(STORAGE):.1f}",
        f"{v.ratio:.2f}",
        f"{v.low:.2f}-{v.high:.2f}",
        len(pairs),
        verdict,
    )
    return row, verdict


def report(result: dict, out: TextIO) -> int:
    """Print the rows of a result and the verdict. Return 0 when it passes.

    Every scenario of every cell of the gate set counts. A scenario that the
    result does not hold is missing, and fails the gate.
    """
    rule = result["rule"]
    python = environment.python_tag(result["python"])
    run = result["run"]
    print(
        f"{STORAGE} against {BASELINE} on {python}, {result['machine']['name']}, "
        f"commit {run['commit'] or 'unknown'}, {run['seconds']:.0f} s",
        file=out,
    )
    found = {}
    for entry in result["cells"]:
        cell = Cell(entry["mode"], entry["concurrency"], entry["object_size"])
        for scenario in entry["scenarios"]:
            found[cell, scenario["name"]] = (entry["objects_per_transaction"], scenario)
    _header(out)
    counts = {"pass": 0, "FAIL": 0, "UNDECIDED": 0, "missing": 0}
    for cell in CELLS:
        for scenario in SCENARIOS:
            if (cell, scenario) not in found:
                # The summary counts a missing scenario. A run of some cells
                # would print a row for each other one.
                counts["missing"] += 1
                continue
            objects, entry = found[cell, scenario]
            row, verdict = _row(
                cell,
                objects,
                scenario,
                entry["pairs"],
                rule["min_pairs"],
                rule["level"],
            )
            counts[verdict] += 1
            print(row, file=out)
    passed = counts["pass"] == len(CELLS) * len(SCENARIOS)
    print(
        f"gate {'passed' if passed else 'failed'}: {STORAGE} faster than "
        f"{BASELINE} on {python} in each cell; {len(CELLS) * len(SCENARIOS)} "
        f"cells, {counts['FAIL']} slower, {counts['UNDECIDED']} undecided, "
        f"{counts['missing']} missing",
        file=out,
    )
    return 0 if passed else 1
