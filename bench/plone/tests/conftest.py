"""The fixtures and the helpers that the tests of bench/plone share."""

import importlib

import pytest


@pytest.fixture
def lock(tmp_path, monkeypatch):
    """Give the test a bench lock of its own, so that it waits for no
    benchmark of the machine. Return the path of the lock file."""
    path = tmp_path / "bench.lock"
    monkeypatch.setenv("ZOE_BENCH_LOCK", str(path))
    return path


def start(directory, execute, *options):
    """Start or resume the showcase in `directory`, with the fake runner
    `execute` and no wait for the load of the machine."""
    showcase = importlib.import_module("plonebench.showcase")
    argv = ["--dir", str(directory), "--max-load", "1000", *options]
    return showcase.main(argv, execute=execute)
