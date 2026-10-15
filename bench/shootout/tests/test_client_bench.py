"""Acceptance test of the client benchmark of F28.

The client benchmark compares the two clients on the same server, through
the storage API (`loadBefore`, `tpc_begin`, `store`, `tpc_vote`,
`tpc_finish`) and without ZODB: `ZEO.ClientStorage` and the native client,
`zoe.zeo.ClientStorage`. zodbshootout mixes the client with ZODB and the
server; this benchmark isolates the client.

The benchmark is the module `shootout.client`, the subcommand `client` of
the harness. `just bench client ...` calls `python -m shootout client ...`.
Its schema is `bench/schema/client.schema.json`.

- `python -m shootout client run --objects N --server zoe-serve --workdir
  DIR --output FILE` runs each workload of each shape and record size with
  both clients, in interleaved rounds, against each server that `--server`
  names (`zoe-serve`, `zeo`; both by default). It writes one result
  document to FILE.
- `python -m shootout client report FILE` prints one row per shape,
  workload, record size and server. A row starts with the mode, the
  concurrency, the workload, the record size in bytes and the server, and
  ends with its verdict: `pass`, `FAIL`, or `missing` when a row of
  `ZEO.ClientStorage` has no counterpart. The report exits with 0 when the
  native client beats `ZEO.ClientStorage` in every row against `zoe serve`:
  a higher rate and a lower client CPU per object. It exits with 1 when a
  row fails or is missing.

The shapes are one process with 1 and 16 threads, and 16 processes. The
workloads are `cold` (loads of distinct oids with an empty cache), `warm`
(the same loads from a client that reopens the cache of the run before)
and `commit` (commits of 100 records). The record sizes are 1 KB and
100 KB.

A result document holds `schema_version`, `machine` and `python` (as in
the results of the harness), `gil_enabled_by` (the modules that turned the
GIL back on), `software` (the versions of both clients: `ZEO` and
`zoedb`), `run` (`started`, `commit`, `objects`) and `rows`. A row holds
`client` (`ZEO.ClientStorage` or `zoe.zeo.ClientStorage`), `server`,
`mode`, `concurrency`, `workload`, `record_size`, `calls_per_second`,
`p50_seconds`, `p99_seconds`, `client_cpu_per_object_seconds`, `rounds`
and `spread`.

The report tests write synthetic results of both clients that are valid
for the schema. The smoke test runs each workload with 10 objects against
`zoe serve`.

`just bench test` runs it.
"""

import datetime
import json
import pathlib
import subprocess
import sys
import sysconfig

import jsonschema
import pytest

BENCH = pathlib.Path(__file__).parents[2]
SCHEMA_PATH = BENCH / "schema" / "client.schema.json"
README = BENCH / "README.md"
# The smoke run starts zoe serve and up to 16 client processes per round.
SMOKE_TIMEOUT = 600
ZEO_CLIENT = "ZEO.ClientStorage"
NATIVE_CLIENT = "zoe.zeo.ClientStorage"
CLIENTS = (ZEO_CLIENT, NATIVE_CLIENT)
SHAPES = (("threads", 1), ("threads", 16), ("processes", 16))
WORKLOADS = ("cold", "warm", "commit")
SIZES = (1024, 102400)
SERVERS = ("zoe-serve", "zeo")
# The key of each row of the report.
KEYS = [
    (mode, concurrency, workload, size, server)
    for mode, concurrency in SHAPES
    for workload in WORKLOADS
    for size in SIZES
    for server in SERVERS
]
# The numbers of each client in the synthetic results: calls per second and
# client CPU per object, in seconds.
ZEO_NUMBERS = (10_000.0, 50e-6)
NATIVE_NUMBERS = (30_000.0, 8e-6)


@pytest.fixture(scope="module")
def schema():
    return json.loads(SCHEMA_PATH.read_text())


def row_of(client, key, numbers):
    mode, concurrency, workload, size, server = key
    rate, cpu = numbers
    return {
        "client": client,
        "server": server,
        "mode": mode,
        "concurrency": concurrency,
        "workload": workload,
        "record_size": size,
        "calls_per_second": rate,
        "p50_seconds": 1 / rate,
        "p99_seconds": 2 / rate,
        "client_cpu_per_object_seconds": cpu,
        "rounds": 3,
        "spread": 0.02,
    }


def rows_of(native=None, drop=()):
    """Return the rows of both clients for every key.

    native maps a key to the numbers of the native client in that row. The
    rows of drop, as pairs of a client and a key, are left out.
    """
    native = native or {}
    rows = []
    for key in KEYS:
        for client, numbers in (
            (ZEO_CLIENT, ZEO_NUMBERS),
            (NATIVE_CLIENT, native.get(key, NATIVE_NUMBERS)),
        ):
            if (client, key) not in drop:
                rows.append(row_of(client, key, numbers))
    return rows


def result_of(rows):
    return {
        "schema_version": 1,
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
            "version": "3.14.0",
            "free_threaded": True,
            "compiler": "gcc",
        },
        "gil_enabled_by": [],
        "software": {"ZEO": "6.2.0", "zoedb": "0.0.0"},
        "run": {
            "started": datetime.datetime.now(datetime.UTC).isoformat(),
            "commit": "0" * 12,
            "objects": 1000,
        },
        "rows": rows,
    }


def write_result(path, rows, schema):
    result = result_of(rows)
    jsonschema.validate(result, schema)
    path.write_text(json.dumps(result))
    return path


def report(path):
    return subprocess.run(
        [sys.executable, "-m", "shootout", "client", "report", str(path)],
        capture_output=True,
        text=True,
        check=False,
    )


def output(result):
    return result.stdout + result.stderr


def report_rows(result):
    """Return the lines of the report that give a row, by their key."""
    rows = {}
    for line in result.stdout.splitlines():
        words = line.split()
        if words[:1] in (["threads"], ["processes"]):
            key = (words[0], int(words[1]), words[2], int(words[3]), words[4])
            assert key not in rows, f"two rows for {key}:\n{result.stdout}"
            rows[key] = line
    return rows


def test_the_report_has_one_row_per_shape_workload_record_size_and_server(
    tmp_path, schema
):
    path = write_result(tmp_path / "client.json", rows_of(), schema)
    result = report(path)
    assert result.returncode == 0, output(result)
    rows = report_rows(result)
    assert set(rows) == set(KEYS), output(result)
    for line in rows.values():
        assert "pass" in line.split(), line


@pytest.mark.parametrize(
    "numbers",
    [(9_000.0, 8e-6), (30_000.0, 60e-6)],
    ids=["a lower rate", "a higher client CPU per object"],
)
def test_the_report_fails_and_names_a_row_where_the_native_client_loses(
    tmp_path, schema, numbers
):
    key = ("threads", 16, "cold", 102400, "zoe-serve")
    path = write_result(tmp_path / "client.json", rows_of({key: numbers}), schema)
    result = report(path)
    assert result.returncode == 1, output(result)
    rows = report_rows(result)
    failed = [line for line in result.stdout.splitlines() if "FAIL" in line]
    assert failed == [rows[key]], output(result)


def test_the_report_fails_a_row_of_zeo_without_a_counterpart(tmp_path, schema):
    key = ("processes", 16, "warm", 1024, "zoe-serve")
    rows = rows_of(drop={(NATIVE_CLIENT, key)})
    path = write_result(tmp_path / "client.json", rows, schema)
    result = report(path)
    assert result.returncode == 1, output(result)
    line = report_rows(result)[key]
    assert "missing" in line or "FAIL" in line, line


@pytest.mark.parametrize("workload", ["cold", "warm"])
@pytest.mark.parametrize("shape", SHAPES, ids=lambda shape: f"{shape[0]}-{shape[1]}")
def test_the_report_needs_the_cold_and_warm_rows_of_each_shape(
    tmp_path, schema, shape, workload
):
    drop = {
        (client, key)
        for client in CLIENTS
        for key in KEYS
        if key[:2] == shape and key[2] == workload
    }
    path = write_result(tmp_path / "client.json", rows_of(drop=drop), schema)
    result = report(path)
    assert result.returncode == 1, output(result)
    named = [
        line
        for line in output(result).splitlines()
        if {shape[0], str(shape[1]), workload} <= set(line.split())
    ]
    assert any("missing" in line for line in named), output(result)


def test_the_readme_describes_the_benchmark_and_its_verdict():
    text = README.read_text()
    assert "## The client benchmark" in text
    section = text.split("## The client benchmark", 1)[1].split("\n## ", 1)[0]
    for words in (
        "just bench client",
        ZEO_CLIENT,
        NATIVE_CLIENT,
        "cold",
        "warm",
        "client CPU per object",
    ):
        assert words in section, words


def test_a_smoke_run_of_each_workload_against_zoe_serve(tmp_path, schema):
    path = tmp_path / "client.json"
    done = subprocess.run(
        [
            sys.executable,
            "-m",
            "shootout",
            "client",
            "run",
            "--objects",
            "10",
            "--server",
            "zoe-serve",
            "--workdir",
            str(tmp_path / "work"),
            "--output",
            str(path),
        ],
        capture_output=True,
        text=True,
        timeout=SMOKE_TIMEOUT,
        check=False,
    )
    assert done.returncode == 0, output(done)
    result = json.loads(path.read_text())
    jsonschema.validate(result, schema)
    assert result["run"]["objects"] == 10
    assert {"ZEO", "zoedb"} <= set(result["software"]), result["software"]
    assert isinstance(result["gil_enabled_by"], list)
    free_threaded = bool(sysconfig.get_config_var("Py_GIL_DISABLED"))
    assert result["python"]["free_threaded"] is free_threaded
    found = {
        (row["client"], row["mode"], row["concurrency"], row["workload"])
        for row in result["rows"]
    }
    expected = {
        (client, mode, concurrency, workload)
        for client in CLIENTS
        for mode, concurrency in SHAPES
        for workload in WORKLOADS
    }
    assert expected <= found, sorted(expected - found)
    for row in result["rows"]:
        assert row["server"] == "zoe-serve", row
        assert row["calls_per_second"] > 0, row
        assert row["client_cpu_per_object_seconds"] >= 0, row
        assert row["rounds"] >= 3, row
        assert row["spread"] >= 0, row
