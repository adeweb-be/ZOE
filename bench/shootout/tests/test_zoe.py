"""Acceptance test of ZOE in the harness.

The harness measures `zoe`, the in-process storage, through the `<zoe>`
stanza with its default settings, beside FileStorage. Like FileStorage, it
runs only in the threads mode, and it runs in the same cells. The results
validate against the published schema and name the version of the zoedb
wheel.

The test runs a tiny profile in place of the quick set, so that it takes
seconds.
"""

import importlib.metadata
import json
import pathlib

import jsonschema
import pytest

import shootout
from shootout import KB, Cell, Profile, run
from shootout.storages import STORAGES, zconfig

SCHEMA_PATH = pathlib.Path(__file__).parents[2] / "schema" / "results.schema.json"
SCENARIOS = {"add", "update", "read_cold", "read_warm", "read_hot"}
# One cell in the threads mode and one in the processes mode, with one sample
# per scenario.
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
    """Make the quick set of the harness the tiny profile."""
    monkeypatch.setattr(shootout, "QUICK", TINY)


def check(result, storage, schema):
    jsonschema.validate(result, schema)
    assert result["storage"] == storage
    assert result["run"]["quick"] is True
    assert SCENARIOS <= {scenario["name"] for scenario in result["scenarios"]}
    for scenario in result["scenarios"]:
        assert scenario["ops_per_second"] > 0
        assert scenario["p50_seconds"] <= scenario["p99_seconds"]


def cells(result):
    """Return the scenarios and cells of a result."""
    return {
        (s["name"], s["mode"], s["concurrency"], s["object_size"])
        for s in result["scenarios"]
    }


def test_zoe_is_a_storage_of_the_harness():
    storage = STORAGES["zoe"]
    assert storage.name == "zoe"
    # Only one process can open a store, as with FileStorage.
    assert storage.multi_process is False
    assert storage.needs_dsn is False


def test_the_configuration_is_the_zoe_stanza_with_its_defaults(tmp_path):
    import ZODB.config
    import zoe

    with zconfig(STORAGES["zoe"], tmp_path, None) as text:
        assert "%import zoe" in text.splitlines()
        body = text[text.index("<zoe>") + len("<zoe>") : text.index("</zoe>")]
        settings = dict(line.split(None, 1) for line in body.strip().splitlines())
        # Every storage runs with its default settings.
        assert set(settings) == {"path"}
        assert pathlib.Path(settings["path"]).is_relative_to(tmp_path)

        db = ZODB.config.databaseFromString(text)
        try:
            assert isinstance(db.storage, zoe.Storage)
            with db.transaction() as connection:
                connection.root()["value"] = 1
        finally:
            db.close()


def test_zoe_runs_in_the_cells_of_filestorage(tiny, schema, tmp_path):
    results = {
        storage: run(storage, quick=True, workdir=tmp_path / storage)
        for storage in ("filestorage", "zoe")
    }
    for storage, result in results.items():
        check(result, storage, schema)

    assert cells(results["zoe"]) == cells(results["filestorage"])
    assert {(mode, c) for _, mode, c, _ in cells(results["zoe"])} == {("threads", 2)}
    assert results["zoe"]["software"]["zoedb"] == importlib.metadata.version("zoedb")
