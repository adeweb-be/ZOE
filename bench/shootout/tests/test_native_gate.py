"""Acceptance test of the phase 3 gate in the harness.

N4 sets the gate of phase 3 (ADR 0037): `zoe serve` through the native
client at a storage cost of at most a third of the cost of ZEO 6 through
`ZEO.ClientStorage`, in every zodbshootout cell of the gate set. The cost
of a storage is its time minus the time of the storage `floor` of the
harness in the same round. The native client has a cache of its own, so
`read_hot` counts in every size. The harness gets two storages:

- `zoe-native`: `zoe serve`, started as for `zoe-server`, opened through
  `<zoeclient>` with the native inside (the key `native true`);
- `relstorage-hf`: RelStorage with `keep-history false` on the same
  PostgreSQL as `relstorage`, and otherwise the same section.

`python -m shootout gate --storage zoe-native --baseline zeo DIR` reads the
gate set and checks a ratio of the costs of 3 when no `--ratio` is given:
the median over the rounds of `(t_zeo - t_floor) / (t_zoe - t_floor)`. It
counts every cell. With `--storage zoe-server`, the gate keeps the parity
rules: a ratio of ops/s of 1 by default, and `read_hot` left out in every
size.

Each scenario entry of a result has a list `workers`: one item per worker
process that ran the cell, at least one per cell. Each item records
`gil_enabled`, whether the GIL was on at the end of the worker
(`sys._is_gil_enabled()`), and `gil_enabled_by`, the modules that turned
it back on. The schema accepts the list and does not require
it.

`shootout.check_cold_loads(storage, objects_read, server_loads)` fails the
run of `zoe-native` with `RuntimeError` when the server counted fewer loads
(`zoe_loads_total` on the admin port, before and after the `read_cold`
loops) than the objects that the loops read. A cache that `_cache.clear()`
does not empty would turn `read_cold` into a cache read.

The gate tests write synthetic gate results that are valid for the schema,
with 7 rounds in each cell of the gate set: threads 1 and 16 and
processes 16, at 1 KB and at 100 KB. The test of
the GIL record runs a tiny profile of `zeo` in place of the quick set. No
test needs the native client itself.

`just bench test` runs it.
"""

import datetime
import json
import pathlib
import socket
import subprocess
import sys
import sysconfig

import jsonschema
import pytest

import shootout
from shootout import KB, Cell, Profile, run
from shootout.storages import STORAGES, zconfig

BENCH = pathlib.Path(__file__).parents[2]
SCHEMA_PATH = BENCH / "schema" / "results.schema.json"
README = BENCH / "README.md"
TIMEOUT = 30
SCENARIOS = ("add", "update", "read_cold", "read_warm", "read_hot")
# The cells of the gate set: the gate counts each of them.
CELLS = (
    ("threads", 1, KB),
    ("threads", 16, KB),
    ("processes", 16, KB),
    ("threads", 1, 100 * KB),
    ("threads", 16, 100 * KB),
    ("processes", 16, 100 * KB),
)
# The time of a transaction of each storage in every round. The cost of
# zoe-native above the floor is a third of the cost of ZEO.
ZEO = dict.fromkeys(SCENARIOS, 1.0)
FLOOR = dict.fromkeys(SCENARIOS, 0.25)
THREE_TIMES = dict.fromkeys(SCENARIOS, 0.5)
ROUNDS = 7
# The key of `<zoeclient>` that selects the native inside.
NATIVE = "native true"
TINY = Profile(
    cells=(Cell("threads", 2, KB), Cell("processes", 2, KB)),
    processes=1,
    values=1,
)


@pytest.fixture(scope="module")
def schema():
    return json.loads(SCHEMA_PATH.read_text())


@pytest.fixture
def tiny(monkeypatch):
    monkeypatch.setattr(shootout, "QUICK", TINY)


def cost(ratio):
    """Return the time of zoe-native whose cost ratio to ZEO is ratio."""
    return FLOOR["add"] + (ZEO["add"] - FLOOR["add"]) / ratio


def result_of(storage, seconds):
    """Return a gate result of storage, with seconds[name] in each round.

    A scenario that is not a key of seconds has no cell in the result.
    """
    scenarios = [
        {
            "name": name,
            "mode": mode,
            "concurrency": concurrency,
            "object_size": size,
            "objects_per_transaction": 100,
            "samples": ROUNDS,
            "ops_per_second": concurrency * 100 / seconds[name],
            "mean_seconds": seconds[name],
            "p50_seconds": seconds[name],
            "p99_seconds": seconds[name],
            "values": [
                {"round": number, "seconds": seconds[name]} for number in range(ROUNDS)
            ],
        }
        for name in SCENARIOS
        if name in seconds
        for mode, concurrency, size in CELLS
    ]
    return {
        "schema_version": 1,
        "storage": storage,
        "machine": {
            "name": "reference",
            "cpu": "synthetic",
            "logical_cpus": 8,
            "memory_bytes": 1 << 34,
            "os": "linux",
            "kernel": "6",
            "arch": "x86_64",
            "disk": None,
        },
        "python": {
            "implementation": "cpython",
            "version": "3.13.0",
            "free_threaded": False,
            "compiler": "gcc",
        },
        "software": {"zodbshootout": "0", "ZODB": "6"},
        "run": {
            "quick": False,
            "set": "gate",
            "started": STARTED,
            "commit": "0" * 12,
        },
        "scenarios": scenarios,
    }


# One start for the results of one gate run: it pairs their rounds.
STARTED = datetime.datetime.now(datetime.UTC).isoformat()


def write_result(directory, storage, seconds, schema):
    result = result_of(storage, seconds)
    jsonschema.validate(result, schema)
    path = directory / "gate" / "reference" / "cp313" / storage / "run.json"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(result))


def gate(directory, storage):
    return subprocess.run(
        [
            sys.executable,
            "-m",
            "shootout",
            "gate",
            "--storage",
            storage,
            "--baseline",
            "zeo",
            str(directory),
        ],
        capture_output=True,
        text=True,
        check=False,
    )


def output(result):
    return result.stdout + result.stderr


def rows(result, scenario=None, size=None):
    """Return the lines of the report that give a cell of scenario at size.

    Without scenario, return the lines of every scenario. Without size,
    return the lines of every size.
    """
    return [
        line
        for line in result.stdout.splitlines()
        if line.split()[:1] in ([[scenario]] if scenario else [[s] for s in SCENARIOS])
        and line.split()[1:2] in (["threads"], ["processes"])
        and size in (None, int(line.split()[3]))
    ]


def failed(result):
    return [line for line in result.stdout.splitlines() if "FAIL" in line]


def test_zoe_native_and_relstorage_hf_are_storages_of_the_harness():
    native = STORAGES["zoe-native"]
    assert native.multi_process is True
    assert native.needs_dsn is False
    history_free = STORAGES["relstorage-hf"]
    assert history_free.multi_process is True
    assert history_free.needs_dsn is True


def test_the_zconfig_of_zoe_native_opens_zoeclient_with_the_native_inside(tmp_path):
    with zconfig(STORAGES["zoe-native"], tmp_path, None) as text:
        assert "%import zoe" in text
        assert "<zoeclient>" in text and "</zoeclient>" in text, text
        body = text.split("<zoeclient>", 1)[1].split("</zoeclient>", 1)[0]
        lines = [line.strip() for line in body.splitlines()]
        assert NATIVE in lines, text
        servers = [line.split()[1] for line in lines if line.startswith("server ")]
        assert len(servers) == 1, text
        # zoe serve listens on the address of the section.
        host, port = servers[0].rsplit(":", 1)
        socket.create_connection((host.strip("[]"), int(port)), TIMEOUT).close()


def test_the_zconfig_of_relstorage_hf_keeps_no_history(tmp_path):
    dsn = "dbname=bench"
    with zconfig(STORAGES["relstorage"], tmp_path, dsn) as text:
        history = [line.strip() for line in text.splitlines()]
    with zconfig(STORAGES["relstorage-hf"], tmp_path, dsn) as text:
        history_free = [line.strip() for line in text.splitlines()]
    assert "keep-history false" in history_free, history_free
    # keep-history is a key of <relstorage>, not of <postgresql>.
    key = history_free.index("keep-history false")
    assert history_free.index("<relstorage>") < key
    assert key < history_free.index("<postgresql>") or key > history_free.index(
        "</postgresql>"
    )
    assert key < history_free.index("</relstorage>")

    # Every other setting is the default, as for relstorage.
    def settings(lines):
        return [
            line
            for line in lines
            if not line.startswith("<zodb ") and line != "keep-history false"
        ]

    assert settings(history_free) == settings(history)


def test_the_default_ratio_is_three_for_zoe_native_and_parity_for_zoe_server(
    tmp_path, schema
):
    passing = tmp_path / "passing"
    write_result(passing, "zeo", ZEO, schema)
    write_result(passing, "floor", FLOOR, schema)
    write_result(passing, "zoe-native", THREE_TIMES, schema)
    write_result(passing, "zoe-server", ZEO, schema)
    result = gate(passing, "zoe-native")
    assert result.returncode == 0, output(result)
    result = gate(passing, "zoe-server")
    assert result.returncode == 0, output(result)

    failing = tmp_path / "failing"
    write_result(failing, "zeo", ZEO, schema)
    write_result(failing, "floor", FLOOR, schema)
    write_result(failing, "zoe-native", dict.fromkeys(SCENARIOS, cost(2.99)), schema)
    result = gate(failing, "zoe-native")
    assert result.returncode == 1, output(result)
    assert "2.99" in result.stdout
    # Every cell counts, read_hot in both sizes too.
    assert failed(result) == rows(result)
    assert len(rows(result)) == len(SCENARIOS) * len(CELLS), result.stdout
    assert "left out" not in result.stdout


def test_read_hot_counts_in_every_size_for_zoe_native_and_stays_left_out_for_zoe_server(
    tmp_path, schema
):
    write_result(tmp_path, "zeo", ZEO, schema)
    write_result(tmp_path, "floor", FLOOR, schema)
    write_result(tmp_path, "zoe-native", {**THREE_TIMES, "read_hot": cost(2.9)}, schema)
    write_result(tmp_path, "zoe-server", {**ZEO, "read_hot": 2.0}, schema)

    native = gate(tmp_path, "zoe-native")
    assert native.returncode == 1, output(native)
    assert "2.90" in native.stdout
    hot = rows(native, "read_hot")
    assert len(hot) == len(CELLS), native.stdout
    assert failed(native) == hot, native.stdout

    server = gate(tmp_path, "zoe-server")
    assert server.returncode == 0, output(server)
    hot = rows(server, "read_hot")
    assert len(hot) == len(CELLS), server.stdout
    for row in hot:
        assert "left out" in row, row
        assert "FAIL" not in row, row


def test_a_missing_read_hot_cell_fails_zoe_native_only(tmp_path, schema):
    others = [name for name in SCENARIOS if name != "read_hot"]
    write_result(tmp_path, "zeo", ZEO, schema)
    write_result(tmp_path, "floor", FLOOR, schema)
    write_result(tmp_path, "zoe-native", {n: THREE_TIMES[n] for n in others}, schema)
    write_result(tmp_path, "zoe-server", {n: ZEO[n] for n in others}, schema)

    native = gate(tmp_path, "zoe-native")
    assert native.returncode == 1, output(native)
    hot = rows(native, "read_hot")
    assert len(hot) == len(CELLS), native.stdout
    for row in hot:
        assert "missing" in row, row

    server = gate(tmp_path, "zoe-server")
    assert server.returncode == 0, output(server)


def test_a_cell_without_the_floor_fails_zoe_native(tmp_path, schema):
    write_result(tmp_path, "zeo", ZEO, schema)
    write_result(tmp_path, "zoe-native", THREE_TIMES, schema)
    native = gate(tmp_path, "zoe-native")
    assert native.returncode == 1, output(native)
    assert len(rows(native)) == len(SCENARIOS) * len(CELLS), native.stdout
    for row in rows(native):
        assert "missing" in row, row


def with_workers(result, workers):
    for scenario in result["scenarios"]:
        scenario["workers"] = workers
    return result


def test_the_schema_accepts_the_gil_record_of_each_worker(schema):
    workers = [
        {"gil_enabled": False, "gil_enabled_by": []},
        {"gil_enabled": True, "gil_enabled_by": ["psycopg2._psycopg"]},
    ]
    jsonschema.validate(with_workers(result_of("zeo", ZEO), workers), schema)
    # A result without the record stays valid.
    jsonschema.validate(result_of("zeo", ZEO), schema)
    for worker in (
        {"gil_enabled": True},
        {"gil_enabled_by": []},
        {"gil_enabled": "yes", "gil_enabled_by": []},
        {"gil_enabled": True, "gil_enabled_by": "psycopg2._psycopg"},
    ):
        with pytest.raises(jsonschema.ValidationError):
            jsonschema.validate(with_workers(result_of("zeo", ZEO), [worker]), schema)


def test_a_run_records_the_gil_of_each_worker(tiny, schema, tmp_path):
    result = run("zeo", quick=True, workdir=tmp_path / "zeo")
    jsonschema.validate(result, schema)
    free_threaded = bool(sysconfig.get_config_var("Py_GIL_DISABLED"))
    for scenario in result["scenarios"]:
        workers = scenario["workers"]
        assert workers, scenario
        for worker in workers:
            assert isinstance(worker["gil_enabled"], bool), worker
            assert all(isinstance(name, str) for name in worker["gil_enabled_by"])
            if not free_threaded:
                # A build with the GIL keeps it on, and no module turns it on.
                assert worker["gil_enabled"] is True, worker
                assert worker["gil_enabled_by"] == [], worker
            elif worker["gil_enabled_by"]:
                assert worker["gil_enabled"] is True, worker


def test_a_cold_read_from_the_client_cache_fails_the_run():
    check = shootout.check_cold_loads
    check("zoe-native", objects_read=1600, server_loads=1600)
    check("zoe-native", objects_read=1600, server_loads=4000)
    with pytest.raises(RuntimeError, match="read_cold") as raised:
        check("zoe-native", objects_read=1600, server_loads=1599)
    assert "cache" in str(raised.value)


def test_the_readme_names_the_phase_3_gate():
    text = README.read_text()
    assert "zoe-native" in text
    assert "relstorage-hf" in text
    assert "## The gate" in text
    section = text.split("## The gate", 1)[1].split("\n## ", 1)[0]
    assert "phase 3" in section
    assert "zoe-native" in section
    assert "read_hot" in section
