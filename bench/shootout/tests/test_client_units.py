"""Unit tests of the client benchmark: the rows, the report and the checks."""

import io
import queue
import sys
import threading

import pytest

from shootout import client, clientworker

ZEO = client.ZEO_CLIENT
NATIVE = client.NATIVE_CLIENT


def row(name, server="zoe-serve", rate=10.0, cpu=1e-5):
    return {
        "client": name,
        "server": server,
        "mode": "threads",
        "concurrency": 1,
        "workload": "cold",
        "record_size": 1024,
        "calls_per_second": rate,
        "p50_seconds": 1e-4,
        "p99_seconds": 2e-4,
        "client_cpu_per_object_seconds": cpu,
        "rounds": 3,
        "spread": 0.0,
    }


def every_row(server, zeo=(10.0, 2e-5), native=(20.0, 1e-5)):
    rows = []
    for mode, concurrency in client.SHAPES:
        for workload in client.WORKLOADS:
            for size in client.SIZES:
                for name, (rate, cpu) in ((ZEO, zeo), (NATIVE, native)):
                    one = row(name, server, rate, cpu)
                    one.update(
                        mode=mode,
                        concurrency=concurrency,
                        workload=workload,
                        record_size=size,
                    )
                    rows.append(one)
    return rows


def report(rows):
    out = io.StringIO()
    code = client.report({"rows": rows}, out)
    return code, out.getvalue()


def test_a_row_against_the_zeo_server_does_not_count():
    rows = every_row("zoe-serve") + every_row("zeo", native=(5.0, 3e-5))
    code, text = report(rows)
    assert code == 0, text
    lines = [line for line in text.splitlines() if " zeo " in line]
    assert len(lines) == len(client.SHAPES) * len(client.WORKLOADS) * len(client.SIZES)
    assert all("fail (does not count)" in line for line in lines), text
    assert "FAIL" not in text


def test_a_result_without_rows_of_zoe_serve_is_missing_them():
    code, text = report(every_row("zeo"))
    assert code == 1
    assert "missing" in text


def test_an_equal_rate_does_not_beat():
    rows = every_row("zoe-serve", zeo=(10.0, 2e-5), native=(10.0, 1e-5))
    code, text = report(rows)
    assert code == 1
    assert text.count("FAIL") == len(rows) // 2


def test_two_rows_of_one_client_for_one_key_are_an_error():
    with pytest.raises(ValueError, match="two rows"):
        report([row(ZEO), row(ZEO), row(NATIVE)])


def test_a_row_takes_the_median_of_its_rounds_and_their_spread():
    rounds = [
        {
            "start": 0.0,
            "end": 1.0,
            "cpu_seconds": 0.5,
            "calls": 100,
            "objects": 100,
            "times": [0.01] * 100,
        },
        {
            "start": 0.0,
            "end": 2.0,
            "cpu_seconds": 0.4,
            "calls": 100,
            "objects": 100,
            "times": [0.02] * 100,
        },
        {
            "start": 0.0,
            "end": 0.5,
            "cpu_seconds": 0.3,
            "calls": 100,
            "objects": 100,
            "times": [0.005] * 100,
        },
    ]
    workers = [{"gil_enabled": True, "gil_enabled_by": []}]
    key = (ZEO, "zoe-serve", "threads", 1, "cold", 1024)
    one = client._row(key, rounds, workers)
    assert one["calls_per_second"] == 100.0
    assert one["client_cpu_per_object_seconds"] == pytest.approx(0.004)
    assert one["spread"] == pytest.approx((200 - 50) / 100)
    assert one["p50_seconds"] == pytest.approx(0.01)
    assert one["p99_seconds"] == pytest.approx(0.02)
    assert one["rounds"] == 3
    assert one["workers"] == workers


def test_the_first_client_changes_from_round_to_round():
    assert client._order(0) == (ZEO, NATIVE)
    assert client._order(1) == (NATIVE, ZEO)
    assert client._order(2) == (ZEO, NATIVE)


@pytest.mark.parametrize("calls,loads", [(10, 9), (10, 0)])
def test_a_cold_round_that_a_cache_served_fails(calls, loads):
    spec = {"workload": "cold", "client": ZEO}
    with pytest.raises(RuntimeError, match="cold round"):
        client.check_loads(spec, calls, loads)


def test_a_warm_round_that_reached_the_server_fails():
    spec = {"workload": "warm", "client": NATIVE}
    client.check_loads(spec, 10, 0)
    with pytest.raises(RuntimeError, match="warm round"):
        client.check_loads(spec, 10, 1)
    client.check_loads({"workload": "cold", "client": NATIVE}, 10, 10)


def test_the_cache_holds_the_records_of_a_round():
    for size in client.SIZES:
        assert client.cache_size(1000, size) > 1000 * (size + 512)


def test_the_disk_bound_counts_the_store_and_both_caches():
    objects, size = 1000, 100 * 1024
    needed = client.disk_needed(objects, size, 20)
    assert needed > client.MOST * objects * size + 2 * client.cache_size(
        client.MOST * objects, size
    )
    assert client.disk_needed(objects, size, 40) - needed >= (
        client.MOST * 20 * client.RECORDS_PER_COMMIT * size
    )


def test_a_worker_commits_at_least_once():
    assert clientworker.default_commits(10) == 1
    assert clientworker.default_commits(1000) == 20


def test_the_records_of_a_commit_are_distinct_and_of_their_size():
    records = clientworker.records_of(1024, 3)
    assert len(records) == clientworker.RECORDS_PER_COMMIT
    assert {len(r) for r in records} == {1024}
    assert len(set(records)) == len(records)
    assert records == clientworker.records_of(1024, 3)


# A client that a test imports instead of a real one. Its module and the
# module of its extension mark their import.
FAKE_CLIENT = """\
import fakeextension


class ClientStorage:
    def __init__(self, addr, **kw):
        self.kw = kw

    def lastTransaction(self):
        return bytes(8)

    def loadBefore(self, oid, tid):
        return b"", bytes(8), None

    def close(self):
        pass
"""
FAKE_MODULES = ("fakeclient", "fakeextension")


@pytest.fixture
def fake_client(tmp_path, monkeypatch):
    """Make the client "fake" importable, and record at each start of the
    CPU clock whether its modules are imported."""
    (tmp_path / "fakeclient.py").write_text(FAKE_CLIENT)
    (tmp_path / "fakeextension.py").write_text("")
    monkeypatch.syspath_prepend(str(tmp_path))
    monkeypatch.setitem(
        clientworker.CLIENT_CLASSES,
        "fake",
        ("fakeclient", "ClientStorage", ("fakeextension",), {}),
    )
    for name in FAKE_MODULES:
        monkeypatch.delitem(sys.modules, name, raising=False)
    imported = []
    real = clientworker.cpu

    def cpu():
        imported.append(all(name in sys.modules for name in FAKE_MODULES))
        return real()

    monkeypatch.setattr(clientworker, "cpu", cpu)
    oids = tmp_path / "oids"
    oids.write_bytes(bytes(8) + bytes(7) + b"\x01")
    spec = {
        "client": "fake",
        "address": ["127.0.0.1", 1],
        "mode": "threads",
        "concurrency": 1,
        "workload": "warm",
        "record_size": 1024,
        "objects": 2,
        "oids": str(oids),
        "var": str(tmp_path),
        "cache_size": 1024,
    }
    yield spec, imported
    for name in FAKE_MODULES:
        sys.modules.pop(name, None)


def test_a_warm_round_of_threads_imports_its_client_before_the_clock(fake_client):
    spec, imported = fake_client
    result = clientworker.run_threads(spec)
    assert result["calls"] == 2
    # The first call starts the clock, the second one stops it.
    assert imported == [True, True]


def test_a_warm_round_of_a_process_imports_its_client_before_the_clock(fake_client):
    spec, imported = fake_client
    reports = queue.Queue()
    clientworker._process(spec, 0, threading.Barrier(1), reports)
    *_, times, error = reports.get_nowait()
    assert error is None
    assert len(times) == 2
    assert imported == [True, True]


@pytest.mark.parametrize("name", [ZEO, NATIVE])
def test_each_client_imports_the_modules_of_its_calls(name):
    opener = clientworker.client_class({"client": name})
    assert {"ZODB.utils", "ZODB.Connection"} <= set(sys.modules)
    if name == NATIVE:
        assert "zoe._zoe" in sys.modules
        assert opener.keywords == {"native": True}
    else:
        assert opener.keywords == {}
