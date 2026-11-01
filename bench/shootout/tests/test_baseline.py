"""Acceptance test of the baseline harness.

The harness runs zodbshootout in its quick mode against one storage and
returns results that validate against the published schema.
"""

import os
import pathlib

import jsonschema
import pytest
from shootout import run

SCHEMA_PATH = pathlib.Path(__file__).parents[2] / "schema" / "results.schema.json"
SCENARIOS = {"add", "update", "read_cold", "read_warm", "read_hot"}


def check(result, storage, schema):
    jsonschema.validate(result, schema)
    assert result["storage"] == storage
    assert result["machine"]["cpu"]
    assert result["python"]["version"]
    assert SCENARIOS <= {scenario["name"] for scenario in result["scenarios"]}
    for scenario in result["scenarios"]:
        assert scenario["ops_per_second"] > 0
        assert scenario["p50_seconds"] <= scenario["p99_seconds"]


@pytest.mark.parametrize("storage", ["filestorage", "zeo"])
def test_quick_run(storage, schema, tmp_path):
    check(run(storage, quick=True, workdir=tmp_path), storage, schema)


@pytest.mark.skipif(
    "ZOE_BENCH_PG_DSN" not in os.environ,
    reason="needs a PostgreSQL database in ZOE_BENCH_PG_DSN",
)
def test_quick_run_relstorage(schema, tmp_path):
    result = run(
        "relstorage",
        quick=True,
        workdir=tmp_path,
        dsn=os.environ["ZOE_BENCH_PG_DSN"],
    )
    check(result, "relstorage", schema)
