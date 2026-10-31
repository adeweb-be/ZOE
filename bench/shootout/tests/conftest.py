"""The fixtures and the helpers that the tests of the harness share."""

import json
from pathlib import Path

import pytest

import shootout
from shootout import KB


@pytest.fixture(scope="module")
def schema(request):
    """Return the JSON schema that the module names in SCHEMA_PATH."""
    return json.loads(Path(request.module.SCHEMA_PATH).read_text())


@pytest.fixture
def tiny(request, monkeypatch):
    """Make the quick set of the harness the profile TINY of the module."""
    monkeypatch.setattr(shootout, "QUICK", request.module.TINY)


def output(result):
    """Return what a command of the harness printed."""
    return result.stdout + result.stderr


def write_gate_result(root, storage, python, ops, sizes=(KB,)):
    """Write a result of storage on python, such as cp314t, with ops per name,
    for the gate.

    Each name has one cell in each object size of sizes.
    """
    builds = {"cp313": ("3.13.0", False), "cp314t": ("3.14.0", True)}
    version, free_threaded = builds[python]
    result = {
        "storage": storage,
        "machine": {"name": "reference"},
        "python": {
            "implementation": "cpython",
            "version": version,
            "free_threaded": free_threaded,
        },
        "run": {"quick": False, "started": "2026-01-01T00:00:00+00:00", "commit": None},
        "scenarios": [
            {
                "name": name,
                "mode": "threads",
                "concurrency": 16,
                "object_size": size,
                "ops_per_second": value,
            }
            for name, value in ops.items()
            for size in sizes
        ],
    }
    path = root / "results" / "reference" / python / storage / "run.json"
    path.parent.mkdir(parents=True)
    path.write_text(json.dumps(result))
