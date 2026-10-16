"""Acceptance test of the phase 3 gate in the harness: the native gate.

N4 sets the gate of phase 3 (ADR 0039): `zoe serve` through the native
client (`zoe-native`) is faster than ZEO 6 through `ZEO.ClientStorage`
(`zeo`) in every cell of the gate set. The gate set is the 5 scenarios of
zodbshootout in 3 shapes (threads 1, threads 16, processes 16), at 1 KB and
at 100 KB: 30 cells. A transaction holds 100 objects at 1 KB and 10 at
100 KB.

`shootout.native` drives both storages itself. For each cell, each storage
gets one server, one populate and one set of worker processes. A scenario
runs in pairs of batches, one batch of each storage, and the first storage
changes from one pair to the next. A batch runs the same number of
transactions in a row in each worker, as the inner loops of zodbshootout
do: about 20 ms of them, at most 20, from the time of the warmup pair.
The ratio of a pair is `t_zeo / t_zoe`, where `t` is the time of the
slowest worker for one transaction. After at
least 10 pairs, a scenario passes when the 99 % confidence interval of the
mean of the log of the ratios is above 0, and fails when it is below 0. It
stops at its first verdict, at 100 pairs or after 10 s of pairs. A scenario
without a verdict is undecided and fails the gate.

`python -m shootout native report FILE` prints the rows of a result and
the verdict: 0 when every cell passes, 1 when a cell is slower, undecided
or missing, 2 when the file is not a result of the gate.

The harness keeps two storages for the full set of zodbshootout:
`zoe-native` (`<zoeclient>` with `native true`) and `relstorage-hf`
(RelStorage with `keep-history false`). `shootout.check_cold_loads` fails
a `read_cold` of `zoe-native` that the server did not load. Each worker
process records its GIL.

The rule and report tests use synthetic pairs. One test runs a tiny gate
end to end against both servers. `just bench test` runs it.
"""

import json
import math
import pathlib
import socket
import subprocess
import sys
import sysconfig

import jsonschema
import pytest

import shootout
from shootout import KB, Cell, Profile, native, run
from shootout.storages import STORAGES, zconfig

BENCH = pathlib.Path(__file__).parents[2]
README = BENCH / "README.md"
RESULTS_SCHEMA = BENCH / "schema" / "results.schema.json"
TIMEOUT = 30
SCENARIOS = ("add", "update", "read_cold", "read_warm", "read_hot")
SHAPES = (("threads", 1), ("threads", 16), ("processes", 16))
# The key of `<zoeclient>` that selects the native inside.
NATIVE = "native true"
TINY = Profile(
    cells=(Cell("threads", 2, KB), Cell("processes", 2, KB)),
    processes=1,
    values=1,
)


@pytest.fixture(scope="module")
def schema():
    return json.loads(native.SCHEMA.read_text())


def spread(ratio, n):
    """Return n ratios around ratio, alternately 5 % above and below."""
    return [ratio * (1.05 if k % 2 else 1 / 1.05) for k in range(n)]


def pairs_of(ratios):
    """Return the pairs whose ratios t_zeo / t_zoe are ratios."""
    return [
        {
            "first": native.order(k)[0],
            "seconds": {"zeo": 0.01 * r, "zoe-native": 0.01},
        }
        for k, r in enumerate(ratios)
    ]


def result_of(ratios_of):
    """Return a result of the gate with the pairs of ratios_of(cell, scenario).

    A scenario for which ratios_of gives None has no entry.
    """
    cells = []
    for cell in native.CELLS:
        scenarios = [
            {"name": name, "transactions": 1, "pairs": pairs_of(r)}
            for name in SCENARIOS
            if (r := ratios_of(cell, name)) is not None
        ]
        if not scenarios:
            continue
        workers = [{"gil_enabled": True, "gil_enabled_by": []}]
        cells.append(
            {
                "mode": cell.mode,
                "concurrency": cell.concurrency,
                "object_size": cell.object_size,
                "objects_per_transaction": cell.objects,
                "workers": {"zeo": workers, "zoe-native": workers},
                "scenarios": scenarios,
            }
        )
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
        "software": {"ZEO": "6", "zoedb": "0", "zoe": "0"},
        "run": {"started": "2026-01-01T00:00:00Z", "commit": "0" * 12, "seconds": 60},
        "rule": {
            "min_pairs": 10,
            "max_pairs": 100,
            "max_seconds": 10,
            "level": 0.99,
            "batch_seconds": 0.02,
            "max_transactions": 20,
        },
        "cells": cells,
    }


def report(tmp_path, result, schema):
    jsonschema.validate(result, schema)
    path = tmp_path / "result.json"
    path.write_text(json.dumps(result))
    return subprocess.run(
        [sys.executable, "-m", "shootout", "native", "report", str(path)],
        capture_output=True,
        text=True,
        check=False,
    )


def rows(done, verdict):
    return [line for line in done.stdout.splitlines() if line.endswith(verdict)]


def test_the_gate_set_is_30_cells():
    assert native.SCENARIOS == SCENARIOS
    assert {(c.mode, c.concurrency) for c in native.CELLS} == set(SHAPES)
    assert {c.object_size for c in native.CELLS} == {KB, 100 * KB}
    assert len(native.CELLS) * len(native.SCENARIOS) == 30
    assert {c.object_size: c.objects for c in native.CELLS} == {KB: 100, 100 * KB: 10}


def test_the_rule_is_a_constant_of_the_gate():
    assert native.MIN_PAIRS == 10
    assert native.MAX_PAIRS == 100
    assert native.MAX_SECONDS == 10
    assert native.LEVEL == 0.99
    assert native.PAIR == ("zeo", "zoe-native")


def test_a_batch_lasts_about_20_ms_with_at_most_20_transactions():
    assert native.BATCH_SECONDS == 0.02
    assert native.MAX_TRANSACTIONS == 20
    assert native.transactions_for(0.0035) == 6
    assert native.transactions_for(0.05) == 1
    assert native.transactions_for(0.0001) == 20
    assert native.transactions_for(0.0) == 20


def test_the_first_storage_changes_from_one_pair_to_the_next():
    assert [native.order(k) for k in range(4)] == [
        ("zeo", "zoe-native"),
        ("zoe-native", "zeo"),
        ("zeo", "zoe-native"),
        ("zoe-native", "zeo"),
    ]


def test_a_faster_storage_passes_and_a_slower_one_fails():
    faster = native.decide(spread(1.2, 10))
    assert faster.verdict == "pass"
    assert faster.low > 1
    assert math.isclose(faster.ratio, 1.2, rel_tol=0.01)
    slower = native.decide(spread(0.8, 10))
    assert slower.verdict == "fail"
    assert slower.high < 1


def test_no_verdict_before_the_minimum_of_pairs():
    assert native.decide(spread(3.0, 9)).verdict is None
    assert native.decide(spread(3.0, 10)).verdict == "pass"


def test_a_tie_gives_no_verdict():
    assert native.decide(spread(1.0, 100)).verdict is None
    # A noisy ratio near 1 gives none either.
    assert native.decide(spread(1.01, 10)).verdict is None


def test_the_interval_is_the_t_interval_at_99_percent():
    # The quantile 0.995 of Student's t with 9 degrees of freedom is 3.2498.
    assert math.isclose(native._t_quantile(0.995, 9), 3.2498, rel_tol=1e-3)
    assert math.isclose(native._t_quantile(0.995, 99), 2.6264, rel_tol=1e-3)


def test_the_gate_passes_when_every_cell_passes(tmp_path, schema):
    done = report(tmp_path, result_of(lambda c, s: spread(1.5, 10)), schema)
    assert done.returncode == 0, done.stdout + done.stderr
    assert len(rows(done, "pass")) == 30
    last = done.stdout.splitlines()[-1]
    assert last.startswith("gate passed:"), last
    assert "on cp314t" in last
    assert last.endswith("30 cells, 0 slower, 0 undecided, 0 missing")


def test_a_slower_cell_fails_the_gate(tmp_path, schema):
    def ratios(cell, scenario):
        slow = scenario == "read_warm" and cell.concurrency == 1
        return spread(0.7 if slow else 1.5, 10)

    done = report(tmp_path, result_of(ratios), schema)
    assert done.returncode == 1, done.stdout
    assert [line.split()[:4] for line in rows(done, "FAIL")] == [
        ["read_warm", "threads", "1", "1024"],
        ["read_warm", "threads", "1", "102400"],
    ]
    assert done.stdout.splitlines()[-1].endswith(
        "30 cells, 2 slower, 0 undecided, 0 missing"
    )


def test_an_undecided_cell_fails_the_gate(tmp_path, schema):
    def ratios(cell, scenario):
        tie = scenario == "update" and cell == native.CELLS[0]
        return spread(1.0, 100) if tie else spread(1.5, 10)

    done = report(tmp_path, result_of(ratios), schema)
    assert done.returncode == 1, done.stdout
    assert len(rows(done, "UNDECIDED")) == 1
    assert done.stdout.splitlines()[-1].endswith(
        "30 cells, 0 slower, 1 undecided, 0 missing"
    )


def test_a_missing_cell_fails_the_gate(tmp_path, schema):
    def ratios(cell, scenario):
        return None if scenario == "read_hot" else spread(1.5, 10)

    done = report(tmp_path, result_of(ratios), schema)
    assert done.returncode == 1, done.stdout
    assert done.stdout.splitlines()[-1].endswith(
        "30 cells, 0 slower, 0 undecided, 6 missing"
    )


def test_a_file_that_is_not_a_result_is_refused(tmp_path):
    path = tmp_path / "other.json"
    path.write_text(json.dumps({"schema_version": 1}))
    done = subprocess.run(
        [sys.executable, "-m", "shootout", "native", "report", str(path)],
        capture_output=True,
        text=True,
        check=False,
    )
    assert done.returncode == 2, done.stdout + done.stderr


def test_a_tiny_gate_runs_end_to_end(tmp_path, schema, monkeypatch):
    monkeypatch.setattr(native, "MIN_PAIRS", 2)
    monkeypatch.setattr(native, "MAX_PAIRS", 3)
    cells = [native.Cell("threads", 2, KB), native.Cell("processes", 2, KB)]
    out = tmp_path / "out.txt"
    with out.open("w") as stream:
        result = native.run(cells=cells, workdir=tmp_path / "work", out=stream)
    jsonschema.validate(result, schema)
    assert result["rule"]["min_pairs"] == 2
    assert [(c["mode"], c["concurrency"]) for c in result["cells"]] == [
        ("threads", 2),
        ("processes", 2),
    ]
    for cell in result["cells"]:
        assert [s["name"] for s in cell["scenarios"]] == list(SCENARIOS)
        processes = 1 if cell["mode"] == "threads" else 2
        for name in ("zeo", "zoe-native"):
            assert len(cell["workers"][name]) == processes, cell["workers"]
        for scenario in cell["scenarios"]:
            pairs = scenario["pairs"]
            assert 1 <= scenario["transactions"] <= 20, scenario
            assert 2 <= len(pairs) <= 3, scenario
            assert [p["first"] for p in pairs] == [
                native.order(k)[0] for k in range(len(pairs))
            ]
    # The run prints one row per scenario, and leaves no store behind.
    assert len(out.read_text().splitlines()) == 1 + 2 * len(SCENARIOS)
    assert list((tmp_path / "work").iterdir()) == []


def test_zoe_native_and_relstorage_hf_are_storages_of_the_harness():
    native_storage = STORAGES["zoe-native"]
    assert native_storage.multi_process is True
    assert native_storage.needs_dsn is False
    history_free = STORAGES["relstorage-hf"]
    assert history_free.multi_process is True
    assert history_free.needs_dsn is True
    assert "floor" not in STORAGES


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


def test_a_run_of_zodbshootout_records_the_gil_of_each_worker(tmp_path, monkeypatch):
    monkeypatch.setattr(shootout, "QUICK", TINY)
    result = run("zeo", quick=True, workdir=tmp_path / "zeo")
    jsonschema.validate(result, json.loads(RESULTS_SCHEMA.read_text()))
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
    assert "## The gate" in text
    section = text.split("## The gate", 1)[1].split("\n## ", 1)[0]
    assert "phase 3" in section
    assert "just bench native" in section
    assert "zoe-native" in section
    assert "ADR 0039" in section
