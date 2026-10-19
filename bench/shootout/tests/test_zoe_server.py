"""Acceptance test of `zoe serve` in the harness, and of the gate report.

The harness measures `zoe-server`: an unmodified ZEO client, through the
`<zeoclient>` section, of a `zoe serve` that the harness starts on a store
in its work directory, with its default settings. Several processes can
use it, so it runs in the cells of ZEO, threads and processes. The harness
finds the binary through `ZOE_BINARY`, or else as `target/release/zoe` of
the repository, and builds it there when it is missing.

`python -m shootout gate --storage S --baseline B --ratio R DIR` reads the
results under DIR, keeps the last result of each storage per machine and
Python build, and compares `ops_per_second` cell by cell. It prints one
line per scenario and cell with the ratio, and exits with 0 when every
ratio is at least R, and with 1 otherwise or when a cell of B has no
counterpart in S. It leaves `read_hot` out, because `read_hot` reads from
the cache of the client only. The phase 2 gate is `--storage zoe-server
--baseline zeo` with the default ratio 1, on the full set of the reference
machine.

The test runs a tiny profile in place of the quick set.
"""

import datetime
import json
import pathlib
import subprocess
import sys

import jsonschema
import pytest
import shootout
from shootout import KB, Cell, Profile, run
from shootout.storages import STORAGES, zconfig

SCHEMA_PATH = pathlib.Path(__file__).parents[2] / "schema" / "results.schema.json"
SCENARIOS = {"add", "update", "read_cold", "read_warm", "read_hot"}
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


def cells(result):
    return {
        (s["name"], s["mode"], s["concurrency"], s["object_size"])
        for s in result["scenarios"]
    }


def test_zoe_server_is_a_storage_of_the_harness():
    storage = STORAGES["zoe-server"]
    assert storage.name == "zoe-server"
    assert storage.multi_process is True
    assert storage.needs_dsn is False


def test_the_configuration_is_a_zeoclient_of_zoe_serve(tmp_path):
    import ZODB.config
    from ZEO.ClientStorage import ClientStorage

    with zconfig(STORAGES["zoe-server"], tmp_path, None) as text:
        assert "<zeoclient>" in text
        db = ZODB.config.databaseFromString(text)
        try:
            assert isinstance(db.storage, ClientStorage)
            # It is zoe serve: get_info says that undo is supported.
            assert db.storage.supportsUndo() is True
            with db.transaction() as connection:
                connection.root()["value"] = 1
        finally:
            db.close()
    # The store lives in the work directory.
    assert any(path.name == "zoe.redb" for path in tmp_path.rglob("zoe.redb"))


def test_zoe_server_runs_in_the_cells_of_zeo(tiny, schema, tmp_path):
    results = {
        storage: run(storage, quick=True, workdir=tmp_path / storage)
        for storage in ("zeo", "zoe-server")
    }
    for storage, result in results.items():
        jsonschema.validate(result, schema)
        assert result["storage"] == storage
        assert SCENARIOS <= {s["name"] for s in result["scenarios"]}
        for scenario in result["scenarios"]:
            assert scenario["ops_per_second"] > 0
    assert cells(results["zoe-server"]) == cells(results["zeo"])


def result_of(storage, ops):
    """A result of storage, valid for the schema, with ops in each cell."""
    scenarios = [
        {
            "name": name,
            "mode": mode,
            "concurrency": 16,
            "object_size": KB,
            "objects_per_transaction": 100,
            "samples": 5,
            "ops_per_second": ops,
            "mean_seconds": 1600 / ops,
            "p50_seconds": 1600 / ops,
            "p99_seconds": 1600 / ops,
        }
        for name in sorted(SCENARIOS)
        for mode in ("threads", "processes")
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
            "started": datetime.datetime.now(datetime.UTC).isoformat(),
            "commit": "0" * 12,
        },
        "scenarios": scenarios,
    }


def write_result(directory, storage, ops, schema):
    result = result_of(storage, ops)
    jsonschema.validate(result, schema)
    path = directory / "results" / "reference" / "cp313" / storage / "run.json"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(result))


def gate(directory, *args):
    return subprocess.run(
        [sys.executable, "-m", "shootout", "gate", *args, str(directory)],
        capture_output=True,
        text=True,
        check=False,
    )


def test_the_gate_report_passes_at_three_times_zeo(tmp_path, schema):
    write_result(tmp_path, "zeo", 1000.0, schema)
    write_result(tmp_path, "zoe-server", 3000.0, schema)
    result = gate(
        tmp_path, "--storage", "zoe-server", "--baseline", "zeo", "--ratio", "3"
    )
    assert result.returncode == 0, result.stdout + result.stderr
    assert "3.0" in result.stdout


def test_the_gate_report_fails_below_the_ratio(tmp_path, schema):
    write_result(tmp_path, "zeo", 1000.0, schema)
    write_result(tmp_path, "zoe-server", 2900.0, schema)
    result = gate(
        tmp_path, "--storage", "zoe-server", "--baseline", "zeo", "--ratio", "3"
    )
    assert result.returncode == 1
    assert "2.9" in result.stdout


def test_the_gate_report_fails_when_a_cell_is_missing(tmp_path, schema):
    write_result(tmp_path, "zeo", 1000.0, schema)
    result = gate(
        tmp_path, "--storage", "zoe-server", "--baseline", "zeo", "--ratio", "3"
    )
    assert result.returncode == 1
