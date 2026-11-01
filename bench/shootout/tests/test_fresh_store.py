"""Acceptance test of a fresh store for each value of zodbshootout.

The harness runs each value of a scenario in its own zodbshootout run: one
scenario, one pyperf worker process, one warmup, then the value
(`--processes 1 --warmups 1 --values 1 --loops 1`). A scenario of a cell has
`processes × values` values of the profile. Only `add` gets `--zap force`.

Each run starts on a fresh store: a new FileStorage file or ZOE store, a new
ZEO server or `zoe serve` on a new store, or a RelStorage database that the
harness empties. When a value succeeds, the harness stops its server and
removes its store, its logs and its pyperf output. So the work directory
holds at most one value at a time, and nothing after the run.

`disk_needed(storage, cell)` is an upper bound of the bytes that one value
of the cell takes in the work directory. Before each cell, the harness
compares it with `shutil.disk_usage(workdir).free`. When the free space is
smaller, `run` raises a RuntimeError before it starts anything for the cell.
The message names the storage, the cell, the bytes needed and the bytes free.

The test watches the work directory when each zodbshootout run starts, when
it ends, and every few milliseconds in between. It runs a tiny profile in
place of the quick set.
"""

import collections
import contextlib
import os
import pathlib
import subprocess
import threading

import jsonschema
import pytest

import shootout
from shootout import KB, Cell, Profile, disk_needed, run
from shootout.storages import STORAGES

SCHEMA_PATH = pathlib.Path(__file__).parents[2] / "schema" / "results.schema.json"
# The zodbshootout names of the scenarios that the harness runs.
SCENARIOS = ("add", "update", "cold", "warm", "hot")
OBJECTS_PER_TRANSACTION = 100
# Two values per scenario. The processes cell runs only for the storages
# that several processes can open.
TINY = Profile(
    cells=(Cell("threads", 2, KB), Cell("processes", 2, KB)),
    processes=2,
    values=1,
)
# A fresh store, its server and the logs of a run that starts take less.
FRESH = 8 * 1024 * 1024
# The logs at the start of two runs differ by less than this. A store that a
# value of the tiny profile leaves takes more.
SLACK = 64 * 1024
# The largest value of the full set must fit the disk of the reference
# machine with room to spare.
LARGEST = 50 * 10**9
POPEN = subprocess.Popen


def disk_use(root):
    """Return the bytes that the regular files under root take on the disk."""
    total = 0
    for directory, _, names in os.walk(root):
        for name in names:
            # The harness can remove a file while the walk runs.
            with contextlib.suppress(FileNotFoundError):
                total += os.lstat(os.path.join(directory, name)).st_blocks * 512
    return total


def option(args, name):
    """Return the value that follows an option on a command line."""
    return args[args.index(name) + 1]


def scenario_of(args):
    [scenario] = [arg for arg in args if arg in SCENARIOS]
    return scenario


def cell_of(args):
    mode = "threads" if "--threads" in args else "processes"
    return Cell(
        mode, int(option(args, "--concurrency")), int(option(args, "--object-size"))
    )


def inner_loops(concurrency):
    """Return the transactions of one worker in one call of add or update."""
    # zodbshootout runs 100 at a concurrency of 10 or less, and 30 above.
    return 100 if concurrency <= 10 else 30


class Watch:
    """Watch the work directory and the zodbshootout runs of the harness.

    `popen` replaces `subprocess.Popen`. It records the disk use of the work
    directory when a zodbshootout run starts and when it ends. A thread
    records the peak in between.
    """

    def __init__(self, workdir, on_start=None):
        self.workdir = workdir
        self.on_start = on_start
        self.runs = []
        self.starts = []
        self.ends = []
        self.peak = 0
        self._stop = threading.Event()
        self._thread = threading.Thread(target=self._poll, daemon=True)
        watch = self

        class Popen(POPEN):
            def __init__(self, args, *rest, **kwargs):
                self.shootout_args = None
                words = [str(arg) for arg in args] if isinstance(args, list) else []
                if "zodbshootout" in words:
                    self.shootout_args = words
                    watch.start(words)
                super().__init__(args, *rest, **kwargs)

            def wait(self, timeout=None):
                code = super().wait(timeout)
                if self.shootout_args is not None:
                    watch.end(self.shootout_args)
                    self.shootout_args = None
                return code

        self.popen = Popen

    def start(self, args):
        self.runs.append(args)
        self.starts.append(disk_use(self.workdir))
        if self.on_start is not None:
            self.on_start()

    def end(self, args):
        self.ends.append((cell_of(args), disk_use(self.workdir)))

    def _poll(self):
        while not self._stop.wait(0.02):
            self.peak = max(self.peak, disk_use(self.workdir))

    def __enter__(self):
        self._thread.start()
        return self

    def __exit__(self, *exc):
        self._stop.set()
        self._thread.join()
        self.peak = max([self.peak, *self.starts, *(used for _, used in self.ends)])


def cells_of(storage, profile):
    return [
        cell
        for cell in profile.cells
        if cell.mode == "threads" or STORAGES[storage].multi_process
    ]


def check_runs(watch, storage):
    """Check that each value of each scenario ran in its own zodbshootout run."""
    cells = cells_of(storage, TINY)
    values = TINY.processes * TINY.values
    for args in watch.runs:
        assert option(args, "--processes") == "1"
        # The warmup stays: it keeps the costs of a first call out of the value.
        assert option(args, "--warmups") == "1"
        assert option(args, "--values") == "1"
        assert option(args, "--loops") == "1"
        # zodbshootout refuses a zap without add.
        assert ("--zap" in args) == (scenario_of(args) == "add")
    counts = collections.Counter(
        (scenario_of(args), cell_of(args)) for args in watch.runs
    )
    assert counts == collections.Counter(
        {(scenario, cell): values for scenario in SCENARIOS for cell in cells}
    )


def check_result(result, storage, schema):
    jsonschema.validate(result, schema)
    assert result["storage"] == storage
    cells = cells_of(storage, TINY)
    assert len(result["scenarios"]) == len(SCENARIOS) * len(cells)
    for scenario in result["scenarios"]:
        assert scenario["samples"] == TINY.processes * TINY.values
        assert scenario["ops_per_second"] > 0


def check_disk(watch, storage):
    """Check that the work directory holds at most one value at a time."""
    # When a run starts, the store, the server and the logs of the value
    # before it are gone: only a fresh store is there.
    assert max(watch.starts) <= FRESH
    assert max(watch.starts) - min(watch.starts) <= SLACK
    # When a run ends, its value takes no more than the harness expects.
    for cell, used in watch.ends:
        assert used <= disk_needed(storage, cell)
    # The peak of the whole run stays at one value.
    assert watch.peak <= max(disk_needed(storage, c) for c in cells_of(storage, TINY))
    # Nothing stays after the run.
    assert disk_use(watch.workdir) == 0


@pytest.mark.parametrize("storage", ["filestorage", "zeo", "zoe", "zoe-server"])
def test_each_value_runs_on_a_fresh_store(storage, tiny, schema, tmp_path, monkeypatch):
    workdir = tmp_path / "work"
    watch = Watch(workdir)
    monkeypatch.setattr(subprocess, "Popen", watch.popen)
    with watch:
        result = run(storage, quick=True, workdir=workdir)

    check_result(result, storage, schema)
    check_runs(watch, storage)
    check_disk(watch, storage)


def object_rows(dsn):
    """Return the number of object revisions in the RelStorage database."""
    import psycopg2

    with contextlib.closing(psycopg2.connect(dsn)) as conn, conn.cursor() as cursor:
        cursor.execute("SELECT to_regclass('object_state')")
        if cursor.fetchone()[0] is None:
            return 0
        cursor.execute("SELECT count(*) FROM object_state")
        return cursor.fetchone()[0]


@pytest.mark.skipif(
    "ZOE_BENCH_PG_DSN" not in os.environ,
    reason="needs a PostgreSQL database in ZOE_BENCH_PG_DSN",
)
def test_each_relstorage_value_starts_on_an_empty_database(
    tiny, schema, tmp_path, monkeypatch
):
    dsn = os.environ["ZOE_BENCH_PG_DSN"]
    rows = []
    workdir = tmp_path / "work"
    watch = Watch(workdir, on_start=lambda: rows.append(object_rows(dsn)))
    monkeypatch.setattr(subprocess, "Popen", watch.popen)
    with watch:
        result = run("relstorage", quick=True, workdir=workdir, dsn=dsn)

    check_result(result, "relstorage", schema)
    check_runs(watch, "relstorage")
    check_disk(watch, "relstorage")
    # The harness empties the database before each value and after it.
    assert rows == [0] * len(watch.runs)
    assert object_rows(dsn) == 0


@pytest.mark.parametrize("storage", ["filestorage", "zeo", "zoe", "zoe-server"])
def test_the_estimate_holds_the_warmup_and_the_value(storage):
    for cell in shootout.FULL.cells:
        # update writes in both calls of a value, and nothing removes the
        # objects of the warmup before the value.
        calls = 2 * cell.concurrency * inner_loops(cell.concurrency)
        written = calls * OBJECTS_PER_TRANSACTION * cell.object_size
        assert disk_needed(storage, cell) >= written
    assert max(disk_needed(storage, cell) for cell in shootout.FULL.cells) < LARGEST


def free_space(free):
    """Return a replacement of os.statvfs that reports free bytes free."""
    blocks = free // 4096

    def statvfs(path):
        return os.statvfs_result(
            (4096, 4096, 4 * blocks, blocks, blocks, 10**6, 10**6, 10**6, 0, 255)
        )

    return statvfs


def test_a_cell_that_does_not_fit_fails_before_it_starts(tmp_path, monkeypatch):
    small = Cell("threads", 1, KB)
    large = Cell("threads", 3, 100 * KB)
    monkeypatch.setattr(
        shootout, "QUICK", Profile(cells=(small, large), processes=1, values=1)
    )
    needed = disk_needed("filestorage", large)
    free = (disk_needed("filestorage", small) + needed) // 2 // 4096 * 4096
    assert disk_needed("filestorage", small) < free < needed
    # shutil.disk_usage reads os.statvfs.
    monkeypatch.setattr(os, "statvfs", free_space(free))
    workdir = tmp_path / "work"
    watch = Watch(workdir)
    monkeypatch.setattr(subprocess, "Popen", watch.popen)

    with watch, pytest.raises(RuntimeError) as failure:
        run("filestorage", quick=True, workdir=workdir)

    message = str(failure.value)
    assert "filestorage" in message
    assert str(large) in message
    assert str(needed) in message
    assert str(free) in message
    assert "free" in message
    # The small cell ran. The large cell started no run.
    assert len(watch.runs) == len(SCENARIOS)
    assert {cell_of(args) for args in watch.runs} == {small}
    # The failure leaves no store behind.
    assert disk_use(workdir) == 0
