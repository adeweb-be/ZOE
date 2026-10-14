"""Acceptance test of the parity gate in the gate report.

N4 sets the gate of phase 2: `zoe serve` is at least at parity with ZEO 6
in each zodbshootout cell that reaches the server, both through
`ZEO.ClientStorage`. `read_hot` does not reach the server: it reads from
the cache of the client only. So `python -m shootout gate DIR`:

- checks a ratio of 1 when no `--ratio` is given;
- leaves `read_hot` out: it prints each `read_hot` row with `left out`,
  says why, and counts it neither as below nor as missing;
- counts each other scenario as before: a ratio below 1 or a missing cell
  fails the gate.

`bench/README.md` describes the parity gate.

The test writes synthetic results that are valid for the schema. It runs
no benchmark.
"""

import datetime
import json
import pathlib
import subprocess
import sys

import jsonschema
import pytest

BENCH = pathlib.Path(__file__).parents[2]
SCHEMA_PATH = BENCH / "schema" / "results.schema.json"
README = BENCH / "README.md"
SCENARIOS = ("add", "update", "read_cold", "read_warm", "read_hot")
SERVER_SCENARIOS = [name for name in SCENARIOS if name != "read_hot"]
CELLS = (("threads", 16), ("processes", 16))
PARITY = dict.fromkeys(SCENARIOS, 1000.0)


@pytest.fixture(scope="module")
def schema():
    return json.loads(SCHEMA_PATH.read_text())


def result_of(storage, ops):
    """Return a full result of storage, with ops[name] in each cell of name.

    A scenario that is not a key of ops has no cell in the result.
    """
    scenarios = [
        {
            "name": name,
            "mode": mode,
            "concurrency": concurrency,
            "object_size": 1024,
            "objects_per_transaction": 100,
            "samples": 20,
            "ops_per_second": ops[name],
            "mean_seconds": concurrency * 100 / ops[name],
            "p50_seconds": concurrency * 100 / ops[name],
            "p99_seconds": concurrency * 100 / ops[name],
        }
        for name in SCENARIOS
        if name in ops
        for mode, concurrency in CELLS
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


def output(result):
    return result.stdout + result.stderr


def rows(result, scenario):
    """Return the lines of the report that give a cell of scenario."""
    return [
        line
        for line in result.stdout.splitlines()
        if line.split()[:1] == [scenario]
        and line.split()[1:2] in (["threads"], ["processes"])
    ]


def test_the_default_ratio_is_parity(tmp_path, schema):
    write_result(tmp_path, "zeo", PARITY, schema)
    write_result(tmp_path, "zoe-server", PARITY, schema)
    result = gate(tmp_path)
    assert result.returncode == 0, output(result)


def test_the_default_ratio_fails_below_parity(tmp_path, schema):
    write_result(tmp_path, "zeo", PARITY, schema)
    write_result(tmp_path, "zoe-server", dict.fromkeys(SCENARIOS, 990.0), schema)
    result = gate(tmp_path)
    assert result.returncode == 1, output(result)
    assert "0.99" in result.stdout


def test_read_hot_is_left_out_with_its_reason(tmp_path, schema):
    write_result(tmp_path, "zeo", PARITY, schema)
    write_result(tmp_path, "zoe-server", {**PARITY, "read_hot": 100.0}, schema)
    result = gate(tmp_path)
    assert result.returncode == 0, output(result)
    hot = rows(result, "read_hot")
    assert len(hot) == len(CELLS), result.stdout
    for row in hot:
        assert "left out" in row, row
        assert "FAIL" not in row, row
    # The report says why: read_hot reads from the cache of the client.
    assert "cache" in result.stdout.lower(), result.stdout


def test_a_missing_read_hot_cell_does_not_fail_the_gate(tmp_path, schema):
    write_result(tmp_path, "zeo", PARITY, schema)
    write_result(
        tmp_path, "zoe-server", {n: PARITY[n] for n in SERVER_SCENARIOS}, schema
    )
    result = gate(tmp_path)
    assert result.returncode == 0, output(result)


@pytest.mark.parametrize("scenario", SERVER_SCENARIOS)
def test_each_other_scenario_counts_below_parity(tmp_path, schema, scenario):
    write_result(tmp_path, "zeo", PARITY, schema)
    write_result(tmp_path, "zoe-server", {**PARITY, scenario: 990.0}, schema)
    result = gate(tmp_path)
    assert result.returncode == 1, output(result)
    failed = [line for line in result.stdout.splitlines() if "FAIL" in line]
    assert failed, result.stdout
    assert failed == rows(result, scenario), result.stdout


@pytest.mark.parametrize("scenario", SERVER_SCENARIOS)
def test_a_missing_cell_of_another_scenario_fails_the_gate(tmp_path, schema, scenario):
    write_result(tmp_path, "zeo", PARITY, schema)
    others = {name: ops for name, ops in PARITY.items() if name != scenario}
    write_result(tmp_path, "zoe-server", others, schema)
    result = gate(tmp_path)
    assert result.returncode == 1, output(result)


def test_the_readme_describes_the_parity_gate():
    text = README.read_text()
    assert "## The gate" in text
    section = text.split("## The gate", 1)[1].split("\n## ", 1)[0]
    assert "parity" in section
    assert "read_hot" in section
    # The default ratio of the gate is 1, not 3.
    assert "--ratio 3` are the defaults" not in text
